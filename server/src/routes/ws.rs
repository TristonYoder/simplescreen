use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::Response,
};
use serde::Deserialize;
use serde_json::json;
use tokio::time::{interval_at, Instant};

use crate::{
    auth::Principal,
    hub::Out,
    models::{NodeStatus, OutputStatus, ScreenStatus},
    state::AppState,
    util::now,
};

pub async fn upgrade(State(s): State<AppState>, p: Principal, up: WebSocketUpgrade) -> Response {
    up.on_upgrade(move |sock| async move {
        if p.is_device() {
            device_socket(s, p, sock).await
        } else {
            user_socket(s, p, sock).await
        }
    })
}

#[derive(Deserialize)]
struct ClientMessage {
    #[serde(rename = "type")]
    kind: String,
    screen_id: Option<String>,
    status: Option<ScreenStatus>,
    outputs: Option<Vec<OutputStatus>>,
    /// The assignments version the device is running; Studio shows it as Applied.
    assignments_version: Option<i64>,
}

async fn node_screens(s: &AppState, node_id: &str) -> crate::error::ApiResult<Vec<String>> {
    super::nodes::screen_ids(s, node_id).await
}

async fn device_socket(s: AppState, p: Principal, mut sock: WebSocket) {
    let Some(node_id) = p.node_id.clone() else { return };
    let (conn, mut rx) = s.hub.register_node(&node_id);
    touch_last_seen(&s, &node_id).await;
    let mut screens = node_screens(&s, &node_id).await.unwrap_or_default();
    crate::assignments::refresh_subscriptions(&s, &node_id).await;
    s.hub.broadcast_all(&json!({ "type": "node_online", "node_id": node_id }));
    for id in &screens {
        s.hub.send_subscribers(id, Some(&node_id), &json!({ "type": "screen_online", "screen_id": id }));
    }
    for id in &screens {
        s.broadcast_screen(id, &json!({ "type": "screen_online", "screen_id": id })).await;
    }
    let hello = json!({ "type": "hello", "server_id": s.server_id }).to_string();
    let mut alive = sock.send(Message::Text(hello.into())).await.is_ok();

    let mut shutdown = s.shutdown.subscribe();
    let mut tick = interval_at(Instant::now() + Duration::from_secs(20), Duration::from_secs(20));
    let mut last_write = std::time::Instant::now();
    let mut reported_info = (None, None, None);
    let mut applied_version = -1;
    while alive {
        tokio::select! {
            out = rx.recv() => match out {
                Some(Out::Msg(text)) => alive = sock.send(Message::Text(text.into())).await.is_ok(),
                Some(Out::Close) | None => {
                    let _ = sock.send(Message::Close(None)).await;
                    alive = false;
                }
            },
            msg = sock.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let Ok(m) = serde_json::from_str::<ClientMessage>(&text) else { continue };
                    if let Some(v) = m.assignments_version {
                        if v != applied_version {
                            applied_version = v;
                            record_applied_version(&s, &node_id, v).await;
                        }
                    }
                    match m.kind.as_str() {
                        "status" => {
                            let Some(mut status) = m.status else { continue };
                            // Ownership is the screens table, not the list from connect. A query failure keeps that list.
                            let current = match node_screens(&s, &node_id).await {
                                Ok(ids) => ids,
                                Err(_) => continue,
                            };
                            for id in screens.iter().filter(|id| !current.contains(id)) {
                                s.statuses.lock().remove(id);
                            }
                            screens = current;
                            let screen_id = match &m.screen_id {
                                Some(id) if screens.contains(id) => id.clone(),
                                Some(_) => continue,
                                None if screens.len() == 1 => screens[0].clone(),
                                None => continue,
                            };
                            status.updated_at = Some(now());
                            let info = (status.model.clone(), status.os_version.clone(), status.app_version.clone());
                            s.statuses.lock().insert(screen_id.clone(), status.clone());
                            let msg = json!({ "type": "screen_status", "screen_id": screen_id, "status": status });
                            s.broadcast_screen(&screen_id, &msg).await;
                            s.hub.send_subscribers(&screen_id, Some(&node_id), &msg);
                            if info != (None, None, None) && info != reported_info {
                                reported_info = info.clone();
                                record_device_info(&s, &node_id, &info).await;
                            }
                            if last_write.elapsed() > Duration::from_secs(30) {
                                last_write = std::time::Instant::now();
                                touch_last_seen(&s, &node_id).await;
                            }
                        }
                        "node_status" => {
                            let Some(outputs) = m.outputs else { continue };
                            let status = NodeStatus { outputs, updated_at: now() };
                            s.node_statuses.lock().insert(node_id.clone(), status.clone());
                            s.hub.broadcast_all(&json!({
                                "type": "node_status", "node_id": node_id,
                                "outputs": status.outputs, "updated_at": status.updated_at,
                            }));
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => alive = false,
                Some(Ok(_)) => {}
            },
            _ = tick.tick() => alive = sock.send(Message::Ping(Default::default())).await.is_ok(),
            _ = shutdown.changed() => {
                let _ = sock.send(Message::Close(None)).await;
                alive = false;
            }
        }
    }

    if s.hub.unregister_node(&node_id, conn) {
        s.node_statuses.lock().remove(&node_id);
        touch_last_seen(&s, &node_id).await;
        // Union the cache with a fresh read. A deleted screen is already gone from the table, and a failed read must not skip offline.
        let mut ids = screens;
        if let Ok(current) = node_screens(&s, &node_id).await {
            for id in current {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        for id in ids {
            s.statuses.lock().remove(&id);
            s.broadcast_screen(&id, &json!({ "type": "screen_offline", "screen_id": id })).await;
            s.hub.send_subscribers(&id, Some(&node_id), &json!({ "type": "screen_offline", "screen_id": id }));
        }
        s.hub.broadcast_all(&json!({ "type": "node_offline", "node_id": node_id }));
    }
}

async fn record_applied_version(s: &AppState, node_id: &str, version: i64) {
    let res = sqlx::query("UPDATE nodes SET applied_assignments_version = ? WHERE id = ? AND applied_assignments_version != ?")
        .bind(version)
        .bind(node_id)
        .bind(version)
        .execute(&s.db)
        .await;
    if res.is_ok_and(|r| r.rows_affected() > 0) {
        s.hub.broadcast_all(&json!({ "type": "node_updated", "node_id": node_id }));
    }
}

async fn record_device_info(s: &AppState, node_id: &str, info: &(Option<String>, Option<String>, Option<String>)) {
    let _ = sqlx::query(
        "UPDATE nodes SET model = COALESCE(?, model), os_version = COALESCE(?, os_version), app_version = COALESCE(?, app_version) WHERE id = ?",
    )
    .bind(&info.0)
    .bind(&info.1)
    .bind(&info.2)
    .bind(node_id)
    .execute(&s.db)
    .await;
}

async fn touch_last_seen(s: &AppState, node_id: &str) {
    let stamp = now();
    let _ = sqlx::query("UPDATE nodes SET last_seen_at = ? WHERE id = ?").bind(&stamp).bind(node_id).execute(&s.db).await;
    let _ = sqlx::query("UPDATE screens SET last_seen_at = ? WHERE node_id = ?").bind(&stamp).bind(node_id).execute(&s.db).await;
}

async fn user_socket(s: AppState, p: Principal, mut sock: WebSocket) {
    let (conn, mut rx) = s.hub.register_user(p.clone());
    let hello = json!({ "type": "hello", "server_id": s.server_id }).to_string();
    let mut alive = sock.send(Message::Text(hello.into())).await.is_ok();
    let w = crate::reach::World::load(&s).await.unwrap_or_default();
    for node_id in s.hub.online_nodes() {
        if !alive {
            continue;
        }
        let mut msgs = vec![json!({ "type": "node_online", "node_id": node_id })];
        let node_status = s.node_statuses.lock().get(&node_id).cloned();
        if let Some(st) = node_status {
            msgs.push(json!({ "type": "node_status", "node_id": node_id, "outputs": st.outputs, "updated_at": st.updated_at }));
        }
        for screen_id in node_screens(&s, &node_id).await.unwrap_or_default() {
            if !p.sees_screen(&w, &screen_id) {
                continue;
            }
            msgs.push(json!({ "type": "screen_online", "screen_id": screen_id }));
            let status = s.statuses.lock().get(&screen_id).cloned();
            if let Some(status) = status {
                msgs.push(json!({ "type": "screen_status", "screen_id": screen_id, "status": status }));
            }
        }
        for m in msgs {
            alive = alive && sock.send(Message::Text(m.to_string().into())).await.is_ok();
        }
    }

    let mut shutdown = s.shutdown.subscribe();
    let mut tick = interval_at(Instant::now() + Duration::from_secs(20), Duration::from_secs(20));
    while alive {
        tokio::select! {
            out = rx.recv() => match out {
                Some(Out::Msg(text)) => alive = sock.send(Message::Text(text.into())).await.is_ok(),
                Some(Out::Close) | None => alive = false,
            },
            msg = sock.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => alive = false,
                Some(Ok(_)) => {}
            },
            _ = tick.tick() => alive = sock.send(Message::Ping(Default::default())).await.is_ok(),
            _ = shutdown.changed() => {
                let _ = sock.send(Message::Close(None)).await;
                alive = false;
            }
        }
    }
    s.hub.unregister_user(conn);
}
