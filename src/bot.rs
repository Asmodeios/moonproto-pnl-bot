//! Updates in, screens out. Every screen is one message edited in place by
//! its inline buttons; adding a core is a short conversation (name, key,
//! and the endpoint when the key carries none) held in `pending`, or one
//! message listing several cores at once (`parse_batch_line`). The Cores
//! screen last shown follows its cores while one is still on its way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::cores::{self, CoreView, Cores, Link};
use crate::pnl::{self, CoreTally, Period, Tally};
use crate::replica::Phase;
use crate::status::Status;
use crate::store::{CoreEntry, ReportFormat, Store};
use crate::table::{self, By};
use crate::telegram::{button, escape, CallbackQuery, Message, Tg, Update};

const MAX_NAME_CHARS: usize = 40;
/// How often a live Cores screen is re-read, and for how long at most.
const LIVE_EVERY: Duration = Duration::from_secs(3);
const LIVE_FOR: Duration = Duration::from_secs(10 * 60);
/// Cores per page of the Cores screen — its table rows and 🗑 buttons.
const CORES_PER_PAGE: usize = 7;
/// Room for the report's notes under Telegram's 1024-character caption limit.
const CAPTION_BUDGET: usize = 900;
/// Wrong passphrases in a row before a pause.
const UNLOCK_TRIES: u32 = 3;
const UNLOCK_PAUSE: Duration = Duration::from_secs(60);
const LOCKED: &str = "🔒 I'm locked: the core keys are encrypted. Send your passphrase to unlock me — I delete \
                      your message as soon as I've read it.";

fn locked_kb() -> Value {
    json!([[button("Forgot it?", "l:forgot")]])
}

fn cancel_kb() -> Value {
    json!([[button("✖ Cancel", "x")]])
}

/// A screen's text — a caption when it comes with an image — and its buttons.
struct Screen {
    text: String,
    kb: Value,
    png: Option<Vec<u8>>,
    follow: Follow,
}

impl From<(String, Value)> for Screen {
    fn from((text, kb): (String, Value)) -> Self {
        Self { text, kb, png: None, follow: Follow::No }
    }
}

/// What keeps a screen current once it is shown.
enum Follow {
    No,
    /// The Cores screen, redrawn with its note while a core is settling.
    Cores(Option<String>),
}

/// No `Debug`: a pending add can hold a key.
enum Pending {
    Name,
    Key { name: String },
    Endpoint { name: String, key: String },
    /// A list of cores, one per line.
    Batch,
}

/// The Cores screen being kept current.
struct Live {
    chat: i64,
    message_id: i64,
    seq: u64,
}

pub struct Bot {
    me: Weak<Bot>,
    tg: Tg,
    /// Claims an unowned bot: `/start <code>`, as the startup log prints it.
    claim_code: Option<String>,
    offset_min: i64,
    store: Arc<Store>,
    cores: Arc<Cores>,
    pending: Mutex<Option<Pending>>,
    /// The add-core conversation's messages as `(chat, id)` — its prompts and
    /// the owner's answers — deleted together when it ends.
    trail: Mutex<Vec<(i64, i64)>>,
    live: Mutex<Option<Live>>,
    live_seq: AtomicU64,
    /// Held across a live screen's check and edit, and while a button takes
    /// its message over — so a late refresh never overwrites the new screen.
    screen_lock: tokio::sync::Mutex<()>,
    /// The Cores screen's page, kept across its screens; clamped when drawn.
    cores_page: AtomicUsize,
    /// Wrong passphrases in a row, and the pause they earned.
    unlock_fails: Mutex<(u32, Option<Instant>)>,
    status: Status,
}

impl Bot {
    pub fn new(
        tg: Tg,
        claim_code: Option<String>,
        offset_min: i64,
        store: Arc<Store>,
        cores: Arc<Cores>,
        status: Status,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            tg,
            claim_code,
            offset_min,
            store,
            cores,
            pending: Mutex::new(None),
            trail: Mutex::new(Vec::new()),
            live: Mutex::new(None),
            live_seq: AtomicU64::new(0),
            screen_lock: tokio::sync::Mutex::new(()),
            cores_page: AtomicUsize::new(0),
            unlock_fails: Mutex::new((0, None)),
            status,
        })
    }

    pub async fn run(&self) {
        let commands = [
            ("menu", "Main menu"),
            ("today", "PnL today"),
            ("month", "PnL this month"),
            ("cores", "Manage cores"),
            ("settings", "Settings"),
            ("cancel", "Cancel adding a core"),
        ];
        if let Err(e) = self.tg.set_commands(&commands).await {
            log::warn!("{e}");
        }
        if let Some(owner) = self.store.owner().filter(|_| self.store.is_locked()) {
            // A private chat's id is its user's.
            if let Err(e) = self.tg.send(owner, LOCKED, Some(locked_kb())).await {
                log::warn!("{e}");
            }
        }
        let mut offset = 0;
        loop {
            match self.tg.get_updates(offset).await {
                Ok(updates) => {
                    for update in updates {
                        offset = update.update_id + 1;
                        self.handle(update).await;
                    }
                }
                Err(e) => {
                    log::warn!("{e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn handle(&self, update: Update) {
        let result = if let Some(msg) = update.message {
            self.on_message(msg).await
        } else if let Some(cq) = update.callback_query {
            self.on_callback(cq).await
        } else {
            Ok(())
        };
        if let Err(e) = result {
            log::warn!("{e}");
        }
    }

    fn set_pending(&self, p: Option<Pending>) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = p;
        }
    }

    fn take_pending(&self) -> Option<Pending> {
        self.pending.lock().ok().and_then(|mut p| p.take())
    }

    fn follow(&self, chat: i64, message_id: i64) {
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
    async fn end_flow(&self, keep: Option<i64>) {
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

    /// Keep the Cores screen just shown as `shown` current while any core
    /// on it is connecting, syncing or reconnecting. Only the newest one is
    /// followed.
    fn watch(&self, chat: i64, message_id: i64, note: Option<String>, shown: String) {
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
    async fn unwatch(&self, chat: i64, message_id: i64) {
        let _screen = self.screen_lock.lock().await;
        if let Ok(mut live) = self.live.lock() {
            if live.as_ref().is_some_and(|l| l.chat == chat && l.message_id == message_id) {
                *live = None;
            }
        }
    }

    /// Whether a core is still on its way: connecting, syncing or reconnecting.
    fn settling(&self) -> bool {
        self.store
            .all()
            .iter()
            .any(|c| core_state(self.cores.view(&c.id).as_ref()).settling)
    }

    async fn show_cores(&self, chat: i64, note: Option<String>) -> Result<(), String> {
        self.show(chat, None, self.cores_screen(note)).await
    }

    async fn on_message(&self, msg: Message) -> Result<(), String> {
        let chat = msg.chat.id;
        let Some(user) = msg.from.as_ref().map(|u| u.id) else {
            return Ok(());
        };
        if msg.chat.kind != "private" {
            // A key pasted into a group would be read by everyone in it.
            return self.tg.send(chat, "Please talk to me in a private chat.", None).await;
        }
        let text = msg.text.as_deref().unwrap_or("").trim();
        match self.store.owner() {
            Some(owner) if owner == user => {}
            Some(_) => {
                log::info!("denied user {user}");
                return self.tg.send(chat, "⛔ This bot is private.", None).await;
            }
            None => return self.claim(chat, user, text).await,
        }
        if self.store.is_locked() {
            return self.on_locked(chat, msg.message_id, text).await;
        }
        if let Some(command) = text.strip_prefix('/') {
            let command = command.split(['@', ' ']).next().unwrap_or("");
            self.end_flow(None).await;
            let screen = match command {
                "today" => self.report_screen(Period::Today, By::Core).await,
                "month" => self.month_screen().into(),
                "cores" => self.cores_screen(None),
                "cancel" => self.cores_screen(Some("Cancelled.".to_string())),
                "settings" => self.settings_screen().into(),
                _ => self.main_screen().into(),
            };
            return self.show(chat, None, screen).await;
        }
        match self.take_pending() {
            Some(p) => self.on_pending(chat, msg.message_id, p, text).await,
            None => {
                let (text, kb) = self.main_screen();
                self.tg.send(chat, &text, Some(kb)).await
            }
        }
    }

    /// The first `/start <code>` with the startup log's code makes its sender
    /// the owner, for good.
    async fn claim(&self, chat: i64, user: i64, text: &str) -> Result<(), String> {
        let code = text.strip_prefix("/start").map(str::trim).filter(|c| !c.is_empty());
        if code.is_none() || code != self.claim_code.as_deref() {
            if code.is_some() {
                log::warn!("wrong claim code from user {user}");
            }
            let text = "This bot has no owner yet. Send <code>/start CODE</code> with the claim code \
                        from the server's log.";
            return self.tg.send(chat, text, None).await;
        }
        self.store.set_owner(user)?;
        log::info!("claimed by user {user}");
        self.status.update(&self.store);
        let (text, kb) = if self.store.is_locked() { (LOCKED.to_string(), locked_kb()) } else { self.main_screen() };
        self.tg.send(chat, &format!("✅ This bot is yours now.\n\n{text}"), Some(kb)).await
    }

    /// While locked, every message but a command is a try at the passphrase.
    async fn on_locked(&self, chat: i64, message_id: i64, text: &str) -> Result<(), String> {
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
    async fn delete_secret(&self, chat: i64, message_id: i64, what: &str) -> Result<(), String> {
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
    async fn on_locked_callback(&self, cq: CallbackQuery) -> Result<(), String> {
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

    async fn on_pending(&self, chat: i64, message_id: i64, pending: Pending, text: &str) -> Result<(), String> {
        let cancel = Some(cancel_kb());
        // A message with keys is deleted the moment it is read, not with the rest.
        if !matches!(pending, Pending::Key { .. } | Pending::Batch) {
            self.follow(chat, message_id);
        }
        match pending {
            Pending::Name => {
                let name = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if !valid_name(&name) {
                    self.set_pending(Some(Pending::Name));
                    let ask = format!("Send a name of 1–{MAX_NAME_CHARS} characters.");
                    return self.prompt(chat, &ask, cancel).await;
                }
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
        }
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

    async fn on_callback(&self, cq: CallbackQuery) -> Result<(), String> {
        let user = cq.from.id;
        if self.store.owner() != Some(user) {
            self.tg.answer_callback(&cq.id, Some("Access denied")).await;
            return Ok(());
        }
        if self.store.is_locked() {
            return self.on_locked_callback(cq).await;
        }
        let Some(msg) = cq.message else {
            self.tg.answer_callback(&cq.id, None).await;
            return Ok(());
        };
        let data = cq.data.as_deref().unwrap_or("");
        // The answer only stops the button's spinner: no need to wait for it.
        let (_, shown) = tokio::join!(self.tg.answer_callback(&cq.id, None), self.on_button(&msg, data));
        shown
    }

    async fn on_button(&self, msg: &Message, data: &str) -> Result<(), String> {
        self.unwatch(msg.chat.id, msg.message_id).await;
        // Any button ends an add-core conversation; `c:add` and `c:batch`
        // start a new one.
        self.end_flow(Some(msg.message_id)).await;
        let screen: Screen = match data {
            "r:t" => self.report_screen(Period::Today, By::Core).await,
            "r:mc" => self.report_screen(Period::Month, By::Core).await,
            "r:md" => self.report_screen(Period::Month, By::Date).await,
            "r:m" => self.month_screen().into(),
            "m" => self.main_screen().into(),
            _ if let Some(page) = data.strip_prefix("c:p:") => {
                self.cores_page.store(page.parse().unwrap_or(0), Ordering::Relaxed);
                self.cores_screen(None)
            }
            "c" => self.cores_screen(None),
            "x" => self.cores_screen(Some("Cancelled.".to_string())),
            "c:add" | "c:batch" => {
                let (pending, text) = if data == "c:add" {
                    (Pending::Name, "Send a name for the new core, e.g. <i>Binance futures</i>.")
                } else {
                    (
                        Pending::Batch,
                        "Send your cores in one message, one per line — a name, then the core's MoonProto key:\n\n\
                         <code>Binance futures  KEY\nGate: KEY\nKEY</code>\n\n\
                         A line with just a key takes MoonBot's own label for the key as its name. For a key \
                         that carries no address, add <code>host:port</code> after it. Empty lines and lines \
                         starting with # are skipped.\n\nI delete your message as soon as I've read it.",
                    )
                };
                self.set_pending(Some(pending));
                self.follow(msg.chat.id, msg.message_id);
                (text.to_string(), cancel_kb()).into()
            }
            "s" => self.settings_screen().into(),
            "s:emu" => {
                self.store.set_show_emulator(!self.store.settings().show_emulator)?;
                self.settings_screen().into()
            }
            "s:fmt" => {
                let next = match self.store.settings().report_format {
                    ReportFormat::Image => ReportFormat::Text,
                    ReportFormat::Text => ReportFormat::Image,
                };
                self.store.set_report_format(next)?;
                self.settings_screen().into()
            }
            _ if let Some(id) = data.strip_prefix("c:del:") => match self.store.get(id) {
                Some(core) => {
                    let text = format!(
                        "Delete <b>{}</b>?\n\nThe bot disconnects from it and drops its local copy of the trade \
                         history. Nothing changes on the core itself.",
                        escape(&core.name)
                    );
                    let kb = json!([[button("✅ Delete", &format!("c:rm:{}", core.id)), button("✖ Cancel", "c")]]);
                    (text, kb).into()
                }
                None => self.cores_screen(Some("That core is already gone.".to_string())),
            },
            _ if let Some(id) = data.strip_prefix("c:rm:") => match self.store.get(id) {
                Some(core) => {
                    self.cores.remove(&core.id);
                    self.store.remove(&core.id)?;
                    log::info!("deleted core {}", core.id);
                    self.cores_screen(Some(format!("🗑 <b>{}</b> deleted.", escape(&core.name))))
                }
                None => self.cores_screen(Some("That core is already gone.".to_string())),
            },
            _ => self.main_screen().into(),
        };
        self.show(msg.chat.id, Some(msg), screen).await.map(|_| ())
    }

    /// Show `screen` in `current`'s place, or as a new message. A text
    /// message can't become a photo or back, so crossing over sends the new
    /// one and deletes the old. A Cores screen is then kept current.
    async fn show(&self, chat: i64, current: Option<&Message>, screen: Screen) -> Result<(), String> {
        let Screen { text, kb, png, follow } = screen;
        let was_photo = current.is_some_and(|m| m.photo.is_some());
        let id = match (current, png) {
            (Some(m), Some(png)) if was_photo => {
                self.tg.edit_photo(chat, m.message_id, png, &text, Some(kb)).await?;
                m.message_id
            }
            (Some(m), None) if !was_photo => {
                self.tg.edit(chat, m.message_id, &text, Some(kb)).await?;
                m.message_id
            }
            (_, Some(png)) => self.tg.send_photo(chat, png, &text, Some(kb)).await?,
            (_, None) => self.tg.send_id(chat, &text, Some(kb)).await?,
        };
        if let Some(m) = current.filter(|m| m.message_id != id) {
            if let Err(e) = self.tg.delete_message(chat, m.message_id).await {
                log::warn!("{e}");
            }
        }
        if let Follow::Cores(note) = follow {
            self.watch(chat, id, note, text);
        }
        Ok(())
    }

    fn main_screen(&self) -> (String, Value) {
        let cores = self.store.all();
        let online = cores
            .iter()
            .filter(|c| self.cores.view(&c.id).is_some_and(|v| v.link == Link::Ready))
            .count();
        let body = if cores.is_empty() {
            "No cores yet — add one under 🖥 Cores.".to_string()
        } else {
            format!("Cores: {} · online: {online}", cores.len())
        };
        let kb = json!([
            [button("📊 Today", "r:t"), button("📅 Month", "r:m")],
            [button("🖥 Cores", "c"), button("⚙️ Settings", "s")],
        ]);
        (format!("<b>MoonBot PnL</b>\n\n{body}"), kb)
    }

    /// The month report's two layouts to pick from.
    fn month_screen(&self) -> (String, Value) {
        let now = chrono::Utc::now().timestamp_millis();
        let label = pnl::bounds(Period::Month, now, self.offset_min).label;
        let text = format!(
            "<b>📅 Month</b> — {label}\n\n🖥 <b>By core</b>: each core's PnL for the month.\n\
             📆 <b>By date</b>: all cores' PnL day by day, with the running total."
        );
        let kb = json!([
            [button("🖥 By core", "r:mc"), button("📆 By date", "r:md")],
            [button("⬅ Menu", "m")],
        ]);
        (text, kb)
    }

    fn settings_screen(&self) -> (String, Value) {
        let settings = self.store.settings();
        let show = settings.show_emulator;
        let image = settings.report_format == ReportFormat::Image;
        let text = format!(
            "<b>⚙️ Settings</b>\n\n🧪 Emulator trades: <b>{}</b> in reports\n📄 Reports come as: <b>{}</b>",
            if show { "shown" } else { "hidden" },
            if image { "an image" } else { "text" }
        );
        let emu = if show { "🙈 Hide emulator trades" } else { "🧪 Show emulator trades" };
        let format = if image { "📝 Send reports as text" } else { "🖼 Send reports as an image" };
        (text, json!([[button(emu, "s:emu")], [button(format, "s:fmt")], [button("⬅ Menu", "m")]]))
    }

    fn cores_screen(&self, note: Option<String>) -> Screen {
        let all = self.store.all();
        let pages = all.len().div_ceil(CORES_PER_PAGE).max(1);
        let page = self.cores_page.load(Ordering::Relaxed).min(pages - 1);
        self.cores_page.store(page, Ordering::Relaxed);
        let cores = all.iter().skip(page * CORES_PER_PAGE).take(CORES_PER_PAGE);
        let mut text = String::from("<b>🖥 Cores</b>");
        if pages > 1 {
            text.push_str(&format!(" · page {} of {pages} · {} cores", page + 1, all.len()));
        }
        text.push('\n');
        if let Some(n) = &note {
            text.push_str(&format!("\n{n}\n"));
        }
        text.push('\n');
        if all.is_empty() {
            text.push_str("No cores yet. Add one with its MoonProto key.");
        }
        let mut rows = Vec::new();
        let mut table = Vec::new();
        let mut notes = String::new();
        for core in cores {
            let view = self.cores.view(&core.id);
            let state = core_state(view.as_ref());
            let exchange = view.as_ref().and_then(|v| v.exchange.clone()).unwrap_or_else(|| "—".to_string());
            table.push((core.name.clone(), exchange, state.icon));
            if let Some(s) = state.note {
                notes.push_str(&format!("{} <b>{}</b>: <i>{s}</i>\n", state.icon, escape(&core.name)));
            }
            rows.push(json!([button(&format!("🗑 {}", core.name), &format!("c:del:{}", core.id))]));
        }
        if !table.is_empty() {
            // The dot goes last: emoji width in Telegram's monospace varies by client.
            let name_w = table.iter().map(|(n, ..)| n.chars().count()).max().unwrap_or(0).max(4);
            let exch_w = table.iter().map(|(_, e, _)| e.chars().count()).max().unwrap_or(0).max(8);
            let mut pre = format!("{:name_w$}  {:exch_w$}\n", "Core", "Exchange");
            for (name, exchange, icon) in &table {
                pre.push_str(&format!("{name:name_w$}  {exchange:exch_w$}  {icon}\n"));
            }
            text.push_str(&format!("<pre>{}</pre>", escape(pre.trim_end())));
            if !notes.is_empty() {
                text.push_str(&format!("\n\n{notes}"));
            }
        }
        if pages > 1 {
            let mut nav = Vec::new();
            if page > 0 {
                nav.push(button("◀ Prev", &format!("c:p:{}", page - 1)));
            }
            if page + 1 < pages {
                nav.push(button("Next ▶", &format!("c:p:{}", page + 1)));
            }
            rows.push(Value::Array(nav));
        }
        rows.push(json!([button("➕ Add core", "c:add"), button("📋 Add several", "c:batch")]));
        rows.push(json!([button("⬅ Menu", "m")]));
        Screen { text, kb: Value::Array(rows), png: None, follow: Follow::Cores(note) }
    }

    /// The report for `period` laid out `by` core or date: a table image
    /// with the notes as its caption, or text alone while there's nothing
    /// to draw. Today's is always by core.
    async fn report_screen(&self, period: Period, by: By) -> Screen {
        let now = chrono::Utc::now().timestamp_millis();
        let bounds = pnl::bounds(period, now, self.offset_min);
        let cores = self.store.all();
        let paths: Vec<_> = cores.iter().map(|c| self.cores.replica_path(&c.id)).collect();
        let (from, to) = (bounds.from, bounds.to);

        let (title, label, kb, by) = match (period, by) {
            (Period::Today, _) => (
                "Today",
                bounds.label.clone(),
                json!([[button("🔄 Refresh", "r:t"), button("📅 Month", "r:m")], [button("⬅ Menu", "m")]]),
                By::Core,
            ),
            (Period::Month, By::Core) => (
                "Month",
                bounds.label.clone(),
                json!([
                    [button("🔄 Refresh", "r:mc"), button("📆 By date", "r:md")],
                    [button("📊 Today", "r:t"), button("⬅ Menu", "m")],
                ]),
                By::Core,
            ),
            (Period::Month, By::Date) => (
                "Month",
                format!("{} · by date", bounds.label),
                json!([
                    [button("🔄 Refresh", "r:md"), button("🖥 By core", "r:mc")],
                    [button("📊 Today", "r:t"), button("⬅ Menu", "m")],
                ]),
                By::Date,
            ),
        };
        if cores.is_empty() {
            let text = format!("<b>{title}</b> — {label}\n\nNo cores yet — add one under 🖥 Cores.");
            return Screen { text, kb, png: None, follow: Follow::No };
        }

        let settings = self.store.settings();
        let show_emu = settings.show_emulator;
        let mut notes = Vec::new();
        let rows = match by {
            By::Core => {
                let tallies = per_replica(paths, move |p| pnl::tally(p, from, to)).await;
                self.core_rows(&cores, tallies, show_emu, &mut notes)
            }
            By::Date => {
                let dailies = per_replica(paths, move |p| pnl::daily(p, from, to)).await;
                date_rows(&cores, dailies, show_emu, &mut notes)
            }
        };

        let zone = match chrono::FixedOffset::east_opt(self.offset_min as i32 * 60) {
            Some(offset) if self.offset_min != 0 => format!("UTC{offset}"),
            _ => "UTC".to_string(),
        };
        let report = table::Report {
            title: title.to_string(),
            label: label.clone(),
            updated: format!("updated {}", chrono::Utc::now().format("%H:%M:%S UTC")),
            footer: format!("Closed trades by close time, {zone}"),
            by,
            rows,
        };
        // A caption holds 1024 characters; the notes are cut to fit, and to
        // keep a text report under a message's 4096.
        let mut text = String::new();
        for (i, note) in notes.iter().enumerate() {
            if text.len() + note.len() > CAPTION_BUDGET {
                text.push_str(&format!("…and {} more", notes.len() - i));
                break;
            }
            text.push_str(note);
            text.push('\n');
        }
        if settings.report_format == ReportFormat::Text {
            let table = report_text(&report);
            let text = if text.is_empty() { table } else { format!("{table}\n\n{}", text.trim_end()) };
            return Screen { text, kb, png: None, follow: Follow::No };
        }
        let png = tokio::task::spawn_blocking(move || table::render(&report))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        match png {
            Ok(png) => Screen { text: text.trim_end().to_string(), kb, png: Some(png), follow: Follow::No },
            Err(e) => {
                log::warn!("{e}");
                let text = format!("<b>{title}</b> — {label}\n\n⚠️ Couldn't draw the report: {}\n\n{text}", escape(&e));
                Screen { text, kb, png: None, follow: Follow::No }
            }
        }
    }

    /// A row per core — two with emulator trades — best profit first, then
    /// the totals when there are several cores.
    fn core_rows(
        &self,
        cores: &[CoreEntry],
        tallies: Vec<Result<Option<CoreTally>, String>>,
        show_emu: bool,
        notes: &mut Vec<String>,
    ) -> Vec<table::Row> {
        let mut real_total: BTreeMap<String, Tally> = BTreeMap::new();
        let mut emu_total: BTreeMap<String, Tally> = BTreeMap::new();
        // Each core's rows with its profit, best first; cores without figures last.
        let mut groups: Vec<(f64, Vec<table::Row>)> = Vec::new();
        for (core, tally) in cores.iter().zip(tallies) {
            let exchange = self.cores.view(&core.id).and_then(|v| v.exchange).unwrap_or_default();
            let cur = core.currency.clone();
            let row = |sub: String, kind, tally| table::Row {
                name: core.name.clone(),
                sub,
                kind,
                tally,
                currency: cur.clone(),
                cumulative: None,
            };
            let mut rows = Vec::new();
            let key = match tally {
                Ok(Some(CoreTally { real, emulator })) => {
                    let emulator = if show_emu { emulator } else { Tally::default() };
                    if real.trades > 0 || emulator.trades == 0 {
                        rows.push(row(exchange.clone(), table::Kind::Core, Some(real)));
                        real_total.entry(cur.clone()).or_default().merge(&real);
                    }
                    if emulator.trades > 0 {
                        let sub = if exchange.is_empty() { "emulator".to_string() } else { format!("emulator · {exchange}") };
                        rows.push(row(sub, table::Kind::Emulator, Some(emulator)));
                        emu_total.entry(cur.clone()).or_default().merge(&emulator);
                    }
                    real.profit
                }
                Ok(None) | Err(_) => {
                    rows.push(row(exchange.clone(), table::Kind::Core, None));
                    notes.push(missing_note(core, tally.err()));
                    f64::NEG_INFINITY
                }
            };
            groups.push((key, rows));
        }
        groups.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut rows: Vec<table::Row> = groups.into_iter().flat_map(|(_, rows)| rows).collect();
        if cores.len() > 1 {
            rows.extend(total_rows(&real_total, &emu_total));
        }
        rows
    }
}

/// `read` on every replica at once, each on its own blocking thread;
/// answers in `paths`' order.
async fn per_replica<T: Send + 'static>(
    paths: Vec<PathBuf>,
    read: impl Fn(&Path) -> Result<T, String> + Copy + Send + 'static,
) -> Vec<Result<T, String>> {
    let tasks: Vec<_> = paths.into_iter().map(|p| tokio::task::spawn_blocking(move || read(&p))).collect();
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        out.push(task.await.map_err(|e| e.to_string()).and_then(|r| r));
    }
    out
}

/// A row per day with trades, newest first, summed over the cores, then the
/// totals. Emulator trades show in the totals only.
fn date_rows(
    cores: &[CoreEntry],
    dailies: Vec<Result<Option<BTreeMap<NaiveDate, CoreTally>>, String>>,
    show_emu: bool,
    notes: &mut Vec<String>,
) -> Vec<table::Row> {
    // Keyed by currency first: a sum across currencies means nothing.
    let mut days: BTreeMap<(String, NaiveDate), Tally> = BTreeMap::new();
    let mut real_total: BTreeMap<String, Tally> = BTreeMap::new();
    let mut emu_total: BTreeMap<String, Tally> = BTreeMap::new();
    for (core, daily) in cores.iter().zip(dailies) {
        let daily = match daily {
            Ok(Some(daily)) => daily,
            Ok(None) | Err(_) => {
                notes.push(missing_note(core, daily.err()));
                continue;
            }
        };
        let cur = &core.currency;
        for (day, CoreTally { real, emulator }) in daily {
            if real.trades > 0 {
                days.entry((cur.clone(), day)).or_default().merge(&real);
                real_total.entry(cur.clone()).or_default().merge(&real);
            }
            if show_emu && emulator.trades > 0 {
                emu_total.entry(cur.clone()).or_default().merge(&emulator);
            }
        }
    }
    // Oldest first within a currency, for the running total.
    let mut cumulative: BTreeMap<String, f64> = BTreeMap::new();
    let mut rows: Vec<table::Row> = days
        .into_iter()
        .map(|((currency, day), t)| {
            let sum = cumulative.entry(currency.clone()).or_default();
            *sum += t.profit;
            table::Row {
                name: day.format("%Y-%m-%d").to_string(),
                sub: String::new(),
                kind: table::Kind::Core,
                tally: Some(t),
                currency,
                cumulative: Some(*sum),
            }
        })
        .collect();
    // ISO dates sort as text; stable, so a day's currencies keep their order.
    rows.sort_by(|a, b| b.name.cmp(&a.name));
    if real_total.is_empty() {
        real_total.insert(cores.first().map(|c| c.currency.clone()).unwrap_or_default(), Tally::default());
    }
    rows.extend(total_rows(&real_total, &emu_total));
    rows
}

/// A TOTAL row per currency, then an emulator one per currency.
fn total_rows(real: &BTreeMap<String, Tally>, emulator: &BTreeMap<String, Tally>) -> Vec<table::Row> {
    let total = |sub: &str, currency: &String, t: &Tally| table::Row {
        name: "TOTAL".to_string(),
        sub: sub.to_string(),
        kind: table::Kind::Total,
        tally: Some(*t),
        currency: currency.clone(),
        cumulative: None,
    };
    real.iter().map(|(cur, t)| total("", cur, t)).chain(emulator.iter().map(|(cur, t)| total("emulator", cur, t))).collect()
}

/// Why a core has no figures: no history synced yet, or `error`.
fn missing_note(core: &CoreEntry, error: Option<String>) -> String {
    match error {
        None => format!("<b>{}</b>: <i>no trade history yet</i>", escape(&core.name)),
        Some(e) => format!("⚠️ <b>{}</b>: {}", escape(&core.name), escape(&e)),
    }
}

/// A core as the Cores screen shows it.
struct CoreState {
    icon: &'static str,
    /// Why its figures may be incomplete.
    note: Option<String>,
    /// Still on its way: connecting, syncing or reconnecting.
    settling: bool,
}

fn core_state(view: Option<&CoreView>) -> CoreState {
    let state = |icon, note: Option<String>, settling| CoreState { icon, note, settling };
    let Some(view) = view else {
        return state("⚪", Some("not connected".to_string()), false);
    };
    match &view.link {
        Link::Failed(e) => state("🔴", Some(format!("offline: {} — figures from the last sync", escape(e))), false),
        Link::Reconnecting => state("🟠", Some("reconnecting — figures from the last sync".to_string()), true),
        Link::Connecting => state("🟡", Some("connecting".to_string()), true),
        Link::Ready => match view.report.phase {
            Phase::Live => state("🟢", None, false),
            Phase::Error => state(
                "🔴",
                Some(format!("report sync error: {}", escape(view.report.error.as_deref().unwrap_or("unknown")))),
                false,
            ),
            Phase::Schema | Phase::Page | Phase::Complete => {
                state("🟡", Some(format!("syncing trade history ({} rows so far)", view.report.rows_synced)), true)
            }
        },
    }
}

/// The report as a message: the image's rows as a monospace table. Volume,
/// average order and the exchange are left out to fit a phone's width, and
/// a day shows without its year — the title has it.
fn report_text(report: &table::Report) -> String {
    let by_date = report.by == By::Date;
    let head: &[&str] = if by_date { &["Day", "Ord", "W/L", "Profit", "%", "Cum"] } else { &["Core", "Ord", "W/L", "Profit", "%"] };
    let mut lines: Vec<Vec<String>> = vec![head.iter().map(|h| h.to_string()).collect()];
    let mut body = 0;
    for row in &report.rows {
        let name = match row.kind {
            table::Kind::Core if by_date => row.name.get(5..).unwrap_or(&row.name).to_string(),
            table::Kind::Core => row.name.clone(),
            table::Kind::Emulator => format!("{} emu", row.name),
            table::Kind::Total if row.sub.is_empty() => "Total".to_string(),
            table::Kind::Total => "Total emu".to_string(),
        };
        let c = table::cells(row);
        let mut line = vec![name, c.orders, c.wl.replace(" / ", "/"), c.profit, c.pct];
        if by_date {
            line.push(c.cumulative);
        }
        lines.push(line);
        if !matches!(row.kind, table::Kind::Total) {
            body = lines.len();
        }
    }
    let mut w = vec![0usize; head.len()];
    for line in &lines {
        for (w, cell) in w.iter_mut().zip(line) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut pre = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i == body && body < lines.len() {
            pre.push_str(&format!("{}\n", "─".repeat(w.iter().sum::<usize>() + 2 * (w.len() - 1))));
        }
        // The name left-aligned, the figures right.
        let cells: Vec<String> = line
            .iter()
            .zip(&w)
            .enumerate()
            .map(|(i, (cell, &w))| if i == 0 { format!("{cell:w$}") } else { format!("{cell:>w$}") })
            .collect();
        pre.push_str(&cells.join("  "));
        pre.push('\n');
    }
    format!(
        "<b>{}</b> — {}\n<pre>{}</pre>\n<i>{} · {}</i>",
        escape(&report.title),
        escape(&report.label),
        // Only the newline: a TOTAL row's blank last cell is padding that lines up.
        escape(pre.trim_end_matches('\n')),
        escape(&report.footer),
        escape(&report.updated)
    )
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

    #[test]
    fn text_report_lines_align() {
        let t = |trades, wins, profit, volume| Some(Tally { trades, wins, profit, volume });
        let row = |name: &str, sub: &str, kind, tally| table::Row {
            name: name.into(),
            sub: sub.into(),
            kind,
            tally,
            currency: "USDT".into(),
            cumulative: None,
        };
        let aligned = |report: &table::Report| {
            let text = report_text(report);
            let pre = text.split("<pre>").nth(1).and_then(|s| s.split("</pre>").next()).unwrap().to_string();
            let pre = pre.replace("&lt;", "<").replace("&gt;", ">");
            let widths: Vec<usize> = pre.lines().map(|l| l.chars().count()).collect();
            assert!(widths.iter().all(|&w| w == widths[0]), "{widths:?}");
            pre
        };
        let day = |name: &str, tally, cumulative| table::Row { cumulative: Some(cumulative), ..row(name, "", table::Kind::Core, tally) };
        let by_date = table::Report {
            title: "Month".into(),
            label: "September 2026 · by date".into(),
            updated: "updated 13:08:25 UTC".into(),
            footer: "Closed trades by close time, UTC".into(),
            by: By::Date,
            rows: vec![
                day("2026-09-24", t(7, 5, 85.82, 44_000.0), -139.2),
                day("2026-09-23", t(26, 15, -225.02, 112_000.0), -225.02),
                row("TOTAL", "", table::Kind::Total, t(33, 20, -139.2, 156_000.0)),
            ],
        };
        let pre = aligned(&by_date);
        assert!(pre.lines().nth(1).unwrap().starts_with("09-24"), "{pre}");
        let report = table::Report {
            title: "Month".into(),
            label: "September 2026".into(),
            updated: "updated 13:08:25 UTC".into(),
            footer: "Closed trades by close time, UTC".into(),
            by: By::Core,
            rows: vec![
                row("Bin9", "ByBit Futures", table::Kind::Core, t(95, 67, 1087.85, 271_962.5)),
                row("Bin9", "emulator", table::Kind::Emulator, t(2, 1, -376.01, 7_915.9)),
                row("test<1>", "", table::Kind::Core, None),
                row("TOTAL", "", table::Kind::Total, t(97, 68, 711.84, 279_878.4)),
            ],
        };
        aligned(&report);
    }
}
