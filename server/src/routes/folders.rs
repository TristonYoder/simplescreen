use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    state::AppState,
    util::{new_id, now},
};

#[derive(Serialize, FromRow)]
pub struct Folder {
    id: String,
    team_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    name: String,
    position: i64,
    created_at: String,
}

const COLUMNS: &str = "id, team_id, parent_id, name, position, created_at";

async fn fetch(s: &AppState, id: &str) -> ApiResult<Folder> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM playlist_folders WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(ApiError::not_found)
}

fn invalid(msg: &str) -> ApiError {
    ApiError::unprocessable("invalid_folder", msg)
}

fn clean_name(name: &str) -> ApiResult<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    Ok(n.to_string())
}

/// Folders shape the device picker, so devices refetch their manifests.
async fn changed(s: &AppState) {
    crate::schedule::notify_screens(s).await;
}

#[derive(Deserialize)]
pub struct ListQuery {
    team_id: Option<String>,
}

pub async fn list(State(s): State<AppState>, p: Principal, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Folder>>> {
    if let Some(t) = &q.team_id {
        s.require_team(&p, t, Role::Viewer).await?;
    }
    let rows: Vec<Folder> = sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM playlist_folders WHERE (?1 IS NULL OR team_id = ?1) ORDER BY position, name"
    ))
    .bind(&q.team_id)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows.into_iter().filter(|f| p.can_view(&f.team_id)).collect()))
}

#[derive(Deserialize)]
pub struct FolderInput {
    team_id: String,
    parent_id: Option<String>,
    name: String,
    position: Option<i64>,
}

async fn check_parent(s: &AppState, team_id: &str, parent: Option<&str>, moving: Option<&str>) -> ApiResult<()> {
    let mut cur = parent.map(String::from);
    while let Some(id) = cur {
        if Some(id.as_str()) == moving {
            return Err(invalid("a folder can't move into itself or a descendant"));
        }
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT team_id, parent_id FROM playlist_folders WHERE id = ?").bind(&id).fetch_optional(&s.db).await?;
        match row {
            Some((t, next)) if t == team_id => cur = next,
            _ => return Err(invalid("parent folder does not exist in the team")),
        }
    }
    Ok(())
}

pub async fn create(State(s): State<AppState>, p: Principal, ApiJson(input): ApiJson<FolderInput>) -> ApiResult<(StatusCode, Json<Folder>)> {
    s.require_team(&p, &input.team_id, Role::Editor).await?;
    let name = clean_name(&input.name)?;
    check_parent(&s, &input.team_id, input.parent_id.as_deref(), None).await?;
    let position = match input.position {
        Some(n) => n,
        None => {
            let (max,): (Option<i64>,) = sqlx::query_as("SELECT MAX(position) FROM playlist_folders WHERE team_id = ? AND parent_id IS ?")
                .bind(&input.team_id)
                .bind(&input.parent_id)
                .fetch_one(&s.db)
                .await?;
            max.map_or(0, |m| m + 1)
        }
    };
    let id = new_id();
    sqlx::query("INSERT INTO playlist_folders (id, team_id, parent_id, name, position, created_at) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(&input.team_id)
        .bind(&input.parent_id)
        .bind(&name)
        .bind(position)
        .bind(now())
        .execute(&s.db)
        .await?;
    changed(&s).await;
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

#[derive(Deserialize)]
pub struct FolderPatch {
    #[serde(default, deserialize_with = "crate::models::double_option")]
    parent_id: Option<Option<String>>,
    name: Option<String>,
    position: Option<i64>,
}

pub async fn patch(State(s): State<AppState>, p: Principal, Path(id): Path<String>, ApiJson(input): ApiJson<FolderPatch>) -> ApiResult<Json<Folder>> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    let name = match &input.name {
        Some(n) => clean_name(n)?,
        None => current.name,
    };
    let parent = input.parent_id.unwrap_or(current.parent_id);
    check_parent(&s, &current.team_id, parent.as_deref(), Some(&id)).await?;
    sqlx::query("UPDATE playlist_folders SET parent_id = ?, name = ?, position = ? WHERE id = ?")
        .bind(&parent)
        .bind(&name)
        .bind(input.position.unwrap_or(current.position))
        .bind(&id)
        .execute(&s.db)
        .await?;
    changed(&s).await;
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    let mut tx = s.db.begin().await?;
    sqlx::query("UPDATE playlist_folders SET parent_id = ? WHERE parent_id = ?").bind(&current.parent_id).bind(&id).execute(&mut *tx).await?;
    sqlx::query("UPDATE playlists SET folder_id = ? WHERE folder_id = ?").bind(&current.parent_id).bind(&id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM playlist_folders WHERE id = ?").bind(&id).execute(&mut *tx).await?;
    tx.commit().await?;
    changed(&s).await;
    Ok(StatusCode::NO_CONTENT)
}
