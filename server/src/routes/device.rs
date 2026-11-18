use std::collections::{HashMap, HashSet};

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::{
    auth::Principal,
    error::{ApiError, ApiResult},
    models::{ItemKind, Media, MEDIA_COLUMNS},
    state::AppState,
};

pub async fn manifest(State(s): State<AppState>, p: Principal, headers: HeaderMap) -> ApiResult<Response> {
    let screen_id = match (&p.screen_id, p.is_device()) {
        (Some(id), true) => id.clone(),
        _ => return Err(ApiError::unauthorized()),
    };
    let screen = super::screens::fetch_row(&s, &screen_id).await.map_err(|_| ApiError::unauthorized())?;

    let playlist = match &screen.playlist_id {
        Some(id) => Some(super::playlists::fetch(&s, id).await?),
        None => None,
    };
    let etag = match &playlist {
        Some(pl) => format!("\"{}\"", pl.version),
        None => "\"none\"".to_string(),
    };
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
    if let Some(pl) = &playlist {
        let all: Vec<Media> = sqlx::query_as(&format!("SELECT {MEDIA_COLUMNS} FROM media WHERE group_id = ?"))
            .bind(&pl.group_id)
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

    let mut body = json!({
        "screen_id": screen.id,
        "screen_name": screen.name,
        "media": media,
        "assets": assets,
    });
    if let Some(pl) = &playlist {
        body["playlist"] = serde_json::to_value(pl)?;
    }
    Ok(([(header::ETAG, etag_value)], Json(body)).into_response())
}
