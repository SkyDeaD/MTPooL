//! Writes of agent reports. Immediate, like the auth writes: the id of a measurement is on
//! disk before the next report is looked at.

use mtpool_core::{NodeId, Ts};
use rusqlite::{Connection, OptionalExtension, params};

use super::StoreError;

/// Measurement ids older than this are forgotten: the clock window of a report and the age
/// limit of the verifier are both far shorter.
pub const MEASUREMENT_KEEP_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportOp {
    /// Remembers the id and forgets the ones past `MEASUREMENT_KEEP_MS`. An id that is
    /// there already is left as it is.
    RecordMeasurement { id: String, node: NodeId, at: Ts },
}

pub(super) fn name(op: &ReportOp) -> &'static str {
    match op {
        ReportOp::RecordMeasurement { .. } => "record_measurement",
    }
}

pub(super) fn apply(conn: &Connection, op: &ReportOp) -> Result<(), StoreError> {
    match op {
        ReportOp::RecordMeasurement { id, node, at } => {
            conn.prepare_cached("DELETE FROM gp_measurements WHERE at < ?1")?
                .execute([at.0.saturating_sub(MEASUREMENT_KEEP_MS)])?;
            conn.prepare_cached(
                "INSERT OR IGNORE INTO gp_measurements(id, node, at) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![id, node.as_str(), at.0])?;
        }
    }
    Ok(())
}

/// Whether the id was recorded.
pub fn measurement_seen(conn: &Connection, id: &str) -> Result<bool, StoreError> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM gp_measurements WHERE id = ?1")?
        .query_row([id], |_| Ok(()))
        .optional()?
        .is_some())
}
