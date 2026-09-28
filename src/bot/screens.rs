//! The menu, Settings and Cores screens.

use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use super::report::{period_button, PERIODS};
use super::{Bot, Follow, Screen};
use crate::cores::{self, CoreView, Link};
use crate::replica::Phase;
use crate::store::ReportFormat;
use crate::telegram::{button, escape};

/// Cores per page of the Cores screen — its table rows and ✏️/🗑 buttons.
const CORES_PER_PAGE: usize = 7;

impl Bot {
    pub(super) async fn show_cores(&self, chat: i64, note: Option<String>) -> Result<(), String> {
        self.show(chat, None, self.cores_screen(note)).await
    }

    pub(super) fn main_screen(&self) -> (String, Value) {
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
        let periods: Vec<Value> = PERIODS.iter().map(|&p| period_button(p)).collect();
        let kb = json!([periods, [button("🖥 Cores", "c"), button("⚙️ Settings", "s")]]);
        (format!("<b>MoonBot PnL</b>\n\n{body}"), kb)
    }

    pub(super) fn settings_screen(&self) -> (String, Value) {
        let settings = self.store.settings();
        let show = settings.show_emulator;
        let image = settings.report_format == ReportFormat::Image;
        let live = settings.live_trades;
        let text = format!(
            "<b>⚙️ Settings</b>\n\n🧪 Emulator trades: <b>{}</b> in reports\n📄 Reports come as: <b>{}</b>\n\
             🔔 Live trades: <b>{}</b>",
            if show { "shown" } else { "hidden" },
            if image { "an image" } else { "text" },
            if live { "on — a message for each closed trade" } else { "off" }
        );
        let emu = if show { "🙈 Hide emulator trades" } else { "🧪 Show emulator trades" };
        let format = if image { "📝 Send reports as text" } else { "🖼 Send reports as an image" };
        let live = if live { "🔕 Turn live trades off" } else { "🔔 Turn live trades on" };
        let kb = json!([
            [button(emu, "s:emu")],
            [button(format, "s:fmt")],
            [button(live, "s:live")],
            [button("⬅ Menu", "m")]
        ]);
        (text, kb)
    }

    pub(super) fn cores_screen(&self, note: Option<String>) -> Screen {
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
            let ip = cores::host(core).map_or_else(|| "—".to_string(), |h| mask_host(&h));
            table.push((core.name.clone(), exchange, ip, state.icon));
            if let Some(s) = state.note {
                notes.push_str(&format!("{} <b>{}</b>: <i>{s}</i>\n", state.icon, escape(&core.name)));
            }
            rows.push(json!([
                button(&format!("✏️ {}", core.name), &format!("c:ren:{}", core.id)),
                button(&format!("🗑 {}", core.name), &format!("c:del:{}", core.id)),
            ]));
        }
        if !table.is_empty() {
            // The dot goes last: emoji width in Telegram's monospace varies by client.
            let (core_h, exch_h, ip_h) = ("Core", "Exchange", "IP");
            let name_w = width(core_h, table.iter().map(|(n, ..)| n));
            let exch_w = width(exch_h, table.iter().map(|(_, e, ..)| e));
            let ip_w = width(ip_h, table.iter().map(|(_, _, ip, _)| ip));
            let mut pre = format!("{core_h:name_w$}  {exch_h:exch_w$}  {ip_h:ip_w$}\n");
            for (name, exchange, ip, icon) in &table {
                pre.push_str(&format!("{name:name_w$}  {exchange:exch_w$}  {ip:ip_w$}  {icon}\n"));
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
}

/// A core as the Cores screen shows it.
pub(super) struct CoreState {
    icon: &'static str,
    /// Why its figures may be incomplete.
    note: Option<String>,
    /// Still on its way: connecting, syncing or reconnecting.
    pub(super) settling: bool,
}

pub(super) fn core_state(view: Option<&CoreView>) -> CoreState {
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

/// A monospace column's width: its widest cell, or its heading.
fn width<'a>(head: &str, cells: impl Iterator<Item = &'a String>) -> usize {
    cells.map(|c| c.chars().count()).fold(head.chars().count(), usize::max)
}

/// A host with its second half hidden: `203.0.*.*`, `2001:db8:*`,
/// `core.exa…` — enough to tell cores apart without giving the address away.
fn mask_host(host: &str) -> String {
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => {
            let [a, b, ..] = ip.octets();
            format!("{a}.{b}.*.*")
        }
        Ok(std::net::IpAddr::V6(ip)) => {
            let [a, b, ..] = ip.segments();
            format!("{a:x}:{b:x}:*")
        }
        Err(_) => {
            let keep = host.chars().count().div_ceil(2);
            format!("{}…", host.chars().take(keep).collect::<String>())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_are_half_masked() {
        assert_eq!(mask_host("203.0.113.5"), "203.0.*.*");
        assert_eq!(mask_host("2001:db8::1"), "2001:db8:*");
        assert_eq!(mask_host("core.example.com"), "core.exa…");
    }
}
