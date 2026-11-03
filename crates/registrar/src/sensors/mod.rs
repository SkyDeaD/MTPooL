//! Sensors: independent witnesses that tell whether a node is reachable from where its users
//! are. Each one answers with a `SensorVote`; the ban rule that combines them lives in `core`.

use std::fmt;
use std::net::Ipv4Addr;

use async_trait::async_trait;
use mtpool_core::{SensorKind, SensorVote};

pub mod cheburcheck;
pub mod globalping;
pub mod redflag;
pub mod round;

/// What a sensor looks at: one node.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    pub ip: Ipv4Addr,
    pub port: u16,
    /// The domain the node's fake-TLS handshake is masked as.
    pub sni: String,
    /// The pool's public secret in link format. Sensors that speak MTProto use it; Globalping
    /// ignores it.
    pub secret: String,
}

// The secret is not a credential, but it stays out of logs all the same.
impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Target")
            .field("ip", &self.ip)
            .field("port", &self.port)
            .field("sni", &self.sni)
            .field("secret", &"<hidden>")
            .finish()
    }
}

#[async_trait]
pub trait Sensor: Send + Sync {
    fn kind(&self) -> SensorKind;

    /// One vote about the target. Never fails: whatever goes wrong is an `Abstain` with the
    /// reason in the evidence.
    async fn vote(&self, target: &Target) -> SensorVote;
}
