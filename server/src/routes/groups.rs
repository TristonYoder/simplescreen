use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    models::OidcBinding,
    schedule,
    state::AppState,
    util::{new_id, now},
};

#[derive(Serialize)]
pub struct Group {
    id: String,
    name: String,
    oidc_bindings: Vec<OidcBinding>,
    timezone: String,
    created_at: String,
}

#[derive(Deserialize)]
pub struct GroupInput {
    name: String,
    #[serde(default)]
    oidc_bindings: Vec<OidcBinding>,
    #[serde(default)]
    timezone: Option<String>,
}

type Row = (String, String, String, Option<String>, String);
const COLUMNS: &str = "id, name, oidc_bindings, timezone, created_at";

fn to_group(s: &AppState, (id, name, bindings, timezone, created_at): Row) -> Group {
    let timezone = timezone.filter(|t| schedule::parse_tz(t).is_some()).unwrap_or_else(|| schedule::server_tz(s).name().to_string());
    Group { id, name, oidc_bindings: serde_json::from_str(&bindings).unwrap_or_default(), timezone, created_at }
}

async fn fetch(s: &AppState, id: &str) -> ApiResult<Group> {
    let row: Option<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM groups WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?;
    row.map(|r| to_group(s, r)).ok_or_else(ApiError::not_found)
}

fn validate(input: &mut GroupInput) -> ApiResult<()> {
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    input.timezone = input.timezone.take().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if let Some(t) = &input.timezone {
        if schedule::parse_tz(t).is_none() {
            return Err(ApiError::unprocessable("invalid_timezone", "timezone must be an IANA zone name"));
        }
    }
    Ok(())
}

pub async fn list(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<Group>>> {
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM groups ORDER BY name")).fetch_all(&s.db).await?;
    Ok(Json(rows.into_iter().filter(|r| p.can_view(&r.0)).map(|r| to_group(&s, r)).collect()))
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(mut input): ApiJson<GroupInput>,
) -> ApiResult<(StatusCode, Json<Group>)> {
    if !p.server_admin {
        return Err(ApiError::forbidden());
    }
    validate(&mut input)?;
    let id = new_id();
    sqlx::query("INSERT INTO groups (id, name, oidc_bindings, timezone, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(input.name.trim())
        .bind(serde_json::to_string(&input.oidc_bindings)?)
        .bind(&input.timezone)
        .bind(now())
        .execute(&s.db)
        .await?;
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Group>> {
    p.require(&id, Role::Viewer)?;
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn update(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(mut input): ApiJson<GroupInput>,
) -> ApiResult<Json<Group>> {
    p.require(&id, Role::Admin)?;
    validate(&mut input)?;
    let before = fetch(&s, &id).await?;
    sqlx::query("UPDATE groups SET name = ?, oidc_bindings = ?, timezone = ? WHERE id = ?")
        .bind(input.name.trim())
        .bind(serde_json::to_string(&input.oidc_bindings)?)
        .bind(&input.timezone)
        .bind(&id)
        .execute(&s.db)
        .await?;
    let after = fetch(&s, &id).await?;
    if after.timezone != before.timezone {
        schedule::notify_group(&s, &id).await;
        schedule::broadcast_changed(&s, &id);
    }
    Ok(Json(after))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    p.require(&id, Role::Viewer)?;
    if !p.server_admin {
        return Err(ApiError::forbidden());
    }
    let (n,): (i64,) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM media WHERE group_id = ?1) + (SELECT COUNT(*) FROM playlists WHERE group_id = ?1)
         + (SELECT COUNT(*) FROM screens WHERE group_id = ?1)",
    )
    .bind(&id)
    .fetch_one(&s.db)
    .await?;
    if n > 0 {
        return Err(ApiError::conflict("group_not_empty", "group still owns resources"));
    }
    sqlx::query("DELETE FROM schedule_entries WHERE group_id = ?").bind(&id).execute(&s.db).await?;
    let res = sqlx::query("DELETE FROM groups WHERE id = ?").bind(&id).execute(&s.db).await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}
