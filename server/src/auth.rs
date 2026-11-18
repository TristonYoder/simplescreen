use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use axum::{extract::FromRequestParts, http::request::Parts};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;

use crate::{
    error::ApiError,
    models::OidcBinding,
    state::AppState,
    util::{now, sha256_hex},
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Viewer,
    Editor,
    Admin,
}

#[derive(Clone, Debug, Serialize)]
pub struct Membership {
    pub group_id: String,
    pub group_name: String,
    pub role: Role,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    User,
    Bootstrap,
    Device,
}

#[derive(Clone, Debug)]
pub struct Principal {
    pub kind: Kind,
    pub subject: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub server_admin: bool,
    pub memberships: Vec<Membership>,
    pub screen_id: Option<String>,
    pub device_group: Option<String>,
}

impl Principal {
    pub fn role_in(&self, group_id: &str) -> Option<Role> {
        if self.server_admin {
            return Some(Role::Admin);
        }
        self.memberships.iter().find(|m| m.group_id == group_id).map(|m| m.role)
    }

    pub fn can_view(&self, group_id: &str) -> bool {
        self.role_in(group_id).is_some()
    }

    pub fn require(&self, group_id: &str, min: Role) -> Result<(), ApiError> {
        match self.role_in(group_id) {
            None => Err(ApiError::not_found()),
            Some(r) if r >= min => Ok(()),
            Some(_) => Err(ApiError::forbidden()),
        }
    }

    pub fn is_editor_anywhere(&self) -> bool {
        self.server_admin || self.memberships.iter().any(|m| m.role >= Role::Editor)
    }

    pub fn is_device(&self) -> bool {
        self.kind == Kind::Device
    }
}

#[derive(Clone)]
pub struct DeviceInfo {
    pub screen_id: String,
    pub group_id: String,
}

impl<S> FromRequestParts<S> for Principal
where
    S: Send + Sync,
    AppState: axum::extract::FromRef<S>,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let state = AppState::from_ref(state);
        let token = bearer(parts).or_else(|| query_token(parts)).ok_or_else(ApiError::unauthorized)?;
        authenticate(&state, &token).await
    }
}

use axum::extract::FromRef;

fn bearer(parts: &Parts) -> Option<String> {
    let value = parts.headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim().to_string())
}

fn query_token(parts: &Parts) -> Option<String> {
    parts
        .uri
        .query()?
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .map(|s| s.to_string())
}

pub async fn authenticate(state: &AppState, token: &str) -> Result<Principal, ApiError> {
    if let Some(boot) = &state.bootstrap {
        if bool::from(boot.as_bytes().ct_eq(token.as_bytes())) {
            return Ok(Principal {
                kind: Kind::Bootstrap,
                subject: "bootstrap".into(),
                display_name: Some("Bootstrap".into()),
                email: None,
                server_admin: true,
                memberships: vec![],
                screen_id: None,
                device_group: None,
            });
        }
    }

    let hash = sha256_hex(token.as_bytes());
    let cached = state.devices.lock().get(&hash).cloned();
    let device = match cached {
        Some(d) => Some(d),
        None => {
            let row: Option<(String, String)> =
                sqlx::query_as("SELECT id, group_id FROM screens WHERE token_hash = ?")
                    .bind(&hash)
                    .fetch_optional(&state.db)
                    .await?;
            row.map(|(screen_id, group_id)| {
                let d = DeviceInfo { screen_id, group_id };
                state.devices.lock().insert(hash.clone(), d.clone());
                d
            })
        }
    };
    if let Some(d) = device {
        return Ok(Principal {
            kind: Kind::Device,
            subject: d.screen_id.clone(),
            display_name: None,
            email: None,
            server_admin: false,
            memberships: vec![],
            screen_id: Some(d.screen_id),
            device_group: Some(d.group_id),
        });
    }

    if let Some(oidc) = &state.oidc {
        if token.matches('.').count() == 2 {
            let claims = oidc.validate(token).await?;
            return user_principal(state, oidc, &claims).await;
        }
    }
    Err(ApiError::unauthorized())
}

async fn user_principal(state: &AppState, oidc: &Oidc, claims: &Value) -> Result<Principal, ApiError> {
    let subject = claims["sub"].as_str().ok_or_else(ApiError::unauthorized)?.to_string();
    let display_name = claims["name"]
        .as_str()
        .or_else(|| claims["preferred_username"].as_str())
        .map(String::from);
    let email = claims["email"].as_str().map(String::from);
    let groups: Vec<String> = match claims.get(&oidc.groups_claim) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    };
    let server_admin = groups.iter().any(|g| oidc.admin_groups.contains(g));

    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, name, oidc_bindings FROM groups").fetch_all(&state.db).await?;
    let mut memberships = vec![];
    for (id, name, bindings) in rows {
        let bindings: Vec<OidcBinding> = serde_json::from_str(&bindings).unwrap_or_default();
        let best = bindings.iter().filter(|b| groups.contains(&b.claim_value)).map(|b| b.role).max();
        if let Some(role) = best {
            memberships.push(Membership { group_id: id, group_name: name, role });
        }
    }

    let due = {
        let mut touched = state.touched.lock();
        let due = touched.get(&subject).is_none_or(|t| t.elapsed() > Duration::from_secs(60));
        if due {
            touched.insert(subject.clone(), Instant::now());
        }
        due
    };
    if due {
        let res = sqlx::query(
            "INSERT INTO users (subject, display_name, email, last_seen_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(subject) DO UPDATE SET display_name = ?2, email = ?3, last_seen_at = ?4",
        )
        .bind(&subject)
        .bind(&display_name)
        .bind(&email)
        .bind(now())
        .execute(&state.db)
        .await;
        if let Err(e) = res {
            tracing::warn!("user upsert failed: {e}");
        }
    }

    Ok(Principal {
        kind: Kind::User,
        subject,
        display_name,
        email,
        server_admin,
        memberships,
        screen_id: None,
        device_group: None,
    })
}

pub struct Oidc {
    pub issuer: String,
    pub client_id: Option<String>,
    pub audience: Option<String>,
    pub groups_claim: String,
    pub admin_groups: Vec<String>,
    http: reqwest::Client,
    cache: Mutex<KeyCache>,
}

#[derive(Default)]
struct KeyCache {
    jwks_uri: Option<String>,
    keys: HashMap<String, DecodingKey>,
    fetched: Option<Instant>,
}

impl Oidc {
    pub fn new(
        issuer: String,
        client_id: Option<String>,
        audience: Option<String>,
        groups_claim: String,
        admin_groups: Vec<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(format!("{}-server", crate::BRAND_SLUG))
            .build()
            .expect("http client");
        Self { issuer, client_id, audience, groups_claim, admin_groups, http, cache: Mutex::default() }
    }

    async fn refresh(&self, cache: &mut KeyCache) {
        cache.fetched = Some(Instant::now());
        if let Err(e) = self.fetch_keys(cache).await {
            tracing::warn!("jwks fetch failed: {e}");
        }
    }

    async fn fetch_keys(&self, cache: &mut KeyCache) -> Result<(), reqwest::Error> {
        if cache.jwks_uri.is_none() {
            let url = format!("{}/.well-known/openid-configuration", self.issuer.trim_end_matches('/'));
            let doc: Value = self.http.get(url).send().await?.error_for_status()?.json().await?;
            cache.jwks_uri = doc["jwks_uri"].as_str().map(String::from);
        }
        let Some(uri) = cache.jwks_uri.clone() else { return Ok(()) };
        let set: JwkSet = self.http.get(uri).send().await?.error_for_status()?.json().await?;
        cache.keys.clear();
        for jwk in &set.keys {
            if let Ok(key) = DecodingKey::from_jwk(jwk) {
                cache.keys.insert(jwk.common.key_id.clone().unwrap_or_default(), key);
            }
        }
        Ok(())
    }

    async fn key_for(&self, kid: Option<&str>) -> Option<DecodingKey> {
        let mut cache = self.cache.lock().await;
        let lookup = |c: &KeyCache| match kid {
            Some(k) => c.keys.get(k).cloned(),
            None if c.keys.len() == 1 => c.keys.values().next().cloned(),
            None => None,
        };
        if let Some(k) = lookup(&cache) {
            return Some(k);
        }
        if cache.fetched.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            self.refresh(&mut cache).await;
        }
        lookup(&cache)
    }

    pub async fn validate(&self, token: &str) -> Result<Value, ApiError> {
        let header = decode_header(token).map_err(|_| ApiError::unauthorized())?;
        if !matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
                | Algorithm::ES256
                | Algorithm::ES384
                | Algorithm::EdDSA
        ) {
            return Err(ApiError::unauthorized());
        }
        let key = self.key_for(header.kid.as_deref()).await.ok_or_else(ApiError::unauthorized)?;
        let mut v = Validation::new(header.alg);
        v.set_issuer(&[&self.issuer]);
        match self.audience.as_ref().or(self.client_id.as_ref()) {
            Some(a) => v.set_audience(&[a]),
            None => v.validate_aud = false,
        }
        decode::<Value>(token, &key, &v).map(|d| d.claims).map_err(|e| {
            tracing::debug!("jwt rejected: {e}");
            ApiError::unauthorized()
        })
    }
}
