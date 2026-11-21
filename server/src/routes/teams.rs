use std::collections::HashSet;

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
pub struct Team {
    id: String,
    name: String,
    oidc_bindings: Vec<OidcBinding>,
    created_at: String,
}

#[derive(Deserialize)]
pub struct TeamInput {
    name: String,
    #[serde(default)]
    oidc_bindings: Vec<OidcBinding>,
}

type Row = (String, String, String, String);
const COLUMNS: &str = "id, name, oidc_bindings, created_at";

fn to_team((id, name, bindings, created_at): Row) -> Team {
    Team { id, name, oidc_bindings: serde_json::from_str(&bindings).unwrap_or_default(), created_at }
}

async fn fetch(s: &AppState, id: &str) -> ApiResult<Team> {
    let row: Option<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM teams WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?;
    row.map(to_team).ok_or_else(ApiError::not_found)
}

fn validate(input: &mut TeamInput) -> ApiResult<()> {
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    let mut seen = HashSet::new();
    for b in &mut input.oidc_bindings {
        b.claim_value = b.claim_value.trim().to_string();
        if b.claim_value.is_empty() {
            return Err(ApiError::bad_request("oidc_bindings claim_value must not be empty"));
        }
        if !seen.insert(b.claim_value.clone()) {
            return Err(ApiError::bad_request(format!("oidc_bindings lists {} more than once", b.claim_value)));
        }
    }
    Ok(())
}

pub async fn list(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<Team>>> {
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM teams ORDER BY name")).fetch_all(&s.db).await?;
    Ok(Json(rows.into_iter().filter(|r| p.can_view(&r.0)).map(to_team).collect()))
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(mut input): ApiJson<TeamInput>,
) -> ApiResult<(StatusCode, Json<Team>)> {
    if !p.server_admin {
        return Err(ApiError::forbidden());
    }
    validate(&mut input)?;
    let id = new_id();
    sqlx::query("INSERT INTO teams (id, name, oidc_bindings, created_at) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(input.name.trim())
        .bind(serde_json::to_string(&input.oidc_bindings)?)
        .bind(now())
        .execute(&s.db)
        .await?;
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Team>> {
    p.require(&id, Role::Viewer)?;
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn update(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(mut input): ApiJson<TeamInput>,
) -> ApiResult<Json<Team>> {
    p.require(&id, Role::Admin)?;
    validate(&mut input)?;
    fetch(&s, &id).await?;
    sqlx::query("UPDATE teams SET name = ?, oidc_bindings = ? WHERE id = ?")
        .bind(input.name.trim())
        .bind(serde_json::to_string(&input.oidc_bindings)?)
        .bind(&id)
        .execute(&s.db)
        .await?;
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    p.require(&id, Role::Viewer)?;
    if !p.server_admin {
        return Err(ApiError::forbidden());
    }
    let (n,): (i64,) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM media WHERE team_id = ?1) + (SELECT COUNT(*) FROM playlists WHERE team_id = ?1)
         + (SELECT COUNT(*) FROM screens WHERE team_id = ?1) + (SELECT COUNT(*) FROM nodes WHERE team_id = ?1)",
    )
    .bind(&id)
    .fetch_one(&s.db)
    .await?;
    if n > 0 {
        return Err(ApiError::conflict("team_not_empty", "team still owns resources"));
    }
    sqlx::query("DELETE FROM schedule_entries WHERE team_id = ?").bind(&id).execute(&s.db).await?;
    sqlx::query("DELETE FROM floor_plans WHERE team_id = ?").bind(&id).execute(&s.db).await?;
    let res = sqlx::query("DELETE FROM teams WHERE id = ?").bind(&id).execute(&s.db).await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}
