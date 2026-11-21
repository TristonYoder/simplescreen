//! Per-output jobs (assignments), device modes, validation and the side effects of changing either.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, Sqlite, Transaction};

use crate::{
    error::{ApiError, ApiResult},
    models::{NodeRow, Output, OutputKind, OutputRole},
    state::AppState,
    util::now,
};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Player,
    Controller,
    Stage,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Player => "player",
            Mode::Controller => "controller",
            Mode::Stage => "stage",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "controller" => Mode::Controller,
            "stage" => Mode::Stage,
            _ => Mode::Player,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Job {
    Screen,
    Mirror,
    Control,
    Confidence,
    Off,
}

impl Job {
    fn as_str(self) -> &'static str {
        match self {
            Job::Screen => "screen",
            Job::Mirror => "mirror",
            Job::Control => "control",
            Job::Confidence => "confidence",
            Job::Off => "off",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "screen" => Job::Screen,
            "mirror" => Job::Mirror,
            "control" => Job::Control,
            "confidence" => Job::Confidence,
            _ => Job::Off,
        }
    }

    fn role(self) -> OutputRole {
        match self {
            Job::Screen => OutputRole::Screen,
            Job::Mirror => OutputRole::Mirror,
            Job::Control => OutputRole::Control,
            Job::Confidence => OutputRole::Confidence,
            Job::Off => OutputRole::None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AudioSettings {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Remote,
    Operator,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AssignmentSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<View>,
    /// 12 or 24.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

impl AssignmentSettings {
    fn audio_on(&self) -> bool {
        self.audio.as_ref().is_some_and(|a| a.enabled)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Assignment {
    pub output_key: String,
    pub job: Job,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_id: Option<String>,
    #[serde(default)]
    pub settings: AssignmentSettings,
    /// Server-set; ignored on input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Assignment {
    pub fn off(output_key: &str) -> Self {
        Self {
            output_key: output_key.to_string(),
            job: Job::Off,
            screen_id: None,
            settings: AssignmentSettings::default(),
            updated_by: None,
            updated_at: None,
        }
    }
}

#[derive(Deserialize)]
pub struct AssignmentsInput {
    pub base_version: i64,
    pub assignments: Vec<Assignment>,
}

#[derive(FromRow)]
struct AssignmentRow {
    output_key: String,
    job: String,
    screen_id: Option<String>,
    settings: String,
    updated_by: Option<String>,
    updated_at: String,
}

fn to_assignment(r: AssignmentRow) -> Assignment {
    Assignment {
        output_key: r.output_key,
        job: Job::parse(&r.job),
        screen_id: r.screen_id,
        settings: serde_json::from_str(&r.settings).unwrap_or_default(),
        updated_by: r.updated_by,
        updated_at: Some(r.updated_at),
    }
}

const COLUMNS: &str = "output_key, job, screen_id, settings, updated_by, updated_at";

pub async fn load(s: &AppState, node_id: &str) -> ApiResult<Vec<Assignment>> {
    let rows: Vec<AssignmentRow> =
        sqlx::query_as(&format!("SELECT {COLUMNS} FROM assignments WHERE node_id = ? ORDER BY output_key")).bind(node_id).fetch_all(&s.db).await?;
    Ok(rows.into_iter().map(to_assignment).collect())
}

pub async fn body(s: &AppState, node: &NodeRow) -> ApiResult<serde_json::Value> {
    let fresh = crate::routes::nodes::fetch_row(s, &node.id).await?;
    Ok(json!({
        "version": fresh.assignments_version,
        "mode": fresh.mode,
        "assignments": load(s, &node.id).await?,
    }))
}

/// Does `node_id` hold a control assignment targeting `screen_id`? (`read` also accepts confidence.)
pub async fn targets(s: &AppState, node_id: &str, screen_id: &str, read: bool) -> ApiResult<bool> {
    let jobs = if read { "('control', 'confidence')" } else { "('control')" };
    let found: Option<(i64,)> = sqlx::query_as(&format!(
        "SELECT 1 FROM assignments WHERE node_id = ? AND screen_id = ? AND job IN {jobs} LIMIT 1"
    ))
    .bind(node_id)
    .bind(screen_id)
    .fetch_optional(&s.db)
    .await?;
    Ok(found.is_some())
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::unprocessable("invalid_assignments", msg)
}

/// The rules shared by the device and editor endpoints. `set` is the complete new assignment list.
pub async fn validate(s: &AppState, node: &NodeRow, set: &[Assignment]) -> ApiResult<()> {
    check(s, node, set, &[], &HashSet::new()).await
}

/// Like `validate`, for a move that releases `releasing` and replaces `ignore_nodes` in the same transaction.
pub async fn validate_move(
    s: &AppState,
    node: &NodeRow,
    set: &[Assignment],
    ignore_nodes: &[&str],
    releasing: &HashSet<&str>,
) -> ApiResult<()> {
    check(s, node, set, ignore_nodes, releasing).await
}

async fn check(
    s: &AppState,
    node: &NodeRow,
    set: &[Assignment],
    ignore_nodes: &[&str],
    releasing: &HashSet<&str>,
) -> ApiResult<()> {
    let mode = Mode::parse(&node.mode);
    let outputs = crate::routes::nodes::outputs_of(node);
    let in_set_screen: HashSet<&str> =
        set.iter().filter(|a| a.job == Job::Screen).filter_map(|a| a.screen_id.as_deref()).collect();

    let mut keys = HashSet::new();
    let mut screen_jobs = HashSet::new();
    for a in set {
        if a.output_key.trim().is_empty() {
            return Err(invalid("output_key must not be empty"));
        }
        if !keys.insert(a.output_key.as_str()) {
            return Err(invalid(format!("duplicate assignment for output {}", a.output_key)));
        }
        if a.settings.clock.is_some_and(|c| c != 12 && c != 24) {
            return Err(invalid("clock must be 12 or 24"));
        }
        if mode != Mode::Player && !matches!(a.job, Job::Control | Job::Confidence | Job::Off) {
            return Err(ApiError::unprocessable(
                "job_not_allowed_in_mode",
                format!("{} devices can only use control, confidence or off", mode.as_str()),
            ));
        }
        if a.job == Job::Off {
            continue;
        }
        let Some(output) = outputs.iter().find(|o| o.key == a.output_key) else {
            return Err(ApiError::unprocessable("unknown_output", format!("output {} is not in the node's reported outputs", a.output_key)));
        };
        if a.job == Job::Control && matches!(output.kind, OutputKind::Decklink | OutputKind::Virtual) {
            return Err(ApiError::unprocessable("control_needs_input", format!("output {} cannot take input", output.key)));
        }
        let Some(screen_id) = a.screen_id.as_deref() else {
            if a.job == Job::Screen {
                continue; // the device creates the screen through POST /v1/device/screens
            }
            return Err(ApiError::unprocessable("screen_required", "screen_id is required for this job"));
        };
        let row: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT team_id, node_id FROM screens WHERE id = ?").bind(screen_id).fetch_optional(&s.db).await?;
        let Some((team_id, owner)) = row.filter(|(g, _)| *g == node.team_id) else {
            return Err(ApiError::unprocessable("target_not_in_team", "screen does not exist in the node's team"));
        };
        let local = owner.as_deref() == Some(node.id.as_str());
        match a.job {
            // A mirror follows a screen job in this set. Owning the screen in the database is not enough:
            // dropping that job in the same request would leave a mirror of a screen this node no longer plays.
            Job::Mirror if !in_set_screen.contains(screen_id) => {
                return Err(ApiError::unprocessable("mirror_must_be_local", "a mirror must target a screen played by this node"));
            }
            Job::Screen => {
                if !screen_jobs.insert(screen_id) {
                    return Err(ApiError::unprocessable("screen_has_output", "a screen can only be played by one output"));
                }
                if owner.is_some() && !local && !releasing.contains(screen_id) {
                    return Err(ApiError::unprocessable("screen_must_be_local", "that screen is played by another node; move it instead"));
                }
            }
            _ => {}
        }
        let _ = team_id;
    }

    // One audio-enabled assignment per screen, across the whole team.
    let mut audio: HashMap<&str, usize> = HashMap::new();
    for a in set.iter().filter(|a| a.settings.audio_on()) {
        if let Some(id) = a.screen_id.as_deref().filter(|_| a.job != Job::Off) {
            *audio.entry(id).or_default() += 1;
        }
    }
    if audio.values().any(|n| *n > 1) {
        return Err(ApiError::unprocessable("audio_conflict", "a screen can play audio on one output only"));
    }
    if !audio.is_empty() {
        let others: Vec<(Option<String>, String, String)> = sqlx::query_as(
            "SELECT screen_id, settings, node_id FROM assignments WHERE node_id != ? AND screen_id IS NOT NULL AND job != 'off'",
        )
        .bind(&node.id)
        .fetch_all(&s.db)
        .await?;
        for (id, settings, other_node) in others {
            if ignore_nodes.contains(&other_node.as_str()) {
                continue;
            }
            let on = serde_json::from_str::<AssignmentSettings>(&settings).is_ok_and(|x| x.audio_on());
            if on && id.as_deref().is_some_and(|id| audio.contains_key(id)) {
                return Err(ApiError::unprocessable("audio_conflict", "another device already plays audio for that screen"));
            }
        }
    }
    Ok(())
}

/// Turns off mirrors whose screen has no screen job left in `set`.
pub fn drop_orphan_mirrors(set: &mut [Assignment]) {
    let played: HashSet<String> = set.iter().filter(|a| a.job == Job::Screen).filter_map(|a| a.screen_id.clone()).collect();
    for a in set.iter_mut() {
        if a.job == Job::Mirror && a.screen_id.as_ref().is_some_and(|id| !played.contains(id)) {
            let key = a.output_key.clone();
            *a = Assignment::off(&key);
        }
    }
}

/// Copies each output's job and screen onto the reported hardware. Client-sent role and screen_id are ignored.
pub fn overlay_outputs(outputs: &mut [Output], set: &[Assignment]) {
    for o in outputs {
        let a = set.iter().find(|a| a.output_key == o.key);
        o.role = a.map_or(OutputRole::None, |a| a.job.role());
        o.screen_id = a.filter(|a| a.job == Job::Screen).and_then(|a| a.screen_id.clone());
    }
}

pub struct Applied {
    pub version: i64,
    pub released: Vec<String>,
    pub claimed: Vec<String>,
}

/// Replaces a node's assignments inside `tx` and makes screen ownership follow the screen jobs.
/// Screens the node no longer plays become unplaced; they are never deleted.
pub async fn apply_set(
    tx: &mut Transaction<'_, Sqlite>,
    node: &NodeRow,
    set: &[Assignment],
    updated_by: &str,
    applied: bool,
) -> ApiResult<Applied> {
    let stamp = now();
    let before: Vec<(String,)> = sqlx::query_as("SELECT id FROM screens WHERE node_id = ?").bind(&node.id).fetch_all(&mut **tx).await?;
    sqlx::query("DELETE FROM assignments WHERE node_id = ?").bind(&node.id).execute(&mut **tx).await?;
    sqlx::query("UPDATE screens SET node_id = NULL, output_key = NULL WHERE node_id = ?").bind(&node.id).execute(&mut **tx).await?;
    let mut claimed = vec![];
    for a in set {
        let screen_id = (a.job != Job::Off).then_some(a.screen_id.as_deref()).flatten();
        sqlx::query(
            "INSERT INTO assignments (node_id, output_key, job, screen_id, settings, updated_by, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&node.id)
        .bind(&a.output_key)
        .bind(a.job.as_str())
        .bind(screen_id)
        .bind(serde_json::to_string(&a.settings)?)
        .bind(updated_by)
        .bind(&stamp)
        .execute(&mut **tx)
        .await?;
        if let (Job::Screen, Some(id)) = (a.job, screen_id) {
            // Only an unplaced screen, or one this node already released above, can be claimed.
            let res = sqlx::query(
                "UPDATE screens SET node_id = ?, output_key = ? WHERE id = ? AND (node_id IS NULL OR node_id = ?)",
            )
            .bind(&node.id)
            .bind(&a.output_key)
            .bind(id)
            .bind(&node.id)
            .execute(&mut **tx)
            .await?;
            if res.rows_affected() == 0 {
                return Err(ApiError::conflict("version_conflict", "assignments were modified; base_version is stale"));
            }
            claimed.push(id.to_string());
        }
    }
    let current: (String,) = sqlx::query_as("SELECT outputs FROM nodes WHERE id = ?").bind(&node.id).fetch_one(&mut **tx).await?;
    let mut outputs: Vec<Output> = serde_json::from_str(&current.0).unwrap_or_default();
    overlay_outputs(&mut outputs, set);
    let version: i64 = sqlx::query_scalar(
        "UPDATE nodes SET assignments_version = assignments_version + 1, outputs = ?, \
         applied_assignments_version = CASE WHEN ? THEN assignments_version + 1 ELSE applied_assignments_version END \
         WHERE id = ? RETURNING assignments_version",
    )
    .bind(serde_json::to_string(&outputs)?)
    .bind(applied)
    .bind(&node.id)
    .fetch_one(&mut **tx)
    .await?;
    let released = before.into_iter().map(|(id,)| id).filter(|id| !claimed.contains(id)).collect();
    Ok(Applied { version, released, claimed })
}

/// Pushes the consequences of a committed change to the device, Studio and watching nodes.
pub async fn announce(s: &AppState, node: &NodeRow, applied: &Applied) {
    s.hub.send_node(&node.id, &json!({ "type": "assignments_changed", "version": applied.version }));
    s.hub.broadcast_team(&node.team_id, &json!({ "type": "node_updated", "node_id": node.id }));
    for id in applied.released.iter().chain(&applied.claimed) {
        s.hub.broadcast_team(&node.team_id, &json!({ "type": "screen_updated", "screen_id": id }));
    }
    for id in &applied.released {
        let owner: Option<(Option<String>,)> =
            sqlx::query_as("SELECT node_id FROM screens WHERE id = ?").bind(id).fetch_optional(&s.db).await.ok().flatten();
        let unplaced = match owner {
            Some((node_id,)) => node_id.is_none(),
            None => true,
        };
        if unplaced {
            s.statuses.lock().remove(id);
            let msg = json!({ "type": "screen_offline", "screen_id": id });
            s.hub.broadcast_team(&node.team_id, &msg);
            s.hub.send_subscribers(id, Some(&node.id), &msg);
        }
    }
    for id in &applied.claimed {
        s.notify_manifest(Some(&node.id), id, 0);
    }
    refresh_subscriptions(s, &node.id).await;
}

/// Recomputes which screens a node watches and sends it their current status.
pub async fn refresh_subscriptions(s: &AppState, node_id: &str) {
    let rows: Vec<(String,)> = match sqlx::query_as(
        "SELECT DISTINCT screen_id FROM assignments WHERE node_id = ? AND job IN ('control', 'confidence') AND screen_id IS NOT NULL",
    )
    .bind(node_id)
    .fetch_all(&s.db)
    .await
    {
        Ok(rows) => rows,
        // Keep the watches the socket already has. An empty set would drop a live controller.
        Err(_) => return,
    };
    let set: HashSet<String> = rows.into_iter().map(|(id,)| id).collect();
    let Some(previous) = s.hub.set_subscriptions(node_id, set.clone()) else { return };
    for id in set.difference(&previous) {
        let status = s.statuses.lock().get(id).cloned();
        if let Some(status) = status {
            s.hub.send_node(node_id, &json!({ "type": "screen_status", "screen_id": id, "status": status }));
            continue;
        }
        let owner: Option<(Option<String>,)> =
            sqlx::query_as("SELECT node_id FROM screens WHERE id = ?").bind(id).fetch_optional(&s.db).await.ok().flatten();
        if owner.and_then(|(n,)| n).is_some_and(|n| s.hub.is_online(&n)) {
            s.hub.send_node(node_id, &json!({ "type": "screen_online", "screen_id": id }));
        }
    }
}

/// Rewrites `nodes.outputs` role and screen_id from the assignments stored in `tx`.
pub async fn sync_reported_outputs(tx: &mut Transaction<'_, Sqlite>, node_id: &str) -> ApiResult<()> {
    let current: (String,) = sqlx::query_as("SELECT outputs FROM nodes WHERE id = ?").bind(node_id).fetch_one(&mut **tx).await?;
    let mut outputs: Vec<Output> = serde_json::from_str(&current.0).unwrap_or_default();
    let rows: Vec<AssignmentRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM assignments WHERE node_id = ? ORDER BY output_key"))
        .bind(node_id)
        .fetch_all(&mut **tx)
        .await?;
    let set: Vec<Assignment> = rows.into_iter().map(to_assignment).collect();
    overlay_outputs(&mut outputs, &set);
    sqlx::query("UPDATE nodes SET outputs = ? WHERE id = ?")
        .bind(serde_json::to_string(&outputs)?)
        .bind(node_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub fn version_conflict(current: serde_json::Value) -> ApiError {
    let mut e = ApiError::conflict("version_conflict", "assignments were modified; base_version is stale");
    e.current = Some(current);
    e
}

/// Validated full replace, shared by the device and editor endpoints.
pub async fn replace(
    s: &AppState,
    node: NodeRow,
    mut input: AssignmentsInput,
    updated_by: &str,
    by_device: bool,
) -> ApiResult<serde_json::Value> {
    if input.base_version != node.assignments_version {
        return Err(version_conflict(body(s, &node).await?));
    }
    for a in &mut input.assignments {
        if a.job == Job::Off {
            a.screen_id = None;
        }
    }
    validate(s, &node, &input.assignments).await?;
    let mut tx = s.db.begin_with("BEGIN IMMEDIATE").await?;
    let current: (i64,) = sqlx::query_as("SELECT assignments_version FROM nodes WHERE id = ?").bind(&node.id).fetch_one(&mut *tx).await?;
    if current.0 != input.base_version {
        drop(tx);
        return Err(version_conflict(body(s, &node).await?));
    }
    let applied = match apply_set(&mut tx, &node, &input.assignments, updated_by, by_device).await {
        Err(e) if e.code == "version_conflict" => {
            drop(tx);
            return Err(version_conflict(body(s, &node).await?));
        }
        other => other?,
    };
    tx.commit().await?;
    announce(s, &node, &applied).await;
    body(s, &node).await
}

/// Changes what a device is for. Leaving player releases its screens (unplaced) and drops its screen and mirror jobs.
pub async fn set_mode(s: &AppState, node: NodeRow, mode: Mode, updated_by: &str, by_device: bool) -> ApiResult<()> {
    let version = node.assignments_version;
    let mut set = load(s, &node.id).await?;
    if mode != Mode::Player {
        set.retain(|a| matches!(a.job, Job::Control | Job::Confidence | Job::Off));
    }
    let mut tx = s.db.begin_with("BEGIN IMMEDIATE").await?;
    let current: (i64,) = sqlx::query_as("SELECT assignments_version FROM nodes WHERE id = ?").bind(&node.id).fetch_one(&mut *tx).await?;
    if current.0 != version {
        drop(tx);
        return Err(version_conflict(body(s, &node).await?));
    }
    sqlx::query("UPDATE nodes SET mode = ? WHERE id = ?").bind(mode.as_str()).bind(&node.id).execute(&mut *tx).await?;
    let applied = match apply_set(&mut tx, &node, &set, updated_by, by_device).await {
        Err(e) if e.code == "version_conflict" => {
            drop(tx);
            return Err(version_conflict(body(s, &node).await?));
        }
        other => other?,
    };
    tx.commit().await?;
    announce(s, &node, &applied).await;
    Ok(())
}
