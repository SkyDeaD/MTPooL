//! Documents of the agent API under `/api/v1/agent`. Requests refuse fields they do not name:
//! the node an agent acts for always comes from its token, never from a body.

use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub use crate::public::Engine;

/// `POST /agent/enroll`. The address of the node is the one the request comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrollRequest {
    /// 1 to 32 letters, digits and `._-`, starting and ending with a letter or digit. The
    /// registrar adds `-` and five random characters to make the node id.
    #[schema(example = "alpha")]
    pub name: String,
    /// The port the proxy listens on.
    #[schema(example = 443)]
    pub port: u16,
    pub engine: Engine,
}

/// The answer to a successful enrollment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnrollResponse {
    #[schema(example = "alpha-a1b2c")]
    pub node_id: String,
    /// The bearer token of the node. Shown here once; only its hash is kept.
    pub node_token: String,
    pub generation: u64,
    /// Another node on the same address that this one displaced.
    pub replaced: Option<String>,
    /// The address was blocked: a decisive check decides whether the node may stay.
    pub reverify: bool,
}

/// `POST /agent/heartbeat` and `POST /agent/retire`: nothing to say. An empty body is fine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyRequest {}

/// An answer with nothing in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Empty {}

/// What the agent says about its Globalping check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GpReport {
    /// A measurement was made; the registrar verifies it under its own key.
    Measured {
        #[schema(example = "abc123")]
        id: String,
    },
    /// No measurement this time (a 429 or an outage of the service). Not a failure.
    NoData {
        /// `rate_limited`, `unavailable`, `timeout`; anything else counts as no data.
        #[schema(example = "rate_limited")]
        reason: String,
    },
}

/// What the engine's metrics said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MetricsReport {
    pub ok: bool,
    /// Cumulative octets from clients, as the engine exports them.
    pub from_client_total: u64,
    /// Cumulative octets to clients.
    pub to_client_total: u64,
    /// Clients connected now.
    pub clients: Option<u64>,
}

/// The check of the second Telegram data center.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Dc2Report {
    pub ok: bool,
    pub reason: Option<String>,
}

/// The host the node runs on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HostReport {
    pub load1: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub disk_total_bytes: u64,
}

/// `POST /agent/report`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportRequest {
    /// The address the measurement was made against. Must be the registered one.
    #[schema(value_type = String, format = "ipv4", example = "203.0.113.7")]
    pub ip: Ipv4Addr,
    pub port: u16,
    /// The TLS domain the measurement used.
    #[schema(example = "tls.example.com")]
    pub sni: String,
    pub gp: GpReport,
    pub metrics: MetricsReport,
    pub dc2: Option<Dc2Report>,
    pub host: Option<HostReport>,
    /// When the agent made the checks, RFC 3339. Must be within 10 minutes before and one
    /// minute after the registrar's clock.
    #[schema(example = "2026-11-03T12:00:00Z")]
    pub checked_at: String,
    /// What went wrong on the node, if something did.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AgentUser {
    pub name: String,
    /// 32 hex characters.
    pub secret_hex: String,
}

/// How often the agent does each of its jobs, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Intervals {
    pub heartbeat_sec: u32,
    pub globalping_sec: u32,
    pub metrics_sec: u32,
    pub sync_sec: u32,
}

/// `GET /agent/config`: what the node's proxy has to be set up with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AgentConfig {
    pub users: Vec<AgentUser>,
    /// The TLS domain the proxy pretends to be.
    pub sni: String,
    pub port: u16,
    pub intervals: Intervals,
    /// Whether the agent defends the port against active probing.
    pub antiscan: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminateReason {
    Banned,
    Dead,
    Pruned,
    AddressRefused,
}

/// The body of a 403 to a node that is out of the pool: it stops its proxy and, if the reason
/// allows, enrolls again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Terminate {
    pub terminate: bool,
    pub reason: TerminateReason,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_refuse_unknown_fields() {
        let enroll = json!({ "name": "a", "port": 443, "engine": "telemt", "owner": 1 });
        assert!(serde_json::from_value::<EnrollRequest>(enroll).is_err());
        assert!(serde_json::from_value::<EmptyRequest>(json!({ "node": "a-00001" })).is_err());
        assert!(serde_json::from_value::<EmptyRequest>(json!({})).is_ok());
    }

    #[test]
    fn gp_report_is_tagged_by_kind() {
        let measured: GpReport =
            serde_json::from_value(json!({ "kind": "measured", "id": "x" })).expect("measured");
        assert_eq!(measured, GpReport::Measured { id: "x".into() });
        let none: GpReport =
            serde_json::from_value(json!({ "kind": "no_data", "reason": "rate_limited" }))
                .expect("no data");
        assert_eq!(
            none,
            GpReport::NoData {
                reason: "rate_limited".into()
            }
        );
    }

    #[test]
    fn terminate_names_its_reason_in_snake_case() {
        let t = Terminate {
            terminate: true,
            reason: TerminateReason::AddressRefused,
            message: "m".into(),
        };
        assert_eq!(
            serde_json::to_value(t).unwrap()["reason"],
            "address_refused"
        );
    }
}
