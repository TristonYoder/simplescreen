use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::{
    auth::{Kind, Membership, Principal, Role},
    error::ApiResult,
    state::AppState,
    BRAND_NAME, BRAND_SLUG,
};

pub async fn info(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": BRAND_NAME,
        "slug": BRAND_SLUG,
        "version": env!("CARGO_PKG_VERSION"),
        "api_version": 1,
        "server_id": s.server_id,
        "oidc_enabled": s.oidc.is_some(),
    }))
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
            sqlx::query_as("SELECT id, name FROM groups ORDER BY name").fetch_all(&s.db).await?;
        rows.into_iter()
            .map(|(group_id, group_name)| Membership { group_id, group_name, role: Role::Admin })
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
    if p.kind == Kind::Device {
        v["screen_id"] = json!(p.screen_id);
    }
    Ok(Json(v))
}
