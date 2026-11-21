use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiResult},
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
        team_id: row.team_id,
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

#[derive(Deserialize)]
pub struct ListQuery {
    team_id: Option<String>,
}

pub async fn list(State(s): State<AppState>, p: Principal, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Node>>> {
    let rows: Vec<NodeRow> = sqlx::query_as(&format!(
        "SELECT {NODE_COLUMNS} FROM nodes WHERE (?1 IS NULL OR team_id = ?1) ORDER BY name"
    ))
    .bind(&q.team_id)
    .fetch_all(&s.db)
    .await?;
    let mut out = vec![];
    for row in rows.into_iter().filter(|r| p.can_view(&r.team_id)) {
        out.push(to_node(&s, row).await?);
    }
    Ok(Json(out))
}

pub async fn get(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<Json<Node>> {
    let row = fetch_row(&s, &id).await?;
    p.require(&row.team_id, Role::Viewer)?;
    Ok(Json(to_node(&s, row).await?))
}
