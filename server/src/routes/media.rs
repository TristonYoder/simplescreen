use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    models::{Media, MediaInput, MediaPatch, MEDIA_COLUMNS},
    state::AppState,
    util::{is_sha256, new_id, now},
};

#[derive(Deserialize)]
pub struct ListQuery {
    group_id: Option<String>,
    kind: Option<String>,
}

pub async fn list(State(s): State<AppState>, p: Principal, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Media>>> {
    let rows: Vec<Media> = sqlx::query_as(&format!(
        "SELECT {MEDIA_COLUMNS} FROM media WHERE (?1 IS NULL OR group_id = ?1) AND (?2 IS NULL OR kind = ?2)
         ORDER BY created_at DESC"
    ))
    .bind(&q.group_id)
    .bind(&q.kind)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows.into_iter().filter(|m| p.can_view(&m.group_id)).collect()))
}

pub async fn fetch(s: &AppState, id: &str) -> ApiResult<Media> {
    sqlx::query_as(&format!("SELECT {MEDIA_COLUMNS} FROM media WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(ApiError::not_found)
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Media>> {
    let m = fetch(&s, &id).await?;
    p.require(&m.group_id, Role::Viewer)?;
    Ok(Json(m))
}

async fn blob_size(s: &AppState, sha: &str) -> ApiResult<Option<i64>> {
    if !is_sha256(sha) {
        return Err(ApiError::bad_request("sha256 must be 64 lowercase hex characters"));
    }
    Ok(tokio::fs::metadata(s.blob_path(sha)).await.ok().map(|m| m.len() as i64))
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<MediaInput>,
) -> ApiResult<(StatusCode, Json<Media>)> {
    s.require_group(&p, &input.group_id, Role::Editor).await?;
    if input.title.trim().is_empty() || input.mime.trim().is_empty() {
        return Err(ApiError::bad_request("title and mime are required"));
    }
    let size = blob_size(&s, &input.sha256)
        .await?
        .ok_or_else(|| ApiError::unprocessable("blob_missing", "blob has not been uploaded"))?;
    if let Some(t) = &input.thumbnail_sha256 {
        if blob_size(&s, t).await?.is_none() {
            return Err(ApiError::unprocessable("blob_missing", "thumbnail blob has not been uploaded"));
        }
    }
    let id = new_id();
    sqlx::query(
        "INSERT INTO media (id, group_id, kind, sha256, mime, title, width, height, duration_ms, codec, hdr,
            frame_rate, source_filename, thumbnail_sha256, size, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.group_id)
    .bind(input.kind.as_str())
    .bind(&input.sha256)
    .bind(&input.mime)
    .bind(&input.title)
    .bind(input.width)
    .bind(input.height)
    .bind(input.duration_ms)
    .bind(&input.codec)
    .bind(input.hdr)
    .bind(input.frame_rate)
    .bind(&input.source_filename)
    .bind(&input.thumbnail_sha256)
    .bind(size)
    .bind(&p.subject)
    .bind(now())
    .execute(&s.db)
    .await?;
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(patch): ApiJson<MediaPatch>,
) -> ApiResult<Json<Media>> {
    let m = fetch(&s, &id).await?;
    p.require(&m.group_id, Role::Editor)?;
    if let Some(title) = patch.title {
        if title.trim().is_empty() {
            return Err(ApiError::bad_request("title must not be empty"));
        }
        sqlx::query("UPDATE media SET title = ? WHERE id = ?").bind(title).bind(&id).execute(&s.db).await?;
    }
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let m = fetch(&s, &id).await?;
    p.require(&m.group_id, Role::Editor)?;
    let (refs,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM playlist_media WHERE media_id = ?").bind(&id).fetch_one(&s.db).await?;
    if refs > 0 {
        return Err(ApiError::conflict("media_in_use", "media is referenced by a playlist"));
    }
    sqlx::query("DELETE FROM media WHERE id = ?").bind(&id).execute(&s.db).await?;
    for sha in [Some(m.sha256), m.thumbnail_sha256].into_iter().flatten() {
        let (n,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM media WHERE sha256 = ?1 OR thumbnail_sha256 = ?1")
                .bind(&sha)
                .fetch_one(&s.db)
                .await?;
        if n == 0 {
            let _ = tokio::fs::remove_file(s.blob_path(&sha)).await;
        }
    }
    Ok(StatusCode::NO_CONTENT)
}
