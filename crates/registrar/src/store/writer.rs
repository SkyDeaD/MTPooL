//! The writer thread: the only holder of the write connection.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Instant;

use mtpool_core::{Durability, Event, EventKind, PersistOp, SensorKind, SrmdState, Ts, Vote};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::json;

use super::{Shared, StoreError, StoreOptions, WriteOp, now_ts};

/// `traffic` holds one row per node and this length of time.
const TRAFFIC_ROW_MS: i64 = 3_600_000;

pub(super) type Reply = Sender<Result<(), StoreError>>;

pub(super) enum Msg {
    Op(Box<WriteOp>),
    /// Writes the batch, then answers: everything sent before is on disk.
    Flush(Reply),
    Retain(Ts, Reply),
    /// One admin op, answered with the id of the row it inserted.
    Admin(Box<super::AdminOp>, Sender<Result<i64, StoreError>>),
    /// Writes the batch, answers and stops the thread.
    Close(Reply),
}

pub(super) fn spawn(
    conn: Connection,
    opts: StoreOptions,
    rx: Receiver<Msg>,
    shared: Arc<Shared>,
) -> Result<JoinHandle<()>, StoreError> {
    let writer = Writer {
        conn,
        batch: Vec::new(),
        interval: opts.batch_interval,
        shared,
        sync_full: false,
        failed: Vec::new(),
    };
    std::thread::Builder::new()
        .name("store-writer".into())
        .spawn(move || writer.run(rx))
        .map_err(|e| StoreError::InvalidData(format!("cannot start the writer thread: {e}")))
}

struct Writer {
    conn: Connection,
    /// Batched ops in arrival order.
    batch: Vec<PersistOp>,
    interval: std::time::Duration,
    shared: Arc<Shared>,
    /// `synchronous` is FULL now (the connection starts at NORMAL).
    sync_full: bool,
    /// Errors of writes since the last flush reply.
    failed: Vec<String>,
}

/// Marks the store failed if the thread dies by a panic.
struct PanicGuard(Arc<Shared>);

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.fail("the store writer panicked".into(), now_ts());
        }
    }
}

impl Writer {
    fn run(mut self, rx: Receiver<Msg>) {
        let _guard = PanicGuard(Arc::clone(&self.shared));
        let mut next_flush = Instant::now() + self.interval;
        loop {
            let wait = next_flush.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(Msg::Op(op)) => self.handle(*op),
                Ok(Msg::Flush(reply)) => {
                    self.flush_batch();
                    let _ = reply.send(self.take_failures());
                }
                Ok(Msg::Admin(op, reply)) => {
                    let _ = reply.send(self.admin_call(&op));
                }
                Ok(Msg::Retain(now, reply)) => {
                    let result = super::queries::retain(&self.conn, now);
                    if let Err(e) = &result {
                        self.record_failure(format!("retention: {e}"));
                    }
                    let _ = reply.send(result);
                }
                Ok(Msg::Close(reply)) => {
                    self.flush_batch();
                    let _ = reply.send(self.take_failures());
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.flush_batch();
                    next_flush = Instant::now() + self.interval;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.flush_batch();
                    return;
                }
            }
        }
    }

    fn handle(&mut self, op: WriteOp) {
        match op {
            WriteOp::Event(event) => {
                self.write_immediate("event", |conn| insert_event(conn, &event))
            }
            WriteOp::Auth(op) => {
                self.write_immediate(super::auth::name(&op), |conn| super::auth::apply(conn, &op))
            }
            WriteOp::Admin(op) => self.write_immediate(super::admin::name(&op), |conn| {
                super::admin::apply(conn, &op).map(|_| ())
            }),
            WriteOp::History(op) => self.write_immediate(super::history::name(&op), |conn| {
                super::history::apply(conn, &op)
            }),
            WriteOp::Report(op) => self.write_immediate(super::reports::name(&op), |conn| {
                super::reports::apply(conn, &op)
            }),
            WriteOp::Persist(op) => match op.durability() {
                Durability::Batched => self.batch.push(op),
                Durability::Immediate => {
                    // The queued batched ops are older than this one. Where it replaces them,
                    // they must not overwrite it when the batch is written later.
                    match &op {
                        PersistOp::UpsertNode(r) => self.batch.retain(
                            |q| !matches!(q, PersistOp::NodeHealth { node, .. } if *node == r.id),
                        ),
                        PersistOp::DeleteNode(id) => self.batch.retain(|q| {
                            !matches!(q,
                                PersistOp::NodeHealth { node, .. } | PersistOp::NodeStats { node, .. }
                                if node == id)
                        }),
                        _ => {}
                    }
                    self.write_immediate(op_name(&op), |conn| apply_op(conn, &op));
                }
            },
        }
    }

    /// An admin op whose caller waits: the error goes to the caller and not into the failures
    /// of the next flush. A constraint violation is the caller's mistake; any other error is the
    /// disk's and makes the store unhealthy.
    fn admin_call(&mut self, op: &super::AdminOp) -> Result<i64, StoreError> {
        let name = super::admin::name(op);
        let result = self.set_sync(true).and_then(|()| {
            let tx = self.conn.transaction()?;
            let id = super::admin::apply(&tx, op)?;
            tx.commit()?;
            Ok(id)
        });
        if let Err(e) = &result
            && !matches!(
                e,
                StoreError::Sqlite(rusqlite::Error::SqliteFailure(f, _))
                    if f.code == rusqlite::ErrorCode::ConstraintViolation
            )
        {
            self.record_failure(format!("{name}: {e}"));
            self.failed.pop();
        }
        result
    }

    /// One transaction with `synchronous=FULL`.
    fn write_immediate(
        &mut self,
        name: &str,
        write: impl FnOnce(&Connection) -> Result<(), StoreError>,
    ) {
        let result = self
            .set_sync(true)
            .and_then(|()| self.in_transaction(write));
        if let Err(e) = result {
            self.record_failure(format!("{name}: {e}"));
        }
    }

    fn in_transaction(
        &mut self,
        write: impl FnOnce(&Connection) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        write(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Writes the queued batched ops in one transaction. An op that fails is rolled back alone
    /// and recorded; the others still go in.
    fn flush_batch(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        let ops = std::mem::take(&mut self.batch);
        let result = self.set_sync(false).and_then(|()| self.write_batch(&ops));
        if let Err(e) = result {
            self.record_failure(format!("batch of {} op(s): {e}", ops.len()));
        }
    }

    fn write_batch(&mut self, ops: &[PersistOp]) -> Result<(), StoreError> {
        let mut failures = Vec::new();
        let mut tx = self.conn.transaction()?;
        for op in ops {
            let mut sp = tx.savepoint()?;
            match apply_op(&sp, op) {
                Ok(()) => sp.commit()?,
                Err(e) => {
                    failures.push(format!("{}: {e}", op_name(op)));
                    sp.rollback()?;
                    sp.commit()?;
                }
            }
        }
        tx.commit()?;
        for failure in failures {
            self.record_failure(failure);
        }
        Ok(())
    }

    fn set_sync(&mut self, full: bool) -> Result<(), StoreError> {
        if self.sync_full != full {
            self.conn
                .pragma_update(None, "synchronous", if full { "FULL" } else { "NORMAL" })?;
            self.sync_full = full;
        }
        #[cfg(test)]
        if full {
            let mode: i64 = self
                .conn
                .query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            self.shared
                .last_immediate_sync
                .store(mode, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }

    /// The store is unhealthy from now on, a `store_error` event says so (if it can be written),
    /// and the next flush reports it.
    fn record_failure(&mut self, error: String) {
        let at = now_ts();
        self.shared.fail(error.clone(), at);
        let event = Event {
            kind: EventKind::StoreError,
            at,
            node: None,
            owner: None,
            domain: None,
            detail: json!({ "error": error }),
        };
        // Best effort: the failure may be the disk itself.
        let _ = self
            .set_sync(true)
            .and_then(|()| self.in_transaction(|c| insert_event(c, &event)));
        self.failed.push(error);
    }

    fn take_failures(&mut self) -> Result<(), StoreError> {
        let failed = std::mem::take(&mut self.failed);
        match failed.first() {
            None => Ok(()),
            Some(first) => Err(StoreError::WriteFailed {
                failed: failed.len(),
                first: first.clone(),
            }),
        }
    }
}

// ---- statements ----

/// Names of ops for error messages. The ops themselves are not printed: settings carry
/// proxy secrets.
fn op_name(op: &PersistOp) -> &'static str {
    match op {
        PersistOp::UpsertNode(_) => "upsert_node",
        PersistOp::DeleteNode(_) => "delete_node",
        PersistOp::NodeHealth { .. } => "node_health",
        PersistOp::NodeStats { .. } => "node_stats",
        PersistOp::Traffic { .. } => "traffic",
        PersistOp::RecordRound { .. } => "record_round",
        PersistOp::RecordBan { .. } => "record_ban",
        PersistOp::LiftBan { .. } => "lift_ban",
        PersistOp::UpsertIpBlock(_) => "upsert_ip_block",
        PersistOp::DeleteIpBlock { .. } => "delete_ip_block",
        PersistOp::UpsertTombstone(_) => "upsert_tombstone",
        PersistOp::DeleteTombstone { .. } => "delete_tombstone",
        PersistOp::SetMasters { .. } => "set_masters",
        PersistOp::DeleteMasters { .. } => "delete_masters",
        PersistOp::UpsertDns { .. } => "upsert_dns",
        PersistOp::SetSrmd(_) => "set_srmd",
        PersistOp::SetSettings(_) => "set_settings",
    }
}

fn to_json<T: Serialize>(what: &str, value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value)
        .map_err(|e| StoreError::InvalidData(format!("cannot serialize {what}: {e}")))
}

/// The snake_case name of a unit-variant enum, as serde writes it.
fn enum_name<T: Serialize>(value: &T) -> Result<String, StoreError> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        Ok(other) => Err(StoreError::InvalidData(format!(
            "{other} is not a plain enum name"
        ))),
        Err(e) => Err(StoreError::InvalidData(format!(
            "cannot serialize an enum: {e}"
        ))),
    }
}

/// SQLite integers are signed 64 bit: a counter past that saturates instead of wrapping.
fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn vote_name(vote: &Vote) -> &'static str {
    match vote {
        Vote::Blocked => "blocked",
        Vote::Clear => "clear",
        Vote::Abstain(_) => "abstain",
    }
}

fn insert_event(conn: &Connection, e: &Event) -> Result<(), StoreError> {
    conn.prepare_cached(
        "INSERT INTO events(at, kind, node, domain, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![
        e.at.0,
        enum_name(&e.kind)?,
        e.node.as_ref().map(|n| n.as_str()),
        e.domain,
        to_json("event detail", &e.detail)?,
    ])?;
    Ok(())
}

fn apply_op(conn: &Connection, op: &PersistOp) -> Result<(), StoreError> {
    match op {
        PersistOp::UpsertNode(r) => {
            conn.prepare_cached(
                "INSERT INTO nodes(id, owner, ip, port, engine, state, record, health) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT(id) DO UPDATE SET owner = excluded.owner, ip = excluded.ip, \
                 port = excluded.port, engine = excluded.engine, state = excluded.state, \
                 record = excluded.record, health = excluded.health",
            )?
            .execute(params![
                r.id.as_str(),
                r.owner.0,
                r.ip.to_string(),
                r.port,
                enum_name(&r.engine)?,
                enum_name(&r.state)?,
                to_json("node record", r)?,
                to_json("node health", &r.health)?,
            ])?;
        }
        PersistOp::DeleteNode(id) => {
            conn.prepare_cached("DELETE FROM nodes WHERE id = ?1")?
                .execute([id.as_str()])?;
            conn.prepare_cached("DELETE FROM node_stats WHERE node = ?1")?
                .execute([id.as_str()])?;
            conn.prepare_cached("DELETE FROM tokens WHERE kind = 'node' AND node_id = ?1")?
                .execute([id.as_str()])?;
        }
        PersistOp::NodeHealth { node, health } => {
            conn.prepare_cached("UPDATE nodes SET health = ?2 WHERE id = ?1")?
                .execute(params![node.as_str(), to_json("node health", health)?])?;
        }
        PersistOp::NodeStats { node, stats } => {
            conn.prepare_cached(
                "INSERT INTO node_stats(node, stats) VALUES (?1, ?2) \
                 ON CONFLICT(node) DO UPDATE SET stats = excluded.stats",
            )?
            .execute(params![node.as_str(), to_json("node stats", stats)?])?;
        }
        PersistOp::Traffic {
            node,
            at,
            from_client,
            to_client,
            other_from_client,
            other_to_client,
        } => {
            // One row per node and hour, whatever the reporting interval.
            conn.prepare_cached(
                "INSERT INTO traffic(node, at, from_client, to_client, \
                 other_from_client, other_to_client) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(node, at) DO UPDATE SET \
                 from_client = from_client + excluded.from_client, \
                 to_client = to_client + excluded.to_client, \
                 other_from_client = other_from_client + excluded.other_from_client, \
                 other_to_client = other_to_client + excluded.other_to_client",
            )?
            .execute(params![
                node.as_str(),
                at.0 - at.0.rem_euclid(TRAFFIC_ROW_MS),
                int(*from_client),
                int(*to_client),
                int(*other_from_client),
                int(*other_to_client)
            ])?;
        }
        PersistOp::RecordRound {
            node,
            generation,
            at,
            verdict,
            votes,
        } => {
            conn.prepare_cached(
                "INSERT INTO sensor_rounds(node, generation, at, verdict) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![
                node.as_str(),
                int(generation.0),
                at.0,
                enum_name(verdict)?
            ])?;
            let round_id = conn.last_insert_rowid();
            let mut insert = conn.prepare_cached(
                "INSERT INTO sensor_votes(round_id, sensor, vote, data) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (sensor, vote) in votes {
                insert.execute(params![
                    round_id,
                    enum_name::<SensorKind>(sensor)?,
                    vote_name(&vote.vote),
                    to_json("sensor vote", vote)?,
                ])?;
            }
        }
        PersistOp::RecordBan {
            ip,
            node,
            at,
            votes,
            reverify,
            node_registered_at,
        } => {
            conn.prepare_cached(
                "INSERT INTO bans(ip, node, at, votes, reverify, node_registered_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                ip.to_string(),
                node.as_str(),
                at.0,
                to_json("ban votes", votes)?,
                reverify,
                node_registered_at.map(|t| t.0),
            ])?;
        }
        PersistOp::LiftBan { ip, at, admin } => {
            conn.prepare_cached(
                "UPDATE bans SET lifted_at = ?2, lifted_by_admin = ?3 \
                 WHERE ip = ?1 AND lifted_at IS NULL",
            )?
            .execute(params![ip.to_string(), at.0, admin])?;
        }
        PersistOp::UpsertIpBlock(b) => {
            conn.prepare_cached(
                "INSERT INTO ip_blocks(ip, node_id, blocked_at, next_reverify_at, owner) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(ip) DO UPDATE SET node_id = excluded.node_id, \
                 blocked_at = excluded.blocked_at, next_reverify_at = excluded.next_reverify_at, \
                 owner = excluded.owner",
            )?
            .execute(params![
                b.ip.to_string(),
                b.node_id.as_str(),
                b.blocked_at.0,
                b.next_reverify_at.map(|t| t.0),
                b.owner.map(|o| o.0),
            ])?;
        }
        PersistOp::DeleteIpBlock { ip } => {
            conn.prepare_cached("DELETE FROM ip_blocks WHERE ip = ?1")?
                .execute([ip.to_string()])?;
        }
        PersistOp::UpsertTombstone(t) => {
            conn.prepare_cached(
                "INSERT INTO tombstones(node_id, ip, strikes, until, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(node_id, ip) DO UPDATE SET strikes = excluded.strikes, \
                 until = excluded.until, updated_at = excluded.updated_at",
            )?
            .execute(params![
                t.node_id.as_str(),
                t.ip.to_string(),
                t.strikes,
                t.until.0,
                t.updated_at.0,
            ])?;
        }
        PersistOp::DeleteTombstone { node_id, ip } => {
            conn.prepare_cached("DELETE FROM tombstones WHERE node_id = ?1 AND ip = ?2")?
                .execute(params![node_id.as_str(), ip.to_string()])?;
        }
        PersistOp::SetMasters { domain, slots } => {
            conn.prepare_cached(
                "INSERT INTO assignments(domain, slots) VALUES (?1, ?2) \
                 ON CONFLICT(domain) DO UPDATE SET slots = excluded.slots",
            )?
            .execute(params![domain, to_json("master slots", slots)?])?;
        }
        PersistOp::DeleteMasters { domain } => {
            conn.prepare_cached("DELETE FROM assignments WHERE domain = ?1")?
                .execute([domain])?;
        }
        PersistOp::UpsertDns { domain, dns } => {
            conn.prepare_cached(
                "INSERT INTO dns_records(domain, dns) VALUES (?1, ?2) \
                 ON CONFLICT(domain) DO UPDATE SET dns = excluded.dns",
            )?
            .execute(params![domain, to_json("dns state", dns)?])?;
        }
        PersistOp::SetSrmd(state) => {
            // Client counts and tick counters are rebuilt from reports, not stored.
            let kept = SrmdState {
                created: state.created.clone(),
                cnames: state.cnames.clone(),
                ..SrmdState::default()
            };
            conn.prepare_cached(
                "INSERT INTO srmd_state(id, json) VALUES (1, ?1) \
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json",
            )?
            .execute([to_json("srmd state", &kept)?])?;
        }
        PersistOp::SetSettings(s) => {
            conn.prepare_cached(
                "INSERT INTO settings(id, json) VALUES (1, ?1) \
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json",
            )?
            .execute([to_json("settings", s)?])?;
        }
    }
    Ok(())
}
