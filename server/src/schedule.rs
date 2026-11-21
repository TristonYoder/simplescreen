use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;

use crate::{
    auth::Principal,
    error::{ApiError, ApiResult},
    models::{ScreenRow, SCREEN_COLUMNS},
    reach::{Match, World},
    state::AppState,
};

pub const TIME_FORMAT: &str = "%H:%M";
pub const DATE_FORMAT: &str = "%Y-%m-%d";
pub const LOCAL_FORMAT: &str = "%Y-%m-%dT%H:%M";
pub const DEVICE_HORIZON: Duration = Duration::hours(48);
pub const MAX_RANGE: Duration = Duration::days(14);
pub const CLEAR_OVERRIDE: &str = "override_set = 0, override_playlist_id = NULL, override_until = NULL, override_set_at = NULL, \
     override_set_by = NULL, override_set_by_name = NULL, override_team_id = NULL";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Weekly,
    Once,
}

#[derive(Serialize, Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub team_id: String,
    pub playlist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub screen_ids: Vec<String>,
    pub tag_ids: Vec<String>,
    pub kind: EntryKind,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub days: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_on: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_on: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skip_dates: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub needs_resolution: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(FromRow)]
pub struct EntryRow {
    id: String,
    team_id: String,
    playlist_id: Option<String>,
    title: Option<String>,
    screen_ids: String,
    tag_ids: String,
    kind: String,
    enabled: bool,
    days: String,
    start_time: Option<String>,
    end_time: Option<String>,
    starts_on: Option<String>,
    ends_on: Option<String>,
    skip_dates: String,
    starts_at: Option<String>,
    ends_at: Option<String>,
    created_by: Option<String>,
    needs_resolution: bool,
    created_at: String,
    updated_at: String,
}

pub const ENTRY_COLUMNS: &str = "id, team_id, playlist_id, title, screen_ids, tag_ids, kind, enabled, days, start_time, end_time, \
     starts_on, ends_on, skip_dates, starts_at, ends_at, created_by, needs_resolution, created_at, updated_at";

impl EntryRow {
    pub fn into_entry(self) -> Entry {
        Entry {
            id: self.id,
            team_id: self.team_id,
            playlist_id: self.playlist_id,
            title: self.title,
            screen_ids: serde_json::from_str(&self.screen_ids).unwrap_or_default(),
            tag_ids: serde_json::from_str(&self.tag_ids).unwrap_or_default(),
            kind: if self.kind == "once" { EntryKind::Once } else { EntryKind::Weekly },
            enabled: self.enabled,
            days: serde_json::from_str(&self.days).unwrap_or_default(),
            start_time: self.start_time,
            end_time: self.end_time,
            starts_on: self.starts_on,
            ends_on: self.ends_on,
            skip_dates: serde_json::from_str(&self.skip_dates).unwrap_or_default(),
            starts_at: self.starts_at,
            ends_at: self.ends_at,
            created_by: self.created_by,
            needs_resolution: self.needs_resolution,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Ordered so that a higher variant beats a lower one.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    Weekly,
    Once,
    Override,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Display {
    Playlist,
    Home,
    Blank,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Segment {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub playlist_id: Option<String>,
    pub source: Source,
    pub display: Display,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Override {
    pub playlist_id: Option<String>,
    pub until: Option<DateTime<Utc>>,
    pub team_id: Option<String>,
}

pub fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s, TIME_FORMAT).ok()
}

pub fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, DATE_FORMAT).ok()
}

pub fn parse_local(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, LOCAL_FORMAT).ok()
}

/// Wall clock to instant; a time skipped by a DST jump moves forward an hour, a repeated one takes the first.
pub fn local_to_utc(tz: &Tz, dt: NaiveDateTime) -> DateTime<Utc> {
    let local = match tz.from_local_datetime(&dt) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t),
        LocalResult::None => tz.from_local_datetime(&(dt + Duration::hours(1))).earliest(),
    };
    local.map(|t| t.with_timezone(&Utc)).unwrap_or_else(|| Utc.from_utc_datetime(&dt))
}

/// Occurrences of an entry that overlap `[from, to)`.
pub fn occurrences(e: &Entry, tz: &Tz, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    if !e.enabled {
        return vec![];
    }
    let overlaps = |s: DateTime<Utc>, t: DateTime<Utc>| s < to && t > from;
    match e.kind {
        EntryKind::Once => {
            let (Some(s), Some(t)) =
                (e.starts_at.as_deref().and_then(parse_local), e.ends_at.as_deref().and_then(parse_local))
            else {
                return vec![];
            };
            let (s, t) = (local_to_utc(tz, s), local_to_utc(tz, t));
            if overlaps(s, t) { vec![(s, t)] } else { vec![] }
        }
        EntryKind::Weekly => {
            let (Some(st), Some(et)) =
                (e.start_time.as_deref().and_then(parse_time), e.end_time.as_deref().and_then(parse_time))
            else {
                return vec![];
            };
            let starts_on = e.starts_on.as_deref().and_then(parse_date);
            let ends_on = e.ends_on.as_deref().and_then(parse_date);
            let skips: Vec<NaiveDate> = e.skip_dates.iter().filter_map(|d| parse_date(d)).collect();
            let mut day = from.with_timezone(tz).date_naive() - Duration::days(1);
            let last = to.with_timezone(tz).date_naive();
            let mut out = vec![];
            while day <= last {
                let in_range =
                    starts_on.is_none_or(|d| day >= d) && ends_on.is_none_or(|d| day <= d) && !skips.contains(&day);
                if in_range && e.days.contains(&(day.weekday().number_from_monday() as u8)) {
                    let end_day = if et <= st { day + Duration::days(1) } else { day };
                    let s = local_to_utc(tz, day.and_time(st));
                    let t = local_to_utc(tz, end_day.and_time(et));
                    if overlaps(s, t) {
                        out.push((s, t));
                    }
                }
                day += Duration::days(1);
            }
            out
        }
    }
}

struct Candidate<'a> {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    rank: (Source, Match, &'a str),
    playlist_id: Option<String>,
    entry_id: Option<String>,
    team_id: Option<String>,
}

/// Contiguous segments covering `[from, to)` for one screen. `entries` must already apply to the screen,
/// each with how it matches. `fallback` plays when nothing is scheduled; without one the home menu shows.
pub fn resolve(
    entries: &[(&Entry, Match)],
    fallback: Option<&str>,
    ovr: Option<&Override>,
    tz: &Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Segment> {
    if to <= from {
        return vec![];
    }
    let mut cands: Vec<Candidate> = vec![];
    for (e, m) in entries {
        let tier = match e.kind {
            EntryKind::Once => Source::Once,
            EntryKind::Weekly => Source::Weekly,
        };
        for (s, t) in occurrences(e, tz, from, to) {
            cands.push(Candidate {
                start: s.max(from),
                end: t.min(to),
                rank: (tier, *m, &e.created_at),
                playlist_id: e.playlist_id.clone(),
                entry_id: Some(e.id.clone()),
                team_id: Some(e.team_id.clone()),
            });
        }
    }
    if let Some(o) = ovr {
        let end = o.until.unwrap_or(to).min(to);
        if end > from {
            cands.push(Candidate {
                start: from,
                end,
                rank: (Source::Override, Match::Screen, ""),
                playlist_id: o.playlist_id.clone(),
                entry_id: None,
                team_id: o.team_id.clone(),
            });
        }
    }
    let mut points = vec![from, to];
    for c in &cands {
        points.push(c.start);
        points.push(c.end);
    }
    points.sort();
    points.dedup();

    let mut out: Vec<Segment> = vec![];
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let best = cands.iter().filter(|c| c.start <= a && c.end >= b).max_by(|x, y| x.rank.cmp(&y.rank));
        let seg = match best {
            Some(c) => Segment {
                starts_at: a,
                ends_at: b,
                playlist_id: c.playlist_id.clone(),
                source: c.rank.0,
                display: if c.playlist_id.is_some() { Display::Playlist } else { Display::Blank },
                entry_id: c.entry_id.clone(),
                team_id: c.team_id.clone(),
            },
            None => Segment {
                starts_at: a,
                ends_at: b,
                playlist_id: fallback.map(String::from),
                source: Source::Default,
                display: if fallback.is_some() { Display::Playlist } else { Display::Home },
                entry_id: None,
                team_id: None,
            },
        };
        if let Some(last) = out.last_mut() {
            if last.playlist_id == seg.playlist_id && last.source == seg.source && last.entry_id == seg.entry_id {
                last.ends_at = b;
                continue;
            }
        }
        out.push(seg);
    }
    out
}

/// The end of the first segment, unless the first segment runs to the end of the range.
pub fn next_change(segments: &[Segment]) -> Option<DateTime<Utc>> {
    (segments.len() > 1).then(|| segments[0].ends_at)
}

// MARK: Loading

pub fn parse_tz(name: &str) -> Option<Tz> {
    name.parse().ok()
}

pub async fn team_entries(s: &AppState, team_id: &str) -> ApiResult<Vec<Entry>> {
    let rows: Vec<EntryRow> =
        sqlx::query_as(&format!("SELECT {ENTRY_COLUMNS} FROM schedule_entries WHERE team_id = ? ORDER BY created_at"))
            .bind(team_id)
            .fetch_all(&s.db)
            .await?;
    Ok(rows.into_iter().map(EntryRow::into_entry).collect())
}

pub async fn all_entries(s: &AppState) -> ApiResult<Vec<Entry>> {
    let rows: Vec<EntryRow> = sqlx::query_as(&format!("SELECT {ENTRY_COLUMNS} FROM schedule_entries ORDER BY created_at"))
        .fetch_all(&s.db)
        .await?;
    Ok(rows.into_iter().map(EntryRow::into_entry).collect())
}

pub async fn all_screens(s: &AppState) -> ApiResult<Vec<ScreenRow>> {
    Ok(sqlx::query_as(&format!("SELECT {SCREEN_COLUMNS} FROM screens ORDER BY name, id")).fetch_all(&s.db).await?)
}

pub fn screen_override(row: &ScreenRow) -> Option<Override> {
    if !row.override_set {
        return None;
    }
    Some(Override {
        playlist_id: row.override_playlist_id.clone(),
        until: row.override_until.as_deref().and_then(|u| DateTime::parse_from_rfc3339(u).ok()).map(|d| d.to_utc()),
        team_id: row.override_team_id.clone(),
    })
}

pub fn screen_tz(row: &ScreenRow, org: &Tz) -> Tz {
    row.timezone.as_deref().and_then(parse_tz).unwrap_or(*org)
}

/// What applies to one screen: entries of teams that reach it whose playlist is available there, the
/// fallback if available, and the override unless its playlist became unavailable.
pub fn screen_segments(
    w: &World,
    row: &ScreenRow,
    entries: &[Entry],
    org: &Tz,
    with_override: bool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Segment> {
    let applying: Vec<(&Entry, Match)> = entries.iter().filter_map(|e| w.applies(e, &row.id).map(|m| (e, m))).collect();
    let fallback = row.fallback_playlist_id.as_deref().filter(|p| w.available(p, &row.id));
    let ovr = if with_override { screen_override(row) } else { None }
        .filter(|o| o.playlist_id.as_deref().is_none_or(|p| w.available(p, &row.id)));
    resolve(&applying, fallback, ovr.as_ref(), &screen_tz(row, org), from, to)
}

pub async fn screen_timeline(s: &AppState, row: &ScreenRow, from: DateTime<Utc>, to: DateTime<Utc>) -> ApiResult<Vec<Segment>> {
    let w = World::load(s).await?;
    let entries = all_entries(s).await?;
    Ok(screen_segments(&w, row, &entries, &s.org_tz(), true, from, to))
}

/// Schedule without the override for the next 14 days.
pub async fn base_timeline(s: &AppState, row: &ScreenRow, now: DateTime<Utc>) -> ApiResult<Vec<Segment>> {
    let w = World::load(s).await?;
    let entries = all_entries(s).await?;
    Ok(screen_segments(&w, row, &entries, &s.org_tz(), false, now, now + MAX_RANGE))
}

/// Every device refetches (with If-None-Match, so unchanged manifests cost a 304).
pub async fn notify_screens(s: &AppState) {
    let ids: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id, node_id FROM screens").fetch_all(&s.db).await.unwrap_or_default();
    for (id, node_id) in ids {
        s.notify_manifest(node_id.as_deref(), &id, 0);
    }
    s.schedule_wake.notify_one();
}

/// Re-resolves every schedule (after an org timezone change): devices refetch, user sockets reload.
pub async fn notify_all(s: &AppState) {
    notify_screens(s).await;
    s.hub.broadcast_all(&json!({ "type": "schedule_changed" }));
}

pub fn broadcast_changed(s: &AppState, team_id: &str) {
    s.hub.broadcast_all(&json!({ "type": "schedule_changed", "team_id": team_id }));
}

// MARK: Clashes

#[derive(Serialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Clash {
    /// Older entry first.
    pub entry_ids: [String; 2],
    pub screen_id: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
}

fn once_span(e: &Entry, tz: &Tz) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let (Some(a), Some(b)) = (e.starts_at.as_deref().and_then(parse_local), e.ends_at.as_deref().and_then(parse_local)) else {
        return None;
    };
    Some((local_to_utc(tz, a), local_to_utc(tz, b)))
}

/// One-off entries that play on the same screen at the same time and haven't ended. With `only`, just
/// the clashes involving that entry.
pub fn find_clashes(
    w: &World,
    screens: &[ScreenRow],
    entries: &[Entry],
    org: &Tz,
    now: DateTime<Utc>,
    only: Option<&str>,
) -> Vec<Clash> {
    let mut out = vec![];
    for row in screens {
        let tz = screen_tz(row, org);
        let mut spans: Vec<(&Entry, DateTime<Utc>, DateTime<Utc>)> = entries
            .iter()
            .filter(|e| e.enabled && e.kind == EntryKind::Once && w.applies(e, &row.id).is_some())
            .filter_map(|e| once_span(e, &tz).map(|(a, b)| (e, a, b)))
            .filter(|(_, _, b)| *b > now)
            .collect();
        spans.sort_by(|x, y| x.0.created_at.cmp(&y.0.created_at).then(x.0.id.cmp(&y.0.id)));
        for i in 0..spans.len() {
            for j in i + 1..spans.len() {
                let (a, b) = (&spans[i], &spans[j]);
                if only.is_some_and(|id| a.0.id != id && b.0.id != id) {
                    continue;
                }
                let (start, end) = (a.1.max(b.1), a.2.min(b.2));
                if start < end {
                    out.push(Clash { entry_ids: [a.0.id.clone(), b.0.id.clone()], screen_id: row.id.clone(), starts_at: start, ends_at: end });
                }
            }
        }
    }
    out
}

/// Re-flags one-off entries that clash (after a retag, a reach change or a schedule change). Newly
/// flagged entries notify their creators and `actor`; entries no longer clashing resolve their notices.
/// Returns the clashes involving a newly flagged entry.
pub async fn refresh_clashes(s: &AppState, actor: Option<&Principal>) -> ApiResult<Vec<Clash>> {
    let w = World::load(s).await?;
    let screens = all_screens(s).await?;
    let entries = all_entries(s).await?;
    let clashes = find_clashes(&w, &screens, &entries, &s.org_tz(), Utc::now(), None);
    let flagged: HashSet<&str> = clashes.iter().flat_map(|c| c.entry_ids.iter().map(String::as_str)).collect();
    let mut newly: HashSet<String> = HashSet::new();
    for e in &entries {
        let now_flagged = flagged.contains(e.id.as_str());
        if now_flagged == e.needs_resolution {
            continue;
        }
        sqlx::query("UPDATE schedule_entries SET needs_resolution = ? WHERE id = ?").bind(now_flagged).bind(&e.id).execute(&s.db).await?;
        if now_flagged {
            newly.insert(e.id.clone());
        } else {
            crate::notices::resolve_entry(s, &e.id).await?;
        }
    }
    let caused: Vec<Clash> = clashes.into_iter().filter(|c| c.entry_ids.iter().any(|id| newly.contains(id))).collect();
    for id in &newly {
        let Some(e) = entries.iter().find(|e| &e.id == id) else { continue };
        let mine: Vec<&Clash> = caused.iter().filter(|c| c.entry_ids.contains(id)).collect();
        let detail = json!({ "clashes": mine, "title": e.title, "team_id": e.team_id });
        let mut to: Vec<String> = e.created_by.iter().cloned().collect();
        if let Some(a) = actor.filter(|a| !a.is_device()) {
            if !to.contains(&a.subject) {
                to.push(a.subject.clone());
            }
        }
        for sub in to {
            crate::notices::create(s, Some(&sub), "schedule_conflict", None, Some(id), detail.clone()).await?;
        }
    }
    if !newly.is_empty() {
        notify_screens(s).await;
        s.hub.broadcast_all(&json!({ "type": "schedule_changed" }));
    }
    Ok(caused)
}

pub fn invalid(message: &str) -> ApiError {
    ApiError::unprocessable("invalid_schedule", message)
}

// MARK: Boundary pushes

/// Pushes `manifest_changed` when a screen's active playlist changes and clears expired overrides.
pub async fn run(s: AppState) {
    let mut shutdown = s.shutdown.subscribe();
    let mut active: HashMap<String, Option<(Option<String>, Display)>> = HashMap::new();
    loop {
        let now = Utc::now();
        let mut next = now + Duration::seconds(60);
        if let Err(e) = tick(&s, now, &mut active, &mut next).await {
            tracing::warn!("schedule tick failed: {e:?}");
        }
        let wait = (next - Utc::now()).to_std().unwrap_or_default() + std::time::Duration::from_millis(50);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = s.schedule_wake.notified() => {}
            _ = shutdown.changed() => return,
        }
    }
}

async fn tick(
    s: &AppState,
    now: DateTime<Utc>,
    active: &mut HashMap<String, Option<(Option<String>, Display)>>,
    next: &mut DateTime<Utc>,
) -> ApiResult<()> {
    let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let expired: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM screens WHERE override_set = 1 AND override_until IS NOT NULL AND override_until <= ?")
            .bind(&stamp)
            .fetch_all(&s.db)
            .await?;
    for (id,) in &expired {
        sqlx::query(&format!("UPDATE screens SET {CLEAR_OVERRIDE} WHERE id = ?")).bind(id).execute(&s.db).await?;
        s.broadcast_screen(id, &json!({ "type": "screen_updated", "screen_id": id })).await;
    }

    let w = World::load(s).await?;
    let rows = all_screens(s).await?;
    let entries = all_entries(s).await?;
    let org = s.org_tz();
    for row in &rows {
        let segs = screen_segments(&w, row, &entries, &org, true, now, now + DEVICE_HORIZON);
        let current = segs.first().map(|g| (g.playlist_id.clone(), g.display));
        if let Some(previous) = active.insert(row.id.clone(), current.clone()) {
            if previous != current {
                s.notify_manifest(row.node_id.as_deref(), &row.id, 0);
                s.broadcast_screen(&row.id, &json!({ "type": "screen_updated", "screen_id": row.id })).await;
            }
        }
        if let Some(change) = next_change(&segs) {
            *next = (*next).min(change);
        }
    }
    active.retain(|id, _| rows.iter().any(|r| &r.id == id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: EntryKind, playlist: &str) -> Entry {
        Entry {
            id: id.into(),
            team_id: "g".into(),
            playlist_id: Some(playlist.into()),
            title: None,
            screen_ids: vec![],
            tag_ids: vec![],
            kind,
            enabled: true,
            days: vec![],
            start_time: None,
            end_time: None,
            starts_on: None,
            ends_on: None,
            skip_dates: vec![],
            starts_at: None,
            ends_at: None,
            created_by: None,
            needs_resolution: false,
            created_at: format!("2026-01-01T00:00:00.000Z{id}"),
            updated_at: String::new(),
        }
    }

    fn weekly(id: &str, playlist: &str, days: &[u8], start: &str, end: &str) -> Entry {
        let mut e = entry(id, EntryKind::Weekly, playlist);
        e.days = days.to_vec();
        e.start_time = Some(start.into());
        e.end_time = Some(end.into());
        e
    }

    fn once(id: &str, playlist: &str, start: &str, end: &str) -> Entry {
        let mut e = entry(id, EntryKind::Once, playlist);
        e.starts_at = Some(start.into());
        e.ends_at = Some(end.into());
        e
    }

    fn m<'a>(es: &[&'a Entry]) -> Vec<(&'a Entry, Match)> {
        es.iter().map(|e| (*e, if e.screen_ids.is_empty() { Match::TeamWide } else { Match::Screen })).collect()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().to_utc()
    }

    fn summary(segs: &[Segment]) -> Vec<(String, String, Option<String>)> {
        segs.iter()
            .map(|g| (g.starts_at.format("%a %H:%M").to_string(), g.ends_at.format("%a %H:%M").to_string(), g.playlist_id.clone()))
            .collect()
    }

    #[test]
    fn weekly_with_default_and_event() {
        // 2026-11-20 is a Friday.
        let breakfast = weekly("1", "breakfast", &[1, 2, 3, 4, 5], "07:00", "10:30");
        let dinner = weekly("2", "dinner", &[1, 2, 3, 4, 5], "17:00", "22:00");
        let launch = once("3", "launch", "2026-11-20T18:00", "2026-11-20T23:00");
        let segs = resolve(
            &m(&[&breakfast, &dinner, &launch]),
            Some("house"),
            None,
            &Tz::UTC,
            utc("2026-11-20T06:00:00Z"),
            utc("2026-11-21T00:00:00Z"),
        );
        let s = |a: &str, b: &str, p: &str| (a.to_string(), b.to_string(), Some(p.to_string()));
        assert_eq!(
            summary(&segs),
            vec![
                s("Fri 06:00", "Fri 07:00", "house"),
                s("Fri 07:00", "Fri 10:30", "breakfast"),
                s("Fri 10:30", "Fri 17:00", "house"),
                s("Fri 17:00", "Fri 18:00", "dinner"),
                s("Fri 18:00", "Fri 23:00", "launch"),
                s("Fri 23:00", "Sat 00:00", "house"),
            ]
        );
        assert_eq!(segs[4].source, Source::Once);
        assert_eq!(next_change(&segs), Some(utc("2026-11-20T07:00:00Z")));
    }

    #[test]
    fn overnight_and_weekday_filter() {
        let late = weekly("1", "late", &[6], "22:00", "02:00");
        let segs = resolve(&m(&[&late]), None, None, &Tz::UTC, utc("2026-11-21T20:00:00Z"), utc("2026-11-22T04:00:00Z"));
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[1].playlist_id.as_deref(), Some("late"));
        assert_eq!(segs[1].ends_at, utc("2026-11-22T02:00:00Z"));
        assert_eq!(segs[0].playlist_id, None);
    }

    #[test]
    fn screen_specific_beats_team_wide_then_newest() {
        let team_wide = weekly("2", "team", &[1, 2, 3, 4, 5, 6, 7], "00:00", "00:00");
        let mut specific = weekly("1", "specific", &[1, 2, 3, 4, 5, 6, 7], "09:00", "10:00");
        specific.screen_ids = vec!["s1".into()];
        let newer = weekly("3", "newer", &[1, 2, 3, 4, 5, 6, 7], "09:30", "11:00");
        let segs = resolve(&m(&[&team_wide, &specific, &newer]), None, None, &Tz::UTC, utc("2026-11-18T08:00:00Z"), utc("2026-11-18T12:00:00Z"));
        let names: Vec<_> = segs.iter().map(|g| g.playlist_id.clone().unwrap()).collect();
        assert_eq!(names, vec!["team", "specific", "newer", "team"]);
    }

    #[test]
    fn override_wins_until_expiry() {
        let lunch = weekly("1", "lunch", &[3], "11:00", "14:00");
        let o = Override { playlist_id: Some("manual".into()), until: Some(utc("2026-11-18T12:00:00Z")), team_id: None };
        let segs = resolve(&m(&[&lunch]), Some("house"), Some(&o), &Tz::UTC, utc("2026-11-18T10:00:00Z"), utc("2026-11-18T15:00:00Z"));
        let names: Vec<_> = segs.iter().map(|g| (g.source, g.playlist_id.clone().unwrap())).collect();
        assert_eq!(
            names,
            vec![(Source::Override, "manual".into()), (Source::Weekly, "lunch".into()), (Source::Default, "house".into())]
        );
    }

    #[test]
    fn date_bounds_and_timezone() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let mut e = weekly("1", "holiday", &[1, 2, 3, 4, 5, 6, 7], "09:00", "17:00");
        e.starts_on = Some("2026-11-20".into());
        e.ends_on = Some("2026-11-21".into());
        let segs = resolve(&m(&[&e]), None, None, &tz, utc("2026-11-19T00:00:00Z"), utc("2026-11-23T00:00:00Z"));
        let on: Vec<_> = segs.iter().filter(|g| g.playlist_id.is_some()).map(|g| g.starts_at).collect();
        assert_eq!(on, vec![utc("2026-11-20T14:00:00Z"), utc("2026-11-21T14:00:00Z")]);
    }

    #[test]
    fn skip_dates_drop_one_occurrence() {
        let mut e = weekly("1", "breakfast", &[1, 2, 3, 4, 5, 6, 7], "07:00", "11:00");
        e.skip_dates = vec!["2026-11-19".into()];
        let segs = resolve(&m(&[&e]), None, None, &Tz::UTC, utc("2026-11-18T00:00:00Z"), utc("2026-11-21T00:00:00Z"));
        let on: Vec<_> = segs.iter().filter(|g| g.playlist_id.is_some()).map(|g| g.starts_at).collect();
        assert_eq!(on, vec![utc("2026-11-18T07:00:00Z"), utc("2026-11-20T07:00:00Z")]);
    }

    #[test]
    fn tag_match_beats_team_wide_and_home_shows_without_fallback() {
        let team_wide = weekly("2", "team", &[3], "09:00", "11:00");
        let tagged = weekly("1", "tagged", &[3], "10:00", "12:00");
        let segs = resolve(
            &[(&team_wide, Match::TeamWide), (&tagged, Match::Tag)],
            None,
            None,
            &Tz::UTC,
            utc("2026-11-18T08:00:00Z"),
            utc("2026-11-18T13:00:00Z"),
        );
        let got: Vec<_> = segs.iter().map(|g| (g.display, g.playlist_id.clone())).collect();
        assert_eq!(
            got,
            vec![
                (Display::Home, None),
                (Display::Playlist, Some("team".into())),
                (Display::Playlist, Some("tagged".into())),
                (Display::Home, None),
            ]
        );
    }

    fn world() -> World {
        let mut w = World::default();
        let set = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<HashSet<String>>();
        w.teams.insert("lobby".into(), crate::reach::TeamInfo { name: "Lobby".into(), all: false, tags: set(&["front"]) });
        w.teams.insert("ops".into(), crate::reach::TeamInfo { name: "Ops".into(), all: true, tags: set(&[]) });
        w.screen_tags.insert("s_front".into(), set(&["front"]));
        w.screen_tags.insert("s_back".into(), set(&["back"]));
        w.screen_tags.insert("s_none".into(), set(&[]));
        w.playlist_tags.insert("p_front".into(), set(&["front"]));
        w.playlist_tags.insert("p_untagged".into(), set(&[]));
        w.playlist_team.insert("p_front".into(), "lobby".into());
        w.playlist_team.insert("p_untagged".into(), "lobby".into());
        w
    }

    #[test]
    fn reach_follows_tags_and_untagged_screens_are_reached_by_none() {
        let w = world();
        assert!(w.reaches("lobby", "s_front"));
        assert!(!w.reaches("lobby", "s_back"));
        assert!(w.reaches("ops", "s_back"));
        assert!(!w.reaches("ops", "s_none"));
        assert!(!w.reaches("lobby", "s_none"));
        assert_eq!(w.screen_teams("s_front"), vec!["lobby".to_string(), "ops".to_string()]);
        assert!(w.team_holds("ops", "anything"));
        assert!(!w.team_holds("lobby", "back"));
    }

    #[test]
    fn availability_needs_a_shared_tag() {
        let w = world();
        assert!(w.available("p_front", "s_front"));
        assert!(!w.available("p_front", "s_back"));
        assert!(!w.available("p_untagged", "s_front"));
        assert_eq!(w.available_on("s_front"), vec!["p_front".to_string()]);
    }

    #[test]
    fn entries_apply_only_where_reached_available_and_targeted() {
        let w = world();
        let wide = entry("1", EntryKind::Weekly, "p_front");
        let mut wide_lobby = wide.clone();
        wide_lobby.team_id = "lobby".into();
        assert_eq!(w.applies(&wide_lobby, "s_front"), Some(Match::TeamWide));
        assert_eq!(w.applies(&wide_lobby, "s_back"), None, "team doesn't reach");
        let mut ops = wide_lobby.clone();
        ops.team_id = "ops".into();
        assert_eq!(w.applies(&ops, "s_back"), None, "playlist not available");
        assert_eq!(w.applies(&ops, "s_none"), None, "untagged screens play nothing");
        let mut blank = ops.clone();
        blank.playlist_id = None;
        blank.tag_ids = vec!["back".into()];
        assert_eq!(w.applies(&blank, "s_back"), Some(Match::Tag));
        assert_eq!(w.applies(&blank, "s_front"), None, "tag doesn't match");
        blank.screen_ids = vec!["s_front".into()];
        assert_eq!(w.applies(&blank, "s_front"), Some(Match::Screen));
    }

    #[test]
    fn dst_gap_moves_forward() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let t = local_to_utc(&tz, parse_local("2026-03-08T02:30").unwrap());
        assert_eq!(t, utc("2026-03-08T07:30:00Z"));
    }
}
