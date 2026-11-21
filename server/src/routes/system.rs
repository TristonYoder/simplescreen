use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{Kind, Membership, Principal, Role},
    error::{ApiError, ApiJson, ApiResult},
    state::AppState,
    BRAND_NAME, BRAND_SLUG,
};

pub async fn info(State(s): State<AppState>) -> Json<Value> {
    let mut v = json!({
        "name": BRAND_NAME,
        "slug": BRAND_SLUG,
        "version": env!("CARGO_PKG_VERSION"),
        "api_version": 1,
        "server_id": s.server_id,
        "oidc_enabled": s.oidc.is_some(),
    });
    if let Some(u) = &s.cfg.public_url {
        v["public_url"] = json!(u);
    }
    Json(v)
}

pub async fn auth_config(State(s): State<AppState>) -> Json<Value> {
    match &s.oidc {
        None => Json(json!({ "oidc_enabled": false })),
        Some(o) => {
            let mut scopes = vec!["openid".to_string(), "profile".into(), "email".into()];
            scopes.push(o.groups_claim.clone());
            Json(json!({
                "oidc_enabled": true,
                "issuer": o.issuer,
                "client_id": o.client_id,
                "scopes": scopes,
                "token_type": s.cfg.oidc_token_type,
            }))
        }
    }
}

pub async fn me(State(s): State<AppState>, p: Principal) -> ApiResult<Json<Value>> {
    let memberships: Vec<Membership> = if p.server_admin {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, name FROM teams ORDER BY name").fetch_all(&s.db).await?;
        rows.into_iter()
            .map(|(team_id, team_name)| Membership { team_id, team_name, role: Role::Admin })
            .collect()
    } else {
        p.memberships.clone()
    };
    let mut v = json!({
        "kind": p.kind,
        "subject": p.subject,
        "server_admin": p.server_admin,
        "memberships": memberships,
    });
    if let Some(n) = &p.display_name {
        v["display_name"] = json!(n);
    }
    if let Some(e) = &p.email {
        v["email"] = json!(e);
    }
    let tz: Option<(Option<String>,)> =
        sqlx::query_as("SELECT timezone FROM users WHERE subject = ?").bind(&p.subject).fetch_optional(&s.db).await?;
    if let Some((Some(z),)) = tz {
        v["timezone"] = json!(z);
    }
    if p.kind == Kind::Device {
        v["node_id"] = json!(p.node_id);
        if let Some(node_id) = &p.node_id {
            let ids = super::nodes::screen_ids(&s, node_id).await?;
            if let [only] = ids.as_slice() {
                v["screen_id"] = json!(only);
            }
        }
    }
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct MePatch {
    #[serde(default, deserialize_with = "crate::models::double_option")]
    timezone: Option<Option<String>>,
}

pub async fn patch_me(State(s): State<AppState>, p: Principal, ApiJson(input): ApiJson<MePatch>) -> ApiResult<Json<Value>> {
    if p.is_device() {
        return Err(ApiError::forbidden());
    }
    if let Some(tz) = &input.timezone {
        if tz.as_deref().is_some_and(|z| crate::schedule::parse_tz(z).is_none()) {
            return Err(ApiError::unprocessable("invalid_timezone", "timezone must be an IANA zone"));
        }
        sqlx::query(
            "INSERT INTO users (subject, display_name, email, last_seen_at, timezone) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(subject) DO UPDATE SET timezone = ?5",
        )
        .bind(&p.subject)
        .bind(&p.display_name)
        .bind(&p.email)
        .bind(crate::util::now())
        .bind(tz)
        .execute(&s.db)
        .await?;
    }
    me(State(s), p).await
}
