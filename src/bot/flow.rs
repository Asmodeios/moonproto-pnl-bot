//! The add-core conversation — name, key, and the endpoint when the key
//! carries none — held in `pending`, the several-cores-at-once message
//! (`parse_batch_line`), and renaming a core. Its messages are deleted
//! together when it ends.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use super::{cancel_kb, Bot, GONE};
use crate::cores;
use crate::store::CoreEntry;
use crate::telegram::escape;

const MAX_NAME_CHARS: usize = 40;

/// No `Debug`: a pending add can hold a key.
pub(super) enum Pending {
    Name,
    Key { name: String },
    Endpoint { name: String, key: String },
    /// A list of cores, one per line.
    Batch,
    /// A new name for the core `id`.
    Rename { id: String },
}

impl Bot {
    pub(super) fn set_pending(&self, p: Option<Pending>) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = p;
        }
    }

    pub(super) fn take_pending(&self) -> Option<Pending> {
        self.pending.lock().ok().and_then(|mut p| p.take())
    }

    pub(super) fn follow(&self, chat: i64, message_id: i64) {
        if let Ok(mut trail) = self.trail.lock() {
            trail.push((chat, message_id));
        }
    }

    /// A step of the add-core conversation, deleted when it ends.
    async fn prompt(&self, chat: i64, text: &str, keyboard: Option<Value>) -> Result<(), String> {
        let id = self.tg.send_id(chat, text, keyboard).await?;
        self.follow(chat, id);
        Ok(())
    }

    /// End the add-core conversation, however it ended, and delete its
    /// messages — all but `keep`, a message the caller turns into a screen.
    pub(super) async fn end_flow(&self, keep: Option<i64>) {
        self.set_pending(None);
        let trail: Vec<(i64, i64)> = self.trail.lock().map(|mut t| std::mem::take(&mut *t)).unwrap_or_default();
        let mut by_chat: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for (chat, id) in trail.into_iter().filter(|(_, id)| Some(*id) != keep) {
            by_chat.entry(chat).or_default().push(id);
        }
        for (chat, ids) in by_chat {
            for batch in ids.chunks(100) {
                if let Err(e) = self.tg.delete_messages(chat, batch).await {
                    log::warn!("{e}");
                }
            }
        }
    }

    pub(super) async fn on_pending(&self, chat: i64, message_id: i64, pending: Pending, text: &str) -> Result<(), String> {
        let cancel = Some(cancel_kb());
        // A message with keys is deleted the moment it is read, not with the rest.
        if !matches!(pending, Pending::Key { .. } | Pending::Batch) {
            self.follow(chat, message_id);
        }
        match pending {
            Pending::Name => {
                let Some(name) = self.take_name(chat, text, Pending::Name).await? else {
                    return Ok(());
                };
                let ask = format!(
                    "Now send the MoonProto key for <b>{}</b> — the key string MoonBot exports.\n\n\
                     I delete your message as soon as I've read it.",
                    escape(&name)
                );
                self.set_pending(Some(Pending::Key { name }));
                self.prompt(chat, &ask, cancel).await
            }
            Pending::Key { name } => {
                let key = text.to_string();
                self.delete_secret(chat, message_id, "the key").await?;
                match cores::key_has_endpoint(&key) {
                    None => {
                        self.set_pending(Some(Pending::Key { name }));
                        let ask = "That isn't a MoonBot key export. Send the key again, or cancel.";
                        self.prompt(chat, ask, cancel).await
                    }
                    Some(true) => self.add_core(chat, name, key, String::new(), 0).await,
                    Some(false) => {
                        self.set_pending(Some(Pending::Endpoint { name, key }));
                        let ask = "This key carries no address. Send the core's address as <code>host:port</code>.";
                        self.prompt(chat, ask, cancel).await
                    }
                }
            }
            Pending::Endpoint { name, key } => {
                match parse_endpoint(text) {
                    Some((host, port)) => self.add_core(chat, name, key, host, port).await,
                    None => {
                        self.set_pending(Some(Pending::Endpoint { name, key }));
                        let ask = "Send it as <code>host:port</code>, e.g. <code>203.0.113.5:4545</code>.";
                        self.prompt(chat, ask, cancel).await
                    }
                }
            }
            Pending::Batch => {
                self.delete_secret(chat, message_id, "the keys").await?;
                self.add_batch(chat, text).await
            }
            Pending::Rename { id } => {
                let Some(name) = self.take_name(chat, text, Pending::Rename { id: id.clone() }).await? else {
                    return Ok(());
                };
                self.end_flow(None).await;
                let note = match self.store.rename(&id, &name) {
                    Ok(Some(old)) => {
                        log::info!("renamed core {id}");
                        format!("✏️ <b>{}</b> renamed to <b>{}</b>.", escape(&old), escape(&name))
                    }
                    Ok(None) => GONE.to_string(),
                    Err(e) => format!("❌ {}", escape(&e)),
                };
                self.show_cores(chat, Some(note)).await
            }
        }
    }

    /// The name sent, cleaned; `None` once asked again with `retry` pending.
    async fn take_name(&self, chat: i64, text: &str, retry: Pending) -> Result<Option<String>, String> {
        let name = cores::clean_name(text);
        if valid_name(&name) {
            return Ok(Some(name));
        }
        self.set_pending(Some(retry));
        let ask = format!("Send a name of 1–{MAX_NAME_CHARS} characters.");
        self.prompt(chat, &ask, Some(cancel_kb())).await?;
        Ok(None)
    }

    async fn add_core(&self, chat: i64, name: String, key: String, host: String, port: u16) -> Result<(), String> {
        self.end_flow(None).await;
        let note = match self.create_core(name, key, host, port) {
            Ok(entry) => {
                format!("✅ <b>{}</b> added — connecting. Its trade history syncs in a minute or so.", escape(&entry.name))
            }
            Err(e) => format!("❌ {}", escape(&e)),
        };
        self.cores_page.store(usize::MAX, Ordering::Relaxed);
        self.show_cores(chat, Some(note)).await
    }

    /// Store a core and start its session; a core whose session can't
    /// start is not kept.
    fn create_core(&self, name: String, key: String, host: String, port: u16) -> Result<CoreEntry, String> {
        let entry = CoreEntry { id: new_id(), name, key, sealed_key: String::new(), host, port, currency: String::new() };
        self.store.add(entry.clone())?;
        if let Err(e) = self.cores.connect(&entry) {
            self.store.remove(&entry.id)?;
            return Err(format!("could not start a connection: {e}"));
        }
        log::info!("added core {}", entry.id);
        Ok(entry)
    }

    /// Every line of `text` that names a core, added; the rest reported by
    /// line number — never by content, which holds keys.
    async fn add_batch(&self, chat: i64, text: &str) -> Result<(), String> {
        self.end_flow(None).await;
        let mut added = Vec::new();
        let mut skipped = Vec::new();
        for (i, line) in text.lines().enumerate() {
            let n = i + 1;
            let core = match parse_batch_line(line) {
                Ok(Some(core)) => core,
                Ok(None) => continue,
                Err(e) => {
                    skipped.push(format!("line {n}: {e}"));
                    continue;
                }
            };
            let name = escape(&core.name);
            match self.create_core(core.name, core.key, core.host, core.port) {
                Ok(_) => added.push(name),
                Err(e) => skipped.push(format!("line {n} ({name}): {}", escape(&e))),
            }
        }
        let mut note = match added.len() {
            0 => "No cores added.".to_string(),
            n => format!("✅ Added {n}: {} — connecting.", added.join(", ")),
        };
        if !skipped.is_empty() {
            note.push_str(&format!("\n\n⚠️ Skipped:\n• {}", skipped.join("\n• ")));
        }
        self.cores_page.store(usize::MAX, Ordering::Relaxed);
        self.show_cores(chat, Some(note)).await
    }
}

fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME_CHARS).contains(&name.chars().count())
}

/// One core of a batch list.
struct BatchLine {
    name: String,
    key: String,
    host: String,
    port: u16,
}

/// `Name KEY`, `Name: KEY`, `KEY` alone (named by the key's label), each
/// optionally followed by `host:port`. `None` for a blank or `#` line.
/// Errors never quote the line: it holds a key.
fn parse_batch_line(line: &str) -> Result<Option<BatchLine>, String> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(None);
    }
    let mut tokens: Vec<&str> = line.split_whitespace().collect();
    let endpoint = match tokens.as_slice() {
        [.., key, last] if cores::key_has_endpoint(key).is_some() => parse_endpoint(last),
        _ => None,
    };
    if endpoint.is_some() {
        tokens.pop();
    }
    let key = tokens.pop().unwrap_or_default().to_string();
    let has_endpoint = cores::key_has_endpoint(&key).ok_or("no MoonBot key at the end of the line")?;
    if !has_endpoint && endpoint.is_none() {
        return Err("this key carries no address — add host:port after it".to_string());
    }
    let name = tokens.join(" ");
    let name = name.trim_end_matches([':', '-', '|', '=', ',', ';', '—', '–', ' ']).trim().to_string();
    let name = if name.is_empty() { cores::key_label(&key).unwrap_or_default() } else { name };
    if !valid_name(&name) {
        return Err(format!("the name must be 1–{MAX_NAME_CHARS} characters"));
    }
    let (host, port) = endpoint.unwrap_or_default();
    Ok(Some(BatchLine { name, key, host, port }))
}

/// `host:port`, the host possibly a bracketed IPv6 address.
fn parse_endpoint(text: &str) -> Option<(String, u16)> {
    let (host, port) = text.trim().rsplit_once(':')?;
    let host = host.trim().trim_matches(['[', ']']);
    let port = port.trim().parse::<u16>().ok().filter(|p| *p != 0)?;
    (!host.is_empty()).then(|| (host.to_string(), port))
}

fn new_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("c{:x}{}", chrono::Utc::now().timestamp_millis(), SEQ.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(parse_endpoint("203.0.113.5:4545"), Some(("203.0.113.5".to_string(), 4545)));
        assert_eq!(parse_endpoint("[2001:db8::1]:4545"), Some(("2001:db8::1".to_string(), 4545)));
        assert_eq!(parse_endpoint("host:0"), None);
        assert_eq!(parse_endpoint(":4545"), None);
        assert_eq!(parse_endpoint("no-port"), None);
    }

    #[test]
    fn batch_lines_without_a_key() {
        assert!(matches!(parse_batch_line("   "), Ok(None)));
        assert!(matches!(parse_batch_line("# Binance cores"), Ok(None)));
        let err = parse_batch_line("Binance futures notakey").err().unwrap_or_default();
        assert_eq!(err, "no MoonBot key at the end of the line");
        // The error names the problem, never the line: it may hold a key.
        assert!(!err.contains("notakey"));
    }
}
