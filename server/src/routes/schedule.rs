use std::collections::HashSet;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    models::{ScreenRow, SCREEN_COLUMNS},
    schedule::{self, invalid, Entry, EntryKind, EntryRow, Segment, ENTRY_COLUMNS},
    state::AppState,
    util::{new_id, now},
};

#[derive(Deserialize)]
pub struct EntryInput {
    group_id: String,
    playlist_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    screen_ids: Vec<String>,
    kind: EntryKind,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    days: Vec<u8>,
    start_time: Option<String>,
    end_time: Option<String>,
    starts_on: Option<String>,
    ends_on: Option<String>,
    #[serde(default)]
    skip_dates: Vec<String>,
    starts_at: Option<String>,
    ends_at: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub struct GroupQuery {
    group_id: String,
}

#[derive(Serialize)]
pub struct ScheduleList {
    timezone: String,
    entries: Vec<Entry>,
}

pub async fn list(State(s): State<AppState>, p: Principal, Query(q): Query<GroupQuery>) -> ApiResult<Json<ScheduleList>> {
    s.require_group(&p, &q.group_id, Role::Viewer).await?;
    let tz = schedule::group_tz(&s, &q.group_id).await?;
    Ok(Json(ScheduleList { timezone: tz.name().to_string(), entries: schedule::group_entries(&s, &q.group_id).await? }))
}

async fn fetch(s: &AppState, id: &str) -> ApiResult<Entry> {
    let row: Option<EntryRow> = sqlx::query_as(&format!("SELECT {ENTRY_COLUMNS} FROM schedule_entries WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?;
    Ok(row.ok_or_else(ApiError::not_found)?.into_entry())
}

/// Normalizes the input in place: trims the title, drops fields that don't belong to `kind`.
async fn validate(s: &AppState, input: &mut EntryInput) -> ApiResult<()> {
    if let Some(pid) = &input.playlist_id {
        let found: Option<(String,)> = sqlx::query_as("SELECT id FROM playlists WHERE id = ? AND group_id = ?")
            .bind(pid)
            .bind(&input.group_id)
            .fetch_optional(&s.db)
            .await?;
        if found.is_none() {
            return Err(ApiError::unprocessable("invalid_playlist", "playlist does not exist in the group"));
        }
    }
    let mut seen = HashSet::new();
    input.screen_ids.retain(|id| seen.insert(id.clone()));
    for id in &input.screen_ids {
        let found: Option<(String,)> = sqlx::query_as("SELECT id FROM screens WHERE id = ? AND group_id = ?")
            .bind(id)
            .bind(&input.group_id)
            .fetch_optional(&s.db)
            .await?;
        if found.is_none() {
            return Err(invalid("screen_ids must be screens in the group"));
        }
    }
    input.title = input.title.take().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    match input.kind {
        EntryKind::Weekly => {
            input.days.sort();
            input.days.dedup();
            if input.days.is_empty() || input.days.iter().any(|d| !(1..=7).contains(d)) {
                return Err(invalid("days must list weekdays 1 (Monday) to 7"));
            }
            if input.start_time.as_deref().and_then(schedule::parse_time).is_none()
                || input.end_time.as_deref().and_then(schedule::parse_time).is_none()
            {
                return Err(invalid("start_time and end_time must be HH:MM"));
            }
            let on = |v: &Option<String>| -> ApiResult<Option<chrono::NaiveDate>> {
                match v {
                    None => Ok(None),
                    Some(d) => schedule::parse_date(d).map(Some).ok_or_else(|| invalid("dates must be YYYY-MM-DD")),
                }
            };
            for d in &input.skip_dates {
                on(&Some(d.clone()))?;
            }
            input.skip_dates.sort();
            input.skip_dates.dedup();
            if let (Some(a), Some(b)) = (on(&input.starts_on)?, on(&input.ends_on)?) {
                if b < a {
                    return Err(invalid("ends_on must not be before starts_on"));
                }
            }
            input.starts_at = None;
            input.ends_at = None;
        }
        EntryKind::Once => {
            let (Some(a), Some(b)) = (
                input.starts_at.as_deref().and_then(schedule::parse_local),
                input.ends_at.as_deref().and_then(schedule::parse_local),
            ) else {
                return Err(invalid("starts_at and ends_at must be YYYY-MM-DDTHH:MM"));
            };
            if b <= a {
                return Err(invalid("ends_at must be after starts_at"));
            }
            input.days.clear();
            input.start_time = None;
            input.end_time = None;
            input.starts_on = None;
            input.ends_on = None;
            input.skip_dates.clear();
        }
    }
    Ok(())
}

fn kind_name(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Weekly => "weekly",
        EntryKind::Once => "once",
    }
}

async fn changed(s: &AppState, group_id: &str) {
    schedule::notify_group(s, group_id).await;
    schedule::broadcast_changed(s, group_id);
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(mut input): ApiJson<EntryInput>,
) -> ApiResult<(StatusCode, Json<Entry>)> {
    s.require_group(&p, &input.group_id, Role::Editor).await?;
    validate(&s, &mut input).await?;
    let id = new_id();
    let stamp = now();
    sqlx::query(
        "INSERT INTO schedule_entries (id, group_id, playlist_id, title, screen_ids, kind, enabled, days, start_time, end_time, \
         starts_on, ends_on, skip_dates, starts_at, ends_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.group_id)
    .bind(&input.playlist_id)
    .bind(&input.title)
    .bind(serde_json::to_string(&input.screen_ids)?)
    .bind(kind_name(input.kind))
    .bind(input.enabled)
    .bind(serde_json::to_string(&input.days)?)
    .bind(&input.start_time)
    .bind(&input.end_time)
    .bind(&input.starts_on)
    .bind(&input.ends_on)
    .bind(serde_json::to_string(&input.skip_dates)?)
    .bind(&input.starts_at)
    .bind(&input.ends_at)
    .bind(&stamp)
    .bind(&stamp)
    .execute(&s.db)
    .await?;
    changed(&s, &input.group_id).await;
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

pub async fn update(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(mut input): ApiJson<EntryInput>,
) -> ApiResult<Json<Entry>> {
    let current = fetch(&s, &id).await?;
    p.require(&current.group_id, Role::Editor)?;
    if input.group_id != current.group_id {
        return Err(invalid("group_id cannot change"));
    }
    validate(&s, &mut input).await?;
    sqlx::query(
        "UPDATE schedule_entries SET playlist_id = ?, title = ?, screen_ids = ?, kind = ?, enabled = ?, days = ?, start_time = ?, \
         end_time = ?, starts_on = ?, ends_on = ?, skip_dates = ?, starts_at = ?, ends_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&input.playlist_id)
    .bind(&input.title)
    .bind(serde_json::to_string(&input.screen_ids)?)
    .bind(kind_name(input.kind))
    .bind(input.enabled)
    .bind(serde_json::to_string(&input.days)?)
    .bind(&input.start_time)
    .bind(&input.end_time)
    .bind(&input.starts_on)
    .bind(&input.ends_on)
    .bind(serde_json::to_string(&input.skip_dates)?)
    .bind(&input.starts_at)
    .bind(&input.ends_at)
    .bind(now())
    .bind(&id)
    .execute(&s.db)
    .await?;
    changed(&s, &current.group_id).await;
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let current = fetch(&s, &id).await?;
    p.require(&current.group_id, Role::Editor)?;
    sqlx::query("DELETE FROM schedule_entries WHERE id = ?").bind(&id).execute(&s.db).await?;
    changed(&s, &current.group_id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TimelineQuery {
    group_id: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct ScreenTimeline {
    screen_id: String,
    segments: Vec<Segment>,
}

#[derive(Serialize)]
pub struct Timeline {
    timezone: String,
    screens: Vec<ScreenTimeline>,
}

pub async fn timeline(State(s): State<AppState>, p: Principal, Query(q): Query<TimelineQuery>) -> ApiResult<Json<Timeline>> {
    s.require_group(&p, &q.group_id, Role::Viewer).await?;
    if q.to <= q.from || q.to - q.from > schedule::MAX_RANGE {
        return Err(ApiError::bad_request("to must be after from and at most 14 days later"));
    }
    let tz = schedule::group_tz(&s, &q.group_id).await?;
    let entries = schedule::group_entries(&s, &q.group_id).await?;
    let rows: Vec<ScreenRow> = sqlx::query_as(&format!("SELECT {SCREEN_COLUMNS} FROM screens WHERE group_id = ? ORDER BY name"))
        .bind(&q.group_id)
        .fetch_all(&s.db)
        .await?;
    let screens = rows
        .iter()
        .map(|row| ScreenTimeline {
            screen_id: row.id.clone(),
            segments: schedule::screen_segments(row, &entries, &tz, true, q.from, q.to),
        })
        .collect();
    Ok(Json(Timeline { timezone: tz.name().to_string(), screens }))
}
