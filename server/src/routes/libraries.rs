use std::path::{Component, Path as FsPath, PathBuf};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::Principal,
    error::{ApiError, ApiJson, ApiResult},
    library::{self, LibraryRow, LIBRARY_COLUMNS},
    models::double_option,
    state::AppState,
    storage::{Backend, S3Settings, S3},
    util::{new_id, now},
};

async fn to_json(s: &AppState, p: &Principal, lib: &LibraryRow) -> ApiResult<Value> {
    let mut v = json!({
        "id": lib.id, "name": lib.name, "kind": lib.kind, "position": lib.position,
        "used_bytes": library::used_bytes(s, &lib.id).await?, "created_at": lib.created_at,
    });
    if let Some(free) = library::free_bytes(s, lib).await? {
        v["free_bytes"] = json!(free);
    }
    if let Some(q) = lib.quota_bytes {
        v["quota_bytes"] = json!(q);
    }
    if p.server_admin {
        if let Some(path) = &lib.path {
            v["path"] = json!(path);
        }
        if let Some(cfg) = lib.s3_settings() {
            v["s3"] = serde_json::to_value(cfg)?;
        }
    }
    Ok(v)
}

pub async fn list(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<Value>>> {
    super::assignments::require_user(&p)?;
    let rows: Vec<LibraryRow> =
        sqlx::query_as(&format!("SELECT {LIBRARY_COLUMNS} FROM libraries ORDER BY position, name COLLATE NOCASE"))
            .fetch_all(&s.db)
            .await?;
    let mut out = vec![];
    for r in &rows {
        out.push(to_json(&s, &p, r).await?);
    }
    Ok(Json(out))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    super::assignments::require_user(&p)?;
    Ok(Json(to_json(&s, &p, &library::library(&s, &id).await?).await?))
}

#[derive(Deserialize, Default, Clone)]
pub struct S3Input {
    endpoint: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    prefix: Option<String>,
    path_style: Option<bool>,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    cdn_base_url: Option<Option<String>>,
    part_size_mib: Option<i64>,
}

impl S3Input {
    /// Applies these fields over `base`, returning the settings and the secret.
    fn merge(&self, base: Option<S3Settings>, secret: Option<String>) -> ApiResult<(S3Settings, String)> {
        let mut cfg = base.unwrap_or_default();
        if let Some(v) = &self.endpoint {
            cfg.endpoint = Some(v.trim().to_string()).filter(|v| !v.is_empty());
        }
        if let Some(v) = &self.region {
            cfg.region = v.trim().to_string();
        }
        if let Some(v) = &self.bucket {
            cfg.bucket = v.trim().to_string();
        }
        if let Some(v) = &self.prefix {
            cfg.prefix = v.trim().trim_matches('/').to_string();
        }
        if let Some(v) = self.path_style {
            cfg.path_style = v;
        }
        if let Some(v) = &self.access_key_id {
            cfg.access_key_id = v.trim().to_string();
        }
        if let Some(v) = &self.cdn_base_url {
            cfg.cdn_base_url = v.as_ref().map(|c| c.trim().trim_end_matches('/').to_string()).filter(|c| !c.is_empty());
        }
        if let Some(v) = self.part_size_mib {
            if !(5..=5120).contains(&v) {
                return Err(ApiError::bad_request("part_size_mib must be between 5 and 5120").because("part_size_invalid"));
            }
            cfg.part_size_mib = Some(v);
        }
        let secret = self.secret_access_key.clone().or(secret).unwrap_or_default();
        if cfg.bucket.is_empty() || cfg.region.is_empty() || cfg.access_key_id.is_empty() || secret.is_empty() {
            return Err(ApiError::bad_request("s3 needs bucket, region, access_key_id and secret_access_key").because("s3_settings_incomplete"));
        }
        Ok((cfg, secret))
    }
}

/// An absolute, normalized path inside one of the configured roots.
fn allowed_path(s: &AppState, path: &str) -> ApiResult<PathBuf> {
    let p = FsPath::new(path.trim());
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Err(ApiError::unprocessable("path_not_allowed", "the path must be absolute, without . or ..").because("path_not_absolute"));
    }
    let p = p.to_path_buf();
    let roots: Vec<PathBuf> = s.cfg.library_roots.iter().map(|r| r.canonicalize().unwrap_or_else(|_| r.clone())).collect();
    let resolved = match p.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            let parent = p.ancestors().find_map(|a| a.canonicalize().ok().map(|c| (a.to_path_buf(), c)));
            match parent {
                Some((a, c)) => c.join(p.strip_prefix(&a).unwrap_or(&p)),
                None => p.clone(),
            }
        }
    };
    if !roots.iter().any(|r| resolved.starts_with(r)) {
        return Err(ApiError::unprocessable(
            "path_not_allowed",
            "the path is not inside a directory allowed by the server's LIBRARY_ROOTS setting",
        )
        .because("path_outside_roots"));
    }
    Ok(resolved)
}

#[derive(Deserialize)]
pub struct LibraryInput {
    name: String,
    kind: String,
    path: Option<String>,
    s3: Option<S3Input>,
    quota_bytes: Option<i64>,
}

fn clean_name(name: &str) -> ApiResult<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(ApiError::bad_request("name must not be empty").because("name_empty"));
    }
    Ok(n.to_string())
}

async fn probe(s: &AppState, b: &Backend) -> ApiResult<()> {
    b.probe(s).await.map_err(|m| ApiError::unprocessable("storage_unreachable", m).because("probe_failed"))
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<LibraryInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    p.require_admin()?;
    let name = clean_name(&input.name)?;
    if input.quota_bytes.is_some_and(|q| q < 0) {
        return Err(ApiError::bad_request("quota_bytes must not be negative").because("quota_negative"));
    }
    let (path, s3, secret) = match input.kind.as_str() {
        "local" => {
            let path = allowed_path(&s, input.path.as_deref().unwrap_or_default())?;
            let b = Backend::Local(path.clone());
            probe(&s, &b).await?;
            (Some(path.to_string_lossy().to_string()), None, None)
        }
        "s3" => {
            let (cfg, secret) = input.s3.clone().unwrap_or_default().merge(None, None)?;
            probe(&s, &Backend::S3(Box::new(S3::new(&cfg, &secret)?))).await?;
            (None, Some(serde_json::to_string(&cfg)?), Some(secret))
        }
        _ => return Err(ApiError::bad_request("kind must be local or s3").because("kind_invalid")),
    };
    let id = new_id();
    let (pos,): (i64,) = sqlx::query_as("SELECT COALESCE(MAX(position), 0) + 1 FROM libraries").fetch_one(&s.db).await?;
    sqlx::query("INSERT INTO libraries (id, name, kind, path, s3, s3_secret, quota_bytes, position, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(&name)
        .bind(&input.kind)
        .bind(&path)
        .bind(&s3)
        .bind(&secret)
        .bind(input.quota_bytes)
        .bind(pos)
        .bind(now())
        .execute(&s.db)
        .await?;
    library::changed(&s, None);
    let lib = library::library(&s, &id).await?;
    Ok((StatusCode::CREATED, Json(to_json(&s, &p, &lib).await?)))
}

#[derive(Deserialize)]
pub struct LibraryPatch {
    name: Option<String>,
    position: Option<i64>,
    #[serde(default, deserialize_with = "double_option")]
    quota_bytes: Option<Option<i64>>,
    path: Option<String>,
    s3: Option<S3Input>,
}

async fn has_folders(s: &AppState, id: &str) -> ApiResult<bool> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media_folders WHERE library_id = ?").bind(id).fetch_one(&s.db).await?;
    Ok(n > 0)
}

pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<LibraryPatch>,
) -> ApiResult<Json<Value>> {
    p.require_admin()?;
    let lib = library::library(&s, &id).await?;
    if let Some(n) = &input.name {
        sqlx::query("UPDATE libraries SET name = ? WHERE id = ?").bind(clean_name(n)?).bind(&id).execute(&s.db).await?;
    }
    if let Some(pos) = input.position {
        sqlx::query("UPDATE libraries SET position = ? WHERE id = ?").bind(pos).bind(&id).execute(&s.db).await?;
    }
    if let Some(q) = input.quota_bytes {
        if q.is_some_and(|q| q < 0) {
            return Err(ApiError::bad_request("quota_bytes must not be negative").because("quota_negative"));
        }
        sqlx::query("UPDATE libraries SET quota_bytes = ? WHERE id = ?").bind(q).bind(&id).execute(&s.db).await?;
    }
    if let Some(path) = &input.path {
        if lib.kind != "local" {
            return Err(ApiError::bad_request("only local libraries have a path").because("path_only_local"));
        }
        let path = allowed_path(&s, path)?;
        let path_str = path.to_string_lossy().to_string();
        if lib.path.as_deref() != Some(path_str.as_str()) {
            if has_folders(&s, &id).await? {
                return Err(ApiError::conflict("library_not_empty", "the path can't change once the library has folders").because("path_locked"));
            }
            probe(&s, &Backend::Local(path)).await?;
            sqlx::query("UPDATE libraries SET path = ? WHERE id = ?").bind(path_str).bind(&id).execute(&s.db).await?;
        }
    }
    if let Some(s3) = &input.s3 {
        if lib.kind != "s3" {
            return Err(ApiError::bad_request("only s3 libraries have s3 settings").because("s3_only_s3"));
        }
        let old = lib.s3_settings();
        let (cfg, secret) = s3.merge(old.clone(), lib.s3_secret.clone())?;
        let moved = old.as_ref().is_some_and(|o| o.bucket != cfg.bucket || o.prefix != cfg.prefix);
        if moved && has_folders(&s, &id).await? {
            return Err(ApiError::conflict("library_not_empty", "the bucket and prefix can't change once the library has folders").because("bucket_locked"));
        }
        probe(&s, &Backend::S3(Box::new(S3::new(&cfg, &secret)?))).await?;
        sqlx::query("UPDATE libraries SET s3 = ?, s3_secret = ? WHERE id = ?")
            .bind(serde_json::to_string(&cfg)?)
            .bind(secret)
            .bind(&id)
            .execute(&s.db)
            .await?;
    }
    library::changed(&s, None);
    Ok(Json(to_json(&s, &p, &library::library(&s, &id).await?).await?))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    p.require_admin()?;
    let lib = library::library(&s, &id).await?;
    if lib.kind == "managed" {
        return Err(ApiError::conflict("library_managed", "the managed library can't be removed"));
    }
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM media WHERE library_id = ?").bind(&id).fetch_one(&s.db).await?;
    if n > 0 || has_folders(&s, &id).await? {
        return Err(ApiError::conflict("library_not_empty", "remove the library's folders first").because("has_folders"));
    }
    sqlx::query("DELETE FROM libraries WHERE id = ?").bind(&id).execute(&s.db).await?;
    library::changed(&s, None);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TestInput {
    library_id: Option<String>,
    kind: String,
    path: Option<String>,
    s3: Option<S3Input>,
}

pub async fn test(State(s): State<AppState>, p: Principal, ApiJson(input): ApiJson<TestInput>) -> ApiResult<Json<Value>> {
    p.require_admin()?;
    let stored = match &input.library_id {
        Some(id) => Some(library::library(&s, id).await?),
        None => None,
    };
    let backend = match input.kind.as_str() {
        "local" => allowed_path(&s, input.path.as_deref().unwrap_or_default()).map(Backend::Local),
        "s3" => input
            .s3
            .clone()
            .unwrap_or_default()
            .merge(stored.as_ref().and_then(|l| l.s3_settings()), stored.and_then(|l| l.s3_secret))
            .and_then(|(cfg, secret)| S3::new(&cfg, &secret))
            .map(|b| Backend::S3(Box::new(b))),
        _ => Err(ApiError::bad_request("kind must be local or s3").because("kind_invalid")),
    };
    let result = match backend {
        Ok(b) => b.probe(&s).await,
        Err(e) => Err(e.message),
    };
    Ok(Json(match result {
        Ok(()) => json!({ "ok": true }),
        Err(m) => json!({ "ok": false, "message": m }),
    }))
}
