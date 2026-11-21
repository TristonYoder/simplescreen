use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{Duration, SecondsFormat, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::{
    auth::{Principal, Role},
    notices::{self, Actor},
    reach::{self, World},
    error::{ApiError, ApiJson, ApiResult},
    models::{Output, OutputKind, OutputRole},
    state::AppState,
    util::{new_id, now, random_token, sha256_hex},
};

const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

#[derive(Serialize)]
pub struct Session {
    pairing_id: String,
    code: String,
    secret: String,
    expires_at: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum PairingKind {
    #[default]
    Single,
    Node,
}

#[derive(Deserialize, Default)]
struct StartInput {
    #[serde(default)]
    kind: PairingKind,
    mode: Option<crate::assignments::Mode>,
}

/// The body is optional: an absent body (or `kind: "single"`) pairs a single-screen device such as an Apple TV.
pub async fn start(State(s): State<AppState>, body: Bytes) -> ApiResult<(StatusCode, Json<Session>)> {
    let input: StartInput = if body.iter().all(|b| b.is_ascii_whitespace()) {
        StartInput::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(e.to_string()))?
    };
    let mode = input.mode.unwrap_or_default();
    if matches!(input.kind, PairingKind::Single) && mode != crate::assignments::Mode::Player {
        return Err(ApiError::bad_request("mode only applies to kind node; single-screen devices are players"));
    }
    let kind = match input.kind {
        PairingKind::Single => "single",
        PairingKind::Node => "node",
    };
    let cutoff = (Utc::now() - Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Millis, true);
    sqlx::query("DELETE FROM pairings WHERE expires_at < ?").bind(cutoff).execute(&s.db).await?;

    let code = loop {
        let code: String = {
            let mut rng = rand::thread_rng();
            (0..6).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect()
        };
        let (taken,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pairings WHERE code = ? AND expires_at > ?")
            .bind(&code)
            .bind(now())
            .fetch_one(&s.db)
            .await?;
        if taken == 0 {
            break code;
        }
    };
    let id = new_id();
    let secret = random_token(32);
    let expires_at = (Utc::now() + Duration::minutes(10)).to_rfc3339_opts(SecondsFormat::Millis, true);
    sqlx::query("INSERT INTO pairings (id, code, secret_hash, expires_at, kind, mode, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(&code)
        .bind(sha256_hex(secret.as_bytes()))
        .bind(&expires_at)
        .bind(kind)
        .bind(mode.as_str())
        .bind(now())
        .execute(&s.db)
        .await?;
    Ok((StatusCode::CREATED, Json(Session { pairing_id: id, code, secret, expires_at })))
}

#[derive(Deserialize)]
pub struct Claim {
    code: String,
    name: String,
    #[serde(default)]
    tag_ids: Option<Vec<String>>,
    playlist_id: Option<String>,
}

pub async fn claim(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<Claim>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    if !p.is_editor_anywhere() || p.is_device() {
        return Err(ApiError::forbidden());
    }
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    let code = input.code.trim().to_uppercase();
    let pairing: Option<(String, String, String)> = sqlx::query_as(
        "SELECT id, kind, mode FROM pairings WHERE code = ? AND expires_at > ? AND node_id IS NULL ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&code)
    .bind(now())
    .fetch_optional(&s.db)
    .await?;
    let (pairing_id, kind, mode) = pairing.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "pairing_not_found", "unknown or expired code"))?;
    let single = kind == "single";
    if !single && (input.playlist_id.is_some() || input.tag_ids.is_some()) {
        return Err(ApiError::bad_request("tag_ids and playlist_id only apply to single-screen pairings"));
    }
    let tags = reach::check_tags(&s, input.tag_ids.as_deref().unwrap_or_default()).await?;
    let w = World::load(&s).await?;
    if single && !p.server_admin {
        // An editor's new screen must land in their reach, with only tags their teams hold.
        let held = |t: &String| p.memberships.iter().any(|m| m.role >= Role::Editor && w.team_holds(&m.team_id, t));
        if let Some(t) = tags.iter().find(|t| !held(t)) {
            return Err(ApiError::unprocessable("invalid_tag", format!("tag {t} is not held by a team you edit")));
        }
        let reached = p.memberships.iter().any(|m| {
            m.role >= Role::Editor && w.teams.get(&m.team_id).is_some_and(|team| tags.iter().any(|t| team.tags.contains(t)) || (team.all && !tags.is_empty()))
        });
        if !reached {
            return Err(ApiError::unprocessable("tag_required", "give the screen at least one tag a team you edit holds"));
        }
    }
    if let Some(pid) = &input.playlist_id {
        let found: Option<(String,)> = sqlx::query_as("SELECT team_id FROM playlists WHERE id = ?").bind(pid).fetch_optional(&s.db).await?;
        let shares = w.playlist_tags.get(pid).is_some_and(|pt| tags.iter().any(|t| pt.contains(t)));
        if !found.is_some_and(|(team,)| p.edits_team(&team)) || !shares {
            return Err(ApiError::unprocessable("playlist_unavailable", "playlist is not one of your team's playlists available on this screen"));
        }
    }

    let node_id = new_id();
    let screen_id = single.then(new_id);
    let token = random_token(32);
    let created = now();
    let mut tx = s.db.begin().await?;
    let outputs = match &screen_id {
        Some(sid) => serde_json::to_string(&[Output {
            key: "main".into(),
            kind: OutputKind::Display,
            name: "Main".into(),
            role: OutputRole::Program,
            screen_id: Some(sid.clone()),
            format: None,
            audio_device: None,
        }])?,
        None => "[]".into(),
    };
    sqlx::query("INSERT INTO nodes (id, name, token_hash, outputs, single, mode, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&node_id)
        .bind(input.name.trim())
        .bind(sha256_hex(token.as_bytes()))
        .bind(outputs)
        .bind(single)
        .bind(&mode)
        .bind(&created)
        .execute(&mut *tx)
        .await?;
    if let Some(sid) = &screen_id {
        sqlx::query("INSERT INTO screens (id, name, fallback_playlist_id, node_id, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(sid)
            .bind(input.name.trim())
            .bind(&input.playlist_id)
            .bind(&node_id)
            .bind(&created)
            .execute(&mut *tx)
            .await?;
        for t in &tags {
            sqlx::query("INSERT INTO screen_tags (screen_id, tag_id) VALUES (?, ?)").bind(sid).bind(t).execute(&mut *tx).await?;
        }
        sqlx::query(
            "INSERT INTO assignments (node_id, output_key, job, screen_id, updated_by, updated_at) VALUES (?, 'main', 'program', ?, 'pairing', ?)",
        )
        .bind(&node_id)
        .bind(sid)
        .bind(&created)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE nodes SET assignments_version = 1, applied_assignments_version = 1 WHERE id = ?").bind(&node_id).execute(&mut *tx).await?;
    }
    let res = sqlx::query("UPDATE pairings SET node_id = ?, screen_id = ?, device_token = ? WHERE id = ? AND node_id IS NULL")
        .bind(&node_id)
        .bind(&screen_id)
        .bind(&token)
        .bind(&pairing_id)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "pairing_not_found", "unknown or expired code"));
    }
    tx.commit().await?;
    s.hub.broadcast_all(&serde_json::json!({ "type": "node_updated", "node_id": node_id }));
    let body = match screen_id {
        Some(sid) => {
            let w = World::load(&s).await?;
            notices::log(&s, &sid, "created", &Actor::of(&p, &w, &sid), serde_json::json!({ "name": input.name.trim(), "node_id": node_id })).await;
            notices::sync_untagged(&s, &sid).await?;
            let row = super::screens::fetch_row(&s, &sid).await?;
            serde_json::to_value(super::screens::to_screen(&s, row).await?)?
        }
        None => serde_json::to_value(super::nodes::to_node(&s, super::nodes::fetch_row(&s, &node_id).await?).await?)?,
    };
    Ok((StatusCode::CREATED, Json(body)))
}

#[derive(Deserialize)]
pub struct PollQuery {
    secret: String,
}

#[derive(sqlx::FromRow)]
struct PairingRow {
    secret_hash: String,
    expires_at: String,
    node_id: Option<String>,
    screen_id: Option<String>,
    device_token: Option<String>,
}

pub async fn poll(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PollQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let row: PairingRow =
        sqlx::query_as("SELECT secret_hash, expires_at, node_id, screen_id, device_token FROM pairings WHERE id = ?")
            .bind(&id)
            .fetch_optional(&s.db)
            .await?
            .ok_or_else(ApiError::not_found)?;
    let given = sha256_hex(q.secret.as_bytes());
    if !bool::from(given.as_bytes().ct_eq(row.secret_hash.as_bytes())) {
        return Err(ApiError::not_found());
    }
    if let Some(node_id) = row.node_id {
        let mut v = serde_json::json!({ "status": "claimed", "node_id": node_id });
        if let Some(screen_id) = row.screen_id {
            v["screen_id"] = serde_json::json!(screen_id);
        }
        let taken = sqlx::query("UPDATE pairings SET device_token = NULL, token_delivered = 1 WHERE id = ? AND device_token IS NOT NULL")
            .bind(&id)
            .execute(&s.db)
            .await?
            .rows_affected();
        if taken > 0 {
            v["device_token"] = serde_json::json!(row.device_token);
        }
        return Ok(Json(v));
    }
    if row.expires_at.as_str() < now().as_str() {
        return Ok(Json(serde_json::json!({ "status": "expired" })));
    }
    Ok(Json(serde_json::json!({ "status": "pending" })))
}
