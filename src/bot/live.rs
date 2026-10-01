//! A screen once shown can be kept current, edited in place when it changes:
//! the Cores screen last shown follows its cores while one is still on its
//! way, re-read every few seconds; a text report follows its age, redrawn
//! each time its "updated … ago" line reads differently.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::screens::core_state;
use super::{Bot, Follow, Screen};

/// How often a live Cores screen is re-read, and for how long at most.
const LIVE_EVERY: Duration = Duration::from_secs(3);
const LIVE_FOR: Duration = Duration::from_secs(10 * 60);
/// How long a text report's age is kept current.
const AGE_FOR: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A message being kept current.
pub(super) struct Kept {
    seq: u64,
    cores: bool,
}

impl Bot {
    /// Keep the screen just shown as `shown` current, as `follow` says. Only
    /// the newest Cores screen is followed, and only while a core on it is
    /// connecting, syncing or reconnecting.
    pub(super) fn keep_current(&self, chat: i64, message_id: i64, follow: Follow, shown: String) {
        let cores = match follow {
            Follow::No => return,
            Follow::Cores(_) if !self.settling() => return,
            Follow::Cores(_) => true,
            Follow::Age(_) => false,
        };
        let seq = self.live_seq.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut kept) = self.kept.lock() {
            if cores {
                kept.retain(|_, k| !k.cores);
            }
            kept.insert((chat, message_id), Kept { seq, cores });
        }
        // Weak, so a report kept for days doesn't keep the bot.
        let me = self.me.clone();
        let started = Instant::now();
        tokio::spawn(async move {
            let mut shown = shown;
            loop {
                let wait = match &follow {
                    Follow::Age(aged) => {
                        let elapsed = aged.elapsed();
                        let next = super::report::next_age(elapsed);
                        if next > AGE_FOR {
                            break;
                        }
                        next - elapsed
                    }
                    _ => LIVE_EVERY,
                };
                tokio::time::sleep(wait).await;
                let Some(bot) = me.upgrade() else {
                    return;
                };
                let _screen = bot.screen_lock.lock().await;
                if !bot.is_kept(chat, message_id, seq) {
                    return;
                }
                let screen: Screen = match &follow {
                    Follow::Cores(note) => bot.cores_screen(note.clone()),
                    Follow::Age(aged) => aged.screen(),
                    Follow::No => break,
                };
                if screen.text != shown {
                    if let Err(e) = bot.tg.edit(chat, message_id, &screen.text, Some(screen.kb)).await {
                        log::warn!("{e}");
                        break;
                    }
                    shown = screen.text;
                }
                if cores && (!bot.settling() || started.elapsed() >= LIVE_FOR) {
                    break;
                }
            }
            if let Some(bot) = me.upgrade() {
                if let Ok(mut kept) = bot.kept.lock() {
                    if kept.get(&(chat, message_id)).is_some_and(|k| k.seq == seq) {
                        kept.remove(&(chat, message_id));
                    }
                }
            }
        });
    }

    fn is_kept(&self, chat: i64, message_id: i64, seq: u64) -> bool {
        self.kept.lock().is_ok_and(|k| k.get(&(chat, message_id)).is_some_and(|k| k.seq == seq))
    }

    /// Stop following `message_id` — a button is turning it into another screen.
    pub(super) async fn unwatch(&self, chat: i64, message_id: i64) {
        let _screen = self.screen_lock.lock().await;
        if let Ok(mut kept) = self.kept.lock() {
            kept.remove(&(chat, message_id));
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
