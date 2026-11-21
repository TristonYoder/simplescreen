use std::collections::HashSet;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{device::device_node, nodes};
use crate::{
    assignments::{self, Assignment, AssignmentsInput, Job, Mode},
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    models::{NodeRow, Output, Screen, ScreenCommand},
    state::AppState,
};

async fn device_row(s: &AppState, p: &Principal) -> ApiResult<NodeRow> {
    nodes::fetch_row(s, device_node(p)?).await.map_err(|_| ApiError::unauthorized())
}

// MARK: Device

pub async fn device_get(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    let node = device_row(&s, &p).await?;
    Ok(Json(assignments::body(&s, &node).await?))
}

pub async fn device_put(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(input): ApiJson<AssignmentsInput>,
) -> ApiResult<Json<Value>> {
    let node = device_row(&s, &p).await?;
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

#[derive(Serialize, sqlx::FromRow)]
struct TeamScreenRow {
    id: String,
    name: String,
    node_id: Option<String>,
    node_name: Option<String>,
    node_mode: Option<String>,
    output_key: Option<String>,
}

#[derive(Serialize)]
pub struct TeamScreen {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_mode: Option<String>,
    online: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_key: Option<String>,
    placed: bool,
}

/// Every screen in the device's team: what the control and confidence pickers list.
pub async fn team_screens(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<TeamScreen>>> {
    let node = device_row(&s, &p).await?;
    let rows: Vec<TeamScreenRow> = sqlx::query_as(
        "SELECT s.id, s.name, s.node_id, n.name AS node_name, n.mode AS node_mode, s.output_key \
         FROM screens s LEFT JOIN nodes n ON n.id = s.node_id WHERE s.team_id = ? ORDER BY s.name, s.id",
    )
    .bind(&node.team_id)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| TeamScreen {
                online: r.node_id.as_deref().is_some_and(|n| s.hub.is_online(n)),
                placed: r.node_id.is_some(),
                id: r.id,
                name: r.name,
                node_id: r.node_id,
                node_name: r.node_name,
                node_mode: r.node_mode,
                output_key: r.output_key,
            })
            .collect(),
    ))
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
    let node = nodes::fetch_row(&s, &id).await?;
    p.require(&node.team_id, Role::Viewer)?;
    Ok(Json(assignments::body(&s, &node).await?))
}

pub async fn node_put(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<AssignmentsInput>,
) -> ApiResult<Json<Value>> {
    let node = nodes::fetch_row(&s, &id).await?;
    p.require(&node.team_id, Role::Editor)?;
    let by = p.subject.clone();
    Ok(Json(assignments::replace(&s, node, input, &by, false).await?))
}

#[derive(Deserialize)]
pub struct MoveInput {
    node_id: String,
    output_key: String,
}

/// Makes `output_key` of `node_id` play the screen. The screen's old output goes to off and the screen's
/// audio and format settings move with it. The old node's mirrors of the screen are switched off.
pub async fn move_screen(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<MoveInput>,
) -> ApiResult<Json<Screen>> {
    let screen = super::screens::fetch_row(&s, &id).await?;
    p.require(&screen.team_id, Role::Editor)?;
    let target = nodes::fetch_row(&s, &input.node_id).await.map_err(|_| ApiError::unprocessable("target_not_in_team", "node does not exist"))?;
    if target.team_id != screen.team_id {
        return Err(ApiError::unprocessable("target_not_in_team", "node is not in the screen's team"));
    }
    if Mode::parse(&target.mode) != Mode::Player {
        return Err(ApiError::unprocessable("job_not_allowed_in_mode", "only player devices can play screens"));
    }
    let outputs: Vec<Output> = nodes::outputs_of(&target);
    if !outputs.iter().any(|o| o.key == input.output_key) {
        return Err(ApiError::unprocessable("unknown_output", "output_key is not in the node's reported outputs"));
    }
    let old = screen.node_id.clone().zip(screen.output_key.clone());
    if old.as_ref().is_some_and(|(n, k)| *n == target.id && *k == input.output_key) {
        return Ok(Json(super::screens::to_screen(&s, screen)));
    }

    let by = p.subject.clone();
    let mut target_set = assignments::load(&s, &target.id).await?;
    // An unplaced screen starts from empty settings. It must not inherit the output it displaces.
    let mut settings = assignments::AssignmentSettings::default();
    let mut old_node: Option<(NodeRow, Vec<Assignment>)> = None;
    if let Some((old_node_id, old_key)) = &old {
        let old_row = nodes::fetch_row(&s, old_node_id).await?;
        let mut set = if *old_node_id == target.id { std::mem::take(&mut target_set) } else { assignments::load(&s, old_node_id).await? };
        if let Some(a) = set.iter_mut().find(|a| a.output_key == *old_key) {
            settings = std::mem::take(&mut a.settings);
            *a = Assignment::off(old_key);
        }
        if *old_node_id == target.id {
            target_set = set;
        } else {
            old_node = Some((old_row, set));
        }
    }
    let new = Assignment {
        output_key: input.output_key.clone(),
        job: Job::Screen,
        screen_id: Some(id.clone()),
        settings,
        updated_by: None,
        updated_at: None,
    };
    match target_set.iter_mut().find(|a| a.output_key == input.output_key) {
        Some(a) => *a = new,
        None => target_set.push(new),
    }
    assignments::drop_orphan_mirrors(&mut target_set);
    if let Some((_, set)) = &mut old_node {
        assignments::drop_orphan_mirrors(set);
    }
    let releasing: HashSet<&str> = if old_node.is_some() { HashSet::from([id.as_str()]) } else { HashSet::new() };
    let ignore_old: Vec<&str> = old_node.iter().map(|(row, _)| row.id.as_str()).collect();
    assignments::validate_move(&s, &target, &target_set, &ignore_old, &releasing).await?;
    if let Some((row, set)) = &old_node {
        assignments::validate(&s, row, set).await?;
    }

    let target_version = target.assignments_version;
    let old_version = old_node.as_ref().map(|(row, _)| (row.id.clone(), row.assignments_version));
    let place = (screen.node_id.clone(), screen.output_key.clone());
    let mut tx = s.db.begin_with("BEGIN IMMEDIATE").await?;
    let fresh: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT node_id, output_key FROM screens WHERE id = ?").bind(&id).fetch_optional(&mut *tx).await?;
    let Some(fresh) = fresh else {
        drop(tx);
        return Err(ApiError::not_found());
    };
    if fresh != place {
        drop(tx);
        return Err(assignments::version_conflict(assignments::body(&s, &target).await?));
    }
    let current: (i64,) = sqlx::query_as("SELECT assignments_version FROM nodes WHERE id = ?").bind(&target.id).fetch_one(&mut *tx).await?;
    if current.0 != target_version {
        drop(tx);
        return Err(assignments::version_conflict(assignments::body(&s, &target).await?));
    }
    if let Some((old_id, old_ver)) = &old_version {
        let current: (i64,) = sqlx::query_as("SELECT assignments_version FROM nodes WHERE id = ?").bind(old_id).fetch_one(&mut *tx).await?;
        if current.0 != *old_ver {
            drop(tx);
            return Err(assignments::version_conflict(assignments::body(&s, &target).await?));
        }
    }
    let old_applied = if let Some((row, set)) = &old_node {
        match assignments::apply_set(&mut tx, row, set, &by, false).await {
            Err(e) if e.code == "version_conflict" => {
                drop(tx);
                return Err(assignments::version_conflict(assignments::body(&s, &target).await?));
            }
            other => Some(other?),
        }
    } else {
        None
    };
    let applied = match assignments::apply_set(&mut tx, &target, &target_set, &by, false).await {
        Err(e) if e.code == "version_conflict" => {
            drop(tx);
            return Err(assignments::version_conflict(assignments::body(&s, &target).await?));
        }
        other => other?,
    };
    tx.commit().await?;
    if let (Some((row, _)), Some(a)) = (&old_node, &old_applied) {
        assignments::announce(&s, row, a).await;
    }
    assignments::announce(&s, &target, &applied).await;
    Ok(Json(super::screens::to_screen(&s, super::screens::fetch_row(&s, &id).await?)))
}
