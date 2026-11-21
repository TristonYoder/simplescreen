use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    auth::Principal,
    error::{ApiError, ApiJson, ApiResult},
    models::{ItemKind, Media, Output, Playlist, Screen, ScreenRow, MEDIA_COLUMNS, SCREEN_COLUMNS},
    notices::Actor,
    reach::World,
    schedule,
    state::AppState,
    util::{new_id, now, sha256_hex},
};

pub(super) fn device_node(p: &Principal) -> ApiResult<&str> {
    match (&p.node_id, p.is_device()) {
        (Some(id), true) => Ok(id),
        _ => Err(ApiError::unauthorized()),
    }
}

/// The screen a device call is about. An explicit `screen_id` must belong to the node (404 otherwise);
/// without one, the node's only screen is used, so single-screen devices need no changes.
async fn device_screen(s: &AppState, p: &Principal, explicit: Option<&str>) -> ApiResult<ScreenRow> {
    let node_id = device_node(p)?;
    if let Some(id) = explicit {
        let row = super::screens::fetch_row(s, id).await?;
        return if row.node_id.as_deref() == Some(node_id) { Ok(row) } else { Err(ApiError::not_found()) };
    }
    let mut rows: Vec<ScreenRow> =
        sqlx::query_as(&format!("SELECT {SCREEN_COLUMNS} FROM screens WHERE node_id = ? LIMIT 2")).bind(node_id).fetch_all(&s.db).await?;
    if rows.len() == 1 {
        Ok(rows.remove(0))
    } else {
        Err(ApiError::unprocessable("screen_id_required", "screen_id is required unless the node has exactly one screen"))
    }
}

/// Like `device_screen`, but a screen this node holds a control assignment on is accepted too
/// (remote playlist choice). Confidence-only targets are 403; unrelated screens are 404.
async fn controlled_screen(s: &AppState, p: &Principal, explicit: Option<&str>) -> ApiResult<ScreenRow> {
    let node_id = device_node(p)?;
    if let Some(id) = explicit {
        let row = super::screens::fetch_row(s, id).await?;
        if row.node_id.as_deref() == Some(node_id) || crate::assignments::targets(s, node_id, id, false).await? {
            return Ok(row);
        }
        return Err(if crate::assignments::targets(s, node_id, id, true).await? { ApiError::forbidden() } else { ApiError::not_found() });
    }
    device_screen(s, p, None).await
}

#[derive(Deserialize)]
pub struct ScreenQuery {
    screen_id: Option<String>,
}

#[derive(Serialize)]
pub struct PickerNode {
    kind: &'static str,
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<PickerNode>>,
}

/// Team → folder → playlist, holding only branches with a playlist available on the screen. Teams stay
/// at the top; a folder with a single child is replaced by it.
pub async fn picker(s: &AppState, w: &World, screen_id: &str) -> ApiResult<Vec<PickerNode>> {
    let available: HashSet<String> = w.available_on(screen_id).into_iter().collect();
    if available.is_empty() {
        return Ok(vec![]);
    }
    let playlists: Vec<(String, String, Option<String>, String)> =
        sqlx::query_as("SELECT id, team_id, folder_id, name FROM playlists ORDER BY name, id").fetch_all(&s.db).await?;
    let playlists: Vec<_> = playlists.into_iter().filter(|p| available.contains(&p.0)).collect();
    let folders: Vec<(String, String, Option<String>, String)> =
        sqlx::query_as("SELECT id, team_id, parent_id, name FROM playlist_folders ORDER BY position, name, id").fetch_all(&s.db).await?;

    fn level(
        team: &str,
        parent: Option<&str>,
        folders: &[(String, String, Option<String>, String)],
        playlists: &[(String, String, Option<String>, String)],
        depth: usize,
    ) -> Vec<PickerNode> {
        let mut out = vec![];
        if depth < 32 {
            for f in folders.iter().filter(|f| f.1 == team && f.2.as_deref() == parent) {
                let mut children = level(team, Some(&f.0), folders, playlists, depth + 1);
                match children.len() {
                    0 => {}
                    1 => out.push(children.remove(0)),
                    _ => out.push(PickerNode { kind: "folder", id: f.0.clone(), name: f.3.clone(), children: Some(children) }),
                }
            }
        }
        for p in playlists.iter().filter(|p| p.1 == team && p.2.as_deref() == parent) {
            out.push(PickerNode { kind: "playlist", id: p.0.clone(), name: p.3.clone(), children: None });
        }
        out
    }

    let mut teams: Vec<(&String, &String)> =
        w.teams.iter().filter(|(id, _)| playlists.iter().any(|p| &p.1 == *id)).map(|(id, t)| (id, &t.name)).collect();
    teams.sort_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0)));
    Ok(teams
        .into_iter()
        .map(|(id, name)| {
            // A playlist whose folder is gone or belongs elsewhere sits at the team root.
            let fixed: Vec<_> = playlists
                .iter()
                .filter(|p| &p.1 == id)
                .map(|p| {
                    let ok = p.2.as_ref().is_some_and(|f| folders.iter().any(|x| &x.0 == f && &x.1 == id));
                    (p.0.clone(), p.1.clone(), if ok { p.2.clone() } else { None }, p.3.clone())
                })
                .collect();
            PickerNode { kind: "team", id: id.clone(), name: name.clone(), children: Some(level(id, None, &folders, &fixed, 0)) }
        })
        .collect())
}

pub async fn playlists(
    State(s): State<AppState>,
    p: Principal,
    Query(q): Query<ScreenQuery>,
) -> ApiResult<Json<Vec<PickerNode>>> {
    let screen = controlled_screen(&s, &p, q.screen_id.as_deref()).await?;
    let w = World::load(&s).await?;
    Ok(Json(picker(&s, &w, &screen.id).await?))
}

#[derive(Deserialize)]
pub struct Selection {
    screen_id: Option<String>,
    playlist_id: Option<String>,
}

pub async fn select_playlist(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(selection): ApiJson<Selection>,
) -> ApiResult<StatusCode> {
    let screen = controlled_screen(&s, &p, selection.screen_id.as_deref()).await?;
    let w = World::load(&s).await?;
    if selection.playlist_id.as_deref().is_some_and(|pl| !w.available(pl, &screen.id)) {
        return Err(ApiError::unprocessable("playlist_unavailable", "playlist is not available on this screen"));
    }
    let base = schedule::base_timeline(&s, &screen, Utc::now()).await?;
    let scheduled = base.len() > 1 || base.first().is_some_and(|g| g.source != schedule::Source::Default);
    if scheduled {
        super::screens::set_override_row(&s, &w, &p, &screen, selection.playlist_id.as_deref(), schedule::next_change(&base)).await?;
    } else {
        sqlx::query("UPDATE screens SET fallback_playlist_id = ? WHERE id = ?").bind(&selection.playlist_id).bind(&screen.id).execute(&s.db).await?;
        crate::notices::log(&s, &screen.id, "fallback", &Actor::of(&p, &w, &screen.id), json!({ "playlist_id": selection.playlist_id })).await;
        if screen.override_set {
            super::screens::clear_override_row(&s, &w, &p, &screen).await?;
        } else {
            super::screens::changed(&s, &screen, 0).await;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn manifest(
    State(s): State<AppState>,
    p: Principal,
    Query(q): Query<ScreenQuery>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    // Read-only access also covers screens this node watches through control or confidence assignments.
    let screen = match q.screen_id.as_deref() {
        Some(id) if crate::assignments::targets(&s, device_node(&p)?, id, true).await? => super::screens::fetch_row(&s, id).await?,
        other => device_screen(&s, &p, other).await?,
    };
    let now = Utc::now();
    let w = World::load(&s).await?;
    let entries = schedule::all_entries(&s).await?;
    let org = s.org_tz();
    let timeline = schedule::screen_segments(&w, &screen, &entries, &org, true, now, now + schedule::DEVICE_HORIZON);
    let available = w.available_on(&screen.id);

    let mut playlists: Vec<Playlist> = vec![];
    for id in &available {
        if let Ok(pl) = super::playlists::fetch(&s, id).await {
            playlists.push(pl);
        }
    }
    // Download order: the active playlist, later timeline playlists by start, then the rest by name.
    let mut order: Vec<String> = vec![];
    for seg in &timeline {
        if let Some(id) = &seg.playlist_id {
            if !order.contains(id) {
                order.push(id.clone());
            }
        }
    }
    let mut rest: Vec<&Playlist> = playlists.iter().filter(|pl| !order.contains(&pl.id)).collect();
    rest.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    order.extend(rest.into_iter().map(|pl| pl.id.clone()));
    playlists.sort_by_key(|pl| order.iter().position(|id| *id == pl.id).unwrap_or(usize::MAX));
    let active = timeline.first().and_then(|g| g.playlist_id.as_ref()).and_then(|id| playlists.iter().find(|pl| &pl.id == id));

    let mut tag_ids: Vec<String> = w.screen_tags.get(&screen.id).map(|t| t.iter().cloned().collect()).unwrap_or_default();
    tag_ids.sort();
    let tags: Vec<serde_json::Value> = {
        let rows: Vec<(String, String, Option<String>, String)> =
            sqlx::query_as("SELECT id, name, color, created_at FROM tags ORDER BY name").fetch_all(&s.db).await?;
        rows.into_iter()
            .filter(|t| tag_ids.contains(&t.0))
            .map(|(id, name, color, created_at)| {
                let mut v = json!({ "id": id, "name": name, "created_at": created_at });
                if let Some(c) = color {
                    v["color"] = json!(c);
                }
                v
            })
            .collect()
    };
    let picker = picker(&s, &w, &screen.id).await?;
    let tz = schedule::screen_tz(&screen, &org);

    let org_name = s.org_name();
    let fingerprint = json!({
        "segments": timeline.iter().enumerate().map(|(i, g)| json!([
            g.playlist_id, g.source, g.display, g.entry_id, (i + 1 < timeline.len()).then_some(g.ends_at),
        ])).collect::<Vec<_>>(),
        "playlists": playlists.iter().map(|pl| json!([pl.id, pl.version])).collect::<Vec<_>>(),
        "name": screen.name,
        "org": org_name,
        "tags": tags,
        "picker": picker,
        "tz": tz.name(),
    });
    let etag = format!("\"{}\"", &sha256_hex(fingerprint.to_string().as_bytes())[..20]);
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim().trim_start_matches("W/") == etag));
    let etag_value = HeaderValue::from_str(&etag).map_err(ApiError::internal)?;
    if not_modified {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag_value)]).into_response());
    }

    let mut media: Vec<Media> = vec![];
    let mut assets = vec![];
    if !playlists.is_empty() {
        let all: Vec<Media> = sqlx::query_as(&format!(
            "SELECT {MEDIA_COLUMNS} FROM media WHERE id IN (SELECT media_id FROM playlist_media WHERE playlist_id IN \
             (SELECT value FROM json_each(?)))"
        ))
        .bind(serde_json::to_string(&available)?)
        .fetch_all(&s.db)
        .await?;
        let by_id: HashMap<String, Media> = all.into_iter().map(|m| (m.id.clone(), m)).collect();
        let mut seen_media: HashSet<String> = HashSet::new();
        let mut seen_blob: HashSet<String> = HashSet::new();
        let mut add = |id: &str, index: usize, media: &mut Vec<Media>, assets: &mut Vec<serde_json::Value>| {
            let Some(m) = by_id.get(id) else { return };
            if seen_media.insert(m.id.clone()) {
                media.push(m.clone());
            }
            if seen_blob.insert(m.sha256.clone()) {
                assets.push(json!({
                    "sha256": m.sha256,
                    "size": m.size,
                    "mime": m.mime,
                    "path": format!("/v1/blobs/{}", m.sha256),
                    "first_use_index": index,
                }));
            }
        };
        for pl in &playlists {
            for (index, item) in pl.items.iter().enumerate() {
                match item.kind {
                    ItemKind::Video | ItemKind::Image => {
                        if let Some(m) = &item.media_id {
                            add(&m.to_string(), index, &mut media, &mut assets);
                        }
                    }
                    ItemKind::Sequence => {
                        for m in item.pages.iter().flatten() {
                            add(&m.to_string(), index, &mut media, &mut assets);
                        }
                    }
                }
                for bed in pl.beds.iter().filter(|b| b.start_item_id == item.id.unwrap_or_default()) {
                    for t in &bed.tracks {
                        add(&t.to_string(), index, &mut media, &mut assets);
                    }
                }
            }
        }
    }

    let mut body = json!({
        "screen_id": screen.id,
        "screen_name": screen.name,
        "organization_id": s.org_id(),
        "organization_name": org_name,
        "timezone": tz.name(),
        "tags": tags,
        "available_playlist_ids": available,
        "picker": picker,
        "timeline": timeline,
        "playlists": playlists,
        "media": media,
        "assets": assets,
    });
    if let Some(pl) = active {
        body["playlist"] = serde_json::to_value(pl)?;
    }
    Ok(([(header::ETAG, etag_value)], Json(body)).into_response())
}

// MARK: Node

pub async fn node(State(s): State<AppState>, p: Principal) -> ApiResult<Json<serde_json::Value>> {
    let node_id = device_node(&p)?;
    let row = super::nodes::fetch_row(&s, node_id).await.map_err(|_| ApiError::unauthorized())?;
    let rows: Vec<ScreenRow> =
        sqlx::query_as(&format!("SELECT {SCREEN_COLUMNS} FROM screens WHERE node_id = ? ORDER BY name, id")).bind(node_id).fetch_all(&s.db).await?;
    let w = World::load(&s).await?;
    let screens: Vec<Screen> = super::screens::views(&s, &w, rows).await?;
    Ok(Json(json!({ "node": super::nodes::to_node(&s, row).await?, "screens": screens })))
}

pub async fn put_outputs(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(outputs): ApiJson<Vec<Output>>,
) -> ApiResult<Json<crate::models::Node>> {
    let node_id = device_node(&p)?;
    let mut keys = HashSet::new();
    for o in &outputs {
        if o.key.trim().is_empty() || o.name.trim().is_empty() {
            return Err(ApiError::unprocessable("invalid_outputs", "output key and name must not be empty"));
        }
        if !keys.insert(o.key.as_str()) {
            return Err(ApiError::unprocessable("invalid_outputs", format!("duplicate output key {}", o.key)));
        }
    }
    // role and screen_id are copies of the assignments. A hardware report cannot change them.
    let node = super::nodes::fetch_row(&s, node_id).await.map_err(|_| ApiError::unauthorized())?;
    let set = crate::assignments::load(&s, node_id).await?;
    let mut stored = outputs;
    crate::assignments::overlay_outputs(&mut stored, &set);
    let res = sqlx::query("UPDATE nodes SET outputs = ? WHERE id = ?")
        .bind(serde_json::to_string(&stored)?)
        .bind(&node.id)
        .execute(&s.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::unauthorized());
    }
    s.hub.broadcast_all(&json!({ "type": "node_updated", "node_id": node_id }));
    Ok(Json(super::nodes::to_node(&s, super::nodes::fetch_row(&s, node_id).await?).await?))
}

#[derive(Deserialize)]
pub struct NewScreen {
    output_key: String,
    name: String,
}

/// A node creates a new untagged screen on one of its outputs (as its program output). It grants
/// nothing until an admin tags it.
pub async fn create_screen(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<NewScreen>,
) -> ApiResult<(StatusCode, Json<Screen>)> {
    let node_id = device_node(&p)?.to_string();
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name must not be empty"));
    }
    let node = super::nodes::fetch_row(&s, &node_id).await.map_err(|_| ApiError::unauthorized())?;
    if crate::assignments::Mode::parse(&node.mode) != crate::assignments::Mode::Player {
        return Err(ApiError::unprocessable("job_not_allowed_in_mode", "only player devices own screens"));
    }
    let mut outputs = super::nodes::outputs_of(&node);
    let Some(output) = outputs.iter_mut().find(|o| o.key == input.output_key) else {
        return Err(ApiError::unprocessable("unknown_output", "output_key is not in the node's reported outputs"));
    };
    let conflict = || ApiError::conflict("output_has_screen", "that output already has a screen");

    let id = new_id();
    output.screen_id = Some(id.clone());
    output.role = crate::models::OutputRole::Program;
    let mut tx = s.db.begin_with("BEGIN IMMEDIATE").await?;
    let taken: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM assignments WHERE node_id = ? AND output_key = ? AND job = 'program' AND screen_id IS NOT NULL",
    )
    .bind(&node_id)
    .bind(&input.output_key)
    .fetch_optional(&mut *tx)
    .await?;
    if taken.is_some() {
        return Err(conflict());
    }
    let stamp = now();
    sqlx::query("INSERT INTO screens (id, name, node_id, created_at) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(name)
        .bind(&node_id)
        .bind(&stamp)
        .execute(&mut *tx)
        .await?;
    // The program job for this output; any settings already stored for it are kept.
    sqlx::query(
        "INSERT INTO assignments (node_id, output_key, job, screen_id, updated_by, updated_at) VALUES (?1, ?2, 'program', ?3, ?1, ?4) \
         ON CONFLICT (node_id, output_key) DO UPDATE SET job = 'program', screen_id = ?3, updated_by = ?1, updated_at = ?4",
    )
    .bind(&node_id)
    .bind(&input.output_key)
    .bind(&id)
    .bind(&stamp)
    .execute(&mut *tx)
    .await?;
    let version: i64 = sqlx::query_scalar(
        "UPDATE nodes SET outputs = ?, assignments_version = assignments_version + 1, \
         applied_assignments_version = assignments_version + 1 WHERE id = ? RETURNING assignments_version",
    )
    .bind(serde_json::to_string(&outputs)?)
    .bind(&node_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    s.hub.send_node(&node_id, &json!({ "type": "assignments_changed", "version": version }));

    let row = super::screens::fetch_row(&s, &id).await?;
    let actor = crate::notices::Actor { sub: node_id.clone(), name: Some(node.name.clone()), team_id: None };
    crate::notices::log(&s, &id, "created", &actor, json!({ "name": name, "node_id": node_id })).await;
    crate::notices::sync_untagged(&s, &id).await?;
    super::screens::broadcast_updated(&s, &id).await;
    s.hub.broadcast_all(&json!({ "type": "node_updated", "node_id": node_id }));
    if s.hub.is_online(&node_id) {
        s.broadcast_screen(&id, &json!({ "type": "screen_online", "screen_id": id })).await;
    }
    Ok((StatusCode::CREATED, Json(super::screens::to_screen(&s, row).await?)))
}
