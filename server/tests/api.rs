use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use screen_server::{build_state, config::Config, router};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct App {
    router: Router,
    token: String,
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
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::defaults(dir.path().to_path_buf());
        cfg.bonjour = false;
        let state = build_state(cfg).await.unwrap();
        let token = state.bootstrap.clone().unwrap();
        Self { router: router(state), token, _dir: dir }
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

    async fn group(&self) -> String {
        let r = self.call("POST", "/v1/groups", Some(json!({ "name": "Lobby" }))).await;
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

    async fn media(&self, group: &str, kind: &str, mime: &str, data: &[u8]) -> (String, String) {
        let sha = self.upload(data).await;
        let r = self
            .call("POST", "/v1/media", Some(json!({ "group_id": group, "kind": kind, "sha256": sha, "mime": mime, "title": "t" })))
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
async fn groups() {
    let app = App::new().await;
    let r = app.call("POST", "/v1/groups", Some(json!({ "name": "" }))).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let id = app.group().await;
    let r = app.call("GET", "/v1/groups", None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);
    let r = app
        .call("PUT", &format!("/v1/groups/{id}"), Some(json!({ "name": "X", "oidc_bindings": [{ "claim_value": "a", "role": "editor" }] })))
        .await;
    assert_eq!(r.json()["oidc_bindings"][0]["role"], "editor");
    app.media(&id, "image", "image/png", b"png").await;
    let r = app.call("DELETE", &format!("/v1/groups/{id}"), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
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
    let g = app.group().await;

    let missing = "a".repeat(64);
    let r = app
        .call("POST", "/v1/media", Some(json!({ "group_id": g, "kind": "video", "sha256": missing, "mime": "video/mp4", "title": "x" })))
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);

    let (media_id, sha) = app.media(&g, "video", "video/mp4", b"video-bytes").await;
    let r = app.call("GET", &format!("/v1/blobs/{sha}"), None).await;
    assert_eq!(r.headers[header::CONTENT_TYPE], "video/mp4");
    let r = app.call("PATCH", &format!("/v1/media/{media_id}"), Some(json!({ "title": "Renamed" }))).await;
    assert_eq!(r.json()["title"], "Renamed");
    assert_eq!(r.json()["size"], 11);
    let r = app.call("GET", &format!("/v1/media?group_id={g}&kind=video"), None).await;
    assert_eq!(r.json().as_array().unwrap().len(), 1);

    let r = app
        .call("POST", "/v1/playlists", Some(json!({ "group_id": g, "name": "P", "items": [{ "kind": "video", "media_id": media_id }] })))
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
    let g = app.group().await;
    let (v1, _) = app.media(&g, "video", "video/mp4", b"v1").await;
    let (v2, _) = app.media(&g, "video", "video/mp4", b"v2").await;
    let (img, _) = app.media(&g, "image", "image/png", b"i1").await;
    let (aud, _) = app.media(&g, "audio", "audio/mp4", b"a1").await;

    let r = app
        .call(
            "POST",
            "/v1/playlists",
            Some(json!({ "group_id": g, "name": "P", "items": [
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

    let r = app.call("GET", &format!("/v1/playlists?group_id={g}"), None).await;
    assert_eq!(r.json()[0]["item_count"], 3);
    assert!(r.json()[0]["total_bytes"].as_i64().unwrap() > 0);
}

async fn pair(app: &App, group: &str, name: &str, playlist: Option<&str>) -> (String, String) {
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

    let r = app.call("POST", "/v1/pairing/claim", Some(json!({ "code": "ZZZZZZ", "name": name, "group_id": group }))).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let mut claim = json!({ "code": code.to_lowercase(), "name": name, "group_id": group });
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
    let g = app.group().await;
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
            Some(json!({ "group_id": g, "name": "P", "items": [
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
    assert_eq!(r.headers[header::ETAG], "\"2\"");
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

    let r = app.send("GET", "/v1/device/manifest", Some(&token), &[("if-none-match", "\"2\"")], Body::empty(), false).await;
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
    assert_eq!(r.headers[header::ETAG], "\"none\"");
    assert!(r.json().get("playlist").is_none());

    let r = app.call("DELETE", &format!("/v1/screens/{screen_id}"), None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call_as(&token, "GET", "/v1/device/manifest", None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn device_selects_playlist() {
    let app = App::new().await;
    let g = app.group().await;
    let other = app.group().await;
    let (img, _) = app.media(&g, "image", "image/png", b"img").await;
    let mk = |group: &str, name: &str| json!({ "group_id": group, "name": name, "items": [{ "kind": "image", "media_id": img }] });
    let a = app.call("POST", "/v1/playlists", Some(mk(&g, "A"))).await.json()["id"].as_str().unwrap().to_string();
    let b = app.call("POST", "/v1/playlists", Some(json!({ "group_id": g, "name": "B", "items": [] }))).await.json()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let foreign = app.call("POST", "/v1/playlists", Some(json!({ "group_id": other, "name": "X", "items": [] }))).await.json()
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
