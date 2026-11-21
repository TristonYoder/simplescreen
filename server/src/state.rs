use std::{collections::HashMap, ops::Deref, path::PathBuf, sync::Arc, time::Duration};

use parking_lot::Mutex;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
    SqlitePool,
};
use tokio::sync::{watch, Notify};

use crate::{
    auth::{DeviceInfo, Oidc, Principal, Role},
    error::ApiError,
    config::Config,
    hub::Hub,
    models::{NodeStatus, ScreenStatus},
    util::{new_id, random_token},
};

pub struct Inner {
    pub cfg: Config,
    pub db: SqlitePool,
    pub server_id: String,
    pub bootstrap: Option<String>,
    pub hub: Hub,
    pub oidc: Option<Oidc>,
    pub devices: Mutex<HashMap<String, DeviceInfo>>,
    pub touched: Mutex<HashMap<String, std::time::Instant>>,
    pub statuses: Mutex<HashMap<String, ScreenStatus>>,
    pub node_statuses: Mutex<HashMap<String, NodeStatus>>,
    pub shutdown: watch::Sender<bool>,
    pub schedule_wake: Notify,
    pub org: Mutex<StoredOrg>,
    /// Serializes `PATCH /v1/settings` across the read, the write, and the in-memory update.
    pub settings_write: tokio::sync::Mutex<()>,
}

/// Org settings written by first-run setup (`PATCH /v1/settings`); env vars win over these.
#[derive(Clone, Default)]
pub struct StoredOrg {
    pub name: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Clone)]
pub struct AppState(pub Arc<Inner>);

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub fn blob_path(&self, sha: &str) -> PathBuf {
        self.cfg.data_dir.join("blobs").join(&sha[..2]).join(sha)
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.cfg.data_dir.join("tmp")
    }

    pub async fn require_team(&self, p: &Principal, team_id: &str, min: Role) -> Result<(), ApiError> {
        p.require(team_id, min)?;
        let found: Option<(String,)> =
            sqlx::query_as("SELECT id FROM teams WHERE id = ?").bind(team_id).fetch_optional(&self.db).await?;
        found.map(|_| ()).ok_or_else(ApiError::not_found)
    }

    /// The org's id: this installation's `server_id`.
    pub fn org_id(&self) -> &str {
        &self.server_id
    }

    pub fn org_name(&self) -> String {
        self.cfg
            .org_name
            .clone()
            .or_else(|| self.org.lock().name.clone())
            .unwrap_or_else(|| crate::BRAND_NAME.to_string())
    }

    /// Whether the org name has been set by env or setup.
    pub fn org_configured(&self) -> bool {
        self.cfg.org_name.is_some() || self.org.lock().name.is_some()
    }

    pub fn org_tz(&self) -> chrono_tz::Tz {
        self.cfg
            .org_timezone
            .clone()
            .or_else(|| self.org.lock().timezone.clone())
            .and_then(|t| t.parse().ok())
            .or_else(|| self.cfg.default_timezone.parse().ok())
            .unwrap_or(chrono_tz::Tz::UTC)
    }

    pub fn forget_device_tokens(&self, node_id: &str) {
        self.devices.lock().retain(|_, d| d.node_id != node_id);
    }

    /// Notifies every screen in the team; devices refetch with If-None-Match.
    pub async fn notify_screens_for_playlist(&self, team_id: &str, version: i64) {
        let ids: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id, node_id FROM screens WHERE team_id = ?")
            .bind(team_id)
            .fetch_all(&self.db)
            .await
            .unwrap_or_default();
        for (id, node_id) in ids {
            self.notify_manifest(node_id.as_deref(), &id, version);
        }
    }

    /// Tells the node that plays `screen_id` to refetch that screen's manifest.
    pub fn notify_manifest(&self, node_id: Option<&str>, screen_id: &str, version: i64) {
        let msg = serde_json::json!({ "type": "manifest_changed", "screen_id": screen_id, "version": version });
        if let Some(node_id) = node_id {
            self.hub.send_node(node_id, &msg);
        }
        self.hub.send_subscribers(screen_id, node_id, &msg);
    }
}

pub async fn build_state(cfg: Config) -> anyhow::Result<AppState> {
    for sub in ["blobs", "tmp"] {
        tokio::fs::create_dir_all(cfg.data_dir.join(sub)).await?;
    }
    if let Ok(mut rd) = tokio::fs::read_dir(cfg.data_dir.join("tmp")).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }

    let opts = SqliteConnectOptions::new()
        .filename(cfg.data_dir.join("db.sqlite"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));
    let db = SqlitePoolOptions::new().max_connections(8).connect_with(opts).await?;
    migrate(&db).await?;

    let existing: Option<(String,)> =
        sqlx::query_as("SELECT value FROM settings WHERE key = 'server_id'").fetch_optional(&db).await?;
    let server_id = match existing {
        Some((v,)) => v,
        None => {
            let id = new_id();
            sqlx::query("INSERT INTO settings (key, value) VALUES ('server_id', ?)").bind(&id).execute(&db).await?;
            id
        }
    };

    let stored: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM settings WHERE key IN ('org_name', 'org_timezone')").fetch_all(&db).await?;
    let mut org = StoredOrg::default();
    for (k, v) in stored {
        match k.as_str() {
            "org_name" => org.name = Some(v),
            _ => org.timezone = Some(v),
        }
    }

    let oidc = cfg.oidc_issuer.clone().map(|issuer| {
        Oidc::new(
            issuer,
            cfg.oidc_client_id.clone(),
            cfg.oidc_audience.clone(),
            cfg.oidc_groups_claim.clone(),
            cfg.oidc_admin_groups.clone(),
        )
    });

    let bootstrap = match &cfg.bootstrap_token_file {
        Some(path) => Some(std::fs::read_to_string(path)?.trim().to_string()),
        None if oidc.is_none() => Some(load_or_create_bootstrap(&cfg.data_dir)?),
        None => None,
    };
    if bootstrap.as_deref() == Some("") {
        anyhow::bail!("bootstrap token file is empty");
    }

    let (shutdown, _) = watch::channel(false);
    Ok(AppState(Arc::new(Inner {
        cfg,
        db,
        server_id,
        bootstrap,
        hub: Hub::default(),
        oidc,
        devices: Mutex::default(),
        touched: Mutex::default(),
        statuses: Mutex::default(),
        node_statuses: Mutex::default(),
        shutdown,
        schedule_wake: Notify::new(),
        org: Mutex::new(org),
        settings_write: tokio::sync::Mutex::new(()),
    })))
}

fn load_or_create_bootstrap(dir: &std::path::Path) -> anyhow::Result<String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let path = dir.join("bootstrap-token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    let token = random_token(32);
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    f.write_all(token.as_bytes())?;
    tracing::info!("generated bootstrap token at {}", path.display());
    Ok(token)
}

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_schedule.sql"),
    include_str!("../migrations/0003_skip_dates.sql"),
    include_str!("../migrations/0004_floor_plans.sql"),
    include_str!("../migrations/0005_teams.sql"),
    include_str!("../migrations/0006_nodes.sql"),
];

async fn migrate(db: &SqlitePool) -> anyhow::Result<()> {
    let (current,): (i64,) = sqlx::query_as("PRAGMA user_version").fetch_one(db).await?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let mut tx = db.begin().await?;
        sqlx::raw_sql(sql).execute(&mut *tx).await?;
        sqlx::raw_sql(&format!("PRAGMA user_version = {}", i + 1)).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}
