//! Updates in, screens out. Every screen is one message edited in place by
//! its inline buttons; adding a core is a short conversation (name, key,
//! and the endpoint when the key carries none) held in `pending`, or one
//! message listing several cores at once (`parse_batch_line`). The Cores
//! screen last shown follows its cores while one is still on its way.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cores::{self, CoreView, Cores, Link};
use crate::pnl::{self, CoreTally, Period, Tally};
use crate::replica::Phase;
use crate::store::{CoreEntry, ReportFormat, Store};
use crate::table;
use crate::telegram::{button, escape, CallbackQuery, Message, Tg, Update};

const MAX_NAME_CHARS: usize = 40;
/// How often a live Cores screen is re-read, and for how long at most.
const LIVE_EVERY: Duration = Duration::from_secs(3);
const LIVE_FOR: Duration = Duration::from_secs(10 * 60);
/// Cores per page of the Cores screen — its table rows and 🗑 buttons.
const CORES_PER_PAGE: usize = 7;
/// Room for the report's notes under Telegram's 1024-character caption limit.
const CAPTION_BUDGET: usize = 900;

/// A screen's text — a caption when it comes with an image — and its buttons.
struct Screen {
    text: String,
    kb: Value,
    png: Option<Vec<u8>>,
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
}

impl Bot {
    pub fn new(tg: Tg, claim_code: Option<String>, offset_min: i64, store: Arc<Store>, cores: Arc<Cores>) -> Arc<Self> {
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
                let (text, kb) = bot.cores_screen(note.as_deref());
                if text != shown {
                    if let Err(e) = bot.tg.edit(chat, message_id, &text, Some(kb)).await {
                        log::warn!("{e}");
                        break;
                    }
                    shown = text;
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
            .any(|c| matches!(core_state(self.cores.view(&c.id).as_ref()).0, "🟡" | "🟠"))
    }

    async fn show_cores(&self, chat: i64, note: Option<String>) -> Result<(), String> {
        let (text, kb) = self.cores_screen(note.as_deref());
        let id = self.tg.send_id(chat, &text, Some(kb)).await?;
        self.watch(chat, id, note, text);
        Ok(())
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
        if let Some(command) = text.strip_prefix('/') {
            let command = command.split(['@', ' ']).next().unwrap_or("");
            self.end_flow(None).await;
            let (text, kb) = match command {
                "today" => return self.show(chat, None, self.report_screen(Period::Today).await).await.map(|_| ()),
                "month" => return self.show(chat, None, self.report_screen(Period::Month).await).await.map(|_| ()),
                "cores" => return self.show_cores(chat, None).await,
                "cancel" => return self.show_cores(chat, Some("Cancelled.".to_string())).await,
                "settings" => self.settings_screen(),
                _ => self.main_screen(),
            };
            return self.tg.send(chat, &text, Some(kb)).await;
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
        let (text, kb) = self.main_screen();
        self.tg.send(chat, &format!("✅ This bot is yours now.\n\n{text}"), Some(kb)).await
    }

    async fn on_pending(&self, chat: i64, message_id: i64, pending: Pending, text: &str) -> Result<(), String> {
        let cancel = Some(json!([[button("✖ Cancel", "x")]]));
        // A message with keys is deleted the moment it is read, not with the rest.
        if !matches!(pending, Pending::Key { .. } | Pending::Batch) {
            self.follow(chat, message_id);
        }
        match pending {
            Pending::Name => {
                let name = text.split_whitespace().collect::<Vec<_>>().join(" ");
                if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
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
                let deleted = self.tg.delete_message(chat, message_id).await;
                if let Err(e) = &deleted {
                    log::warn!("{e}");
                    let warn = "⚠️ I couldn't delete your message with the key — delete it yourself.";
                    self.tg.send(chat, warn, None).await?;
                }
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
                if let Err(e) = self.tg.delete_message(chat, message_id).await {
                    log::warn!("{e}");
                    let warn = "⚠️ I couldn't delete your message with the keys — delete it yourself.";
                    self.tg.send(chat, warn, None).await?;
                }
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
        let entry = CoreEntry { id: new_id(), name, key, host, port, currency: String::new() };
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
            note.push_str(&format!("

⚠️ Skipped:
• {}", skipped.join("
• ")));
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
        let Some(msg) = cq.message else {
            self.tg.answer_callback(&cq.id, None).await;
            return Ok(());
        };
        let data = cq.data.as_deref().unwrap_or("");
        self.tg.answer_callback(&cq.id, None).await;
        self.unwatch(msg.chat.id, msg.message_id).await;
        // Set by the arms that show the Cores screen: its note, to follow it live.
        let mut cores_note: Option<Option<String>> = None;
        let mut cores = |note: Option<String>| {
            let screen = self.cores_screen(note.as_deref());
            cores_note = Some(note);
            screen
        };
        let period = match data {
            "r:t" => Some(Period::Today),
            "r:m" => Some(Period::Month),
            _ => None,
        };
        if let Some(period) = period {
            let screen = self.report_screen(period).await;
            return self.show(msg.chat.id, Some(&msg), screen).await.map(|_| ());
        }
        let (text, kb) = match data {
            "m" => {
                self.end_flow(Some(msg.message_id)).await;
                self.main_screen()
            }
            _ if data.starts_with("c:p:") => {
                self.end_flow(Some(msg.message_id)).await;
                self.cores_page.store(data[4..].parse().unwrap_or(0), Ordering::Relaxed);
                cores(None)
            }
            "c" => {
                self.end_flow(Some(msg.message_id)).await;
                cores(None)
            }
            "x" => {
                self.end_flow(Some(msg.message_id)).await;
                cores(Some("Cancelled.".to_string()))
            }
            "c:add" => {
                self.end_flow(Some(msg.message_id)).await;
                self.set_pending(Some(Pending::Name));
                self.follow(msg.chat.id, msg.message_id);
                let text = "Send a name for the new core, e.g. <i>Binance futures</i>.".to_string();
                (text, json!([[button("✖ Cancel", "x")]]))
            }
            "c:batch" => {
                self.end_flow(Some(msg.message_id)).await;
                self.set_pending(Some(Pending::Batch));
                self.follow(msg.chat.id, msg.message_id);
                let text = "Send your cores in one message, one per line — a name, then the core's MoonProto key:\n\n\
                            <code>Binance futures  KEY\nGate: KEY\nKEY</code>\n\n\
                            A line with just a key takes MoonBot's own label for the key as its name. For a key \
                            that carries no address, add <code>host:port</code> after it. Empty lines and lines \
                            starting with # are skipped.\n\nI delete your message as soon as I've read it."
                    .to_string();
                (text, json!([[button("✖ Cancel", "x")]]))
            }
            "s" => {
                self.end_flow(Some(msg.message_id)).await;
                self.settings_screen()
            }
            "s:emu" => {
                self.store.set_show_emulator(!self.store.settings().show_emulator)?;
                self.settings_screen()
            }
            "s:fmt" => {
                let next = match self.store.settings().report_format {
                    ReportFormat::Image => ReportFormat::Text,
                    ReportFormat::Text => ReportFormat::Image,
                };
                self.store.set_report_format(next)?;
                self.settings_screen()
            }
            _ if data.starts_with("c:del:") => match self.store.get(&data[6..]) {
                Some(core) => {
                    let text = format!(
                        "Delete <b>{}</b>?\n\nThe bot disconnects from it and drops its local copy of the trade \
                         history. Nothing changes on the core itself.",
                        escape(&core.name)
                    );
                    let kb = json!([[button("✅ Delete", &format!("c:rm:{}", core.id)), button("✖ Cancel", "c")]]);
                    (text, kb)
                }
                None => cores(Some("That core is already gone.".to_string())),
            },
            _ if data.starts_with("c:rm:") => match self.store.get(&data[5..]) {
                Some(core) => {
                    self.cores.remove(&core.id);
                    self.store.remove(&core.id)?;
                    log::info!("deleted core {}", core.id);
                    cores(Some(format!("🗑 <b>{}</b> deleted.", escape(&core.name))))
                }
                None => cores(Some("That core is already gone.".to_string())),
            },
            _ => self.main_screen(),
        };
        let id = self.show(msg.chat.id, Some(&msg), Screen { text: text.clone(), kb, png: None }).await?;
        if let Some(note) = cores_note {
            self.watch(msg.chat.id, id, note, text);
        }
        Ok(())
    }

    /// Show `screen` in `current`'s place, or as a new message. A text
    /// message can't become a photo or back, so crossing over sends the new
    /// one and deletes the old. Answers the id the screen ended up in.
    async fn show(&self, chat: i64, current: Option<&Message>, screen: Screen) -> Result<i64, String> {
        let Screen { text, kb, png } = screen;
        let was_photo = current.is_some_and(|m| m.photo.is_some());
        let id = match (current, png) {
            (Some(m), Some(png)) if was_photo => {
                self.tg.edit_photo(chat, m.message_id, png, &text, Some(kb)).await?;
                return Ok(m.message_id);
            }
            (Some(m), None) if !was_photo => {
                self.tg.edit(chat, m.message_id, &text, Some(kb)).await?;
                return Ok(m.message_id);
            }
            (_, Some(png)) => self.tg.send_photo(chat, png, &text, Some(kb)).await?,
            (_, None) => self.tg.send_id(chat, &text, Some(kb)).await?,
        };
        if let Some(m) = current {
            if let Err(e) = self.tg.delete_message(chat, m.message_id).await {
                log::warn!("{e}");
            }
        }
        Ok(id)
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

    fn cores_screen(&self, note: Option<&str>) -> (String, Value) {
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
        if let Some(n) = note {
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
            let (icon, state) = core_state(view.as_ref());
            let exchange = view.as_ref().and_then(|v| v.exchange.clone()).unwrap_or_else(|| "—".to_string());
            table.push((core.name.clone(), exchange, icon));
            if let Some(s) = state {
                notes.push_str(&format!("{icon} <b>{}</b>: <i>{s}</i>\n", escape(&core.name)));
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
        (text, Value::Array(rows))
    }

    /// The report for `period`: a table image with the notes as its caption,
    /// or text alone while there's nothing to draw.
    async fn report_screen(&self, period: Period) -> Screen {
        let now = chrono::Utc::now().timestamp_millis();
        let bounds = pnl::bounds(period, now, self.offset_min);
        let cores = self.store.all();
        let paths: Vec<_> = cores.iter().map(|c| self.cores.replica_path(&c.id)).collect();
        let (from, to) = (bounds.from, bounds.to);
        let tallies = tokio::task::spawn_blocking(move || paths.iter().map(|p| pnl::tally(p, from, to)).collect::<Vec<_>>())
        .await
        .unwrap_or_default();

        let (title, other) = match period {
            Period::Today => ("Today", ("📅 Month", "r:m")),
            Period::Month => ("Month", ("📊 Today", "r:t")),
        };
        let refresh = if period == Period::Today { "r:t" } else { "r:m" };
        let kb = json!([
            [button("🔄 Refresh", refresh), button(other.0, other.1)],
            [button("⬅ Menu", "m")],
        ]);
        if cores.is_empty() {
            let text = format!("<b>{title}</b> — {}\n\nNo cores yet — add one under 🖥 Cores.", bounds.label);
            return Screen { text, kb, png: None };
        }

        let show_emu = self.store.settings().show_emulator;
        let mut real_total: BTreeMap<String, Tally> = BTreeMap::new();
        let mut emu_total: BTreeMap<String, Tally> = BTreeMap::new();
        // Each core's rows with its profit, best first; cores without figures last.
        let mut groups: Vec<(f64, Vec<table::Row>)> = Vec::new();
        let mut notes = Vec::new();
        for (core, tally) in cores.iter().zip(tallies) {
            let exchange = self.cores.view(&core.id).and_then(|v| v.exchange).unwrap_or_default();
            let cur = core.currency.clone();
            let row = |sub: String, kind, tally| table::Row { name: core.name.clone(), sub, kind, tally, currency: cur.clone() };
            let mut rows = Vec::new();
            let key = match tally {
                Ok(Some(CoreTally { real, emulator })) => {
                    let emulator = if show_emu { emulator } else { Tally::default() };
                    if real.trades > 0 || emulator.trades == 0 {
                        rows.push(row(exchange.clone(), table::Kind::Core, Some(real)));
                        merge(real_total.entry(cur.clone()).or_default(), &real);
                    }
                    if emulator.trades > 0 {
                        let sub = if exchange.is_empty() { "emulator".to_string() } else { format!("emulator · {exchange}") };
                        rows.push(row(sub, table::Kind::Emulator, Some(emulator)));
                        merge(emu_total.entry(cur.clone()).or_default(), &emulator);
                    }
                    real.profit
                }
                Ok(None) => {
                    rows.push(row(exchange.clone(), table::Kind::Core, None));
                    notes.push(format!("<b>{}</b>: <i>no trade history yet</i>", escape(&core.name)));
                    f64::NEG_INFINITY
                }
                Err(e) => {
                    rows.push(row(exchange.clone(), table::Kind::Core, None));
                    notes.push(format!("⚠️ <b>{}</b>: {}", escape(&core.name), escape(&e)));
                    f64::NEG_INFINITY
                }
            };
            groups.push((key, rows));
        }
        groups.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut rows: Vec<table::Row> = groups.into_iter().flat_map(|(_, rows)| rows).collect();
        if cores.len() > 1 {
            let total = |sub: &str, currency: &String, t: &Tally| table::Row {
                name: "TOTAL".to_string(),
                sub: sub.to_string(),
                kind: table::Kind::Total,
                tally: Some(*t),
                currency: currency.clone(),
            };
            rows.extend(real_total.iter().map(|(cur, t)| total("", cur, t)));
            rows.extend(emu_total.iter().map(|(cur, t)| total("emulator", cur, t)));
        }

        let zone = match self.offset_min {
            0 => "UTC".to_string(),
            m => format!("UTC{}{:02}:{:02}", if m < 0 { '-' } else { '+' }, m.abs() / 60, m.abs() % 60),
        };
        let report = table::Report {
            title: title.to_string(),
            label: bounds.label.clone(),
            updated: format!("updated {}", chrono::Utc::now().format("%H:%M:%S UTC")),
            footer: format!("Closed trades by close time, {zone}"),
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
        if self.store.settings().report_format == ReportFormat::Text {
            let table = report_text(&report);
            let text = if text.is_empty() { table } else { format!("{table}\n\n{}", text.trim_end()) };
            return Screen { text, kb, png: None };
        }
        let png = tokio::task::spawn_blocking(move || table::render(&report))
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
        match png {
            Ok(png) => Screen { text: text.trim_end().to_string(), kb, png: Some(png) },
            Err(e) => {
                log::warn!("{e}");
                let text = format!("<b>{title}</b> — {}\n\n⚠️ Couldn't draw the report: {}\n\n{text}", bounds.label, escape(&e));
                Screen { text, kb, png: None }
            }
        }
    }
}

/// The core's dot, and a note while its figures may be incomplete.
fn core_state(view: Option<&CoreView>) -> (&'static str, Option<String>) {
    let Some(view) = view else {
        return ("⚪", Some("not connected".to_string()));
    };
    match &view.link {
        Link::Failed(e) => ("🔴", Some(format!("offline: {} — figures from the last sync", escape(e)))),
        Link::Reconnecting => ("🟠", Some("reconnecting — figures from the last sync".to_string())),
        Link::Connecting | Link::Initializing => ("🟡", Some("connecting".to_string())),
        Link::Ready => match view.report.phase {
            Phase::Live => ("🟢", None),
            Phase::Offline => ("🟠", Some("report link offline".to_string())),
            Phase::Error => (
                "🔴",
                Some(format!("report sync error: {}", escape(view.report.error.as_deref().unwrap_or("unknown")))),
            ),
            Phase::Schema | Phase::Page | Phase::Complete => {
                ("🟡", Some(format!("syncing trade history ({} rows so far)", view.report.rows_synced)))
            }
        },
    }
}

/// The report as a message: the image's rows as a monospace table. Volume,
/// average order and the exchange are left out to fit a phone's width.
fn report_text(report: &table::Report) -> String {
    let mut lines: Vec<[String; 5]> = vec![["Core", "Ord", "W/L", "Profit", "%"].map(str::to_string)];
    let mut body = 0;
    for row in &report.rows {
        let name = match row.kind {
            table::Kind::Core => row.name.clone(),
            table::Kind::Emulator => format!("{} emu", row.name),
            table::Kind::Total if row.sub.is_empty() => "Total".to_string(),
            table::Kind::Total => "Total emu".to_string(),
        };
        let c = table::cells(row);
        lines.push([name, c.orders, c.wl.replace(" / ", "/"), c.profit, c.pct]);
        if !matches!(row.kind, table::Kind::Total) {
            body = lines.len();
        }
    }
    let mut w = [0usize; 5];
    for line in &lines {
        for (w, cell) in w.iter_mut().zip(line) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut pre = String::new();
    for (i, [name, orders, wl, profit, pct]) in lines.iter().enumerate() {
        if i == body && body < lines.len() {
            pre.push_str(&format!("{}\n", "─".repeat(w.iter().sum::<usize>() + 8)));
        }
        pre.push_str(&format!(
            "{name:w0$}  {orders:>w1$}  {wl:>w2$}  {profit:>w3$}  {pct:>w4$}\n",
            w0 = w[0],
            w1 = w[1],
            w2 = w[2],
            w3 = w[3],
            w4 = w[4]
        ));
    }
    format!(
        "<b>{}</b> — {}\n<pre>{}</pre>\n<i>{} · {}</i>",
        escape(&report.title),
        escape(&report.label),
        escape(pre.trim_end()),
        escape(&report.footer),
        escape(&report.updated)
    )
}

fn merge(into: &mut Tally, t: &Tally) {
    into.trades += t.trades;
    into.wins += t.wins;
    into.profit += t.profit;
    into.volume += t.volume;
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
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
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
        let row = |name: &str, sub: &str, kind, tally| table::Row { name: name.into(), sub: sub.into(), kind, tally, currency: "USDT".into() };
        let report = table::Report {
            title: "Month".into(),
            label: "September 2026".into(),
            updated: "updated 13:08:25 UTC".into(),
            footer: "Closed trades by close time, UTC".into(),
            rows: vec![
                row("Bin9", "ByBit Futures", table::Kind::Core, t(95, 67, 1087.85, 271_962.5)),
                row("Bin9", "emulator", table::Kind::Emulator, t(2, 1, -376.01, 7_915.9)),
                row("test<1>", "", table::Kind::Core, None),
                row("TOTAL", "", table::Kind::Total, t(97, 68, 711.84, 279_878.4)),
            ],
        };
        let text = report_text(&report);
        let pre = text.split("<pre>").nth(1).and_then(|s| s.split("</pre>").next()).unwrap();
        let pre = pre.replace("&lt;", "<").replace("&gt;", ">");
        let widths: Vec<usize> = pre.lines().map(|l| l.chars().count()).collect();
        assert!(widths.iter().all(|&w| w == widths[0]), "{widths:?}");
    }
}
