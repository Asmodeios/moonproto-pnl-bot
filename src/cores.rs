//! One MoonProto session per configured core, each with its report replica.
//!
//! The crate runs its own threads: `connect_with_sink` returns at once and
//! events arrive on the crate's delivery thread, which only updates the
//! session's link state and hands report events to the replica's writer.
//! A failed connect is not retried by the crate, so `supervise` does it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use moonproto::{
    parse_key_info, ClientConfig, ConnectConfig, Event, InitConfig, InitialStrategies, LifecycleEvent, MoonClient,
    MoonClientEvent, MoonEventSink, RefreshConfig, TransportMode,
};

use crate::replica::{self, Replica, SyncStatus};
use crate::store::{CoreEntry, Store};

const SUPERVISE_EVERY: Duration = Duration::from_secs(60);

static GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, PartialEq)]
pub enum Link {
    /// Connecting or running init.
    Connecting,
    Ready,
    Reconnecting,
    Failed(String),
}

pub struct CoreView {
    pub link: Link,
    pub report: SyncStatus,
    pub exchange: Option<String>,
}

struct Session {
    generation: u64,
    client: MoonClient,
    replica: Replica,
    link: Link,
    /// Init runs once per session: a re-handshake after it is ready at once.
    was_ready: bool,
    exchange: Option<String>,
}

pub struct Cores {
    me: Weak<Cores>,
    reports_dir: PathBuf,
    store: Arc<Store>,
    sessions: Mutex<HashMap<String, Session>>,
    /// The last connect failure logged per core — a core that stays down is
    /// retried every minute, and logged only when the reason changes.
    logged_failure: Mutex<HashMap<String, String>>,
}

impl Cores {
    pub fn new(reports_dir: PathBuf, store: Arc<Store>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            reports_dir,
            store,
            sessions: Mutex::new(HashMap::new()),
            logged_failure: Mutex::new(HashMap::new()),
        })
    }

    pub fn replica_path(&self, id: &str) -> PathBuf {
        self.reports_dir.join(format!("{id}.sqlite3"))
    }

    /// Start a session for `entry`, replacing any earlier one under its id.
    pub fn connect(&self, entry: &CoreEntry) -> Result<(), String> {
        let cfg = client_config(entry)?;
        let init = InitConfig {
            // An explicit empty list is the crate's "client has none": the
            // bot never touches strategies, the core's list stays its own.
            initial_strategies: Some(InitialStrategies::new(0, Vec::new())),
            subscribe_trades: None,
            ..Default::default()
        };
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        let sink = MoonEventSink::callback({
            let me = self.me.clone();
            let id = entry.id.clone();
            move |event| {
                if let Some(cores) = me.upgrade() {
                    cores.on_event(&id, generation, event);
                }
            }
        });
        let mut sessions = self.sessions.lock().map_err(|_| "session state is poisoned".to_string())?;
        // Held across the start, so the first events find the session.
        let previous = sessions.remove(&entry.id);
        let started = MoonClient::connect_with_sink(cfg, ConnectConfig::new(init), sink);
        let result = match started {
            Ok(client) => {
                let replica = Replica::open(&entry.id, self.replica_path(&entry.id), client.reports());
                sessions.insert(
                    entry.id.clone(),
                    Session {
                        generation,
                        client,
                        replica,
                        link: Link::Connecting,
                        was_ready: false,
                        exchange: None,
                    },
                );
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        };
        drop(sessions);
        stop(previous);
        result
    }

    /// Start a session for every stored core — at startup, or once unlocked.
    pub fn connect_all(&self) {
        for entry in self.store.all() {
            if let Err(e) = self.connect(&entry) {
                log::error!("[core {}] {e}", entry.id);
            }
        }
    }

    /// End the core's session and delete its replica.
    pub fn remove(&self, id: &str) {
        let session = self.sessions.lock().ok().and_then(|mut s| s.remove(id));
        stop(session);
        replica::remove_files(&self.replica_path(id));
        if let Ok(mut logged) = self.logged_failure.lock() {
            logged.remove(id);
        }
    }

    pub fn view(&self, id: &str) -> Option<CoreView> {
        let sessions = self.sessions.lock().ok()?;
        let s = sessions.get(id)?;
        Some(CoreView {
            link: s.link.clone(),
            report: s.replica.status(),
            exchange: s.exchange.clone(),
        })
    }

    pub fn shutdown(&self) {
        let sessions: Vec<Session> = match self.sessions.lock() {
            Ok(mut s) => s.drain().map(|(_, v)| v).collect(),
            Err(_) => return,
        };
        for s in sessions {
            let _ = s.client.disconnect();
            let _ = s.client.wait_finished();
        }
    }

    /// Every minute: re-register each replica's open rows, and connect again
    /// the cores whose connect failed (a core that was down, or not yet up).
    pub fn supervise(self: &Arc<Self>) {
        let cores = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SUPERVISE_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let mut failed = Vec::new();
                let mut open_rows = Vec::new();
                if let Ok(sessions) = cores.sessions.lock() {
                    for (id, s) in sessions.iter() {
                        open_rows.push(s.replica.open_rows());
                        if matches!(s.link, Link::Failed(_)) {
                            failed.push(id.clone());
                        }
                    }
                }
                // Outside the lock: every core's events wait on it.
                for rows in open_rows {
                    rows.send();
                }
                for id in failed {
                    if let Some(entry) = cores.store.get(&id) {
                        if let Err(e) = cores.connect(&entry) {
                            cores.note_failure(&id, &e);
                        }
                    }
                }
            }
        });
    }

    fn note_failure(&self, id: &str, error: &str) {
        let new = self
            .logged_failure
            .lock()
            .map(|mut m| m.insert(id.to_string(), error.to_string()).as_deref() != Some(error))
            .unwrap_or(true);
        if new {
            log::warn!("[core {id}] connect failed: {error} — retrying every minute");
        }
    }

    fn on_event(&self, id: &str, generation: u64, event: MoonClientEvent) {
        let mut ready_currency = None;
        {
            let Ok(mut sessions) = self.sessions.lock() else {
                return;
            };
            let Some(s) = sessions.get_mut(id).filter(|s| s.generation == generation) else {
                return;
            };
            match event {
                MoonClientEvent::Domain(Event::Report(event)) => s.replica.send(event),
                MoonClientEvent::Lifecycle(event) => match event {
                    LifecycleEvent::Connecting => s.link = Link::Connecting,
                    LifecycleEvent::Connected { .. } => {
                        s.link = if s.was_ready { Link::Ready } else { Link::Connecting };
                    }
                    LifecycleEvent::Ready => {
                        s.link = Link::Ready;
                        s.was_ready = true;
                        let info = s.client.server_info();
                        s.exchange = info.as_ref().and_then(|i| i.exchange_name.clone()).filter(|n| !n.is_empty());
                        ready_currency = info.and_then(|i| i.base_currency_name).filter(|c| !c.is_empty());
                        log::info!("[core {id}] ready ({})", s.exchange.as_deref().unwrap_or("unknown exchange"));
                    }
                    LifecycleEvent::ConnectFailed { error } => {
                        let error = error.to_string();
                        s.link = Link::Failed(error.clone());
                        drop(sessions);
                        self.note_failure(id, &error);
                        return;
                    }
                    LifecycleEvent::Reconnecting => {
                        s.link = Link::Reconnecting;
                        log::warn!("[core {id}] link lost, reconnecting");
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if let Some(currency) = ready_currency {
            if let Ok(mut logged) = self.logged_failure.lock() {
                logged.remove(id);
            }
            if let Err(e) = self.store.set_currency(id, &currency) {
                log::error!("[core {id}] {e}");
            }
        }
    }
}

/// `disconnect`, then join the runtime on a throwaway thread — never on the
/// sink thread, which a session must not join itself.
fn stop(session: Option<Session>) {
    let Some(session) = session else {
        return;
    };
    let _ = session.client.disconnect();
    std::thread::spawn(move || {
        let _ = session.client.wait_finished();
    });
}

/// The endpoint the entry overrides, else the one the key export carries.
fn client_config(entry: &CoreEntry) -> Result<ClientConfig, String> {
    let info = parse_key_info(entry.key.trim()).ok_or("the stored key is not a MoonBot key export")?;
    let suggested = info.network.as_ref();
    let host = host(entry).ok_or("this key carries no address")?;
    let port = match entry.port {
        0 => suggested.map(|n| n.port).filter(|p| *p != 0).ok_or("this key carries no port")?,
        port => port,
    };
    let mode = suggested.map(|n| n.transport_mode).unwrap_or(TransportMode::V0);
    Ok(ClientConfig::new(host, port, info.keys.master_key, info.keys.mac_key)
        .with_transport_mode(mode)
        // Reports only: no market price refresh, no tag checks.
        .with_refresh(RefreshConfig { update_markets_every: None, check_tags_every: None }))
}

/// The host the entry overrides, else the one the key export carries.
pub fn host(entry: &CoreEntry) -> Option<String> {
    match entry.host.trim() {
        "" => parse_key_info(entry.key.trim())?.network?.address.map(|ip| ip.to_string()),
        host => Some(host.to_string()),
    }
}

/// MoonBot's own label for a key — a name for a core listed without one.
pub fn key_label(key: &str) -> Option<String> {
    parse_key_info(key.trim()).map(|info| clean_name(&info.display_name))
}

/// A name with its runs of whitespace made single spaces.
pub fn clean_name(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a key names its own endpoint — asked before a core is added.
pub fn key_has_endpoint(key: &str) -> Option<bool> {
    let info = parse_key_info(key.trim())?;
    Some(info.network.as_ref().is_some_and(|n| n.address.is_some() && n.port != 0))
}
