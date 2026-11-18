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
    models::ScreenStatus,
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
    status: Option<ScreenStatus>,
}

async fn device_socket(s: AppState, p: Principal, mut sock: WebSocket) {
    let (Some(screen_id), Some(group_id)) = (p.screen_id.clone(), p.device_group.clone()) else { return };
    let (conn, mut rx) = s.hub.register_device(&screen_id, &group_id);
    touch_last_seen(&s, &screen_id).await;
    s.hub.broadcast_group(&group_id, &json!({ "type": "screen_online", "screen_id": screen_id }));
    let hello = json!({ "type": "hello", "server_id": s.server_id }).to_string();
    let mut alive = sock.send(Message::Text(hello.into())).await.is_ok();

    let mut shutdown = s.shutdown.subscribe();
    let mut tick = interval_at(Instant::now() + Duration::from_secs(20), Duration::from_secs(20));
    let mut last_write = std::time::Instant::now();
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
                    if m.kind == "status" {
                        if let Some(mut status) = m.status {
                            status.updated_at = Some(now());
                            s.statuses.lock().insert(screen_id.clone(), status.clone());
                            s.hub.broadcast_group(&group_id, &json!({
                                "type": "screen_status", "screen_id": screen_id, "status": status,
                            }));
                            if last_write.elapsed() > Duration::from_secs(30) {
                                last_write = std::time::Instant::now();
                                touch_last_seen(&s, &screen_id).await;
                            }
                        }
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

    if s.hub.unregister_device(&screen_id, conn) {
        s.statuses.lock().remove(&screen_id);
        touch_last_seen(&s, &screen_id).await;
        s.hub.broadcast_group(&group_id, &json!({ "type": "screen_offline", "screen_id": screen_id }));
    }
}

async fn touch_last_seen(s: &AppState, screen_id: &str) {
    let _ = sqlx::query("UPDATE screens SET last_seen_at = ? WHERE id = ?")
        .bind(now())
        .bind(screen_id)
        .execute(&s.db)
        .await;
}

async fn user_socket(s: AppState, p: Principal, mut sock: WebSocket) {
    let (conn, mut rx) = s.hub.register_user(p.clone());
    let hello = json!({ "type": "hello", "server_id": s.server_id }).to_string();
    let mut alive = sock.send(Message::Text(hello.into())).await.is_ok();
    for (screen_id, group_id) in s.hub.online_devices() {
        if !alive || !p.can_view(&group_id) {
            continue;
        }
        let mut msgs = vec![json!({ "type": "screen_online", "screen_id": screen_id })];
        if let Some(status) = s.statuses.lock().get(&screen_id) {
            msgs.push(json!({ "type": "screen_status", "screen_id": screen_id, "status": status }));
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
