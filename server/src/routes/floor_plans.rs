use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;

use crate::{
    auth::Principal,
    error::{ApiError, ApiJson, ApiResult},
    state::AppState,
    util::{is_sha256, new_id, now},
};

#[derive(Serialize, FromRow)]
pub struct FloorPlan {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_height: Option<i64>,
    position: i64,
    created_at: String,
}

const COLUMNS: &str = "id, name, image_sha256, image_width, image_height, position, created_at";

#[derive(Deserialize)]
pub struct FloorPlanInput {
    name: String,
    #[serde(default)]
    image_sha256: Option<String>,
    #[serde(default)]
    image_width: Option<i64>,
    #[serde(default)]
    image_height: Option<i64>,
    #[serde(default)]
    position: Option<i64>,
}

fn validate(input: &mut FloorPlanInput) -> ApiResult<()> {
    input.name = input.name.trim().to_string();
    if input.name.is_empty() {
        return Err(ApiError::bad_request("name must not be empty").because("name_empty"));
    }
    if input.image_sha256.as_deref().is_some_and(|h| !is_sha256(h)) {
        return Err(ApiError::bad_request("image_sha256 must be 64 lowercase hex characters").because("sha256_invalid"));
    }
    if input.image_width.is_some_and(|w| w < 1) || input.image_height.is_some_and(|h| h < 1) {
        return Err(ApiError::bad_request("image size must be positive").because("image_size_invalid"));
    }
    Ok(())
}

async fn check_blob(s: &AppState, input: &FloorPlanInput) -> ApiResult<()> {
    if let Some(sha) = &input.image_sha256 {
        if tokio::fs::metadata(s.blob_path(sha)).await.is_err() {
            return Err(ApiError::unprocessable("blob_missing", "upload the image blob first").because("image"));
        }
    }
    Ok(())
}

async fn fetch(s: &AppState, id: &str) -> ApiResult<FloorPlan> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM floor_plans WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?
        .ok_or_else(ApiError::not_found)
}

fn broadcast(s: &AppState) {
    s.hub.broadcast_all(&json!({ "type": "floor_plans_changed" }));
}

pub async fn list(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Vec<FloorPlan>>> {
    super::assignments::require_user(&p)?;
    let rows = sqlx::query_as(&format!("SELECT {COLUMNS} FROM floor_plans ORDER BY position, created_at"))
        .fetch_all(&s.db)
        .await?;
    Ok(Json(rows))
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(mut input): ApiJson<FloorPlanInput>,
) -> ApiResult<(StatusCode, Json<FloorPlan>)> {
    p.require_admin()?;
    validate(&mut input)?;
    check_blob(&s, &input).await?;
    let position = match input.position {
        Some(n) => n,
        None => {
            let (max,): (Option<i64>,) = sqlx::query_as("SELECT MAX(position) FROM floor_plans").fetch_one(&s.db).await?;
            max.map_or(0, |m| m + 1)
        }
    };
    let id = new_id();
    sqlx::query(
        "INSERT INTO floor_plans (id, name, image_sha256, image_width, image_height, position, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.name)
    .bind(&input.image_sha256)
    .bind(input.image_width)
    .bind(input.image_height)
    .bind(position)
    .bind(now())
    .execute(&s.db)
    .await?;
    broadcast(&s);
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

pub async fn update(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(mut input): ApiJson<FloorPlanInput>,
) -> ApiResult<Json<FloorPlan>> {
    let existing = fetch(&s, &id).await?;
    p.require_admin()?;
    validate(&mut input)?;
    check_blob(&s, &input).await?;
    sqlx::query("UPDATE floor_plans SET name = ?, image_sha256 = ?, image_width = ?, image_height = ?, position = ? WHERE id = ?")
        .bind(&input.name)
        .bind(&input.image_sha256)
        .bind(input.image_width)
        .bind(input.image_height)
        .bind(input.position.unwrap_or(existing.position))
        .bind(&id)
        .execute(&s.db)
        .await?;
    broadcast(&s);
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    p.require_admin()?;
    fetch(&s, &id).await?;
    let mut tx = s.db.begin().await?;
    sqlx::query("UPDATE screens SET placement_plan_id = NULL, placement_x = NULL, placement_y = NULL WHERE placement_plan_id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM floor_plans WHERE id = ?").bind(&id).execute(&mut *tx).await?;
    tx.commit().await?;
    broadcast(&s);
    Ok(StatusCode::NO_CONTENT)
}
