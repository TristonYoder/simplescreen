//! Where a library's bytes live: the managed blob store, a local directory, or an S3 bucket.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use base64::Engine;
use futures_util::StreamExt;
use rusty_s3::{
    actions::{DeleteObject, GetObject, HeadObject, ListObjectsV2, PutObject},
    Bucket, Credentials, S3Action, UrlStyle,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{
    error::{ApiError, ApiResult},
    state::AppState,
    util::new_id,
};

pub const S3_SINGLE_PUT_LIMIT: i64 = 5 * 1024 * 1024 * 1024;
const PRESIGN_TTL: Duration = Duration::from_secs(3600);

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct S3Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub path_style: bool,
    pub access_key_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdn_base_url: Option<String>,
}

pub struct S3 {
    bucket: Bucket,
    creds: Credentials,
    prefix: String,
    cdn: Option<String>,
    http: reqwest::Client,
}

pub enum Backend {
    Managed,
    Local(PathBuf),
    S3(Box<S3>),
}

fn s3_err(e: impl std::fmt::Display) -> ApiError {
    ApiError::unprocessable("storage_unreachable", format!("S3: {e}"))
}

impl S3 {
    pub fn new(cfg: &S3Settings, secret: &str) -> ApiResult<S3> {
        let endpoint = match &cfg.endpoint {
            Some(e) => e.clone(),
            None => format!("https://s3.{}.amazonaws.com", cfg.region),
        };
        let url = url::Url::parse(&endpoint).map_err(|e| ApiError::bad_request(format!("invalid S3 endpoint: {e}")))?;
        let style = if cfg.path_style { UrlStyle::Path } else { UrlStyle::VirtualHost };
        let bucket = Bucket::new(url, style, cfg.bucket.clone(), cfg.region.clone())
            .map_err(|e| ApiError::bad_request(format!("invalid S3 bucket: {e}")))?;
        let prefix = cfg.prefix.trim_matches('/');
        Ok(S3 {
            bucket,
            creds: Credentials::new(cfg.access_key_id.clone(), secret.to_string()),
            prefix: if prefix.is_empty() { String::new() } else { format!("{prefix}/") },
            cdn: cfg.cdn_base_url.as_ref().map(|c| c.trim_end_matches('/').to_string()),
            http: reqwest::Client::new(),
        })
    }

    fn key(&self, path: &str) -> String {
        format!("{}{}", self.prefix, path)
    }

    pub fn get_url(&self, path: &str) -> String {
        let key = self.key(path);
        match &self.cdn {
            Some(base) => format!("{base}/{}", key.split('/').map(|c| urlencode(c)).collect::<Vec<_>>().join("/")),
            None => GetObject::new(&self.bucket, Some(&self.creds), &key).sign(PRESIGN_TTL).to_string(),
        }
    }

    /// A presigned PUT that S3 only accepts with these bytes (it checks the SHA-256).
    pub fn put_url(&self, path: &str, sha: &str) -> ApiResult<(String, Vec<(String, String)>)> {
        let key = self.key(path);
        let checksum = sha_b64(sha)?;
        let mut action = PutObject::new(&self.bucket, Some(&self.creds), &key);
        action.headers_mut().insert("x-amz-checksum-sha256", checksum.clone());
        Ok((action.sign(PRESIGN_TTL).to_string(), vec![("x-amz-checksum-sha256".into(), checksum)]))
    }

    /// Size and SHA-256 (hex) of an object, if it exists and S3 has its checksum.
    pub async fn head(&self, path: &str) -> ApiResult<Option<(i64, Option<String>)>> {
        let key = self.key(path);
        let mut action = HeadObject::new(&self.bucket, Some(&self.creds), &key);
        action.headers_mut().insert("x-amz-checksum-mode", "ENABLED");
        let url = action.sign(PRESIGN_TTL);
        let resp = self.http.head(url).header("x-amz-checksum-mode", "ENABLED").send().await.map_err(s3_err)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(s3_err(format!("HEAD {key}: {}", resp.status())));
        }
        let size = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let sha = resp
            .headers()
            .get("x-amz-checksum-sha256")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
            .map(hex::encode);
        Ok(Some((size, sha)))
    }

    pub async fn put_file(&self, path: &str, file: &Path, sha: &str) -> ApiResult<()> {
        let (url, headers) = self.put_url(path, sha)?;
        let f = tokio::fs::File::open(file).await?;
        let len = f.metadata().await?.len();
        let mut req = self.http.put(url).header(reqwest::header::CONTENT_LENGTH, len);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
        let resp = req.body(body).send().await.map_err(s3_err)?;
        if !resp.status().is_success() {
            return Err(s3_err(format!("PUT {path}: {}", resp.status())));
        }
        Ok(())
    }

    pub async fn download(&self, path: &str, to: &Path) -> ApiResult<()> {
        let key = self.key(path);
        let url = GetObject::new(&self.bucket, Some(&self.creds), &key).sign(PRESIGN_TTL);
        let resp = self.http.get(url).send().await.map_err(s3_err)?;
        if !resp.status().is_success() {
            return Err(s3_err(format!("GET {key}: {}", resp.status())));
        }
        let mut out = tokio::fs::File::create(to).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            out.write_all(&chunk.map_err(s3_err)?).await?;
        }
        out.flush().await?;
        Ok(())
    }

    pub async fn copy(&self, from: &str, to: &str) -> ApiResult<()> {
        let (src, dst) = (self.key(from), self.key(to));
        let source = format!("/{}/{}", self.bucket.name(), src.split('/').map(|c| urlencode(c)).collect::<Vec<_>>().join("/"));
        let mut action = PutObject::new(&self.bucket, Some(&self.creds), &dst);
        action.headers_mut().insert("x-amz-copy-source", source.clone());
        let resp = self
            .http
            .put(action.sign(PRESIGN_TTL))
            .header("x-amz-copy-source", source)
            .header(reqwest::header::CONTENT_LENGTH, 0)
            .send()
            .await
            .map_err(s3_err)?;
        if !resp.status().is_success() {
            return Err(s3_err(format!("copy {src} to {dst}: {}", resp.status())));
        }
        Ok(())
    }

    pub async fn delete(&self, path: &str) -> ApiResult<()> {
        let key = self.key(path);
        let url = DeleteObject::new(&self.bucket, Some(&self.creds), &key).sign(PRESIGN_TTL);
        let resp = self.http.delete(url).send().await.map_err(s3_err)?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(s3_err(format!("DELETE {key}: {}", resp.status())));
        }
        Ok(())
    }

    /// Paths (relative to the library) of every object under `dir/`.
    pub async fn list(&self, dir: &str) -> ApiResult<Vec<String>> {
        let prefix = self.key(&format!("{dir}/"));
        let mut out = vec![];
        let mut token: Option<String> = None;
        loop {
            let mut action = ListObjectsV2::new(&self.bucket, Some(&self.creds));
            action.with_prefix(prefix.clone());
            if let Some(t) = &token {
                action.with_continuation_token(t.clone());
            }
            let resp = self.http.get(action.sign(PRESIGN_TTL)).send().await.map_err(s3_err)?;
            if !resp.status().is_success() {
                return Err(s3_err(format!("list {prefix}: {}", resp.status())));
            }
            let text = resp.text().await.map_err(s3_err)?;
            let parsed = ListObjectsV2::parse_response(&text).map_err(s3_err)?;
            out.extend(parsed.contents.into_iter().filter_map(|c| c.key.strip_prefix(&self.prefix).map(String::from)));
            match parsed.next_continuation_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        Ok(out)
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn sha_b64(sha: &str) -> ApiResult<String> {
    let bytes = hex::decode(sha).map_err(|_| ApiError::bad_request("sha256 must be 64 lowercase hex characters"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Free bytes on the filesystem holding `path`.
pub fn disk_free(path: &Path) -> Option<i64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64) as i64)
}

/// Copies `from` to `to` through a temp file in the same directory, so `to` appears whole.
pub async fn copy_into(from: &Path, to: &Path) -> ApiResult<()> {
    let dir = to.parent().ok_or_else(|| ApiError::internal("destination has no parent"))?;
    tokio::fs::create_dir_all(dir).await?;
    let tmp = dir.join(format!(".{}.part", new_id()));
    if let Err(e) = tokio::fs::copy(from, &tmp).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    if let Err(e) = tokio::fs::rename(&tmp, to).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok(())
}

impl Backend {
    pub fn local_path(&self, path: &str) -> Option<PathBuf> {
        match self {
            Backend::Local(root) => Some(root.join(path)),
            _ => None,
        }
    }

    pub async fn mkdir(&self, path: &str) -> ApiResult<()> {
        if let Some(p) = self.local_path(path) {
            tokio::fs::create_dir_all(p).await?;
        }
        Ok(())
    }

    /// Puts a file whose bytes are at `src` (a local file) at `path`.
    pub async fn put_from(&self, s: &AppState, src: &Path, path: &str, sha: &str) -> ApiResult<()> {
        match self {
            Backend::Managed => {
                let dest = s.blob_path(sha);
                if tokio::fs::metadata(&dest).await.is_err() {
                    copy_into(src, &dest).await?;
                }
                Ok(())
            }
            Backend::Local(root) => copy_into(src, &root.join(path)).await,
            Backend::S3(b) => b.put_file(path, src, sha).await,
        }
    }

    /// A local file with the bytes of `path`: the file itself, or a download into the temp dir
    /// (the bool says whether the caller must delete it).
    pub async fn fetch_local(&self, s: &AppState, path: &str, sha: &str) -> ApiResult<(PathBuf, bool)> {
        match self {
            Backend::Managed => Ok((s.blob_path(sha), false)),
            Backend::Local(root) => Ok((root.join(path), false)),
            Backend::S3(b) => {
                let tmp = s.tmp_dir().join(new_id());
                if let Err(e) = b.download(path, &tmp).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(e);
                }
                Ok((tmp, true))
            }
        }
    }

    /// Deletes the file at `path`. Managed blobs are left to `library::gc_blob`.
    pub async fn delete_file(&self, path: &str) -> ApiResult<()> {
        match self {
            Backend::Managed => Ok(()),
            Backend::Local(root) => match tokio::fs::remove_file(root.join(path)).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
            Backend::S3(b) => b.delete(path).await,
        }
    }

    pub async fn rename_file(&self, from: &str, to: &str) -> ApiResult<()> {
        match self {
            Backend::Managed => Ok(()),
            Backend::Local(root) => {
                let dest = root.join(to);
                if let Some(dir) = dest.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                tokio::fs::rename(root.join(from), dest).await?;
                Ok(())
            }
            Backend::S3(b) => {
                b.copy(from, to).await?;
                b.delete(from).await
            }
        }
    }

    pub async fn rename_dir(&self, from: &str, to: &str) -> ApiResult<()> {
        match self {
            Backend::Managed => Ok(()),
            Backend::Local(root) => {
                let (src, dest) = (root.join(from), root.join(to));
                if let Some(dir) = dest.parent() {
                    tokio::fs::create_dir_all(dir).await?;
                }
                match tokio::fs::rename(&src, &dest).await {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => tokio::fs::create_dir_all(&dest).await?,
                    r => r?,
                }
                Ok(())
            }
            Backend::S3(b) => {
                for key in b.list(from).await? {
                    let rest = &key[from.len()..];
                    b.copy(&key, &format!("{to}{rest}")).await?;
                    b.delete(&key).await?;
                }
                Ok(())
            }
        }
    }

    pub async fn remove_dir(&self, path: &str) -> ApiResult<()> {
        match self {
            Backend::Managed => Ok(()),
            Backend::Local(root) => match tokio::fs::remove_dir_all(root.join(path)).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
            Backend::S3(b) => {
                for key in b.list(path).await? {
                    b.delete(&key).await?;
                }
                Ok(())
            }
        }
    }

    /// Free bytes in the underlying storage; None = unknown or unlimited.
    pub fn free(&self, s: &AppState) -> Option<i64> {
        match self {
            Backend::Managed => disk_free(&s.cfg.data_dir),
            Backend::Local(root) => disk_free(root),
            Backend::S3(_) => None,
        }
    }

    /// Writes, reads back and deletes a small probe file.
    pub async fn probe(&self, s: &AppState) -> Result<(), String> {
        let name = format!(".probe-{}", new_id());
        let body = b"probe".to_vec();
        let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&body));
        let tmp = s.tmp_dir().join(new_id());
        tokio::fs::write(&tmp, &body).await.map_err(|e| e.to_string())?;
        let result = async {
            match self {
                Backend::Managed => Ok(()),
                Backend::Local(root) => {
                    tokio::fs::create_dir_all(root).await.map_err(|e| e.to_string())?;
                    let p = root.join(&name);
                    tokio::fs::copy(&tmp, &p).await.map_err(|e| e.to_string())?;
                    let back = tokio::fs::read(&p).await.map_err(|e| e.to_string())?;
                    tokio::fs::remove_file(&p).await.map_err(|e| e.to_string())?;
                    if back != body {
                        return Err("read back different bytes".into());
                    }
                    Ok(())
                }
                Backend::S3(b) => {
                    b.put_file(&name, &tmp, &sha).await.map_err(|e| e.message)?;
                    let head = b.head(&name).await.map_err(|e| e.message)?;
                    b.delete(&name).await.map_err(|e| e.message)?;
                    match head {
                        Some(_) => Ok(()),
                        None => Err("the probe file was not found after writing it".into()),
                    }
                }
            }
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        result
    }
}
