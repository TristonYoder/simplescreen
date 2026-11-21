use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    models::{
        Bed, ItemKind, Playlist, PlaylistDefaults, PlaylistInput, PlaylistItem, PlaylistRow, PlaylistUpdate,
        PLAYLIST_COLUMNS,
    },
    state::AppState,
    util::{new_id, now},
};

#[derive(Deserialize)]
pub struct ListQuery {
    team_id: Option<String>,
}

#[derive(Serialize)]
pub struct Summary {
    id: String,
    team_id: String,
    name: String,
    version: i64,
    item_count: i64,
    total_bytes: i64,
    updated_at: String,
}

pub async fn list(
    State(s): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<Summary>>> {
    let all = summaries(&s, q.team_id.as_deref()).await?;
    Ok(Json(all.into_iter().filter(|r| p.can_view(&r.team_id)).collect()))
}

pub async fn summaries(s: &AppState, team_id: Option<&str>) -> ApiResult<Vec<Summary>> {
    let rows: Vec<(String, String, String, i64, i64, i64, String)> = sqlx::query_as(
        "SELECT p.id, p.team_id, p.name, p.version, json_array_length(p.items),
                COALESCE((SELECT SUM(m.size) FROM playlist_media pm JOIN media m ON m.id = pm.media_id
                          WHERE pm.playlist_id = p.id), 0),
                p.updated_at
         FROM playlists p WHERE (?1 IS NULL OR p.team_id = ?1) ORDER BY p.name",
    )
    .bind(team_id)
    .fetch_all(&s.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, team_id, name, version, item_count, total_bytes, updated_at)| Summary {
            id,
            team_id,
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
    Ok(row.ok_or_else(ApiError::not_found)?.into_playlist()?)
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Playlist>> {
    let pl = fetch(&s, &id).await?;
    p.require(&pl.team_id, Role::Viewer)?;
    Ok(Json(pl))
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
    let id = new_id();
    let mut tx = s.db.begin().await?;
    sqlx::query(
        "INSERT INTO playlists (id, team_id, name, version, loop, defaults, items, beds, updated_at, updated_by)
         VALUES (?, ?, ?, 1, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.team_id)
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
    s.notify_screens_for_playlist(&updated.team_id, updated.version).await;
    Ok(Json(updated))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    sqlx::query(
        "UPDATE screens SET override_set = 0, override_playlist_id = NULL, override_until = NULL, override_set_at = NULL \
         WHERE override_playlist_id = ?",
    )
    .bind(&id)
    .execute(&s.db)
    .await?;
    sqlx::query("DELETE FROM playlists WHERE id = ?").bind(&id).execute(&s.db).await?;
    crate::schedule::notify_team(&s, &current.team_id).await;
    crate::schedule::broadcast_changed(&s, &current.team_id);
    Ok(StatusCode::NO_CONTENT)
}
