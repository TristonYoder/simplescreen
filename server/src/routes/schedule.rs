use std::collections::{BTreeMap, HashSet};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    reach::World,
    schedule::{self, invalid, Clash, Entry, EntryKind, EntryRow, Segment, ENTRY_COLUMNS},
    state::AppState,
    util::{new_id, now},
};

#[derive(Deserialize)]
pub struct EntryInput {
    team_id: String,
    playlist_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    screen_ids: Vec<String>,
    #[serde(default)]
    tag_ids: Vec<String>,
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
pub struct ListQuery {
    team_id: Option<String>,
    scope: Option<String>,
}

#[derive(Serialize)]
pub struct ScheduleList {
    timezone: String,
    entries: Vec<Entry>,
}

pub async fn list(State(s): State<AppState>, p: Principal, Query(q): Query<ListQuery>) -> ApiResult<Json<ScheduleList>> {
    if let Some(t) = &q.team_id {
        s.require_team(&p, t, Role::Viewer).await?;
    }
    let wide = match q.scope.as_deref() {
        None | Some("mine") => false,
        Some("visible") => true,
        Some(_) => return Err(ApiError::bad_request("scope must be mine or visible")),
    };
    let all = schedule::all_entries(&s).await?;
    let w = World::load(&s).await?;
    let mine = |e: &Entry| match &q.team_id {
        Some(t) => &e.team_id == t,
        None => p.can_view(&e.team_id),
    };
    let entries = all
        .into_iter()
        .filter(|e| {
            mine(e) || (wide && w.screen_tags.keys().any(|sc| p.sees_screen(&w, sc) && w.applies(e, sc).is_some()))
        })
        .collect();
    Ok(Json(ScheduleList { timezone: s.org_tz().name().to_string(), entries }))
}

async fn fetch(s: &AppState, id: &str) -> ApiResult<Entry> {
    let row: Option<EntryRow> = sqlx::query_as(&format!("SELECT {ENTRY_COLUMNS} FROM schedule_entries WHERE id = ?"))
        .bind(id)
        .fetch_optional(&s.db)
        .await?;
    Ok(row.ok_or_else(ApiError::not_found)?.into_entry())
}

/// Normalizes the input in place: trims the title, drops fields that don't belong to `kind`.
async fn validate(s: &AppState, w: &World, input: &mut EntryInput) -> ApiResult<()> {
    if let Some(pid) = &input.playlist_id {
        if w.playlist_team.get(pid) != Some(&input.team_id) {
            return Err(ApiError::unprocessable("invalid_playlist", "playlist does not exist in the team"));
        }
    }
    let mut seen = HashSet::new();
    input.screen_ids.retain(|id| seen.insert(id.clone()));
    if input.screen_ids.iter().any(|id| !w.reaches(&input.team_id, id)) {
        return Err(invalid("screen_ids must be screens the team reaches"));
    }
    let mut seen = HashSet::new();
    input.tag_ids.retain(|id| seen.insert(id.clone()));
    crate::reach::check_tags(s, &input.tag_ids).await.map_err(|_| invalid("tag_ids must be existing tags"))?;
    if input.tag_ids.iter().any(|t| !w.team_holds(&input.team_id, t)) {
        return Err(invalid("tag_ids must be tags the team holds"));
    }
    input.title = input.title.take().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    match input.kind {
        EntryKind::Weekly => {
            input.days.sort();
            input.days.dedup();
            if input.days.is_empty() || input.days.iter().any(|d| !(1..=7).contains(d)) {
                return Err(invalid("days must list weekdays 1 (Monday) to 7"));
            }
            let (Some(a), Some(b)) = (
                input.start_time.as_deref().and_then(schedule::parse_time),
                input.end_time.as_deref().and_then(schedule::parse_time),
            ) else {
                return Err(invalid("start_time and end_time must be HH:MM or HH:MM:SS"));
            };
            input.start_time = Some(schedule::format_time(a));
            input.end_time = Some(schedule::format_time(b));
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
                return Err(invalid("starts_at and ends_at must be YYYY-MM-DDTHH:MM or YYYY-MM-DDTHH:MM:SS"));
            };
            if b <= a {
                return Err(invalid("ends_at must be after starts_at"));
            }
            input.starts_at = Some(schedule::format_local(a));
            input.ends_at = Some(schedule::format_local(b));
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

/// The entry as it would be saved, for clash checks and previews.
fn candidate(input: &EntryInput, id: &str, created_at: &str, created_by: Option<String>) -> Entry {
    Entry {
        id: id.to_string(),
        team_id: input.team_id.clone(),
        playlist_id: input.playlist_id.clone(),
        title: input.title.clone(),
        screen_ids: input.screen_ids.clone(),
        tag_ids: input.tag_ids.clone(),
        kind: input.kind,
        enabled: input.enabled,
        days: input.days.clone(),
        start_time: input.start_time.clone(),
        end_time: input.end_time.clone(),
        starts_on: input.starts_on.clone(),
        ends_on: input.ends_on.clone(),
        skip_dates: input.skip_dates.clone(),
        starts_at: input.starts_at.clone(),
        ends_at: input.ends_at.clone(),
        created_by,
        needs_resolution: false,
        created_at: created_at.to_string(),
        updated_at: created_at.to_string(),
    }
}

/// The one-off clashes `cand` would cause, with every other saved entry.
async fn clashes_with(s: &AppState, w: &World, cand: &Entry) -> ApiResult<Vec<Clash>> {
    if cand.kind != EntryKind::Once || !cand.enabled {
        return Ok(vec![]);
    }
    let mut entries: Vec<Entry> = schedule::all_entries(s).await?.into_iter().filter(|e| e.id != cand.id).collect();
    entries.push(cand.clone());
    let screens = schedule::all_screens(s).await?;
    Ok(schedule::find_clashes(w, &screens, &entries, &s.org_tz(), Utc::now(), Some(&cand.id)))
}

fn conflict(clashes: Vec<Clash>) -> ApiError {
    let mut e = ApiError::conflict("schedule_conflict", "a one-off entry already plays on that screen at that time");
    e.conflicts = Some(json!(clashes));
    e
}

async fn changed(s: &AppState, p: &Principal, team_id: &str) -> ApiResult<()> {
    schedule::refresh_clashes(s, Some(p)).await?;
    schedule::notify_screens(s).await;
    schedule::broadcast_changed(s, team_id);
    Ok(())
}

pub async fn create(
    State(s): State<AppState>,
    p: Principal,
    ApiJson(mut input): ApiJson<EntryInput>,
) -> ApiResult<(StatusCode, Json<Entry>)> {
    s.require_team(&p, &input.team_id, Role::Editor).await?;
    let w = World::load(&s).await?;
    validate(&s, &w, &mut input).await?;
    let id = new_id();
    let stamp = now();
    let clashes = clashes_with(&s, &w, &candidate(&input, &id, &stamp, None)).await?;
    if !clashes.is_empty() {
        return Err(conflict(clashes));
    }
    sqlx::query(
        "INSERT INTO schedule_entries (id, team_id, playlist_id, title, screen_ids, tag_ids, kind, enabled, days, start_time, end_time, \
         starts_on, ends_on, skip_dates, starts_at, ends_at, created_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.team_id)
    .bind(&input.playlist_id)
    .bind(&input.title)
    .bind(serde_json::to_string(&input.screen_ids)?)
    .bind(serde_json::to_string(&input.tag_ids)?)
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
    .bind(&p.subject)
    .bind(&stamp)
    .bind(&stamp)
    .execute(&s.db)
    .await?;
    changed(&s, &p, &input.team_id).await?;
    Ok((StatusCode::CREATED, Json(fetch(&s, &id).await?)))
}

pub async fn update(
    State(s): State<AppState>,
    p: Principal,
    Path(id): Path<String>,
    ApiJson(mut input): ApiJson<EntryInput>,
) -> ApiResult<Json<Entry>> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    if input.team_id != current.team_id {
        return Err(invalid("team_id cannot change"));
    }
    let w = World::load(&s).await?;
    validate(&s, &w, &mut input).await?;
    let clashes = clashes_with(&s, &w, &candidate(&input, &id, &current.created_at, current.created_by.clone())).await?;
    if !clashes.is_empty() {
        return Err(conflict(clashes));
    }
    sqlx::query(
        "UPDATE schedule_entries SET playlist_id = ?, title = ?, screen_ids = ?, tag_ids = ?, kind = ?, enabled = ?, days = ?, \
         start_time = ?, end_time = ?, starts_on = ?, ends_on = ?, skip_dates = ?, starts_at = ?, ends_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&input.playlist_id)
    .bind(&input.title)
    .bind(serde_json::to_string(&input.screen_ids)?)
    .bind(serde_json::to_string(&input.tag_ids)?)
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
    changed(&s, &p, &current.team_id).await?;
    Ok(Json(fetch(&s, &id).await?))
}

pub async fn delete(State(s): State<AppState>, p: Principal, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let current = fetch(&s, &id).await?;
    p.require(&current.team_id, Role::Editor)?;
    sqlx::query("DELETE FROM schedule_entries WHERE id = ?").bind(&id).execute(&s.db).await?;
    crate::notices::resolve_entry(&s, &id).await?;
    changed(&s, &p, &current.team_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// MARK: Preview

#[derive(Deserialize)]
pub struct PreviewInput {
    #[serde(flatten)]
    entry: EntryInput,
    entry_id: Option<String>,
}

#[derive(Serialize)]
struct Range {
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct EntryRanges {
    entry_id: String,
    team_id: String,
    ranges: Vec<Range>,
}

#[derive(Serialize)]
struct ScreenPreview {
    screen_id: String,
    wins: Vec<Range>,
    covered_by: Vec<EntryRanges>,
    covers: Vec<EntryRanges>,
    clashes: Vec<Clash>,
}

#[derive(Serialize)]
pub struct Preview {
    screens: Vec<ScreenPreview>,
}

fn grouped(map: BTreeMap<(String, String), Vec<Range>>) -> Vec<EntryRanges> {
    map.into_iter().map(|((entry_id, team_id), ranges)| EntryRanges { entry_id, team_id, ranges }).collect()
}

pub async fn preview(State(s): State<AppState>, p: Principal, ApiJson(input): ApiJson<PreviewInput>) -> ApiResult<Json<Preview>> {
    let PreviewInput { entry: mut input, entry_id } = input;
    s.require_team(&p, &input.team_id, Role::Editor).await?;
    let w = World::load(&s).await?;
    validate(&s, &w, &mut input).await?;
    let existing = match &entry_id {
        Some(id) => {
            let e = fetch(&s, id).await?;
            if e.team_id != input.team_id {
                return Err(invalid("entry_id must be an entry of the team"));
            }
            Some(e)
        }
        None => None,
    };
    let id = entry_id.clone().unwrap_or_else(|| "preview".to_string());
    let created = existing.as_ref().map(|e| e.created_at.clone()).unwrap_or_else(now);
    let cand = candidate(&input, &id, &created, existing.and_then(|e| e.created_by));
    let others: Vec<Entry> = schedule::all_entries(&s).await?.into_iter().filter(|e| e.id != id).collect();
    let mut with = others.clone();
    with.push(cand.clone());
    let screens = schedule::all_screens(&s).await?;
    let org = s.org_tz();
    let start = Utc::now();
    let clashes = if cand.kind == EntryKind::Once && cand.enabled {
        schedule::find_clashes(&w, &screens, &with, &org, start, Some(&id))
    } else {
        vec![]
    };
    let mut out = vec![];
    for row in &screens {
        if w.applies(&cand, &row.id).is_none() {
            continue;
        }
        let tz = schedule::screen_tz(row, &org);
        let (from, to) = match cand.kind {
            EntryKind::Weekly => (start, start + schedule::MAX_RANGE),
            EntryKind::Once => {
                let occ = schedule::occurrences(&cand, &tz, chrono::DateTime::<Utc>::MIN_UTC, chrono::DateTime::<Utc>::MAX_UTC);
                match occ.first() {
                    Some((a, b)) => (*a, (*b).min(*a + schedule::MAX_RANGE)),
                    None => continue,
                }
            }
        };
        let after = schedule::screen_segments(&w, row, &with, &org, false, from, to);
        let before = schedule::screen_segments(&w, row, &others, &org, false, from, to);
        let occ = schedule::occurrences(&cand, &tz, from, to);
        let mut wins = vec![];
        let mut covered: BTreeMap<(String, String), Vec<Range>> = BTreeMap::new();
        let mut covers: BTreeMap<(String, String), Vec<Range>> = BTreeMap::new();
        for (a, b) in &occ {
            for seg in after.iter().filter(|g| g.starts_at < *b && g.ends_at > *a) {
                let r = Range { starts_at: seg.starts_at.max(*a), ends_at: seg.ends_at.min(*b) };
                match &seg.entry_id {
                    Some(e) if *e == id => {
                        for old in before.iter().filter(|o| o.entry_id.is_some() && o.starts_at < r.ends_at && o.ends_at > r.starts_at) {
                            let key = (old.entry_id.clone().unwrap_or_default(), old.team_id.clone().unwrap_or_default());
                            covers.entry(key).or_default().push(Range {
                                starts_at: old.starts_at.max(r.starts_at),
                                ends_at: old.ends_at.min(r.ends_at),
                            });
                        }
                        wins.push(r);
                    }
                    Some(e) => {
                        covered.entry((e.clone(), seg.team_id.clone().unwrap_or_default())).or_default().push(r);
                    }
                    None => {}
                }
            }
        }
        out.push(ScreenPreview {
            screen_id: row.id.clone(),
            wins,
            covered_by: grouped(covered),
            covers: grouped(covers),
            clashes: clashes.iter().filter(|c| c.screen_id == row.id).cloned().collect(),
        });
    }
    Ok(Json(Preview { screens: out }))
}

// MARK: Timeline

#[derive(Deserialize)]
pub struct TimelineQuery {
    team_id: Option<String>,
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
    if let Some(t) = &q.team_id {
        s.require_team(&p, t, Role::Viewer).await?;
    }
    if q.to <= q.from || q.to - q.from > schedule::MAX_RANGE {
        return Err(ApiError::bad_request("to must be after from and at most 14 days later"));
    }
    let tz = s.org_tz();
    let w = World::load(&s).await?;
    let entries = schedule::all_entries(&s).await?;
    let rows = schedule::all_screens(&s).await?;
    let screens = rows
        .iter()
        .filter(|row| match &q.team_id {
            Some(t) => w.reaches(t, &row.id),
            None => p.sees_screen(&w, &row.id),
        })
        .map(|row| ScreenTimeline {
            screen_id: row.id.clone(),
            segments: schedule::screen_segments(&w, row, &entries, &tz, true, q.from, q.to),
        })
        .collect();
    Ok(Json(Timeline { timezone: tz.name().to_string(), screens }))
}
