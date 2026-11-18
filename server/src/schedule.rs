use std::collections::HashMap;

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;

use crate::{
    error::{ApiError, ApiResult},
    models::{ScreenRow, SCREEN_COLUMNS},
    state::AppState,
};

pub const TIME_FORMAT: &str = "%H:%M";
pub const DATE_FORMAT: &str = "%Y-%m-%d";
pub const LOCAL_FORMAT: &str = "%Y-%m-%dT%H:%M";
pub const DEVICE_HORIZON: Duration = Duration::hours(48);
pub const MAX_RANGE: Duration = Duration::days(14);

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Weekly,
    Once,
}

#[derive(Serialize, Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub group_id: String,
    pub playlist_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub screen_ids: Vec<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Entry {
    pub fn applies_to(&self, screen_id: &str) -> bool {
        self.screen_ids.is_empty() || self.screen_ids.iter().any(|s| s == screen_id)
    }
}

#[derive(FromRow)]
pub struct EntryRow {
    id: String,
    group_id: String,
    playlist_id: Option<String>,
    title: Option<String>,
    screen_ids: String,
    kind: String,
    enabled: bool,
    days: String,
    start_time: Option<String>,
    end_time: Option<String>,
    starts_on: Option<String>,
    ends_on: Option<String>,
    starts_at: Option<String>,
    ends_at: Option<String>,
    created_at: String,
    updated_at: String,
}

pub const ENTRY_COLUMNS: &str = "id, group_id, playlist_id, title, screen_ids, kind, enabled, days, start_time, end_time, \
     starts_on, ends_on, starts_at, ends_at, created_at, updated_at";

impl EntryRow {
    pub fn into_entry(self) -> Entry {
        Entry {
            id: self.id,
            group_id: self.group_id,
            playlist_id: self.playlist_id,
            title: self.title,
            screen_ids: serde_json::from_str(&self.screen_ids).unwrap_or_default(),
            kind: if self.kind == "once" { EntryKind::Once } else { EntryKind::Weekly },
            enabled: self.enabled,
            days: serde_json::from_str(&self.days).unwrap_or_default(),
            start_time: self.start_time,
            end_time: self.end_time,
            starts_on: self.starts_on,
            ends_on: self.ends_on,
            starts_at: self.starts_at,
            ends_at: self.ends_at,
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

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Segment {
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub playlist_id: Option<String>,
    pub source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Override {
    pub playlist_id: Option<String>,
    pub until: Option<DateTime<Utc>>,
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
            let mut day = from.with_timezone(tz).date_naive() - Duration::days(1);
            let last = to.with_timezone(tz).date_naive();
            let mut out = vec![];
            while day <= last {
                let in_range = starts_on.is_none_or(|d| day >= d) && ends_on.is_none_or(|d| day <= d);
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
    rank: (Source, bool, &'a str),
    playlist_id: Option<String>,
    entry_id: Option<String>,
}

/// Contiguous segments covering `[from, to)` for one screen. `entries` must already apply to the screen.
pub fn resolve(
    entries: &[&Entry],
    default: Option<&str>,
    ovr: Option<&Override>,
    tz: &Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Segment> {
    if to <= from {
        return vec![];
    }
    let mut cands: Vec<Candidate> = vec![];
    for e in entries {
        let tier = match e.kind {
            EntryKind::Once => Source::Once,
            EntryKind::Weekly => Source::Weekly,
        };
        for (s, t) in occurrences(e, tz, from, to) {
            cands.push(Candidate {
                start: s.max(from),
                end: t.min(to),
                rank: (tier, !e.screen_ids.is_empty(), &e.created_at),
                playlist_id: e.playlist_id.clone(),
                entry_id: Some(e.id.clone()),
            });
        }
    }
    if let Some(o) = ovr {
        let end = o.until.unwrap_or(to).min(to);
        if end > from {
            cands.push(Candidate {
                start: from,
                end,
                rank: (Source::Override, true, ""),
                playlist_id: o.playlist_id.clone(),
                entry_id: None,
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
                entry_id: c.entry_id.clone(),
            },
            None => Segment {
                starts_at: a,
                ends_at: b,
                playlist_id: default.map(String::from),
                source: Source::Default,
                entry_id: None,
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

pub fn server_tz(s: &AppState) -> Tz {
    parse_tz(&s.cfg.timezone).unwrap_or(Tz::UTC)
}

pub async fn group_tz(s: &AppState, group_id: &str) -> ApiResult<Tz> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT timezone FROM groups WHERE id = ?").bind(group_id).fetch_optional(&s.db).await?;
    Ok(row.and_then(|r| r.0).and_then(|t| parse_tz(&t)).unwrap_or_else(|| server_tz(s)))
}

pub async fn group_entries(s: &AppState, group_id: &str) -> ApiResult<Vec<Entry>> {
    let rows: Vec<EntryRow> =
        sqlx::query_as(&format!("SELECT {ENTRY_COLUMNS} FROM schedule_entries WHERE group_id = ? ORDER BY created_at"))
            .bind(group_id)
            .fetch_all(&s.db)
            .await?;
    Ok(rows.into_iter().map(EntryRow::into_entry).collect())
}

pub fn screen_override(row: &ScreenRow) -> Option<Override> {
    if !row.override_set {
        return None;
    }
    Some(Override {
        playlist_id: row.override_playlist_id.clone(),
        until: row.override_until.as_deref().and_then(|u| DateTime::parse_from_rfc3339(u).ok()).map(|d| d.to_utc()),
    })
}

pub fn screen_segments(
    row: &ScreenRow,
    entries: &[Entry],
    tz: &Tz,
    with_override: bool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Vec<Segment> {
    let applying: Vec<&Entry> = entries.iter().filter(|e| e.applies_to(&row.id)).collect();
    let ovr = if with_override { screen_override(row) } else { None };
    resolve(&applying, row.playlist_id.as_deref(), ovr.as_ref(), tz, from, to)
}

pub async fn screen_timeline(s: &AppState, row: &ScreenRow, from: DateTime<Utc>, to: DateTime<Utc>) -> ApiResult<Vec<Segment>> {
    let tz = group_tz(s, &row.group_id).await?;
    let entries = group_entries(s, &row.group_id).await?;
    Ok(screen_segments(row, &entries, &tz, true, from, to))
}

/// Schedule without the override for the next 14 days.
pub async fn base_timeline(s: &AppState, row: &ScreenRow, now: DateTime<Utc>) -> ApiResult<Vec<Segment>> {
    let tz = group_tz(s, &row.group_id).await?;
    let entries = group_entries(s, &row.group_id).await?;
    Ok(screen_segments(row, &entries, &tz, false, now, now + MAX_RANGE))
}

pub async fn notify_group(s: &AppState, group_id: &str) {
    let ids: Vec<(String,)> = sqlx::query_as("SELECT id FROM screens WHERE group_id = ?")
        .bind(group_id)
        .fetch_all(&s.db)
        .await
        .unwrap_or_default();
    for (id,) in ids {
        s.notify_manifest(&id, 0);
    }
    s.schedule_wake.notify_one();
}

pub fn broadcast_changed(s: &AppState, group_id: &str) {
    s.hub.broadcast_group(group_id, &json!({ "type": "schedule_changed", "group_id": group_id }));
}

pub fn invalid(message: &str) -> ApiError {
    ApiError::unprocessable("invalid_schedule", message)
}

// MARK: Boundary pushes

/// Pushes `manifest_changed` when a screen's active playlist changes and clears expired overrides.
pub async fn run(s: AppState) {
    let mut shutdown = s.shutdown.subscribe();
    let mut active: HashMap<String, Option<String>> = HashMap::new();
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
    active: &mut HashMap<String, Option<String>>,
    next: &mut DateTime<Utc>,
) -> ApiResult<()> {
    let stamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let expired: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, group_id FROM screens WHERE override_set = 1 AND override_until IS NOT NULL AND override_until <= ?",
    )
    .bind(&stamp)
    .fetch_all(&s.db)
    .await?;
    for (id, group_id) in &expired {
        sqlx::query("UPDATE screens SET override_set = 0, override_playlist_id = NULL, override_until = NULL, override_set_at = NULL WHERE id = ?")
            .bind(id)
            .execute(&s.db)
            .await?;
        s.hub.broadcast_group(group_id, &json!({ "type": "screen_updated", "screen_id": id }));
    }

    let rows: Vec<ScreenRow> = sqlx::query_as(&format!("SELECT {SCREEN_COLUMNS} FROM screens")).fetch_all(&s.db).await?;
    let mut groups: HashMap<String, (Tz, Vec<Entry>)> = HashMap::new();
    for row in &rows {
        if !groups.contains_key(&row.group_id) {
            let tz = group_tz(s, &row.group_id).await?;
            let entries = group_entries(s, &row.group_id).await?;
            groups.insert(row.group_id.clone(), (tz, entries));
        }
        let (tz, entries) = &groups[&row.group_id];
        let segs = screen_segments(row, entries, tz, true, now, now + DEVICE_HORIZON);
        let current = segs.first().and_then(|g| g.playlist_id.clone());
        if let Some(previous) = active.insert(row.id.clone(), current.clone()) {
            if previous != current {
                s.notify_manifest(&row.id, 0);
                s.hub.broadcast_group(&row.group_id, &json!({ "type": "screen_updated", "screen_id": row.id }));
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
            group_id: "g".into(),
            playlist_id: Some(playlist.into()),
            title: None,
            screen_ids: vec![],
            kind,
            enabled: true,
            days: vec![],
            start_time: None,
            end_time: None,
            starts_on: None,
            ends_on: None,
            starts_at: None,
            ends_at: None,
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
            &[&breakfast, &dinner, &launch],
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
        let segs = resolve(&[&late], None, None, &Tz::UTC, utc("2026-11-21T20:00:00Z"), utc("2026-11-22T04:00:00Z"));
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[1].playlist_id.as_deref(), Some("late"));
        assert_eq!(segs[1].ends_at, utc("2026-11-22T02:00:00Z"));
        assert_eq!(segs[0].playlist_id, None);
    }

    #[test]
    fn screen_specific_beats_group_wide_then_newest() {
        let group_wide = weekly("2", "group", &[1, 2, 3, 4, 5, 6, 7], "00:00", "00:00");
        let mut specific = weekly("1", "specific", &[1, 2, 3, 4, 5, 6, 7], "09:00", "10:00");
        specific.screen_ids = vec!["s1".into()];
        let newer = weekly("3", "newer", &[1, 2, 3, 4, 5, 6, 7], "09:30", "11:00");
        let segs = resolve(&[&group_wide, &specific, &newer], None, None, &Tz::UTC, utc("2026-11-18T08:00:00Z"), utc("2026-11-18T12:00:00Z"));
        let names: Vec<_> = segs.iter().map(|g| g.playlist_id.clone().unwrap()).collect();
        assert_eq!(names, vec!["group", "specific", "newer", "group"]);
    }

    #[test]
    fn override_wins_until_expiry() {
        let lunch = weekly("1", "lunch", &[3], "11:00", "14:00");
        let o = Override { playlist_id: Some("manual".into()), until: Some(utc("2026-11-18T12:00:00Z")) };
        let segs = resolve(&[&lunch], Some("house"), Some(&o), &Tz::UTC, utc("2026-11-18T10:00:00Z"), utc("2026-11-18T15:00:00Z"));
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
        let segs = resolve(&[&e], None, None, &tz, utc("2026-11-19T00:00:00Z"), utc("2026-11-23T00:00:00Z"));
        let on: Vec<_> = segs.iter().filter(|g| g.playlist_id.is_some()).map(|g| g.starts_at).collect();
        assert_eq!(on, vec![utc("2026-11-20T14:00:00Z"), utc("2026-11-21T14:00:00Z")]);
    }

    #[test]
    fn dst_gap_moves_forward() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let t = local_to_utc(&tz, parse_local("2026-03-08T02:30").unwrap());
        assert_eq!(t, utc("2026-03-08T07:30:00Z"));
    }
}
