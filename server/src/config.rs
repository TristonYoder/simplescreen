use std::{collections::HashMap, path::PathBuf};

use crate::{BRAND_DEFAULT_PORT, BRAND_SLUG};

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub bind: String,
    pub data_dir: PathBuf,
    pub bootstrap_token_file: Option<PathBuf>,
    pub oidc_issuer: Option<String>,
    pub oidc_client_id: Option<String>,
    pub oidc_audience: Option<String>,
    pub oidc_groups_claim: String,
    pub oidc_admin_groups: Vec<String>,
    pub oidc_token_type: String,
    pub bonjour: bool,
    /// Org display name from `<SLUG>_ORG_NAME`; wins over the stored setting.
    pub org_name: Option<String>,
    /// Org IANA zone from `<SLUG>_ORG_TIMEZONE`; wins over the stored setting.
    pub org_timezone: Option<String>,
    /// Zone used when neither env nor setup set one: the host zone, else UTC.
    pub default_timezone: String,
    /// `unix:/path` listens on a unix socket instead of `bind`:`port`.
    pub listen: Option<String>,
    /// Externally reachable base URL (behind a reverse proxy or gateway).
    pub public_url: Option<String>,
    /// Directories local libraries may live under (`<SLUG>_LIBRARY_ROOTS`, comma-separated).
    pub library_roots: Vec<PathBuf>,
}

impl Config {
    pub fn defaults(data_dir: PathBuf) -> Self {
        Self {
            port: BRAND_DEFAULT_PORT.parse().unwrap_or(8420),
            bind: "0.0.0.0".into(),
            data_dir,
            bootstrap_token_file: None,
            oidc_issuer: None,
            oidc_client_id: None,
            oidc_audience: None,
            oidc_groups_claim: "groups".into(),
            oidc_admin_groups: vec![],
            oidc_token_type: "access_token".into(),
            bonjour: true,
            org_name: None,
            org_timezone: None,
            listen: None,
            library_roots: vec![],
            public_url: None,
            default_timezone: iana_time_zone::get_timezone().ok().filter(|t| t.parse::<chrono_tz::Tz>().is_ok()).unwrap_or_else(|| "UTC".into()),
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let prefix = BRAND_SLUG.to_uppercase();
        let file: HashMap<String, String> = match std::env::var(format!("{prefix}_CONFIG")) {
            Ok(path) => {
                let text = std::fs::read_to_string(&path)?;
                let table: toml::Table = text.parse()?;
                table
                    .into_iter()
                    .map(|(k, v)| {
                        let s = match v {
                            toml::Value::String(s) => s,
                            toml::Value::Array(a) => a
                                .iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect::<Vec<_>>()
                                .join(","),
                            other => other.to_string(),
                        };
                        (k.to_lowercase(), s)
                    })
                    .collect()
            }
            Err(_) => HashMap::new(),
        };
        let get = |key: &str| -> Option<String> {
            std::env::var(format!("{prefix}_{}", key.to_uppercase()))
                .ok()
                .or_else(|| file.get(key).cloned())
                .filter(|s| !s.is_empty())
        };
        let mut cfg = Self::defaults(PathBuf::from(get("data_dir").unwrap_or_else(|| "./data".into())));
        if let Some(p) = get("port") {
            cfg.port = p.parse()?;
        }
        if let Some(b) = get("bind") {
            cfg.bind = b;
        }
        cfg.bootstrap_token_file = get("bootstrap_token_file").map(PathBuf::from);
        cfg.oidc_issuer = get("oidc_issuer");
        cfg.oidc_client_id = get("oidc_client_id");
        cfg.oidc_audience = get("oidc_audience");
        if let Some(c) = get("oidc_groups_claim") {
            cfg.oidc_groups_claim = c;
        }
        if let Some(g) = get("oidc_admin_groups") {
            cfg.oidc_admin_groups =
                g.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
        if let Some(t) = get("oidc_token_type") {
            anyhow::ensure!(t == "access_token" || t == "id_token", "invalid oidc_token_type");
            cfg.oidc_token_type = t;
        }
        if let Some(b) = get("bonjour") {
            cfg.bonjour = matches!(b.to_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        cfg.org_name = get("org_name").map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
        if let Some(t) = get("org_timezone") {
            anyhow::ensure!(t.parse::<chrono_tz::Tz>().is_ok(), "invalid org_timezone {t}");
            cfg.org_timezone = Some(t);
        }
        if get("timezone").is_some() {
            anyhow::bail!(
                "{prefix}_TIMEZONE was renamed to {prefix}_ORG_TIMEZONE; set only the new variable (the old one is refused, not read)"
            );
        }
        if let Some(l) = get("listen") {
            anyhow::ensure!(
                l.strip_prefix("unix:").is_some_and(|p| !p.is_empty()),
                "listen must be unix:/path (use bind and port for TCP)"
            );
            cfg.listen = Some(l);
        }
        cfg.public_url = get("public_url").map(|u| u.trim_end_matches('/').to_string());
        if let Some(r) = get("library_roots") {
            for root in r.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let p = PathBuf::from(root);
                anyhow::ensure!(p.is_absolute(), "library_roots entries must be absolute paths: {root}");
                cfg.library_roots.push(p);
            }
        }
        Ok(cfg)
    }
}
