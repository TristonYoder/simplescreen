use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    auth::Principal,
    error::{ApiError, ApiJson, ApiResult},
    models::{
        CommandName, Screen, ScreenCommand, ScreenOverride, ScreenOverrideInput, ScreenPatch, ScreenPlacement, ScreenRow, ScreenState,
        ScreenStatus, SCREEN_COLUMNS,
    },
    notices::{self, Actor},
    reach::{self, World},
    schedule::{self, Clash, CLEAR_OVERRIDE},
    state::AppState,
    util::now,
};

#[derive(Serialize, Clone, Debug)]
pub struct Warning {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_id: Option<String>,
}

fn view(s: &AppState, w: &World, row: ScreenRow, outputs: &HashMap<String, Vec<String>>) -> Screen {
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
        set_by: row.override_set_by.clone(),
        set_by_name: row.override_set_by_name.clone(),
        team_id: row.override_team_id.clone(),
    });
    let placement = match (row.placement_plan_id, row.placement_x, row.placement_y) {
        (Some(floor_plan_id), Some(x), Some(y)) => Some(ScreenPlacement { floor_plan_id, x, y }),
        _ => None,
    };
    let mut tag_ids: Vec<String> = w.screen_tags.get(&row.id).map(|t| t.iter().cloned().collect()).unwrap_or_default();
    tag_ids.sort();
    Screen {
        team_ids: w.screen_teams(&row.id),
        output_keys: outputs.get(&row.id).cloned().unwrap_or_default(),
        id: row.id,
        name: row.name,
        tag_ids,
        fallback_playlist_id: row.fallback_playlist_id,
        timezone: row.timezone,
        override_,
        placement,
        node_id: row.node_id,
        online,
        status,
        last_seen_at: row.last_seen_at,
        created_at: row.created_at,
    }
}

pub async fn views(s: &AppState, w: &World, rows: Vec<ScreenRow>) -> ApiResult<Vec<Screen>> {
    let pairs: Vec<(String, String)> = sqlx::query_as(
        "SELECT a.screen_id, a.output_key FROM assignments a JOIN screens s ON s.id = a.screen_id AND s.node_id = a.node_id \
         WHERE a.job = 'program' ORDER BY a.output_key",
    )
    .fetch_all(&s.db)
    .await?;
    let mut outputs: HashMap<String, Vec<String>> = HashMap::new();
    for (screen, key) in pairs {
        outputs.entry(screen).or_default().push(key);
    }
    Ok(rows.into_iter().map(|r| view(s, w, r, &outputs)).collect())
}

pub async fn to_screen(s: &AppState, row: ScreenRow) -> ApiResult<Screen> {
    let w = World::load(s).await?;
    Ok(views(s, &w, vec![row]).await?.remove(0))
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

pub async fn list(State(s): State<AppState>, p: Principal, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Screen>>> {
    super::assignments::require_user(&p)?;
    let w = World::load(&s).await?;
    if let Some(t) = &q.team_id {
        p.require(t, crate::auth::Role::Viewer)?;
    }
    let rows: Vec<ScreenRow> = schedule::all_screens(&s)
        .await?
        .into_iter()
        .filter(|r| p.sees_screen(&w, &r.id))
        .filter(|r| q.team_id.as_deref().is_none_or(|t| w.reaches(t, &r.id)))
        .collect();
    Ok(Json(views(&s, &w, rows).await?))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Screen>> {
    let w = World::load(&s).await?;
    let row = fetch_row(&s, &id).await?;
    if !p.sees_screen(&w, &id) {
        return Err(ApiError::not_found());
    }
    Ok(Json(views(&s, &w, vec![row]).await?.remove(0)))
}

/// A playlist someone may show on a screen: one of their team's (any team for admins and devices)
/// that is available there. Returns its version.
async fn check_playlist(s: &AppState, w: &World, p: &Principal, screen: &str, playlist: Option<&str>) -> ApiResult<i64> {
    let Some(pid) = playlist else { return Ok(0) };
    let found: Option<(i64, String)> = sqlx::query_as("SELECT version, team_id FROM playlists WHERE id = ?").bind(pid).fetch_optional(&s.db).await?;
    let unavailable = || ApiError::unprocessable("playlist_unavailable", "playlist is not one of your team's playlists available on this screen");
    let (version, team) = found.ok_or_else(unavailable)?;
    if !(p.is_device() || p.edits_team(&team)) || !w.available(pid, screen) {
        return Err(unavailable());
    }
    Ok(version)
}

pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<ScreenPatch>,
) -> ApiResult<Json<Screen>> {
    let w = World::load(&s).await?;
    let row = fetch_row(&s, &id).await?;
    p.require_operate(&w, &id)?;
    let actor = Actor::of(&p, &w, &id);
    let name = match &patch.name {
        Some(n) if n.trim().is_empty() => return Err(ApiError::bad_request("name must not be empty")),
        Some(n) => Some(n.trim().to_string()),
        None => None,
    };
    let timezone = match &patch.timezone {
        Some(Some(t)) if schedule::parse_tz(t.trim()).is_none() => {
            return Err(ApiError::unprocessable("invalid_timezone", "timezone must be an IANA zone name"))
        }
        Some(t) => Some(t.as_ref().map(|t| t.trim().to_string())),
        None => None,
    };
    let mut version = 0;
    if let Some(fallback) = &patch.fallback_playlist_id {
        version = check_playlist(&s, &w, &p, &id, fallback.as_deref()).await?;
    }
    if let Some(Some(pl)) = &patch.placement {
        if !(0.0..=1.0).contains(&pl.x) || !(0.0..=1.0).contains(&pl.y) {
            return Err(ApiError::bad_request("placement x and y must be between 0 and 1"));
        }
        let found: Option<(String,)> = sqlx::query_as("SELECT id FROM floor_plans WHERE id = ?").bind(&pl.floor_plan_id).fetch_optional(&s.db).await?;
        if found.is_none() {
            return Err(ApiError::unprocessable("invalid_floor_plan", "floor plan does not exist"));
        }
    }

    if let Some(n) = &name {
        sqlx::query("UPDATE screens SET name = ? WHERE id = ?").bind(n).bind(&id).execute(&s.db).await?;
        notices::log(&s, &id, "renamed", &actor, json!({ "name": n })).await;
    }
    if let Some(placement) = &patch.placement {
        let pl = placement.as_ref();
        sqlx::query("UPDATE screens SET placement_plan_id = ?, placement_x = ?, placement_y = ? WHERE id = ?")
            .bind(pl.map(|p| &p.floor_plan_id))
            .bind(pl.map(|p| p.x))
            .bind(pl.map(|p| p.y))
            .bind(&id)
            .execute(&s.db)
            .await?;
        notices::log(&s, &id, "placed", &actor, json!({ "placement": pl })).await;
    }
    if let Some(fallback) = &patch.fallback_playlist_id {
        sqlx::query("UPDATE screens SET fallback_playlist_id = ? WHERE id = ?").bind(fallback).bind(&id).execute(&s.db).await?;
        notices::log(&s, &id, "fallback", &actor, json!({ "playlist_id": fallback })).await;
    }
    if let Some(tz) = &timezone {
        sqlx::query("UPDATE screens SET timezone = ? WHERE id = ?").bind(tz).bind(&id).execute(&s.db).await?;
        notices::log(&s, &id, "timezone", &actor, json!({ "timezone": tz })).await;
        schedule::refresh_clashes(&s, Some(&p)).await?;
    }
    if patch.fallback_playlist_id.is_some() || timezone.is_some() || name.is_some() {
        changed(&s, &row, version).await;
    } else {
        broadcast_updated(&s, &id).await;
    }
    Ok(Json(to_screen(&s, fetch_row(&s, &id).await?).await?))
}

#[derive(Deserialize)]
pub struct TagIds {
    pub tag_ids: Vec<String>,
}

/// Warnings for a screen whose fallback or override is no longer available.
pub fn availability_warnings(w: &World, row: &ScreenRow) -> Vec<Warning> {
    let mut out = vec![];
    if let Some(pl) = row.fallback_playlist_id.as_deref().filter(|p| !w.available(p, &row.id)) {
        out.push(Warning {
            code: "fallback_unavailable",
            message: format!("{}: the fallback playlist is no longer available; the home menu shows instead", row.name),
            screen_id: Some(row.id.clone()),
            playlist_id: Some(pl.to_string()),
        });
    }
    if let Some(pl) = row.override_playlist_id.as_deref().filter(|p| row.override_set && !w.available(p, &row.id)) {
        out.push(Warning {
            code: "override_unavailable",
            message: format!("{}: the override playlist is no longer available and is skipped", row.name),
            screen_id: Some(row.id.clone()),
            playlist_id: Some(pl.to_string()),
        });
    }
    out
}

pub async fn put_tags(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<TagIds>,
) -> ApiResult<Json<Value>> {
    p.require_admin()?;
    let row = fetch_row(&s, &id).await?;
    let tags = reach::check_tags(&s, &input.tag_ids).await?;
    let before = reach::screen_team_ids(&s, &id).await?;
    let mut tx = s.db.begin().await?;
    sqlx::query("DELETE FROM screen_tags WHERE screen_id = ?").bind(&id).execute(&mut *tx).await?;
    for t in &tags {
        sqlx::query("INSERT INTO screen_tags (screen_id, tag_id) VALUES (?, ?)").bind(&id).bind(t).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    let w = World::load(&s).await?;
    let warnings = availability_warnings(&w, &row);
    let clashes = schedule::refresh_clashes(&s, Some(&p)).await?;
    notices::sync_untagged(&s, &id).await?;
    notices::log(&s, &id, "tags", &Actor::of(&p, &w, &id), json!({ "tag_ids": tags })).await;
    s.hub.broadcast_teams(&before, &json!({ "type": "screen_updated", "screen_id": id }));
    changed(&s, &row, 0).await;
    let screen = views(&s, &w, vec![fetch_row(&s, &id).await?]).await?.remove(0);
    Ok(Json(json!({ "screen": screen, "clashes": clashes, "warnings": warnings })))
}

#[derive(Deserialize)]
pub struct EventsQuery {
    limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
struct EventRow {
    id: String,
    screen_id: String,
    kind: String,
    user_sub: Option<String>,
    user_name: Option<String>,
    team_id: Option<String>,
    detail: String,
    created_at: String,
}

pub async fn events(State(s): State<AppState>, p: Principal, Path(id): Path<String>, Query(q): Query<EventsQuery>) -> ApiResult<Json<Vec<Value>>> {
    let w = World::load(&s).await?;
    fetch_row(&s, &id).await?;
    if !p.sees_screen(&w, &id) {
        return Err(ApiError::not_found());
    }
    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT id, screen_id, kind, user_sub, user_name, team_id, detail, created_at FROM screen_events WHERE screen_id = ? \
         ORDER BY created_at DESC, rowid DESC LIMIT ?",
    )
    .bind(&id)
    .bind(q.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(&s.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let mut v = json!({ "id": r.id, "screen_id": r.screen_id, "kind": r.kind, "created_at": r.created_at });
                v["detail"] = serde_json::from_str(&r.detail).unwrap_or_else(|_| json!({}));
                for (k, val) in [("user_sub", r.user_sub), ("user_name", r.user_name), ("team_id", r.team_id)] {
                    if let Some(val) = val {
                        v[k] = json!(val);
                    }
                }
                v
            })
            .collect(),
    ))
}

/// Sets the override after the caller's checks. `p` may be a device (the node is the actor).
pub async fn set_override_row(
    s: &AppState,
    w: &World,
    p: &Principal,
    row: &ScreenRow,
    playlist: Option<&str>,
    until: Option<chrono::DateTime<chrono::Utc>>,
) -> ApiResult<()> {
    let version = check_playlist(s, w, p, &row.id, playlist).await?;
    if until.is_some_and(|u| u <= chrono::Utc::now()) {
        return Err(ApiError::unprocessable("invalid_override", "until must be in the future"));
    }
    let actor = Actor::of(p, w, &row.id);
    let team = match playlist {
        Some(pl) => w.playlist_team.get(pl).cloned(),
        None => actor.team_id.clone(),
    };
    let until = until.map(|u| u.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    sqlx::query(
        "UPDATE screens SET override_set = 1, override_playlist_id = ?, override_until = ?, override_set_at = ?, \
         override_set_by = ?, override_set_by_name = ?, override_team_id = ? WHERE id = ?",
    )
    .bind(playlist)
    .bind(&until)
    .bind(now())
    .bind(&actor.sub)
    .bind(&actor.name)
    .bind(&team)
    .bind(&row.id)
    .execute(&s.db)
    .await?;
    notices::log(s, &row.id, "override", &actor, json!({ "playlist_id": playlist, "until": until })).await;
    changed(s, row, version).await;
    Ok(())
}

pub async fn clear_override_row(s: &AppState, w: &World, p: &Principal, row: &ScreenRow) -> ApiResult<()> {
    sqlx::query(&format!("UPDATE screens SET {CLEAR_OVERRIDE} WHERE id = ?")).bind(&row.id).execute(&s.db).await?;
    notices::log(s, &row.id, "override_cleared", &Actor::of(p, w, &row.id), json!({})).await;
    changed(s, row, 0).await;
    Ok(())
}

pub async fn changed(s: &AppState, row: &ScreenRow, version: i64) {
    s.notify_manifest(row.node_id.as_deref(), &row.id, version);
    s.schedule_wake.notify_one();
    broadcast_updated(s, &row.id).await;
}

pub async fn set_override(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<ScreenOverrideInput>,
) -> ApiResult<Json<Screen>> {
    let w = World::load(&s).await?;
    let row = fetch_row(&s, &id).await?;
    p.require_operate(&w, &id)?;
    let until = match (input.until, input.until_next_change) {
        (Some(u), _) => Some(u),
        (None, true) => schedule::next_change(&schedule::base_timeline(&s, &row, chrono::Utc::now()).await?),
        (None, false) => None,
    };
    set_override_row(&s, &w, &p, &row, input.playlist_id.as_deref(), until).await?;
    Ok(Json(to_screen(&s, fetch_row(&s, &id).await?).await?))
}

pub async fn clear_override(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Screen>> {
    let w = World::load(&s).await?;
    let row = fetch_row(&s, &id).await?;
    p.require_operate(&w, &id)?;
    clear_override_row(&s, &w, &p, &row).await?;
    Ok(Json(to_screen(&s, fetch_row(&s, &id).await?).await?))
}

#[derive(Deserialize)]
pub struct BulkOverride {
    #[serde(default)]
    screen_ids: Vec<String>,
    #[serde(default)]
    tag_ids: Vec<String>,
    playlist_id: Option<String>,
    until: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    clear: bool,
}

pub async fn bulk_override(State(s): State<AppState>, p: Principal, ApiJson(input): ApiJson<BulkOverride>) -> ApiResult<Json<Value>> {
    super::assignments::require_user(&p)?;
    let w = World::load(&s).await?;
    let mut targets: Vec<String> = vec![];
    for id in &input.screen_ids {
        if !w.screen_tags.contains_key(id) || !p.sees_screen(&w, id) {
            return Err(ApiError::not_found());
        }
        if !p.operates_screen(&w, id) {
            return Err(ApiError::forbidden());
        }
        if !targets.contains(id) {
            targets.push(id.clone());
        }
    }
    let tags: HashSet<&String> = input.tag_ids.iter().collect();
    let mut by_tag: Vec<&String> = w
        .screen_tags
        .iter()
        .filter(|(id, t)| t.iter().any(|x| tags.contains(x)) && p.operates_screen(&w, id))
        .map(|(id, _)| id)
        .collect();
    by_tag.sort();
    for id in by_tag {
        if !targets.contains(id) {
            targets.push(id.clone());
        }
    }
    if targets.is_empty() {
        return Err(ApiError::unprocessable("no_screens", "no screens you operate match"));
    }
    if let (false, Some(pl)) = (input.clear, &input.playlist_id) {
        let team: Option<(String,)> = sqlx::query_as("SELECT team_id FROM playlists WHERE id = ?").bind(pl).fetch_optional(&s.db).await?;
        if !team.is_some_and(|(t,)| p.edits_team(&t)) {
            return Err(ApiError::unprocessable("playlist_unavailable", "playlist is not one of your team's playlists"));
        }
    }
    let mut warnings = vec![];
    let mut done = vec![];
    for id in targets {
        let row = fetch_row(&s, &id).await?;
        if input.clear {
            clear_override_row(&s, &w, &p, &row).await?;
        } else if input.playlist_id.as_deref().is_some_and(|pl| !w.available(pl, &id)) {
            warnings.push(Warning {
                code: "override_unavailable",
                message: format!("{}: the playlist is not available on this screen", row.name),
                screen_id: Some(id.clone()),
                playlist_id: input.playlist_id.clone(),
            });
            continue;
        } else {
            set_override_row(&s, &w, &p, &row, input.playlist_id.as_deref(), input.until).await?;
        }
        done.push(fetch_row(&s, &id).await?);
    }
    let screens = views(&s, &w, done).await?;
    Ok(Json(json!({ "screens": screens, "warnings": warnings })))
}

pub async fn broadcast_updated(s: &AppState, screen_id: &str) {
    s.broadcast_screen(screen_id, &json!({ "type": "screen_updated", "screen_id": screen_id })).await;
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    p.require_admin()?;
    let row = fetch_row(&s, &id).await?;
    let teams = reach::screen_team_ids(&s, &id).await?;

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
    s.hub.broadcast_teams(&teams, &offline);
    s.hub.send_subscribers(&id, row.node_id.as_deref(), &offline);
    if let Some(node_id) = &removed_node {
        s.forget_device_tokens(node_id);
        s.node_statuses.lock().remove(node_id);
        s.hub.send_node(node_id, &json!({ "type": "unpaired" }));
        s.hub.close_node(node_id);
    }
    for (node_id, version) in &versions {
        s.hub.send_node(node_id, &json!({ "type": "assignments_changed", "version": version }));
        s.hub.broadcast_all(&json!({ "type": "node_updated", "node_id": node_id }));
        crate::assignments::refresh_subscriptions(&s, node_id).await;
    }
    notices::sync_untagged(&s, &id).await?;
    let _: Vec<Clash> = schedule::refresh_clashes(&s, Some(&p)).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn command(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(cmd): ApiJson<ScreenCommand>,
) -> ApiResult<StatusCode> {
    let w = World::load(&s).await?;
    let row = fetch_row(&s, &id).await?;
    p.require_operate(&w, &id)?;
    let status = send_command(&s, &row, &cmd)?;
    notices::log(&s, &id, "command", &Actor::of(&p, &w, &id), json!({ "command": cmd })).await;
    Ok(status)
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
