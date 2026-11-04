//! The pool state and the vocabulary of `apply`: commands in, effects out.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dns::{DnsState, RecordValue};
use crate::health::NodeHealth;
use crate::lifecycle::{IpBlock, NodeRecord, RejectReason, Tombstone};
use crate::selection::{Assignments, Slot};
use crate::settings::Settings;
use crate::srmd::SrmdState;
use crate::traffic::TrafficCounters;
use crate::types::{EngineKind, Generation, NodeId, OwnerId, Ts};
use crate::votes::{RoundVerdict, SensorKind, SensorVote, Vote};

/// Everything the state owner keeps in memory. Maps are ordered: the same input must give the
/// same output.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoolState {
    pub settings: Settings,
    pub nodes: BTreeMap<NodeId, NodeRecord>,
    pub stats: BTreeMap<NodeId, NodeStats>,
    pub blocks: Vec<IpBlock>,
    pub tombstones: Vec<Tombstone>,
    pub assignments: Assignments,
    pub dns: BTreeMap<String, DnsState>,
    pub srmd: SrmdState,
    /// No enrollment evicts a silent node before this time. Heartbeats are written in batches, so
    /// after a restart the last ones are missing and a live node would look silent. Not stored:
    /// the state owner sets it on start.
    pub evictions_from: Option<Ts>,
}

impl PoolState {
    pub fn new(settings: Settings) -> PoolState {
        PoolState {
            settings,
            ..PoolState::default()
        }
    }
}

/// What an agent reports besides the health conditions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeStats {
    /// The last cumulative counters seen: the base of the next delta.
    pub counters: Option<TrafficCounters>,
    /// Bytes counted since registration.
    pub from_client_total: u64,
    pub to_client_total: u64,
    /// Clients connected to the node, as the last report said.
    pub clients: Option<u64>,
    /// Uptime of the last 30 days in hundredths of a percent (9997 is 99.97 %). Kept in
    /// memory only: the registrar recomputes it from the stored samples.
    #[serde(skip)]
    pub availability_bp: Option<u32>,
}

// ---- commands ----

/// Correlates a command with its `Effect::Reply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReplyId(pub u64);

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Enroll(EnrollRequest),
    Heartbeat(HeartbeatRequest),
    Report(ReportRequest),
    /// The result of one TCP probe from outside.
    TcpProbe {
        node: NodeId,
        generation: Generation,
        open: bool,
    },
    /// The votes of a sensor round that `Effect::RunRound` started.
    RoundDone {
        node: NodeId,
        generation: Generation,
        votes: Vec<(SensorKind, SensorVote)>,
    },
    /// The outcome of `Effect::PushDns`: `Err` carries the provider's message.
    DnsDone {
        domain: String,
        generation: Generation,
        result: Result<(), String>,
    },
    /// The node (or its owner) takes it out of the pool.
    Retire(NodeAction),
    AdminUnban(NodeAction),
    /// An extra sensor round outside the schedule. The caller limits how often.
    Recheck(NodeAction),
    SrmdManual(SrmdManualRequest),
    /// The 30-day availability of the nodes that have samples, in hundredths of a percent.
    /// A node missing from the list has no samples and so no value; a name that is not in the
    /// pool is ignored. Not tied to a generation: it describes the past.
    AvailabilityRefreshed(Vec<(NodeId, u32)>),
    /// The admin asks for every DNS record to be written again: they all count as not
    /// applied and their backoff starts over. A record being pushed is left alone.
    DnsPushAll,
    /// Only valid settings are applied; the caller validates and reports the error.
    SettingsChanged(Settings),
    Tick,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollRequest {
    pub reply: ReplyId,
    pub node_id: NodeId,
    pub owner: OwnerId,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub engine: EngineKind,
    /// How many nodes the owner may have; `None`: any number.
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatRequest {
    pub reply: ReplyId,
    pub node: NodeId,
    /// The address the request came from. Another one than registered is an IP change.
    pub ip: Ipv4Addr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportRequest {
    pub reply: ReplyId,
    pub node: NodeId,
    /// The node generation the registrar verified the measurement under.
    pub generation: Generation,
    pub metrics_ok: bool,
    /// The verified Globalping result: `Clear` is a success, `Blocked` a failure, `Abstain`
    /// no data (a 429 or a measurement that could not be verified).
    pub globalping: Vote,
    pub traffic: Option<TrafficCounters>,
    pub clients: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAction {
    pub reply: ReplyId,
    pub node: NodeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrmdManualRequest {
    pub reply: ReplyId,
    pub domain: String,
    pub action: SrmdManualAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrmdManualAction {
    Take,
    Release,
}

// ---- effects ----

#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Persist(PersistOp),
    /// Start a sensor round. `fresh_gp` is the Globalping vote of a recent verified agent
    /// report: when it is there the worker does not spend a measurement of its own.
    RunRound {
        node: NodeId,
        generation: Generation,
        ip: Ipv4Addr,
        port: u16,
        fresh_gp: Option<SensorVote>,
    },
    PushDns {
        domain: String,
        generation: Generation,
        desired: RecordValue,
    },
    Emit(Event),
    Reply(ReplyId, Reply),
}

/// How soon a write has to reach the disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// Registrations, bans, assignments, DNS and settings: written at once.
    Immediate,
    /// Heartbeats, reports and counters: written in batches. Losing a few seconds of them
    /// in a crash is harmless.
    Batched,
}

/// One write per entity: the store upserts or deletes a row and never gets a whole snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum PersistOp {
    UpsertNode(NodeRecord),
    /// The record, its stats and its node token go. Samples, rounds, bans and events stay.
    DeleteNode(NodeId),
    NodeHealth {
        node: NodeId,
        health: NodeHealth,
    },
    NodeStats {
        node: NodeId,
        stats: NodeStats,
    },
    /// Bytes since the previous report; the store adds them to its aggregates.
    Traffic {
        node: NodeId,
        at: Ts,
        from_client: u64,
        to_client: u64,
    },
    /// A judged sensor round with the evidence of every vote.
    RecordRound {
        node: NodeId,
        generation: Generation,
        at: Ts,
        verdict: RoundVerdict,
        votes: Vec<(SensorKind, SensorVote)>,
    },
    /// A ban for the `bans` table: the votes that confirmed it, or the votes of the reverify
    /// round that failed to lift it.
    RecordBan {
        ip: Ipv4Addr,
        node: NodeId,
        at: Ts,
        votes: Vec<(SensorKind, Vote)>,
        reverify: bool,
    },
    /// The ban of an IP ended: a clean reverify round, or an admin.
    LiftBan {
        ip: Ipv4Addr,
        at: Ts,
        admin: bool,
    },
    UpsertIpBlock(IpBlock),
    DeleteIpBlock {
        ip: Ipv4Addr,
    },
    UpsertTombstone(Tombstone),
    DeleteTombstone {
        node_id: NodeId,
        ip: Ipv4Addr,
    },
    SetMasters {
        domain: String,
        slots: Vec<Slot>,
    },
    DeleteMasters {
        domain: String,
    },
    UpsertDns {
        domain: String,
        dns: DnsState,
    },
    /// The domains SRMD owns and the CNAMEs it set. Client counts and tick counters are not
    /// stored: they are rebuilt from reports.
    SetSrmd(SrmdState),
    SetSettings(Settings),
}

impl PersistOp {
    pub fn durability(&self) -> Durability {
        match self {
            PersistOp::NodeHealth { .. }
            | PersistOp::NodeStats { .. }
            | PersistOp::Traffic { .. } => Durability::Batched,
            _ => Durability::Immediate,
        }
    }
}

// ---- events ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    NodeRegistered,
    NodeQuarantined,
    NodeRecovered,
    NodeBanned,
    /// A reverify round did not prove the IP clean. For the event log of the panel only: the
    /// agent registers again every few minutes, a webhook for each would be noise.
    NodeReverifyFailed,
    /// A heartbeat came from another address and the node starts over there. For the event log
    /// of the panel only.
    NodeIpChanged,
    NodeDead,
    NodePruned,
    BanLifted,
    MasterChanged,
    SensorRound,
    DnsApplied,
    DnsFailed,
    SrmdAction,
    /// `apply` never emits these three: the registrar raises them.
    ClockSkew,
    StoreError,
    RolloutChanged,
}

impl EventKind {
    /// The webhook event name, for the kinds that have one.
    pub fn webhook_name(self) -> Option<&'static str> {
        Some(match self {
            EventKind::NodeRegistered => "node.registered",
            EventKind::NodeQuarantined => "node.quarantined",
            EventKind::NodeRecovered => "node.recovered",
            EventKind::NodeBanned => "node.banned",
            EventKind::NodeDead => "node.dead",
            EventKind::NodePruned => "node.pruned",
            EventKind::BanLifted => "ban.lifted",
            EventKind::MasterChanged => "master.changed",
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub kind: EventKind,
    pub at: Ts,
    pub node: Option<NodeId>,
    /// The owner of `node`, when the event was made with its record in hand. The record may be
    /// gone by the time a worker handles the event: a ban removes it in the same `apply`.
    pub owner: Option<OwnerId>,
    pub domain: Option<String>,
    pub detail: Value,
}

// ---- replies ----

/// The answer to a command, mapped to an HTTP status by the registrar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// 200 with the node record created.
    Enrolled {
        node: NodeId,
        generation: Generation,
        /// The node that gave way: another one on the same IP or, if none, a silent node of the
        /// owner that was holding the last place.
        replaced: Option<NodeId>,
        /// The IP was blocked: a decisive round decides.
        reverify: bool,
    },
    /// 429 with `Retry-After`.
    /// The owner is at its limit and has no silent node to give way.
    OwnerLimit {
        limit: i64,
        current: usize,
    },
    EnrollRejected {
        retry_after_sec: u32,
        reason: RejectReason,
    },
    Ok,
    /// 403 terminate: the node is out of the pool.
    Terminate(TerminateReason),
    /// 410: unknown node, enroll again.
    UnknownNode,
    /// 409: the report belongs to an older generation.
    Stale,
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateReason {
    Banned,
    Dead,
    Pruned,
    /// The node moved to an IP that may not enroll: it has to enroll again.
    AddressRefused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    NotBanned,
    /// No sensor round is due for the node in its state.
    NotRecheckable,
    UnknownDomain,
    AlreadyManaged,
    NotManaged,
}
