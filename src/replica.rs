//! A local SQLite replica of one core's `Orders` report database, kept by
//! MoonProto's report replication (`client.reports()`, the crate's
//! `docs/reports.md`).
//!
//! One writer thread per replica owns the read-write connection and applies
//! every `ReportEvent` in delivery order, fed by a bare channel send from the
//! crate's sink thread, which must never block on SQLite. Readers (`pnl.rs`)
//! open their own connection against the same WAL-mode file.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moonproto::{
    MoonReports, ReportAliveMapComplete, ReportAliveMapOutcome, ReportEvent, ReportHistoryDepth, ReportRow,
    ReportRowsDeleted, ReportSchema, ReportSchemaField, ReportSyncCheckpoint, ReportSyncComplete, ReportSyncPage,
    ReportSyncRequest, ReportValue,
};
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::Connection;

pub const REPORT_TABLE: &str = "Orders";
/// The report protocol's own fixed field names.
const COL_REC_ID: &str = "newRecID";
pub const COL_DELETED: &str = "deleted";
pub const COL_CLOSE_DATE: &str = "CloseDate";
pub const COL_CLOSE_DATE_MS: &str = "CloseDateMs";

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
    pub fn open(core_id: &str, path: PathBuf, reports: MoonReports) -> Self {
        let (tx, rx) = mpsc::channel();
        let open_ids = Arc::new(Mutex::new(HashSet::new()));
        let status = Arc::new(Mutex::new(SyncStatus::default()));
        {
            let core_id = core_id.to_string();
            let reports = reports.clone();
            let open_ids = Arc::clone(&open_ids);
            let status = Arc::clone(&status);
            std::thread::spawn(move || run_writer(core_id, path, rx, reports, open_ids, status));
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

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The report table's column names; empty before it exists.
pub fn report_columns(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", quote_ident(REPORT_TABLE)))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    rows.collect()
}

fn history_depth_label(depth: ReportHistoryDepth) -> String {
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

fn bind_values<'a>(fields: &[ReportSchemaField], row: &'a ReportRow) -> Vec<ToSqlOutput<'a>> {
    fields.iter().map(|f| to_sql_value(row.value(f.index))).collect()
}

fn build_upsert_sql(columns: &[String], rec_id_col: &str) -> String {
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

fn open_connection(path: &Path) -> Result<Connection, String> {
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

fn read_checkpoint(conn: &Connection) -> Option<(ReportSyncCheckpoint, ReportHistoryDepth)> {
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
struct Table {
    fields: Vec<ReportSchemaField>,
    rec_id_col: String,
    upsert_sql: String,
    delete_sql: String,
    close_idx: Option<u16>,
    close_ms_idx: Option<u16>,
}

impl Table {
    /// A row with no close date yet is open — kept for `check_open_rows`.
    fn track_open(&self, open: &mut HashSet<i64>, row: &ReportRow) {
        let ms = self.close_ms_idx.and_then(|i| row.value(i)).and_then(as_i64);
        let secs = self.close_idx.and_then(|i| row.value(i)).and_then(as_i64);
        let is_open = ms.or_else(|| secs.map(|s| s.saturating_mul(1000))).unwrap_or(0) == 0;
        if is_open {
            open.insert(row.rec_id);
        } else {
            open.remove(&row.rec_id);
        }
    }
}

struct Writer {
    core_id: String,
    conn: Connection,
    reports: MoonReports,
    open_ids: Arc<Mutex<HashSet<i64>>>,
    status: Arc<Mutex<SyncStatus>>,
    table: Option<Table>,
    history_depth: ReportHistoryDepth,
    pending_sync_complete: Option<ReportSyncComplete>,
    /// The schema is revalidated once per hard session, not per `sync()`: a
    /// mid-session `database_recreated` gets pages with no further `Schema`
    /// event, so `recreate_database` re-migrates from this cache.
    cached_schema: Option<Arc<ReportSchema>>,
}

fn run_writer(
    core_id: String,
    path: PathBuf,
    rx: mpsc::Receiver<ReportEvent>,
    reports: MoonReports,
    open_ids: Arc<Mutex<HashSet<i64>>>,
    status: Arc<Mutex<SyncStatus>>,
) {
    let conn = match open_connection(&path) {
        Ok(c) => c,
        Err(e) => {
            log::error!("[replica] {core_id}: {e}");
            if let Ok(mut s) = status.lock() {
                *s = SyncStatus { phase: Phase::Error, error: Some(e), ..Default::default() };
            }
            return;
        }
    };
    let mut writer = Writer {
        core_id,
        conn,
        reports,
        open_ids,
        status,
        table: None,
        history_depth: ReportHistoryDepth::ServerDefault,
        pending_sync_complete: None,
        cached_schema: None,
    };
    writer.start();
    while let Ok(event) = rx.recv() {
        writer.handle_event(event);
    }
}

impl Writer {
    fn set(&self, edit: impl FnOnce(&mut SyncStatus)) {
        if let Ok(mut s) = self.status.lock() {
            edit(&mut s);
        }
    }

    fn start(&mut self) {
        let started = match read_checkpoint(&self.conn) {
            Some((checkpoint, depth)) => {
                self.history_depth = depth;
                self.reports.sync_from(checkpoint)
            }
            None => self.reports.sync(ReportSyncRequest::fresh(ReportHistoryDepth::ServerDefault)),
        };
        if let Err(e) = started {
            self.fail(format!("sync failed: {e}"));
        }
    }

    fn handle_event(&mut self, event: ReportEvent) {
        match event {
            ReportEvent::Schema(schema) => {
                self.migrate(&schema);
                self.cached_schema = Some(schema);
            }
            ReportEvent::SyncStarted { request, .. } => {
                self.history_depth = request.history_depth;
                self.set(|s| {
                    s.phase = Phase::Page;
                    s.rows_synced = 0;
                });
            }
            ReportEvent::SyncPage(page) => self.apply_page(&page),
            ReportEvent::RowUpsert(row) => self.upsert_row(&row),
            ReportEvent::RowDelete { rec_id } => self.delete_row(rec_id),
            ReportEvent::RowsDeleted(change) => self.apply_rows_deleted(&change),
            ReportEvent::SyncComplete(done) => {
                self.pending_sync_complete = Some(done.clone());
                self.set(|s| s.phase = Phase::Complete);
                if let Err(e) = self.reports.reconcile_alive(&done) {
                    self.fail(format!("reconcile_alive failed: {e}"));
                }
            }
            ReportEvent::AliveMapComplete(map) => self.apply_alive_map(map),
            ReportEvent::SchemaRejected { reason } => self.fail(reason),
            _ => {}
        }
    }

    fn fail(&self, msg: String) {
        log::error!("[replica] {}: {msg}", self.core_id);
        self.set(|s| {
            s.phase = Phase::Error;
            s.error = Some(msg);
        });
    }

    /// Create the table from the schema, or add any field it is missing —
    /// the schema is append-only, so this is safe on every `Schema` event.
    fn migrate(&mut self, schema: &ReportSchema) {
        let existing_cols = report_columns(&self.conn).unwrap_or_default();
        if existing_cols.is_empty() {
            let sql = schema.sqlite_create_table_sql(REPORT_TABLE);
            if let Err(e) = self.conn.execute(&sql, []) {
                self.fail(format!("create Orders table: {e}"));
                return;
            }
            let _ = self.conn.execute(&schema.sqlite_unique_index_sql(REPORT_TABLE), []);
        } else {
            for field in schema.fields() {
                if !existing_cols.iter().any(|c| c == &field.name) {
                    let sql = schema.sqlite_add_column_sql(REPORT_TABLE, field);
                    if let Err(e) = self.conn.execute(&sql, []) {
                        self.fail(format!("add column {}: {e}", field.name));
                        return;
                    }
                }
            }
        }
        let close_idx = schema.field_by_name(COL_CLOSE_DATE).map(|f| f.index);
        if close_idx.is_some() {
            // Reports narrow on it (`pnl.rs`) before the exact ms bounds.
            let sql = format!(
                "CREATE INDEX IF NOT EXISTS {} ON {}({})",
                quote_ident(&format!("{REPORT_TABLE}_{COL_CLOSE_DATE}")),
                quote_ident(REPORT_TABLE),
                quote_ident(COL_CLOSE_DATE)
            );
            if let Err(e) = self.conn.execute(&sql, []) {
                log::warn!("[replica] {}: close date index: {e}", self.core_id);
            }
        }
        let columns: Vec<String> = schema.fields().iter().map(|f| f.name.clone()).collect();
        let rec_id_col = schema
            .field(schema.rec_id_field_index())
            .map(|f| f.name.clone())
            .unwrap_or_else(|| COL_REC_ID.to_string());
        self.table = Some(Table {
            fields: schema.fields().to_vec(),
            upsert_sql: build_upsert_sql(&columns, &rec_id_col),
            delete_sql: format!("DELETE FROM {} WHERE {}=?1", quote_ident(REPORT_TABLE), quote_ident(&rec_id_col)),
            rec_id_col,
            close_idx,
            close_ms_idx: schema.field_by_name(COL_CLOSE_DATE_MS).map(|f| f.index),
        });
    }

    fn apply_page(&mut self, page: &ReportSyncPage) {
        if page.database_recreated {
            self.recreate_database();
            if let Err(e) = self.reports.page_applied(page) {
                self.fail(format!("page_applied (recreated) failed: {e}"));
            }
            return;
        }
        let Some(table) = &self.table else {
            self.fail("report page arrived before schema".to_string());
            return;
        };
        let result = (|| -> rusqlite::Result<()> {
            let tx = self.conn.transaction()?;
            {
                let mut stmt = tx.prepare_cached(&table.upsert_sql)?;
                for row in page.rows.iter() {
                    stmt.execute(rusqlite::params_from_iter(bind_values(&table.fields, row)))?;
                }
            }
            tx.commit()
        })();
        match result {
            Ok(()) => {
                if let Ok(mut open) = self.open_ids.lock() {
                    for row in page.rows.iter() {
                        table.track_open(&mut open, row);
                    }
                }
                let synced = page.row_count() as u32;
                self.set(|s| {
                    s.phase = Phase::Page;
                    s.rows_synced += synced;
                });
                // Only after the commit: the next page is requested by this.
                if let Err(e) = self.reports.page_applied(page) {
                    self.fail(format!("page_applied failed: {e}"));
                }
            }
            Err(e) => self.fail(format!("commit page: {e}")),
        }
    }

    fn upsert_row(&mut self, row: &ReportRow) {
        let Some(table) = &self.table else {
            return;
        };
        let values = bind_values(&table.fields, row);
        let upserted = self
            .conn
            .prepare_cached(&table.upsert_sql)
            .and_then(|mut stmt| stmt.execute(rusqlite::params_from_iter(values)));
        if let Err(e) = upserted {
            self.fail(format!("upsert row {}: {e}", row.rec_id));
            return;
        }
        if let Ok(mut open) = self.open_ids.lock() {
            table.track_open(&mut open, row);
        }
    }

    fn delete_row(&mut self, rec_id: i64) {
        let Some(table) = &self.table else {
            return;
        };
        let deleted = self
            .conn
            .prepare_cached(&table.delete_sql)
            .and_then(|mut stmt| stmt.execute(rusqlite::params![rec_id]));
        if let Err(e) = deleted {
            self.fail(format!("delete row {rec_id}: {e}"));
            return;
        }
        if let Ok(mut open) = self.open_ids.lock() {
            open.remove(&rec_id);
        }
    }

    fn apply_rows_deleted(&mut self, change: &ReportRowsDeleted) {
        let Some(t) = &self.table else {
            return;
        };
        let table = quote_ident(REPORT_TABLE);
        let rec = quote_ident(&t.rec_id_col);
        let del = quote_ident(COL_DELETED);
        let flag = change.deleted as i64;
        let result = (|| -> rusqlite::Result<()> {
            if !change.ranges.is_empty() {
                let mut by_range = self
                    .conn
                    .prepare_cached(&format!("UPDATE {table} SET {del}=?1 WHERE {rec} BETWEEN ?2 AND ?3"))?;
                for range in change.ranges.iter() {
                    by_range.execute(rusqlite::params![flag, range.from_rec_id, range.to_rec_id])?;
                }
            }
            if !change.singles.is_empty() {
                let placeholders = vec!["?"; change.singles.len()].join(",");
                let sql = format!("UPDATE {table} SET {del}=?1 WHERE {rec} IN ({placeholders})");
                let mut params: Vec<SqlValue> = vec![SqlValue::Integer(flag)];
                params.extend(change.singles.iter().map(|id| SqlValue::Integer(*id)));
                self.conn.execute(&sql, rusqlite::params_from_iter(params))?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.fail(format!("apply rows_deleted: {e}"));
        }
    }

    /// Reconcile visibility after a completed catch-up (`Snapshot`) and store
    /// the checkpoint in the same transaction, or discard the stale replica
    /// and resync (`DatabaseRecreated`).
    fn apply_alive_map(&mut self, map: ReportAliveMapComplete) {
        match map.outcome {
            ReportAliveMapOutcome::DatabaseRecreated => {
                self.recreate_database();
                if let Err(e) = self.reports.sync(ReportSyncRequest::fresh(self.history_depth)) {
                    self.fail(format!("resync after database_recreated: {e}"));
                }
            }
            ReportAliveMapOutcome::Snapshot => {
                let Some(done) = self.pending_sync_complete.take() else {
                    return;
                };
                let Some(t) = &self.table else {
                    return;
                };
                let table = quote_ident(REPORT_TABLE);
                let rec = quote_ident(&t.rec_id_col);
                let del = quote_ident(COL_DELETED);
                let checkpoint = done.checkpoint();
                let history_depth = history_depth_label(self.history_depth);
                let synced_at = chrono::Utc::now().timestamp_millis();
                let result = (|| -> rusqlite::Result<()> {
                    let tx = self.conn.transaction()?;
                    {
                        let flagged: Vec<(i64, bool)> = {
                            let mut stmt = tx.prepare(&format!("SELECT {rec}, {del} FROM {table} WHERE {rec} <= ?1"))?;
                            let found = stmt
                                .query_map(rusqlite::params![map.covered_up_to], |r| {
                                    Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0) != 0))
                                })?
                                .filter_map(Result::ok)
                                .collect();
                            found
                        };
                        let mut update = tx.prepare(&format!("UPDATE {table} SET {del}=?1 WHERE {rec}=?2"))?;
                        for (id, deleted) in flagged {
                            if let Some(alive) = map.is_alive(id) {
                                if alive == deleted {
                                    update.execute(rusqlite::params![!alive as i64, id])?;
                                }
                            }
                        }
                    }
                    tx.execute(
                        "INSERT INTO _sync_meta (id, epoch, next_from_rec_id, history_depth, synced_at)
                         VALUES (1, ?1, ?2, ?3, ?4)
                         ON CONFLICT(id) DO UPDATE SET epoch=excluded.epoch, next_from_rec_id=excluded.next_from_rec_id,
                           history_depth=excluded.history_depth, synced_at=excluded.synced_at",
                        rusqlite::params![checkpoint.epoch, checkpoint.next_from_rec_id, history_depth, synced_at],
                    )?;
                    tx.commit()
                })();
                match result {
                    Ok(()) => self.set(|s| {
                        s.phase = Phase::Live;
                        s.error = None;
                    }),
                    Err(e) => self.fail(format!("apply alive map: {e}")),
                }
            }
        }
    }

    fn recreate_database(&mut self) {
        let _ = self.conn.execute(&format!("DROP TABLE IF EXISTS {}", quote_ident(REPORT_TABLE)), []);
        let _ = self.conn.execute("DELETE FROM _sync_meta WHERE id=1", []);
        self.table = None;
        if let Ok(mut open) = self.open_ids.lock() {
            open.clear();
        }
        self.set(|s| *s = SyncStatus::default());
        if let Some(schema) = self.cached_schema.clone() {
            self.migrate(&schema);
        }
    }
}
