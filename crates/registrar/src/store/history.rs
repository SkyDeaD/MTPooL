//! Writes of the client history. Immediate, like the others: one transaction per sample.

use mtpool_core::Ts;
use rusqlite::{Connection, params};

use super::StoreError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryOp {
    /// The rows of one moment: the total under the empty domain and one per domain. A moment
    /// that is there already is replaced.
    ClientSamples { at: Ts, rows: Vec<(String, u64)> },
}

pub(super) fn name(op: &HistoryOp) -> &'static str {
    match op {
        HistoryOp::ClientSamples { .. } => "client_samples",
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
    }
    Ok(())
}
