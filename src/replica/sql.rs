//! The replica's SQLite side: its connection and checkpoint, report values
//! as SQL, and the report table as the schema lays it out.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use moonproto::{ReportHistoryDepth, ReportRow, ReportSchemaField, ReportSyncCheckpoint, ReportValue};
use rusqlite::types::{ToSqlOutput, ValueRef};
use rusqlite::Connection;

use super::{
    ClosedTrade, COL_CLOSE_DATE, COL_CLOSE_DATE_MS, COL_COIN, COL_EMULATOR, COL_PROFIT, COL_SPENT, REPORT_TABLE,
};

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A stored row's close date in ms, 0 while open, as [`Table::close_ms`]
/// reads a row: the ms column, else the seconds one. `None` with neither.
pub fn close_ms_sql(has_ms: bool, has_secs: bool) -> Option<String> {
    let mut parts = Vec::new();
    if has_ms {
        parts.push(format!("NULLIF({}, 0)", quote_ident(COL_CLOSE_DATE_MS)));
    }
    if has_secs {
        parts.push(format!("{} * 1000", quote_ident(COL_CLOSE_DATE)));
    }
    (!parts.is_empty()).then(|| format!("COALESCE({}, 0)", parts.join(", ")))
}

/// The report table's column names; empty before it exists.
pub fn report_columns(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(REPORT_TABLE)))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    rows.collect()
}

pub(super) fn history_depth_label(depth: ReportHistoryDepth) -> String {
    match depth {
        ReportHistoryDepth::ServerDefault => "serverDefault".to_string(),
        ReportHistoryDepth::All => "all".to_string(),
        ReportHistoryDepth::Days(n) => format!("days:{n}"),
    }
}

fn history_depth_from_label(label: &str) -> ReportHistoryDepth {
    if label == "all" {
        ReportHistoryDepth::All
    } else if let Some(n) = label.strip_prefix("days:").and_then(|s| s.parse::<u16>().ok()) {
        ReportHistoryDepth::Days(n)
    } else {
        ReportHistoryDepth::ServerDefault
    }
}

fn to_sql_value(v: Option<&ReportValue>) -> ToSqlOutput<'_> {
    ToSqlOutput::Borrowed(match v {
        None => ValueRef::Null,
        Some(ReportValue::Integer(i)) => ValueRef::Integer(*i),
        Some(ReportValue::Float(f)) => ValueRef::Real(*f),
        Some(ReportValue::Text(s)) => ValueRef::Text(s.as_bytes()),
    })
}

fn as_i64(v: &ReportValue) -> Option<i64> {
    match v {
        ReportValue::Integer(i) => Some(*i),
        _ => None,
    }
}

pub(super) fn bind_values<'a>(fields: &[ReportSchemaField], row: &'a ReportRow) -> Vec<ToSqlOutput<'a>> {
    fields.iter().map(|f| to_sql_value(row.value(f.index))).collect()
}

pub(super) fn build_upsert_sql(columns: &[String], rec_id_col: &str) -> String {
    let cols = columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    let params = vec!["?"; columns.len()].join(", ");
    let updates = columns
        .iter()
        .filter(|c| c.as_str() != rec_id_col)
        .map(|c| {
            let q = quote_ident(c);
            format!("{q}=excluded.{q}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {table} ({cols}) VALUES ({params}) ON CONFLICT({rec}) DO UPDATE SET {updates}",
        table = quote_ident(REPORT_TABLE),
        rec = quote_ident(rec_id_col),
    )
}

pub(super) fn open_connection(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("could not create reports dir: {e}"))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("could not open replica: {e}"))?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(|e| e.to_string())?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .map_err(|e| e.to_string())?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS _sync_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            epoch INTEGER NOT NULL,
            next_from_rec_id INTEGER NOT NULL,
            history_depth TEXT NOT NULL,
            synced_at INTEGER NOT NULL
        )",
        [],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn)
}

pub(super) fn read_checkpoint(conn: &Connection) -> Option<(ReportSyncCheckpoint, ReportHistoryDepth)> {
    conn.query_row("SELECT epoch, next_from_rec_id, history_depth FROM _sync_meta WHERE id=1", [], |row| {
        let epoch: i64 = row.get(0)?;
        let next: i64 = row.get(1)?;
        let depth: String = row.get(2)?;
        Ok((ReportSyncCheckpoint { epoch: epoch as i32, next_from_rec_id: next }, history_depth_from_label(&depth)))
    })
    .ok()
}

/// The report table as the schema lays it out — none until the first
/// `Schema` event, and again after the database is recreated.
pub(super) struct Table {
    pub(super) fields: Vec<ReportSchemaField>,
    pub(super) rec_id_col: String,
    pub(super) upsert_sql: String,
    pub(super) delete_sql: String,
    pub(super) close_idx: Option<u16>,
    pub(super) close_ms_idx: Option<u16>,
    /// A stored row's close date in ms, 0 while open; `None` without a
    /// close date column.
    pub(super) close_sql: Option<String>,
}

impl Table {
    /// The row's close date in ms; 0 while it is open.
    pub(super) fn close_ms(&self, row: &ReportRow) -> i64 {
        let ms = self.close_ms_idx.and_then(|i| row.value(i)).and_then(as_i64).filter(|ms| *ms != 0);
        let secs = self.close_idx.and_then(|i| row.value(i)).and_then(as_i64);
        ms.or_else(|| secs.map(|s| s.saturating_mul(1000))).unwrap_or(0)
    }

    fn value<'a>(&self, row: &'a ReportRow, name: &str) -> Option<&'a ReportValue> {
        self.fields.iter().find(|f| f.name == name).and_then(|f| row.value(f.index))
    }

    fn text(&self, row: &ReportRow, name: &str) -> String {
        match self.value(row, name) {
            Some(ReportValue::Text(s)) => s.trim().to_string(),
            _ => String::new(),
        }
    }

    fn num(&self, row: &ReportRow, name: &str) -> f64 {
        match self.value(row, name) {
            Some(ReportValue::Float(f)) => *f,
            Some(ReportValue::Integer(i)) => *i as f64,
            _ => 0.0,
        }
    }

    pub(super) fn int(&self, row: &ReportRow, name: &str) -> i64 {
        self.value(row, name).and_then(as_i64).unwrap_or(0)
    }

    pub(super) fn closed_trade(&self, core_id: &str, row: &ReportRow, close_ms: i64) -> ClosedTrade {
        let buy_ms = match self.int(row, "BuyDateMs") {
            0 => self.int(row, "BuyDate").saturating_mul(1000),
            ms => ms,
        };
        ClosedTrade {
            core_id: core_id.to_string(),
            coin: self.text(row, COL_COIN),
            profit: self.num(row, COL_PROFIT),
            spent: self.num(row, COL_SPENT),
            emulator: self.int(row, COL_EMULATOR) != 0,
            short: self.int(row, "IsShort") != 0,
            buy_ms,
            close_ms,
            sell_reason: self.text(row, "SellReason"),
            channel: self.text(row, "ChannelName"),
            signal_type: self.text(row, "SignalType"),
        }
    }

    /// A row with no close date yet is open — kept for `check_open_rows`.
    pub(super) fn track_open(&self, open: &mut HashSet<i64>, row: &ReportRow) {
        if self.close_ms(row) == 0 {
            open.insert(row.rec_id);
        } else {
            open.remove(&row.rec_id);
        }
    }
}
