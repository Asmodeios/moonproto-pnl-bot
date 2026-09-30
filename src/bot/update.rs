//! The Update button: a newer release shows on the main menu, and pressing
//! it asks the server to install it (crate::update).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{menu_kb, Bot};
use crate::telegram::{button, escape};
use crate::update::{self, CURRENT, REPO_URL};

/// How often GitHub is asked for a newer release.
const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);
/// How often a requested update is looked in on.
const UPDATE_POLL: Duration = Duration::from_secs(2);
/// pnl-bot-update.path takes a request at once; one still there after this
/// has nothing on the server to take it.
const TAKEN_WITHIN: Duration = Duration::from_secs(15);
/// pnl-bot-update.service restarts the bot or reports a failure within
/// 5 minutes; this is only for when neither happens.
const UPDATE_WITHIN: Duration = Duration::from_secs(6 * 60);

impl Bot {
    /// Look for a newer release now, then every `CHECK_EVERY`.
    pub fn check_updates(&self) {
        let me = self.me.clone();
        tokio::spawn(async move {
            loop {
                let found = update::latest().await;
                let Some(bot) = me.upgrade() else {
                    return;
                };
                match found {
                    Ok(version) => bot.set_latest(update::newer(&version).then_some(version)),
                    Err(e) => log::warn!("{e}"),
                }
                drop(bot);
                tokio::time::sleep(CHECK_EVERY).await;
            }
        });
    }

    fn set_latest(&self, newer: Option<String>) {
        let Ok(mut latest) = self.latest.lock() else {
            return;
        };
        if newer.is_some() && *latest != newer {
            log::info!("pnl-bot {} is out (this is {CURRENT})", newer.as_deref().unwrap_or_default());
        }
        *latest = newer;
    }

    /// A release newer than this build, once the check has found one.
    pub(super) fn new_version(&self) -> Option<String> {
        self.latest.lock().ok()?.clone()
    }

    /// Tell the owner, on the first start of a new version, or after an
    /// update that failed once it had restarted the bot.
    pub(super) async fn announce_update(&self, owner: i64) {
        let updated = update::updated_from(&self.data_dir).map(|from| format!("✅ Updated from v{from} to v{CURRENT}."));
        let failed = update::failure(&self.data_dir).map(|log| failed_text(&log));
        for text in updated.into_iter().chain(failed) {
            if let Err(e) = self.tg.send(owner, &text, None).await {
                log::warn!("{e}");
            }
        }
    }

    /// Asks before updating, or says how to when the server can't take the
    /// bot's request.
    pub(super) fn update_screen(&self) -> (String, Value) {
        let Some(v) = self.new_version() else {
            return self.main_screen();
        };
        let notes = format!("<a href=\"{REPO_URL}/releases/tag/v{v}\">What's new</a>");
        if !update::can_request() {
            let text = format!(
                "<b>⬆️ Version v{v} is out</b> (this is v{CURRENT}) · {notes}\n\n\
                 To install it, run on the server:\n<code>pnl --update</code>\n\n\
                 After that, updates also install from this button."
            );
            return (text, menu_kb());
        }
        let text = format!(
            "<b>⬆️ Update to v{v}?</b> (this is v{CURRENT}) · {notes}\n\n\
             The server downloads the release from GitHub, checks it and restarts me — it takes about a \
             minute.{}",
            self.passphrase_hint()
        );
        (text, json!([[button("✅ Update", "u:go"), button("✖ Cancel", "m")]]))
    }

    /// Ask the server to update, and tell the owner if it doesn't.
    pub(super) fn start_update(&self, chat: i64) -> Result<(String, Value), String> {
        let Some(v) = self.new_version().filter(|_| update::can_request()) else {
            return Ok(self.main_screen());
        };
        if self.updating.swap(true, Ordering::Relaxed) {
            return Ok(("⏳ Already updating…".to_string(), json!([])));
        }
        if let Err(e) = update::request(&self.data_dir) {
            self.updating.store(false, Ordering::Relaxed);
            return Err(e);
        }
        log::info!("update to {v} requested");
        let text = format!("⏳ Updating to v{v}. I'll be back in a minute.{}", self.passphrase_hint());
        // A successful update restarts the bot, which ends this.
        let me = self.me.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let (bot, text) = loop {
                tokio::time::sleep(UPDATE_POLL).await;
                let Some(bot) = me.upgrade() else {
                    return;
                };
                if let Some(log) = update::failure(&bot.data_dir) {
                    break (bot, failed_text(&log));
                }
                if started.elapsed() >= TAKEN_WITHIN && update::withdraw(&bot.data_dir) {
                    let text = "⚠️ The update didn't start: nothing on the server took the request. Run \
                                <code>pnl --update</code> there.";
                    break (bot, text.to_string());
                }
                if started.elapsed() >= UPDATE_WITHIN {
                    break (bot, failed_text(""));
                }
            };
            log::warn!("update to {v} did not go through");
            bot.updating.store(false, Ordering::Relaxed);
            if let Err(e) = bot.tg.send(chat, &text, Some(menu_kb())).await {
                log::warn!("{e}");
            }
        });
        Ok((text, json!([])))
    }

    fn passphrase_hint(&self) -> &'static str {
        if self.store.has_lock() {
            "\n\nAfter the restart, send me your passphrase again."
        } else {
            ""
        }
    }
}

/// A failed update, with the end of its log when there is one.
fn failed_text(log: &str) -> String {
    let mut text = String::from("⚠️ The update failed.");
    if !log.is_empty() {
        // Its last 1000 characters, well inside a message.
        let start = log.char_indices().rev().nth(999).map_or(0, |(i, _)| i);
        text.push_str(&format!("\n<pre>{}</pre>", escape(&log[start..])));
    }
    text.push_str("\nThe full log is on the server: <code>journalctl -u pnl-bot-update</code>");
    text
}
