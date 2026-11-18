use std::io::SeekFrom;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio_util::io::ReaderStream;

use crate::{
    auth::Principal,
    error::{ApiError, ApiResult},
    state::AppState,
    util::{is_sha256, new_id},
};

const READ_BUF: usize = 256 * 1024;
const CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

pub async fn put(
    State(s): State<AppState>,
    p: Principal,
    Path(sha): Path<String>,
    body: Body,
) -> ApiResult<Response> {
    if !p.is_editor_anywhere() {
        return Err(ApiError::forbidden());
    }
    if !is_sha256(&sha) {
        return Err(ApiError::bad_request("sha256 must be 64 lowercase hex characters"));
    }
    let dest = s.blob_path(&sha);
    if let Ok(meta) = tokio::fs::metadata(&dest).await {
        return Ok((StatusCode::OK, Json(json!({ "sha256": sha, "size": meta.len() }))).into_response());
    }

    let tmp = s.tmp_dir().join(new_id());
    let result = write_stream(&tmp, body).await;
    let (hash, size) = match result {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
    };
    if hash != sha {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(ApiError::unprocessable("hash_mismatch", "uploaded bytes do not match the sha256 in the path"));
    }
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    if let Err(e) = tokio::fs::rename(&tmp, &dest).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok((StatusCode::CREATED, Json(json!({ "sha256": sha, "size": size }))).into_response())
}

async fn write_stream(tmp: &std::path::Path, body: Body) -> ApiResult<(String, u64)> {
    let file = tokio::fs::File::create(tmp).await?;
    let mut out = BufWriter::with_capacity(1024 * 1024, file);
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::bad_request(format!("upload interrupted: {e}")))?;
        hasher.update(&chunk);
        size += chunk.len() as u64;
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    out.get_ref().sync_all().await?;
    Ok((hex::encode(hasher.finalize()), size))
}

enum RangeSpec {
    Full,
    Part(u64, u64),
    Unsatisfiable,
}

fn parse_range(header: Option<&str>, len: u64) -> RangeSpec {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return RangeSpec::Full;
    };
    if spec.contains(',') {
        return RangeSpec::Full;
    }
    let Some((a, b)) = spec.split_once('-') else { return RangeSpec::Full };
    let (a, b) = (a.trim(), b.trim());
    if a.is_empty() {
        let Ok(n) = b.parse::<u64>() else { return RangeSpec::Full };
        if n == 0 || len == 0 {
            return RangeSpec::Unsatisfiable;
        }
        return RangeSpec::Part(len.saturating_sub(n), len - 1);
    }
    let Ok(start) = a.parse::<u64>() else { return RangeSpec::Full };
    let end = if b.is_empty() {
        len.saturating_sub(1)
    } else {
        match b.parse::<u64>() {
            Ok(e) => e.min(len.saturating_sub(1)),
            Err(_) => return RangeSpec::Full,
        }
    };
    if start >= len || start > end {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Part(start, end)
}

pub async fn get(
    State(s): State<AppState>,
    _p: Principal,
    method: Method,
    Path(sha): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    if !is_sha256(&sha) {
        return Err(ApiError::not_found());
    }
    let mut file = tokio::fs::File::open(s.blob_path(&sha)).await.map_err(|_| ApiError::not_found())?;
    let len = file.metadata().await?.len();
    let etag = format!("\"{sha}\"");

    let mut resp = Response::builder()
        .header(header::ETAG, &etag)
        .header(header::CACHE_CONTROL, CACHE_CONTROL)
        .header(header::ACCEPT_RANGES, "bytes");

    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == "*" || t.trim().trim_start_matches("W/") == etag));
    if matches {
        return Ok(resp.status(StatusCode::NOT_MODIFIED).body(Body::empty()).unwrap());
    }

    let mime: Option<(String,)> = sqlx::query_as("SELECT mime FROM media WHERE sha256 = ? LIMIT 1")
        .bind(&sha)
        .fetch_optional(&s.db)
        .await?;
    let content_type = mime
        .and_then(|(m,)| HeaderValue::from_str(&m).ok())
        .unwrap_or(HeaderValue::from_static("application/octet-stream"));
    resp = resp.header(header::CONTENT_TYPE, content_type);

    let range = parse_range(headers.get(header::RANGE).and_then(|v| v.to_str().ok()), len);
    let (status, start, count) = match range {
        RangeSpec::Full => (StatusCode::OK, 0, len),
        RangeSpec::Part(a, b) => {
            resp = resp.header(header::CONTENT_RANGE, format!("bytes {a}-{b}/{len}"));
            (StatusCode::PARTIAL_CONTENT, a, b - a + 1)
        }
        RangeSpec::Unsatisfiable => {
            return Ok(resp
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .unwrap());
        }
    };
    resp = resp.status(status).header(header::CONTENT_LENGTH, count);
    if method == Method::HEAD {
        return Ok(resp.body(Body::empty()).unwrap());
    }
    if start > 0 {
        file.seek(SeekFrom::Start(start)).await?;
    }
    let stream = ReaderStream::with_capacity(file.take(count), READ_BUF);
    Ok(resp.body(Body::from_stream(stream)).unwrap())
}
