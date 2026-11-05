//! Documents of the agent API under `/api/v1/agent`. Requests refuse fields they do not name:
//! the node an agent acts for always comes from its token, never from a body. The report and its
//! metrics are the exception: a newer agent may say more than this registrar knows, and refusing
//! it would take the node out of the pool for a number nobody here reads.

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
    /// The node that gave way to this one: another one on the same address or, if none, a silent
    /// node of the same owner that was holding the last place.
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

/// What the engine's metrics said. The first three numbers count the users the pool set up on
/// the node; the `other_*` ones count everyone else on the engine, that is the owner's own users.
/// A report without `other_*` comes from an older agent, or from one that does not yet know the
/// pool's users or has no per-user statistics; then the first three are all the engine has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MetricsReport {
    pub ok: bool,
    /// Cumulative octets from clients of the pool's users, as the engine exports them.
    pub from_client_total: u64,
    /// Cumulative octets to clients of the pool's users.
    pub to_client_total: u64,
    /// Clients of the pool's users now: the unique addresses the engine counts for them.
    pub clients: Option<u64>,
    /// Clients of the other users of the engine now, counted the same way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_clients: Option<u64>,
    /// Cumulative octets from clients of the other users. Present (0 included) whenever the agent
    /// tells the pool's users from the others.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_from_client_total: Option<u64>,
    /// Cumulative octets to clients of the other users. Present together with
    /// `other_from_client_total`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_to_client_total: Option<u64>,
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

/// The `error` of a report whose node refused the pool's configuration because a name is taken.
pub const ERROR_POOL_USER_TAKEN: &str = "pool_user_taken";

/// `POST /agent/report`. Fields this version does not name are ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
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
    /// What went wrong on the node, if something did. `pool_user_taken`: the owner holds a user
    /// under a name of the pool, with another secret, so the pool's configuration is not applied.
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
    /// This registrar takes the `other_*` fields of a report. A registrar that does not say so may
    /// refuse a report with them, so the agent then sums all the users of the engine into the
    /// plain fields, as older agents did.
    #[serde(default)]
    pub report_other_users: bool,
}

/// `POST /agent/update`: the agent says which version it runs and asks whether to update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdatePoll {
    /// The version the agent runs, `x.y.z` with an optional `-pre` part.
    #[schema(example = "0.1.0")]
    pub version: String,
}

/// The answer to `POST /agent/update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpdateOffer {
    /// The version to update to, or absent when the node is not asked to update. The agent
    /// fetches `/releases/manifest.json` and checks the signature itself; this is only the
    /// registrar's wish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "0.2.0")]
    pub version: Option<String>,
}

/// What happened to the update on the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOutcome {
    /// The agent began the switch; the engine is not touched.
    Started,
    /// The new version runs and has talked to the registrar.
    Updated,
    /// The update was refused before the switch (`code` says why); the old version still runs.
    Failed,
    /// The new version did not come up and the old one was put back.
    RolledBack,
}

/// `POST /agent/update-status`: the agent reports an update step. It is sent again until the
/// registrar answers, so a report may repeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateStatus {
    /// The version the update is about.
    #[schema(example = "0.2.0")]
    pub version: String,
    pub outcome: UpdateOutcome,
    /// Required with `failed`: a short code of 1 to 40 characters `a-z`, `0-9`, `_`
    /// (`bad_signature`, `bad_hash`, `downgrade`, `key_mismatch`, `download`, `start_failed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "bad_signature")]
    pub code: Option<String>,
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
    fn a_report_says_more_than_this_version_knows() {
        let metrics = json!({
            "ok": true, "from_client_total": 1, "to_client_total": 2, "clients": 3,
            "from_a_later_version": 4
        });
        let parsed: MetricsReport = serde_json::from_value(metrics).expect("metrics");
        assert_eq!(parsed.other_from_client_total, None);
        let report = json!({
            "ip": "192.0.2.1", "port": 443, "sni": "tls.example.test",
            "gp": { "kind": "no_data", "reason": "rate_limited" },
            "metrics": { "ok": true, "from_client_total": 1, "to_client_total": 2, "clients": 3 },
            "dc2": null, "host": null, "checked_at": "2026-11-03T12:00:00Z", "error": null,
            "from_a_later_version": 4
        });
        assert!(serde_json::from_value::<ReportRequest>(report).is_ok());
    }

    #[test]
    fn metrics_leave_out_what_the_agent_does_not_tell() {
        let old_shape = MetricsReport {
            ok: true,
            from_client_total: 1,
            to_client_total: 2,
            clients: None,
            other_clients: None,
            other_from_client_total: None,
            other_to_client_total: None,
        };
        assert_eq!(
            serde_json::to_value(&old_shape).unwrap(),
            json!({ "ok": true, "from_client_total": 1, "to_client_total": 2, "clients": null })
        );
    }

    #[test]
    fn a_config_without_the_report_capability_is_an_older_registrar() {
        let config = json!({
            "users": [], "sni": "tls.example.test", "port": 443, "antiscan": true,
            "intervals": {
                "heartbeat_sec": 15, "globalping_sec": 300, "metrics_sec": 60, "sync_sec": 60
            }
        });
        let parsed: AgentConfig = serde_json::from_value(config).expect("config");
        assert!(!parsed.report_other_users);
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
