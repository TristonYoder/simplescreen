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
    models::ScreenStatus,
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
    pub shutdown: watch::Sender<bool>,
    pub schedule_wake: Notify,
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

    pub async fn require_group(&self, p: &Principal, group_id: &str, min: Role) -> Result<(), ApiError> {
        p.require(group_id, min)?;
        let found: Option<(String,)> =
            sqlx::query_as("SELECT id FROM groups WHERE id = ?").bind(group_id).fetch_optional(&self.db).await?;
        found.map(|_| ()).ok_or_else(ApiError::not_found)
    }

    pub fn forget_device_tokens(&self, screen_id: &str) {
        self.devices.lock().retain(|_, d| d.screen_id != screen_id);
    }

    /// Any screen in the group may have the playlist scheduled; devices refetch with If-None-Match.
    pub async fn notify_screens_for_playlist(&self, group_id: &str, version: i64) {
        let ids: Vec<(String,)> = sqlx::query_as("SELECT id FROM screens WHERE group_id = ?")
            .bind(group_id)
            .fetch_all(&self.db)
            .await
            .unwrap_or_default();
        for (id,) in ids {
            self.notify_manifest(&id, version);
        }
    }

    pub fn notify_manifest(&self, screen_id: &str, version: i64) {
        self.hub
            .send_device(screen_id, &serde_json::json!({ "type": "manifest_changed", "version": version }));
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
        shutdown,
        schedule_wake: Notify::new(),
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

const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql"), include_str!("../migrations/0002_schedule.sql"),
    include_str!("../migrations/0003_skip_dates.sql"),
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
