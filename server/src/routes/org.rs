//! The server's single organization: org listing for the apps, and the org settings.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{Principal, Unscoped},
    error::{ApiError, ApiJson, ApiResult},
    schedule,
    state::AppState,
};

/// Always exactly this server's org. A hosting gateway answers this itself for several orgs.
pub async fn mine(State(s): State<AppState>, Unscoped(p): Unscoped) -> ApiResult<Json<Value>> {
    if p.is_device() {
        return Err(ApiError::forbidden());
    }
    let team_count = if p.server_admin {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM teams").fetch_one(&s.db).await?;
        n as usize
    } else {
        p.memberships.len()
    };
    Ok(Json(json!({
        "organizations": [{
            "id": s.org_id(),
            "name": s.org_name(),
            "team_count": team_count,
            "role": if p.server_admin { "admin" } else { "member" },
        }],
        "can_create": false,
        "invites": [],
    })))
}

pub async fn create(Unscoped(_): Unscoped) -> ApiError {
    ApiError::not_supported("this server hosts a single organization")
}

pub async fn accept_invite(Unscoped(_): Unscoped, Path(_id): Path<String>) -> ApiError {
    ApiError::not_found()
}

fn settings_json(s: &AppState) -> Value {
    let mut locked = vec![];
    if s.cfg.org_name.is_some() {
        locked.push("org_name");
    }
    if s.cfg.org_timezone.is_some() {
        locked.push("timezone");
    }
    json!({
        "organization_id": s.org_id(),
        "org_name": s.org_name(),
        "timezone": s.org_tz().name(),
        "locked": locked,
        "configured": s.org_configured(),
    })
}

pub async fn get_settings(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    if p.is_device() {
        return Err(ApiError::forbidden());
    }
    Ok(Json(settings_json(&s)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    org_name: Option<String>,
    timezone: Option<String>,
}

pub async fn patch_settings(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<SettingsPatch>,
) -> ApiResult<Json<Value>> {
    if !p.server_admin {
        return Err(ApiError::forbidden());
    }
    let name = input.org_name.map(|n| n.trim().to_string());
    let timezone = input.timezone.map(|t| t.trim().to_string());
    if name.as_deref() == Some("") {
        return Err(ApiError::bad_request("org_name must not be empty"));
    }
    if name.is_some() && s.cfg.org_name.is_some() {
        return Err(ApiError::conflict("setting_locked", "org_name is set by an environment variable"));
    }
    if let Some(t) = &timezone {
        if s.cfg.org_timezone.is_some() {
            return Err(ApiError::conflict("setting_locked", "timezone is set by an environment variable"));
        }
        if schedule::parse_tz(t).is_none() {
            return Err(ApiError::unprocessable("invalid_timezone", "timezone must be an IANA zone name"));
        }
    }

    let before = s.org_tz();
    let mut tx = s.db.begin().await?;
    for (key, value) in [("org_name", &name), ("org_timezone", &timezone)] {
        if let Some(v) = value {
            sqlx::query("INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2")
                .bind(key)
                .bind(v)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    {
        let mut org = s.org.lock();
        if name.is_some() {
            org.name = name;
        }
        if timezone.is_some() {
            org.timezone = timezone;
        }
    }
    if s.org_tz() != before {
        schedule::notify_all(&s).await;
    }
    Ok(Json(settings_json(&s)))
}
