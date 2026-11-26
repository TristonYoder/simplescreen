use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    models::OidcBinding,
    reach::{self, World},
    schedule,
    state::AppState,
    util::{new_id, now},
};

#[derive(Serialize)]
pub struct Team {
    id: String,
    name: String,
    oidc_bindings: Vec<OidcBinding>,
    screen_access: String,
    tag_ids: Vec<String>,
    created_at: String,
}

#[derive(Deserialize)]
pub struct TeamInput {
    name: String,
    #[serde(default)]
    oidc_bindings: Vec<OidcBinding>,
}

type Row = (String, String, String, String, String, Option<String>);
const COLUMNS: &str = "id, name, oidc_bindings, screen_access, created_at, \
    (SELECT json_group_array(tag_id) FROM (SELECT tag_id FROM team_tags WHERE team_id = teams.id ORDER BY tag_id))";

fn to_team((id, name, bindings, screen_access, created_at, tags): Row) -> Team {
    Team {
        id,
        name,
        oidc_bindings: serde_json::from_str(&bindings).unwrap_or_default(),
        screen_access,
        tag_ids: tags.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default(),
        created_at,
    }
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
        return Err(ApiError::bad_request("name must not be empty").because("name_empty"));
    }
    let mut seen = HashSet::new();
    for b in &mut input.oidc_bindings {
        b.claim_value = b.claim_value.trim().to_string();
        if b.claim_value.is_empty() {
            return Err(ApiError::bad_request("oidc_bindings claim_value must not be empty").because("claim_value_empty"));
        }
        if !seen.insert(b.claim_value.clone()) {
            return Err(ApiError::bad_request(format!("oidc_bindings lists {} more than once", b.claim_value)).because("claim_value_repeated").with("claim_value", &b.claim_value));
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
        "SELECT (SELECT COUNT(*) FROM playlists WHERE team_id = ?1)
         + (SELECT COUNT(*) FROM playlist_folders WHERE team_id = ?1)",
    )
    .bind(&id)
    .fetch_one(&s.db)
    .await?;
    if n > 0 {
        return Err(ApiError::conflict("team_not_empty", "team still owns resources"));
    }
    sqlx::query("DELETE FROM schedule_entries WHERE team_id = ?").bind(&id).execute(&s.db).await?;
    let res = sqlx::query("DELETE FROM teams WHERE id = ?").bind(&id).execute(&s.db).await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    schedule::refresh_clashes(&s, Some(&p)).await?;
    schedule::notify_screens(&s).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TagsInput {
    tag_ids: Vec<String>,
    screen_access: Option<String>,
}

pub async fn put_tags(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<TagsInput>,
) -> ApiResult<Json<Value>> {
    p.require_admin()?;
    let current = fetch(&s, &id).await?;
    let access = input.screen_access.unwrap_or(current.screen_access);
    if access != "all" && access != "tags" {
        return Err(ApiError::bad_request("screen_access must be all or tags").because("screen_access_invalid"));
    }
    let tags = reach::check_tags(&s, &input.tag_ids).await?;
    let before = World::load(&s).await?.team_screens(&id);
    let mut tx = s.db.begin().await?;
    sqlx::query("UPDATE teams SET screen_access = ? WHERE id = ?").bind(&access).bind(&id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM team_tags WHERE team_id = ?").bind(&id).execute(&mut *tx).await?;
    for t in &tags {
        sqlx::query("INSERT INTO team_tags (team_id, tag_id) VALUES (?, ?)").bind(&id).bind(t).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    let clashes = schedule::refresh_clashes(&s, Some(&p)).await?;
    schedule::notify_screens(&s).await;
    let after = World::load(&s).await?.team_screens(&id);
    let mut touched = before;
    touched.extend(after);
    touched.sort();
    touched.dedup();
    for screen in &touched {
        super::screens::broadcast_updated(&s, screen).await;
    }
    s.hub.broadcast_all(&json!({ "type": "tags_changed" }));
    schedule::broadcast_changed(&s, &id);
    Ok(Json(json!({ "team": fetch(&s, &id).await?, "clashes": clashes, "warnings": [] })))
}

pub async fn reach_matrix(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    p.require_admin()?;
    let w = World::load(&s).await?;
    let rows: Vec<Row> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM teams ORDER BY name")).fetch_all(&s.db).await?;
    let teams: Vec<Value> = rows
        .into_iter()
        .map(to_team)
        .map(|t| {
            json!({
                "team_id": t.id,
                "team_name": t.name,
                "screen_access": t.screen_access,
                "tag_ids": t.tag_ids,
                "screen_ids": w.team_screens(&t.id),
            })
        })
        .collect();
    Ok(Json(json!({ "teams": teams })))
}
