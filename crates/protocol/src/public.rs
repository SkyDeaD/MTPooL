//! Documents of the public read-only API. Timestamps are RFC 3339 strings in UTC. Nothing
//! here carries a full node address or a measurement id.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Pending,
    Healthy,
    Quarantine,
    Banned,
    Dead,
    Pruned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Telemt,
    MtProxyL,
    Meko,
}

/// What one sensor said in a round.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Vote {
    Blocked,
    Clear,
    Abstain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Confirmed,
    Clean,
    Undecided,
}

/// A node of the pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PublicNode {
    #[schema(example = "alpha-a1b2c")]
    pub id: String,
    pub state: NodeState,
    pub engine: Engine,
    /// The address with the two middle octets hidden.
    #[schema(example = "203.x.x.113")]
    pub ip: String,
    pub port: u16,
    /// RFC 3339, UTC.
    pub registered_at: String,
    /// When the node entered its current state. RFC 3339, UTC.
    pub state_since: String,
    /// Whether the node is the master of at least one domain.
    pub is_master: bool,
    /// The domains the node is master of.
    pub domains: Vec<String>,
    /// The latest sensor round. Only in the answer for a single node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_round: Option<LastRound>,
}

/// The latest sensor round of a node. Evidence and measurement ids are not published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LastRound {
    /// RFC 3339, UTC.
    pub at: String,
    pub verdict: Verdict,
    pub votes: Vec<LastVote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LastVote {
    #[schema(example = "globalping")]
    pub sensor: String,
    pub vote: Vote,
    /// Why the sensor abstained: `not_configured`, `unavailable`, `rate_limited`, `timeout`,
    /// `unknown_answer` or `no_data`. Only for `abstain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum DnsRecordType {
    A,
    #[serde(rename = "CNAME")]
    Cname,
}

/// What the domain is set to resolve to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DnsRecord {
    #[serde(rename = "type")]
    pub kind: DnsRecordType,
    /// Addresses of an `A` record, with the two middle octets hidden, or the one target of a
    /// `CNAME`.
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PublicDomain {
    #[schema(example = "proxy.example.com")]
    pub domain: String,
    /// The node ids that are masters of the domain.
    pub masters: Vec<String>,
    /// `null` while the domain has no DNS state yet.
    pub dns: Option<DnsRecord>,
}

/// A Telegram proxy link of one user on one domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProxyLink {
    /// `<user>@<domain>`.
    #[schema(example = "alice@proxy.example.com")]
    pub id: String,
    pub domain: String,
    pub user: String,
    /// The `tg://proxy?...` link.
    pub link: String,
    /// Path of the QR code of the link (SVG).
    #[schema(example = "/api/v1/links/alice@proxy.example.com/qr.svg")]
    pub qr_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BanVote {
    pub sensor: String,
    pub vote: Vote,
}

/// One entry of the ban log. `reverify` entries record a failed attempt to lift a ban, not a
/// new ban: they carry no `lifetime_sec` and no `gap_sec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BanRecord {
    /// Pass the id of the last entry as `before` to get the next page.
    pub id: i64,
    /// The address with the two middle octets hidden.
    #[schema(example = "203.x.x.113")]
    pub ip: String,
    pub node_id: String,
    /// RFC 3339, UTC.
    pub at: String,
    /// The votes that confirmed the ban.
    pub votes: Vec<BanVote>,
    pub reverify: bool,
    pub lifted_at: Option<String>,
    pub lifted_by_admin: bool,
    /// Seconds from the registration of the node to the ban; `null` once the node is gone.
    pub lifetime_sec: Option<i64>,
    /// Seconds since the previous ban in the pool; `null` for the first one.
    pub gap_sec: Option<i64>,
}

/// The windows the statistics can be asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum StatsRange {
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
    #[serde(rename = "30d")]
    Month,
}

impl StatsRange {
    pub fn parse(text: &str) -> Option<StatsRange> {
        match text {
            "24h" => Some(StatsRange::Day),
            "7d" => Some(StatsRange::Week),
            "30d" => Some(StatsRange::Month),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StatsRange::Day => "24h",
            StatsRange::Week => "7d",
            StatsRange::Month => "30d",
        }
    }

    pub fn seconds(self) -> i64 {
        match self {
            StatsRange::Day => 24 * 3600,
            StatsRange::Week => 7 * 24 * 3600,
            StatsRange::Month => 30 * 24 * 3600,
        }
    }

    /// The length of one bucket of the series.
    pub fn step_sec(self) -> i64 {
        match self {
            StatsRange::Day => 3600,
            StatsRange::Week => 6 * 3600,
            StatsRange::Month => 24 * 3600,
        }
    }
}

/// Numbers of one stretch of time: the whole range (`kpi`) or one bucket of the series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StatsPoint {
    /// Bans that began in the stretch.
    pub bans: i64,
    /// Mean seconds from the registration to the ban over those of them whose node is still
    /// known; `null` when there is none.
    pub avg_lifetime_sec: Option<i64>,
    /// Mean seconds since the previous ban over those bans that have a previous one; `null`
    /// when there is none.
    pub avg_gap_sec: Option<i64>,
    /// Bytes from clients to the nodes.
    pub traffic_ingress_bytes: i64,
    /// Bytes from the nodes to clients.
    pub traffic_egress_bytes: i64,
    pub traffic_total_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StatsBucket {
    /// RFC 3339, UTC: the start of the bucket.
    pub start_ts: String,
    #[serde(flatten)]
    pub point: StatsPoint,
}

/// A ban in the activity feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RecentBan {
    /// RFC 3339, UTC.
    pub ts: String,
    pub node_id: String,
    #[schema(example = "203.x.x.113")]
    pub ip_masked: String,
    pub lifetime_sec: Option<i64>,
    pub gap_sec: Option<i64>,
}

/// Totals over a range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StatsSummary {
    pub range: StatsRange,
    /// RFC 3339, UTC: when the figures were computed.
    pub now: String,
    /// RFC 3339, UTC: the start of the range, aligned down to a step.
    pub from: String,
    pub step_sec: i64,
    pub kpi: StatsPoint,
    /// Bans by the sensors that confirmed them: the names of the sensors that voted
    /// `blocked`, sorted and joined by `+` (`cheburcheck+globalping`).
    pub by_reason: BTreeMap<String, i64>,
    /// The last 30 bans, newest first.
    pub recent: Vec<RecentBan>,
    /// Whether the stored history reaches back to `from` (traffic is kept for 35 days).
    pub history_ok: bool,
}

/// Figures per bucket over a range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StatsSeries {
    pub range: StatsRange,
    pub now: String,
    pub from: String,
    pub step_sec: i64,
    /// From `from` on, `step_sec` apart; the last one is still running.
    pub buckets: Vec<StatsBucket>,
    pub history_ok: bool,
}
