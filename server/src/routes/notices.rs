use crate::error::ApiQuery;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    auth::Principal,
    error::{ApiError, ApiResult},
    notices::{Notice, NOTICE_COLUMNS},
    state::AppState,
    util::now,
};

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    all: bool,
}

pub async fn list(State(s): State<AppState>, p: Principal, ApiQuery(q): ApiQuery<ListQuery>) -> ApiResult<Json<Vec<Value>>> {
    super::assignments::require_user(&p)?;
    let rows: Vec<Notice> = sqlx::query_as(&format!(
        "SELECT {NOTICE_COLUMNS} FROM notices WHERE (user_sub = ?1 OR (user_sub IS NULL AND ?2)) \
         AND (?3 OR (resolved_at IS NULL AND dismissed_at IS NULL)) ORDER BY created_at DESC, id"
    ))
    .bind(&p.subject)
    .bind(p.server_admin)
    .bind(q.all)
    .fetch_all(&s.db)
    .await?;
    Ok(Json(rows.iter().map(Notice::to_json).collect()))
}

pub async fn dismiss(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    super::assignments::require_user(&p)?;
    let res = sqlx::query(
        "UPDATE notices SET dismissed_at = COALESCE(dismissed_at, ?) WHERE id = ? AND (user_sub = ? OR (user_sub IS NULL AND ?))",
    )
    .bind(now())
    .bind(&id)
    .bind(&p.subject)
    .bind(p.server_admin)
    .execute(&s.db)
    .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}
