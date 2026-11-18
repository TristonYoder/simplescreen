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
        Ok(cfg)
    }
}
