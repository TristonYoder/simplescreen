use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::Value;

use super::{device::device_node, nodes};
use crate::{
    assignments::{self, Assignment, AssignmentsInput, Job, Mode},
    auth::Principal,
    error::{ApiError, ApiJson, ApiResult},
    models::{NodeRow, Output, Screen, ScreenCommand},
    reach::World,
    state::AppState,
};

async fn device_row(s: &AppState, p: &Principal) -> ApiResult<NodeRow> {
    nodes::fetch_row(s, device_node(p)?).await.map_err(|_| ApiError::unauthorized())
}

pub(super) fn require_user(p: &Principal) -> ApiResult<()> {
    if p.is_device() {
        Err(ApiError::forbidden())
    } else {
        Ok(())
    }
}

// MARK: Device

pub async fn device_get(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    let node = device_row(&s, &p).await?;
    Ok(Json(assignments::body(&s, &node).await?))
}

/// A change made on the device can only rearrange the screens the node is already assigned to.
pub async fn device_put(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<AssignmentsInput>,
) -> ApiResult<Json<Value>> {
    let node = device_row(&s, &p).await?;
    if input.base_version == node.assignments_version {
        let allowed = assignments::assigned_screens(&s, &node.id).await?;
        let foreign = input.assignments.iter().filter(|a| a.job != Job::Off).filter_map(|a| a.screen_id.as_ref()).any(|id| !allowed.contains(id));
        if foreign {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "target_not_assigned",
                "a device can only use screens it is already assigned to; assign new screens in Studio",
            ));
        }
    }
    let by = node.id.clone();
    Ok(Json(assignments::replace(&s, node, input, &by, true).await?))
}

#[derive(Deserialize)]
pub struct ModeInput {
    mode: Mode,
}

pub async fn device_mode(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<ModeInput>,
) -> ApiResult<Json<Value>> {
    let node = device_row(&s, &p).await?;
    let by = node.id.clone();
    let id = node.id.clone();
    assignments::set_mode(&s, node, input.mode, &by, true).await?;
    let fresh = nodes::fetch_row(&s, &id).await?;
    Ok(Json(assignments::body(&s, &fresh).await?))
}

/// Remote control: a node may command a screen only if it has a control assignment for it.
pub async fn device_command(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(cmd): ApiJson<ScreenCommand>,
) -> ApiResult<StatusCode> {
    let node_id = device_node(&p)?;
    if !assignments::targets(&s, node_id, &id, false).await? {
        return Err(ApiError::forbidden());
    }
    let row = super::screens::fetch_row(&s, &id).await?;
    super::screens::send_command(&s, &row, &cmd)
}

// MARK: Editor

pub async fn node_get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Value>> {
    require_user(&p)?;
    let node = nodes::fetch_row(&s, &id).await?;
    Ok(Json(assignments::body(&s, &node).await?))
}

/// Each changed output needs the caller to operate every screen it touches; one touching none is admin only.
fn check_changes(p: &Principal, w: &World, old: &[Assignment], new: &[Assignment]) -> ApiResult<()> {
    for (_, screens) in assignments::changes(old, new) {
        if screens.is_empty() {
            p.require_admin()?;
        }
        for screen in &screens {
            if w.screen_tags.contains_key(screen) && !p.operates_screen(w, screen) {
                return Err(ApiError::forbidden());
            }
        }
    }
    Ok(())
}

pub async fn node_put(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<AssignmentsInput>,
) -> ApiResult<Json<Value>> {
    require_user(&p)?;
    let node = nodes::fetch_row(&s, &id).await?;
    if input.base_version == node.assignments_version {
        let w = World::load(&s).await?;
        let current = assignments::load(&s, &node.id).await?;
        check_changes(&p, &w, &current, &input.assignments)?;
    }
    let by = p.subject.clone();
    Ok(Json(assignments::replace(&s, node, input, &by, false).await?))
}

#[derive(Deserialize)]
pub struct MoveInput {
    node_id: String,
    output_key: String,
}

/// Makes `output_key` of `node_id` the screen's only program output. Its old program outputs go to off,
/// and the first one's settings (the audio one if any) move with it.
pub async fn move_screen(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<MoveInput>,
) -> ApiResult<Json<Screen>> {
    let w = World::load(&s).await?;
    let screen = super::screens::fetch_row(&s, &id).await?;
    p.require_operate(&w, &id)?;
    let target = nodes::fetch_row(&s, &input.node_id).await.map_err(|_| ApiError::unprocessable("unknown_node", "node does not exist"))?;
    if Mode::parse(&target.mode) != Mode::Player {
        return Err(ApiError::unprocessable("job_not_allowed_in_mode", "only player devices can play screens").because("screens_need_player"));
    }
    let outputs: Vec<Output> = nodes::outputs_of(&target);
    if !outputs.iter().any(|o| o.key == input.output_key) {
        return Err(ApiError::unprocessable("unknown_output", "output_key is not in the node's reported outputs"));
    }
    let old_node_id = screen.node_id.clone();
    let mut target_set = assignments::load(&s, &target.id).await?;
    let is_program = |a: &Assignment| a.job == Job::Program && a.screen_id.as_deref() == Some(id.as_str());
    if old_node_id.as_deref() == Some(target.id.as_str())
        && target_set.iter().filter(|a| is_program(a)).map(|a| a.output_key.as_str()).eq([input.output_key.as_str()])
    {
        return Ok(Json(super::screens::to_screen(&s, screen).await?));
    }
    if let Some(displaced) = target_set.iter().find(|a| a.output_key == input.output_key && a.job != Job::Off).and_then(|a| a.screen_id.clone()) {
        if displaced != id && !p.operates_screen(&w, &displaced) {
            return Err(ApiError::forbidden());
        }
    }

    let by = p.subject.clone();
    // An unplaced screen starts from empty settings. It must not inherit the output it displaces.
    let mut settings = assignments::AssignmentSettings::default();
    let mut take = |set: &mut Vec<Assignment>| {
        let mut picked: Option<assignments::AssignmentSettings> = None;
        for a in set.iter_mut().filter(|a| is_program(a)) {
            let st = std::mem::take(&mut a.settings);
            let audio = st.audio.as_ref().is_some_and(|x| x.enabled);
            if picked.is_none() || audio {
                picked = Some(st);
            }
            let key = a.output_key.clone();
            *a = Assignment::off(&key);
        }
        if let Some(st) = picked {
            settings = st;
        }
    };
    let mut old_node: Option<(NodeRow, Vec<Assignment>)> = None;
    match &old_node_id {
        Some(old) if *old == target.id => take(&mut target_set),
        Some(old) => {
            let row = nodes::fetch_row(&s, old).await?;
            let mut set = assignments::load(&s, old).await?;
            take(&mut set);
            old_node = Some((row, set));
        }
        None => {}
    }
    let new = Assignment {
        output_key: input.output_key.clone(),
        job: Job::Program,
        screen_id: Some(id.clone()),
        settings,
        updated_by: None,
        updated_at: None,
    };
    match target_set.iter_mut().find(|a| a.output_key == input.output_key) {
        Some(a) => *a = new,
        None => target_set.push(new),
    }
    let releasing: HashSet<&str> = if old_node.is_some() { HashSet::from([id.as_str()]) } else { HashSet::new() };
    let ignore_old: Vec<&str> = old_node.iter().map(|(row, _)| row.id.as_str()).collect();
    assignments::validate_move(&s, &target, &target_set, &ignore_old, &releasing).await?;
    if let Some((row, set)) = &old_node {
        assignments::validate(&s, row, set).await?;
    }

    let target_version = target.assignments_version;
    let old_version = old_node.as_ref().map(|(row, _)| (row.id.clone(), row.assignments_version));
    let mut tx = s.db.begin_with("BEGIN IMMEDIATE").await?;
    let fresh: Option<(Option<String>,)> = sqlx::query_as("SELECT node_id FROM screens WHERE id = ?").bind(&id).fetch_optional(&mut *tx).await?;
    let Some((fresh,)) = fresh else {
        drop(tx);
        return Err(ApiError::not_found());
    };
    if fresh != old_node_id {
        { drop(tx); return Err(stale(&s, &target).await) };
    }
    let current: (i64,) = sqlx::query_as("SELECT assignments_version FROM nodes WHERE id = ?").bind(&target.id).fetch_one(&mut *tx).await?;
    if current.0 != target_version {
        { drop(tx); return Err(stale(&s, &target).await) };
    }
    if let Some((old_id, old_ver)) = &old_version {
        let current: (i64,) = sqlx::query_as("SELECT assignments_version FROM nodes WHERE id = ?").bind(old_id).fetch_one(&mut *tx).await?;
        if current.0 != *old_ver {
            { drop(tx); return Err(stale(&s, &target).await) };
        }
    }
    let old_applied = if let Some((row, set)) = &old_node {
        match assignments::apply_set(&mut tx, row, set, &by, false).await {
            Err(e) if e.code == "version_conflict" => {
                drop(tx);
                return Err(stale(&s, &target).await);
            }
            other => Some(other?),
        }
    } else {
        None
    };
    let applied = match assignments::apply_set(&mut tx, &target, &target_set, &by, false).await {
        Err(e) if e.code == "version_conflict" => {
                drop(tx);
                return Err(stale(&s, &target).await);
            }
        other => other?,
    };
    tx.commit().await?;
    if let (Some((row, _)), Some(a)) = (&old_node, &old_applied) {
        assignments::announce(&s, row, a).await;
    }
    assignments::announce(&s, &target, &applied).await;
    Ok(Json(super::screens::to_screen(&s, super::screens::fetch_row(&s, &id).await?).await?))
}

async fn stale(s: &AppState, target: &NodeRow) -> ApiError {
    match assignments::body(s, target).await {
        Ok(body) => assignments::version_conflict(body),
        Err(e) => e,
    }
}
