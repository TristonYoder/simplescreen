use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use screen_server::{build_state, config::Config, hub::Out, router, AppState};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct App {
    router: Router,
    token: String,
    state: AppState,
    tags: std::sync::Mutex<std::collections::HashMap<String, String>>,
    folders: std::sync::Mutex<std::collections::HashMap<String, String>>,
    files: std::sync::atomic::AtomicUsize,
    _dir: tempfile::TempDir,
}

struct Resp {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: bytes::Bytes,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| panic!("not json: {:?}", self.body))
    }
}

impl App {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::defaults(dir.path().to_path_buf());
        cfg.bonjour = false;
        tweak(&mut cfg);
        let state = build_state(cfg).await.unwrap();
        let token = state.bootstrap.clone().unwrap();
        Self { router: router(state.clone()), token, state, tags: Default::default(), folders: Default::default(), files: Default::default(), _dir: dir }
    }

    async fn send(&self, method: &str, uri: &str, token: Option<&str>, headers: &[(&str, &str)], body: Body, json: bool) -> Resp {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if json {
            b = b.header(header::CONTENT_TYPE, "application/json");
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let resp = self.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        Resp { status, headers, body }
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> Resp {
        let t = self.token.clone();
        self.call_as(&t, method, uri, body).await
    }

    async fn call_as(&self, token: &str, method: &str, uri: &str, body: Option<Value>) -> Resp {
        match body {
            Some(v) => self.send(method, uri, Some(token), &[], Body::from(v.to_string()), true).await,
            None => self.send(method, uri, Some(token), &[], Body::empty(), false).await,
        }
    }

    /// A team holding a tag of its own (see `tag`).
    async fn team(&self) -> String {
        let r = self.call("POST", "/v1/teams", Some(json!({ "name": "Lobby" }))).await;
        assert_eq!(r.status, StatusCode::CREATED);
        let team = r.json()["id"].as_str().unwrap().to_string();
        let tag = self.new_tag(&format!("tag-{team}")).await;
        let r = self.call("PUT", &format!("/v1/teams/{team}/tags"), Some(json!({ "tag_ids": [tag] }))).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
        self.tags.lock().unwrap().insert(team.clone(), tag);
        team
    }

    fn tag(&self, team: &str) -> String {
        self.tags.lock().unwrap()[team].clone()
    }

    async fn new_tag(&self, name: &str) -> String {
        let r = self.call("POST", "/v1/tags", Some(json!({ "name": name }))).await;
        assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
        r.json()["id"].as_str().unwrap().to_string()
    }

    /// Makes a playlist available wherever its team's own tag is.
    async fn tag_playlist(&self, team: &str, playlist: &str) {
        let r = self.call("PUT", &format!("/v1/playlists/{playlist}/tags"), Some(json!({ "tag_ids": [self.tag(team)] }))).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    }

    async fn upload(&self, data: &[u8]) -> String {
        let sha = hex::encode(Sha256::digest(data));
        let r = self
            .send("PUT", &format!("/v1/blobs/{sha}"), Some(&self.token), &[], Body::from(data.to_vec()), false)
            .await;
        assert!(r.status == StatusCode::CREATED || r.status == StatusCode::OK);
        sha
    }

    /// A top-level managed-library folder granted to `team`.
    async fn folder(&self, team: &str) -> String {
        if let Some(f) = self.folders.lock().unwrap().get(team) {
            return f.clone();
        }
        let r = self.call("POST", "/v1/media-folders", Some(json!({ "library_id": "managed", "name": format!("f-{team}") }))).await;
        assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
        let f = r.json()["id"].as_str().unwrap().to_string();
        let r = self.call("PUT", &format!("/v1/media-folders/{f}/grants"), Some(json!({ "team_ids": [team] }))).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
        self.folders.lock().unwrap().insert(team.to_string(), f.clone());
        f
    }

    async fn media(&self, team: &str, kind: &str, mime: &str, data: &[u8]) -> (String, String) {
        let folder = self.folder(team).await;
        self.media_in(&folder, kind, mime, data).await
    }

    async fn media_in(&self, folder: &str, kind: &str, mime: &str, data: &[u8]) -> (String, String) {
        let sha = self.upload(data).await;
        let n = self.files.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let r = self
            .call(
                "POST",
                "/v1/media",
                Some(json!({ "folder_id": folder, "name": format!("file {n}"), "kind": kind, "sha256": sha, "mime": mime })),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
        (r.json()["id"].as_str().unwrap().to_string(), sha)
    }
}

#[tokio::test]
async fn info_and_auth() {
    let app = App::new().await;
    let r = app.send("GET", "/v1/info", None, &[], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["slug"], "simplescreen");
    assert_eq!(v["api_version"], 1);
    assert!(v["server_id"].as_str().unwrap().len() == 36);

    let r = app.send("GET", "/v1/me", None, &[], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["code"], "unauthorized");

    let r = app.call("GET", "/v1/me", None).await;
    assert_eq!(r.json()["kind"], "bootstrap");
    assert_eq!(r.json()["server_admin"], true);
}

#[tokio::test]
async fn teams() {
    let app = App::new().await;
    let r = app.call("POST", "/v1/teams", Some(json!({ "name": "" }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["reason"], "name_empty");
    let r = app.call("POST", "/v1/teams", Some(json!({ "name": 5 }))).await;
    assert_eq!(r.json()["reason"], "invalid_body");
    let r = app.call("GET", "/v1/schedule?scope=7&from=x", None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["code"], "bad_request");
    let id = app.team().await;
    let r = app.call("GET", "/v1/teams", None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    let r = app
        .call("PUT", &format!("/v1/teams/{id}"), Some(json!({ "name": "X", "oidc_bindings": [{ "claim_value": "a", "role": "editor" }] })))
        .await;
    assert_eq!(r.json()["oidc_bindings"][0]["role"], "editor");
    assert!(r.json().get("timezone").is_none());
    app.call("POST", "/v1/playlists", Some(json!({ "team_id": id, "name": "P", "items": [] }))).await;
    let r = app.call("DELETE", &format!("/v1/teams/{id}"), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn first_team_with_sign_in_groups() {
    let app = App::new().await;
    let bindings = json!([
        { "claim_value": "signage-admins", "role": "admin" },
        { "claim_value": " signage-editors ", "role": "editor" },
        { "claim_value": "staff", "role": "viewer" },
    ]);
    let r = app.call("POST", "/v1/teams", Some(json!({ "name": "Production", "oidc_bindings": bindings }))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let team = r.json();
    assert_eq!(team["name"], "Production");
    assert_eq!(team["oidc_bindings"].as_array().unwrap().len(), 3);
    assert_eq!(team["oidc_bindings"][1]["claim_value"], "signage-editors");
    assert_eq!(team["oidc_bindings"][0]["role"], "admin");

    let r = app.call("GET", &format!("/v1/teams/{}", team["id"].as_str().unwrap()), None).await;
    assert_eq!(r.json()["oidc_bindings"][2]["role"], "viewer");

    for bad in [
        json!([{ "claim_value": "", "role": "editor" }]),
        json!([{ "claim_value": "a", "role": "editor" }, { "claim_value": "a", "role": "viewer" }]),
        json!([{ "claim_value": "a", "role": "owner" }]),
    ] {
        let r = app.call("POST", "/v1/teams", Some(json!({ "name": "Kids", "oidc_bindings": bad }))).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let r = app.call("GET", "/v1/teams", None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn organizations() {
    let app = App::new().await;
    let org_id = app.send("GET", "/v1/info", None, &[], Body::empty(), false).await.json()["server_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = app.call("GET", "/v1/me/organizations", None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        r.json(),
        json!({
            "organizations": [{ "id": org_id, "name": "Simple Screen", "team_count": 0, "role": "admin" }],
            "can_create": false,
            "invites": [],
        })
    );
    let r = app.send("GET", "/v1/me/organizations", None, &[], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let team = app.team().await;
    app.team().await;
    let r = app.call("GET", "/v1/me/organizations", None).await;
    assert_eq!(r.json()["organizations"][0]["team_count"], 2);

    let r = app
        .call("POST", "/v1/organizations", Some(json!({ "name": "Grace", "timezone": "UTC", "first_team_name": "Production" })))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.json()["code"], "not_supported");
    let r = app.call("POST", "/v1/invites/abc/accept", None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // X-Organization: absent or matching is accepted, a mismatch is organization_not_found.
    let t = app.token.clone();
    let r = app.send("GET", "/v1/teams", Some(&t), &[("X-Organization", &org_id)], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::OK);
    for uri in ["/v1/teams", "/v1/me", "/v1/settings", "/v1/ws"] {
        let r = app.send("GET", uri, Some(&t), &[("X-Organization", "other-org")], Body::empty(), false).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(r.json()["code"], "organization_not_found", "{uri}");
    }
    // The org listing itself is never scoped.
    let r = app.send("GET", "/v1/me/organizations", Some(&t), &[("X-Organization", "other-org")], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::OK);

    // Devices ignore the header; the manifest names the org.
    let (_, device) = pair(&app, &team, "Hall", None).await;
    let r = app.send("GET", "/v1/device/manifest", Some(&device), &[("X-Organization", "other-org")], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["organization_id"], org_id.as_str());
    assert_eq!(r.json()["organization_name"], "Simple Screen");
    let etag = r.headers[header::ETAG].to_str().unwrap().to_string();
    let r = app.call_as(&device, "GET", "/v1/me/organizations", None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // Renaming the org changes the manifest.
    app.call("PATCH", "/v1/settings", Some(json!({ "org_name": "Grace Church" }))).await;
    let r = app.send("GET", "/v1/device/manifest", Some(&device), &[("If-None-Match", &etag)], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["organization_name"], "Grace Church");
}

#[tokio::test]
async fn org_settings_first_run() {
    let app = App::new().await;
    let r = app.call("GET", "/v1/settings", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let v = r.json();
    assert_eq!(v["configured"], false);
    assert_eq!(v["org_name"], "Simple Screen");
    assert_eq!(v["locked"], json!([]));
    assert!(v["timezone"].as_str().unwrap().parse::<chrono_tz::Tz>().is_ok());

    let r = app.call("PATCH", "/v1/settings", Some(json!({ "timezone": "Mars/Base" }))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "invalid_timezone");
    let r = app.call("PATCH", "/v1/settings", Some(json!({ "org_name": "  " }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app.call("PATCH", "/v1/settings", Some(json!({ "name": "typo" }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let r = app
        .call("PATCH", "/v1/settings", Some(json!({ "org_name": " Grace Church ", "timezone": "America/Chicago" })))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["org_name"], "Grace Church");
    assert_eq!(r.json()["timezone"], "America/Chicago");
    assert_eq!(r.json()["configured"], true);

    let team = app.team().await;
    let r = app.call("GET", &format!("/v1/schedule?team_id={team}"), None).await;
    assert_eq!(r.json()["timezone"], "America/Chicago");
    let r = app.call("GET", "/v1/me/organizations", None).await;
    assert_eq!(r.json()["organizations"][0]["name"], "Grace Church");
}

#[tokio::test]
async fn org_settings_from_env_are_locked() {
    let app = App::with(|c| {
        c.org_name = Some("Grace Church".into());
        c.org_timezone = Some("Europe/London".into());
    })
    .await;
    let r = app.call("GET", "/v1/settings", None).await;
    let v = r.json();
    assert_eq!(v["org_name"], "Grace Church");
    assert_eq!(v["timezone"], "Europe/London");
    assert_eq!(v["configured"], true);
    assert_eq!(v["locked"], json!(["org_name", "timezone"]));
    for body in [json!({ "org_name": "Other" }), json!({ "timezone": "UTC" })] {
        let r = app.call("PATCH", "/v1/settings", Some(body)).await;
        assert_eq!(r.status, StatusCode::CONFLICT);
        assert_eq!(r.json()["code"], "setting_locked");
    }
}

/// Every operation in api/openapi.yaml is routed: secured ones answer 401 without a token.
#[tokio::test]
async fn contract_operations_are_routed() {
    let spec = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../api/openapi.yaml")).unwrap();
    let mut ops: Vec<(String, String, bool)> = vec![];
    let mut path = None;
    for line in spec.lines() {
        if line == "components:" {
            break;
        }
        if let Some(p) = line.strip_prefix("  /").and_then(|l| l.strip_suffix(':')) {
            path = Some(format!("/{p}"));
        } else if let (Some(p), Some(m)) = (&path, line.strip_prefix("    ").and_then(|l| l.strip_suffix(':'))) {
            if ["get", "post", "put", "patch", "delete", "head"].contains(&m) {
                ops.push((m.to_uppercase(), p.clone(), true));
            }
        } else if line.trim() == "security: []" {
            ops.last_mut().unwrap().2 = false;
        }
    }
    assert!(ops.len() > 40, "parsed {} operations", ops.len());
    let app = App::new().await;
    let id = "00000000-0000-4000-8000-000000000000";
    for (method, path, secured) in ops.iter().filter(|o| o.2) {
        let uri = path
            .split('/')
            .map(|seg| if seg.starts_with('{') { if seg == "{sha256}" { "0".repeat(64) } else { id.to_string() } } else { seg.to_string() })
            .collect::<Vec<_>>()
            .join("/");
        let r = app.send(method, &uri, None, &[], Body::empty(), false).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{method} {path} (secured: {secured})");
    }
}

#[tokio::test]
async fn blobs() {
    let app = App::new().await;
    let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
    let sha = hex::encode(Sha256::digest(&data));

    let wrong = "0".repeat(64);
    let r = app.send("PUT", &format!("/v1/blobs/{wrong}"), Some(&app.token), &[], Body::from(data.clone()), false).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "hash_mismatch");
    let r = app.call("HEAD", &format!("/v1/blobs/{wrong}"), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let r = app.send("PUT", &format!("/v1/blobs/{sha}"), Some(&app.token), &[], Body::from(data.clone()), false).await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.json()["size"], 1000);
    let r2 = app.send("PUT", &format!("/v1/blobs/{sha}"), Some(&app.token), &[], Body::from(data.clone()), false).await;
    assert_eq!(r2.status, StatusCode::OK);

    let r = app.call("HEAD", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers[header::CONTENT_LENGTH], "1000");

    let r = app.call("GET", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body.as_ref(), data.as_slice());
    assert_eq!(r.headers[header::ETAG], format!("\"{sha}\"").as_str());
    assert_eq!(r.headers[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
    assert_eq!(r.headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(r.headers[header::CONTENT_TYPE], "application/octet-stream");

    let uri = format!("/v1/blobs/{sha}");
    let r = app.send("GET", &uri, Some(&app.token), &[("range", "bytes=10-19")], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.body.as_ref(), &data[10..20]);
    assert_eq!(r.headers[header::CONTENT_RANGE], "bytes 10-19/1000");
    let r = app.send("GET", &uri, Some(&app.token), &[("range", "bytes=990-")], Body::empty(), false).await;
    assert_eq!(r.body.as_ref(), &data[990..]);
    let r = app.send("GET", &uri, Some(&app.token), &[("range", "bytes=-5")], Body::empty(), false).await;
    assert_eq!(r.body.as_ref(), &data[995..]);
    let r = app.send("GET", &uri, Some(&app.token), &[("range", "bytes=2000-")], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.headers[header::CONTENT_RANGE], "bytes */1000");

    let etag = format!("\"{sha}\"");
    let r = app.send("GET", &uri, Some(&app.token), &[("if-none-match", &etag)], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);

    let r = app.send("GET", &format!("{uri}?token={}", app.token), None, &[], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = app.send("GET", &uri, None, &[], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn media_lifecycle_and_delete_conflict() {
    let app = App::new().await;
    let g = app.team().await;
    let folder = app.folder(&g).await;

    let missing = "a".repeat(64);
    let r = app
        .call("POST", "/v1/media", Some(json!({ "folder_id": folder, "name": "x.mp4", "kind": "video", "sha256": missing, "mime": "video/mp4" })))
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "blob_missing");

    let (media_id, sha) = app.media(&g, "video", "video/mp4", b"video-bytes").await;
    let r = app.call("GET", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.headers[header::CONTENT_TYPE], "video/mp4");
    let r = app.call("PATCH", &format!("/v1/media/{media_id}"), Some(json!({ "name": "Renamed.mp4" }))).await;
    assert_eq!(r.json()["name"], "Renamed.mp4");
    assert_eq!(r.json()["path"], format!("f-{g}/Renamed.mp4"));
    assert_eq!(r.json()["size"], 11);
    let r = app.call("GET", &format!("/v1/media?team_id={g}&kind=video"), None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);

    let r = app
        .call("POST", "/v1/playlists", Some(json!({ "team_id": g, "name": "P", "items": [{ "kind": "video", "media_id": media_id }] })))
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let pid = r.json()["id"].as_str().unwrap().to_string();

    let r = app.call("GET", &format!("/v1/media/{media_id}/usage"), None).await;
    assert_eq!(r.json()["playlists"][0]["id"], pid.as_str());
    assert_eq!(r.json()["playlists"][0]["items"], 1);

    // Deleting a file in use keeps it as missing; the playlist keeps the item.
    let r = app.call("DELETE", &format!("/v1/media/{media_id}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call("GET", &format!("/v1/media/{media_id}"), None).await;
    assert_eq!(r.json()["missing"], true);
    assert!(r.json().get("folder_id").is_none());
    let r = app.call("HEAD", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app.call("GET", &format!("/v1/playlists/{pid}"), None).await;
    assert_eq!(r.json()["version"], 2);
    let r = app.call("DELETE", &format!("/v1/media/{media_id}"), None).await;
    assert_eq!(r.json()["code"], "media_missing");

    // Once no playlist uses it, the record goes too.
    app.call("DELETE", &format!("/v1/playlists/{pid}"), None).await;
    let r = app.call("GET", &format!("/v1/media/{media_id}"), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn playlists() {
    let app = App::new().await;
    let g = app.team().await;
    let (v1, _) = app.media(&g, "video", "video/mp4", b"v1").await;
    let (v2, _) = app.media(&g, "video", "video/mp4", b"v2").await;
    let (img, _) = app.media(&g, "image", "image/png", b"i1").await;
    let (aud, _) = app.media(&g, "audio", "audio/mp4", b"a1").await;

    let r = app
        .call(
            "POST",
            "/v1/playlists",
            Some(json!({ "team_id": g, "name": "P", "items": [
                { "kind": "video", "media_id": v1 },
                { "kind": "image", "media_id": img },
            ]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let pl = r.json();
    assert_eq!(pl["version"], 1);
    assert_eq!(pl["loop"], true);
    assert_eq!(pl["defaults"]["then"], "auto");
    assert_eq!(pl["defaults"]["while_waiting"], "hold_last_frame");
    assert_eq!(pl["defaults"]["transition"], json!({ "type": "dissolve", "duration_ms": 1000 }));
    assert_eq!(pl["defaults"]["image_duration_ms"], 10000);
    assert_eq!(pl["defaults"]["clip_audio"], "duck_bed");
    let pid = pl["id"].as_str().unwrap();
    let ids: Vec<String> = pl["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(ids.len(), 2);

    let update = |base: i64, items: Value, beds: Value| {
        json!({ "base_version": base, "name": "P2", "loop": false, "defaults": pl["defaults"], "items": items, "beds": beds })
    };
    let items = json!([
        { "id": ids[0], "kind": "video", "media_id": v1 },
        { "id": ids[1], "kind": "image", "media_id": img },
        { "kind": "video", "media_id": v2 },
    ]);
    let beds = json!([{ "start_item_id": ids[0], "end_item_id": ids[1], "tracks": [aud] }]);
    let r = app.call("PUT", &format!("/v1/playlists/{pid}"), Some(update(1, items.clone(), beds.clone()))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let v = r.json();
    assert_eq!(v["version"], 2);
    assert_eq!(v["loop"], false);
    assert_eq!(v["items"][0]["id"], ids[0]);
    assert!(v["items"][2]["id"].is_string());
    assert!(v["beds"][0]["id"].is_string());
    assert_eq!(v["beds"][0]["volume"], 0.8);

    let r = app.call("PUT", &format!("/v1/playlists/{pid}"), Some(update(1, items.clone(), beds.clone()))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let e = r.json();
    assert_eq!(e["code"], "version_conflict");
    assert_eq!(e["current"]["version"], 2);

    let third = v["items"][2]["id"].as_str().unwrap().to_string();
    let overlap = json!([
        { "start_item_id": ids[0], "end_item_id": ids[1], "tracks": [aud] },
        { "start_item_id": ids[1], "end_item_id": third, "tracks": [aud] },
    ]);
    let r = app.call("PUT", &format!("/v1/playlists/{pid}"), Some(update(2, v["items"].clone(), overlap))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(r.json()["message"].as_str().unwrap().contains("overlap"));
    assert_eq!(r.json()["reason"], "beds_overlap");

    let bad_kind = json!([{ "kind": "image", "media_id": v1 }]);
    let r = app.call("PUT", &format!("/v1/playlists/{pid}"), Some(update(2, bad_kind, json!([])))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["reason"], "media_kind");
    assert_eq!(r.json()["params"]["expected"], "image");

    let r = app.call("GET", &format!("/v1/playlists?team_id={g}"), None).await;
    assert_eq!(r.json()[0]["item_count"], 3);
    assert!(r.json()[0]["total_bytes"].as_i64().unwrap() > 0);
}

async fn pair(app: &App, team: &str, name: &str, playlist: Option<&str>) -> (String, String) {
    let r = app.send("POST", "/v1/pairing", None, &[], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let s = r.json();
    let code = s["code"].as_str().unwrap();
    assert_eq!(code.len(), 6);
    assert!(code.chars().all(|c| "23456789ABCDEFGHJKLMNPQRSTUVWXYZ".contains(c)));
    let poll = format!("/v1/pairing/{}?secret={}", s["pairing_id"].as_str().unwrap(), s["secret"].as_str().unwrap());

    let r = app.send("GET", &poll, None, &[], Body::empty(), false).await;
    assert_eq!(r.json()["status"], "pending");
    let r = app.send("GET", &format!("{}x", poll), None, &[], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let r = app.call("POST", "/v1/pairing/claim", Some(json!({ "code": "ZZZZZZ", "name": name }))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let mut claim = json!({ "code": code.to_lowercase(), "name": name, "tag_ids": [app.tag(team)] });
    if let Some(p) = playlist {
        app.tag_playlist(team, p).await;
        claim["playlist_id"] = json!(p);
    }
    let r = app.call("POST", "/v1/pairing/claim", Some(claim)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let screen_id = r.json()["id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["online"], false);
    assert_eq!(r.json()["status"]["state"], "offline");

    let r = app.send("GET", &poll, None, &[], Body::empty(), false).await;
    let v = r.json();
    assert_eq!(v["status"], "claimed");
    assert_eq!(v["screen_id"], screen_id);
    let token = v["device_token"].as_str().unwrap().to_string();
    let r = app.send("GET", &poll, None, &[], Body::empty(), false).await;
    assert_eq!(r.json()["status"], "claimed");
    assert!(r.json().get("device_token").is_none());
    (screen_id, token)
}

#[tokio::test]
async fn pairing_and_manifest() {
    let app = App::new().await;
    let g = app.team().await;
    let (v1, sha_v1) = app.media(&g, "video", "video/mp4", b"video-1").await;
    let (v2, sha_v2) = app.media(&g, "video", "video/mp4", b"video-2").await;
    let (img, sha_img) = app.media(&g, "image", "image/png", b"image-1").await;
    let (p1, sha_p1) = app.media(&g, "image", "image/png", b"page-1").await;
    let (p2, sha_p2) = app.media(&g, "image", "image/png", b"page-2").await;
    let (aud, sha_aud) = app.media(&g, "audio", "audio/mp4", b"audio-1").await;

    let r = app
        .call(
            "POST",
            "/v1/playlists",
            Some(json!({ "team_id": g, "name": "P", "items": [
                { "kind": "video", "media_id": v1 },
                { "kind": "image", "media_id": img },
                { "kind": "sequence", "pages": [p1, p2] },
                { "kind": "video", "media_id": v2 },
                { "kind": "video", "media_id": v1 },
            ]})),
        )
        .await;
    let pl = r.json();
    let pid = pl["id"].as_str().unwrap().to_string();
    let item_id = |i: usize| pl["items"][i]["id"].as_str().unwrap().to_string();
    let put = json!({
        "base_version": 1, "name": "P", "loop": true, "defaults": pl["defaults"], "items": pl["items"],
        "beds": [{ "start_item_id": item_id(1), "end_item_id": item_id(2), "tracks": [aud] }],
    });
    let r = app.call("PUT", &format!("/v1/playlists/{pid}"), Some(put)).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);

    let (screen_id, token) = pair(&app, &g, "Lobby TV", Some(&pid)).await;

    let r = app.call_as(&token, "GET", "/v1/me", None).await;
    assert_eq!(r.json()["kind"], "device");
    assert_eq!(r.json()["screen_id"], screen_id);

    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let etag = r.headers[header::ETAG].to_str().unwrap().to_string();
    let m = r.json();
    assert_eq!(m["screen_name"], "Lobby TV");
    assert_eq!(m["playlist"]["version"], 2);
    let shas: Vec<&str> = m["assets"].as_array().unwrap().iter().map(|a| a["sha256"].as_str().unwrap()).collect();
    assert_eq!(shas, vec![sha_v1.as_str(), sha_img.as_str(), sha_aud.as_str(), sha_p1.as_str(), sha_p2.as_str(), sha_v2.as_str()]);
    let idx: Vec<i64> = m["assets"].as_array().unwrap().iter().map(|a| a["first_use_index"].as_i64().unwrap()).collect();
    assert_eq!(idx, vec![0, 1, 1, 2, 2, 3]);
    assert_eq!(m["assets"][0]["path"], format!("/v1/blobs/{sha_v1}"));
    assert_eq!(m["assets"][0]["mime"], "video/mp4");
    assert_eq!(m["media"].as_array().unwrap().len(), 6);

    let r = app.send("GET", "/v1/device/manifest", Some(&token), &[("if-none-match", &etag)], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);

    let r = app.call_as(&token, "GET", &format!("/v1/blobs/{sha_v1}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = app.call_as(&token, "GET", "/v1/media", None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call("GET", "/v1/device/manifest", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let r = app.call("POST", &format!("/v1/screens/{screen_id}/command"), Some(json!({ "name": "next" }))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    let r = app.call("PATCH", &format!("/v1/screens/{screen_id}"), Some(json!({ "fallback_playlist_id": null }))).await;
    assert!(r.json().get("fallback_playlist_id").is_none());
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_ne!(r.headers[header::ETAG].to_str().unwrap(), etag);
    assert!(r.json().get("playlist").is_none());

    let r = app.call("DELETE", &format!("/v1/screens/{screen_id}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn device_selects_playlist() {
    let app = App::new().await;
    let g = app.team().await;
    let other = app.team().await;
    let (img, _) = app.media(&g, "image", "image/png", b"img").await;
    let mk = |team: &str, name: &str| json!({ "team_id": team, "name": name, "items": [{ "kind": "image", "media_id": img }] });
    let a = app.call("POST", "/v1/playlists", Some(mk(&g, "A"))).await.json()["id"].as_str().unwrap().to_string();
    let b = app.call("POST", "/v1/playlists", Some(json!({ "team_id": g, "name": "B", "items": [] }))).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let foreign = app.call("POST", "/v1/playlists", Some(json!({ "team_id": other, "name": "X", "items": [] }))).await.json()
        ["id"]
        .as_str()
        .unwrap()
        .to_string();
    app.tag_playlist(&g, &a).await;
    app.tag_playlist(&g, &b).await;
    let (screen_id, token) = pair(&app, &g, "Hall", None).await;

    let r = app.call_as(&token, "GET", "/v1/device/playlists", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let tree = r.json();
    assert_eq!((tree[0]["kind"].as_str(), tree[0]["id"].as_str()), (Some("team"), Some(g.as_str())));
    let names: Vec<&str> = tree[0]["children"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["A", "B"]);

    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "playlist_id": a }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{:?}", r.body);
    let r = app.call("GET", &format!("/v1/screens/{screen_id}"), None).await;
    assert_eq!(r.json()["fallback_playlist_id"], a);
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.json()["playlist"]["id"], a);

    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "playlist_id": foreign }))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "playlist_id": b }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "fallback_playlist_id": null }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call("GET", &format!("/v1/screens/{screen_id}"), None).await;
    assert!(r.json().get("fallback_playlist_id").is_none());

    let r = app.call("GET", "/v1/device/playlists", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = app.call("PUT", "/v1/device/playlist", Some(json!({ "playlist_id": a }))).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn schedule_resolves_events_weekly_and_overrides() {
    use chrono::{DateTime, Duration, Timelike, Utc};
    let app = App::new().await;
    let g = app.team().await;
    let r = app.call("PATCH", "/v1/settings", Some(json!({ "timezone": "UTC" }))).await;
    assert_eq!(r.json()["timezone"], "UTC");

    let (img, _) = app.media(&g, "image", "image/png", b"one").await;
    let (img2, sha2) = app.media(&g, "image", "image/png", b"two").await;
    let mk = |name: &str, m: &str| json!({ "team_id": g, "name": name, "items": [{ "kind": "image", "media_id": m }] });
    let id = |r: Resp| r.json()["id"].as_str().unwrap().to_string();
    let a = id(app.call("POST", "/v1/playlists", Some(mk("A", &img))).await);
    let b = id(app.call("POST", "/v1/playlists", Some(mk("B", &img))).await);
    let c = id(app.call("POST", "/v1/playlists", Some(mk("C", &img2))).await);
    app.tag_playlist(&g, &b).await;
    app.tag_playlist(&g, &c).await;
    let (screen, token) = pair(&app, &g, "Hall", Some(&a)).await;

    let now = Utc::now().with_second(0).unwrap().with_nanosecond(0).unwrap();
    let local = |d: DateTime<Utc>| d.format("%Y-%m-%dT%H:%M").to_string();
    let clock = |d: DateTime<Utc>| d.format("%H:%M").to_string();

    let bad = app
        .call("POST", "/v1/schedule", Some(json!({ "team_id": g, "playlist_id": b, "kind": "once", "starts_at": local(now), "ends_at": local(now) })))
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    let bad = app
        .call("POST", "/v1/schedule", Some(json!({ "team_id": g, "playlist_id": b, "kind": "weekly", "days": [], "start_time": "07:00", "end_time": "08:00" })))
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);

    let r = app
        .call("POST", "/v1/schedule", Some(json!({ "team_id": g, "playlist_id": b, "kind": "weekly", "days": [3], "start_time": "07:00:15", "end_time": "08:00:00", "enabled": false })))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    assert_eq!(r.json()["start_time"], "07:00:15");
    assert_eq!(r.json()["end_time"], "08:00");
    let seconds_entry = r.json()["id"].as_str().unwrap().to_string();
    assert_eq!(app.call("DELETE", &format!("/v1/schedule/{seconds_entry}"), None).await.status, StatusCode::NO_CONTENT);

    let event_end = now + Duration::hours(2);
    let r = app
        .call(
            "POST",
            "/v1/schedule",
            Some(json!({ "team_id": g, "playlist_id": b, "kind": "once", "title": "Launch", "starts_at": local(now - Duration::hours(1)), "ends_at": local(event_end) })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let event = r.json()["id"].as_str().unwrap().to_string();
    let r = app
        .call(
            "POST",
            "/v1/schedule",
            Some(json!({ "team_id": g, "playlist_id": c, "kind": "weekly", "days": [1,2,3,4,5,6,7], "screen_ids": [screen],
                         "start_time": clock(now + Duration::hours(3)), "end_time": clock(now + Duration::hours(4)) })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);

    let r = app.call("GET", &format!("/v1/schedule?team_id={g}"), None).await;
    assert_eq!(r.json()["timezone"], "UTC");
    assert_eq!(r.json()["entries"].as_array().unwrap().len(), 2);

    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    let m = r.json();
    assert_eq!(m["playlist"]["id"], b);
    assert_eq!(m["timeline"][0]["source"], "once");
    assert_eq!(m["timeline"][0]["entry_id"], event);
    let sources: Vec<&str> = m["timeline"].as_array().unwrap().iter().map(|s| s["source"].as_str().unwrap()).collect();
    assert_eq!(&sources[..4], &["once", "default", "weekly", "default"]);
    let ids: Vec<&str> = m["playlists"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec![b.as_str(), a.as_str(), c.as_str()]);
    assert!(m["assets"].as_array().unwrap().iter().any(|x| x["sha256"] == sha2.as_str()));
    let etag = r.headers.get(header::ETAG).unwrap().to_str().unwrap().to_string();
    let r = app.send("GET", "/v1/device/manifest", Some(&token), &[("If-None-Match", &etag)], Body::empty(), false).await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);

    let from = Utc::now().to_rfc3339();
    let to = (Utc::now() + Duration::hours(6)).to_rfc3339();
    let r = app
        .call("GET", &format!("/v1/schedule/timeline?team_id={g}&from={}&to={}", from.replace('+', "%2B"), to.replace('+', "%2B")), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["screens"][0]["segments"][0]["playlist_id"], b);

    let r = app.call("PUT", &format!("/v1/screens/{screen}/override"), Some(json!({ "playlist_id": c, "until_next_change": true }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let until = DateTime::parse_from_rfc3339(r.json()["override"]["until"].as_str().unwrap()).unwrap();
    assert_eq!(until, event_end);
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.json()["playlist"]["id"], c);
    assert_eq!(r.json()["timeline"][0]["source"], "override");
    let r = app.call("DELETE", &format!("/v1/screens/{screen}/override"), None).await;
    assert!(r.json().get("override").is_none());

    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "playlist_id": a }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call("GET", &format!("/v1/screens/{screen}"), None).await;
    assert_eq!(r.json()["override"]["playlist_id"], a);

    let r = app.call("DELETE", &format!("/v1/schedule/{event}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call("DELETE", &format!("/v1/playlists/{c}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call("GET", &format!("/v1/schedule?team_id={g}"), None).await;
    assert_eq!(r.json()["entries"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn floor_plans_and_placement() {
    let app = App::new().await;
    let g = app.team().await;
    let (screen, _) = pair(&app, &g, "Hall", None).await;

    let missing = "0".repeat(64);
    let r = app.call("POST", "/v1/floor-plans", Some(json!({ "team_id": g, "name": "Ground", "image_sha256": missing }))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let sha = app.upload(b"map").await;
    let r = app
        .call("POST", "/v1/floor-plans", Some(json!({ "team_id": g, "name": " Ground ", "image_sha256": sha, "image_width": 1600, "image_height": 900 })))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let plan = r.json();
    assert_eq!(plan["name"], "Ground");
    assert_eq!(plan["position"], 0);
    let plan_id = plan["id"].as_str().unwrap().to_string();
    let r = app.call("POST", "/v1/floor-plans", Some(json!({ "team_id": g, "name": "Upstairs" }))).await;
    assert_eq!(r.json()["position"], 1);

    let r = app.call("PATCH", &format!("/v1/screens/{screen}"), Some(json!({ "placement": { "floor_plan_id": plan_id, "x": 1.5, "y": 0.2 } }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app.call("PATCH", &format!("/v1/screens/{screen}"), Some(json!({ "placement": { "floor_plan_id": plan_id, "x": 0.25, "y": 0.75 } }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["placement"]["x"], 0.25);

    let r = app.call("PUT", &format!("/v1/floor-plans/{plan_id}"), Some(json!({ "team_id": g, "name": "Lobby floor", "image_sha256": null }))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json().get("image_sha256").is_none());
    assert_eq!(r.json()["position"], 0);

    let r = app.call("GET", &format!("/v1/floor-plans?team_id={g}"), None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 2);

    let r = app.call("DELETE", &format!("/v1/floor-plans/{plan_id}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call("GET", &format!("/v1/screens/{screen}"), None).await;
    assert!(r.json().get("placement").is_none());

    let r = app.call("PATCH", &format!("/v1/screens/{screen}"), Some(json!({ "placement": { "floor_plan_id": plan_id, "x": 0.1, "y": 0.1 } }))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
}

// MARK: Nodes

async fn pair_node(app: &App, _team: &str, name: &str) -> (String, String) {
    let r = app.send("POST", "/v1/pairing", None, &[], Body::from(json!({ "kind": "node" }).to_string()), true).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let s = r.json();
    let poll = format!("/v1/pairing/{}?secret={}", s["pairing_id"].as_str().unwrap(), s["secret"].as_str().unwrap());
    let r = app
        .call("POST", "/v1/pairing/claim", Some(json!({ "code": s["code"], "name": name })))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let node = r.json();
    assert_eq!(node["screen_ids"], json!([]));
    assert_eq!(node["outputs"], json!([]));
    let node_id = node["id"].as_str().unwrap().to_string();

    let r = app.send("GET", &poll, None, &[], Body::empty(), false).await;
    let v = r.json();
    assert_eq!(v["status"], "claimed");
    assert_eq!(v["node_id"], node_id);
    assert!(v.get("screen_id").is_none());
    (node_id, v["device_token"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn single_pairing_creates_node_and_screen() {
    let app = App::new().await;
    let g = app.team().await;
    let (screen_id, token) = pair(&app, &g, "Hall", None).await;
    let screen = app.call("GET", &format!("/v1/screens/{screen_id}"), None).await.json();
    assert_eq!(screen["output_keys"], json!(["main"]));
    let node_id = screen["node_id"].as_str().unwrap();
    let r = app.call("GET", &format!("/v1/nodes?team_id={g}"), None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    assert_eq!(r.json()[0]["id"], node_id);
    assert_eq!(r.json()[0]["screen_ids"], json!([screen_id]));

    let r = app.call_as(&token, "GET", "/v1/device/node", None).await;
    assert_eq!(r.json()["node"]["id"], node_id);
    assert_eq!(r.json()["screens"][0]["id"], screen_id);

    // Explicit screen_id works; someone else's screen does not.
    let r = app.call_as(&token, "GET", &format!("/v1/device/manifest?screen_id={screen_id}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    let (other, _) = pair(&app, &g, "Other", None).await;
    let r = app.call_as(&token, "GET", &format!("/v1/device/manifest?screen_id={other}"), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "screen_id": other, "playlist_id": null }))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "screen_id": screen_id, "playlist_id": null }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);

    // Editors cannot reach device endpoints and devices cannot list nodes' teams' data.
    let r = app.call("GET", "/v1/device/node", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn node_outputs_and_screen_creation() {
    let app = App::new().await;
    let g = app.team().await;
    let (node_id, token) = pair_node(&app, &g, "Mac mini").await;

    // No screens yet: implicit resolution is ambiguous.
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "screen_id_required");

    let r = app
        .call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "hdmi-1", "name": "Left" })))
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "unknown_output");

    let outputs = json!([
        { "key": "hdmi-1", "kind": "display", "name": "Left", "role": "program" },
        { "key": "hdmi-2", "kind": "external", "name": "Right", "role": "program", "format": "1080p60" },
        { "key": "dl-1", "kind": "decklink", "name": "DeckLink 1", "role": "none" },
        { "key": "v-1", "kind": "virtual", "name": "Virtual", "role": "confidence", "audio_device": "BlackHole" },
    ]);
    let r = app.call_as(&token, "PUT", "/v1/device/outputs", Some(outputs.clone())).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let bad = json!([{ "key": "a", "kind": "hdmi", "name": "A", "role": "program" }]);
    assert_eq!(app.call_as(&token, "PUT", "/v1/device/outputs", Some(bad)).await.status, StatusCode::BAD_REQUEST);
    let dup = json!([
        { "key": "a", "kind": "display", "name": "A", "role": "program" },
        { "key": "a", "kind": "display", "name": "B", "role": "program" },
    ]);
    assert_eq!(app.call_as(&token, "PUT", "/v1/device/outputs", Some(dup)).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    // A reported screen_id is ignored. The stored role comes from assignments, which are still empty.
    let foreign = json!([{ "key": "a", "kind": "display", "name": "A", "role": "program", "screen_id": "nope" }]);
    let r = app.call_as(&token, "PUT", "/v1/device/outputs", Some(foreign)).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert!(r.json()["outputs"][0].get("screen_id").is_none(), "{:?}", r.json());
    assert_eq!(r.json()["outputs"][0]["role"], "none");
    let r = app.call_as(&token, "PUT", "/v1/device/outputs", Some(outputs)).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let r = app.call("GET", &format!("/v1/nodes/{node_id}"), None).await;
    assert_eq!(r.json()["outputs"].as_array().unwrap().len(), 4);

    let r = app
        .call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "hdmi-1", "name": " Left TV " })))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let left = r.json();
    assert_eq!(left["name"], "Left TV");
    assert_eq!(left["tag_ids"], json!([]));
    assert_eq!(left["node_id"], node_id);
    assert_eq!(left["output_keys"], json!(["hdmi-1"]));
    let left_id = left["id"].as_str().unwrap().to_string();

    let r = app
        .call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "hdmi-1", "name": "Again" })))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.json()["code"], "output_has_screen");

    // One screen: implicit works. Two screens: it no longer does, but explicit does.
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["screen_id"], left_id);
    let r = app
        .call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "hdmi-2", "name": "Right" })))
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let right_id = r.json()["id"].as_str().unwrap().to_string();
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = app.call_as(&token, "GET", &format!("/v1/device/manifest?screen_id={right_id}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["screen_name"], "Right");
    let r = app.call_as(&token, "GET", "/v1/device/playlists", None).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = app.call_as(&token, "GET", &format!("/v1/device/playlists?screen_id={left_id}"), None).await;
    assert_eq!(r.status, StatusCode::OK);

    let r = app.call_as(&token, "GET", "/v1/device/node", None).await;
    assert_eq!(r.json()["screens"].as_array().unwrap().len(), 2);
    let node = app.call("GET", &format!("/v1/nodes/{node_id}"), None).await.json();
    assert_eq!(node["screen_ids"].as_array().unwrap().len(), 2);
    let out = node["outputs"].as_array().unwrap().iter().find(|o| o["key"] == "hdmi-1").unwrap().clone();
    assert_eq!(out["screen_id"], left_id);

    // Replacing outputs never deletes screens.
    let r = app.call_as(&token, "PUT", "/v1/device/outputs", Some(json!([]))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(app.call("GET", &format!("/v1/screens/{left_id}"), None).await.status, StatusCode::OK);

    // Deleting a screen of a multi-output node keeps the node and its token.
    let r = app.call("DELETE", &format!("/v1/screens/{right_id}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(app.call_as(&token, "GET", "/v1/device/node", None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn node_socket_routes_screen_messages_and_jump() {
    let app = App::new().await;
    let g = app.team().await;
    let (node_id, token) = pair_node(&app, &g, "Mac mini").await;
    app.call_as(&token, "PUT", "/v1/device/outputs", Some(json!([
        { "key": "a", "kind": "display", "name": "A", "role": "program" },
        { "key": "b", "kind": "display", "name": "B", "role": "program" },
    ])))
    .await;
    let mk = |key: &str| json!({ "output_key": key, "name": key });
    let a = app.call_as(&token, "POST", "/v1/device/screens", Some(mk("a"))).await.json()["id"].as_str().unwrap().to_string();
    let b = app.call_as(&token, "POST", "/v1/device/screens", Some(mk("b"))).await.json()["id"].as_str().unwrap().to_string();

    // Offline: a valid jump is delivered nowhere.
    let jump = json!({ "name": "jump", "item_index": 2, "step_index": 1 });
    let r = app.call("POST", &format!("/v1/screens/{a}/command"), Some(jump.clone())).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(app.call("GET", &format!("/v1/screens/{a}"), None).await.json()["online"], false);

    let (_conn, mut rx) = app.state.hub.register_node(&node_id);
    assert_eq!(app.call("GET", &format!("/v1/screens/{b}"), None).await.json()["online"], true);

    let r = app.call("POST", &format!("/v1/screens/{b}/command"), Some(jump)).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let Some(Out::Msg(m)) = rx.recv().await else { panic!("no message") };
    let m: Value = serde_json::from_str(&m).unwrap();
    assert_eq!(m["type"], "command");
    assert_eq!(m["screen_id"], b);
    assert_eq!(m["command"]["name"], "jump");
    assert_eq!(m["command"]["item_index"], 2);
    assert_eq!(m["command"]["step_index"], 1);

    let r = app.call("POST", &format!("/v1/screens/{a}/command"), Some(json!({ "name": "jump" }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app.call("POST", &format!("/v1/screens/{a}/command"), Some(json!({ "name": "next" }))).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let Some(Out::Msg(m)) = rx.recv().await else { panic!("no message") };
    assert_eq!(serde_json::from_str::<Value>(&m).unwrap()["screen_id"], a);

    // Manifest changes are routed to the node and name the screen.
    let r = app.call("PATCH", &format!("/v1/screens/{a}"), Some(json!({ "fallback_playlist_id": null }))).await;
    assert_eq!(r.status, StatusCode::OK);
    let Some(Out::Msg(m)) = rx.recv().await else { panic!("no message") };
    let m: Value = serde_json::from_str(&m).unwrap();
    assert_eq!((m["type"].as_str(), m["screen_id"].as_str(), m["version"].as_i64()), (Some("manifest_changed"), Some(a.as_str()), Some(0)));
}

// MARK: Assignments

async fn pair_mode(app: &App, team: &str, name: &str, mode: &str) -> (String, String) {
    let r = app
        .send("POST", "/v1/pairing", None, &[], Body::from(json!({ "kind": "node", "mode": mode }).to_string()), true)
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let s = r.json();
    let poll = format!("/v1/pairing/{}?secret={}", s["pairing_id"].as_str().unwrap(), s["secret"].as_str().unwrap());
    let _ = team;
    let r = app.call("POST", "/v1/pairing/claim", Some(json!({ "code": s["code"], "name": name }))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let node = r.json();
    assert_eq!(node["mode"], mode);
    assert_eq!(node["assignments_version"], 0);
    let v = app.send("GET", &poll, None, &[], Body::empty(), false).await.json();
    (node["id"].as_str().unwrap().to_string(), v["device_token"].as_str().unwrap().to_string())
}

async fn outputs(app: &App, token: &str) {
    let r = app
        .call_as(token, "PUT", "/v1/device/outputs", Some(json!([
            { "key": "a", "kind": "display", "name": "A" },
            { "key": "b", "kind": "external", "name": "B" },
            { "key": "dl", "kind": "decklink", "name": "DeckLink" },
            { "key": "v", "kind": "virtual", "name": "Virtual" },
        ])))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
}

/// A player node with outputs a, b, dl, v and a screen on `a`.
async fn player(app: &App, g: &str, name: &str) -> (String, String, String) {
    let (node, token) = pair_node(app, g, name).await;
    outputs(app, &token).await;
    let r = app.call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "a", "name": name }))).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let screen = r.json()["id"].as_str().unwrap().to_string();
    let r = app.call("PUT", &format!("/v1/screens/{screen}/tags"), Some(json!({ "tag_ids": [app.tag(g)] }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    (node, token, screen)
}

/// Studio-side (admin) replacement of a node's assignments.
async fn put_n(app: &App, node: &str, base: i64, list: Value) -> Resp {
    app.call("PUT", &format!("/v1/nodes/{node}/assignments"), Some(json!({ "base_version": base, "assignments": list }))).await
}

async fn put_a(app: &App, token: &str, base: i64, list: Value) -> Resp {
    app.call_as(token, "PUT", "/v1/device/assignments", Some(json!({ "base_version": base, "assignments": list }))).await
}

async fn recv_json(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Out>) -> Value {
    let Some(Out::Msg(m)) = rx.recv().await else { panic!("no message") };
    serde_json::from_str(&m).unwrap()
}

async fn code(r: Resp, status: StatusCode) -> String {
    assert_eq!(r.status, status, "{:?}", r.body);
    r.json()["code"].as_str().unwrap_or_default().to_string()
}

async fn version(app: &App, token: &str) -> i64 {
    app.call_as(token, "GET", "/v1/device/assignments", None).await.json()["version"].as_i64().unwrap()
}

#[tokio::test]
async fn assignments_validation_and_versions() {
    let app = App::new().await;
    let g = app.team().await;
    let (node, token, screen) = player(&app, &g, "Mini").await;
    // create_screen made the screen job.
    let cur = app.call_as(&token, "GET", "/v1/device/assignments", None).await.json();
    assert_eq!(cur["mode"], "player");
    assert_eq!(cur["version"], 1);
    assert_eq!(cur["assignments"][0]["job"], "program");
    assert_eq!(cur["assignments"][0]["screen_id"], screen);
    let node_json = app.call("GET", &format!("/v1/nodes/{node}"), None).await.json();
    assert_eq!((node_json["assignments_version"].as_i64(), node_json["applied_assignments_version"].as_i64()), (Some(1), Some(1)));

    let keep = json!({ "output_key": "a", "job": "program", "screen_id": screen });
    let bad = |list: Value| put_a(&app, &token, 1, list);
    let c = code(bad(json!([keep, { "output_key": "zzz", "job": "control", "screen_id": screen }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "unknown_output");
    let c = code(bad(json!([keep, { "output_key": "dl", "job": "control", "screen_id": screen }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "control_needs_input");
    let c = code(bad(json!([keep, { "output_key": "v", "job": "control", "screen_id": screen }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "control_needs_input");
    // The device can only use screens it is already assigned to; Studio can't take another node's screen.
    let (_n2, _t2, other) = player(&app, &g, "Other").await;
    let c = code(bad(json!([keep, { "output_key": "b", "job": "control", "screen_id": other }])).await, StatusCode::FORBIDDEN).await;
    assert_eq!(c, "target_not_assigned");
    let c = code(put_n(&app, &node, 1, json!([keep, { "output_key": "b", "job": "program", "screen_id": other }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "screen_must_be_local");
    let c = code(put_n(&app, &node, 1, json!([keep, { "output_key": "b", "job": "control", "screen_id": "nope" }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "unknown_screen");
    let c = code(bad(json!([keep, { "output_key": "b", "job": "control" }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "screen_required");
    // Unknown outputs are only allowed when off.
    let ok = bad(json!([keep, { "output_key": "gone", "job": "off" }])).await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);
    assert_eq!(ok.json()["version"], 2);

    // Audio: one enabled assignment per screen, across devices too.
    let audio = |key: &str, job: &str, sid: &str| json!({ "output_key": key, "job": job, "screen_id": sid, "settings": { "audio": { "enabled": true } } });
    let c = code(put_a(&app, &token, 2, json!([audio("a", "program", &screen), audio("b", "program", &screen)])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "audio_conflict");
    let r = put_a(&app, &token, 2, json!([audio("a", "program", &screen)])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let (n4, t4) = pair_mode(&app, &g, "iPad", "controller").await;
    outputs(&app, &t4).await;
    let r = put_n(&app, &n4, 0, json!([audio("a", "control", &screen)])).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "audio_conflict");

    // Stale base_version: 409 with the current body.
    let r = put_a(&app, &token, 1, json!([])).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let body = r.json();
    assert_eq!(body["code"], "version_conflict");
    assert_eq!(body["current"]["version"], 3);
    assert_eq!(body["current"]["assignments"][0]["screen_id"], screen);
    assert_eq!(body["current"]["assignments"][0]["settings"]["audio"]["enabled"], true);
}

#[tokio::test]
async fn controller_mode_limits_jobs() {
    let app = App::new().await;
    let g = app.team().await;
    let (_p, _pt, screen) = player(&app, &g, "Mini").await;
    let (node, token) = pair_mode(&app, &g, "iPad", "controller").await;
    outputs(&app, &token).await;
    let r = put_n(&app, &node, 0, json!([{ "output_key": "a", "job": "program", "screen_id": screen }])).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "job_not_allowed_in_mode");
    // Controller devices own no screens, so creating one is refused as well.
    let r = app.call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "a", "name": "x" }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "job_not_allowed_in_mode");
    // Control and confidence may target any screen, on any output of a controller node.
    let r = put_n(&app, &node, 0, json!([
        { "output_key": "a", "job": "control", "screen_id": screen, "settings": { "view": "operator", "clock": 24 } },
        { "output_key": "b", "job": "confidence", "screen_id": screen, "settings": { "clock": 12 } },
        { "output_key": "dl", "job": "off" },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["assignments"].as_array().unwrap().len(), 3);
    let bad_clock = put_n(&app, &node, 1, json!([{ "output_key": "a", "job": "control", "screen_id": screen, "settings": { "clock": 5 } }])).await;
    assert_eq!(code(bad_clock, StatusCode::UNPROCESSABLE_ENTITY).await, "invalid_assignments");

    // The device can rearrange its own assignments.
    let r = put_a(&app, &token, 1, json!([{ "output_key": "b", "job": "control", "screen_id": screen }])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
}

#[tokio::test]
async fn unplace_on_job_removal_and_mode_change() {
    let app = App::new().await;
    let g = app.team().await;
    let (node, token, screen) = player(&app, &g, "Mini").await;
    let r = app.call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "b", "name": "Second" }))).await;
    let second = r.json()["id"].as_str().unwrap().to_string();

    // Removing the screen job unplaces, never deletes.
    let v = version(&app, &token).await;
    let r = put_a(&app, &token, v, json!([
        { "output_key": "a", "job": "off" },
        { "output_key": "b", "job": "program", "screen_id": second },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let s = app.call("GET", &format!("/v1/screens/{screen}"), None).await.json();
    assert!(s.get("node_id").is_none() && s["output_keys"] == json!([]), "{s}");
    let listed = app.call("GET", &format!("/v1/screens?team_id={g}"), None).await.json();
    assert!(listed.as_array().unwrap().iter().any(|x| x["id"] == screen && x.get("node_id").is_none()));
    let n = app.call("GET", &format!("/v1/nodes/{node}"), None).await.json();
    assert_eq!(n["screen_ids"], json!([second]));
    // An unplaced screen is no longer the device's to take; Studio can place it again.
    let v = version(&app, &token).await;
    let take = json!([
        { "output_key": "a", "job": "program", "screen_id": screen },
        { "output_key": "b", "job": "program", "screen_id": second },
    ]);
    assert_eq!(code(put_a(&app, &token, v, take).await, StatusCode::FORBIDDEN).await, "target_not_assigned");
    let r = put_n(&app, &node, v, json!([
        { "output_key": "a", "job": "program", "screen_id": screen },
        { "output_key": "b", "job": "program", "screen_id": second },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(app.call("GET", &format!("/v1/screens/{screen}"), None).await.json()["output_keys"], json!(["a"]));

    // Leaving player releases all screens and drops screen/mirror jobs but keeps control.
    let v = version(&app, &token).await;
    let r = put_a(&app, &token, v, json!([
        { "output_key": "a", "job": "program", "screen_id": screen },
        { "output_key": "b", "job": "control", "screen_id": second },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let r = app.call_as(&token, "PUT", "/v1/device/mode", Some(json!({ "mode": "stage" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let body = r.json();
    assert_eq!(body["mode"], "stage");
    let jobs: Vec<_> = body["assignments"].as_array().unwrap().iter().map(|a| (a["output_key"].as_str().unwrap(), a["job"].as_str().unwrap())).collect();
    assert!(!jobs.iter().any(|(_, j)| *j == "program"), "{jobs:?}");
    assert!(jobs.contains(&("b", "control")));
    for id in [&screen, &second] {
        let s = app.call("GET", &format!("/v1/screens/{id}"), None).await.json();
        assert!(s.get("node_id").is_none(), "{s}");
    }
    let n = app.call("GET", &format!("/v1/nodes/{node}"), None).await.json();
    assert_eq!(n["mode"], "stage");
    assert_eq!(n["screen_ids"], json!([]));
    assert_eq!(n["applied_assignments_version"], n["assignments_version"]);
    let r = app.call_as(&token, "PUT", "/v1/device/mode", Some(json!({ "mode": "bogus" }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn editor_assignments_and_move() {
    let app = App::new().await;
    let g = app.team().await;
    let (n1, t1, screen) = player(&app, &g, "Mini").await;
    let (n2, t2) = pair_node(&app, &g, "Studio").await;
    outputs(&app, &t2).await;
    let audio = json!({ "audio": { "enabled": true, "device": "HDMI" } });
    let v = version(&app, &t1).await;
    let r = put_a(&app, &t1, v, json!([{ "output_key": "a", "job": "program", "screen_id": screen, "settings": audio }])).await;
    assert_eq!(r.status, StatusCode::OK);

    // Editor GET/PUT on a node, versioned, with updated_by set from the user.
    let r = app.call("GET", &format!("/v1/nodes/{n2}/assignments"), None).await;
    assert_eq!((r.json()["version"].as_i64(), r.json()["mode"].as_str()), (Some(0), Some("player")));
    let r = app.call("PUT", &format!("/v1/nodes/{n2}/assignments"), Some(json!({ "base_version": 5, "assignments": [] }))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = app.call("PUT", &format!("/v1/nodes/{n2}/assignments"), Some(json!({ "base_version": 0, "assignments": [{ "output_key": "b", "job": "confidence", "screen_id": screen }] }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let by = r.json()["assignments"][0]["updated_by"].clone();
    assert!(by.is_string() && by != json!(n2), "{by}");
    let studio = app.call("GET", &format!("/v1/nodes/{n2}"), None).await.json();
    assert_eq!((studio["assignments_version"].as_i64(), studio["applied_assignments_version"].as_i64()), (Some(1), Some(0)));

    // Move the screen to the other node's output; its settings follow and the old output goes off.
    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": n2, "output_key": "zzz" }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "unknown_output");
    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": "nope", "output_key": "a" }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "unknown_node");

    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": n2, "output_key": "a" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!((r.json()["node_id"].as_str(), r.json()["output_keys"].clone()), (Some(n2.as_str()), json!(["a"])));
    let old = app.call("GET", &format!("/v1/nodes/{n1}/assignments"), None).await.json();
    let a = old["assignments"].as_array().unwrap().iter().find(|a| a["output_key"] == "a").unwrap();
    assert_eq!(a["job"], "off");
    assert!(a.get("screen_id").is_none());
    assert_eq!(old["version"], 3);
    let new = app.call("GET", &format!("/v1/nodes/{n2}/assignments"), None).await.json();
    let a = new["assignments"].as_array().unwrap().iter().find(|a| a["output_key"] == "a").unwrap();
    assert_eq!((a["job"].as_str(), a["screen_id"].as_str()), (Some("program"), Some(screen.as_str())));
    assert_eq!(a["settings"]["audio"]["device"], "HDMI");
    assert_eq!(new["version"], 2);
    // The confidence job on the new node keeps pointing at the screen.
    assert!(new["assignments"].as_array().unwrap().iter().any(|a| a["job"] == "confidence" && a["screen_id"] == screen));
    let n1_json = app.call("GET", &format!("/v1/nodes/{n1}"), None).await.json();
    assert_eq!(n1_json["screen_ids"], json!([]));

    // Moving to an output in controller mode is refused.
    let (nc, tc) = pair_mode(&app, &g, "iPad", "controller").await;
    outputs(&app, &tc).await;
    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": nc, "output_key": "a" }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "job_not_allowed_in_mode");
    // Move back by moving onto the first node again (old output off, a different output used).
    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": n1, "output_key": "b" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["node_id"], n1);
    let _ = t2;
}

#[tokio::test]
async fn program_outputs_and_moves_keep_their_own_audio() {
    let app = App::new().await;
    let g = app.team().await;
    let (node, token, screen) = player(&app, &g, "Mini").await;
    // One screen may play on several outputs of its node; the device may move it between them.
    let v = version(&app, &token).await;
    let r = put_a(&app, &token, v, json!([
        { "output_key": "a", "job": "program", "screen_id": screen },
        { "output_key": "b", "job": "program", "screen_id": screen },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(app.call("GET", &format!("/v1/screens/{screen}"), None).await.json()["output_keys"], json!(["a", "b"]));
    let r = put_a(&app, &token, v + 1, json!([{ "output_key": "a", "job": "program", "screen_id": screen }])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);

    let second = app.call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "b", "name": "Second" }))).await;
    assert_eq!(second.status, StatusCode::CREATED, "{:?}", second.body);
    let second_id = second.json()["id"].as_str().unwrap().to_string();
    let v = version(&app, &token).await;
    let r = put_a(&app, &token, v, json!([
        { "output_key": "a", "job": "program", "screen_id": screen },
        { "output_key": "b", "job": "program", "screen_id": second_id, "settings": { "audio": { "enabled": true } } },
        { "output_key": "v", "job": "program", "screen_id": second_id },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    // Moving `screen` onto b takes that output; `second` keeps playing on v.
    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": node, "output_key": "b" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let body = app.call("GET", &format!("/v1/nodes/{node}/assignments"), None).await.json();
    let jobs: Vec<_> = body["assignments"].as_array().unwrap().iter().map(|a| (a["output_key"].as_str().unwrap(), a["job"].as_str().unwrap())).collect();
    assert!(jobs.contains(&("a", "off")) && jobs.contains(&("b", "program")) && jobs.contains(&("v", "program")), "{jobs:?}");
    let moved = body["assignments"].as_array().unwrap().iter().find(|a| a["output_key"] == "b").unwrap();
    assert_eq!(moved["screen_id"], screen);
    assert!(moved["settings"].get("audio").is_none(), "{moved}");
    assert_eq!(app.call("GET", &format!("/v1/screens/{second_id}"), None).await.json()["output_keys"], json!(["v"]));

    // An unplaced screen moved onto an output does not take that output's audio.
    let v = body["version"].as_i64().unwrap();
    let r = put_a(&app, &token, v, json!([
        { "output_key": "b", "job": "program", "screen_id": screen, "settings": { "audio": { "enabled": true, "device": "HDMI" } } },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert!(app.call("GET", &format!("/v1/screens/{second_id}"), None).await.json().get("node_id").is_none());
    let r = app.call("POST", &format!("/v1/screens/{second_id}/move"), Some(json!({ "node_id": node, "output_key": "b" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let body = app.call("GET", &format!("/v1/nodes/{node}/assignments"), None).await.json();
    let landed = body["assignments"].as_array().unwrap().iter().find(|a| a["output_key"] == "b").unwrap();
    assert_eq!(landed["screen_id"], second_id);
    assert!(landed["settings"].get("audio").is_none(), "{landed}");
}

#[tokio::test]
async fn remote_command_manifest_and_blob_access() {
    let app = App::new().await;
    let g = app.team().await;
    let (_pn, _pt, screen) = player(&app, &g, "Mini").await;
    let (_p2, _pt2, other) = player(&app, &g, "Other").await;
    let (cn, ct) = pair_mode(&app, &g, "iPad", "controller").await;
    outputs(&app, &ct).await;

    // Without an assignment: no remote command, no remote manifest.
    let cmd = json!({ "name": "next" });
    let r = app.call_as(&ct, "POST", &format!("/v1/device/screens/{screen}/command"), Some(cmd.clone())).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call_as(&ct, "GET", &format!("/v1/device/manifest?screen_id={screen}"), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let r = put_n(&app, &cn, 0, json!([
        { "output_key": "a", "job": "control", "screen_id": screen },
        { "output_key": "b", "job": "confidence", "screen_id": other },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);

    // The owner must be online to receive it.
    let r = app.call_as(&ct, "POST", &format!("/v1/device/screens/{screen}/command"), Some(cmd.clone())).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let owner = app.call("GET", &format!("/v1/screens/{screen}"), None).await.json()["node_id"].as_str().unwrap().to_string();
    let (_c, mut rx) = app.state.hub.register_node(&owner);
    let r = app.call_as(&ct, "POST", &format!("/v1/device/screens/{screen}/command"), Some(json!({ "name": "jump", "item_index": 3 }))).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let Some(Out::Msg(m)) = rx.recv().await else { panic!("no message") };
    let m: Value = serde_json::from_str(&m).unwrap();
    assert_eq!((m["type"].as_str(), m["screen_id"].as_str(), m["command"]["item_index"].as_i64()), (Some("command"), Some(screen.as_str()), Some(3)));
    let r = app.call_as(&ct, "POST", &format!("/v1/device/screens/{screen}/command"), Some(json!({ "name": "jump" }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    // Confidence alone never allows commands; unrelated screens and plain devices are refused.
    let r = app.call_as(&ct, "POST", &format!("/v1/device/screens/{other}/command"), Some(cmd.clone())).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call("POST", &format!("/v1/device/screens/{screen}/command"), Some(cmd)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Read-only manifest and blobs for control and confidence targets; writes stay with the owner.
    for id in [&screen, &other] {
        let r = app.call_as(&ct, "GET", &format!("/v1/device/manifest?screen_id={id}"), None).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
        assert_eq!(r.json()["screen_id"], *id);
    }
    // Remote playlist choice: control target allowed, confidence-only 403, unrelated 404.
    let pl = app.call("POST", "/v1/playlists", Some(json!({ "team_id": g, "name": "Remote", "items": [] }))).await.json()["id"].as_str().unwrap().to_string();
    app.tag_playlist(&g, &pl).await;
    let r = app.call_as(&ct, "GET", &format!("/v1/device/playlists?screen_id={screen}"), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert!(r.json()[0]["children"].as_array().unwrap().iter().any(|p| p["id"] == pl));
    let r = app.call_as(&ct, "GET", &format!("/v1/device/playlists?screen_id={other}"), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call_as(&ct, "PUT", "/v1/device/playlist", Some(json!({ "screen_id": screen, "playlist_id": pl }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{:?}", r.body);
    assert_eq!(app.call("GET", &format!("/v1/screens/{screen}"), None).await.json()["fallback_playlist_id"], pl);
    let r = app.call_as(&ct, "PUT", "/v1/device/playlist", Some(json!({ "screen_id": other, "playlist_id": pl }))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call_as(&ct, "PUT", "/v1/device/playlist", Some(json!({ "screen_id": uuid::Uuid::new_v4().to_string(), "playlist_id": pl }))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let sha = app.upload(b"thumbnail bytes").await;
    let r = app.call_as(&ct, "GET", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn watchers_receive_status_manifest_and_assignment_messages() {
    let app = App::new().await;
    let g = app.team().await;
    let (pn, _pt, screen) = player(&app, &g, "Mini").await;
    let (cn, ct) = pair_mode(&app, &g, "iPad", "controller").await;
    outputs(&app, &ct).await;
    let (_pc, mut prx) = app.state.hub.register_node(&pn);
    let (_cc, mut crx) = app.state.hub.register_node(&cn);
    app.state.statuses.lock().insert(screen.clone(), screen_server::models::ScreenStatus::simple(screen_server::models::ScreenState::Playing));

    let r = put_n(&app, &cn, 0, json!([{ "output_key": "a", "job": "control", "screen_id": screen }])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let m = recv_json(&mut crx).await;
    assert_eq!((m["type"].as_str(), m["version"].as_i64()), (Some("assignments_changed"), Some(1)));
    // The current status is pushed as soon as the node starts watching.
    let m = recv_json(&mut crx).await;
    assert_eq!((m["type"].as_str(), m["screen_id"].as_str(), m["status"]["state"].as_str()), (Some("screen_status"), Some(screen.as_str()), Some("playing")));

    // Status the player reports is fanned out to the watcher but not echoed to the player.
    let msg = json!({ "type": "screen_status", "screen_id": screen, "status": { "state": "idle" } });
    app.state.hub.send_subscribers(&screen, Some(&pn), &msg);
    assert_eq!(recv_json(&mut crx).await["type"], "screen_status");
    assert!(prx.try_recv().is_err());

    // Manifest changes reach the watcher too (and the player, once each).
    let r = app.call("PATCH", &format!("/v1/screens/{screen}"), Some(json!({ "fallback_playlist_id": null }))).await;
    assert_eq!(r.status, StatusCode::OK);
    let m = recv_json(&mut crx).await;
    assert_eq!((m["type"].as_str(), m["screen_id"].as_str()), (Some("manifest_changed"), Some(screen.as_str())));
    let Some(Out::Msg(m)) = prx.recv().await else { panic!("no message") };
    assert!(m.contains("manifest_changed"));
    assert!(prx.try_recv().is_err());

    // Dropping the assignment stops the fan-out.
    let r = put_n(&app, &cn, 1, json!([{ "output_key": "a", "job": "off" }])).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(recv_json(&mut crx).await["type"], "assignments_changed");
    app.state.hub.send_subscribers(&screen, Some(&pn), &msg);
    assert!(crx.try_recv().is_err());
}

// MARK: Tags, reach and clashes

/// A local OIDC provider: discovery and JWKS on loopback, Ed25519 tokens signed with a key made per run.
struct Idp {
    issuer: String,
    pkcs8: Vec<u8>,
}

impl Idp {
    async fn start() -> Self {
        use base64::Engine;
        use ring::signature::KeyPair;
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref().to_vec();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&pkcs8).unwrap();
        let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pair.public_key().as_ref());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let discovery = json!({ "issuer": issuer, "jwks_uri": format!("{issuer}/jwks") });
        let jwks = json!({ "keys": [{ "kty": "OKP", "crv": "Ed25519", "x": x, "kid": "test", "alg": "EdDSA", "use": "sig" }] });
        let idp = Router::new()
            .route("/.well-known/openid-configuration", axum::routing::get(move || async move { axum::Json(discovery) }))
            .route("/jwks", axum::routing::get(move || async move { axum::Json(jwks) }));
        tokio::spawn(async move { axum::serve(listener, idp).await.unwrap() });
        Self { issuer, pkcs8 }
    }

    fn token(&self, sub: &str, groups: &[&str]) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        header.kid = Some("test".into());
        let exp = chrono::Utc::now().timestamp() + 3600;
        let claims = json!({ "sub": sub, "iss": self.issuer, "exp": exp, "name": sub, "groups": groups });
        jsonwebtoken::encode(&header, &claims, &jsonwebtoken::EncodingKey::from_ed_der(&self.pkcs8)).unwrap()
    }
}

/// Lobby holds `front`, Kitchen holds `back`. Admin, lobby editor, lobby viewer and kitchen editor tokens.
struct Org {
    app: App,
    lobby: String,
    kitchen: String,
    front: String,
    back: String,
    le: String,
    lv: String,
    ke: String,
}

async fn org() -> Org {
    let idp = Idp::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::defaults(dir.path().to_path_buf());
    cfg.bonjour = false;
    cfg.oidc_issuer = Some(idp.issuer.clone());
    cfg.oidc_admin_groups = vec!["admins".into()];
    let state = build_state(cfg).await.unwrap();
    let app = App { router: router(state.clone()), token: idp.token("u-admin", &["admins"]), state, tags: Default::default(), folders: Default::default(), files: Default::default(), _dir: dir };
    let team = |name: &str, ed: &str, view: &str| {
        json!({ "name": name, "oidc_bindings": [{ "claim_value": ed, "role": "editor" }, { "claim_value": view, "role": "viewer" }] })
    };
    let lobby = app.call("POST", "/v1/teams", Some(team("Lobby", "lobby-ed", "lobby-view"))).await.json()["id"].as_str().unwrap().to_string();
    let kitchen = app.call("POST", "/v1/teams", Some(team("Kitchen", "kitchen-ed", "kitchen-view"))).await.json()["id"].as_str().unwrap().to_string();
    let front = app.new_tag("front").await;
    let back = app.new_tag("back").await;
    let r = app.call("POST", "/v1/tags", Some(json!({ "name": "FRONT" }))).await;
    assert_eq!(code(r, StatusCode::CONFLICT).await, "tag_exists");
    for (t, tag) in [(&lobby, &front), (&kitchen, &back)] {
        let r = app.call("PUT", &format!("/v1/teams/{t}/tags"), Some(json!({ "tag_ids": [tag] }))).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    }
    let r = app.call("PATCH", "/v1/settings", Some(json!({ "timezone": "UTC" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    Org {
        lobby,
        kitchen,
        front,
        back,
        le: idp.token("u-lobby", &["lobby-ed"]),
        lv: idp.token("u-view", &["lobby-view"]),
        ke: idp.token("u-kitchen", &["kitchen-ed"]),
        app,
    }
}

/// Single pairing by the admin with these tags; returns the screen and its device token.
async fn pair_tagged(app: &App, name: &str, tags: Value) -> (String, String) {
    let s = app.send("POST", "/v1/pairing", None, &[], Body::empty(), false).await.json();
    let r = app.call("POST", "/v1/pairing/claim", Some(json!({ "code": s["code"], "name": name, "tag_ids": tags }))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let screen = r.json()["id"].as_str().unwrap().to_string();
    let poll = format!("/v1/pairing/{}?secret={}", s["pairing_id"].as_str().unwrap(), s["secret"].as_str().unwrap());
    let token = app.send("GET", &poll, None, &[], Body::empty(), false).await.json()["device_token"].as_str().unwrap().to_string();
    (screen, token)
}

async fn playlist_as(app: &App, token: &str, team: &str, name: &str) -> String {
    let r = app.call_as(token, "POST", "/v1/playlists", Some(json!({ "team_id": team, "name": name, "items": [] }))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    r.json()["id"].as_str().unwrap().to_string()
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn reach_availability_and_untagged_screens() {
    let o = org().await;
    let app = &o.app;
    let (s_front, front_token) = pair_tagged(app, "Front", json!([o.front])).await;
    let (s_back, back_token) = pair_tagged(app, "Back", json!([o.back])).await;
    let (node, nt) = pair_node(app, "", "Mini").await;
    outputs(app, &nt).await;
    let r = app.call_as(&nt, "POST", "/v1/device/screens", Some(json!({ "output_key": "a", "name": "Bare" }))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let s_none = r.json()["id"].as_str().unwrap().to_string();

    // Reach: tags shared with the team; untagged screens are admin only.
    let mine = app.call_as(&o.le, "GET", "/v1/screens", None).await.json();
    assert_eq!(ids(&mine), vec![s_front.clone()]);
    assert_eq!(mine[0]["team_ids"], json!([o.lobby]));
    assert_eq!(app.call("GET", "/v1/screens", None).await.json().as_array().unwrap().len(), 3);
    assert_eq!(app.call_as(&o.le, "GET", &format!("/v1/screens/{s_back}"), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(app.call_as(&o.le, "GET", &format!("/v1/screens/{s_none}"), None).await.status, StatusCode::NOT_FOUND);
    // Viewers see but don't operate.
    assert_eq!(app.call_as(&o.lv, "GET", &format!("/v1/screens/{s_front}"), None).await.status, StatusCode::OK);
    let r = app.call_as(&o.lv, "PUT", &format!("/v1/screens/{s_front}/override"), Some(json!({ "playlist_id": null }))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // Only admins tag screens and set reach.
    let r = app.call_as(&o.le, "PUT", &format!("/v1/screens/{s_front}/tags"), Some(json!({ "tag_ids": [o.back] }))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call_as(&o.le, "PUT", &format!("/v1/teams/{}/tags", o.lobby), Some(json!({ "tag_ids": [o.back] }))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // screen_access all reaches every tagged screen, still not the untagged one.
    let r = app.call("PUT", &format!("/v1/teams/{}/tags", o.kitchen), Some(json!({ "tag_ids": [o.back], "screen_access": "all" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["team"]["screen_access"], "all");
    let mut seen = ids(&app.call_as(&o.ke, "GET", "/v1/screens", None).await.json());
    seen.sort();
    let mut want = vec![s_front.clone(), s_back.clone()];
    want.sort();
    assert_eq!(seen, want);
    let reach = app.call("GET", "/v1/teams/reach", None).await.json();
    let k = reach["teams"].as_array().unwrap().iter().find(|t| t["team_id"] == o.kitchen.as_str()).unwrap();
    assert_eq!(k["screen_ids"].as_array().unwrap().len(), 2);

    // Availability: a playlist shows on a screen only through a shared tag, and only held tags go on it.
    let p = playlist_as(app, &o.le, &o.lobby, "Menu").await;
    let r = app.call_as(&o.le, "PATCH", &format!("/v1/screens/{s_front}"), Some(json!({ "fallback_playlist_id": p }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "playlist_unavailable");
    let r = app.call_as(&o.le, "PUT", &format!("/v1/playlists/{p}/tags"), Some(json!({ "tag_ids": [o.back] }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "invalid_tag");
    let r = app.call_as(&o.le, "PUT", &format!("/v1/playlists/{p}/tags"), Some(json!({ "tag_ids": [o.front] }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["playlist"]["tag_ids"], json!([o.front]));
    let r = app.call_as(&o.le, "PATCH", &format!("/v1/screens/{s_front}"), Some(json!({ "fallback_playlist_id": p }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let m = app.call_as(&front_token, "GET", "/v1/device/manifest", None).await.json();
    assert_eq!(m["available_playlist_ids"], json!([p]));
    assert_eq!(m["playlist"]["id"], p);
    let m = app.call_as(&back_token, "GET", "/v1/device/manifest", None).await.json();
    assert_eq!(m["available_playlist_ids"], json!([]));
    // Kitchen reaches the screen showing it, so it may read it; scope=visible lists it.
    assert_eq!(app.call_as(&o.ke, "GET", &format!("/v1/playlists/{p}"), None).await.status, StatusCode::OK);
    assert!(app.call_as(&o.ke, "GET", "/v1/playlists", None).await.json().as_array().unwrap().is_empty());
    assert_eq!(ids(&app.call_as(&o.ke, "GET", "/v1/playlists?scope=visible", None).await.json()), vec![p.clone()]);
    // Untagging the playlist makes the fallback unavailable: the change goes through with a warning.
    let r = app.call_as(&o.le, "PUT", &format!("/v1/playlists/{p}/tags"), Some(json!({ "tag_ids": [] }))).await;
    assert_eq!(r.json()["warnings"][0]["code"], "fallback_unavailable");
    let m = app.call_as(&front_token, "GET", "/v1/device/manifest", None).await.json();
    assert!(m.get("playlist").is_none(), "{m}");

    // The untagged screen raises an admin notice that resolves once it's tagged.
    let open = |n: Value| n.as_array().unwrap().iter().filter(|x| x["kind"] == "untagged_screen" && x["screen_id"] == s_none.as_str()).count();
    assert_eq!(open(app.call("GET", "/v1/notices", None).await.json()), 1);
    assert_eq!(open(app.call_as(&o.le, "GET", "/v1/notices", None).await.json()), 0);
    let r = app.call("PUT", &format!("/v1/screens/{s_none}/tags"), Some(json!({ "tag_ids": [o.front] }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(open(app.call("GET", "/v1/notices", None).await.json()), 0);
    assert_eq!(open(app.call("GET", "/v1/notices?all=true", None).await.json()), 1);
    let events = app.call("GET", &format!("/v1/screens/{s_none}/events"), None).await.json();
    assert!(events.as_array().unwrap().iter().any(|e| e["kind"] == "tags"), "{events}");
    let _ = node;
}

#[tokio::test]
async fn assignment_permissions_follow_reach() {
    let o = org().await;
    let app = &o.app;
    let (s_front, _) = pair_tagged(app, "Front", json!([o.front])).await;
    let (s_back, _) = pair_tagged(app, "Back", json!([o.back])).await;
    let (c, ct) = pair_mode(app, "", "iPad", "controller").await;
    outputs(app, &ct).await;
    let put = |token: String, list: Value| {
        let c = c.clone();
        async move {
            let base = app.call("GET", &format!("/v1/nodes/{c}/assignments"), None).await.json()["version"].as_i64().unwrap();
            app.call_as(&token, "PUT", &format!("/v1/nodes/{c}/assignments"), Some(json!({ "base_version": base, "assignments": list }))).await
        }
    };
    let ctl = |key: &str, screen: &str| json!({ "output_key": key, "job": "control", "screen_id": screen });

    // An editor assigns screens their team reaches, and only those.
    assert_eq!(put(o.le.clone(), json!([ctl("a", &s_front)])).await.status, StatusCode::OK);
    assert_eq!(put(o.le.clone(), json!([ctl("a", &s_front), ctl("b", &s_back)])).await.status, StatusCode::FORBIDDEN);
    assert_eq!(put(o.lv.clone(), json!([ctl("a", &s_front), ctl("b", &s_front)])).await.status, StatusCode::FORBIDDEN);
    // Another team's editor can add its own output but can't touch Lobby's.
    assert_eq!(put(o.ke.clone(), json!([ctl("a", &s_front), ctl("b", &s_back)])).await.status, StatusCode::OK);
    assert_eq!(put(o.ke.clone(), json!([ctl("b", &s_back)])).await.status, StatusCode::FORBIDDEN);
    assert_eq!(put(o.le.clone(), json!([ctl("a", &s_front), { "output_key": "b", "job": "off" }])).await.status, StatusCode::FORBIDDEN);
    // Nodes are hardware: everyone lists them, only admins rename them.
    assert_eq!(app.call_as(&o.le, "GET", "/v1/nodes", None).await.json().as_array().unwrap().len(), 3);
    let r = app.call_as(&o.le, "PATCH", &format!("/v1/nodes/{c}"), Some(json!({ "name": "Desk" }))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call("PATCH", &format!("/v1/nodes/{c}"), Some(json!({ "name": "Desk" }))).await;
    assert_eq!((r.status, r.json()["name"].as_str()), (StatusCode::OK, Some("Desk")));
    // Moving a screen needs to operate it.
    let back_node = app.call("GET", &format!("/v1/screens/{s_back}"), None).await.json()["node_id"].as_str().unwrap().to_string();
    let r = app.call_as(&o.le, "POST", &format!("/v1/screens/{s_back}/move"), Some(json!({ "node_id": back_node, "output_key": "main" }))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    // Pairing: an editor may only use tags their team holds, and must tag the screen.
    let s = app.send("POST", "/v1/pairing", None, &[], Body::empty(), false).await.json();
    let claim = |tags: Value| json!({ "code": s["code"], "name": "New", "tag_ids": tags });
    let r = app.call_as(&o.le, "POST", "/v1/pairing/claim", Some(claim(json!([o.back])))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "invalid_tag");
    let r = app.call_as(&o.le, "POST", "/v1/pairing/claim", Some(claim(json!([])))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "tag_required");
    let r = app.call_as(&o.le, "POST", "/v1/pairing/claim", Some(claim(json!([o.front])))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
}

fn at(days: i64, hour: u32) -> String {
    let d = chrono::Utc::now().date_naive() + chrono::Duration::days(days);
    format!("{}T{hour:02}:00", d.format("%Y-%m-%d"))
}

#[tokio::test]
async fn overlapping_one_off_save_is_refused() {
    let o = org().await;
    let app = &o.app;
    // Kitchen also holds `front`, so both teams reach the screen.
    let r = app.call("PUT", &format!("/v1/teams/{}/tags", o.kitchen), Some(json!({ "tag_ids": [o.back, o.front] }))).await;
    assert_eq!(r.status, StatusCode::OK);
    let (screen, _) = pair_tagged(app, "Front", json!([o.front])).await;
    let p = playlist_as(app, &o.le, &o.lobby, "P").await;
    app.call_as(&o.le, "PUT", &format!("/v1/playlists/{p}/tags"), Some(json!({ "tag_ids": [o.front] }))).await;
    let q = playlist_as(app, &o.ke, &o.kitchen, "Q").await;
    app.call_as(&o.ke, "PUT", &format!("/v1/playlists/{q}/tags"), Some(json!({ "tag_ids": [o.front] }))).await;

    let once = |team: &str, pl: &str, a: u32, b: u32| {
        json!({ "team_id": team, "playlist_id": pl, "kind": "once", "tag_ids": [o.front], "starts_at": at(1, a), "ends_at": at(1, b) })
    };
    let r = app.call_as(&o.le, "POST", "/v1/schedule", Some(once(&o.lobby, &p, 10, 12))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let first = r.json()["id"].as_str().unwrap().to_string();
    assert_eq!(r.json()["created_by"], "u-lobby");

    // Preview shows the clash before saving.
    let r = app.call_as(&o.ke, "POST", "/v1/schedule/preview", Some(once(&o.kitchen, &q, 11, 13))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let pv = &r.json()["screens"][0];
    assert_eq!(pv["screen_id"], screen);
    assert_eq!(pv["clashes"][0]["entry_ids"][0], first);
    assert_eq!(pv["covers"][0]["entry_id"], first);

    let r = app.call_as(&o.ke, "POST", "/v1/schedule", Some(once(&o.kitchen, &q, 11, 13))).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.body);
    let e = r.json();
    assert_eq!(e["code"], "schedule_conflict");
    assert_eq!((e["conflicts"][0]["entry_ids"][0].as_str(), e["conflicts"][0]["screen_id"].as_str()), (Some(first.as_str()), Some(screen.as_str())));
    // Back-to-back is fine, and weekly entries only layer.
    assert_eq!(app.call_as(&o.ke, "POST", "/v1/schedule", Some(once(&o.kitchen, &q, 12, 13))).await.status, StatusCode::CREATED);
    let weekly = json!({ "team_id": o.kitchen, "playlist_id": q, "kind": "weekly", "days": [1,2,3,4,5,6,7], "start_time": "09:00", "end_time": "17:00" });
    assert_eq!(app.call_as(&o.ke, "POST", "/v1/schedule", Some(weekly)).await.status, StatusCode::CREATED);
    // A team can't target screens or tags beyond its reach.
    let mut bad = once(&o.lobby, &p, 14, 15);
    bad["tag_ids"] = json!([o.back]);
    assert_eq!(app.call_as(&o.le, "POST", "/v1/schedule", Some(bad)).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    // Read-only visibility of the other team's entries on reached screens.
    let all = app.call_as(&o.le, "GET", "/v1/schedule?scope=visible", None).await.json();
    assert_eq!(all["entries"].as_array().unwrap().len(), 3);
    let own = app.call_as(&o.le, "GET", "/v1/schedule", None).await.json();
    assert_eq!(own["entries"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn retag_clash_is_allowed_and_flagged() {
    let o = org().await;
    let app = &o.app;
    let (screen, _) = pair_tagged(app, "Front", json!([o.front])).await;
    let p = playlist_as(app, &o.le, &o.lobby, "P").await;
    app.call_as(&o.le, "PUT", &format!("/v1/playlists/{p}/tags"), Some(json!({ "tag_ids": [o.front] }))).await;
    let q = playlist_as(app, &o.ke, &o.kitchen, "Q").await;
    app.call_as(&o.ke, "PUT", &format!("/v1/playlists/{q}/tags"), Some(json!({ "tag_ids": [o.back] }))).await;
    let once = |team: &str, pl: &str, tag: &str, a: u32, b: u32| {
        json!({ "team_id": team, "playlist_id": pl, "kind": "once", "tag_ids": [tag], "starts_at": at(1, a), "ends_at": at(1, b) })
    };
    let lobby_entry = app.call_as(&o.le, "POST", "/v1/schedule", Some(once(&o.lobby, &p, &o.front, 10, 12))).await.json()["id"].as_str().unwrap().to_string();
    let r = app.call_as(&o.ke, "POST", "/v1/schedule", Some(once(&o.kitchen, &q, &o.back, 11, 13))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let kitchen_entry = r.json()["id"].as_str().unwrap().to_string();

    // The admin adds `back` to the screen: Kitchen now reaches it and both one-offs play there at 11-12.
    let r = app.call("PUT", &format!("/v1/screens/{screen}/tags"), Some(json!({ "tag_ids": [o.front, o.back] }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let clash = &r.json()["clashes"][0];
    assert_eq!(clash["entry_ids"], json!([lobby_entry, kitchen_entry]));
    assert_eq!(clash["screen_id"], screen);
    let entries = app.call("GET", "/v1/schedule", None).await.json();
    assert!(entries["entries"].as_array().unwrap().iter().all(|e| e["needs_resolution"] == true), "{entries}");
    let conflict = |token: &str| {
        let token = token.to_string();
        async move {
            app.call_as(&token, "GET", "/v1/notices", None).await.json().as_array().unwrap().iter().filter(|n| n["kind"] == "schedule_conflict").count()
        }
    };
    assert_eq!(conflict(&o.le).await, 1);
    assert_eq!(conflict(&o.ke).await, 1);
    assert_eq!(conflict(&app.token).await, 2);

    // The newer entry plays while it's unresolved.
    let from = format!("{}:00Z", at(1, 11));
    let to = format!("{}:00Z", at(1, 12));
    let t = app.call("GET", &format!("/v1/schedule/timeline?from={from}&to={to}"), None).await.json();
    let segs = &t["screens"][0]["segments"];
    assert_eq!((segs[0]["entry_id"].as_str(), segs[0]["team_id"].as_str()), (Some(kitchen_entry.as_str()), Some(o.kitchen.as_str())));

    // Saving it unchanged is still refused; moving it clear resolves both.
    let r = app.call_as(&o.ke, "PUT", &format!("/v1/schedule/{kitchen_entry}"), Some(once(&o.kitchen, &q, &o.back, 11, 13))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = app.call_as(&o.ke, "PUT", &format!("/v1/schedule/{kitchen_entry}"), Some(once(&o.kitchen, &q, &o.back, 12, 13))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["needs_resolution"], false);
    assert_eq!(conflict(&o.le).await, 0);
    assert_eq!(conflict(&o.ke).await, 0);
    let entries = app.call("GET", "/v1/schedule", None).await.json();
    assert!(entries["entries"].as_array().unwrap().iter().all(|e| e["needs_resolution"] == false));
}

// MARK: Libraries

async fn folder_as(app: &App, token: &str, library: &str, parent: Option<&str>, name: &str) -> Resp {
    let mut body = json!({ "library_id": library, "name": name });
    if let Some(p) = parent {
        body["parent_id"] = json!(p);
    }
    app.call_as(token, "POST", "/v1/media-folders", Some(body)).await
}

async fn file_as(app: &App, token: &str, folder: &str, name: &str, data: &[u8]) -> Resp {
    let sha = hex::encode(Sha256::digest(data));
    let up = app
        .call_as(token, "POST", "/v1/media/uploads", Some(json!({ "folder_id": folder, "name": name, "sha256": sha, "size": data.len(), "mime": "image/png" })))
        .await;
    if up.status != StatusCode::OK {
        return up;
    }
    let up = up.json();
    if !up["already_present"].as_bool().unwrap() {
        let url = up["url"].as_str().unwrap();
        if url.starts_with('/') {
            let r = app.send("PUT", url, Some(token), &[], Body::from(data.to_vec()), false).await;
            assert!(r.status.is_success(), "{:?}", r.body);
        } else {
            let mut req = reqwest::Client::new().put(url).body(data.to_vec());
            for (k, v) in up["headers"].as_object().unwrap() {
                req = req.header(k.as_str(), v.as_str().unwrap());
            }
            assert!(req.send().await.unwrap().status().is_success());
        }
    }
    app.call_as(token, "POST", "/v1/media", Some(json!({ "folder_id": folder, "name": name, "kind": "image", "sha256": sha, "mime": "image/png" })))
        .await
}

fn id(r: &Resp) -> String {
    assert!(r.status.is_success(), "{} {:?}", r.status, r.body);
    r.json()["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn folder_reach_tags_grants_and_inheritance() {
    let o = org().await;
    let app = &o.app;
    let admin = app.token.clone();

    let r = folder_as(app, &o.le, "managed", None, "Mine").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let shared = id(&folder_as(app, &admin, "managed", None, "Shared").await);
    let r = app.call("PUT", &format!("/v1/media-folders/{shared}/tags"), Some(json!({ "tag_ids": [o.front] }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);

    // Lobby reaches it through its tag; subfolders inherit; viewers can't write.
    let loops = id(&folder_as(app, &o.le, "managed", Some(&shared), "Loops").await);
    assert_eq!(folder_as(app, &o.lv, "managed", Some(&shared), "Nope").await.status, StatusCode::FORBIDDEN);
    let r = app.call_as(&o.lv, "GET", "/v1/media-folders", None).await.json();
    let access: Vec<(&str, &str)> = r.as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap(), f["access"].as_str().unwrap())).collect();
    assert_eq!(access, vec![("Shared", "view"), ("Shared/Loops", "view")]);
    assert_eq!(app.call_as(&o.ke, "GET", "/v1/media-folders", None).await.json().as_array().unwrap().len(), 0);
    let r = app.call_as(&o.le, "GET", &format!("/v1/media-folders/{loops}"), None).await.json();
    assert_eq!(r["inherited_tag_ids"], json!([o.front]));

    // Lobby grants Loops to Kitchen, but can't tag with a tag it doesn't hold.
    let r = app.call_as(&o.le, "PUT", &format!("/v1/media-folders/{loops}/grants"), Some(json!({ "team_ids": [o.kitchen] }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let r = app.call_as(&o.le, "PUT", &format!("/v1/media-folders/{loops}/tags"), Some(json!({ "tag_ids": [o.back] }))).await;
    assert_eq!(code(r, StatusCode::FORBIDDEN).await, "tag_not_held");
    let r = app.call_as(&o.ke, "GET", "/v1/media-folders", None).await.json();
    let access: Vec<(&str, &str)> = r.as_array().unwrap().iter().map(|f| (f["path"].as_str().unwrap(), f["access"].as_str().unwrap())).collect();
    assert_eq!(access, vec![("Shared", "none"), ("Shared/Loops", "edit")]);

    // Kitchen uploads; both teams can use the file.
    let media = id(&file_as(app, &o.ke, &loops, "Welcome.png", b"welcome").await);
    let r = file_as(app, &o.ke, &loops, "welcome.PNG", b"other").await;
    assert_eq!(code(r, StatusCode::CONFLICT).await, "name_taken");
    let item = json!([{ "kind": "image", "media_id": media }]);
    let lp = id(&app.call_as(&o.le, "POST", "/v1/playlists", Some(json!({ "team_id": o.lobby, "name": "Lobby loop", "items": item }))).await);
    let kp = app.call_as(&o.ke, "POST", "/v1/playlists", Some(json!({ "team_id": o.kitchen, "name": "Kitchen loop", "items": item }))).await;
    let kp_json = kp.json();
    let kp = id(&kp);

    // Kitchen loses reach: its playlist keeps the file read-only, but can't add it again.
    let r = app.call_as(&o.le, "PUT", &format!("/v1/media-folders/{loops}/grants"), Some(json!({ "team_ids": [] }))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(app.call_as(&o.ke, "GET", &format!("/v1/media/{media}"), None).await.json()["access"], "playlist");
    let r = app
        .call_as(&o.ke, "PUT", &format!("/v1/playlists/{kp}"), Some(json!({ "base_version": 1, "name": "Kitchen loop 2", "loop": true, "defaults": kp_json["defaults"], "items": item, "beds": [] })))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let r = app.call_as(&o.ke, "POST", "/v1/playlists", Some(json!({ "team_id": o.kitchen, "name": "New", "items": item }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "invalid_playlist");

    // Usage shows both playlists; the other team's only by name.
    let u = app.call_as(&o.le, "GET", &format!("/v1/media-folders/{shared}/usage"), None).await.json();
    assert_eq!(u["media_count"], 1);
    let rows: Vec<(&str, bool)> = u["playlists"].as_array().unwrap().iter().map(|p| (p["name"].as_str().unwrap(), p["member"].as_bool().unwrap())).collect();
    assert_eq!(rows, vec![("Kitchen loop 2", false), ("Lobby loop", true)]);

    // Renaming the top folder (admin) moves the file's path.
    let r = app.call("PATCH", &format!("/v1/media-folders/{shared}"), Some(json!({ "name": "Common" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(app.call("GET", &format!("/v1/media/{media}"), None).await.json()["path"], "Common/Loops/Welcome.png");
    let r = app.call_as(&o.le, "PATCH", &format!("/v1/media-folders/{shared}"), Some(json!({ "name": "Mine" }))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // Deleting the folder leaves the file missing and tells Kitchen's last editor.
    let r = app.call_as(&o.le, "DELETE", &format!("/v1/media-folders/{loops}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(app.call_as(&o.le, "GET", &format!("/v1/media/{media}"), None).await.json()["missing"], true);
    let n = app.call_as(&o.ke, "GET", "/v1/notices", None).await.json();
    let n: Vec<&Value> = n.as_array().unwrap().iter().filter(|n| n["kind"] == "media_missing").collect();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0]["detail"]["media_id"], media.as_str());
    let lv = app.call_as(&o.le, "GET", &format!("/v1/playlists/{lp}"), None).await.json();
    assert_eq!(lv["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn manifest_leaves_out_missing_media() {
    let app = App::new().await;
    let g = app.team().await;
    let (a, _) = app.media(&g, "image", "image/png", b"a").await;
    let (b, sha_b) = app.media(&g, "image", "image/png", b"b").await;
    let (track, _) = app.media(&g, "audio", "audio/mp4", b"t").await;
    let ia = "00000000-0000-4000-8000-00000000000a";
    let ib = "00000000-0000-4000-8000-00000000000b";
    let r = app
        .call(
            "POST",
            "/v1/playlists",
            Some(json!({ "team_id": g, "name": "P", "items": [
                { "id": ia, "kind": "image", "media_id": a },
                { "id": ib, "kind": "image", "media_id": b },
            ], "beds": [{ "start_item_id": ia, "end_item_id": ib, "tracks": [track] }] })),
        )
        .await;
    let pl = id(&r);
    app.tag_playlist(&g, &pl).await;
    let (_, token) = pair_tagged(&app, "Lobby", json!([app.tag(&g)])).await;
    app.call("DELETE", &format!("/v1/media/{a}"), None).await;
    let m = app.call_as(&token, "GET", "/v1/device/manifest", None).await.json();
    let p = &m["playlists"][0];
    assert_eq!(p["items"].as_array().unwrap().len(), 1);
    assert_eq!(p["items"][0]["media_id"], b.as_str());
    assert_eq!(p["beds"][0]["start_item_id"], ib);
    let shas: Vec<&str> = m["assets"].as_array().unwrap().iter().map(|x| x["sha256"].as_str().unwrap()).collect();
    assert!(shas.contains(&sha_b.as_str()));
    assert!(m["media"].as_array().unwrap().iter().all(|x| x["id"] != a.as_str()));
}

#[tokio::test]
async fn local_library_paths_moves_and_quota() {
    let roots = tempfile::tempdir().unwrap();
    let root = roots.path().canonicalize().unwrap();
    let r2 = root.clone();
    let app = App::with(move |c| c.library_roots = vec![r2]).await;
    let g = app.team().await;

    let r = app.call("POST", "/v1/libraries", Some(json!({ "name": "Elsewhere", "kind": "local", "path": "/tmp/../etc" }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "path_not_allowed");
    let r = app.call("POST", "/v1/libraries", Some(json!({ "name": "Elsewhere", "kind": "local", "path": "/var/empty/x" }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "path_not_allowed");
    let dir = root.join("signage");
    let lib = id(&app.call("POST", "/v1/libraries", Some(json!({ "name": "Drive", "kind": "local", "path": dir.to_str().unwrap() }))).await);

    let top = id(&folder_as(&app, &app.token, &lib, None, "Lobby").await);
    assert!(dir.join("Lobby").is_dir());
    app.call("PUT", &format!("/v1/media-folders/{top}/grants"), Some(json!({ "team_ids": [g] }))).await;
    let r = file_as(&app, &app.token, &top, "Hello.png", b"hello local").await;
    let media = id(&r);
    let sha = r.json()["sha256"].as_str().unwrap().to_string();
    assert_eq!(std::fs::read(dir.join("Lobby/Hello.png")).unwrap(), b"hello local");
    assert!(!app.state.blob_path(&sha).exists(), "the staged blob is cleaned up");
    let r = app.call("GET", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(&r.body[..], b"hello local");

    let pl = id(&app.call("POST", "/v1/playlists", Some(json!({ "team_id": g, "name": "P", "items": [{ "kind": "image", "media_id": media }] }))).await);

    // Rename and folder rename happen on disk.
    app.call("PATCH", &format!("/v1/media/{media}"), Some(json!({ "name": "Hi.png" }))).await;
    assert!(dir.join("Lobby/Hi.png").exists() && !dir.join("Lobby/Hello.png").exists());
    let r = app.call("PATCH", &format!("/v1/media-folders/{top}"), Some(json!({ "name": "Foyer" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert!(dir.join("Foyer/Hi.png").exists());

    // Moving to the managed library copies the bytes and keeps the sha and the playlist.
    let managed = app.folder(&g).await;
    let r = app.call("PATCH", &format!("/v1/media/{media}"), Some(json!({ "folder_id": managed }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["library_id"], "managed");
    assert_eq!(r.json()["sha256"], sha.as_str());
    assert!(!dir.join("Foyer/Hi.png").exists());
    assert!(app.state.blob_path(&sha).exists());
    let p = app.call("GET", &format!("/v1/playlists/{pl}"), None).await.json();
    assert_eq!(p["items"][0]["media_id"], media.as_str());

    // A quota smaller than the file refuses the upload.
    let r = app.call("PATCH", &format!("/v1/libraries/{lib}"), Some(json!({ "quota_bytes": 3 }))).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = file_as(&app, &app.token, &top, "Big.png", b"too big for it").await;
    assert_eq!(r.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(r.json()["free_bytes"], 3);

    let r = app.call("DELETE", &format!("/v1/libraries/{lib}"), None).await;
    assert_eq!(code(r, StatusCode::CONFLICT).await, "library_not_empty");
    let r = app.call("DELETE", "/v1/libraries/managed", None).await;
    assert_eq!(code(r, StatusCode::CONFLICT).await, "library_managed");
}

/// A tiny path-style S3: PUT (with checksum check and copy), GET, HEAD, DELETE, ListObjectsV2 and
/// multipart uploads (with part checksums and part copies).
async fn fake_s3() -> (String, std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>) {
    use axum::{extract::Request as AxReq, response::IntoResponse};
    use base64::Engine;
    type Store = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>;
    type Meta = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (Option<String>, Option<String>)>>>;
    struct Upload {
        key: String,
        meta_sha: Option<String>,
        checksums: bool,
        parts: std::collections::BTreeMap<u32, (Vec<u8>, String)>,
    }
    type Uploads = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Upload>>>;
    let store: Store = Default::default();
    let s2 = store.clone();
    let meta: Meta = Default::default();
    let uploads: Uploads = Default::default();
    let handler = move |req: AxReq| {
        let store = s2.clone();
        let meta = meta.clone();
        let uploads = uploads.clone();
        async move {
            let method = req.method().clone();
            let uri = req.uri().clone();
            let headers = req.headers().clone();
            let path = percent_decode(uri.path().trim_start_matches('/'));
            let (_, key) = path.split_once('/').unwrap_or((path.as_str(), ""));
            let key = key.to_string();
            let query = uri.query().unwrap_or_default().to_string();
            let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
            let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD.encode(Sha256::digest(d));
            let param = |name: &str| query.split('&').find_map(|kv| kv.strip_prefix(&format!("{name}="))).map(percent_decode);
            let header = |name: &str| headers.get(name).map(|v| v.to_str().unwrap().to_string());
            let source = |src: String| {
                let src = percent_decode(src.trim_start_matches('/'));
                src.split_once('/').unwrap().1.to_string()
            };
            let xml_ns = "xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"";
            if method == "POST" && query.split('&').any(|kv| kv.starts_with("uploads")) {
                let id = uuid::Uuid::new_v4().simple().to_string();
                let up = Upload { key: key.clone(), meta_sha: header("x-amz-meta-sha256"), checksums: header("x-amz-checksum-algorithm").is_some(), parts: Default::default() };
                uploads.lock().unwrap().insert(id.clone(), up);
                return format!("<?xml version=\"1.0\"?><InitiateMultipartUploadResult {xml_ns}><Bucket>media</Bucket><Key>{key}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>").into_response();
            }
            if let Some(upload_id) = param("uploadId") {
                let mut ups = uploads.lock().unwrap();
                match method.as_str() {
                    "PUT" => {
                        let Some(up) = ups.get_mut(&upload_id) else { return StatusCode::NOT_FOUND.into_response() };
                        let n: u32 = param("partNumber").unwrap().parse().unwrap();
                        if let Some(src) = header("x-amz-copy-source") {
                            let data = store.lock().unwrap().get(&source(src)).cloned().unwrap();
                            let range = header("x-amz-copy-source-range").unwrap();
                            let (a, b) = range.trim_start_matches("bytes=").split_once('-').unwrap();
                            let part = data[a.parse::<usize>().unwrap()..=b.parse::<usize>().unwrap()].to_vec();
                            let etag = format!("\"{}\"", hex::encode(&Sha256::digest(&part)[..16]));
                            up.parts.insert(n, (part, etag.clone()));
                            return format!("<?xml version=\"1.0\"?><CopyPartResult {xml_ns}><ETag>{}</ETag></CopyPartResult>", etag.replace('"', "&quot;")).into_response();
                        }
                        if header("x-amz-checksum-sha256").is_some_and(|c| c != b64(&body)) {
                            return StatusCode::BAD_REQUEST.into_response();
                        }
                        let etag = format!("\"{}\"", hex::encode(&Sha256::digest(&body)[..16]));
                        up.parts.insert(n, (body, etag.clone()));
                        return ([(header::ETAG, etag)]).into_response();
                    }
                    "POST" => {
                        let Some(up) = ups.remove(&upload_id) else { return StatusCode::NOT_FOUND.into_response() };
                        let text = String::from_utf8(body).unwrap();
                        let mut data = vec![];
                        let mut digests = vec![];
                        for chunk in text.split("<Part>").skip(1) {
                            let field = |t: &str| chunk.split(&format!("<{t}>")).nth(1).and_then(|r| r.split(&format!("</{t}>")).next()).map(String::from);
                            let n: u32 = field("PartNumber").unwrap().parse().unwrap();
                            let Some((part, etag)) = up.parts.get(&n) else {
                                return "<Error><Code>InvalidPart</Code></Error>".into_response();
                            };
                            if field("ETag").as_deref() != Some(etag.as_str()) {
                                return "<Error><Code>InvalidPart</Code></Error>".into_response();
                            }
                            if up.checksums && field("ChecksumSHA256") != Some(b64(part)) {
                                return StatusCode::BAD_REQUEST.into_response();
                            }
                            digests.extend_from_slice(&Sha256::digest(part));
                            data.extend_from_slice(part);
                        }
                        let checksum = up.checksums.then(|| format!("{}-{}", b64(&digests), up.parts.len()));
                        store.lock().unwrap().insert(up.key.clone(), data);
                        meta.lock().unwrap().insert(up.key.clone(), (checksum, up.meta_sha.clone()));
                        return format!("<?xml version=\"1.0\"?><CompleteMultipartUploadResult {xml_ns}><ETag>\"x\"</ETag></CompleteMultipartUploadResult>").into_response();
                    }
                    "DELETE" => {
                        ups.remove(&upload_id);
                        return StatusCode::NO_CONTENT.into_response();
                    }
                    _ => return StatusCode::METHOD_NOT_ALLOWED.into_response(),
                }
            }
            match method.as_str() {
                "GET" if query.contains("list-type=2") => {
                    let prefix = query
                        .split('&')
                        .find_map(|kv| kv.strip_prefix("prefix="))
                        .map(percent_decode)
                        .unwrap_or_default();
                    let keys: Vec<String> = store.lock().unwrap().keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
                    let items: String = keys.iter().map(|k| format!("<Contents><Key>{k}</Key><Size>1</Size></Contents>")).collect();
                    format!("<?xml version=\"1.0\"?><ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsTruncated>false</IsTruncated>{items}</ListBucketResult>").into_response()
                }
                "PUT" => {
                    let copied_meta = header("x-amz-copy-source").and_then(|src| meta.lock().unwrap().get(&source(src)).map(|m| (None, m.1.clone())));
                    let data = match header("x-amz-copy-source") {
                        Some(src) => match store.lock().unwrap().get(&source(src)) {
                            Some(d) => d.clone(),
                            None => return StatusCode::NOT_FOUND.into_response(),
                        },
                        None => body,
                    };
                    match copied_meta {
                        Some(m) => meta.lock().unwrap().insert(key.clone(), m),
                        None => meta.lock().unwrap().remove(&key),
                    };
                    if let Some(c) = headers.get("x-amz-checksum-sha256") {
                        if c.to_str().unwrap() != b64(&data) {
                            return StatusCode::BAD_REQUEST.into_response();
                        }
                    }
                    store.lock().unwrap().insert(key, data);
                    StatusCode::OK.into_response()
                }
                "HEAD" => match store.lock().unwrap().get(&key) {
                    Some(d) => {
                        let (checksum, meta_sha) = meta.lock().unwrap().get(&key).cloned().unwrap_or((Some(b64(d)), None));
                        let mut h = axum::http::HeaderMap::new();
                        h.insert(header::CONTENT_LENGTH, d.len().into());
                        if let Some(c) = checksum {
                            h.insert("x-amz-checksum-sha256", c.parse().unwrap());
                        }
                        if let Some(m) = meta_sha {
                            h.insert("x-amz-meta-sha256", m.parse().unwrap());
                        }
                        h.into_response()
                    }
                    None => StatusCode::NOT_FOUND.into_response(),
                },
                "GET" => match store.lock().unwrap().get(&key) {
                    Some(d) => d.clone().into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                },
                "DELETE" => {
                    store.lock().unwrap().remove(&key);
                    meta.lock().unwrap().remove(&key);
                    StatusCode::NO_CONTENT.into_response()
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, Router::new().fallback(handler)).await.unwrap() });
    (format!("http://{addr}"), store)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

#[tokio::test]
async fn s3_library_uploads_redirects_and_moves() {
    let (endpoint, store) = fake_s3().await;
    let o = org().await;
    let app = &o.app;
    let s3 = json!({ "endpoint": endpoint, "region": "us-east-1", "bucket": "media", "prefix": "signage", "path_style": true,
                     "access_key_id": "AKID", "secret_access_key": "SECRET" });
    let lib = id(&app.call("POST", "/v1/libraries", Some(json!({ "name": "Cloud", "kind": "s3", "s3": s3 }))).await);
    assert!(store.lock().unwrap().is_empty(), "the probe cleans up");
    let libs = app.call("GET", "/v1/libraries", None).await.json();
    let cloud = libs.as_array().unwrap().iter().find(|l| l["id"] == lib.as_str()).unwrap().clone();
    assert_eq!(cloud["s3"]["bucket"], "media");
    assert!(cloud["s3"].get("secret_access_key").is_none());
    let libs = app.call_as(&o.le, "GET", "/v1/libraries", None).await.json();
    assert!(libs.as_array().unwrap().iter().all(|l| l.get("s3").is_none()));

    let top = id(&folder_as(app, &app.token, &lib, None, "Lobby").await);
    app.call("PUT", &format!("/v1/media-folders/{top}/tags"), Some(json!({ "tag_ids": [o.front] }))).await;
    let r = file_as(app, &o.le, &top, "Sky.png", b"sky bytes").await;
    let media = id(&r);
    let sha = r.json()["sha256"].as_str().unwrap().to_string();
    assert_eq!(store.lock().unwrap().get("signage/Lobby/Sky.png").unwrap(), b"sky bytes");

    // A wrong checksum is refused by S3 before the server records anything.
    let up = app
        .call_as(&o.le, "POST", "/v1/media/uploads", Some(json!({ "folder_id": top, "name": "Bad.png", "sha256": sha, "size": 3, "mime": "image/png" })))
        .await
        .json();
    let mut req = reqwest::Client::new().put(up["url"].as_str().unwrap()).body(b"bad".to_vec());
    for (k, v) in up["headers"].as_object().unwrap() {
        req = req.header(k.as_str(), v.as_str().unwrap());
    }
    assert_eq!(req.send().await.unwrap().status(), reqwest::StatusCode::BAD_REQUEST);

    // Players are redirected to S3.
    let r = app.call("GET", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.status, StatusCode::FOUND);
    let loc = r.headers[header::LOCATION].to_str().unwrap().to_string();
    assert!(loc.starts_with(&endpoint) && loc.contains("X-Amz-Signature"));
    assert_eq!(&reqwest::get(&loc).await.unwrap().bytes().await.unwrap()[..], b"sky bytes");

    // Renames copy within the bucket; moving a managed file in uploads it.
    app.call_as(&o.le, "PATCH", &format!("/v1/media/{media}"), Some(json!({ "name": "Clouds.png" }))).await;
    assert!(store.lock().unwrap().contains_key("signage/Lobby/Clouds.png"));
    assert!(!store.lock().unwrap().contains_key("signage/Lobby/Sky.png"));
    let (other, other_sha) = app.media(&o.lobby, "image", "image/png", b"managed bytes").await;
    let r = app.call("PATCH", &format!("/v1/media/{other}"), Some(json!({ "folder_id": top, "name": "Moved.png" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(store.lock().unwrap().get("signage/Lobby/Moved.png").unwrap(), b"managed bytes");
    assert!(!app.state.blob_path(&other_sha).exists());

    // Folder renames and deletes reach the bucket.
    let r = app.call("PATCH", &format!("/v1/media-folders/{top}"), Some(json!({ "name": "Foyer" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert!(store.lock().unwrap().contains_key("signage/Foyer/Clouds.png"));
    app.call("DELETE", &format!("/v1/media-folders/{top}"), None).await;
    assert!(store.lock().unwrap().is_empty());
}

#[tokio::test]
async fn s3_multipart_uploads_copies_and_moves() {
    let (endpoint, store) = fake_s3().await;
    let o = org().await;
    let app = &o.app;
    let s3 = json!({ "endpoint": endpoint, "region": "us-east-1", "bucket": "media", "path_style": true,
                     "access_key_id": "AKID", "secret_access_key": "SECRET", "part_size_mib": 5 });
    let lib = id(&app.call("POST", "/v1/libraries", Some(json!({ "name": "Cloud", "kind": "s3", "s3": s3 }))).await);
    let r = app.call("PATCH", &format!("/v1/libraries/{lib}"), Some(json!({ "s3": { "part_size_mib": 4 } }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let top = id(&folder_as(app, &app.token, &lib, None, "Lobby").await);
    app.call("PUT", &format!("/v1/media-folders/{top}/tags"), Some(json!({ "tag_ids": [o.front] }))).await;

    let mib = 1024 * 1024;
    let data: Vec<u8> = (0..11 * mib + 7).map(|i| (i % 251) as u8).collect();
    let sha = hex::encode(Sha256::digest(&data));
    let input = json!({ "folder_id": top, "name": "Big.mov", "sha256": sha, "size": data.len(), "mime": "video/quicktime" });
    let up = app.call_as(&o.le, "POST", "/v1/media/uploads", Some(input.clone())).await.json();
    assert_eq!(up["already_present"], false, "{up}");
    assert!(up.get("url").is_none());
    let mp = &up["multipart"];
    let (upload, part_size, count) = (mp["upload_id"].as_str().unwrap().to_string(), mp["part_size"].as_u64().unwrap() as usize, mp["part_count"].as_u64().unwrap());
    assert_eq!((part_size, count), (5 * mib, 3));

    // Only the uploader can touch it.
    let r = app.call("POST", &format!("/v1/media/uploads/{upload}/parts"), Some(json!({ "parts": [{ "number": 1, "sha256": sha }] }))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call_as(&o.le, "POST", &format!("/v1/media/uploads/{upload}/parts"), Some(json!({ "parts": [{ "number": 4, "sha256": sha }] }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let chunks: Vec<&[u8]> = data.chunks(part_size).collect();
    let parts: Vec<Value> = chunks.iter().enumerate().map(|(i, c)| json!({ "number": i + 1, "sha256": hex::encode(Sha256::digest(c)) })).collect();
    let signed = app.call_as(&o.le, "POST", &format!("/v1/media/uploads/{upload}/parts"), Some(json!({ "parts": parts }))).await.json();
    let mut etags = vec![];
    for (part, chunk) in signed.as_array().unwrap().iter().zip(&chunks) {
        let mut req = reqwest::Client::new().put(part["url"].as_str().unwrap()).body(chunk.to_vec());
        for (k, v) in part["headers"].as_object().unwrap() {
            req = req.header(k.as_str(), v.as_str().unwrap());
        }
        let resp = req.send().await.unwrap();
        assert!(resp.status().is_success());
        etags.push(json!({ "number": part["number"], "etag": resp.headers()["etag"].to_str().unwrap() }));
    }
    // A part S3 refuses (wrong bytes for its hash).
    let mut req = reqwest::Client::new().put(signed[0]["url"].as_str().unwrap()).body(b"wrong".to_vec());
    for (k, v) in signed[0]["headers"].as_object().unwrap() {
        req = req.header(k.as_str(), v.as_str().unwrap());
    }
    assert_eq!(req.send().await.unwrap().status(), reqwest::StatusCode::BAD_REQUEST);

    let r = app.call_as(&o.le, "POST", &format!("/v1/media/uploads/{upload}/complete"), Some(json!({ "parts": &etags[..2] }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "upload_incomplete");
    let r = app.call_as(&o.le, "POST", &format!("/v1/media/uploads/{upload}/complete"), Some(json!({ "parts": etags }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{:?}", r.body);
    assert_eq!(store.lock().unwrap().get("Lobby/Big.mov").unwrap(), &data);
    let r = app.call_as(&o.le, "POST", &format!("/v1/media/uploads/{upload}/complete"), Some(json!({ "parts": [] }))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let r = app.call_as(&o.le, "POST", "/v1/media", Some(json!({ "folder_id": top, "name": "Big.mov", "kind": "video", "sha256": sha, "mime": "video/quicktime" }))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    let media = id(&r);
    assert_eq!(r.json()["size"], data.len());

    // A second upload of the same bytes to the same path is already present.
    let mut again = input.clone();
    again["name"] = json!("Big.mov");
    let r = app.call_as(&o.le, "POST", "/v1/media/uploads", Some(again)).await;
    assert_eq!(code(r, StatusCode::CONFLICT).await, "name_taken");

    // Renames copy in parts and keep the hash.
    let r = app.call_as(&o.le, "PATCH", &format!("/v1/media/{media}"), Some(json!({ "name": "Bigger.mov" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(store.lock().unwrap().get("Lobby/Bigger.mov").unwrap(), &data);
    assert!(!store.lock().unwrap().contains_key("Lobby/Big.mov"));

    // Aborting frees the upload.
    let mut other = input.clone();
    other["name"] = json!("Other.mov");
    let up = app.call_as(&o.le, "POST", "/v1/media/uploads", Some(other)).await.json();
    let upload = up["multipart"]["upload_id"].as_str().unwrap().to_string();
    let r = app.call_as(&o.le, "DELETE", &format!("/v1/media/uploads/{upload}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call_as(&o.le, "DELETE", &format!("/v1/media/uploads/{upload}"), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // Moving a large managed file in uploads it in parts.
    let big: Vec<u8> = (0..6 * mib).map(|i| (i % 241) as u8).collect();
    let (managed, _) = app.media(&o.lobby, "video", "video/quicktime", &big).await;
    let r = app.call("PATCH", &format!("/v1/media/{managed}"), Some(json!({ "folder_id": top, "name": "Moved.mov" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(store.lock().unwrap().get("Lobby/Moved.mov").unwrap(), &big);
}

#[tokio::test]
async fn media_source_sha256_lookup() {
    let app = App::new().await;
    let g = app.team().await;
    let folder = app.folder(&g).await;

    let (passthrough, sha) = app.media(&g, "image", "image/png", b"png-bytes").await;
    let r = app.call("GET", &format!("/v1/media?source_sha256={sha}"), None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    assert_eq!(r.json()[0]["id"], passthrough.as_str());
    assert_eq!(r.json()[0]["source_sha256"], sha.as_str());

    let original = hex::encode(Sha256::digest(b"original-prores"));
    let transcoded = app.upload(b"hevc-bytes").await;
    let r = app
        .call(
            "POST",
            "/v1/media",
            Some(json!({ "folder_id": folder, "name": "t.mov", "kind": "video", "sha256": transcoded, "mime": "video/quicktime", "source_sha256": original })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.body);
    assert_eq!(r.json()["source_sha256"], original.as_str());
    let r = app.call("GET", &format!("/v1/media?source_sha256={original}"), None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    assert_eq!(r.json()[0]["sha256"], transcoded.as_str());

    let r = app.call("GET", &format!("/v1/media?source_sha256={}", "f".repeat(64)), None).await;
    assert!(r.json().as_array().unwrap().is_empty());
    let r = app.call("GET", "/v1/media?source_sha256=nope", None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn media_replace_keeps_id_and_bumps_playlists() {
    let app = App::new().await;
    let g = app.team().await;
    let folder = app.folder(&g).await;
    let (media_id, old_sha) = app.media(&g, "video", "video/mp4", b"first-cut").await;
    let (other, _) = app.media(&g, "video", "video/mp4", b"other").await;
    let other_name = app.call("GET", &format!("/v1/media/{other}"), None).await.json()["name"].as_str().unwrap().to_string();
    let r = app
        .call("POST", "/v1/playlists", Some(json!({ "team_id": g, "name": "P", "items": [{ "kind": "video", "media_id": media_id }] })))
        .await;
    let pid = r.json()["id"].as_str().unwrap().to_string();

    let new_sha = hex::encode(Sha256::digest(b"second-cut"));
    let r = app
        .call("POST", "/v1/media/uploads", Some(json!({ "folder_id": folder, "name": "Cut.mov", "sha256": new_sha, "size": 10, "mime": "video/quicktime", "replaces": media_id })))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    app.upload(b"second-cut").await;

    let r = app
        .call("POST", &format!("/v1/media/{media_id}/replace"), Some(json!({ "name": "Cut.png", "kind": "image", "sha256": new_sha, "mime": "image/png" })))
        .await;
    assert_eq!(r.json()["code"], "kind_mismatch");
    let body = |name: &str| json!({ "name": name, "kind": "video", "sha256": new_sha, "mime": "video/quicktime", "duration_ms": 5000 });
    let r = app.call("POST", &format!("/v1/media/{media_id}/replace"), Some(body(&other_name))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    let r = app.call("POST", &format!("/v1/media/{media_id}/replace"), Some(body("Cut.mov"))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["id"], media_id.as_str());
    assert_eq!(r.json()["sha256"], new_sha.as_str());
    assert_eq!(r.json()["name"], "Cut.mov");
    assert_eq!(r.json()["size"], 10);
    assert_eq!(r.json()["duration_ms"], 5000);
    assert!(r.json().get("width").map_or(true, |w| w.is_null()));

    let r = app.call("HEAD", &format!("/v1/blobs/{old_sha}"), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app.call("GET", &format!("/v1/playlists/{pid}"), None).await;
    assert_eq!(r.json()["version"], 2);

    let r = app
        .call("POST", "/v1/media/uploads", Some(json!({ "folder_id": app.folder(&app.team().await).await, "name": "x.mov", "sha256": new_sha, "size": 10, "mime": "video/quicktime", "replaces": media_id })))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}
