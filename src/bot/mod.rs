//! Updates in, screens out. Every screen is one message edited in place by
//! its inline buttons; adding a core is a short conversation (name, key,
//! and the endpoint when the key carries none) held in `pending`, or one
//! message listing several cores at once (`parse_batch_line`). The Cores
//! screen last shown follows its cores while one is still on its way.

mod feed;
mod flow;
mod live;
mod report;
mod screens;
mod unlock;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cores::Cores;
use crate::pnl::Period;
use crate::status::Status;
use crate::store::{ReportFormat, Store};
use crate::telegram::{button, escape, CallbackQuery, Message, Tg, Update};

use flow::Pending;
use live::Live;
use report::{default_view, REPORTS, SORT_PREFIX};
use unlock::{locked_kb, LOCKED};

/// The note for a core deleted while its screen was open.
const GONE: &str = "That core is already gone.";

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
            ("hour", "PnL for the last hour"),
            ("today", "PnL today"),
            ("month", "PnL this month"),
            ("cores", "Manage cores"),
            ("settings", "Settings"),
            ("cancel", "Cancel adding or renaming a core"),
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
                "hour" => self.report_screen(Period::Hour, default_view(Period::Hour)).await,
                "today" => self.report_screen(Period::Today, default_view(Period::Today)).await,
                "month" => self.report_screen(Period::Month, default_view(Period::Month)).await,
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
            _ if let Some(&(period, by, _)) = REPORTS.iter().find(|r| r.2 == data) => {
                self.report_screen(period, by).await
            }
            // The sort toggle, then the report it was pressed on.
            _ if let Some(&(period, by, _)) =
                data.strip_prefix(SORT_PREFIX).and_then(|d| REPORTS.iter().find(|r| r.2 == d)) =>
            {
                self.store.edit_settings(|s| s.sort_by_name = !s.sort_by_name)?;
                self.report_screen(period, by).await
            }
            // The old Month picker's button, still on messages sent before it went.
            "r:m" => self.report_screen(Period::Month, default_view(Period::Month)).await,
            "m" => self.main_screen().into(),
            _ if let Some(page) = data.strip_prefix("c:p:") => {
                self.cores_page.store(page.parse().unwrap_or(0), Ordering::Relaxed);
                self.cores_screen(None)
            }
            "c" => self.cores_screen(None),
            "x" => self.cores_screen(Some("Cancelled.".to_string())),
            "c:add" | "c:batch" => {
                let (pending, text) = if data == "c:add" {
                    (Pending::Name, "Send a name for the new core, e.g. <i>Binance 1</i>.")
                } else {
                    (
                        Pending::Batch,
                        "Send your cores in one message, one per line — a name, then the core's MoonProto key:\n\n\
                         <pre>Binance 1  KEY\nBinance 2: KEY\nKEY\nBinance 4  KEY  203.0.113.5:4545</pre>\n\n\
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
                self.store.edit_settings(|s| s.show_emulator = !s.show_emulator)?;
                self.settings_screen().into()
            }
            "s:live" => {
                self.store.edit_settings(|s| s.live_trades = !s.live_trades)?;
                self.settings_screen().into()
            }
            "s:fmt" => {
                self.store.edit_settings(|s| {
                    s.report_format = match s.report_format {
                        ReportFormat::Image => ReportFormat::Text,
                        ReportFormat::Text => ReportFormat::Image,
                    }
                })?;
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
                None => self.cores_screen(Some(GONE.to_string())),
            },
            _ if let Some(id) = data.strip_prefix("c:ren:") => match self.store.get(id) {
                Some(core) => {
                    self.set_pending(Some(Pending::Rename { id: core.id.clone() }));
                    self.follow(msg.chat.id, msg.message_id);
                    let text = format!("Send a new name for <b>{}</b>.", escape(&core.name));
                    (text, cancel_kb()).into()
                }
                None => self.cores_screen(Some(GONE.to_string())),
            },
            _ if let Some(id) = data.strip_prefix("c:rm:") => match self.store.get(id) {
                Some(core) => {
                    self.cores.remove(&core.id);
                    self.store.remove(&core.id)?;
                    log::info!("deleted core {}", core.id);
                    self.cores_screen(Some(format!("🗑 <b>{}</b> deleted.", escape(&core.name))))
                }
                None => self.cores_screen(Some(GONE.to_string())),
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
}
