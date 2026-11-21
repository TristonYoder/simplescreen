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
    models::{
        CommandName, Screen, ScreenCommand, ScreenOverride, ScreenPlacement, ScreenOverrideInput, ScreenPatch, ScreenRow, ScreenState, ScreenStatus,
        SCREEN_COLUMNS,
    },
    schedule,
    util::now,
    state::AppState,
};

pub fn to_screen(s: &AppState, row: ScreenRow) -> Screen {
    let online = row.node_id.as_deref().is_some_and(|n| s.hub.is_online(n));
    let status = if online {
        s.statuses.lock().get(&row.id).cloned().unwrap_or_else(|| ScreenStatus::simple(ScreenState::Idle))
    } else {
        ScreenStatus::simple(ScreenState::Offline)
    };
    let override_ = row.override_set.then(|| ScreenOverride {
        playlist_id: row.override_playlist_id.clone(),
        until: row.override_until.clone(),
        set_at: row.override_set_at.clone().unwrap_or_default(),
    });
    let placement = match (row.placement_plan_id, row.placement_x, row.placement_y) {
        (Some(floor_plan_id), Some(x), Some(y)) => Some(ScreenPlacement { floor_plan_id, x, y }),
        _ => None,
    };
    Screen {
        id: row.id,
        team_id: row.team_id,
        name: row.name,
        playlist_id: row.playlist_id,
        override_,
        placement,
        node_id: row.node_id,
        output_key: row.output_key,
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
    team_id: Option<String>,
}

pub async fn list(
    State(s): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Screen>>> {
    let rows: Vec<ScreenRow> = sqlx::query_as(&format!(
        "SELECT {SCREEN_COLUMNS} FROM screens WHERE (?1 IS NULL OR team_id = ?1) ORDER BY name"
    ))
    .bind(&q.team_id)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows.into_iter().filter(|r| p.can_view(&r.team_id)).map(|r| to_screen(&s, r)).collect()))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Screen>> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.team_id, Role::Viewer)?;
    Ok(Json(to_screen(&s, row)))
}

pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<ScreenPatch>,
) -> ApiResult<Json<Screen>> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.team_id, Role::Editor)?;
    if let Some(name) = &patch.name {
        if name.trim().is_empty() {
            return Err(ApiError::bad_request("name must not be empty"));
        }
        sqlx::query("UPDATE screens SET name = ? WHERE id = ?").bind(name.trim()).bind(&id).execute(&s.db).await?;
    }
    if let Some(placement) = &patch.placement {
        set_placement(&s, &row, placement.as_ref()).await?;
    }
    if let Some(playlist) = &patch.playlist_id {
        assign_playlist(&s, &row, playlist.as_deref()).await?;
    } else if patch.name.is_some() || patch.placement.is_some() {
        broadcast_updated(&s, &row);
    }
    Ok(Json(to_screen(&s, fetch_row(&s, &id).await?)))
}

async fn set_placement(s: &AppState, row: &ScreenRow, placement: Option<&ScreenPlacement>) -> ApiResult<()> {
    if let Some(p) = placement {
        if !(0.0..=1.0).contains(&p.x) || !(0.0..=1.0).contains(&p.y) {
            return Err(ApiError::bad_request("placement x and y must be between 0 and 1"));
        }
        let found: Option<(String,)> = sqlx::query_as("SELECT id FROM floor_plans WHERE id = ? AND team_id = ?")
            .bind(&p.floor_plan_id)
            .bind(&row.team_id)
            .fetch_optional(&s.db)
            .await?;
        if found.is_none() {
            return Err(ApiError::unprocessable("invalid_floor_plan", "floor plan does not exist in the screen's team"));
        }
    }
    sqlx::query("UPDATE screens SET placement_plan_id = ?, placement_x = ?, placement_y = ? WHERE id = ?")
        .bind(placement.map(|p| &p.floor_plan_id))
        .bind(placement.map(|p| p.x))
        .bind(placement.map(|p| p.y))
        .bind(&row.id)
        .execute(&s.db)
        .await?;
    Ok(())
}

async fn check_playlist(s: &AppState, row: &ScreenRow, playlist: Option<&str>) -> ApiResult<i64> {
    let Some(pid) = playlist else { return Ok(0) };
    let found: Option<(i64,)> = sqlx::query_as("SELECT version FROM playlists WHERE id = ? AND team_id = ?")
        .bind(pid)
        .bind(&row.team_id)
        .fetch_optional(&s.db)
        .await?;
    Ok(found.ok_or_else(|| ApiError::unprocessable("invalid_playlist", "playlist does not exist in the screen's team"))?.0)
}

/// Sets (or with `None` clears) the default playlist from the screen's own team and notifies the device and team users.
pub async fn assign_playlist(s: &AppState, row: &ScreenRow, playlist: Option<&str>) -> ApiResult<()> {
    let version = check_playlist(s, row, playlist).await?;
    sqlx::query("UPDATE screens SET playlist_id = ? WHERE id = ?").bind(playlist).bind(&row.id).execute(&s.db).await?;
    changed(s, row, version);
    Ok(())
}

pub async fn set_override_row(
    s: &AppState,
    row: &ScreenRow,
    playlist: Option<&str>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> ApiResult<()> {
    let version = check_playlist(s, row, playlist).await?;
    if until.is_some_and(|u| u <= chrono::Utc::now()) {
        return Err(ApiError::unprocessable("invalid_override", "until must be in the future"));
    }
    sqlx::query("UPDATE screens SET override_set = 1, override_playlist_id = ?, override_until = ?, override_set_at = ? WHERE id = ?")
        .bind(playlist)
        .bind(until.map(|u| u.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
        .bind(now())
        .bind(&row.id)
        .execute(&s.db)
        .await?;
    changed(s, row, version);
    Ok(())
}

pub async fn clear_override_row(s: &AppState, row: &ScreenRow) -> ApiResult<()> {
    sqlx::query("UPDATE screens SET override_set = 0, override_playlist_id = NULL, override_until = NULL, override_set_at = NULL WHERE id = ?")
        .bind(&row.id)
        .execute(&s.db)
        .await?;
    changed(s, row, 0);
    Ok(())
}

fn changed(s: &AppState, row: &ScreenRow, version: i64) {
    s.notify_manifest(row.node_id.as_deref(), &row.id, version);
    s.schedule_wake.notify_one();
    broadcast_updated(s, row);
}

pub async fn set_override(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<ScreenOverrideInput>,
) -> ApiResult<Json<Screen>> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.team_id, Role::Editor)?;
    let until = match (input.until, input.until_next_change) {
        (Some(u), _) => Some(u),
        (None, true) => schedule::next_change(&schedule::base_timeline(&s, &row, chrono::Utc::now()).await?),
        (None, false) => None,
    };
    set_override_row(&s, &row, input.playlist_id.as_deref(), until).await?;
    Ok(Json(to_screen(&s, fetch_row(&s, &id).await?)))
}

pub async fn clear_override(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Screen>> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.team_id, Role::Editor)?;
    clear_override_row(&s, &row).await?;
    Ok(Json(to_screen(&s, fetch_row(&s, &id).await?)))
}

pub fn broadcast_updated(s: &AppState, row: &ScreenRow) {
    s.hub.broadcast_team(&row.team_id, &json!({ "type": "screen_updated", "screen_id": row.id }));
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.team_id, Role::Editor)?;

    let mut tx = s.db.begin_with("BEGIN IMMEDIATE").await?;
    let pointing: Vec<(String,)> =
        sqlx::query_as("SELECT DISTINCT node_id FROM assignments WHERE screen_id = ?").bind(&id).fetch_all(&mut *tx).await?;
    // Off, with no screen id, before the row goes away. ON DELETE SET NULL must not leave a live job.
    sqlx::query("UPDATE assignments SET job = 'off', screen_id = NULL, settings = '{}' WHERE screen_id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM screens WHERE id = ?").bind(&id).execute(&mut *tx).await?;

    let mut node_ids: Vec<String> = pointing.into_iter().map(|(n,)| n).collect();
    if let Some(owner) = &row.node_id {
        if !node_ids.iter().any(|n| n == owner) {
            node_ids.push(owner.clone());
        }
    }
    let mut versions = Vec::new();
    let mut removed_node = None;
    for node_id in &node_ids {
        let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM nodes WHERE id = ?").bind(node_id).fetch_optional(&mut *tx).await?;
        if exists.is_none() {
            continue;
        }
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM screens WHERE node_id = ?").bind(node_id).fetch_one(&mut *tx).await?;
        // `nodes.single` is the pairing kind, not a screen count. Unpair only when nothing is left.
        if row.node_id.as_deref() == Some(node_id.as_str()) && remaining == 0 {
            sqlx::query("DELETE FROM nodes WHERE id = ?").bind(node_id).execute(&mut *tx).await?;
            removed_node = Some(node_id.clone());
            continue;
        }
        crate::assignments::sync_reported_outputs(&mut tx, node_id).await?;
        let version: i64 = sqlx::query_scalar(
            "UPDATE nodes SET assignments_version = assignments_version + 1 WHERE id = ? RETURNING assignments_version",
        )
        .bind(node_id)
        .fetch_one(&mut *tx)
        .await?;
        versions.push((node_id.clone(), version));
    }
    tx.commit().await?;

    s.statuses.lock().remove(&id);
    let offline = json!({ "type": "screen_offline", "screen_id": &id });
    s.hub.broadcast_team(&row.team_id, &offline);
    s.hub.send_subscribers(&id, row.node_id.as_deref(), &offline);
    if let Some(node_id) = &removed_node {
        s.forget_device_tokens(node_id);
        s.node_statuses.lock().remove(node_id);
        s.hub.send_node(node_id, &json!({ "type": "unpaired" }));
        s.hub.close_node(node_id);
    }
    for (node_id, version) in &versions {
        s.hub.send_node(node_id, &json!({ "type": "assignments_changed", "version": version }));
        s.hub.broadcast_team(&row.team_id, &json!({ "type": "node_updated", "node_id": node_id }));
        crate::assignments::refresh_subscriptions(&s, node_id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn command(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(cmd): ApiJson<ScreenCommand>,
) -> ApiResult<StatusCode> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.team_id, Role::Editor)?;
    send_command(&s, &row, &cmd)
}

/// Validates a command and delivers it to the node that plays the screen.
pub fn send_command(s: &AppState, row: &ScreenRow, cmd: &ScreenCommand) -> ApiResult<StatusCode> {
    if matches!(cmd.name, CommandName::Jump) && cmd.item_index.is_none_or(|i| i < 0) {
        return Err(ApiError::bad_request("jump requires a non-negative item_index"));
    }
    if cmd.step_index.is_some_and(|i| i < 0) {
        return Err(ApiError::bad_request("step_index must not be negative"));
    }
    let delivered = row.node_id.as_deref().is_some_and(|n| {
        s.hub.send_node(n, &json!({ "type": "command", "screen_id": row.id, "command": cmd }))
    });
    if !delivered {
        return Err(ApiError::conflict("screen_offline", "screen is offline"));
    }
    Ok(StatusCode::ACCEPTED)
}
