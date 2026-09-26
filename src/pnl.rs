//! Realized PnL over a period, read off a core's report replica.
//!
//! A trade counts in the period its close date falls in; open trades are left
//! out. Report dates are the core's own wall clock encoded as epoch ms, so the
//! bounds are taken in that clock too (`REPORT_UTC_OFFSET_MINUTES`). A win is a
//! close at or above zero; profit % is Σ profit / Σ spent.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Datelike, Months, NaiveDate};
use rusqlite::types::FromSql;
use rusqlite::Connection;

use crate::replica::{quote_ident, report_columns, COL_CLOSE_DATE, COL_CLOSE_DATE_MS, COL_DELETED, REPORT_TABLE};

const COL_PROFIT: &str = "ProfitBTC";
const COL_SPENT: &str = "SpentBTC";
const COL_EMULATOR: &str = "Emulator";
const COL_COIN: &str = "Coin";

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
            let next = first.checked_add_months(Months::new(1)).unwrap_or(now);
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
    /// Fold another tally into this one.
    pub fn merge(&mut self, other: &Tally) {
        self.trades += other.trades;
        self.wins += other.wins;
        self.profit += other.profit;
        self.volume += other.volume;
    }

    pub fn losses(&self) -> u32 {
        self.trades - self.wins
    }

    pub fn pct(&self) -> Option<f64> {
        (self.volume != 0.0).then(|| self.profit / self.volume * 100.0)
    }
}

#[derive(Clone, Copy, Default)]
pub struct CoreTally {
    pub real: Tally,
    pub emulator: Tally,
}

impl CoreTally {
    fn merge(&mut self, emulator: bool, t: &Tally) {
        if emulator {
            self.emulator.merge(t);
        } else {
            self.real.merge(t);
        }
    }

    /// Fold another core's tally into this one.
    pub fn add(&mut self, other: &CoreTally) {
        self.real.merge(&other.real);
        self.emulator.merge(&other.emulator);
    }
}

const DAY_MS: i64 = 86_400_000;

/// `None` while the replica holds no report table yet (never synced).
pub fn tally(path: &Path, from: i64, to: i64) -> Result<Option<CoreTally>, String> {
    Ok(sums::<i64>(path, from, to, GroupBy::Nothing)?.map(|groups| {
        let mut out = CoreTally::default();
        for (_, emulator, t) in groups {
            out.merge(emulator, &t);
        }
        out
    }))
}

/// [`tally`] split by the report-clock day each trade closed on.
pub fn daily(path: &Path, from: i64, to: i64) -> Result<Option<BTreeMap<NaiveDate, CoreTally>>, String> {
    Ok(sums::<i64>(path, from, to, GroupBy::Day)?.map(|groups| {
        let mut out: BTreeMap<NaiveDate, CoreTally> = BTreeMap::new();
        for (day, emulator, t) in groups {
            let day = DateTime::from_timestamp_millis(day * DAY_MS).unwrap_or_default().date_naive();
            out.entry(day).or_default().merge(emulator, &t);
        }
        out
    }))
}

/// [`tally`] split by the coin traded; `?` for a trade without one.
pub fn by_coin(path: &Path, from: i64, to: i64) -> Result<Option<BTreeMap<String, CoreTally>>, String> {
    Ok(sums(path, from, to, GroupBy::Coin)?.map(|groups| {
        let mut out: BTreeMap<String, CoreTally> = BTreeMap::new();
        for (coin, emulator, t) in groups {
            out.entry(coin).or_default().merge(emulator, &t);
        }
        out
    }))
}

#[derive(Clone, Copy)]
enum GroupBy {
    Nothing,
    Day,
    Coin,
}

/// A group of closed trades: its key (days since the epoch, the coin, or 0
/// when not grouped), whether they are emulator trades, and their sums.
type Group<K> = (K, bool, Tally);

/// The closed trades in `[from, to)` summed per emulator flag and `group`.
fn sums<K: FromSql>(path: &Path, from: i64, to: i64, group: GroupBy) -> Result<Option<Vec<Group<K>>>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open(path).map_err(|e| format!("could not open the replica: {e}"))?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(2));
    let table = quote_ident(REPORT_TABLE);
    let cols: HashSet<String> = report_columns(&conn)
        .map_err(|e| format!("could not read the replica: {e}"))?
        .into_iter()
        .collect();
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
    let close = col(COL_CLOSE_DATE_MS, &format!("{} * 1000", quote_ident(COL_CLOSE_DATE)));
    let profit = col(COL_PROFIT, "0");
    let key = match group {
        GroupBy::Nothing => "0".to_string(),
        GroupBy::Day => format!("CAST({close} AS INTEGER) / {DAY_MS}"),
        GroupBy::Coin => format!("COALESCE(NULLIF(TRIM(CAST({} AS TEXT)), ''), '?')", col(COL_COIN, "''")),
    };
    // The seconds column narrows the scan through its index; a day's margin
    // either side, and the exact bounds on the ms value.
    let sql = format!(
        "SELECT {key}, {emu} != 0, COUNT(*), SUM({profit} >= 0), TOTAL({profit}), TOTAL({spent}) FROM {table}
         WHERE {deleted} = 0 AND {close_s} >= ?3 AND {close_s} < ?4 AND {close} >= ?1 AND {close} < ?2
         GROUP BY 1, 2",
        close_s = quote_ident(COL_CLOSE_DATE),
        spent = col(COL_SPENT, "0"),
        emu = col(COL_EMULATOR, "0"),
        deleted = col(COL_DELETED, "0"),
    );
    let margin_s = DAY_MS / 1000;
    let params = rusqlite::params![from, to, from / 1000 - margin_s, to / 1000 + margin_s];
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("could not read the replica: {e}"))?;
    let groups = stmt
        .query_map(params, |r| {
            let tally = Tally {
                trades: r.get::<_, i64>(2)? as u32,
                wins: r.get::<_, i64>(3)? as u32,
                profit: r.get(4)?,
                volume: r.get(5)?,
            };
            Ok((r.get::<_, K>(0)?, r.get::<_, bool>(1)?, tally))
        })
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("could not read the replica: {e}"))?;
    Ok(Some(groups))
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
               CloseDateMs INTEGER, ProfitBTC REAL, SpentBTC REAL, Emulator INTEGER, Coin TEXT);
             INSERT INTO Orders VALUES (1, 0, 1000, 1000500, 5.0, 100.0, 0, 'BTC');
             INSERT INTO Orders VALUES (2, 0, 2000, NULL, -2.0, 100.0, 0, 'ETH');
             INSERT INTO Orders VALUES (3, 1, 2000, 2000000, 50.0, 100.0, 0, 'BTC');
             INSERT INTO Orders VALUES (4, 0, 0, 0, 0.0, 100.0, 0, 'BTC');
             INSERT INTO Orders VALUES (5, 0, 1500, 1500000, 1.0, 10.0, 1, 'BTC');
             INSERT INTO Orders VALUES (6, 0, 9000, 9000000, 7.0, 10.0, 0, NULL);
             INSERT INTO Orders VALUES (7, 0, 86405, 86405000, 2.0, 20.0, 0, 'BTC');",
        )
        .unwrap();
        drop(conn);
        let t = tally(&path, 1_000_000, 3_000_000).unwrap().unwrap();
        assert_eq!((t.real.trades, t.real.wins), (2, 1));
        assert!((t.real.profit - 3.0).abs() < 1e-9);
        assert_eq!(t.emulator.trades, 1);
        // All three closes in range fall on 1970-01-01.
        let d = daily(&path, 1_000_000, 3_000_000).unwrap().unwrap();
        assert_eq!(d.len(), 1);
        let day = d[&NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()];
        assert_eq!((day.real.trades, day.emulator.trades), (2, 1));
        // Two days: rows 1, 2, 5 and 6 on the first, row 7 on the second.
        let d = daily(&path, 1, 2 * DAY).unwrap().unwrap();
        assert_eq!(d.len(), 2);
        let second = d[&NaiveDate::from_ymd_opt(1970, 1, 2).unwrap()];
        assert_eq!((second.real.trades, second.real.wins), (1, 1));
        assert!((second.real.volume - 20.0).abs() < 1e-9);
        let c = by_coin(&path, 1_000_000, 3_000_000).unwrap().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!((c["BTC"].real.trades, c["BTC"].emulator.trades), (1, 1));
        assert!((c["ETH"].real.profit + 2.0).abs() < 1e-9);
        // Row 6 has no coin.
        let c = by_coin(&path, 1, 2 * DAY).unwrap().unwrap();
        assert_eq!((c["BTC"].real.trades, c["?"].real.trades), (2, 1));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
