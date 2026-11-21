use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::{
    assignments::Mode,
    auth::Principal,
    error::{ApiError, ApiJson, ApiResult},
    models::{Node, NodeRow, Output, NODE_COLUMNS},
    state::AppState,
};

pub async fn fetch_row(s: &AppState, id: &str) -> ApiResult<NodeRow> {
    sqlx::query_as(&format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(ApiError::not_found)
}

pub fn outputs_of(row: &NodeRow) -> Vec<Output> {
    serde_json::from_str(&row.outputs).unwrap_or_default()
}

pub async fn screen_ids(s: &AppState, node_id: &str) -> ApiResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM screens WHERE node_id = ? ORDER BY name, id")
        .bind(node_id)
        .fetch_all(&s.db)
        .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

pub async fn to_node(s: &AppState, row: NodeRow) -> ApiResult<Node> {
    let screen_ids = screen_ids(s, &row.id).await?;
    let outputs = outputs_of(&row);
    Ok(Node {
        online: s.hub.is_online(&row.id),
        status: s.node_statuses.lock().get(&row.id).cloned(),
        id: row.id,
        name: row.name,
        model: row.model,
        os_version: row.os_version,
        app_version: row.app_version,
        outputs,
        screen_ids,
        mode: row.mode,
        assignments_version: row.assignments_version,
        applied_assignments_version: row.applied_assignments_version,
        last_seen_at: row.last_seen_at,
        created_at: row.created_at,
    })
}

pub async fn list(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<Node>>> {
    super::assignments::require_user(&p)?;
    let rows: Vec<NodeRow> = sqlx::query_as(&format!("SELECT {NODE_COLUMNS} FROM nodes ORDER BY name")).fetch_all(&s.db).await?;
    let mut out = vec![];
    for row in rows {
        out.push(to_node(&s, row).await?);
    }
    Ok(Json(out))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Node>> {
    super::assignments::require_user(&p)?;
    let row = fetch_row(&s, &id).await?;
    Ok(Json(to_node(&s, row).await?))
}

#[derive(Deserialize)]
pub struct NodePatch {
    name: Option<String>,
    mode: Option<Mode>,
}

/// Node edits that touch no screen are admin only.
pub async fn patch(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<NodePatch>,
) -> ApiResult<Json<Node>> {
    p.require_admin()?;
    let row = fetch_row(&s, &id).await?;
    if let Some(name) = &input.name {
        if name.trim().is_empty() {
            return Err(ApiError::bad_request("name must not be empty"));
        }
        sqlx::query("UPDATE nodes SET name = ? WHERE id = ?").bind(name.trim()).bind(&id).execute(&s.db).await?;
    }
    if let Some(mode) = input.mode.filter(|m| *m != Mode::parse(&row.mode)) {
        let by = p.subject.clone();
        crate::assignments::set_mode(&s, row, mode, &by, false).await?;
    } else {
        s.hub.broadcast_all(&json!({ "type": "node_updated", "node_id": id }));
    }
    Ok(Json(to_node(&s, fetch_row(&s, &id).await?).await?))
}
