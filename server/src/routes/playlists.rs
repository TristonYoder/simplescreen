use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    models::{
        Bed, ItemKind, Playlist, PlaylistDefaults, PlaylistInput, PlaylistItem, PlaylistRow, PlaylistUpdate,
        PLAYLIST_COLUMNS,
    },
    reach::{self, World},
    schedule,
    state::AppState,
    util::{new_id, now},
};

#[derive(Deserialize)]
pub struct ListQuery {
    team_id: Option<String>,
    scope: Option<String>,
}

#[derive(Serialize)]
pub struct Summary {
    id: String,
    team_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    folder_id: Option<String>,
    tag_ids: Vec<String>,
    name: String,
    version: i64,
    item_count: i64,
    total_bytes: i64,
    updated_at: String,
}

/// Viewers of the owning team, and anyone who reaches a screen the playlist is available on.
pub fn visible(p: &Principal, w: &World, playlist: &str, team: &str) -> bool {
    p.can_view(team) || w.playlist_screens(playlist).iter().any(|s| p.sees_screen(w, s))
}

pub async fn list(
    State(s): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Summary>>> {
    let all = summaries(&s, q.team_id.as_deref()).await?;
    let wide = match q.scope.as_deref() {
        None | Some("mine") => false,
        Some("visible") => true,
        Some(_) => return Err(ApiError::bad_request("scope must be mine or visible")),
    };
    let w = if wide { Some(World::load(&s).await?) } else { None };
    Ok(Json(
        all.into_iter()
            .filter(|r| p.can_view(&r.team_id) || w.as_ref().is_some_and(|w| visible(&p, w, &r.id, &r.team_id)))
            .collect(),
    ))
}

async fn tag_ids(s: &AppState, playlist: &str) -> ApiResult<Vec<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT tag_id FROM playlist_tags WHERE playlist_id = ? ORDER BY tag_id").bind(playlist).fetch_all(&s.db).await?;
    Ok(rows.into_iter().map(|(t,)| t).collect())
}

pub async fn summaries(s: &AppState, team_id: Option<&str>) -> ApiResult<Vec<Summary>> {
    let rows: Vec<(String, String, Option<String>, String, i64, i64, i64, String, Option<String>)> = sqlx::query_as(
        "SELECT p.id, p.team_id, p.folder_id, p.name, p.version, json_array_length(p.items),
                COALESCE((SELECT SUM(m.size) FROM playlist_media pm JOIN media m ON m.id = pm.media_id
                          WHERE pm.playlist_id = p.id), 0),
                p.updated_at,
                (SELECT json_group_array(tag_id) FROM (SELECT tag_id FROM playlist_tags WHERE playlist_id = p.id ORDER BY tag_id))
         FROM playlists p WHERE (?1 IS NULL OR p.team_id = ?1) ORDER BY p.name",
    )
    .bind(team_id)
    .fetch_all(&s.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, team_id, folder_id, name, version, item_count, total_bytes, updated_at, tags)| Summary {
            id,
            team_id,
            folder_id,
            tag_ids: tags.and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default(),
            name,
            version,
            item_count,
            total_bytes,
            updated_at,
        })
        .collect())
}

pub async fn fetch(s: &AppState, id: &str) -> ApiResult<Playlist> {
    let row: Option<PlaylistRow> =
        sqlx::query_as(&format!("SELECT {PLAYLIST_COLUMNS} FROM playlists WHERE id = ?"))
            .bind(id)
            .fetch_optional(&s.db)
            .await?;
    let row = row.ok_or_else(ApiError::not_found)?;
    let tags = tag_ids(s, id).await?;
    Ok(row.into_playlist(tags)?)
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Playlist>> {
    let pl = fetch(&s, &id).await?;
    if !p.can_view(&pl.team_id) {
        let w = World::load(&s).await?;
        if !visible(&p, &w, &pl.id, &pl.team_id) {
            return Err(ApiError::not_found());
        }
    }
    Ok(Json(pl))
}

async fn check_folder(s: &AppState, team_id: &str, folder: Option<&str>) -> ApiResult<()> {
    let Some(f) = folder else { return Ok(()) };
    let found: Option<(String,)> =
        sqlx::query_as("SELECT id FROM playlist_folders WHERE id = ? AND team_id = ?").bind(f).bind(team_id).fetch_optional(&s.db).await?;
    found.map(|_| ()).ok_or_else(|| ApiError::unprocessable("invalid_folder", "folder does not exist in the team"))
}

#[derive(Deserialize)]
pub struct PlaylistPatch {
    folder_id: Option<String>,
}

pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<PlaylistPatch>,
) -> ApiResult<Json<Playlist>> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    check_folder(&s, &current.team_id, input.folder_id.as_deref()).await?;
    sqlx::query("UPDATE playlists SET folder_id = ? WHERE id = ?").bind(&input.folder_id).bind(&id).execute(&s.db).await?;
    s.notify_screens_for_playlist(&id, current.version).await;
    Ok(Json(fetch(&s, &id).await?))
}

#[derive(Deserialize)]
pub struct TagIds {
    tag_ids: Vec<String>,
}

pub async fn put_tags(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<TagIds>,
) -> ApiResult<Json<Value>> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    let tags = reach::check_tags(&s, &input.tag_ids).await?;
    let w = World::load(&s).await?;
    if !p.server_admin {
        if let Some(t) = tags.iter().find(|t| !w.team_holds(&current.team_id, t)) {
            return Err(ApiError::unprocessable("invalid_tag", format!("tag {t} is not held by the playlist's team")));
        }
    }
    let before = w.playlist_screens(&id);
    let mut tx = s.db.begin().await?;
    sqlx::query("DELETE FROM playlist_tags WHERE playlist_id = ?").bind(&id).execute(&mut *tx).await?;
    for t in &tags {
        sqlx::query("INSERT INTO playlist_tags (playlist_id, tag_id) VALUES (?, ?)").bind(&id).bind(t).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    let clashes = schedule::refresh_clashes(&s, Some(&p)).await?;
    let w = World::load(&s).await?;
    let mut warnings = vec![];
    let mut touched: Vec<String> = before;
    touched.extend(w.playlist_screens(&id));
    touched.sort();
    touched.dedup();
    for screen in &touched {
        let row = super::screens::fetch_row(&s, screen).await?;
        if row.fallback_playlist_id.as_deref() == Some(id.as_str()) || row.override_playlist_id.as_deref() == Some(id.as_str()) {
            warnings.extend(super::screens::availability_warnings(&w, &row));
        }
        s.notify_manifest(row.node_id.as_deref(), &row.id, current.version);
    }
    s.schedule_wake.notify_one();
    Ok(Json(json!({ "playlist": fetch(&s, &id).await?, "clashes": clashes, "warnings": warnings })))
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::unprocessable("invalid_playlist", msg)
}

async fn validate(
    s: &AppState,
    team_id: &str,
    defaults: &PlaylistDefaults,
    items: &mut [PlaylistItem],
    beds: &mut [Bed],
) -> ApiResult<()> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, kind FROM media WHERE team_id = ?").bind(team_id).fetch_all(&s.db).await?;
    let media: HashMap<String, String> = rows.into_iter().collect();
    let check = |id: &Uuid, kind: &str| -> ApiResult<()> {
        match media.get(&id.to_string()) {
            None => Err(invalid(format!("media {id} does not exist in this team"))),
            Some(k) if k == kind => Ok(()),
            Some(k) => Err(invalid(format!("media {id} is {k}, expected {kind}"))),
        }
    };

    if defaults.transition.duration_ms > 10000 {
        return Err(invalid("transition duration_ms must be <= 10000"));
    }
    let mut index: HashMap<Uuid, usize> = HashMap::new();
    for (i, item) in items.iter_mut().enumerate() {
        let id = *item.id.get_or_insert_with(Uuid::new_v4);
        if index.insert(id, i).is_some() {
            return Err(invalid(format!("duplicate item id {id}")));
        }
        match item.kind {
            ItemKind::Video | ItemKind::Image => {
                let m = item.media_id.as_ref().ok_or_else(|| invalid(format!("item {i} requires media_id")))?;
                check(m, if item.kind == ItemKind::Video { "video" } else { "image" })?;
                if item.pages.is_some() {
                    return Err(invalid(format!("item {i} must not have pages")));
                }
            }
            ItemKind::Sequence => {
                let pages = item.pages.as_ref().filter(|p| !p.is_empty());
                let pages = pages.ok_or_else(|| invalid(format!("item {i} requires pages")))?;
                for m in pages {
                    check(m, "image")?;
                }
            }
        }
        if item.volume.is_some_and(|v| !(0.0..=1.0).contains(&v)) {
            return Err(invalid(format!("item {i} volume must be within 0..1")));
        }
        if item.transition.as_ref().is_some_and(|t| t.duration_ms > 10000) {
            return Err(invalid(format!("item {i} transition duration_ms must be <= 10000")));
        }
    }

    let mut ranges: Vec<(usize, usize)> = vec![];
    for (b, bed) in beds.iter_mut().enumerate() {
        bed.id.get_or_insert_with(Uuid::new_v4);
        if bed.tracks.is_empty() {
            return Err(invalid(format!("bed {b} requires at least one track")));
        }
        for t in &bed.tracks {
            check(t, "audio")?;
        }
        for v in [bed.volume, bed.duck_volume] {
            if !(0.0..=1.0).contains(&v) {
                return Err(invalid(format!("bed {b} volumes must be within 0..1")));
            }
        }
        let start = *index.get(&bed.start_item_id).ok_or_else(|| invalid(format!("bed {b} start item unknown")))?;
        let end = *index.get(&bed.end_item_id).ok_or_else(|| invalid(format!("bed {b} end item unknown")))?;
        if start > end {
            return Err(invalid(format!("bed {b} start item comes after end item")));
        }
        ranges.push((start, end));
    }
    ranges.sort();
    if ranges.windows(2).any(|w| w[1].0 <= w[0].1) {
        return Err(invalid("bed ranges overlap"));
    }
    Ok(())
}

fn referenced_media(items: &[PlaylistItem], beds: &[Bed]) -> HashSet<String> {
    let mut set = HashSet::new();
    for item in items {
        set.extend(item.media_id.iter().map(|m| m.to_string()));
        set.extend(item.pages.iter().flatten().map(|m| m.to_string()));
    }
    for bed in beds {
        set.extend(bed.tracks.iter().map(|m| m.to_string()));
    }
    set
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(mut input): ApiJson<PlaylistInput>,
) -> ApiResult<(StatusCode, Json<Playlist>)> {
    s.require_team(&p, &input.team_id, Role::Editor).await?;
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    let defaults = input.defaults.take().unwrap_or_default();
    validate(&s, &input.team_id, &defaults, &mut input.items, &mut input.beds).await?;
    check_folder(&s, &input.team_id, input.folder_id.as_deref()).await?;
    let id = new_id();
    let mut tx = s.db.begin().await?;
    sqlx::query(
        "INSERT INTO playlists (id, team_id, folder_id, name, version, loop, defaults, items, beds, updated_at, updated_by)
         VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.team_id)
    .bind(&input.folder_id)
    .bind(input.name.trim())
    .bind(input.looping.unwrap_or(true))
    .bind(serde_json::to_string(&defaults)?)
    .bind(serde_json::to_string(&input.items)?)
    .bind(serde_json::to_string(&input.beds)?)
    .bind(now())
    .bind(&p.subject)
    .execute(&mut *tx)
    .await?;
    for m in referenced_media(&input.items, &input.beds) {
        sqlx::query("INSERT INTO playlist_media (playlist_id, media_id) VALUES (?, ?)")
            .bind(&id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

pub async fn update(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(mut input): ApiJson<PlaylistUpdate>,
) -> ApiResult<Json<Playlist>> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    let conflict = |cur: &Playlist| -> ApiError {
        let mut e = ApiError::conflict("version_conflict", "playlist was modified; base_version is stale");
        e.current = serde_json::to_value(cur).ok();
        e
    };
    if input.base_version != current.version {
        return Err(conflict(&current));
    }
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    validate(&s, &current.team_id, &input.defaults, &mut input.items, &mut input.beds).await?;

    let mut tx = s.db.begin().await?;
    let res = sqlx::query(
        "UPDATE playlists SET name = ?, loop = ?, defaults = ?, items = ?, beds = ?, version = version + 1,
            updated_at = ?, updated_by = ? WHERE id = ? AND version = ?",
    )
    .bind(input.name.trim())
    .bind(input.looping)
    .bind(serde_json::to_string(&input.defaults)?)
    .bind(serde_json::to_string(&input.items)?)
    .bind(serde_json::to_string(&input.beds)?)
    .bind(now())
    .bind(&p.subject)
    .bind(&id)
    .bind(input.base_version)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        drop(tx);
        return Err(conflict(&fetch(&s, &id).await?));
    }
    sqlx::query("DELETE FROM playlist_media WHERE playlist_id = ?").bind(&id).execute(&mut *tx).await?;
    for m in referenced_media(&input.items, &input.beds) {
        sqlx::query("INSERT INTO playlist_media (playlist_id, media_id) VALUES (?, ?)")
            .bind(&id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let updated = fetch(&s, &id).await?;
    s.notify_screens_for_playlist(&updated.id, updated.version).await;
    Ok(Json(updated))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    let w = World::load(&s).await?;
    let affected = w.playlist_screens(&id);
    sqlx::query(&format!("UPDATE screens SET {} WHERE override_playlist_id = ?", schedule::CLEAR_OVERRIDE))
        .bind(&id)
        .execute(&s.db)
        .await?;
    sqlx::query("DELETE FROM playlists WHERE id = ?").bind(&id).execute(&s.db).await?;
    schedule::refresh_clashes(&s, Some(&p)).await?;
    schedule::notify_screens(&s).await;
    schedule::broadcast_changed(&s, &current.team_id);
    for screen in affected {
        super::screens::broadcast_updated(&s, &screen).await;
    }
    Ok(StatusCode::NO_CONTENT)
}
