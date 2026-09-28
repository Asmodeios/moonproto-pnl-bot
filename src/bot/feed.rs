//! The Live trades feed: the trades the cores close, sent to the owner.
//! Closes are collected for a short window from the first one, across all
//! cores, and go out together — a burst of closes is one message, not one
//! each, well inside Telegram's per-chat limits.

use std::time::Duration;

use super::Bot;
use crate::replica::ClosedTrade;
use crate::store::CoreEntry;
use crate::table;
use crate::telegram::escape;

/// A live close older than this — one that happened while the link was
/// down, reaching the bot on reconnect — is left to the reports.
const LIVE_TRADE_MAX_AGE_MS: i64 = 10 * 60_000;
/// How long the closes after the first are collected before sending.
const LIVE_WINDOW: Duration = Duration::from_secs(2);
/// Room under Telegram's 4096-character message limit; a longer batch is
/// split between trades.
const MESSAGE_BUDGET: usize = 4000;

impl Bot {
    /// Send the owner the trades the cores close, while Live trades is on.
    pub fn live_trades(&self, mut closed: tokio::sync::mpsc::UnboundedReceiver<ClosedTrade>) {
        let me = self.me.clone();
        tokio::spawn(async move {
            loop {
                // Wait as long as it takes for the first close, then only
                // until the window ends.
                let Some(first) = closed.recv().await else {
                    return;
                };
                let end = tokio::time::Instant::now() + LIVE_WINDOW;
                let mut trades = vec![first];
                while let Ok(Some(trade)) = tokio::time::timeout_at(end, closed.recv()).await {
                    trades.push(trade);
                }
                let Some(bot) = me.upgrade() else {
                    return;
                };
                let batch: Vec<String> = trades.iter().filter_map(|t| bot.live_text(t)).collect();
                if !batch.is_empty() {
                    bot.send_live(batch).await;
                }
            }
        });
    }

    /// The trade's text, or `None` when the feed leaves it out.
    fn live_text(&self, trade: &ClosedTrade) -> Option<String> {
        let settings = self.store.settings();
        if !settings.live_trades || (trade.emulator && !settings.show_emulator) {
            return None;
        }
        let now = chrono::Utc::now().timestamp_millis() + self.offset_min * 60_000;
        if now - trade.close_ms > LIVE_TRADE_MAX_AGE_MS {
            return None;
        }
        let core = self.store.get(&trade.core_id)?;
        Some(trade_text(trade, &core))
    }

    async fn send_live(&self, batch: Vec<String>) {
        let Some(owner) = self.store.owner() else {
            return;
        };
        for message in messages(batch) {
            if let Err(e) = self.tg.send(owner, &message, None).await {
                log::warn!("live trades: {e}");
            }
        }
    }
}

/// The trades' texts joined into as few messages as fit the budget.
fn messages(texts: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for text in texts {
        match out.last_mut() {
            Some(last) if last.chars().count() + 2 + text.chars().count() <= MESSAGE_BUDGET => {
                last.push_str("\n\n");
                last.push_str(&text);
            }
            _ => out.push(text),
        }
    }
    out
}

/// A closed trade as the Live trades feed sends it.
fn trade_text(t: &ClosedTrade, core: &CoreEntry) -> String {
    let unit = table::unit(&core.currency);
    let icon = if t.profit >= 0.0 { "🟢" } else { "🔴" };
    let coin = if t.coin.is_empty() { "?" } else { &t.coin };
    let side = if t.short { "SHORT" } else { "LONG" };
    let emu = if t.emulator { " · 🧪 emulator" } else { "" };
    let pct = if t.spent != 0.0 { format!(" ({}%)", table::signed(t.profit / t.spent * 100.0)) } else { String::new() };
    let mut text = format!(
        "{icon} <b>{}</b> {side} · {}{emu}\n<b>{}{unit}</b>{pct}",
        escape(coin),
        escape(&core.name),
        table::signed(t.profit),
    );
    let mut last = Vec::new();
    if t.buy_ms > 0 && t.close_ms >= t.buy_ms {
        last.push(duration(t.close_ms - t.buy_ms));
    }
    if !t.sell_reason.is_empty() {
        last.push(escape(&t.sell_reason));
    }
    if !last.is_empty() {
        text.push('\n');
        text.push_str(&last.join(" · "));
    }
    // A liquidation's channel names no strategy; its signal type does.
    let liquidation = t.channel.to_ascii_uppercase().contains("LIQUIDATION");
    let source = if liquidation && !t.signal_type.is_empty() { &t.signal_type } else { &t.channel };
    if !source.is_empty() {
        let source: String = source.chars().take(120).collect();
        text.push('\n');
        text.push_str(&escape(&source));
    }
    text
}

/// `42s`, `3m 05s`, `2h 10m`, `1d 4h`.
fn duration(ms: i64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    } else {
        format!("{}d {}h", s / 86_400, s % 86_400 / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_trade_text() {
        let core = CoreEntry {
            id: "c1".into(),
            name: "Bybit <1>".into(),
            key: String::new(),
            sealed_key: String::new(),
            host: String::new(),
            port: 0,
            currency: "USDT".into(),
        };
        let trade = ClosedTrade {
            core_id: "c1".into(),
            coin: "FOLKS".into(),
            profit: -183.794,
            spent: 3961.018,
            emulator: false,
            short: false,
            buy_ms: 1_790_129_615_487,
            close_ms: 1_790_129_616_705,
            sell_reason: "StopLoss Market Sell".into(),
            channel: "MoonShot: (strategy <MR FOLKS 5.86% LONG>)".into(),
            signal_type: String::new(),
        };
        assert_eq!(
            trade_text(&trade, &core),
            "🔴 <b>FOLKS</b> LONG · Bybit &lt;1&gt;\n<b>-183.79$</b> (-4.64%)\n\
             1s · StopLoss Market Sell\nMoonShot: (strategy &lt;MR FOLKS 5.86% LONG&gt;)"
        );
        let one = trade_text(&trade, &core);
        let liquidated = ClosedTrade {
            channel: "LIQUIDATION".into(),
            signal_type: "MR FOLKS 5.86% LONG".into(),
            ..trade
        };
        assert!(trade_text(&liquidated, &core).ends_with("\nMR FOLKS 5.86% LONG"));

        let joined = messages(vec![one.clone(), one.clone()]);
        assert_eq!(joined, vec![format!("{one}\n\n{one}")]);
        // A batch past the budget splits between trades, never inside one.
        let many = messages(vec![one.clone(); 60]);
        assert!(many.len() > 1);
        assert!(many.iter().all(|m| m.chars().count() <= MESSAGE_BUDGET && m.starts_with("🔴")));
        assert_eq!(many.iter().map(|m| m.matches("FOLKS").count()).sum::<usize>(), 120);
        assert_eq!(duration(185_000), "3m 05s");
        assert_eq!(duration(90_061_000), "1d 1h");
    }
}
