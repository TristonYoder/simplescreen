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
    state::AppState,
    util::{new_id, now},
};

#[derive(Serialize)]
pub struct Group {
    id: String,
    name: String,
    oidc_bindings: Vec<OidcBinding>,
    created_at: String,
}

#[derive(Deserialize)]
pub struct GroupInput {
    name: String,
    #[serde(default)]
    oidc_bindings: Vec<OidcBinding>,
}

type Row = (String, String, String, String);

fn to_group((id, name, bindings, created_at): Row) -> Group {
    Group { id, name, oidc_bindings: serde_json::from_str(&bindings).unwrap_or_default(), created_at }
}

async fn fetch(s: &AppState, id: &str) -> ApiResult<Group> {
    let row: Option<Row> = sqlx::query_as("SELECT id, name, oidc_bindings, created_at FROM groups WHERE id = ?")
        .bind(id)
        .fetch_optional(&s.db)
        .await?;
    row.map(to_group).ok_or_else(ApiError::not_found)
}

fn validate(input: &GroupInput) -> ApiResult<()> {
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    Ok(())
}

pub async fn list(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<Group>>> {
    let rows: Vec<Row> = sqlx::query_as("SELECT id, name, oidc_bindings, created_at FROM groups ORDER BY name")
        .fetch_all(&s.db)
        .await?;
    Ok(Json(rows.into_iter().filter(|r| p.can_view(&r.0)).map(to_group).collect()))
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<GroupInput>,
) -> ApiResult<(StatusCode, Json<Group>)> {
    if !p.server_admin {
        return Err(ApiError::forbidden());
    }
    validate(&input)?;
    let id = new_id();
    sqlx::query("INSERT INTO groups (id, name, oidc_bindings, created_at) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(input.name.trim())
        .bind(serde_json::to_string(&input.oidc_bindings)?)
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
    ApiJson(input): ApiJson<GroupInput>,
) -> ApiResult<Json<Group>> {
    p.require(&id, Role::Admin)?;
    validate(&input)?;
    let res = sqlx::query("UPDATE groups SET name = ?, oidc_bindings = ? WHERE id = ?")
        .bind(input.name.trim())
        .bind(serde_json::to_string(&input.oidc_bindings)?)
        .bind(&id)
        .execute(&s.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    Ok(Json(fetch(&s, &id).await?))
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
    let res = sqlx::query("DELETE FROM groups WHERE id = ?").bind(&id).execute(&s.db).await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}
