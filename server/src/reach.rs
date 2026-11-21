//! Reach and availability: which teams reach which screens, and which playlists show on them.

use std::collections::{HashMap, HashSet};

use crate::{
    auth::{Principal, Role},
    error::{ApiError, ApiResult},
    schedule::Entry,
    state::AppState,
};

#[derive(Clone, Debug, Default)]
pub struct TeamInfo {
    pub name: String,
    pub all: bool,
    pub tags: HashSet<String>,
}

/// How an entry matches a screen; higher beats lower within a tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Match {
    TeamWide,
    Tag,
    Screen,
}

/// A snapshot of the org's tags and reach, loaded per request.
#[derive(Default)]
pub struct World {
    pub teams: HashMap<String, TeamInfo>,
    pub screen_tags: HashMap<String, HashSet<String>>,
    pub playlist_tags: HashMap<String, HashSet<String>>,
    pub playlist_team: HashMap<String, String>,
}

impl World {
    pub async fn load(s: &AppState) -> ApiResult<World> {
        let mut w = World::default();
        let teams: Vec<(String, String, String)> = sqlx::query_as("SELECT id, name, screen_access FROM teams").fetch_all(&s.db).await?;
        for (id, name, access) in teams {
            w.teams.insert(id, TeamInfo { name, all: access == "all", tags: HashSet::new() });
        }
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT team_id, tag_id FROM team_tags").fetch_all(&s.db).await?;
        for (team, tag) in rows {
            if let Some(t) = w.teams.get_mut(&team) {
                t.tags.insert(tag);
            }
        }
        let screens: Vec<(String,)> = sqlx::query_as("SELECT id FROM screens").fetch_all(&s.db).await?;
        for (id,) in screens {
            w.screen_tags.insert(id, HashSet::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT screen_id, tag_id FROM screen_tags").fetch_all(&s.db).await?;
        for (screen, tag) in rows {
            w.screen_tags.entry(screen).or_default().insert(tag);
        }
        let playlists: Vec<(String, String)> = sqlx::query_as("SELECT id, team_id FROM playlists").fetch_all(&s.db).await?;
        for (id, team) in playlists {
            w.playlist_tags.insert(id.clone(), HashSet::new());
            w.playlist_team.insert(id, team);
        }
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT playlist_id, tag_id FROM playlist_tags").fetch_all(&s.db).await?;
        for (pl, tag) in rows {
            w.playlist_tags.entry(pl).or_default().insert(tag);
        }
        Ok(w)
    }

    fn tags_of(&self, screen: &str) -> Option<&HashSet<String>> {
        self.screen_tags.get(screen)
    }

    pub fn untagged(&self, screen: &str) -> bool {
        self.tags_of(screen).is_none_or(|t| t.is_empty())
    }

    /// Untagged screens grant nothing: no team reaches them, `screen_access: all` included.
    pub fn reaches(&self, team: &str, screen: &str) -> bool {
        let (Some(t), Some(tags)) = (self.teams.get(team), self.tags_of(screen)) else { return false };
        !tags.is_empty() && (t.all || !t.tags.is_disjoint(tags))
    }

    pub fn screen_teams(&self, screen: &str) -> Vec<String> {
        let mut ids: Vec<String> = self.teams.keys().filter(|t| self.reaches(t, screen)).cloned().collect();
        ids.sort();
        ids
    }

    pub fn team_screens(&self, team: &str) -> Vec<String> {
        let mut ids: Vec<String> = self.screen_tags.keys().filter(|s| self.reaches(team, s)).cloned().collect();
        ids.sort();
        ids
    }

    pub fn available(&self, playlist: &str, screen: &str) -> bool {
        match (self.playlist_tags.get(playlist), self.tags_of(screen)) {
            (Some(p), Some(s)) => !p.is_disjoint(s),
            _ => false,
        }
    }

    pub fn available_on(&self, screen: &str) -> Vec<String> {
        let mut ids: Vec<String> = self.playlist_tags.keys().filter(|p| self.available(p, screen)).cloned().collect();
        ids.sort();
        ids
    }

    /// Screens a playlist is available on.
    pub fn playlist_screens(&self, playlist: &str) -> Vec<String> {
        self.screen_tags.keys().filter(|s| self.available(playlist, s)).cloned().collect()
    }

    /// Whether `team` may put `tag` on its playlists or entries.
    pub fn team_holds(&self, team: &str, tag: &str) -> bool {
        self.teams.get(team).is_some_and(|t| t.all || t.tags.contains(tag))
    }

    /// How an entry targets a screen, ignoring reach and availability.
    pub fn target(&self, e: &Entry, screen: &str) -> Option<Match> {
        if e.screen_ids.is_empty() && e.tag_ids.is_empty() {
            return Some(Match::TeamWide);
        }
        if e.screen_ids.iter().any(|s| s == screen) {
            return Some(Match::Screen);
        }
        let tags = self.tags_of(screen)?;
        e.tag_ids.iter().any(|t| tags.contains(t)).then_some(Match::Tag)
    }

    /// Whether an entry plays on a screen: its team reaches the screen, it targets it, and its playlist
    /// (if any) is available there. Untagged screens play nothing.
    pub fn applies(&self, e: &Entry, screen: &str) -> Option<Match> {
        if self.untagged(screen) || !self.reaches(&e.team_id, screen) {
            return None;
        }
        if e.playlist_id.as_deref().is_some_and(|p| !self.available(p, screen)) {
            return None;
        }
        self.target(e, screen)
    }
}

impl Principal {
    pub fn sees_screen(&self, w: &World, screen: &str) -> bool {
        self.server_admin || self.memberships.iter().any(|m| w.reaches(&m.team_id, screen))
    }

    pub fn operates_screen(&self, w: &World, screen: &str) -> bool {
        self.server_admin || self.memberships.iter().any(|m| m.role >= Role::Editor && w.reaches(&m.team_id, screen))
    }

    /// The team an operator acts for on a screen (for the activity log).
    pub fn acting_team(&self, w: &World, screen: &str) -> Option<String> {
        self.memberships
            .iter()
            .find(|m| m.role >= Role::Editor && w.reaches(&m.team_id, screen))
            .map(|m| m.team_id.clone())
    }

    pub fn edits_team(&self, team: &str) -> bool {
        self.role_in(team).is_some_and(|r| r >= Role::Editor)
    }

    /// 404 if the caller can't see the screen, 403 if they see it but can't operate it.
    pub fn require_operate(&self, w: &World, screen: &str) -> ApiResult<()> {
        if !self.sees_screen(w, screen) {
            return Err(ApiError::not_found());
        }
        if !self.operates_screen(w, screen) {
            return Err(ApiError::forbidden());
        }
        Ok(())
    }

    pub fn require_admin(&self) -> ApiResult<()> {
        if self.server_admin {
            Ok(())
        } else {
            Err(ApiError::forbidden())
        }
    }
}

/// Teams reaching a screen, straight from the database (for broadcasts outside a request).
pub async fn screen_team_ids(s: &AppState, screen: &str) -> ApiResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT id FROM teams WHERE screen_access = 'all' AND EXISTS (SELECT 1 FROM screen_tags WHERE screen_id = ?1) \
         UNION SELECT tt.team_id FROM team_tags tt JOIN screen_tags st ON st.tag_id = tt.tag_id WHERE st.screen_id = ?1",
    )
    .bind(screen)
    .fetch_all(&s.db)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Validates tag ids: all must exist. Returns them deduplicated.
pub async fn check_tags(s: &AppState, ids: &[String]) -> ApiResult<Vec<String>> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for id in ids {
        if !seen.insert(id.clone()) {
            continue;
        }
        let found: Option<(String,)> = sqlx::query_as("SELECT id FROM tags WHERE id = ?").bind(id).fetch_optional(&s.db).await?;
        if found.is_none() {
            return Err(ApiError::unprocessable("invalid_tag", format!("tag {id} does not exist")));
        }
        out.push(id.clone());
    }
    Ok(out)
}
