//! The passphrase lock as the owner meets it: while the core keys are
//! sealed, every message is a try at the passphrase, and the only button
//! is the way out of a forgotten one.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zeroize::Zeroizing;

use super::Bot;
use crate::telegram::{button, escape, CallbackQuery};

/// Wrong passphrases in a row before a pause.
const UNLOCK_TRIES: u32 = 3;
const UNLOCK_PAUSE: Duration = Duration::from_secs(60);
pub(super) const LOCKED: &str = "🔒 I'm locked: the core keys are encrypted. Send your passphrase to unlock me — I delete \
                      your message as soon as I've read it.";

pub(super) fn locked_kb() -> Value {
    json!([[button("Forgot it?", "l:forgot")]])
}

impl Bot {
    /// While locked, every message but a command is a try at the passphrase.
    pub(super) async fn on_locked(&self, chat: i64, message_id: i64, text: &str) -> Result<(), String> {
        if text.is_empty() || text.starts_with('/') {
            return self.tg.send(chat, LOCKED, Some(locked_kb())).await;
        }
        self.delete_secret(chat, message_id, "the passphrase").await?;
        if let Some(wait) = self.unlock_pause() {
            let text = format!("⏳ Too many wrong tries. Try again in {} s.", wait.as_secs() + 1);
            return self.tg.send(chat, &text, None).await;
        }
        let store = Arc::clone(&self.store);
        let passphrase = Zeroizing::new(text.to_string());
        let unlocked = tokio::task::spawn_blocking(move || store.unlock(&passphrase))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        match unlocked {
            Ok(true) => {
                self.note_unlock(true);
                log::info!("unlocked");
                self.status.update(&self.store);
                self.cores.connect_all();
                let (text, kb) = self.main_screen();
                self.tg.send(chat, &format!("🔓 Unlocked.\n\n{text}"), Some(kb)).await
            }
            Ok(false) => {
                log::warn!("wrong passphrase");
                let text = match self.note_unlock(false) {
                    0 => format!("❌ Wrong passphrase. Try again in {} s.", UNLOCK_PAUSE.as_secs()),
                    left => format!("❌ Wrong passphrase. {left} more {} before a pause.", if left == 1 { "try" } else { "tries" }),
                };
                self.tg.send(chat, &text, Some(locked_kb())).await
            }
            Err(e) => {
                log::error!("unlock: {e}");
                self.tg.send(chat, &format!("❌ Couldn't unlock: {}", escape(&e)), Some(locked_kb())).await
            }
        }
    }

    /// Delete the owner's message holding `what`, or tell them to.
    pub(super) async fn delete_secret(&self, chat: i64, message_id: i64, what: &str) -> Result<(), String> {
        if let Err(e) = self.tg.delete_message(chat, message_id).await {
            log::warn!("{e}");
            let warn = format!("⚠️ I couldn't delete your message with {what} — delete it yourself.");
            self.tg.send(chat, &warn, None).await?;
        }
        Ok(())
    }

    /// Count an unlock try; answers the wrong tries left before a pause.
    fn note_unlock(&self, right: bool) -> u32 {
        let Ok(mut fails) = self.unlock_fails.lock() else {
            return 0;
        };
        if right {
            *fails = (0, None);
            return 0;
        }
        fails.0 += 1;
        if fails.0 >= UNLOCK_TRIES {
            *fails = (0, Some(Instant::now() + UNLOCK_PAUSE));
            return 0;
        }
        UNLOCK_TRIES - fails.0
    }

    fn unlock_pause(&self) -> Option<Duration> {
        let fails = self.unlock_fails.lock().ok()?;
        fails.1.and_then(|until| until.checked_duration_since(Instant::now()))
    }

    /// Buttons while locked: only the way out of a forgotten passphrase.
    pub(super) async fn on_locked_callback(&self, cq: CallbackQuery) -> Result<(), String> {
        let data = cq.data.as_deref().unwrap_or("");
        let Some(msg) = cq.message.filter(|_| data.starts_with("l:")) else {
            self.tg.answer_callback(&cq.id, Some("🔒 Locked — send your passphrase")).await;
            return Ok(());
        };
        self.tg.answer_callback(&cq.id, None).await;
        let (text, kb) = match data {
            "l:forgot" => (
                "Forgot the passphrase? There is no way to recover it. A reset deletes every saved core and its \
                 local trade history; then you add the cores again with their keys. Nothing changes on the cores \
                 themselves."
                    .to_string(),
                json!([[button("🗑 Reset", "l:reset"), button("✖ Cancel", "l:back")]]),
            ),
            "l:reset" => {
                for id in self.store.forget()? {
                    self.cores.remove(&id);
                }
                log::warn!("passphrase forgotten: cores and lock reset");
                self.status.update(&self.store);
                let (text, kb) = self.main_screen();
                let note = "♻️ Reset. The keys you add now are stored unencrypted — to lock them again, run \
                            <code>sudo pnl-bot-passphrase</code> on the server.";
                (format!("{note}\n\n{text}"), kb)
            }
            _ => (LOCKED.to_string(), locked_kb()),
        };
        self.tg.edit(msg.chat.id, msg.message_id, &text, Some(kb)).await
    }
}
