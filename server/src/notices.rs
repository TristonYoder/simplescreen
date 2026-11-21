//! Notices (the inbox) and the screen activity log.

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::FromRow;

use crate::{
    auth::Principal,
    error::ApiResult,
    reach::World,
    state::AppState,
    util::{new_id, now},
};

#[derive(Serialize, FromRow)]
pub struct Notice {
    pub id: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(skip)]
    pub detail: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<String>,
}

pub const NOTICE_COLUMNS: &str = "id, kind, screen_id, entry_id, detail, created_at, resolved_at, dismissed_at";

impl Notice {
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v["detail"] = serde_json::from_str(&self.detail).unwrap_or_else(|_| json!({}));
        v
    }
}

/// `user_sub` None addresses every admin.
pub async fn create(
    s: &AppState,
    user_sub: Option<&str>,
    kind: &str,
    screen_id: Option<&str>,
    entry_id: Option<&str>,
    detail: Value,
) -> ApiResult<()> {
    let id = new_id();
    sqlx::query("INSERT INTO notices (id, user_sub, kind, screen_id, entry_id, detail, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(user_sub)
        .bind(kind)
        .bind(screen_id)
        .bind(entry_id)
        .bind(detail.to_string())
        .bind(now())
        .execute(&s.db)
        .await?;
    let n: Notice = sqlx::query_as(&format!("SELECT {NOTICE_COLUMNS} FROM notices WHERE id = ?")).bind(&id).fetch_one(&s.db).await?;
    s.hub.send_user(user_sub, &json!({ "type": "notice", "notice": n.to_json() }));
    Ok(())
}

pub async fn resolve_entry(s: &AppState, entry_id: &str) -> ApiResult<()> {
    sqlx::query("UPDATE notices SET resolved_at = ? WHERE entry_id = ? AND kind = 'schedule_conflict' AND resolved_at IS NULL")
        .bind(now())
        .bind(entry_id)
        .execute(&s.db)
        .await?;
    Ok(())
}

/// Keeps the admins' `untagged_screen` notice in step with the screen's tags.
pub async fn sync_untagged(s: &AppState, screen_id: &str) -> ApiResult<()> {
    let (tags,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM screen_tags WHERE screen_id = ?").bind(screen_id).fetch_one(&s.db).await?;
    let open: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM notices WHERE kind = 'untagged_screen' AND screen_id = ? AND resolved_at IS NULL LIMIT 1",
    )
    .bind(screen_id)
    .fetch_optional(&s.db)
    .await?;
    let exists: Option<(String,)> = sqlx::query_as("SELECT name FROM screens WHERE id = ?").bind(screen_id).fetch_optional(&s.db).await?;
    match (exists, tags, open) {
        (Some((name,)), 0, None) => create(s, None, "untagged_screen", Some(screen_id), None, json!({ "screen_name": name })).await?,
        (None, _, Some(_)) | (Some(_), 1.., Some(_)) => {
            sqlx::query("UPDATE notices SET resolved_at = ? WHERE kind = 'untagged_screen' AND screen_id = ? AND resolved_at IS NULL")
                .bind(now())
                .bind(screen_id)
                .execute(&s.db)
                .await?;
        }
        _ => {}
    }
    Ok(())
}

pub struct Actor {
    pub sub: String,
    pub name: Option<String>,
    pub team_id: Option<String>,
}

impl Actor {
    pub fn of(p: &Principal, w: &World, screen_id: &str) -> Self {
        Actor { sub: p.subject.clone(), name: p.display_name.clone(), team_id: p.acting_team(w, screen_id) }
    }
}

/// Writes one activity log entry and sends `screen_activity` to everyone who reaches the screen.
pub async fn log(s: &AppState, screen_id: &str, kind: &str, actor: &Actor, detail: Value) {
    let id = new_id();
    let stamp = now();
    let res = sqlx::query(
        "INSERT INTO screen_events (id, screen_id, kind, user_sub, user_name, team_id, detail, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(screen_id)
    .bind(kind)
    .bind(&actor.sub)
    .bind(&actor.name)
    .bind(&actor.team_id)
    .bind(detail.to_string())
    .bind(&stamp)
    .execute(&s.db)
    .await;
    if let Err(e) = res {
        tracing::warn!("activity log write failed: {e}");
        return;
    }
    let mut event = json!({ "id": id, "screen_id": screen_id, "kind": kind, "user_sub": actor.sub, "detail": detail, "created_at": stamp });
    if let Some(n) = &actor.name {
        event["user_name"] = json!(n);
    }
    if let Some(t) = &actor.team_id {
        event["team_id"] = json!(t);
    }
    s.broadcast_screen(screen_id, &json!({ "type": "screen_activity", "screen_id": screen_id, "event": event })).await;
}
