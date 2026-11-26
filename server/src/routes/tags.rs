use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::FromRow;

use crate::{
    auth::Principal,
    error::{ApiError, ApiJson, ApiResult},
    notices,
    reach::World,
    schedule,
    state::AppState,
    util::{new_id, now},
};

#[derive(Serialize, FromRow)]
pub struct Tag {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    created_at: String,
}

const COLUMNS: &str = "id, name, color, created_at";

async fn fetch(s: &AppState, id: &str) -> ApiResult<Tag> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM tags WHERE id = ?")).bind(id).fetch_optional(&s.db).await?.ok_or_else(ApiError::not_found)
}

fn exists(e: sqlx::Error) -> ApiError {
    match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => ApiError::conflict("tag_exists", "a tag with that name exists"),
        _ => e.into(),
    }
}

fn clean_name(name: &str) -> ApiResult<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(ApiError::bad_request("name must not be empty").because("name_empty"));
    }
    Ok(n.to_string())
}

fn changed(s: &AppState) {
    s.hub.broadcast_all(&json!({ "type": "tags_changed" }));
}

pub async fn list(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<Tag>>> {
    super::assignments::require_user(&p)?;
    Ok(Json(sqlx::query_as(&format!("SELECT {COLUMNS} FROM tags ORDER BY name COLLATE NOCASE")).fetch_all(&s.db).await?))
}

#[derive(Deserialize)]
pub struct TagInput {
    name: String,
    color: Option<String>,
}

pub async fn create(State(s): State<AppState>, p: Principal, ApiJson(input): ApiJson<TagInput>) -> ApiResult<(StatusCode, Json<Tag>)> {
    p.require_admin()?;
    let name = clean_name(&input.name)?;
    let id = new_id();
    sqlx::query("INSERT INTO tags (id, name, color, created_at) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(&name)
        .bind(&input.color)
        .bind(now())
        .execute(&s.db)
        .await
        .map_err(exists)?;
    changed(&s);
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

#[derive(Deserialize)]
pub struct TagPatch {
    name: Option<String>,
    #[serde(default, deserialize_with = "crate::models::double_option")]
    color: Option<Option<String>>,
}

pub async fn patch(State(s): State<AppState>, p: Principal, Path(id): Path<String>, ApiJson(input): ApiJson<TagPatch>) -> ApiResult<Json<Tag>> {
    p.require_admin()?;
    let current = fetch(&s, &id).await?;
    let name = match &input.name {
        Some(n) => clean_name(n)?,
        None => current.name,
    };
    let color = input.color.unwrap_or(current.color);
    sqlx::query("UPDATE tags SET name = ?, color = ? WHERE id = ?").bind(&name).bind(&color).bind(&id).execute(&s.db).await.map_err(exists)?;
    changed(&s);
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    p.require_admin()?;
    fetch(&s, &id).await?;
    let screens: Vec<(String,)> = sqlx::query_as("SELECT screen_id FROM screen_tags WHERE tag_id = ?").bind(&id).fetch_all(&s.db).await?;
    let mut tx = s.db.begin().await?;
    sqlx::query(
        "UPDATE schedule_entries SET tag_ids = (SELECT json_group_array(value) FROM json_each(schedule_entries.tag_ids) WHERE value != ?1) \
         WHERE EXISTS (SELECT 1 FROM json_each(schedule_entries.tag_ids) WHERE value = ?1)",
    )
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM tags WHERE id = ?").bind(&id).execute(&mut *tx).await?;
    tx.commit().await?;
    for (screen,) in &screens {
        notices::sync_untagged(&s, screen).await?;
    }
    let clashes = schedule::refresh_clashes(&s, Some(&p)).await?;
    let w = World::load(&s).await?;
    let mut warnings = vec![];
    for (screen,) in &screens {
        let row = super::screens::fetch_row(&s, screen).await?;
        warnings.extend(super::screens::availability_warnings(&w, &row));
        super::screens::broadcast_updated(&s, screen).await;
    }
    schedule::notify_screens(&s).await;
    s.hub.broadcast_all(&json!({ "type": "schedule_changed" }));
    changed(&s);
    Ok(Json(json!({ "clashes": clashes, "warnings": warnings })))
}

pub async fn usage(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    p.require_admin()?;
    fetch(&s, &id).await?;
    let ids = |rows: Vec<(String,)>| rows.into_iter().map(|(x,)| x).collect::<Vec<_>>();
    let screens = ids(sqlx::query_as("SELECT screen_id FROM screen_tags WHERE tag_id = ? ORDER BY screen_id").bind(&id).fetch_all(&s.db).await?);
    let teams = ids(sqlx::query_as("SELECT team_id FROM team_tags WHERE tag_id = ? ORDER BY team_id").bind(&id).fetch_all(&s.db).await?);
    let playlists =
        ids(sqlx::query_as("SELECT playlist_id FROM playlist_tags WHERE tag_id = ? ORDER BY playlist_id").bind(&id).fetch_all(&s.db).await?);
    let entries = ids(sqlx::query_as(
        "SELECT id FROM schedule_entries WHERE EXISTS (SELECT 1 FROM json_each(schedule_entries.tag_ids) WHERE value = ?) ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&s.db)
    .await?);
    Ok(Json(json!({ "screen_ids": screens, "team_ids": teams, "playlist_ids": playlists, "entry_ids": entries })))
}
