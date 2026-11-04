//! Writes of the client history and the node availability history. Immediate, like the
//! others: one transaction per sample.

use mtpool_core::{NodeId, NodeSample, Ts};
use rusqlite::{Connection, params};

use super::StoreError;

/// The length of one availability bucket, in seconds.
pub const BUCKET_SEC: i64 = 900;
/// The window availability is counted over: 30 days.
const AVAILABILITY_BUCKETS: i64 = 30 * 24 * 3600 / BUCKET_SEC;

/// The bucket a moment falls into: Unix time divided by the bucket length.
pub(super) fn bucket_of(at: Ts) -> i64 {
    at.0.div_euclid(1000).div_euclid(BUCKET_SEC)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryOp {
    /// The rows of one moment: the total under the empty domain and one per domain. A moment
    /// that is there already is replaced.
    ClientSamples { at: Ts, rows: Vec<(String, u64)> },
    /// One tick of the nodes: each adds one to `total` and, when up, to `up` in the bucket of
    /// `at`. The highest client count of the bucket is kept.
    NodeSamples { at: Ts, rows: Vec<NodeSample> },
}

pub(super) fn name(op: &HistoryOp) -> &'static str {
    match op {
        HistoryOp::ClientSamples { .. } => "client_samples",
        HistoryOp::NodeSamples { .. } => "node_samples",
    }
}

pub(super) fn apply(conn: &Connection, op: &HistoryOp) -> Result<(), StoreError> {
    match op {
        HistoryOp::ClientSamples { at, rows } => {
            let mut insert = conn.prepare_cached(
                "INSERT OR REPLACE INTO client_samples(at, domain, clients) VALUES (?1, ?2, ?3)",
            )?;
            for (domain, clients) in rows {
                // SQLite integers are signed: a count past that is capped, not wrapped.
                let clients = i64::try_from(*clients).unwrap_or(i64::MAX);
                insert.execute(params![at.0, domain, clients])?;
            }
        }
        HistoryOp::NodeSamples { at, rows } => {
            let mut upsert = conn.prepare_cached(
                "INSERT INTO node_samples(node, bucket, up, total, clients) \
                 VALUES (?1, ?2, ?3, 1, ?4) \
                 ON CONFLICT(node, bucket) DO UPDATE SET \
                     up = up + excluded.up, \
                     total = total + 1, \
                     clients = max(clients, excluded.clients)",
            )?;
            let bucket = bucket_of(*at);
            for row in rows {
                let clients = row
                    .clients
                    .map_or(0, |n| i64::try_from(n).unwrap_or(i64::MAX));
                upsert.execute(params![
                    row.node.as_str(),
                    bucket,
                    i64::from(row.up),
                    clients
                ])?;
            }
        }
    }
    Ok(())
}

/// One bucket of one node, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBucket {
    /// Start of the bucket, Unix milliseconds.
    pub start: Ts,
    pub up: u32,
    pub total: u32,
    pub clients: u64,
}

/// The 30-day availability of every node that has samples, in hundredths of a percent
/// (rounded down, so a node that was ever down never shows 100 %). A node without samples is
/// not in the list. Rows of an id that no longer parses are skipped.
pub fn availability(conn: &Connection, now: Ts) -> Result<Vec<(NodeId, u32)>, StoreError> {
    let first = bucket_of(now) - AVAILABILITY_BUCKETS + 1;
    let mut stmt = conn.prepare_cached(
        "SELECT node, SUM(up), SUM(total) FROM node_samples \
         WHERE bucket >= ?1 GROUP BY node HAVING SUM(total) > 0",
    )?;
    let rows = stmt.query_map([first], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (node, up, total) = row?;
        let Ok(node) = NodeId::parse(&node) else {
            continue;
        };
        let bp = up.clamp(0, total) * 10_000 / total;
        out.push((node, u32::try_from(bp).unwrap_or(10_000)));
    }
    Ok(out)
}

/// The last `count` buckets of a node that have samples, oldest first. A bucket without a
/// sample is missing, not zero.
pub fn node_history(
    conn: &Connection,
    node: &NodeId,
    now: Ts,
    count: i64,
) -> Result<Vec<NodeBucket>, StoreError> {
    let first = bucket_of(now) - count + 1;
    let mut stmt = conn.prepare_cached(
        "SELECT bucket, up, total, clients FROM node_samples \
         WHERE node = ?1 AND bucket >= ?2 ORDER BY bucket",
    )?;
    let rows = stmt.query_map(params![node.as_str(), first], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (bucket, up, total, clients) = row?;
        out.push(NodeBucket {
            start: Ts(bucket * BUCKET_SEC * 1000),
            up: u32::try_from(up).unwrap_or(0),
            total: u32::try_from(total).unwrap_or(0),
            clients: u64::try_from(clients).unwrap_or(0),
        });
    }
    Ok(out)
}
