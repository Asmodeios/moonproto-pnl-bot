//! The Cores screen last shown follows its cores while one is still on its
//! way: re-read every few seconds and edited in place when it changes.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::screens::core_state;
use super::Bot;

/// How often a live Cores screen is re-read, and for how long at most.
const LIVE_EVERY: Duration = Duration::from_secs(3);
const LIVE_FOR: Duration = Duration::from_secs(10 * 60);

/// The Cores screen being kept current.
pub(super) struct Live {
    chat: i64,
    message_id: i64,
    seq: u64,
}

impl Bot {
    /// Keep the Cores screen just shown as `shown` current while any core
    /// on it is connecting, syncing or reconnecting. Only the newest one is
    /// followed.
    pub(super) fn watch(&self, chat: i64, message_id: i64, note: Option<String>, shown: String) {
        if !self.settling() {
            return;
        }
        let Some(bot) = self.me.upgrade() else {
            return;
        };
        let seq = self.live_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut live) = self.live.lock() {
            *live = Some(Live { chat, message_id, seq });
        }
        tokio::spawn(async move {
            let started = Instant::now();
            let mut shown = shown;
            loop {
                tokio::time::sleep(LIVE_EVERY).await;
                let _screen = bot.screen_lock.lock().await;
                if !bot.is_live(seq) {
                    return;
                }
                let screen = bot.cores_screen(note.clone());
                if screen.text != shown {
                    if let Err(e) = bot.tg.edit(chat, message_id, &screen.text, Some(screen.kb)).await {
                        log::warn!("{e}");
                        break;
                    }
                    shown = screen.text;
                }
                if !bot.settling() || started.elapsed() >= LIVE_FOR {
                    break;
                }
            }
            if let Ok(mut live) = bot.live.lock() {
                if live.as_ref().is_some_and(|l| l.seq == seq) {
                    *live = None;
                }
            }
        });
    }

    fn is_live(&self, seq: u64) -> bool {
        self.live.lock().is_ok_and(|l| l.as_ref().is_some_and(|l| l.seq == seq))
    }

    /// Stop following `message_id` — a button is turning it into another screen.
    pub(super) async fn unwatch(&self, chat: i64, message_id: i64) {
        let _screen = self.screen_lock.lock().await;
        if let Ok(mut live) = self.live.lock() {
            if live.as_ref().is_some_and(|l| l.chat == chat && l.message_id == message_id) {
                *live = None;
            }
        }
    }

    /// Whether a core is still on its way: connecting, syncing or reconnecting.
    pub(super) fn settling(&self) -> bool {
        self.store
            .all()
            .iter()
            .any(|c| core_state(self.cores.view(&c.id).as_ref()).settling)
    }
}
