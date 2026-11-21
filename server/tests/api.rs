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
        Self { router: router(state.clone()), token, state, _dir: dir }
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

    async fn team(&self) -> String {
        let r = self.call("POST", "/v1/teams", Some(json!({ "name": "Lobby" }))).await;
        assert_eq!(r.status, StatusCode::CREATED);
        r.json()["id"].as_str().unwrap().to_string()
    }

    async fn upload(&self, data: &[u8]) -> String {
        let sha = hex::encode(Sha256::digest(data));
        let r = self
            .send("PUT", &format!("/v1/blobs/{sha}"), Some(&self.token), &[], Body::from(data.to_vec()), false)
            .await;
        assert!(r.status == StatusCode::CREATED || r.status == StatusCode::OK);
        sha
    }

    async fn media(&self, team: &str, kind: &str, mime: &str, data: &[u8]) -> (String, String) {
        let sha = self.upload(data).await;
        let r = self
            .call("POST", "/v1/media", Some(json!({ "team_id": team, "kind": kind, "sha256": sha, "mime": mime, "title": "t" })))
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
    let id = app.team().await;
    let r = app.call("GET", "/v1/teams", None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    let r = app
        .call("PUT", &format!("/v1/teams/{id}"), Some(json!({ "name": "X", "oidc_bindings": [{ "claim_value": "a", "role": "editor" }] })))
        .await;
    assert_eq!(r.json()["oidc_bindings"][0]["role"], "editor");
    assert!(r.json().get("timezone").is_none());
    app.media(&id, "image", "image/png", b"png").await;
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

    let missing = "a".repeat(64);
    let r = app
        .call("POST", "/v1/media", Some(json!({ "team_id": g, "kind": "video", "sha256": missing, "mime": "video/mp4", "title": "x" })))
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    let (media_id, sha) = app.media(&g, "video", "video/mp4", b"video-bytes").await;
    let r = app.call("GET", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.headers[header::CONTENT_TYPE], "video/mp4");
    let r = app.call("PATCH", &format!("/v1/media/{media_id}"), Some(json!({ "title": "Renamed" }))).await;
    assert_eq!(r.json()["title"], "Renamed");
    assert_eq!(r.json()["size"], 11);
    let r = app.call("GET", &format!("/v1/media?team_id={g}&kind=video"), None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);

    let r = app
        .call("POST", "/v1/playlists", Some(json!({ "team_id": g, "name": "P", "items": [{ "kind": "video", "media_id": media_id }] })))
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let pid = r.json()["id"].as_str().unwrap().to_string();

    let r = app.call("DELETE", &format!("/v1/media/{media_id}"), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.json()["code"], "media_in_use");

    app.call("DELETE", &format!("/v1/playlists/{pid}"), None).await;
    let r = app.call("DELETE", &format!("/v1/media/{media_id}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call("HEAD", &format!("/v1/blobs/{sha}"), None).await;
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

    let bad_kind = json!([{ "kind": "image", "media_id": v1 }]);
    let r = app.call("PUT", &format!("/v1/playlists/{pid}"), Some(update(2, bad_kind, json!([])))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

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

    let r = app.call("POST", "/v1/pairing/claim", Some(json!({ "code": "ZZZZZZ", "name": name, "team_id": team }))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let mut claim = json!({ "code": code.to_lowercase(), "name": name, "team_id": team });
    if let Some(p) = playlist {
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
    assert_eq!(r.json().as_array().unwrap().len(), 0);
    let r = app.call("GET", "/v1/device/manifest", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    let r = app.call("POST", &format!("/v1/screens/{screen_id}/command"), Some(json!({ "name": "next" }))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    let r = app.call("PATCH", &format!("/v1/screens/{screen_id}"), Some(json!({ "playlist_id": null }))).await;
    assert!(r.json().get("playlist_id").is_none());
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
    let (screen_id, token) = pair(&app, &g, "Hall", None).await;

    let r = app.call_as(&token, "GET", "/v1/device/playlists", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let list = r.json();
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["A", "B"]);

    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "playlist_id": a }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{:?}", r.body);
    let r = app.call("GET", &format!("/v1/screens/{screen_id}"), None).await;
    assert_eq!(r.json()["playlist_id"], a);
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.json()["playlist"]["id"], a);

    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "playlist_id": foreign }))).await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "playlist_id": b }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call_as(&token, "PUT", "/v1/device/playlist", Some(json!({ "playlist_id": null }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call("GET", &format!("/v1/screens/{screen_id}"), None).await;
    assert!(r.json().get("playlist_id").is_none());

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
    assert_eq!(r.json()["playlist_id"], a);

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

async fn pair_node(app: &App, team: &str, name: &str) -> (String, String) {
    let r = app.send("POST", "/v1/pairing", None, &[], Body::from(json!({ "kind": "node" }).to_string()), true).await;
    assert_eq!(r.status, StatusCode::CREATED);
    let s = r.json();
    let poll = format!("/v1/pairing/{}?secret={}", s["pairing_id"].as_str().unwrap(), s["secret"].as_str().unwrap());
    let r = app
        .call("POST", "/v1/pairing/claim", Some(json!({ "code": s["code"], "name": name, "team_id": team })))
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
    assert_eq!(screen["output_key"], "main");
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
        { "key": "hdmi-1", "kind": "display", "name": "Left", "role": "screen" },
        { "key": "hdmi-2", "kind": "external", "name": "Right", "role": "screen", "format": "1080p60" },
        { "key": "dl-1", "kind": "decklink", "name": "DeckLink 1", "role": "none" },
        { "key": "v-1", "kind": "virtual", "name": "Virtual", "role": "confidence", "audio_device": "BlackHole" },
    ]);
    let r = app.call_as(&token, "PUT", "/v1/device/outputs", Some(outputs.clone())).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let bad = json!([{ "key": "a", "kind": "hdmi", "name": "A", "role": "screen" }]);
    assert_eq!(app.call_as(&token, "PUT", "/v1/device/outputs", Some(bad)).await.status, StatusCode::BAD_REQUEST);
    let dup = json!([
        { "key": "a", "kind": "display", "name": "A", "role": "screen" },
        { "key": "a", "kind": "display", "name": "B", "role": "screen" },
    ]);
    assert_eq!(app.call_as(&token, "PUT", "/v1/device/outputs", Some(dup)).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    // A reported screen_id is ignored. The stored role comes from assignments, which are still empty.
    let foreign = json!([{ "key": "a", "kind": "display", "name": "A", "role": "screen", "screen_id": "nope" }]);
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
    assert_eq!(left["team_id"], g);
    assert_eq!(left["node_id"], node_id);
    assert_eq!(left["output_key"], "hdmi-1");
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
        { "key": "a", "kind": "display", "name": "A", "role": "screen" },
        { "key": "b", "kind": "display", "name": "B", "role": "screen" },
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

    let (_conn, mut rx) = app.state.hub.register_node(&node_id, &g);
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
    let r = app.call("PATCH", &format!("/v1/screens/{a}"), Some(json!({ "playlist_id": null }))).await;
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
    let r = app.call("POST", "/v1/pairing/claim", Some(json!({ "code": s["code"], "name": name, "team_id": team }))).await;
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
    (node, token, r.json()["id"].as_str().unwrap().to_string())
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
    assert_eq!(cur["assignments"][0]["job"], "screen");
    assert_eq!(cur["assignments"][0]["screen_id"], screen);
    let node_json = app.call("GET", &format!("/v1/nodes/{node}"), None).await.json();
    assert_eq!((node_json["assignments_version"].as_i64(), node_json["applied_assignments_version"].as_i64()), (Some(1), Some(1)));

    let keep = json!({ "output_key": "a", "job": "screen", "screen_id": screen });
    let bad = |list: Value| put_a(&app, &token, 1, list);
    let c = code(bad(json!([keep, { "output_key": "zzz", "job": "control", "screen_id": screen }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "unknown_output");
    let c = code(bad(json!([keep, { "output_key": "dl", "job": "control", "screen_id": screen }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "control_needs_input");
    let c = code(bad(json!([keep, { "output_key": "v", "job": "control", "screen_id": screen }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "control_needs_input");
    // A second output cannot play the same screen.
    let c = code(bad(json!([keep, { "output_key": "b", "job": "screen", "screen_id": screen }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "screen_has_output");

    // Another node's screen: not local for mirror or screen; a foreign team's screen is not in the team.
    let (_n2, _t2, other) = player(&app, &g, "Other").await;
    let c = code(bad(json!([keep, { "output_key": "b", "job": "mirror", "screen_id": other }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "mirror_must_be_local");
    let c = code(bad(json!([keep, { "output_key": "b", "job": "screen", "screen_id": other }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "screen_must_be_local");
    let g2 = app.call("POST", "/v1/teams", Some(json!({ "name": "Elsewhere" }))).await.json()["id"].as_str().unwrap().to_string();
    let (_n3, _t3, far) = player(&app, &g2, "Far").await;
    let c = code(bad(json!([keep, { "output_key": "b", "job": "control", "screen_id": far }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "target_not_in_team");
    let c = code(bad(json!([keep, { "output_key": "b", "job": "control" }])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "screen_required");
    // Unknown outputs are only allowed when off.
    let ok = bad(json!([keep, { "output_key": "gone", "job": "off" }])).await;
    assert_eq!(ok.status, StatusCode::OK, "{:?}", ok.body);
    assert_eq!(ok.json()["version"], 2);

    // Audio: one enabled assignment per screen, across devices too.
    let audio = |key: &str, job: &str, sid: &str| json!({ "output_key": key, "job": job, "screen_id": sid, "settings": { "audio": { "enabled": true } } });
    let c = code(put_a(&app, &token, 2, json!([audio("a", "screen", &screen), audio("b", "mirror", &screen)])).await, StatusCode::UNPROCESSABLE_ENTITY).await;
    assert_eq!(c, "audio_conflict");
    let r = put_a(&app, &token, 2, json!([audio("a", "screen", &screen)])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let (_n4, t4) = pair_mode(&app, &g, "iPad", "controller").await;
    outputs(&app, &t4).await;
    let r = put_a(&app, &t4, 0, json!([audio("a", "control", &screen)])).await;
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
    for job in ["screen", "mirror"] {
        let r = put_a(&app, &token, 0, json!([{ "output_key": "a", "job": job, "screen_id": screen }])).await;
        assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "job_not_allowed_in_mode");
    }
    // Controller devices own no screens, so creating one is refused as well.
    let r = app.call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "a", "name": "x" }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "job_not_allowed_in_mode");
    // Control and confidence may target any screen in the team, on any output of a controller node.
    let r = put_a(&app, &token, 0, json!([
        { "output_key": "a", "job": "control", "screen_id": screen, "settings": { "view": "operator", "clock": 24 } },
        { "output_key": "b", "job": "confidence", "screen_id": screen, "settings": { "clock": 12 } },
        { "output_key": "dl", "job": "off" },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["assignments"].as_array().unwrap().len(), 3);
    let bad_clock = put_a(&app, &token, 1, json!([{ "output_key": "a", "job": "control", "screen_id": screen, "settings": { "clock": 5 } }])).await;
    assert_eq!(code(bad_clock, StatusCode::UNPROCESSABLE_ENTITY).await, "invalid_assignments");

    // team-screens lists everything in the team.
    let r = app.call_as(&token, "GET", "/v1/device/team-screens", None).await;
    let list = r.json();
    let s = list.as_array().unwrap().iter().find(|x| x["id"] == screen).unwrap();
    assert_eq!((s["node_name"].as_str(), s["node_mode"].as_str(), s["output_key"].as_str()), (Some("Mini"), Some("player"), Some("a")));
    assert_eq!((s["online"].as_bool(), s["placed"].as_bool()), (Some(false), Some(true)));
    let _ = node;
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
        { "output_key": "b", "job": "screen", "screen_id": second },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let s = app.call("GET", &format!("/v1/screens/{screen}"), None).await.json();
    assert!(s.get("node_id").is_none() && s.get("output_key").is_none(), "{s}");
    let listed = app.call("GET", &format!("/v1/screens?team_id={g}"), None).await.json();
    assert!(listed.as_array().unwrap().iter().any(|x| x["id"] == screen && x.get("node_id").is_none()));
    let n = app.call("GET", &format!("/v1/nodes/{node}"), None).await.json();
    assert_eq!(n["screen_ids"], json!([second]));
    // Unplaced screens can be taken over again ("use existing screen").
    let v = version(&app, &token).await;
    let r = put_a(&app, &token, v, json!([
        { "output_key": "a", "job": "screen", "screen_id": screen },
        { "output_key": "b", "job": "screen", "screen_id": second },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(app.call("GET", &format!("/v1/screens/{screen}"), None).await.json()["output_key"], "a");

    // Leaving player releases all screens and drops screen/mirror jobs but keeps control.
    let v = version(&app, &token).await;
    let r = put_a(&app, &token, v, json!([
        { "output_key": "a", "job": "screen", "screen_id": screen },
        { "output_key": "b", "job": "control", "screen_id": second },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let r = app.call_as(&token, "PUT", "/v1/device/mode", Some(json!({ "mode": "stage" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let body = r.json();
    assert_eq!(body["mode"], "stage");
    let jobs: Vec<_> = body["assignments"].as_array().unwrap().iter().map(|a| (a["output_key"].as_str().unwrap(), a["job"].as_str().unwrap())).collect();
    assert!(!jobs.iter().any(|(_, j)| *j == "screen" || *j == "mirror"), "{jobs:?}");
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
    let r = put_a(&app, &t1, v, json!([{ "output_key": "a", "job": "screen", "screen_id": screen, "settings": audio }])).await;
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
    let (other_node, _other_token, _) = {
        let g2 = app.call("POST", "/v1/teams", Some(json!({ "name": "Elsewhere" }))).await.json()["id"].as_str().unwrap().to_string();
        let (n, t) = pair_node(&app, &g2, "Far").await;
        (n, t, ())
    };
    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": other_node, "output_key": "a" }))).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "target_not_in_team");

    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": n2, "output_key": "a" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!((r.json()["node_id"].as_str(), r.json()["output_key"].as_str()), (Some(n2.as_str()), Some("a")));
    let old = app.call("GET", &format!("/v1/nodes/{n1}/assignments"), None).await.json();
    let a = old["assignments"].as_array().unwrap().iter().find(|a| a["output_key"] == "a").unwrap();
    assert_eq!(a["job"], "off");
    assert!(a.get("screen_id").is_none());
    assert_eq!(old["version"], 3);
    let new = app.call("GET", &format!("/v1/nodes/{n2}/assignments"), None).await.json();
    let a = new["assignments"].as_array().unwrap().iter().find(|a| a["output_key"] == "a").unwrap();
    assert_eq!((a["job"].as_str(), a["screen_id"].as_str()), (Some("screen"), Some(screen.as_str())));
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
async fn mirrors_follow_screen_jobs_and_moves_keep_their_own_audio() {
    let app = App::new().await;
    let g = app.team().await;
    let (node, token, screen) = player(&app, &g, "Mini").await;
    let v = version(&app, &token).await;
    // The node plays the screen, but this request drops that job and keeps a mirror.
    let r = put_a(&app, &token, v, json!([
        { "output_key": "a", "job": "off" },
        { "output_key": "b", "job": "mirror", "screen_id": screen },
    ])).await;
    assert_eq!(code(r, StatusCode::UNPROCESSABLE_ENTITY).await, "mirror_must_be_local");

    // Same-node move onto an output that mirrors a different screen turns that mirror off.
    let second = app.call_as(&token, "POST", "/v1/device/screens", Some(json!({ "output_key": "b", "name": "Second" }))).await;
    assert_eq!(second.status, StatusCode::CREATED, "{:?}", second.body);
    let second_id = second.json()["id"].as_str().unwrap().to_string();
    let v = version(&app, &token).await;
    let r = put_a(&app, &token, v, json!([
        { "output_key": "a", "job": "screen", "screen_id": screen },
        { "output_key": "b", "job": "screen", "screen_id": second_id, "settings": { "audio": { "enabled": true } } },
        { "output_key": "v", "job": "mirror", "screen_id": second_id },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    // Unplace `second` by moving `screen` onto its output. The mirror of `second` cannot stay.
    let r = app.call("POST", &format!("/v1/screens/{screen}/move"), Some(json!({ "node_id": node, "output_key": "b" }))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let body = app.call("GET", &format!("/v1/nodes/{node}/assignments"), None).await.json();
    let jobs: Vec<_> = body["assignments"].as_array().unwrap().iter().map(|a| (a["output_key"].as_str().unwrap(), a["job"].as_str().unwrap())).collect();
    assert!(jobs.contains(&("v", "off")), "{jobs:?}");
    assert!(jobs.iter().any(|(k, j)| *k == "b" && *j == "screen"));
    let moved = body["assignments"].as_array().unwrap().iter().find(|a| a["output_key"] == "b").unwrap();
    assert!(moved["settings"].get("audio").is_none(), "{moved}");
    let parked = app.call("GET", &format!("/v1/screens/{second_id}"), None).await.json();
    assert!(parked.get("node_id").is_none(), "{parked}");

    // An unplaced screen moved onto an output does not take that output's audio.
    let v = body["version"].as_i64().unwrap();
    let r = put_a(&app, &token, v, json!([
        { "output_key": "b", "job": "screen", "screen_id": screen, "settings": { "audio": { "enabled": true, "device": "HDMI" } } },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
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

    let r = put_a(&app, &ct, 0, json!([
        { "output_key": "a", "job": "control", "screen_id": screen },
        { "output_key": "b", "job": "confidence", "screen_id": other },
    ])).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);

    // The owner must be online to receive it.
    let r = app.call_as(&ct, "POST", &format!("/v1/device/screens/{screen}/command"), Some(cmd.clone())).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let owner = app.call("GET", &format!("/v1/screens/{screen}"), None).await.json()["node_id"].as_str().unwrap().to_string();
    let (_c, mut rx) = app.state.hub.register_node(&owner, &g);
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
    let (pl, _) = (app.call("POST", "/v1/playlists", Some(json!({ "team_id": g, "name": "Remote", "items": [] }))).await.json()["id"].as_str().unwrap().to_string(), ());
    let r = app.call_as(&ct, "GET", &format!("/v1/device/playlists?screen_id={screen}"), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert!(r.json().as_array().unwrap().iter().any(|p| p["id"] == pl));
    let r = app.call_as(&ct, "GET", &format!("/v1/device/playlists?screen_id={other}"), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call_as(&ct, "PUT", "/v1/device/playlist", Some(json!({ "screen_id": screen, "playlist_id": pl }))).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{:?}", r.body);
    assert_eq!(app.call("GET", &format!("/v1/screens/{screen}"), None).await.json()["playlist_id"], pl);
    let r = app.call_as(&ct, "PUT", "/v1/device/playlist", Some(json!({ "screen_id": other, "playlist_id": pl }))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app.call_as(&ct, "PUT", "/v1/device/playlist", Some(json!({ "screen_id": uuid::Uuid::new_v4().to_string(), "playlist_id": pl }))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let sha = app.upload(b"thumbnail bytes").await;
    let r = app.call_as(&ct, "GET", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    let _ = cn;
}

#[tokio::test]
async fn watchers_receive_status_manifest_and_assignment_messages() {
    let app = App::new().await;
    let g = app.team().await;
    let (pn, _pt, screen) = player(&app, &g, "Mini").await;
    let (cn, ct) = pair_mode(&app, &g, "iPad", "controller").await;
    outputs(&app, &ct).await;
    let (_pc, mut prx) = app.state.hub.register_node(&pn, &g);
    let (_cc, mut crx) = app.state.hub.register_node(&cn, &g);
    app.state.statuses.lock().insert(screen.clone(), screen_server::models::ScreenStatus::simple(screen_server::models::ScreenState::Playing));

    let r = put_a(&app, &ct, 0, json!([{ "output_key": "a", "job": "control", "screen_id": screen }])).await;
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
    let r = app.call("PATCH", &format!("/v1/screens/{screen}"), Some(json!({ "playlist_id": null }))).await;
    assert_eq!(r.status, StatusCode::OK);
    let m = recv_json(&mut crx).await;
    assert_eq!((m["type"].as_str(), m["screen_id"].as_str()), (Some("manifest_changed"), Some(screen.as_str())));
    let Some(Out::Msg(m)) = prx.recv().await else { panic!("no message") };
    assert!(m.contains("manifest_changed"));
    assert!(prx.try_recv().is_err());

    // Dropping the assignment stops the fan-out.
    let r = put_a(&app, &ct, 1, json!([{ "output_key": "a", "job": "off" }])).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(recv_json(&mut crx).await["type"], "assignments_changed");
    app.state.hub.send_subscribers(&screen, Some(&pn), &msg);
    assert!(crx.try_recv().is_err());
}
