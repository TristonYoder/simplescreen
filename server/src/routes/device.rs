use std::collections::{HashMap, HashSet};

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::{
    auth::Principal,
    error::{ApiError, ApiJson, ApiResult},
    models::{ItemKind, Media, Playlist, ScreenRow, MEDIA_COLUMNS},
    schedule,
    state::AppState,
    util::sha256_hex,
};

async fn device_screen(s: &AppState, p: &Principal) -> ApiResult<ScreenRow> {
    match (&p.screen_id, p.is_device()) {
        (Some(id), true) => super::screens::fetch_row(s, id).await.map_err(|_| ApiError::unauthorized()),
        _ => Err(ApiError::unauthorized()),
    }
}

pub async fn playlists(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<super::playlists::Summary>>> {
    let screen = device_screen(&s, &p).await?;
    Ok(Json(super::playlists::summaries(&s, Some(&screen.group_id)).await?))
}

#[derive(Deserialize)]
pub struct Selection {
    playlist_id: Option<String>,
}

pub async fn select_playlist(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(selection): ApiJson<Selection>,
) -> ApiResult<StatusCode> {
    let screen = device_screen(&s, &p).await?;
    let base = schedule::base_timeline(&s, &screen, Utc::now()).await?;
    let scheduled = base.len() > 1 || base.first().is_some_and(|g| g.source != schedule::Source::Default);
    if scheduled {
        super::screens::set_override_row(&s, &screen, selection.playlist_id.as_deref(), schedule::next_change(&base)).await?;
    } else {
        super::screens::assign_playlist(&s, &screen, selection.playlist_id.as_deref()).await?;
        if screen.override_set {
            super::screens::clear_override_row(&s, &screen).await?;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn manifest(State(s): State<AppState>, p: Principal, headers: HeaderMap) -> ApiResult<Response> {
    let screen = device_screen(&s, &p).await?;
    let now = Utc::now();
    let timeline = schedule::screen_timeline(&s, &screen, now, now + schedule::DEVICE_HORIZON).await?;

    let mut playlists: Vec<Playlist> = vec![];
    for seg in &timeline {
        if let Some(id) = &seg.playlist_id {
            if !playlists.iter().any(|pl| &pl.id == id) {
                if let Ok(pl) = super::playlists::fetch(&s, id).await {
                    playlists.push(pl);
                }
            }
        }
    }
    let active = timeline.first().and_then(|g| g.playlist_id.as_ref()).and_then(|id| playlists.iter().find(|pl| &pl.id == id));

    let fingerprint = json!({
        "segments": timeline.iter().enumerate().map(|(i, g)| json!([
            g.playlist_id, g.source, g.entry_id, (i + 1 < timeline.len()).then_some(g.ends_at),
        ])).collect::<Vec<_>>(),
        "playlists": playlists.iter().map(|pl| json!([pl.id, pl.version])).collect::<Vec<_>>(),
        "name": screen.name,
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
        let all: Vec<Media> = sqlx::query_as(&format!("SELECT {MEDIA_COLUMNS} FROM media WHERE group_id = ?"))
            .bind(&screen.group_id)
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
