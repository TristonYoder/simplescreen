use std::{
    collections::{HashMap, HashSet},
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
    /// Screens this node watches through control/confidence assignments (it does not play them).
    subs: HashSet<String>,
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
    /// One socket per node; it serves every screen of that node.
    pub fn register_node(&self, node_id: &str, team_id: &str) -> (u64, mpsc::UnboundedReceiver<Out>) {
        let conn = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let old = self
            .devices
            .lock()
            .insert(node_id.to_string(), Device { conn, team_id: team_id.to_string(), tx, subs: HashSet::new() });
        if let Some(old) = old {
            let _ = old.tx.send(Out::Close);
        }
        (conn, rx)
    }

    pub fn unregister_node(&self, node_id: &str, conn: u64) -> bool {
        let mut devices = self.devices.lock();
        if devices.get(node_id).is_some_and(|d| d.conn == conn) {
            devices.remove(node_id);
            true
        } else {
            false
        }
    }

    pub fn is_online(&self, node_id: &str) -> bool {
        self.devices.lock().contains_key(node_id)
    }

    pub fn online_nodes(&self) -> Vec<(String, String)> {
        self.devices.lock().iter().map(|(k, d)| (k.clone(), d.team_id.clone())).collect()
    }

    pub fn send_node(&self, node_id: &str, msg: &Value) -> bool {
        match self.devices.lock().get(node_id) {
            Some(d) => d.tx.send(Out::Msg(msg.to_string())).is_ok(),
            None => false,
        }
    }

    /// Replaces the screens a node watches. Returns the previous set, or `None` when the node is offline.
    pub fn set_subscriptions(&self, node_id: &str, screens: HashSet<String>) -> Option<HashSet<String>> {
        self.devices.lock().get_mut(node_id).map(|d| std::mem::replace(&mut d.subs, screens))
    }

    /// Sends `msg` to every online node watching `screen_id`, except `except` (the node that plays it).
    pub fn send_subscribers(&self, screen_id: &str, except: Option<&str>, msg: &Value) {
        let text = msg.to_string();
        for (id, d) in self.devices.lock().iter() {
            if d.subs.contains(screen_id) && except != Some(id.as_str()) {
                let _ = d.tx.send(Out::Msg(text.clone()));
            }
        }
    }

    pub fn close_node(&self, node_id: &str) {
        if let Some(d) = self.devices.lock().get(node_id) {
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
