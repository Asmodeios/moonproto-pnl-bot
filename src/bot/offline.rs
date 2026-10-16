//! Notify offline core: a message to the owner when a core that was online
//! has stayed offline for `OFFLINE_AFTER`, and one more when it is back.
//!
//! The crate calls a link lost after a few seconds of silence and reconnects
//! on its own, so a core counts as offline only once it has stayed that way:
//! a short drop or a quick restart of the core is not worth a message. A core
//! not yet online since the bot started is left to the Cores screen.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::feed::duration;
use super::Bot;
use crate::telegram::escape;

/// How often every core's link is looked at.
const CHECK_EVERY: Duration = Duration::from_secs(10);
/// How many minutes a core is offline before the owner hears of it.
pub(super) const OFFLINE_AFTER_MIN: u64 = 1;
const OFFLINE_AFTER: Duration = Duration::from_secs(OFFLINE_AFTER_MIN * 60);

/// A core's link as the checks have found it.
#[derive(Default)]
enum Watch {
    /// Not online yet since the bot started.
    #[default]
    Unseen,
    Online,
    /// Offline since the first check that found it so; `told` once the owner
    /// was told.
    Down { since: Instant, told: bool },
}

/// What the owner hears of a core.
#[derive(Debug, PartialEq)]
enum Change {
    /// Offline this long — `OFFLINE_AFTER` or more.
    Offline(Duration),
    /// Online again, after this long offline.
    Back(Duration),
}

impl Bot {
    /// Look at every core's link every `CHECK_EVERY`, and tell the owner of
    /// the changes while Notify offline core is on.
    pub fn watch_cores(&self) {
        let me = self.me.clone();
        tokio::spawn(async move {
            let mut watches = HashMap::new();
            loop {
                tokio::time::sleep(CHECK_EVERY).await;
                let Some(bot) = me.upgrade() else {
                    return;
                };
                let texts = bot.link_changes(&mut watches);
                if !texts.is_empty() {
                    bot.tell_owner(texts, "\n", "offline core alert").await;
                }
            }
        });
    }

    /// Every core's link as found now: the changes to tell the owner of.
    fn link_changes(&self, watches: &mut HashMap<String, Watch>) -> Vec<String> {
        let notify = self.store.settings().notify_offline;
        let cores = self.store.all();
        watches.retain(|id, _| cores.iter().any(|c| c.id == *id));
        let now = Instant::now();
        let mut texts = Vec::new();
        for core in cores {
            let online = self.cores.online(&core.id);
            let watch = watches.entry(core.id).or_default();
            if let Some(change) = watch.check(online, now, notify) {
                texts.push(change_text(&change, &core.name));
            }
        }
        texts
    }
}

impl Watch {
    /// The core found online or not at `now`: what to tell the owner, if
    /// `notify`. Kept up to date while it is off, so turning it on tells of
    /// a core that is offline already.
    fn check(&mut self, online: bool, now: Instant, notify: bool) -> Option<Change> {
        if online {
            let back = match *self {
                Watch::Down { since, told: true } if notify => Some(Change::Back(now.duration_since(since))),
                _ => None,
            };
            *self = Watch::Online;
            return back;
        }
        if matches!(self, Watch::Online) {
            *self = Watch::Down { since: now, told: false };
        }
        let Watch::Down { since, told } = self else {
            return None;
        };
        let down = now.duration_since(*since);
        if notify && !*told && down >= OFFLINE_AFTER {
            *told = true;
            return Some(Change::Offline(down));
        }
        None
    }
}

fn change_text(change: &Change, name: &str) -> String {
    let name = escape(name);
    match change {
        Change::Offline(down) => format!("🔴 <b>{name}</b> is offline — no link for {}", duration(down.as_millis() as i64)),
        Change::Back(down) => format!("🟢 <b>{name}</b> is back online — offline for {}", duration(down.as_millis() as i64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_for_a_while_then_back() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = Watch::default();
        // Not online yet: a core that doesn't connect is left to the Cores screen.
        assert_eq!(w.check(false, at(0), true), None);
        assert_eq!(w.check(false, at(600), true), None);
        assert_eq!(w.check(true, at(610), true), None);
        // A short drop is no news.
        assert_eq!(w.check(false, at(620), true), None);
        assert_eq!(w.check(false, at(670), true), None);
        assert_eq!(w.check(true, at(680), true), None);
        // A long one is, once, and then its end.
        assert_eq!(w.check(false, at(710), true), None);
        assert_eq!(w.check(false, at(760), true), None);
        assert_eq!(w.check(false, at(770), true), Some(Change::Offline(Duration::from_secs(60))));
        assert_eq!(w.check(false, at(780), true), None);
        assert_eq!(w.check(true, at(1000), true), Some(Change::Back(Duration::from_secs(290))));
        assert_eq!(w.check(true, at(1010), true), None);
    }

    #[test]
    fn the_setting_holds_back_messages_only() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = Watch::default();
        assert_eq!(w.check(true, at(0), false), None);
        assert_eq!(w.check(false, at(10), false), None);
        assert_eq!(w.check(false, at(500), false), None);
        // Turned on while the core is offline: it has been all along.
        assert_eq!(w.check(false, at(510), true), Some(Change::Offline(Duration::from_secs(500))));
        // Turned off before it is back: no "back" either, nor a stale one later.
        assert_eq!(w.check(true, at(600), false), None);
        assert_eq!(w.check(false, at(610), true), None);
        assert_eq!(w.check(true, at(620), true), None);
    }

    #[test]
    fn alert_texts() {
        let offline = change_text(&Change::Offline(Duration::from_secs(125)), "Bybit <1>");
        assert_eq!(offline, "🔴 <b>Bybit &lt;1&gt;</b> is offline — no link for 2m 05s");
        let back = change_text(&Change::Back(Duration::from_secs(3700)), "Bybit <1>");
        assert_eq!(back, "🟢 <b>Bybit &lt;1&gt;</b> is back online — offline for 1h 01m");
    }
}
