//! Libraries, the media folder tree, who reaches which folder, and missing media.

use std::collections::{HashMap, HashSet};

use serde_json::json;
use sqlx::FromRow;

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiResult},
    models::{Media, MEDIA_COLUMNS},
    notices,
    reach::World,
    state::AppState,
    storage::{Backend, S3Settings, S3},
    util::now,
};

#[derive(FromRow, Clone)]
pub struct LibraryRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub path: Option<String>,
    pub s3: Option<String>,
    pub s3_secret: Option<String>,
    pub quota_bytes: Option<i64>,
    pub position: i64,
    pub created_at: String,
}

pub const LIBRARY_COLUMNS: &str = "id, name, kind, path, s3, s3_secret, quota_bytes, position, created_at";

impl LibraryRow {
    pub fn s3_settings(&self) -> Option<S3Settings> {
        self.s3.as_deref().and_then(|t| serde_json::from_str(t).ok())
    }

    pub fn backend(&self) -> ApiResult<Backend> {
        Ok(match self.kind.as_str() {
            "local" => Backend::Local(self.path.clone().ok_or_else(|| ApiError::internal("local library without path"))?.into()),
            "s3" => {
                let cfg = self.s3_settings().ok_or_else(|| ApiError::internal("s3 library without settings"))?;
                Backend::S3(Box::new(S3::new(&cfg, self.s3_secret.as_deref().unwrap_or_default())?))
            }
            _ => Backend::Managed,
        })
    }
}

pub async fn library(s: &AppState, id: &str) -> ApiResult<LibraryRow> {
    sqlx::query_as(&format!("SELECT {LIBRARY_COLUMNS} FROM libraries WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(ApiError::not_found)
}

pub async fn used_bytes(s: &AppState, library_id: &str) -> ApiResult<i64> {
    let (n,): (Option<i64>,) = sqlx::query_as("SELECT SUM(size) FROM media WHERE library_id = ? AND missing_at IS NULL")
        .bind(library_id)
        .fetch_one(&s.db)
        .await?;
    Ok(n.unwrap_or(0))
}

/// Space left: the quota less what's used, capped by the storage's free space.
pub async fn free_bytes(s: &AppState, lib: &LibraryRow) -> ApiResult<Option<i64>> {
    let disk = lib.backend()?.free(s);
    let quota = match lib.quota_bytes {
        Some(q) => Some((q - used_bytes(s, &lib.id).await?).max(0)),
        None => None,
    };
    Ok(match (disk, quota) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    })
}

pub fn insufficient(free: i64) -> ApiError {
    let mut e = ApiError::new(axum::http::StatusCode::INSUFFICIENT_STORAGE, "insufficient_storage", "not enough space in the library").with("free_bytes", free);
    e.extra = Some(json!({ "free_bytes": free }));
    e
}

pub async fn ensure_space(s: &AppState, lib: &LibraryRow, size: i64) -> ApiResult<()> {
    match free_bytes(s, lib).await? {
        Some(free) if free < size => Err(insufficient(free)),
        _ => Ok(()),
    }
}

/// File and folder names: one path component, not hidden, no separators.
pub fn check_name(name: &str) -> ApiResult<String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 255 || n.starts_with('.') || n.contains(['/', '\\', '\0']) {
        return Err(ApiError::bad_request("names can't be empty, start with a dot, or contain / or \\").because("name_invalid"));
    }
    Ok(n.to_string())
}

#[derive(FromRow, Clone)]
pub struct FolderRow {
    pub id: String,
    pub library_id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub created_at: String,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Access {
    None,
    View,
    Edit,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::None => "none",
            Access::View => "view",
            Access::Edit => "edit",
        }
    }
}

/// A snapshot of every folder with its tags and grants.
#[derive(Default)]
pub struct Tree {
    pub folders: HashMap<String, FolderRow>,
    pub tags: HashMap<String, HashSet<String>>,
    pub grants: HashMap<String, HashSet<String>>,
}

impl Tree {
    pub async fn load(s: &AppState) -> ApiResult<Tree> {
        let mut t = Tree::default();
        let rows: Vec<FolderRow> =
            sqlx::query_as("SELECT id, library_id, parent_id, name, created_at FROM media_folders").fetch_all(&s.db).await?;
        t.folders = rows.into_iter().map(|f| (f.id.clone(), f)).collect();
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT folder_id, tag_id FROM media_folder_tags").fetch_all(&s.db).await?;
        for (f, tag) in rows {
            t.tags.entry(f).or_default().insert(tag);
        }
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT folder_id, team_id FROM media_folder_grants").fetch_all(&s.db).await?;
        for (f, team) in rows {
            t.grants.entry(f).or_default().insert(team);
        }
        Ok(t)
    }

    pub fn get(&self, id: &str) -> ApiResult<&FolderRow> {
        self.folders.get(id).ok_or_else(ApiError::not_found)
    }

    /// Ancestors, nearest first (not including `id`).
    pub fn ancestors(&self, id: &str) -> Vec<&FolderRow> {
        let mut out = vec![];
        let mut cur = self.folders.get(id).and_then(|f| f.parent_id.as_deref());
        while let Some(pid) = cur {
            let Some(f) = self.folders.get(pid) else { break };
            if out.len() > 256 {
                break;
            }
            out.push(f);
            cur = f.parent_id.as_deref();
        }
        out
    }

    pub fn path(&self, id: &str) -> String {
        let Some(f) = self.folders.get(id) else { return String::new() };
        let mut parts: Vec<&str> = self.ancestors(id).iter().map(|a| a.name.as_str()).collect();
        parts.reverse();
        parts.push(&f.name);
        parts.join("/")
    }

    /// `id` and every folder under it.
    pub fn subtree(&self, id: &str) -> Vec<String> {
        let mut out = vec![id.to_string()];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i].clone();
            out.extend(self.folders.values().filter(|f| f.parent_id.as_deref() == Some(&cur)).map(|f| f.id.clone()));
            i += 1;
        }
        out
    }

    pub fn inherited_tags(&self, id: &str) -> HashSet<String> {
        self.ancestors(id).iter().flat_map(|a| self.tags.get(&a.id).into_iter().flatten().cloned()).collect()
    }

    pub fn inherited_teams(&self, id: &str) -> HashSet<String> {
        self.ancestors(id).iter().flat_map(|a| self.grants.get(&a.id).into_iter().flatten().cloned()).collect()
    }

    pub fn team_reaches(&self, w: &World, team: &str, folder: &str) -> bool {
        let Some(info) = w.teams.get(team) else { return false };
        if !self.folders.contains_key(folder) {
            return false;
        }
        if info.all {
            return true;
        }
        let mut chain = vec![folder.to_string()];
        chain.extend(self.ancestors(folder).iter().map(|a| a.id.clone()));
        chain.iter().any(|f| {
            self.grants.get(f).is_some_and(|g| g.contains(team))
                || self.tags.get(f).is_some_and(|t| !t.is_disjoint(&info.tags))
        })
    }

    pub fn access(&self, p: &Principal, w: &World, folder: &str) -> Access {
        if !self.folders.contains_key(folder) {
            return Access::None;
        }
        if p.server_admin {
            return Access::Edit;
        }
        let mut best = Access::None;
        for m in &p.memberships {
            if self.team_reaches(w, &m.team_id, folder) {
                best = best.max(if m.role >= Role::Editor { Access::Edit } else { Access::View });
            }
        }
        best
    }

    pub fn require(&self, p: &Principal, w: &World, folder: &str, min: Access) -> ApiResult<()> {
        match self.access(p, w, folder) {
            Access::None => Err(ApiError::not_found()),
            a if a >= min => Ok(()),
            _ => Err(ApiError::forbidden()),
        }
    }
}

pub async fn fetch_media(s: &AppState, id: &str) -> ApiResult<Media> {
    sqlx::query_as(&format!("SELECT {MEDIA_COLUMNS} FROM media WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(ApiError::not_found)
}

/// Media ids used by playlists of the given teams.
pub async fn used_by_teams(s: &AppState, teams: &[String]) -> ApiResult<HashSet<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT pm.media_id FROM playlist_media pm JOIN playlists pl ON pl.id = pm.playlist_id
         WHERE pl.team_id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(teams)?)
    .fetch_all(&s.db)
    .await?;
    Ok(rows.into_iter().map(|(m,)| m).collect())
}

/// What `p` may do with `m`, or None if it's not visible to them.
pub async fn media_access(s: &AppState, p: &Principal, w: &World, tree: &Tree, m: &Media) -> ApiResult<Option<&'static str>> {
    if let Some(f) = &m.folder_id {
        match tree.access(p, w, f) {
            Access::Edit => return Ok(Some("edit")),
            Access::View => return Ok(Some("view")),
            Access::None => {}
        }
    }
    if p.server_admin {
        return Ok(Some(if m.missing_at.is_some() { "playlist" } else { "edit" }));
    }
    let teams: Vec<String> = p.memberships.iter().map(|m| m.team_id.clone()).collect();
    Ok(used_by_teams(s, &teams).await?.contains(&m.id).then_some("playlist"))
}

/// Deletes a managed blob once nothing refers to its sha any more.
pub async fn gc_blob(s: &AppState, sha: &str) -> ApiResult<()> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM media WHERE sha256 = ?1 AND library_id = 'managed' AND missing_at IS NULL)
              + (SELECT COUNT(*) FROM media WHERE thumbnail_sha256 = ?1)
              + (SELECT COUNT(*) FROM floor_plans WHERE image_sha256 = ?1)",
    )
    .bind(sha)
    .fetch_one(&s.db)
    .await?;
    if n == 0 {
        let _ = tokio::fs::remove_file(s.blob_path(sha)).await;
    }
    Ok(())
}

/// Removes a file's bytes and then its record, or marks it missing when playlists use it.
pub async fn delete_media(s: &AppState, p: &Principal, m: &Media) -> ApiResult<()> {
    if m.missing_at.is_none() {
        let lib = library(s, &m.library_id).await?;
        lib.backend()?.delete_file(&m.path).await?;
    }
    let users: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT pl.id, pl.team_id, pl.updated_by FROM playlist_media pm JOIN playlists pl ON pl.id = pm.playlist_id WHERE pm.media_id = ?",
    )
    .bind(&m.id)
    .fetch_all(&s.db)
    .await?;
    if users.is_empty() {
        sqlx::query("DELETE FROM media WHERE id = ?").bind(&m.id).execute(&s.db).await?;
        if let Some(t) = &m.thumbnail_sha256 {
            gc_blob(s, t).await?;
        }
    } else if m.missing_at.is_none() {
        sqlx::query("UPDATE media SET missing_at = ?, folder_id = NULL WHERE id = ?").bind(now()).bind(&m.id).execute(&s.db).await?;
        let playlist_ids: Vec<&String> = users.iter().map(|u| &u.0).collect();
        let mut subs: Vec<&str> = users.iter().filter_map(|u| u.2.as_deref()).filter(|u| *u != p.subject).collect();
        subs.sort();
        subs.dedup();
        for sub in subs {
            notices::create_media(s, sub, &m.id, json!({
                "media_id": m.id, "name": m.name, "path": m.path, "playlist_ids": playlist_ids,
                "deleted_by": p.display_name.clone().unwrap_or_else(|| p.subject.clone()),
            }))
            .await?;
        }
        for (pl, team, _) in &users {
            let (version,): (i64,) =
                sqlx::query_as("UPDATE playlists SET version = version + 1 WHERE id = ? RETURNING version").bind(pl).fetch_one(&s.db).await?;
            s.notify_screens_for_playlist(pl, version).await;
            s.hub.broadcast_team(team, &json!({ "type": "schedule_changed", "team_id": team }));
        }
    }
    if m.library_id == "managed" && m.missing_at.is_none() {
        gc_blob(s, &m.sha256).await?;
    }
    Ok(())
}

/// Bumps every playlist using `media_id` so screens refetch their manifests.
pub async fn bump_playlists_using(s: &AppState, media_id: &str) -> ApiResult<()> {
    let users: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT pl.id, pl.team_id FROM playlist_media pm JOIN playlists pl ON pl.id = pm.playlist_id WHERE pm.media_id = ?",
    )
    .bind(media_id)
    .fetch_all(&s.db)
    .await?;
    for (pl, team) in &users {
        let (version,): (i64,) =
            sqlx::query_as("UPDATE playlists SET version = version + 1 WHERE id = ? RETURNING version").bind(pl).fetch_one(&s.db).await?;
        s.notify_screens_for_playlist(pl, version).await;
        s.hub.broadcast_team(team, &json!({ "type": "schedule_changed", "team_id": team }));
    }
    Ok(())
}

/// Drops missing files no playlist uses any more, and resolves their notices.
pub async fn sweep_missing(s: &AppState) -> ApiResult<()> {
    let gone: Vec<(String, Option<String>)> = sqlx::query_as(
        "DELETE FROM media WHERE missing_at IS NOT NULL AND id NOT IN (SELECT media_id FROM playlist_media)
         RETURNING id, thumbnail_sha256",
    )
    .fetch_all(&s.db)
    .await?;
    for (id, thumb) in gone {
        sqlx::query("UPDATE notices SET resolved_at = ? WHERE kind = 'media_missing' AND media_id = ? AND resolved_at IS NULL")
            .bind(now())
            .bind(&id)
            .execute(&s.db)
            .await?;
        if let Some(t) = thumb {
            gc_blob(s, &t).await?;
        }
    }
    Ok(())
}

pub fn changed(s: &AppState, folder_id: Option<&str>) {
    let mut msg = json!({ "type": "media_changed" });
    if let Some(f) = folder_id {
        msg["folder_id"] = json!(f);
    }
    s.hub.broadcast_all(&msg);
}

/// Playlists using any of `media_ids`, for the delete confirmation.
pub async fn usage(s: &AppState, p: &Principal, media_ids: &[String]) -> ApiResult<serde_json::Value> {
    let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT pl.id, pl.name, pl.team_id, t.name, pm.media_id FROM playlist_media pm
         JOIN playlists pl ON pl.id = pm.playlist_id JOIN teams t ON t.id = pl.team_id
         WHERE pm.media_id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(media_ids)?)
    .fetch_all(&s.db)
    .await?;
    let ids: HashSet<&String> = media_ids.iter().collect();
    let mut by_playlist: HashMap<String, (String, String, String, i64)> = HashMap::new();
    let mut items_json: HashMap<String, String> = HashMap::new();
    for (id, name, team, team_name, _) in &rows {
        by_playlist.entry(id.clone()).or_insert((name.clone(), team.clone(), team_name.clone(), 0));
        if !items_json.contains_key(id) {
            let (items, beds): (String, String) =
                sqlx::query_as("SELECT items, beds FROM playlists WHERE id = ?").bind(id).fetch_one(&s.db).await?;
            items_json.insert(id.clone(), format!("{items}\n{beds}"));
        }
    }
    for (id, entry) in by_playlist.iter_mut() {
        let text = &items_json[id];
        entry.3 = ids.iter().map(|m| text.matches(m.as_str()).count() as i64).sum();
    }
    let mut list: Vec<serde_json::Value> = by_playlist
        .into_iter()
        .map(|(id, (name, team, team_name, items))| {
            json!({ "id": id, "name": name, "team_id": team, "team_name": team_name, "items": items, "member": p.can_view(&team) })
        })
        .collect();
    list.sort_by(|a, b| {
        (a["team_name"].as_str(), a["name"].as_str()).cmp(&(b["team_name"].as_str(), b["name"].as_str()))
    });
    Ok(json!({ "media_count": media_ids.len(), "playlists": list }))
}
