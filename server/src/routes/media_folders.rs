use crate::error::ApiQuery;
use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    library::{self, Access, Tree},
    models::{double_option, Media, MEDIA_COLUMNS},
    reach::World,
    state::AppState,
    util::{new_id, now},
};

fn sorted(set: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v.dedup();
    v
}

fn folder_json(tree: &Tree, id: &str, access: Access) -> Value {
    let f = &tree.folders[id];
    let mut v = json!({
        "id": f.id, "library_id": f.library_id, "name": f.name, "path": tree.path(id),
        "tag_ids": sorted(tree.tags.get(id).cloned().unwrap_or_default()),
        "team_ids": sorted(tree.grants.get(id).cloned().unwrap_or_default()),
        "inherited_tag_ids": sorted(tree.inherited_tags(id)),
        "inherited_team_ids": sorted(tree.inherited_teams(id)),
        "access": access.as_str(), "created_at": f.created_at,
    });
    if let Some(pid) = &f.parent_id {
        v["parent_id"] = json!(pid);
    }
    v
}

fn name_taken(e: sqlx::Error) -> ApiError {
    match &e {
        sqlx::Error::Database(d) if d.is_unique_violation() => ApiError::conflict("name_taken", "that name is already used here").because("folder"),
        _ => e.into(),
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    library_id: Option<String>,
    team_id: Option<String>,
}

pub async fn list(State(s): State<AppState>, p: Principal, ApiQuery(q): ApiQuery<ListQuery>) -> ApiResult<Json<Vec<Value>>> {
    super::assignments::require_user(&p)?;
    if let Some(t) = &q.team_id {
        s.require_team(&p, t, Role::Viewer).await?;
    }
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    let mut access: Vec<(String, Access)> = vec![];
    for id in tree.folders.keys() {
        if q.library_id.as_ref().is_some_and(|l| *l != tree.folders[id].library_id) {
            continue;
        }
        let a = match &q.team_id {
            Some(t) if tree.team_reaches(&w, t, id) => {
                if p.role_in(t).is_some_and(|r| r >= Role::Editor) { Access::Edit } else { Access::View }
            }
            Some(_) => Access::None,
            None => tree.access(&p, &w, id),
        };
        if a != Access::None {
            access.push((id.clone(), a));
        }
    }
    let reached: HashSet<String> = access.iter().map(|(id, _)| id.clone()).collect();
    let mut ancestors: HashSet<String> = HashSet::new();
    for id in &reached {
        for a in tree.ancestors(id) {
            if !reached.contains(&a.id) {
                ancestors.insert(a.id.clone());
            }
        }
    }
    access.extend(ancestors.into_iter().map(|id| (id, Access::None)));
    let mut out: Vec<(String, String, Value)> = access
        .into_iter()
        .map(|(id, a)| (tree.folders[&id].library_id.clone(), tree.path(&id).to_lowercase(), folder_json(&tree, &id, a)))
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    Ok(Json(out.into_iter().map(|(_, _, v)| v).collect()))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    super::assignments::require_user(&p)?;
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    tree.get(&id)?;
    tree.require(&p, &w, &id, Access::View)?;
    Ok(Json(folder_json(&tree, &id, tree.access(&p, &w, &id))))
}

#[derive(Deserialize)]
pub struct FolderInput {
    library_id: String,
    parent_id: Option<String>,
    name: String,
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<FolderInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    super::assignments::require_user(&p)?;
    let name = library::check_name(&input.name)?;
    let lib = library::library(&s, &input.library_id).await?;
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    let path = match &input.parent_id {
        None => {
            p.require_admin()?;
            name.clone()
        }
        Some(pid) => {
            let parent = tree.get(pid)?;
            if parent.library_id != lib.id {
                return Err(ApiError::unprocessable("invalid_folder", "the parent folder is in another library").because("parent_other_library"));
            }
            tree.require(&p, &w, pid, Access::Edit)?;
            format!("{}/{name}", tree.path(pid))
        }
    };
    let id = new_id();
    sqlx::query("INSERT INTO media_folders (id, library_id, parent_id, name, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(&lib.id)
        .bind(&input.parent_id)
        .bind(&name)
        .bind(now())
        .execute(&s.db)
        .await
        .map_err(name_taken)?;
    if let Err(e) = lib.backend()?.mkdir(&path).await {
        sqlx::query("DELETE FROM media_folders WHERE id = ?").bind(&id).execute(&s.db).await?;
        return Err(e);
    }
    library::changed(&s, input.parent_id.as_deref());
    let tree = Tree::load(&s).await?;
    Ok((StatusCode::CREATED, Json(folder_json(&tree, &id, tree.access(&p, &w, &id)))))
}

#[derive(Deserialize)]
pub struct FolderPatch {
    name: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    parent_id: Option<Option<String>>,
}

pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<FolderPatch>,
) -> ApiResult<Json<Value>> {
    super::assignments::require_user(&p)?;
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    let folder = tree.get(&id)?.clone();
    tree.require(&p, &w, &id, Access::Edit)?;
    if folder.parent_id.is_none() {
        p.require_admin()?;
    }
    let name = match &input.name {
        Some(n) => library::check_name(n)?,
        None => folder.name.clone(),
    };
    let parent = match &input.parent_id {
        None => folder.parent_id.clone(),
        Some(None) => {
            p.require_admin()?;
            None
        }
        Some(Some(pid)) => {
            let target = tree.get(pid)?;
            if target.library_id != folder.library_id || tree.subtree(&id).contains(pid) {
                return Err(ApiError::unprocessable("invalid_folder", "a folder can't move into itself, a subfolder, or another library").because("move_invalid"));
            }
            tree.require(&p, &w, pid, Access::Edit)?;
            Some(pid.clone())
        }
    };
    if name == folder.name && parent == folder.parent_id {
        return Ok(Json(folder_json(&tree, &id, tree.access(&p, &w, &id))));
    }
    let old_path = tree.path(&id);
    let new_path = match &parent {
        Some(pid) => format!("{}/{name}", tree.path(pid)),
        None => name.clone(),
    };
    let taken: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM media_folders WHERE library_id = ? AND COALESCE(parent_id, '') = COALESCE(?, '') AND name = ? COLLATE NOCASE AND id != ?",
    )
    .bind(&folder.library_id)
    .bind(&parent)
    .bind(&name)
    .bind(&id)
    .fetch_optional(&s.db)
    .await?;
    if taken.is_some() {
        return Err(ApiError::conflict("name_taken", "that name is already used here").because("folder"));
    }
    let lib = library::library(&s, &folder.library_id).await?;
    if !old_path.eq(&new_path) {
        lib.backend()?.rename_dir(&old_path, &new_path).await?;
    }
    let mut tx = s.db.begin().await?;
    sqlx::query("UPDATE media_folders SET name = ?, parent_id = ? WHERE id = ?")
        .bind(&name)
        .bind(&parent)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(name_taken)?;
    let sub = tree.subtree(&id);
    sqlx::query(
        "UPDATE media SET path = ? || substr(path, ?) WHERE folder_id IN (SELECT value FROM json_each(?))",
    )
    .bind(&new_path)
    .bind(old_path.chars().count() as i64 + 1)
    .bind(serde_json::to_string(&sub)?)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    library::changed(&s, None);
    let tree = Tree::load(&s).await?;
    Ok(Json(folder_json(&tree, &id, tree.access(&p, &w, &id))))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    super::assignments::require_user(&p)?;
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    let folder = tree.get(&id)?.clone();
    tree.require(&p, &w, &id, Access::Edit)?;
    if folder.parent_id.is_none() {
        p.require_admin()?;
    }
    let sub = tree.subtree(&id);
    let files: Vec<Media> = sqlx::query_as(&format!(
        "SELECT {MEDIA_COLUMNS} FROM media WHERE folder_id IN (SELECT value FROM json_each(?))"
    ))
    .bind(serde_json::to_string(&sub)?)
    .fetch_all(&s.db)
    .await?;
    for m in &files {
        library::delete_media(&s, &p, m).await?;
    }
    let lib = library::library(&s, &folder.library_id).await?;
    lib.backend()?.remove_dir(&tree.path(&id)).await?;
    for f in sub.iter().rev() {
        sqlx::query("DELETE FROM media_folders WHERE id = ?").bind(f).execute(&s.db).await?;
    }
    library::changed(&s, None);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TagIds {
    tag_ids: Vec<String>,
}

pub async fn put_tags(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<TagIds>,
) -> ApiResult<Json<Value>> {
    super::assignments::require_user(&p)?;
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    tree.get(&id)?;
    tree.require(&p, &w, &id, Access::Edit)?;
    let new: HashSet<String> = input.tag_ids.into_iter().collect();
    for t in &new {
        let found: Option<(String,)> = sqlx::query_as("SELECT id FROM tags WHERE id = ?").bind(t).fetch_optional(&s.db).await?;
        if found.is_none() {
            return Err(ApiError::unprocessable("unknown_tag", format!("tag {t} does not exist")).with("tag", t));
        }
    }
    let old = tree.tags.get(&id).cloned().unwrap_or_default();
    if !p.server_admin {
        let editor_teams: Vec<&str> =
            p.memberships.iter().filter(|m| m.role >= Role::Editor).map(|m| m.team_id.as_str()).collect();
        for t in old.symmetric_difference(&new) {
            if !editor_teams.iter().any(|team| w.team_holds(team, t)) {
                return Err(ApiError::new(StatusCode::FORBIDDEN, "tag_not_held", "only tags your team holds can be added or removed"));
            }
        }
    }
    let mut tx = s.db.begin().await?;
    sqlx::query("DELETE FROM media_folder_tags WHERE folder_id = ?").bind(&id).execute(&mut *tx).await?;
    for t in &new {
        sqlx::query("INSERT INTO media_folder_tags (folder_id, tag_id) VALUES (?, ?)").bind(&id).bind(t).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    library::changed(&s, None);
    let tree = Tree::load(&s).await?;
    Ok(Json(folder_json(&tree, &id, tree.access(&p, &w, &id))))
}

#[derive(Deserialize)]
pub struct TeamIds {
    team_ids: Vec<String>,
}

pub async fn put_grants(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<TeamIds>,
) -> ApiResult<Json<Value>> {
    super::assignments::require_user(&p)?;
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    tree.get(&id)?;
    tree.require(&p, &w, &id, Access::Edit)?;
    let new: HashSet<String> = input.team_ids.into_iter().collect();
    for t in &new {
        if !w.teams.contains_key(t) {
            return Err(ApiError::unprocessable("unknown_team", format!("team {t} does not exist")).with("team", t));
        }
    }
    let mut tx = s.db.begin().await?;
    sqlx::query("DELETE FROM media_folder_grants WHERE folder_id = ?").bind(&id).execute(&mut *tx).await?;
    for t in &new {
        sqlx::query("INSERT INTO media_folder_grants (folder_id, team_id) VALUES (?, ?)").bind(&id).bind(t).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    library::changed(&s, None);
    let tree = Tree::load(&s).await?;
    Ok(Json(folder_json(&tree, &id, tree.access(&p, &w, &id))))
}

pub async fn usage(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    super::assignments::require_user(&p)?;
    let (w, tree) = (World::load(&s).await?, Tree::load(&s).await?);
    tree.get(&id)?;
    tree.require(&p, &w, &id, Access::View)?;
    let ids: Vec<(String,)> = sqlx::query_as("SELECT id FROM media WHERE folder_id IN (SELECT value FROM json_each(?))")
        .bind(serde_json::to_string(&tree.subtree(&id))?)
        .fetch_all(&s.db)
        .await?;
    let ids: Vec<String> = ids.into_iter().map(|(i,)| i).collect();
    Ok(Json(library::usage(&s, &p, &ids).await?))
}
