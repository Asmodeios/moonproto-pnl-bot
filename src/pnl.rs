//! Realized PnL over a period, read off a core's report replica.
//!
//! A trade counts in the period its close date falls in; open trades are left
//! out. Report dates are the core's own wall clock encoded as epoch ms, so the
//! bounds are taken in that clock too (`REPORT_UTC_OFFSET_MINUTES`). A win is a
//! close at or above zero; profit % is Σ profit / Σ spent.

use std::collections::HashSet;
use std::path::Path;

use chrono::{DateTime, Datelike, NaiveDate};
use rusqlite::Connection;

use crate::replica::{quote_ident, COL_CLOSE_DATE, COL_CLOSE_DATE_MS, COL_DELETED, REPORT_TABLE};

const COL_PROFIT: &str = "ProfitBTC";
const COL_SPENT: &str = "SpentBTC";
const COL_EMULATOR: &str = "Emulator";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Today,
    Month,
}

pub struct Bounds {
    /// Inclusive, report-clock epoch ms.
    pub from: i64,
    /// Exclusive.
    pub to: i64,
    pub label: String,
}

pub fn bounds(period: Period, now_utc_ms: i64, offset_min: i64) -> Bounds {
    let now = DateTime::from_timestamp_millis(now_utc_ms + offset_min * 60_000)
        .unwrap_or_default()
        .date_naive();
    let (from, to, label) = match period {
        Period::Today => (now, now.succ_opt().unwrap_or(now), now.format("%d.%m.%Y").to_string()),
        Period::Month => {
            let first = now.with_day(1).unwrap_or(now);
            let next = if now.month() == 12 {
                NaiveDate::from_ymd_opt(now.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(now.year(), now.month() + 1, 1)
            }
            .unwrap_or(now);
            (first, next, now.format("%B %Y").to_string())
        }
    };
    let ms = |d: NaiveDate| d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc().timestamp_millis();
    Bounds { from: ms(from), to: ms(to), label }
}

#[derive(Clone, Copy, Default)]
pub struct Tally {
    pub trades: u32,
    pub wins: u32,
    pub profit: f64,
    pub volume: f64,
}

impl Tally {
    fn add(&mut self, profit: f64, spent: f64) {
        self.trades += 1;
        if profit >= 0.0 {
            self.wins += 1;
        }
        self.profit += profit;
        self.volume += spent;
    }

    pub fn losses(&self) -> u32 {
        self.trades - self.wins
    }

    pub fn pct(&self) -> Option<f64> {
        (self.volume != 0.0).then(|| self.profit / self.volume * 100.0)
    }
}

#[derive(Default)]
pub struct CoreTally {
    pub real: Tally,
    pub emulator: Tally,
}

/// `None` while the replica holds no report table yet (never synced).
pub fn tally(path: &Path, from: i64, to: i64) -> Result<Option<CoreTally>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open(path).map_err(|e| format!("could not open the replica: {e}"))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let table = quote_ident(REPORT_TABLE);
    let cols: HashSet<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .and_then(|mut stmt| stmt.query_map([], |r| r.get::<_, String>(1))?.collect())
        .map_err(|e| format!("could not read the replica: {e}"))?;
    if cols.is_empty() {
        return Ok(None);
    }
    let col = |name: &str, fallback: &str| {
        if cols.contains(name) {
            format!("COALESCE({}, {fallback})", quote_ident(name))
        } else {
            fallback.to_string()
        }
    };
    if !cols.contains(COL_CLOSE_DATE) {
        return Err("the core's report has no close date".to_string());
    }
    // The ms column where the core has it; rows older than it are NULL there.
    let close = if cols.contains(COL_CLOSE_DATE_MS) {
        format!("COALESCE({}, {} * 1000)", quote_ident(COL_CLOSE_DATE_MS), quote_ident(COL_CLOSE_DATE))
    } else {
        format!("{} * 1000", quote_ident(COL_CLOSE_DATE))
    };
    let sql = format!(
        "SELECT {profit}, {spent}, {emu} FROM {table} WHERE {deleted} = 0 AND {close} >= ?1 AND {close} < ?2",
        profit = col(COL_PROFIT, "0"),
        spent = col(COL_SPENT, "0"),
        emu = col(COL_EMULATOR, "0"),
        deleted = col(COL_DELETED, "0"),
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("could not read the replica: {e}"))?;
    let mut out = CoreTally::default();
    let rows = stmt
        .query_map(rusqlite::params![from, to], |r| {
            Ok((r.get::<_, f64>(0)?, r.get::<_, f64>(1)?, r.get::<_, i64>(2)? != 0))
        })
        .map_err(|e| format!("could not read the replica: {e}"))?;
    for row in rows {
        let (profit, spent, emulator) = row.map_err(|e| format!("could not read the replica: {e}"))?;
        if emulator {
            out.emulator.add(profit, spent);
        } else {
            out.real.add(profit, spent);
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400_000;

    fn utc(y: i32, m: u32, d: u32, h: u32) -> i64 {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, 0, 0).unwrap().and_utc().timestamp_millis()
    }

    #[test]
    fn today_and_month_bounds() {
        let now = utc(2026, 12, 31, 22);
        let t = bounds(Period::Today, now, 0);
        assert_eq!((t.from, t.to), (utc(2026, 12, 31, 0), utc(2026, 12, 31, 0) + DAY));
        let m = bounds(Period::Month, now, 0);
        assert_eq!((m.from, m.to), (utc(2026, 12, 1, 0), utc(2027, 1, 1, 0)));
        // 22:00 UTC is already the next day on a UTC+3 core.
        let t3 = bounds(Period::Today, now, 180);
        assert_eq!(t3.from, utc(2027, 1, 1, 0));
    }

    #[test]
    fn tally_counts_closed_rows_in_range() {
        let dir = std::env::temp_dir().join(format!("pnl-bot-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sqlite3");
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE Orders (newRecID INTEGER PRIMARY KEY, deleted INTEGER, CloseDate INTEGER,
               CloseDateMs INTEGER, ProfitBTC REAL, SpentBTC REAL, Emulator INTEGER);
             INSERT INTO Orders VALUES (1, 0, 1000, 1000500, 5.0, 100.0, 0);
             INSERT INTO Orders VALUES (2, 0, 2000, NULL, -2.0, 100.0, 0);
             INSERT INTO Orders VALUES (3, 1, 2000, 2000000, 50.0, 100.0, 0);
             INSERT INTO Orders VALUES (4, 0, 0, 0, 0.0, 100.0, 0);
             INSERT INTO Orders VALUES (5, 0, 1500, 1500000, 1.0, 10.0, 1);
             INSERT INTO Orders VALUES (6, 0, 9000, 9000000, 7.0, 10.0, 0);",
        )
        .unwrap();
        drop(conn);
        let t = tally(&path, 1_000_000, 3_000_000).unwrap().unwrap();
        assert_eq!((t.real.trades, t.real.wins), (2, 1));
        assert!((t.real.profit - 3.0).abs() < 1e-9);
        assert_eq!(t.emulator.trades, 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
