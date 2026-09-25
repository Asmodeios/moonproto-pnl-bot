//! The PnL report drawn as a PNG table: SVG laid out here, rasterized by
//! resvg with JetBrains Mono built in, so it looks the same on any server.
//!
//! The font is monospace, so a cell's width is its char count times one
//! advance — no text measuring.

use crate::pnl::Tally;

const REGULAR: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf");
const BOLD: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Bold.ttf");
/// JetBrains Mono's advance, in em.
const ADVANCE: f64 = 0.6;
/// Rendered at twice the layout size; Telegram scales photos down anyway.
const SCALE: f32 = 2.0;

const BODY: f64 = 20.0;
const SUB: f64 = 14.0;
const HEAD: f64 = 15.0;
const HEAD_SPACING: f64 = 2.0;
const PAD: f64 = 22.0;
const GAP: f64 = 34.0;
const TITLE_H: f64 = 66.0;
const HEAD_H: f64 = 48.0;
const ROW_H: f64 = 62.0;
const FOOT_H: f64 = 44.0;

const BG: &str = "#0f1115";
const HEAD_BG: &str = "#23272e";
const TOTAL_BG: &str = "#171a20";
const RULE: &str = "#262a31";
const HEAD_RULE: &str = "#4a4f58";
const TEXT: &str = "#e8eaed";
const NUM: &str = "#d2d5da";
const DIM: &str = "#8a8f98";
const EMU: &str = "#d4a24c";
const GREEN: &str = "#5fb96b";
const RED: &str = "#e06c6c";

pub enum Kind {
    Core,
    Emulator,
    Total,
}

pub struct Row {
    pub name: String,
    /// The dim line under the name.
    pub sub: String,
    pub kind: Kind,
    /// `None` shows dashes: no history yet, or it could not be read.
    pub tally: Option<Tally>,
    pub currency: String,
}

pub struct Report {
    pub title: String,
    pub label: String,
    pub updated: String,
    pub footer: String,
    pub rows: Vec<Row>,
}

pub fn render(report: &Report) -> Result<Vec<u8>, String> {
    let svg = svg(report);
    let mut opt = resvg::usvg::Options::default();
    let db = opt.fontdb_mut();
    db.load_font_data(REGULAR.to_vec());
    db.load_font_data(BOLD.to_vec());
    let tree = resvg::usvg::Tree::from_str(&svg, &opt).map_err(|e| format!("report image: {e}"))?;
    let size = tree.size().to_int_size().scale_by(SCALE).ok_or("report image: bad size")?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height()).ok_or("report image: bad size")?;
    resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(SCALE, SCALE), &mut pixmap.as_mut());
    pixmap.encode_png().map_err(|e| format!("report image: {e}"))
}

pub struct Cells {
    pub orders: String,
    pub wl: String,
    volume: String,
    avg: String,
    pub profit: String,
    pub pct: String,
    sign: f64,
}

pub fn cells(row: &Row) -> Cells {
    let dash = || "—".to_string();
    let Some(t) = row.tally else {
        return Cells { orders: dash(), wl: dash(), volume: dash(), avg: dash(), profit: dash(), pct: dash(), sign: 0.0 };
    };
    let unit = unit(&row.currency);
    let avg = if t.trades > 0 { t.volume / f64::from(t.trades) } else { 0.0 };
    Cells {
        orders: t.trades.to_string(),
        wl: format!("{} / {}", t.wins, t.losses()),
        volume: format!("{}{unit}", compact(t.volume)),
        avg: if t.trades > 0 { format!("{}{unit}", compact(avg)) } else { dash() },
        profit: format!("{}{unit}", signed(t.profit)),
        pct: t.pct().map(|p| format!("{}%", signed(p))).unwrap_or_else(dash),
        sign: if t.profit.abs() < 0.005 { 0.0 } else { t.profit },
    }
}

fn svg(report: &Report) -> String {
    let heads = ["CORE", "ORDERS", "W/L", "VOLUME", "AVG ORDER", "PROFIT", "%"];
    let rows: Vec<(&Row, Cells)> = report.rows.iter().map(|r| (r, cells(r))).collect();

    let body_w = |s: &str| s.chars().count() as f64 * BODY * ADVANCE;
    let head_w = |s: &str| s.chars().count() as f64 * (HEAD * ADVANCE + HEAD_SPACING);
    let mut widths: Vec<f64> = heads.iter().map(|h| head_w(h)).collect();
    for (row, c) in &rows {
        let name = body_w(&row.name).max(row.sub.chars().count() as f64 * SUB * ADVANCE);
        for (w, v) in widths.iter_mut().zip([name, body_w(&c.orders), body_w(&c.wl), body_w(&c.volume), body_w(&c.avg), body_w(&c.profit), body_w(&c.pct)]) {
            *w = w.max(v);
        }
    }
    widths[0] += GAP;
    let table_w = PAD * 2.0 + widths.iter().sum::<f64>() + GAP * (widths.len() - 1) as f64;
    let title_w = PAD * 2.0
        + (report.title.chars().count() + report.label.chars().count() + 3) as f64 * 24.0 * ADVANCE
        + GAP
        + report.updated.chars().count() as f64 * SUB * ADVANCE;
    let width = table_w.max(title_w).max(PAD * 2.0 + report.footer.chars().count() as f64 * SUB * ADVANCE).ceil();
    // Column i's anchor x: the left edge for CORE, the right edge for the rest.
    let mut xs = Vec::new();
    let mut x = PAD;
    for (i, w) in widths.iter().enumerate() {
        xs.push(if i == 0 { x } else { x + w });
        x += w + GAP;
    }
    let slack = width - table_w;
    for x in xs.iter_mut().skip(1) {
        *x += slack;
    }

    let height = TITLE_H + HEAD_H + ROW_H * rows.len() as f64 + FOOT_H;
    let mut s = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="JetBrains Mono">"#
    );
    s += &format!(r#"<rect width="{width}" height="{height}" fill="{BG}"/>"#);

    let title_y = TITLE_H / 2.0 + 8.0;
    s += &format!(
        r#"<text x="{PAD}" y="{title_y}" font-size="24" fill="{TEXT}"><tspan font-weight="700">{}</tspan><tspan fill="{DIM}"> — {}</tspan></text>"#,
        esc(&report.title),
        esc(&report.label)
    );
    s += &text(width - PAD, title_y, SUB, DIM, "end", false, &report.updated);

    let top = TITLE_H;
    s += &format!(r#"<rect y="{top}" width="{width}" height="{HEAD_H}" fill="{HEAD_BG}"/>"#);
    s += &format!(r#"<rect y="{}" width="{width}" height="2" fill="{HEAD_RULE}"/>"#, top + HEAD_H - 2.0);
    for (i, h) in heads.iter().enumerate() {
        let anchor = if i == 0 { "start" } else { "end" };
        s += &format!(
            r#"<text x="{}" y="{}" font-size="{HEAD}" font-weight="700" letter-spacing="{HEAD_SPACING}" fill="{TEXT}" text-anchor="{anchor}">{h}</text>"#,
            // The spacing trails the last letter; end-anchored heads shift by it.
            if i == 0 { xs[i] } else { xs[i] + HEAD_SPACING },
            top + HEAD_H / 2.0 + HEAD * 0.35
        );
    }

    let mut y = top + HEAD_H;
    for (row, c) in &rows {
        let total = matches!(row.kind, Kind::Total);
        if total {
            s += &format!(r#"<rect y="{y}" width="{width}" height="{ROW_H}" fill="{TOTAL_BG}"/>"#);
            s += &format!(r#"<rect y="{y}" width="{width}" height="2" fill="{HEAD_RULE}"/>"#);
        }
        let mid = y + ROW_H / 2.0;
        let (name_y, sub_y) = if row.sub.is_empty() { (mid + BODY * 0.35, 0.0) } else { (mid - 3.0, mid + 18.0) };
        s += &text(xs[0], name_y, BODY, TEXT, "start", true, &row.name);
        if !row.sub.is_empty() {
            let color = if matches!(row.kind, Kind::Emulator) { EMU } else { DIM };
            s += &text(xs[0], sub_y, SUB, color, "start", false, &row.sub);
        }
        let money = if c.sign > 0.0 {
            GREEN
        } else if c.sign < 0.0 {
            RED
        } else {
            NUM
        };
        let num_y = mid + BODY * 0.35;
        for (i, (v, color, bold)) in [
            (&c.orders, NUM, total),
            (&c.wl, DIM, false),
            (&c.volume, NUM, total),
            (&c.avg, NUM, false),
            (&c.profit, money, true),
            (&c.pct, money, false),
        ]
        .into_iter()
        .enumerate()
        {
            s += &text(xs[i + 1], num_y, BODY, color, "end", bold, v);
        }
        y += ROW_H;
        s += &format!(r#"<rect y="{}" width="{width}" height="1" fill="{RULE}"/>"#, y - 1.0);
    }

    s += &text(PAD, y + FOOT_H / 2.0 + SUB * 0.35, SUB, DIM, "start", false, &report.footer);
    s += "</svg>";
    s
}

fn text(x: f64, y: f64, size: f64, fill: &str, anchor: &str, bold: bool, body: &str) -> String {
    let weight = if bold { 700 } else { 400 };
    format!(
        r#"<text x="{x}" y="{y}" font-size="{size}" font-weight="{weight}" fill="{fill}" text-anchor="{anchor}">{}</text>"#,
        esc(body)
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `$` for dollars and their stablecoins, the code otherwise.
fn unit(currency: &str) -> String {
    match currency.to_ascii_uppercase().as_str() {
        "" | "USD" | "USDT" | "USDC" | "BUSD" | "FDUSD" | "TUSD" => "$".to_string(),
        other => format!(" {other}"),
    }
}

/// `186k`, `3.87k`, `1.24M`: three significant-ish digits.
fn compact(v: f64) -> String {
    let a = v.abs();
    if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if a >= 1e3 {
        format!("{:.2}k", v / 1e3)
    } else if a >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

/// Two decimals with the sign, and no `-0.00`.
fn signed(v: f64) -> String {
    let v = if v.abs() < 0.005 { 0.0 } else { v };
    format!("{v:+.2}")
}


#[cfg(test)]
mod tests {
    use super::*;

    /// `PNL_PREVIEW=<file.png>` also saves the image, to look at.
    #[test]
    fn renders_png() {
        let t = |trades, wins, profit, volume| Some(Tally { trades, wins, profit, volume });
        let row = |name: &str, sub: &str, kind, tally| Row { name: name.into(), sub: sub.into(), kind, tally, currency: "USDT".into() };
        let report = Report {
            title: "Month".into(),
            label: "September 2026".into(),
            updated: "updated 13:08:25 UTC".into(),
            footer: "Closed trades by close time, UTC".into(),
            rows: vec![
                row("Bin9", "ByBit Futures", Kind::Core, t(95, 67, 1087.85, 271_962.5)),
                row("Bin9", "emulator · ByBit Futures", Kind::Emulator, t(2, 1, -376.01, 7_915.9)),
                row("Bin10", "ByBit Futures", Kind::Core, t(991, 231, 1041.67, 90_580.0)),
                row("Bin11", "", Kind::Core, None),
                row("TOTAL", "", Kind::Total, t(1086, 298, 2129.52, 362_542.5)),
                row("TOTAL", "emulator", Kind::Total, t(2, 1, -376.01, 7_915.9)),
            ],
        };
        let png = render(&report).unwrap();
        assert_eq!(&png[..4], b"\x89PNG");
        if let Ok(path) = std::env::var("PNL_PREVIEW") {
            std::fs::write(path, &png).unwrap();
        }
    }
}
