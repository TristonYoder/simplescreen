use std::collections::HashSet;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    library::{self, Access, Tree},
    models::{Media, MediaInput, MediaPatch, MEDIA_COLUMNS},
    reach::World,
    state::AppState,
    storage::{composite_checksum, sha_b64, Backend, S3_MAX_OBJECT},
    util::{is_sha256, new_id, now},
};

#[derive(Deserialize)]
pub struct ListQuery {
    folder_id: Option<String>,
    team_id: Option<String>,
    kind: Option<String>,
}

pub async fn list(State(s): State<AppState>, p: Principal, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Media>>> {
    super::assignments::require_user(&p)?;
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    if let Some(t) = &q.team_id {
        s.require_team(&p, t, Role::Viewer).await?;
    }
    if let Some(f) = &q.folder_id {
        tree.get(f)?;
        tree.require(&p, &w, f, Access::View)?;
    }
    let teams: Vec<String> = match &q.team_id {
        Some(t) => vec![t.clone()],
        None => p.memberships.iter().map(|m| m.team_id.clone()).collect(),
    };
    let used = library::used_by_teams(&s, &teams).await?;
    let rows: Vec<Media> = sqlx::query_as(&format!(
        "SELECT {MEDIA_COLUMNS} FROM media WHERE (?1 IS NULL OR folder_id = ?1) AND (?2 IS NULL OR kind = ?2)
         ORDER BY path COLLATE NOCASE"
    ))
    .bind(&q.folder_id)
    .bind(&q.kind)
    .fetch_all(&s.db)
    .await?;
    let mut out = vec![];
    for m in rows {
        let folder_access = match (&m.folder_id, &q.team_id) {
            (Some(f), Some(t)) if tree.team_reaches(&w, t, f) => {
                if p.role_in(t).is_some_and(|r| r >= Role::Editor) { Access::Edit } else { Access::View }
            }
            (Some(_), Some(_)) | (None, _) => Access::None,
            (Some(f), None) => tree.access(&p, &w, f),
        };
        let access = match folder_access {
            Access::Edit => "edit",
            Access::View => "view",
            Access::None if used.contains(&m.id) => "playlist",
            Access::None if q.team_id.is_none() && p.server_admin && q.folder_id.is_none() => "playlist",
            Access::None => continue,
        };
        out.push(m.with_access(access));
    }
    Ok(Json(out))
}

async fn visible(s: &AppState, p: &Principal, id: &str) -> ApiResult<(Media, &'static str)> {
    super::assignments::require_user(p)?;
    let m = library::fetch_media(s, id).await?;
    let (w, tree) = (World::load(s).await?, Tree::load(s).await?);
    let access = library::media_access(s, p, &w, &tree, &m).await?.ok_or_else(ApiError::not_found)?;
    Ok((m, access))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Media>> {
    let (m, access) = visible(&s, &p, &id).await?;
    Ok(Json(m.with_access(access)))
}

pub async fn usage(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    let (m, _) = visible(&s, &p, &id).await?;
    Ok(Json(library::usage(&s, &p, &[m.id]).await?))
}

async fn name_free(s: &AppState, folder_id: &str, name: &str, except: Option<&str>) -> ApiResult<()> {
    let taken: Option<(String,)> =
        sqlx::query_as("SELECT id FROM media WHERE folder_id = ? AND name = ? COLLATE NOCASE AND id != COALESCE(?, '')")
            .bind(folder_id)
            .bind(name)
            .bind(except)
            .fetch_optional(&s.db)
            .await?;
    match taken {
        Some(_) => Err(ApiError::conflict("name_taken", "the folder already has a file with that name")),
        None => Ok(()),
    }
}

fn check_sha(sha: &str) -> ApiResult<()> {
    if is_sha256(sha) {
        Ok(())
    } else {
        Err(ApiError::bad_request("sha256 must be 64 lowercase hex characters"))
    }
}

/// The folder (with Edit access) and the file's path in its library.
async fn target(s: &AppState, p: &Principal, folder_id: &str, name: &str) -> ApiResult<(library::LibraryRow, String, String)> {
    super::assignments::require_user(p)?;
    let name = library::check_name(name)?;
    let (w, tree) = (World::load(s).await?, Tree::load(s).await?);
    let folder = tree.get(folder_id)?;
    tree.require(p, &w, folder_id, Access::Edit)?;
    let lib = library::library(s, &folder.library_id).await?;
    let path = format!("{}/{name}", tree.path(folder_id));
    Ok((lib, name, path))
}

#[derive(Deserialize)]
pub struct UploadInput {
    folder_id: String,
    name: String,
    sha256: String,
    size: i64,
    #[allow(dead_code)]
    mime: String,
}

pub async fn upload(State(s): State<AppState>, p: Principal, ApiJson(input): ApiJson<UploadInput>) -> ApiResult<Json<Value>> {
    check_sha(&input.sha256)?;
    let (lib, name, path) = target(&s, &p, &input.folder_id, &input.name).await?;
    name_free(&s, &input.folder_id, &name, None).await?;
    if input.size < 0 {
        return Err(ApiError::bad_request("size must not be negative"));
    }
    let backend = lib.backend()?;
    let in_store = tokio::fs::metadata(s.blob_path(&input.sha256)).await.is_ok();
    if !(in_store && matches!(backend, Backend::Managed)) {
        library::ensure_space(&s, &lib, input.size).await?;
    }
    Ok(Json(match &backend {
        Backend::Managed | Backend::Local(_) => json!({
            "already_present": in_store, "method": "PUT",
            "url": format!("/v1/blobs/{}", input.sha256), "headers": {},
        }),
        Backend::S3(b) => {
            if input.size > S3_MAX_OBJECT {
                return Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "file_too_large", "S3 objects are limited to 5 TiB"));
            }
            let present = b.head(&path).await?.is_some_and(|(_, sha)| sha.as_deref() == Some(input.sha256.as_str()));
            if let Some((part_size, part_count)) = b.plan(input.size) {
                if present {
                    return Ok(Json(json!({ "already_present": true })));
                }
                let s3_upload_id = b.create_multipart(&path, &input.sha256, true).await?;
                let id = new_id();
                sqlx::query(
                    "INSERT INTO media_uploads (id, s3_upload_id, library_id, path, sha256, size, part_size, part_count, created_by, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(&s3_upload_id)
                .bind(&lib.id)
                .bind(&path)
                .bind(&input.sha256)
                .bind(input.size)
                .bind(part_size)
                .bind(part_count)
                .bind(&p.subject)
                .bind(now())
                .execute(&s.db)
                .await?;
                return Ok(Json(json!({
                    "already_present": false,
                    "multipart": { "upload_id": id, "part_size": part_size, "part_count": part_count },
                })));
            }
            let (url, headers) = b.put_url(&path, &input.sha256)?;
            let headers: serde_json::Map<String, Value> = headers.into_iter().map(|(k, v)| (k, json!(v))).collect();
            let expires = chrono::Utc::now() + chrono::Duration::hours(1);
            json!({
                "already_present": present, "method": "PUT", "url": url, "headers": headers,
                "expires_at": expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            })
        }
    }))
}

#[derive(sqlx::FromRow)]
struct UploadRow {
    id: String,
    s3_upload_id: String,
    library_id: String,
    path: String,
    size: i64,
    part_count: i64,
    created_by: String,
}

/// The caller's multipart upload and its S3 library.
async fn own_upload(s: &AppState, p: &Principal, id: &str) -> ApiResult<(UploadRow, Box<crate::storage::S3>)> {
    super::assignments::require_user(p)?;
    let row: UploadRow = sqlx::query_as(
        "SELECT id, s3_upload_id, library_id, path, size, part_count, created_by FROM media_uploads WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(&s.db)
    .await?
    .ok_or_else(ApiError::not_found)?;
    if row.created_by != p.subject {
        return Err(ApiError::forbidden());
    }
    match library::library(s, &row.library_id).await?.backend()? {
        Backend::S3(b) => Ok((row, b)),
        _ => Err(ApiError::internal("multipart upload outside an S3 library")),
    }
}

#[derive(Deserialize)]
pub struct PartSha {
    number: i64,
    sha256: String,
}

#[derive(Deserialize)]
pub struct PartsInput {
    parts: Vec<PartSha>,
}

pub async fn upload_parts(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<PartsInput>,
) -> ApiResult<Json<Value>> {
    let (row, b) = own_upload(&s, &p, &id).await?;
    if input.parts.is_empty() || input.parts.len() > 100 {
        return Err(ApiError::bad_request("sign between 1 and 100 parts at a time"));
    }
    let expires = (chrono::Utc::now() + chrono::Duration::from_std(crate::storage::PRESIGN_TTL).unwrap_or_default())
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut out = vec![];
    for part in &input.parts {
        check_sha(&part.sha256)?;
        if part.number < 1 || part.number > row.part_count {
            return Err(ApiError::bad_request(format!("part numbers run from 1 to {}", row.part_count)));
        }
        let (url, headers) = b.part_url(&row.path, &row.s3_upload_id, part.number as u16, &part.sha256)?;
        sqlx::query("INSERT INTO media_upload_parts (upload_id, number, sha256) VALUES (?, ?, ?) ON CONFLICT DO UPDATE SET sha256 = excluded.sha256")
            .bind(&row.id)
            .bind(part.number)
            .bind(&part.sha256)
            .execute(&s.db)
            .await?;
        let headers: serde_json::Map<String, Value> = headers.into_iter().map(|(k, v)| (k, json!(v))).collect();
        out.push(json!({ "number": part.number, "method": "PUT", "url": url, "headers": headers, "expires_at": expires }));
    }
    Ok(Json(Value::Array(out)))
}

#[derive(Deserialize)]
pub struct PartEtag {
    number: i64,
    etag: String,
}

#[derive(Deserialize)]
pub struct CompleteInput {
    parts: Vec<PartEtag>,
}

pub async fn complete_upload(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<CompleteInput>,
) -> ApiResult<StatusCode> {
    let (row, b) = own_upload(&s, &p, &id).await?;
    let signed: Vec<(i64, String)> = sqlx::query_as("SELECT number, sha256 FROM media_upload_parts WHERE upload_id = ? ORDER BY number")
        .bind(&row.id)
        .fetch_all(&s.db)
        .await?;
    let etags: std::collections::HashMap<i64, &str> = input.parts.iter().map(|p| (p.number, p.etag.as_str())).collect();
    let mut parts = vec![];
    let mut shas = vec![];
    for n in 1..=row.part_count {
        let sha = signed.iter().find(|(m, _)| *m == n).map(|(_, sha)| sha.clone());
        let (Some(sha), Some(etag)) = (sha, etags.get(&n)) else {
            return Err(ApiError::unprocessable("upload_incomplete", format!("part {n} was not signed or has no ETag")));
        };
        parts.push((n as u16, etag.to_string(), Some(sha_b64(&sha)?)));
        shas.push(sha);
    }
    b.complete_multipart(&row.path, &row.s3_upload_id, &parts).await?;
    let info = b.head_info(&row.path).await?.ok_or_else(|| ApiError::unprocessable("upload_incomplete", "the joined file is not in S3"))?;
    let composite = composite_checksum(&shas)?;
    let checksum_ok = info.checksum.as_deref().is_none_or(|c| c == composite);
    if info.size != row.size || !checksum_ok {
        b.delete(&row.path).await?;
        sqlx::query("DELETE FROM media_uploads WHERE id = ?").bind(&row.id).execute(&s.db).await?;
        return Err(ApiError::unprocessable("hash_mismatch", "the joined file does not match the declared size and part hashes"));
    }
    sqlx::query("DELETE FROM media_uploads WHERE id = ?").bind(&row.id).execute(&s.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn abort_upload(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let (row, b) = own_upload(&s, &p, &id).await?;
    b.abort_multipart(&row.path, &row.s3_upload_id).await?;
    sqlx::query("DELETE FROM media_uploads WHERE id = ?").bind(&row.id).execute(&s.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Aborts multipart uploads older than a day, hourly.
pub async fn sweep_uploads(s: AppState) {
    loop {
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let stale: Vec<(String,)> = sqlx::query_as("SELECT id FROM media_uploads WHERE created_at < ?")
            .bind(&cutoff)
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
        for (id,) in stale {
            let row: Option<UploadRow> = sqlx::query_as(
                "SELECT id, s3_upload_id, library_id, path, size, part_count, created_by FROM media_uploads WHERE id = ?",
            )
            .bind(&id)
            .fetch_optional(&s.db)
            .await
            .ok()
            .flatten();
            let Some(row) = row else { continue };
            if let Ok(lib) = library::library(&s, &row.library_id).await {
                if let Ok(Backend::S3(b)) = lib.backend() {
                    if let Err(e) = b.abort_multipart(&row.path, &row.s3_upload_id).await {
                        tracing::warn!("aborting stale upload {id}: {}", e.message);
                        continue;
                    }
                }
            }
            let _ = sqlx::query("DELETE FROM media_uploads WHERE id = ?").bind(&id).execute(&s.db).await;
        }
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<MediaInput>,
) -> ApiResult<(StatusCode, Json<Media>)> {
    check_sha(&input.sha256)?;
    if input.mime.trim().is_empty() {
        return Err(ApiError::bad_request("mime is required"));
    }
    let (lib, name, path) = target(&s, &p, &input.folder_id, &input.name).await?;
    name_free(&s, &input.folder_id, &name, None).await?;
    if let Some(t) = &input.thumbnail_sha256 {
        check_sha(t)?;
        if tokio::fs::metadata(s.blob_path(t)).await.is_err() {
            return Err(ApiError::unprocessable("blob_missing", "thumbnail blob has not been uploaded"));
        }
    }
    let staged = s.blob_path(&input.sha256);
    let backend = lib.backend()?;
    let size = match &backend {
        Backend::Managed | Backend::Local(_) => {
            let meta = tokio::fs::metadata(&staged)
                .await
                .map_err(|_| ApiError::unprocessable("blob_missing", "the file has not been uploaded"))?;
            if matches!(backend, Backend::Local(_)) {
                library::ensure_space(&s, &lib, meta.len() as i64).await?;
                backend.put_from(&s, &staged, &path, &input.sha256).await?;
            }
            meta.len() as i64
        }
        Backend::S3(b) => match b.head(&path).await? {
            None => return Err(ApiError::unprocessable("blob_missing", "the file has not been uploaded to S3")),
            Some((size, sha)) if sha.as_deref() == Some(input.sha256.as_str()) => size,
            Some(_) => return Err(ApiError::unprocessable("hash_mismatch", "the object in S3 does not match sha256")),
        },
    };
    let id = new_id();
    let res = sqlx::query(
        "INSERT INTO media (id, library_id, folder_id, name, path, kind, sha256, mime, width, height, duration_ms, codec, hdr,
            frame_rate, source_filename, thumbnail_sha256, size, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&lib.id)
    .bind(&input.folder_id)
    .bind(&name)
    .bind(&path)
    .bind(input.kind.as_str())
    .bind(&input.sha256)
    .bind(input.mime.trim())
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
    .await;
    if let Err(e) = res {
        return Err(match &e {
            sqlx::Error::Database(d) if d.is_unique_violation() => ApiError::conflict("name_taken", "the folder already has a file with that name"),
            _ => e.into(),
        });
    }
    if !matches!(backend, Backend::Managed) {
        library::gc_blob(&s, &input.sha256).await?;
    }
    library::changed(&s, Some(&input.folder_id));
    Ok((StatusCode::CREATED, Json(library::fetch_media(&s, &id).await?.with_access("edit"))))
}

async fn editable(s: &AppState, p: &Principal, id: &str) -> ApiResult<(Media, Tree, World)> {
    super::assignments::require_user(p)?;
    let m = library::fetch_media(s, id).await?;
    let (w, tree) = (World::load(s).await?, Tree::load(s).await?);
    match &m.folder_id {
        Some(f) => tree.require(p, &w, f, Access::Edit)?,
        None => {
            library::media_access(s, p, &w, &tree, &m).await?.ok_or_else(ApiError::not_found)?;
            return Err(ApiError::unprocessable("media_missing", "the file was deleted"));
        }
    }
    Ok((m, tree, w))
}

pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<MediaPatch>,
) -> ApiResult<Json<Media>> {
    let (m, tree, w) = editable(&s, &p, &id).await?;
    let old_folder = m.folder_id.clone().unwrap_or_default();
    let folder = input.folder_id.clone().unwrap_or_else(|| old_folder.clone());
    let name = match &input.name {
        Some(n) => library::check_name(n)?,
        None => m.name.clone(),
    };
    if folder == old_folder && name == m.name {
        return Ok(Json(m.with_access("edit")));
    }
    let dest = tree.get(&folder)?;
    tree.require(&p, &w, &folder, Access::Edit)?;
    name_free(&s, &folder, &name, Some(&m.id)).await?;
    let path = format!("{}/{name}", tree.path(&folder));
    let src_lib = library::library(&s, &m.library_id).await?;
    if dest.library_id == m.library_id {
        src_lib.backend()?.rename_file(&m.path, &path).await?;
    } else {
        let dst_lib = library::library(&s, &dest.library_id).await?;
        library::ensure_space(&s, &dst_lib, m.size).await?;
        let (src, dst) = (src_lib.backend()?, dst_lib.backend()?);
        let (file, temp) = src.fetch_local(&s, &m.path, &m.sha256).await?;
        let put = dst.put_from(&s, &file, &path, &m.sha256).await;
        if temp {
            let _ = tokio::fs::remove_file(&file).await;
        }
        put?;
        src.delete_file(&m.path).await?;
    }
    sqlx::query("UPDATE media SET library_id = ?, folder_id = ?, name = ?, path = ? WHERE id = ?")
        .bind(&dest.library_id)
        .bind(&folder)
        .bind(&name)
        .bind(&path)
        .bind(&m.id)
        .execute(&s.db)
        .await?;
    if m.library_id == "managed" && dest.library_id != "managed" {
        library::gc_blob(&s, &m.sha256).await?;
    }
    library::changed(&s, Some(&old_folder));
    if folder != old_folder {
        library::changed(&s, Some(&folder));
    }
    Ok(Json(library::fetch_media(&s, &id).await?.with_access("edit")))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let (m, _, _) = editable(&s, &p, &id).await?;
    library::delete_media(&s, &p, &m).await?;
    library::changed(&s, m.folder_id.as_deref());
    Ok(StatusCode::NO_CONTENT)
}

/// Media ids a team may add to its playlists: files in folders it reaches.
pub async fn usable_by_team(s: &AppState, team: &str) -> ApiResult<HashSet<String>> {
    let (w, tree) = (World::load(s).await?, Tree::load(s).await?);
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, folder_id FROM media WHERE folder_id IS NOT NULL").fetch_all(&s.db).await?;
    Ok(rows.into_iter().filter(|(_, f)| tree.team_reaches(&w, team, f)).map(|(id, _)| id).collect())
}
