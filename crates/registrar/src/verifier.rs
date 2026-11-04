//! Checking the Globalping measurement an agent names in its report.
//!
//! The agent does not say what the measurement found, it says which measurement it made. The
//! registrar fetches that measurement under its own key and checks it was made against this
//! node, so a node cannot vouch for itself.

use std::future::Future;
use std::pin::Pin;

use mtpool_core::{SensorVote, Ts};

/// What the measurement has to match: the node as the registrar knows it, and when the agent
/// said it measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpExpect {
    pub ip: std::net::Ipv4Addr,
    pub port: u16,
    pub sni: String,
    pub reported_at: Ts,
}

/// A measurement that cannot be used. The report counts as no data, never as a failure of the
/// node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GpRejected {
    #[error("the measurement service cannot be reached or did not answer")]
    Unavailable,
    #[error("no such measurement")]
    UnknownMeasurement,
    #[error("the measurement was made against something else")]
    Mismatch,
    #[error("the measurement is too old")]
    Stale,
}

pub type VerifyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<SensorVote, GpRejected>> + Send + 'a>>;

/// Verifies a measurement by id. Object safe, so the registrar holds it as `Arc<dyn _>`; an
/// implementation writes `fn verify<'a>(..) -> VerifyFuture<'a> { Box::pin(async move { .. }) }`.
pub trait MeasurementVerifier: Send + Sync + 'static {
    fn verify<'a>(&'a self, id: &'a str, expect: &'a GpExpect) -> VerifyFuture<'a>;
}

/// The verifier until a real one is plugged in: nothing can be verified.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoVerifier;

impl MeasurementVerifier for NoVerifier {
    fn verify<'a>(&'a self, _id: &'a str, _expect: &'a GpExpect) -> VerifyFuture<'a> {
        Box::pin(async { Err(GpRejected::Unavailable) })
    }
}
