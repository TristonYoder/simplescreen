use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    models::{Screen, ScreenCommand, ScreenPatch, ScreenRow, ScreenState, ScreenStatus, SCREEN_COLUMNS},
    state::AppState,
};

pub fn to_screen(s: &AppState, row: ScreenRow) -> Screen {
    let online = s.hub.is_online(&row.id);
    let status = if online {
        s.statuses.lock().get(&row.id).cloned().unwrap_or_else(|| ScreenStatus::simple(ScreenState::Idle))
    } else {
        ScreenStatus::simple(ScreenState::Offline)
    };
    Screen {
        id: row.id,
        group_id: row.group_id,
        name: row.name,
        playlist_id: row.playlist_id,
        online,
        status,
        last_seen_at: row.last_seen_at,
        created_at: row.created_at,
    }
}

pub async fn fetch_row(s: &AppState, id: &str) -> ApiResult<ScreenRow> {
    sqlx::query_as(&format!("SELECT {SCREEN_COLUMNS} FROM screens WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(ApiError::not_found)
}

#[derive(Deserialize)]
pub struct ListQuery {
    group_id: Option<String>,
}

pub async fn list(
    State(s): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Screen>>> {
    let rows: Vec<ScreenRow> = sqlx::query_as(&format!(
        "SELECT {SCREEN_COLUMNS} FROM screens WHERE (?1 IS NULL OR group_id = ?1) ORDER BY name"
    ))
    .bind(&q.group_id)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows.into_iter().filter(|r| p.can_view(&r.group_id)).map(|r| to_screen(&s, r)).collect()))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Screen>> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.group_id, Role::Viewer)?;
    Ok(Json(to_screen(&s, row)))
}

pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<ScreenPatch>,
) -> ApiResult<Json<Screen>> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.group_id, Role::Editor)?;
    if let Some(name) = &patch.name {
        if name.trim().is_empty() {
            return Err(ApiError::bad_request("name must not be empty"));
        }
        sqlx::query("UPDATE screens SET name = ? WHERE id = ?").bind(name.trim()).bind(&id).execute(&s.db).await?;
    }
    if let Some(playlist) = &patch.playlist_id {
        let version = match playlist {
            None => 0,
            Some(pid) => {
                let found: Option<(i64,)> =
                    sqlx::query_as("SELECT version FROM playlists WHERE id = ? AND group_id = ?")
                        .bind(pid)
                        .bind(&row.group_id)
                        .fetch_optional(&s.db)
                        .await?;
                found
                    .ok_or_else(|| {
                        ApiError::unprocessable("invalid_playlist", "playlist does not exist in the screen's group")
                    })?
                    .0
            }
        };
        sqlx::query("UPDATE screens SET playlist_id = ? WHERE id = ?").bind(playlist).bind(&id).execute(&s.db).await?;
        s.notify_manifest(&id, version);
    }
    Ok(Json(to_screen(&s, fetch_row(&s, &id).await?)))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.group_id, Role::Editor)?;
    sqlx::query("DELETE FROM screens WHERE id = ?").bind(&id).execute(&s.db).await?;
    s.forget_device_tokens(&id);
    s.hub.send_device(&id, &json!({ "type": "unpaired" }));
    s.hub.close_device(&id);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn command(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(cmd): ApiJson<ScreenCommand>,
) -> ApiResult<StatusCode> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.group_id, Role::Editor)?;
    if !s.hub.send_device(&id, &json!({ "type": "command", "command": cmd })) {
        return Err(ApiError::conflict("screen_offline", "screen is offline"));
    }
    Ok(StatusCode::ACCEPTED)
}
