//! SQLite store: restores `PoolState` on start and persists what `core` emits.
//!
//! One writer thread owns the only write connection. `send` queues an op and returns at once.
//! Immediate ops (see `PersistOp::durability`, and every event) are written in order, each in
//! its own transaction with `synchronous=FULL`. Batched ops wait in memory and go to disk in
//! one transaction every `batch_interval` and on `close`. Reads use a small pool of read-only
//! connections, so a slow query never waits for the writer.
//!
//! `flush` and `close` block the calling thread: call them from `spawn_blocking` in async
//! code.

use std::ops::Deref;
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mtpool_core::{Event, PersistOp, PoolState, Ts};
use rusqlite::{Connection, OpenFlags};

pub use admin::AdminOp;
pub use auth::{AuthOp, NewTokenRow};
pub use history::{BUCKET_SEC, HistoryOp, NodeBucket, availability, node_history};
pub use reports::{ReportOp, measurement_seen};

mod admin;
mod auth;
mod history;
mod queries;
mod reports;
#[cfg(test)]
mod tests;
mod writer;

/// Interval of the batched flush.
pub const DEFAULT_BATCH_INTERVAL: Duration = Duration::from_secs(15);

/// Read connections kept open.
const READ_POOL_SIZE: usize = 4;
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Schema migrations in order. A database with a higher version than the last one here belongs
/// to a newer build and is not opened.
const MIGRATIONS: &[(i64, &str)] = &[
    (1, include_str!("../../migrations/0001_init.sql")),
    (2, include_str!("../../migrations/0002_auth.sql")),
    (3, include_str!("../../migrations/0003_reports.sql")),
    (4, include_str!("../../migrations/0004_admin.sql")),
    (5, include_str!("../../migrations/0005_rollouts.sql")),
    (6, include_str!("../../migrations/0006_client_samples.sql")),
    (7, include_str!("../../migrations/0007_node_samples.sql")),
    (8, include_str!("../../migrations/0008_record_cleanup.sql")),
];

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot open database {path}: {source}")]
    Open {
        path: String,
        #[source]
        source: rusqlite::Error,
    },
    #[error(
        "database {path} is corrupt or not a SQLite database ({detail}); \
         restore it from a backup or move it away to start with an empty pool"
    )]
    Corrupt { path: String, detail: String },
    #[error(
        "database schema version {found} is newer than this build supports ({supported}); \
         run a newer registrar or restore a database that matches this build"
    )]
    UnknownSchemaVersion { found: i64, supported: i64 },
    #[error("migration {version} failed: {source}")]
    Migration {
        version: i64,
        #[source]
        source: rusqlite::Error,
    },
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A row that no longer parses into a core type: edited by hand or written by another
    /// version.
    #[error("stored data is invalid: {0}")]
    InvalidData(String),
    /// Writes since the previous `flush` (or the first one) that were lost; the first error.
    #[error("{failed} write(s) failed, the first: {first}")]
    WriteFailed { failed: usize, first: String },
    /// The copy of the database for a rollback could not be made or taken back.
    #[error("database copy: {0}")]
    Copy(String),
    #[error("the store writer is not running")]
    WriterStopped,
}

#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// How often batched ops are written.
    pub batch_interval: Duration,
}

impl Default for StoreOptions {
    fn default() -> StoreOptions {
        StoreOptions {
            batch_interval: DEFAULT_BATCH_INTERVAL,
        }
    }
}

/// What the writer takes: the ops of `core`, its events, the auth writes, the agent report
/// writes and the writes of the admin and bot API.
#[derive(Debug, Clone, PartialEq)]
pub enum WriteOp {
    Persist(PersistOp),
    Event(Event),
    Auth(AuthOp),
    Report(ReportOp),
    Admin(AdminOp),
    History(HistoryOp),
}

impl From<HistoryOp> for WriteOp {
    fn from(op: HistoryOp) -> WriteOp {
        WriteOp::History(op)
    }
}

impl From<AdminOp> for WriteOp {
    fn from(op: AdminOp) -> WriteOp {
        WriteOp::Admin(op)
    }
}

impl From<AuthOp> for WriteOp {
    fn from(op: AuthOp) -> WriteOp {
        WriteOp::Auth(op)
    }
}

impl From<ReportOp> for WriteOp {
    fn from(op: ReportOp) -> WriteOp {
        WriteOp::Report(op)
    }
}

impl From<PersistOp> for WriteOp {
    fn from(op: PersistOp) -> WriteOp {
        WriteOp::Persist(op)
    }
}

impl From<Event> for WriteOp {
    fn from(event: Event) -> WriteOp {
        WriteOp::Event(event)
    }
}

/// Whether the disk keeps up with the state. `Failed` stays until the process restarts: a lost
/// write means the disk no longer matches memory, and only a restart reconciles them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteHealth {
    Ok,
    Failed { error: String, at: Ts },
}

/// Handle to the store. Clones share one writer; the writer stops (after writing the queue)
/// when the last clone is dropped or `close` is called.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    tx: Sender<writer::Msg>,
    shared: Arc<Shared>,
    handle: Mutex<Option<JoinHandle<()>>>,
    readers: Arc<ReadPool>,
}

/// State the writer thread shares with the handles.
pub(crate) struct Shared {
    health: Mutex<WriteHealth>,
    /// What `PRAGMA synchronous` read back right after the writer set it for an immediate op
    /// (2 is FULL), or -1 before the first one. A test probe.
    #[cfg(test)]
    last_immediate_sync: AtomicI64,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            health: Mutex::new(WriteHealth::Ok),
            #[cfg(test)]
            last_immediate_sync: AtomicI64::new(-1),
        }
    }

    pub(crate) fn fail(&self, error: String, at: Ts) {
        *lock(&self.health) = WriteHealth::Failed { error, at };
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn now_ts() -> Ts {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ts(i64::try_from(ms).unwrap_or(i64::MAX))
}

impl Store {
    /// Opens (and creates) the database, runs migrations and starts the writer. Refuses a
    /// corrupt file and a schema version newer than this build knows.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Store::open_with(path, StoreOptions::default())
    }

    pub fn open_with(path: &Path, opts: StoreOptions) -> Result<Store, StoreError> {
        // Private from the first byte, and narrowed on every open so an older install heals.
        // SQLite gives the -wal and -shm files the mode of this one.
        // A failure here (a missing directory, say) shows again as the error of the open below.
        if let Err(e) = crate::backup::make_private(path) {
            tracing::warn!(error = %e, "cannot make the database file private");
        }
        let mut conn = Connection::open(path).map_err(|e| classify(path, e))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| classify(path, e))?;
        check_integrity(path, &conn)?;
        // Switching to WAL does not wait for a lock by itself, and on a fresh install another
        // process opens the same new file at the same moment.
        until_unlocked(|| conn.pragma_update(None, "journal_mode", "WAL"))
            .map_err(|e| classify(path, e))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate_keeping_a_copy(path, &mut conn)?;

        let mut readers = Vec::with_capacity(READ_POOL_SIZE);
        for _ in 0..READ_POOL_SIZE {
            readers.push(open_reader(path)?);
        }

        let shared = Arc::new(Shared::new());
        let (tx, rx) = mpsc::channel();
        let handle = writer::spawn(conn, opts, rx, Arc::clone(&shared))?;
        Ok(Store {
            inner: Arc::new(Inner {
                tx,
                shared,
                handle: Mutex::new(Some(handle)),
                readers: Arc::new(ReadPool {
                    conns: Mutex::new(readers),
                    available: Condvar::new(),
                }),
            }),
        })
    }

    /// Restores the pool state. An empty database gives the default settings and nothing else.
    pub fn load(&self) -> Result<PoolState, StoreError> {
        self.load_at(now_ts())
    }

    /// `load` at a given time: tombstones past their 7 days are skipped.
    pub fn load_at(&self, now: Ts) -> Result<PoolState, StoreError> {
        queries::load_state(&self.read(), now)
    }

    /// Queues a write and returns at once. If the writer is gone the write is lost and the
    /// store reports `WriteHealth::Failed`.
    pub fn send(&self, op: impl Into<WriteOp>) {
        if self
            .inner
            .tx
            .send(writer::Msg::Op(Box::new(op.into())))
            .is_err()
        {
            self.inner
                .shared
                .fail("the store writer is not running".into(), now_ts());
        }
    }

    /// Blocks until everything sent before the call is on disk, batched ops included. An error
    /// means writes since the last flush failed (they are lost) or the writer is gone.
    pub fn flush(&self) -> Result<(), StoreError> {
        self.request(writer::Msg::Flush)
    }

    /// Writes the queue and stops the writer thread. Idempotent. Reads still work afterwards;
    /// writes do not.
    pub fn close(&self) -> Result<(), StoreError> {
        let Some(handle) = lock(&self.inner.handle).take() else {
            return Ok(());
        };
        let result = self.request(writer::Msg::Close);
        let _ = handle.join();
        result
    }

    pub fn health(&self) -> WriteHealth {
        lock(&self.inner.shared.health).clone()
    }

    /// A read-only connection from the pool; waits while all of them are in use.
    pub fn read(&self) -> ReadConn {
        let pool = &self.inner.readers;
        let mut conns = lock(&pool.conns);
        let conn = loop {
            if let Some(conn) = conns.pop() {
                break conn;
            }
            conns = pool
                .available
                .wait(conns)
                .unwrap_or_else(|e| e.into_inner());
        };
        ReadConn {
            conn: Some(conn),
            pool: Arc::clone(pool),
        }
    }

    /// Deletes events older than 30 days, traffic and sensor rounds (with their votes) older
    /// than 35 days and tombstones past their 7 days. Bans are kept. Runs on the writer thread,
    /// in order with the other writes.
    pub fn retain(&self, now: Ts) -> Result<(), StoreError> {
        self.request(|reply| writer::Msg::Retain(now, reply))
    }

    /// Writes one admin op and waits for it: the id of the row an insert made (0 for the other
    /// ops). A failed write is an error for this caller only; it does not make the store
    /// unhealthy unless the disk itself failed. Blocks the calling thread.
    pub fn admin(&self, op: AdminOp) -> Result<i64, StoreError> {
        let (reply, answer) = mpsc::channel();
        self.inner
            .tx
            .send(writer::Msg::Admin(Box::new(op), reply))
            .map_err(|_| StoreError::WriterStopped)?;
        answer.recv().map_err(|_| StoreError::WriterStopped)?
    }

    /// Sends a message with a reply channel and waits for the reply.
    fn request(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<(), StoreError>>) -> writer::Msg,
    ) -> Result<(), StoreError> {
        let (reply, answer) = mpsc::channel();
        self.inner
            .tx
            .send(make(reply))
            .map_err(|_| StoreError::WriterStopped)?;
        answer.recv().map_err(|_| StoreError::WriterStopped)?
    }

    #[cfg(test)]
    pub(crate) fn last_immediate_sync(&self) -> i64 {
        self.inner.shared.last_immediate_sync.load(Ordering::SeqCst)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last handle is gone: write what is queued and stop the writer.
        if let Some(handle) = lock(&self.handle).take() {
            let (reply, answer) = mpsc::channel();
            if self.tx.send(writer::Msg::Close(reply)).is_ok() {
                let _ = answer.recv();
            }
            let _ = handle.join();
        }
    }
}

struct ReadPool {
    conns: Mutex<Vec<Connection>>,
    available: Condvar,
}

/// A pooled read-only connection; it goes back to the pool on drop.
pub struct ReadConn {
    conn: Option<Connection>,
    pool: Arc<ReadPool>,
}

impl Deref for ReadConn {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("the connection is only taken on drop")
    }
}

impl Drop for ReadConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            lock(&self.pool.conns).push(conn);
            self.pool.available.notify_one();
        }
    }
}

fn open_reader(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| classify(path, e))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Maps the errors of a damaged file to `Corrupt`, the rest to `Open`.
fn classify(path: &Path, e: rusqlite::Error) -> StoreError {
    use rusqlite::ErrorCode::{DatabaseCorrupt, NotADatabase};
    match e.sqlite_error_code() {
        Some(NotADatabase | DatabaseCorrupt) => StoreError::Corrupt {
            path: display(path),
            detail: e.to_string(),
        },
        _ => StoreError::Open {
            path: display(path),
            source: e,
        },
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn check_integrity(path: &Path, conn: &Connection) -> Result<(), StoreError> {
    let verdict: String = conn
        .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
        .map_err(|e| classify(path, e))?;
    if verdict == "ok" {
        Ok(())
    } else {
        Err(StoreError::Corrupt {
            path: display(path),
            detail: format!("integrity check: {verdict}"),
        })
    }
}

/// How long after a migration the build before it may still take the database back. A rollback
/// happens within a minute or two of an update: after the watch of the new build nothing rolls it
/// back. Whoever starts an old build later would lose everything written since, so they restore
/// by hand.
const UNDO_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Goes back to the database this build knows when a newer build migrated it moments ago and was
/// rolled back: the copy kept before the migration replaces the migrated database. `true` when it
/// did; what the newer build wrote in between is lost. Call under the serving lock, before
/// [`Store::open`], which refuses a newer database in every other case.
pub fn undo_newer_schema(path: &Path) -> Result<bool, StoreError> {
    undo_schema(path, latest_version(), UNDO_WINDOW)
}

fn undo_schema(path: &Path, supported: i64, window: Duration) -> Result<bool, StoreError> {
    if !path.is_file() {
        return Ok(false);
    }
    // A file that cannot be read is for `Store::open` to explain.
    let found = Connection::open(path)
        .ok()
        .and_then(|conn| stored_version(&conn).ok());
    if !found.is_some_and(|found| found > supported) {
        return Ok(false);
    }
    let copy = crate::backup::before_migration(path, supported);
    let age = std::fs::metadata(&copy)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|at| at.elapsed().ok());
    if !age.is_some_and(|age| age < window) {
        return Ok(false);
    }
    crate::backup::take_back_before_migration(path, supported)
        .map_err(|e| StoreError::Copy(e.to_string()))?;
    Ok(true)
}

fn latest_version() -> i64 {
    MIGRATIONS.last().map_or(0, |(v, _)| *v)
}

/// A migrated database does not open in the build before it, so a rollback needs the one from
/// before the migration. Without the copy the migration does not happen: the failed start leaves
/// the database as it was, which is safe for that build too. Other processes wait, so none of
/// them takes the half-migrated database for an old one and copies it; a new database has nothing
/// to keep.
fn migrate_keeping_a_copy(path: &Path, conn: &mut Connection) -> Result<(), StoreError> {
    if stored_version(conn)? >= latest_version() {
        return migrate(conn);
    }
    let _others_wait =
        crate::backup::lock_for_migration(path).map_err(|e| StoreError::Copy(e.to_string()))?;
    let current = stored_version(conn)?;
    if current != 0 && current < latest_version() {
        crate::backup::keep_before_migration(path, current)
            .map_err(|e| StoreError::Copy(e.to_string()))?;
    }
    migrate(conn)
}

/// Applies every migration above the stored version, each in its own transaction. The version
/// is read again under the write lock, since another process may have just applied it.
fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let current = stored_version(conn)?;
    if current > latest_version() {
        return Err(StoreError::UnknownSchemaVersion {
            found: current,
            supported: latest_version(),
        });
    }
    for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
        let migration = |source| StoreError::Migration {
            version: *version,
            source,
        };
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(migration)?;
        if stored_version(&tx)? >= *version {
            continue;
        }
        tx.execute_batch(sql).map_err(migration)?;
        tx.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![version, now_ts().0],
        )
        .map_err(migration)?;
        tx.commit().map_err(migration)?;
    }
    Ok(())
}

/// The newest applied migration, 0 on a new database.
fn stored_version(conn: &Connection) -> Result<i64, StoreError> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations')",
        [],
        |r| r.get(0),
    )?;
    if !has_table {
        return Ok(0);
    }
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?)
}

/// Repeats `op` while the database is locked, up to `BUSY_TIMEOUT`.
fn until_unlocked<T>(mut op: impl FnMut() -> rusqlite::Result<T>) -> rusqlite::Result<T> {
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match op() {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            result => return result,
        }
    }
}
