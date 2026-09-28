//! The PnL reports: per period, laid out by core, coin or date, as a table
//! image or as text.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde_json::{json, Value};

use super::{Bot, Follow, Screen};
use crate::pnl::{self, CoreTally, Period, Tally};
use crate::store::{CoreEntry, ReportFormat};
use crate::table::{self, By};
use crate::telegram::{button, escape};

/// Coins a By coin report lists one by one, the biggest profits and losses.
const MAX_COIN_ROWS: usize = 20;
/// Room for the report's notes under Telegram's 1024-character caption limit.
const CAPTION_BUDGET: usize = 900;
/// Each report layout a period comes in, with its button data.
pub(super) const REPORTS: [(Period, By, &str); 7] = [
    (Period::Hour, By::Core, "r:h"),
    (Period::Hour, By::Coin, "r:hk"),
    (Period::Today, By::Core, "r:t"),
    (Period::Today, By::Coin, "r:tk"),
    (Period::Month, By::Core, "r:mc"),
    (Period::Month, By::Coin, "r:mk"),
    (Period::Month, By::Date, "r:md"),
];
/// Before a report's button data: flips the name/profit order, then shows that report.
pub(super) const SORT_PREFIX: &str = "rs:";

impl Bot {
    /// The report for `period` laid out `by` core, coin or date: a table
    /// image with the notes as its caption, or text alone while there's
    /// nothing to draw. Only the month's comes by date.
    pub(super) async fn report_screen(&self, period: Period, by: By) -> Screen {
        let now = chrono::Utc::now().timestamp_millis();
        let bounds = pnl::bounds(period, now, self.offset_min);
        let cores = self.store.all();
        let paths: Vec<_> = cores.iter().map(|c| self.cores.replica_path(&c.id)).collect();
        let (from, to) = (bounds.from, bounds.to);

        let by = if views(period).any(|v| v == by) { by } else { default_view(period) };
        let label = match by {
            By::Core => bounds.label.clone(),
            By::Coin => format!("{} · by coin", bounds.label),
            By::Date => format!("{} · by date", bounds.label),
        };
        let title = match period {
            Period::Hour => "Last hour",
            Period::Today => "Today",
            Period::Month => "Month",
        };
        let mut first = vec![button("🔄 Refresh", report_code(period, by))];
        first.extend(views(period).filter(|&v| v != by).map(|v| view_button(period, v)));
        let settings = self.store.settings();
        let by_name = settings.sort_by_name;
        if by != By::Date {
            let label = if by_name { "💰 Sort by profit" } else { "🔤 Sort by name" };
            first.push(button(label, &format!("{SORT_PREFIX}{}", report_code(period, by))));
        }
        let mut second: Vec<Value> = PERIODS.iter().filter(|&&p| p != period).map(|&p| period_button(p)).collect();
        second.push(button("⬅ Menu", "m"));
        let kb = json!([first, second]);
        if cores.is_empty() {
            let text = format!("<b>{title}</b> — {label}\n\nNo cores yet — add one under 🖥 Cores.");
            return Screen { text, kb, png: None, follow: Follow::No };
        }

        let show_emu = settings.show_emulator;
        let mut notes = Vec::new();
        let rows = match by {
            By::Core => {
                let tallies = per_replica(paths, move |p| pnl::tally(p, from, to)).await;
                self.core_rows(&cores, tallies, show_emu, by_name, &mut notes)
            }
            By::Coin => {
                let coins = per_replica(paths, move |p| pnl::by_coin(p, from, to)).await;
                coin_rows(&cores, coins, show_emu, by_name, &mut notes)
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

    /// A row per core — two with emulator trades — best profit first, or by
    /// name, then the totals when there are several cores.
    fn core_rows(
        &self,
        cores: &[CoreEntry],
        tallies: Vec<Result<Option<CoreTally>, String>>,
        show_emu: bool,
        by_name: bool,
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
        if by_name {
            groups.sort_by_cached_key(|(_, rows)| rows[0].name.to_lowercase());
        } else {
            groups.sort_by(|a, b| b.0.total_cmp(&a.0));
        }
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
    let mut rows: Vec<table::Row> = days
        .into_iter()
        .map(|((currency, day), t)| table::Row {
            name: day.format("%Y-%m-%d").to_string(),
            sub: String::new(),
            kind: table::Kind::Core,
            tally: Some(t),
            currency,
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

/// A row per coin — two with emulator trades — summed over the cores, the
/// biggest profit or loss first, or by name, then the totals. Past
/// `MAX_COIN_ROWS` coins, the rest — the smallest profits and losses either
/// way — are summed into one row so the image stays a size Telegram takes.
fn coin_rows(
    cores: &[CoreEntry],
    coins: Vec<Result<Option<BTreeMap<String, CoreTally>>, String>>,
    show_emu: bool,
    by_name: bool,
    notes: &mut Vec<String>,
) -> Vec<table::Row> {
    // Keyed by currency first: a sum across currencies means nothing.
    let mut sums: BTreeMap<(String, String), CoreTally> = BTreeMap::new();
    for (core, by_coin) in cores.iter().zip(coins) {
        let by_coin = match by_coin {
            Ok(Some(by_coin)) => by_coin,
            Ok(None) | Err(_) => {
                notes.push(missing_note(core, by_coin.err()));
                continue;
            }
        };
        for (coin, mut t) in by_coin {
            if !show_emu {
                t.emulator = Tally::default();
            }
            sums.entry((core.currency.clone(), coin)).or_default().add(&t);
        }
    }
    let mut coins: Vec<((String, String), CoreTally)> =
        sums.into_iter().filter(|(_, t)| t.real.trades > 0 || t.emulator.trades > 0).collect();
    coins.sort_by(|a, b| b.1.real.profit.abs().total_cmp(&a.1.real.profit.abs()));
    let mut real_total: BTreeMap<String, Tally> = BTreeMap::new();
    let mut emu_total: BTreeMap<String, Tally> = BTreeMap::new();
    for ((cur, _), t) in &coins {
        if t.real.trades > 0 {
            real_total.entry(cur.clone()).or_default().merge(&t.real);
        }
        if t.emulator.trades > 0 {
            emu_total.entry(cur.clone()).or_default().merge(&t.emulator);
        }
    }
    let rows_of = |name: String, currency: &String, t: &CoreTally| {
        let row = |sub: &str, kind, tally| table::Row {
            name: name.clone(),
            sub: sub.to_string(),
            kind,
            tally: Some(tally),
            currency: currency.clone(),
        };
        let mut rows = Vec::new();
        if t.real.trades > 0 || t.emulator.trades == 0 {
            rows.push(row("", table::Kind::Core, t.real));
        }
        if t.emulator.trades > 0 {
            rows.push(row("emulator", table::Kind::Emulator, t.emulator));
        }
        rows
    };
    let mut rows = Vec::new();
    let cap = coins.len().min(MAX_COIN_ROWS);
    let (shown, rest) = coins.split_at_mut(cap);
    if by_name {
        shown.sort_by(|a, b| a.0 .1.cmp(&b.0 .1));
    }
    for ((cur, coin), t) in shown.iter() {
        rows.extend(rows_of(coin.clone(), cur, t));
    }
    let mut others: BTreeMap<String, (usize, CoreTally)> = BTreeMap::new();
    for ((cur, _), t) in rest.iter() {
        let (n, sum) = others.entry(cur.clone()).or_default();
        *n += 1;
        sum.add(t);
    }
    for (cur, (n, t)) in &others {
        rows.extend(rows_of(format!("{n} others"), cur, t));
    }
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

/// The report as a message: the image's rows as a monospace table. Volume,
/// average order and the exchange are left out to fit a phone's width, and
/// a day shows without its year — the title has it.
fn report_text(report: &table::Report) -> String {
    let by_date = report.by == By::Date;
    let head: Vec<String> = [report.by.name(), "O/W/L", "Profit"].map(String::from).to_vec();
    let mut lines: Vec<Vec<String>> = vec![head];
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
        // Orders, wins and losses in one column; a core without figures shows one dash.
        let owl = if c.orders == c.wl { c.orders } else { format!("{}/{}", c.orders, c.wl.replace(" / ", "/")) };
        lines.push(vec![name, owl, c.profit]);
        if !matches!(row.kind, table::Kind::Total) {
            body = lines.len();
        }
    }
    let mut w = vec![0usize; lines[0].len()];
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

/// The layouts `period`'s report comes in.
fn views(period: Period) -> impl Iterator<Item = By> {
    REPORTS.into_iter().filter(move |r| r.0 == period).map(|r| r.1)
}

/// The button data that opens `period`'s report laid out `by`.
fn report_code(period: Period, by: By) -> &'static str {
    REPORTS.iter().find(|r| r.0 == period && r.1 == by).map_or("r:t", |r| r.2)
}

/// The periods the menu offers, in button order.
pub(super) const PERIODS: [Period; 3] = [Period::Hour, Period::Today, Period::Month];

/// The layout `period`'s report opens in: by core for the hour and today, by
/// date for the month.
pub(super) fn default_view(period: Period) -> By {
    match period {
        Period::Hour | Period::Today => By::Core,
        Period::Month => By::Date,
    }
}

/// A button that opens `period`'s report in its default layout.
pub(super) fn period_button(period: Period) -> Value {
    let label = match period {
        Period::Hour => "⏱ Hour",
        Period::Today => "📊 Today",
        Period::Month => "📅 Month",
    };
    button(label, report_code(period, default_view(period)))
}

/// A button that opens `period`'s report laid out `by`.
fn view_button(period: Period, by: By) -> Value {
    let label = match by {
        By::Core => "🖥 By core",
        By::Coin => "🪙 By coin",
        By::Date => "📆 By date",
    };
    button(label, report_code(period, by))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coin_rows_sum_cores_and_cap() {
        let core = |id: &str| CoreEntry {
            id: id.into(),
            name: id.into(),
            key: String::new(),
            sealed_key: String::new(),
            host: String::new(),
            port: 0,
            currency: "USDT".into(),
        };
        let real = |profit| CoreTally { real: Tally { trades: 1, wins: 1, profit, volume: 10.0 }, ..Default::default() };
        // Profits -15 to +14; C1 loses 100 more on core b.
        let a: BTreeMap<String, CoreTally> = (0..30).map(|i| (format!("C{i}"), real(f64::from(i - 15)))).collect();
        let b: BTreeMap<String, CoreTally> = [("C1".to_string(), real(-100.0))].into();
        let cores = [core("a"), core("b"), core("c")];
        let mut notes = Vec::new();
        let rows = coin_rows(&cores, vec![Ok(Some(a)), Ok(Some(b)), Ok(None)], true, false, &mut notes);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        // The biggest profits and losses first, the other 10 in one row, the total.
        assert_eq!(names.len(), MAX_COIN_ROWS + 2);
        assert_eq!(&names[..3], ["C1", "C0", "C29"]);
        assert_eq!(names[MAX_COIN_ROWS], "10 others");
        assert_eq!(rows[0].tally.unwrap().trades, 2);
        assert_eq!(rows[MAX_COIN_ROWS].tally.unwrap().trades, 10);
        assert_eq!(names.last(), Some(&"TOTAL"));
        assert_eq!(notes.len(), 1);

        // By name: the same coins, alphabetical, the others and the total still last.
        let a: BTreeMap<String, CoreTally> = (0..30).map(|i| (format!("C{i}"), real(f64::from(i - 15)))).collect();
        let rows = coin_rows(&cores[..1], vec![Ok(Some(a))], true, true, &mut Vec::new());
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        let shown = &names[..MAX_COIN_ROWS];
        assert!(shown.windows(2).all(|w| w[0] < w[1]), "{shown:?}");
        assert!(shown.contains(&"C0") && !shown.contains(&"C15"), "{shown:?}");
        assert_eq!(&names[MAX_COIN_ROWS..], ["10 others", "TOTAL"]);
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
        };
        let aligned = |report: &table::Report| {
            let text = report_text(report);
            let pre = text.split("<pre>").nth(1).and_then(|s| s.split("</pre>").next()).unwrap().to_string();
            let pre = pre.replace("&lt;", "<").replace("&gt;", ">");
            let widths: Vec<usize> = pre.lines().map(|l| l.chars().count()).collect();
            assert!(widths.iter().all(|&w| w == widths[0]), "{widths:?}");
            pre
        };
        let day = |name: &str, tally| row(name, "", table::Kind::Core, tally);
        let by_date = table::Report {
            title: "Month".into(),
            label: "September 2026 · by date".into(),
            updated: "updated 13:08:25 UTC".into(),
            footer: "Closed trades by close time, UTC".into(),
            by: By::Date,
            rows: vec![
                day("2026-09-24", t(7, 5, 85.82, 44_000.0)),
                day("2026-09-23", t(26, 15, -225.02, 112_000.0)),
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
