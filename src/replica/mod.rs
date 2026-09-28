//! A local SQLite replica of one core's `Orders` report database, kept by
//! MoonProto's report replication (`client.reports()`, the crate's
//! `docs/reports.md`).
//!
//! One writer thread per replica owns the read-write connection and applies
//! every `ReportEvent` in delivery order, fed by a bare channel send from the
//! crate's sink thread, which must never block on SQLite. Readers (`pnl.rs`)
//! open their own connection against the same WAL-mode file.
//!
//! A live upsert that turns a row from open (or unknown) to closed is also
//! handed on as a [`ClosedTrade`], for the Live trades feed (`bot/feed.rs`).

mod sql;
mod writer;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use moonproto::{MoonReports, ReportEvent};
use tokio::sync::mpsc::UnboundedSender;

pub use sql::{close_ms_sql, quote_ident, report_columns};

pub const REPORT_TABLE: &str = "Orders";
/// The report protocol's own fixed field names.
const COL_REC_ID: &str = "newRecID";
pub const COL_DELETED: &str = "deleted";
pub const COL_CLOSE_DATE: &str = "CloseDate";
pub const COL_CLOSE_DATE_MS: &str = "CloseDateMs";
/// MoonBot's `Orders` fields the reports and the Live trades feed read.
pub const COL_PROFIT: &str = "ProfitBTC";
pub const COL_SPENT: &str = "SpentBTC";
pub const COL_EMULATOR: &str = "Emulator";
pub const COL_COIN: &str = "Coin";

/// A trade the core just closed, as its report row has it.
pub struct ClosedTrade {
    pub core_id: String,
    pub coin: String,
    /// In the core's base currency, whatever the `*BTC` names say.
    pub profit: f64,
    pub spent: f64,
    pub emulator: bool,
    pub short: bool,
    /// Report-clock epoch ms; 0 when the row has none.
    pub buy_ms: i64,
    pub close_ms: i64,
    pub sell_reason: String,
    /// Where the buy came from, e.g. `MoonShot: (strategy <…>)`.
    pub channel: String,
    /// The strategy behind a liquidation, whose channel is just `LIQUIDATION`.
    pub signal_type: String,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Schema,
    Page,
    Complete,
    Live,
    Error,
}

#[derive(Clone, Default)]
pub struct SyncStatus {
    pub phase: Phase,
    /// Rows applied by the catch-up in progress.
    pub rows_synced: u32,
    pub error: Option<String>,
}

/// The session's side of a replica: dropping it ends the writer thread.
pub struct Replica {
    tx: mpsc::Sender<ReportEvent>,
    reports: MoonReports,
    open_ids: Arc<Mutex<HashSet<i64>>>,
    status: Arc<Mutex<SyncStatus>>,
}

impl Replica {
    /// Open (or reopen) the replica at `path` and start catch-up — from its
    /// own checkpoint, or fresh for an empty file. The crate resumes report
    /// catch-up across reconnects of the same session on its own.
    pub fn open(core_id: &str, path: PathBuf, reports: MoonReports, closed: UnboundedSender<ClosedTrade>) -> Self {
        let (tx, rx) = mpsc::channel();
        let open_ids = Arc::new(Mutex::new(HashSet::new()));
        let status = Arc::new(Mutex::new(SyncStatus::default()));
        {
            let core_id = core_id.to_string();
            let reports = reports.clone();
            let open_ids = Arc::clone(&open_ids);
            let status = Arc::clone(&status);
            std::thread::spawn(move || writer::run_writer(core_id, path, rx, reports, open_ids, status, closed));
        }
        Self { tx, reports, open_ids, status }
    }

    pub fn send(&self, event: ReportEvent) {
        let _ = self.tx.send(event);
    }

    pub fn status(&self) -> SyncStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// The open rows, to register again with the core so a close or change
    /// outside catch-up range — or while offline — still reaches the replica.
    /// Taken under the sessions lock, sent outside it.
    pub fn open_rows(&self) -> OpenRows {
        let ids = self.open_ids.lock().map(|s| s.iter().copied().collect()).unwrap_or_default();
        OpenRows { reports: self.reports.clone(), ids }
    }
}

pub struct OpenRows {
    reports: MoonReports,
    ids: Vec<i64>,
}

impl OpenRows {
    pub fn send(self) {
        if !self.ids.is_empty() {
            let _ = self.reports.check_open_rows(&self.ids);
        }
    }
}

pub fn remove_files(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar(path, "-wal"));
    let _ = std::fs::remove_file(sidecar(path, "-shm"));
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}
