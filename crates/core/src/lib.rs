//! Pure pool logic without I/O.

pub mod dns;
pub mod health;
pub mod lifecycle;
pub mod links;
pub mod selection;
pub mod settings;
pub mod srmd;
pub mod types;
pub mod votes;

pub use dns::{
    DnsOp, DnsPlanError, DnsState, LiveRecord, NewRecord, RecordKind, RecordValue, backoff,
    plan_changes,
};
pub use health::{FailedCondition, GpStatus, Latch, NodeHealth, failed_conditions};
pub use lifecycle::{
    EnrollDecision, EnrollInput, IpBlock, LifecycleEvent, NodeRecord, RejectReason, RoundResult,
    Tombstone, apply_round, decide_enroll, is_alive, needs_sensors, next_round_at, record_prune,
    reset_tombstones, tick_node, unban,
};
pub use links::build_proxy_link;
pub use selection::{Assignments, MasterChange, SelectionResult, Slot, queue_order, select};
pub use settings::{GlobalpingSettings, ProxyUser, Settings, SettingsError, SrmdSettings};
pub use srmd::{
    SrmdAction, SrmdError, SrmdState, rebalance, release, required_domains, set_clients, take,
};
pub use types::{
    EngineKind, Generation, IdError, NodeId, NodeState, OwnerId, Ts, is_public_ipv4, mask_ip,
};
pub use votes::{AbstainReason, RoundVerdict, SensorKind, SensorVote, Vote, judge_round};
