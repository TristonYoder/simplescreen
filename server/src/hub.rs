use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::auth::Principal;

pub enum Out {
    Msg(String),
    Close,
}

type Tx = mpsc::UnboundedSender<Out>;

struct Device {
    conn: u64,
    team_id: String,
    tx: Tx,
}

struct UserConn {
    principal: Principal,
    tx: Tx,
}

#[derive(Default)]
pub struct Hub {
    devices: Mutex<HashMap<String, Device>>,
    users: Mutex<HashMap<u64, UserConn>>,
    next: AtomicU64,
}

impl Hub {
    pub fn register_device(&self, screen_id: &str, team_id: &str) -> (u64, mpsc::UnboundedReceiver<Out>) {
        let conn = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let old = self
            .devices
            .lock()
            .insert(screen_id.to_string(), Device { conn, team_id: team_id.to_string(), tx });
        if let Some(old) = old {
            let _ = old.tx.send(Out::Close);
        }
        (conn, rx)
    }

    pub fn unregister_device(&self, screen_id: &str, conn: u64) -> bool {
        let mut devices = self.devices.lock();
        if devices.get(screen_id).is_some_and(|d| d.conn == conn) {
            devices.remove(screen_id);
            true
        } else {
            false
        }
    }

    pub fn is_online(&self, screen_id: &str) -> bool {
        self.devices.lock().contains_key(screen_id)
    }

    pub fn online_devices(&self) -> Vec<(String, String)> {
        self.devices.lock().iter().map(|(k, d)| (k.clone(), d.team_id.clone())).collect()
    }

    pub fn send_device(&self, screen_id: &str, msg: &Value) -> bool {
        match self.devices.lock().get(screen_id) {
            Some(d) => d.tx.send(Out::Msg(msg.to_string())).is_ok(),
            None => false,
        }
    }

    pub fn close_device(&self, screen_id: &str) {
        if let Some(d) = self.devices.lock().get(screen_id) {
            let _ = d.tx.send(Out::Close);
        }
    }

    pub fn register_user(&self, principal: Principal) -> (u64, mpsc::UnboundedReceiver<Out>) {
        let conn = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.users.lock().insert(conn, UserConn { principal, tx });
        (conn, rx)
    }

    pub fn unregister_user(&self, conn: u64) {
        self.users.lock().remove(&conn);
    }

    pub fn broadcast_team(&self, team_id: &str, msg: &Value) {
        let text = msg.to_string();
        for u in self.users.lock().values() {
            if u.principal.can_view(team_id) {
                let _ = u.tx.send(Out::Msg(text.clone()));
            }
        }
    }
}
