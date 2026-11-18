use axum::{
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
    error::{ApiError, ApiJson, ApiResult},
    models::Screen,
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

pub async fn start(State(s): State<AppState>) -> ApiResult<(StatusCode, Json<Session>)> {
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
    sqlx::query("INSERT INTO pairings (id, code, secret_hash, expires_at, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(&code)
        .bind(sha256_hex(secret.as_bytes()))
        .bind(&expires_at)
        .bind(now())
        .execute(&s.db)
        .await?;
    Ok((StatusCode::CREATED, Json(Session { pairing_id: id, code, secret, expires_at })))
}

#[derive(Deserialize)]
pub struct Claim {
    code: String,
    name: String,
    group_id: String,
    playlist_id: Option<String>,
}

pub async fn claim(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<Claim>,
) -> ApiResult<(StatusCode, Json<Screen>)> {
    s.require_group(&p, &input.group_id, Role::Editor).await?;
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    let code = input.code.trim().to_uppercase();
    let pairing: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM pairings WHERE code = ? AND expires_at > ? AND screen_id IS NULL ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&code)
    .bind(now())
    .fetch_optional(&s.db)
    .await?;
    let (pairing_id,) = pairing.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "pairing_not_found", "unknown or expired code"))?;
    if let Some(pid) = &input.playlist_id {
        let found: Option<(String,)> = sqlx::query_as("SELECT id FROM playlists WHERE id = ? AND group_id = ?")
            .bind(pid)
            .bind(&input.group_id)
            .fetch_optional(&s.db)
            .await?;
        if found.is_none() {
            return Err(ApiError::unprocessable("invalid_playlist", "playlist does not exist in the group"));
        }
    }

    let screen_id = new_id();
    let token = random_token(32);
    let created = now();
    let mut tx = s.db.begin().await?;
    sqlx::query(
        "INSERT INTO screens (id, group_id, name, playlist_id, token_hash, created_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&screen_id)
    .bind(&input.group_id)
    .bind(input.name.trim())
    .bind(&input.playlist_id)
    .bind(sha256_hex(token.as_bytes()))
    .bind(&created)
    .execute(&mut *tx)
    .await?;
    let res = sqlx::query("UPDATE pairings SET screen_id = ?, device_token = ? WHERE id = ? AND screen_id IS NULL")
        .bind(&screen_id)
        .bind(&token)
        .bind(&pairing_id)
        .execute(&mut *tx)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "pairing_not_found", "unknown or expired code"));
    }
    tx.commit().await?;
    let row = super::screens::fetch_row(&s, &screen_id).await?;
    Ok((StatusCode::CREATED, Json(super::screens::to_screen(&s, row))))
}

#[derive(Deserialize)]
pub struct PollQuery {
    secret: String,
}

#[derive(sqlx::FromRow)]
struct PairingRow {
    secret_hash: String,
    expires_at: String,
    screen_id: Option<String>,
    device_token: Option<String>,
}

pub async fn poll(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PollQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let row: PairingRow =
        sqlx::query_as("SELECT secret_hash, expires_at, screen_id, device_token FROM pairings WHERE id = ?")
            .bind(&id)
            .fetch_optional(&s.db)
            .await?
            .ok_or_else(ApiError::not_found)?;
    let given = sha256_hex(q.secret.as_bytes());
    if !bool::from(given.as_bytes().ct_eq(row.secret_hash.as_bytes())) {
        return Err(ApiError::not_found());
    }
    if let Some(screen_id) = row.screen_id {
        let mut v = serde_json::json!({ "status": "claimed", "screen_id": screen_id });
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
