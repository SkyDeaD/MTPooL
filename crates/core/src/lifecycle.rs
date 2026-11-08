//! Node lifecycle: quarantine, ban, reverify, prune and enrollment decisions.
//!
//! Pure state transitions. Time is a parameter, nothing here waits or talks to the network.
//! The transitions follow section 4 of the spec.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use crate::health::{
    FailedCondition, GpStatus, Latch, NodeHealth, failed_conditions, younger_than,
};
use crate::settings::Settings;
use crate::types::{EngineKind, Generation, NodeId, NodeState, OwnerId, Ts};
use crate::votes::{RoundVerdict, SensorKind, Vote, judge_round};

/// Pauses before a pruned node or IP may enroll again: 15 min, 30 min, 1 h, 2 h, 3 h. After
/// the last one the pause stays at 3 h.
const PRUNE_PAUSES_SEC: [u32; 5] = [900, 1800, 3600, 7200, 10_800];
/// A tombstone is forgotten this long after its last update.
const TOMBSTONE_TTL_MS: i64 = 7 * 24 * 3600 * 1000;
/// A `Dead` or `Pruned` record is removed this long after it entered that state.
const FINAL_RECORD_TTL_MS: i64 = 24 * 3600 * 1000;
/// Retry hint while a reverify round for the same IP is running.
const REVERIFY_IN_FLIGHT_RETRY_SEC: u32 = 60;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: NodeId,
    pub owner: OwnerId,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub engine: EngineKind,
    pub state: NodeState,
    /// Bumps on IP change, replacement and unban. A worker result carrying another value
    /// is stale.
    pub generation: Generation,
    pub health: NodeHealth,
    pub registered_at: Ts,
    /// Entered the master queue (became `Healthy`). Orders the queue together with
    /// `registered_at` and the node id.
    pub queued_at: Ts,
    /// When the node entered its current state. In `Quarantine` it starts the 60 minute
    /// prune timer.
    pub state_since: Ts,
    /// Since when the node is continuously not alive; `None` while it is alive.
    pub not_alive_since: Option<Ts>,
    pub last_round_at: Option<Ts>,
    /// Rounds without progress in this quarantine: they set the round interval.
    pub undecided_rounds: u32,
    /// Counted clean rounds in a row.
    pub clean_rounds: u32,
    /// This quarantine has entered the sensor path: the node was alive while its Globalping
    /// check was unproven (unknown, failed or aged out). Stays set until the node leaves
    /// `Quarantine`; only counted clean rounds, not a fresh measurement, end it.
    pub suspected: bool,
    /// Why the node entered `Quarantine`; empty in every other state.
    #[serde(default)]
    pub quarantine_reasons: Vec<FailedCondition>,
    /// Registered from a blocked IP: a decisive round must lift the block before the node
    /// may proceed.
    pub reverify: bool,
}

impl NodeRecord {
    /// A freshly enrolled node: `Pending`, latches down, no measurement yet.
    pub fn new(
        id: NodeId,
        owner: OwnerId,
        ip: Ipv4Addr,
        port: u16,
        engine: EngineKind,
        now: Ts,
        s: &Settings,
    ) -> NodeRecord {
        NodeRecord {
            id,
            owner,
            ip,
            port,
            engine,
            state: NodeState::Pending,
            generation: Generation(0),
            health: NodeHealth {
                tcp: Latch::new(),
                metrics: Latch::new(),
                gp: GpStatus::Unknown,
                last_heartbeat: now,
                last_report: Ts(0),
                port_ok: port == s.proxy_port,
                pool_user_taken: false,
                ru_confirmed_at: None,
                port_filter: false,
            },
            registered_at: now,
            queued_at: now,
            state_since: now,
            not_alive_since: None,
            last_round_at: None,
            undecided_rounds: 0,
            clean_rounds: 0,
            suspected: false,
            quarantine_reasons: Vec::new(),
            reverify: false,
        }
    }

    /// A node enrolled from a blocked IP.
    pub fn new_reverify(
        id: NodeId,
        owner: OwnerId,
        ip: Ipv4Addr,
        port: u16,
        engine: EngineKind,
        now: Ts,
        s: &Settings,
    ) -> NodeRecord {
        NodeRecord {
            reverify: true,
            ..NodeRecord::new(id, owner, ip, port, engine, now, s)
        }
    }

    /// Marks this record as the replacement of `old`: the generation moves past it.
    pub fn replacing(self, old: &NodeRecord) -> NodeRecord {
        NodeRecord {
            generation: old.generation.next(),
            ..self
        }
    }

    /// The node moved to another IP: this is a new check. Returns false and changes nothing
    /// when the IP is the same or the node is `Banned`, `Dead` or `Pruned`.
    ///
    /// The caller decides whether the new IP may enroll (blocks, tombstones) before calling.
    pub fn change_ip(&mut self, new_ip: Ipv4Addr, now: Ts) -> bool {
        if new_ip == self.ip || !is_live_state(self.state) {
            return false;
        }
        self.ip = new_ip;
        self.reset_for_recheck(now);
        true
    }

    /// Back to `Pending` with nothing proven about the IP: Globalping, its confirmation from
    /// Russia, the port filter, the TCP latch and the report age start over. The metrics latch and the heartbeat describe the agent, not the
    /// IP, so they stay.
    fn reset_for_recheck(&mut self, now: Ts) {
        self.quarantine_reasons.clear();
        self.generation = self.generation.next();
        self.state = NodeState::Pending;
        self.state_since = now;
        self.health.tcp = Latch::new();
        self.health.gp = GpStatus::Unknown;
        self.health.last_report = Ts(0);
        // A confirmation and a filter both describe the old address.
        self.health.ru_confirmed_at = None;
        self.health.port_filter = false;
        self.not_alive_since = None;
        self.last_round_at = None;
        self.undecided_rounds = 0;
        self.clean_rounds = 0;
        self.suspected = false;
        self.reverify = false;
    }

    /// Enters `Quarantine`: the node leaves the masters and the prune timer starts.
    fn enter_quarantine(&mut self, now: Ts, suspected: bool, reasons: Vec<FailedCondition>) {
        self.quarantine_reasons = reasons;
        self.state = NodeState::Quarantine;
        self.state_since = now;
        self.last_round_at = None;
        self.undecided_rounds = 0;
        self.clean_rounds = 0;
        self.suspected = suspected;
    }

    /// Enters `Healthy` and the back of the master queue.
    fn enter_healthy(&mut self, now: Ts) {
        self.quarantine_reasons.clear();
        self.state = NodeState::Healthy;
        self.state_since = now;
        self.queued_at = now;
        self.last_round_at = None;
        self.undecided_rounds = 0;
        self.clean_rounds = 0;
        self.suspected = false;
    }

    pub(crate) fn enter_final(&mut self, state: NodeState, now: Ts) {
        self.quarantine_reasons.clear();
        self.state = state;
        self.state_since = now;
    }
}

/// An IP that may not enroll as a normal node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpBlock {
    pub ip: Ipv4Addr,
    /// The node that was banned.
    pub node_id: NodeId,
    pub blocked_at: Ts,
    /// No reverify before this moment: set by the ban and by every failed reverify.
    pub next_reverify_at: Option<Ts>,
    /// The owner of the banned node. The record is gone by the time the ban is lifted. `None`
    /// for a block made before the owner was kept.
    #[serde(default)]
    pub owner: Option<OwnerId>,
}

/// Escalating pause after a prune, kept per node id and per IP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub node_id: NodeId,
    pub ip: Ipv4Addr,
    /// Prunes in a row; picks the pause.
    pub strikes: u32,
    /// Enrollment is refused until this moment.
    pub until: Ts,
    pub updated_at: Ts,
}

impl Tombstone {
    pub fn expired(&self, now: Ts) -> bool {
        now.0.saturating_sub(self.updated_at.0) >= TOMBSTONE_TTL_MS
    }

    fn matches(&self, id: &NodeId, ip: Ipv4Addr) -> bool {
        self.node_id == *id || self.ip == ip
    }
}

/// Records that `node` was pruned: the pause grows with every prune of the same node id or IP.
pub fn record_prune(tombstones: &mut Vec<Tombstone>, node: &NodeRecord, now: Ts) {
    let previous = tombstones
        .iter()
        .filter(|t| t.matches(&node.id, node.ip) && !t.expired(now))
        .map(|t| t.strikes)
        .max()
        .unwrap_or(0);
    tombstones.retain(|t| !t.matches(&node.id, node.ip));
    let strikes = previous.saturating_add(1);
    let step = (strikes as usize - 1).min(PRUNE_PAUSES_SEC.len() - 1);
    tombstones.push(Tombstone {
        node_id: node.id.clone(),
        ip: node.ip,
        strikes,
        until: add_sec(now, PRUNE_PAUSES_SEC[step]),
        updated_at: now,
    });
}

/// The node returned to the queue (became `Healthy`): its pause starts over.
pub fn reset_tombstones(tombstones: &mut Vec<Tombstone>, node: &NodeRecord) {
    tombstones.retain(|t| !t.matches(&node.id, node.ip));
}

/// One state transition. Public events are built from these by the caller.
#[derive(Debug, Clone, PartialEq)]
pub enum LifecycleEvent {
    /// Emitted by the caller after it creates a record: creation is not a transition.
    Registered,
    Quarantined {
        reason: Vec<FailedCondition>,
    },
    /// The node became `Healthy`: from `Quarantine`, or for the first time from `Pending`.
    /// The caller resets the node's tombstones.
    Recovered,
    Banned {
        votes: Vec<(SensorKind, Vote)>,
    },
    Dead,
    Pruned,
    BanLifted,
    /// A reverify round was not clean: the block stays and the registration gets a 429.
    ReverifyFailed {
        retry_after_sec: u32,
        /// The votes of the round that did not prove the IP clean.
        votes: Vec<(SensorKind, Vote)>,
    },
}

/// Votes of one round, tagged with the generation the round started under.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundResult {
    pub generation: Generation,
    pub votes: Vec<(SensorKind, Vote)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollInput {
    pub node_id: NodeId,
    pub ip: Ipv4Addr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The node or IP was pruned recently.
    PrunePause,
    /// The IP is blocked and the last reverify was not clean.
    ReverifyPause,
    /// The IP is blocked and a reverify node for it is already being checked.
    ReverifyInFlight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollDecision {
    Accept,
    /// The IP already has a live record: the new node replaces it, never two records per IP.
    Replace {
        old: NodeId,
    },
    /// The IP is blocked: create the node in reverify mode and run a decisive round.
    Reverify,
    Reject {
        retry_after_sec: u32,
        reason: RejectReason,
    },
}

pub fn decide_enroll(
    req: &EnrollInput,
    existing: &[&NodeRecord],
    blocks: &[IpBlock],
    tombstones: &[Tombstone],
    now: Ts,
) -> EnrollDecision {
    // A second registration must not start a second round and spend sensor quota.
    if existing
        .iter()
        .any(|n| n.ip == req.ip && n.reverify && is_live_state(n.state))
    {
        return EnrollDecision::Reject {
            retry_after_sec: REVERIFY_IN_FLIGHT_RETRY_SEC,
            reason: RejectReason::ReverifyInFlight,
        };
    }
    if let Some(block) = blocks.iter().find(|b| b.ip == req.ip) {
        return match block.next_reverify_at {
            Some(retry_at) if now < retry_at => EnrollDecision::Reject {
                retry_after_sec: secs_until(retry_at, now),
                reason: RejectReason::ReverifyPause,
            },
            _ => EnrollDecision::Reverify,
        };
    }
    let pause_ends = tombstones
        .iter()
        .filter(|t| !t.expired(now) && t.matches(&req.node_id, req.ip) && now < t.until)
        .map(|t| t.until)
        .max();
    if let Some(until) = pause_ends {
        return EnrollDecision::Reject {
            retry_after_sec: secs_until(until, now),
            reason: RejectReason::PrunePause,
        };
    }
    match existing
        .iter()
        .find(|n| n.ip == req.ip && is_live_state(n.state))
    {
        Some(old) => EnrollDecision::Replace {
            old: old.id.clone(),
        },
        None => EnrollDecision::Accept,
    }
}

/// What an enrollment that passed `decide_enroll` asks of the owner's limit.
pub struct SlotRequest<'a> {
    pub node_id: &'a NodeId,
    pub owner: OwnerId,
    pub limit: Option<i64>,
    /// The node of the same address that gives way anyway: its place is free.
    pub displaced: Option<&'a NodeId>,
    /// Whether a silent node may give way to this one.
    pub may_evict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotDecision {
    Free,
    /// The limit is full: this node is the one silent the longest and gives way.
    Evict(NodeId),
    Full {
        limit: i64,
        current: usize,
    },
}

/// Counts the owner's live nodes against the limit. Only a limit that is exactly full evicts: with
/// the limit lowered below the nodes the owner has, one more would stay over it.
pub fn decide_slot<'a>(
    req: &SlotRequest<'_>,
    nodes: impl Iterator<Item = &'a NodeRecord>,
    now: Ts,
    s: &Settings,
) -> SlotDecision {
    let Some(limit) = req.limit else {
        return SlotDecision::Free;
    };
    let counted: Vec<&NodeRecord> = nodes
        .filter(|n| {
            n.owner == req.owner
                && is_live_state(n.state)
                && n.id != *req.node_id
                && Some(&n.id) != req.displaced
        })
        .collect();
    let current = counted.len();
    let used = i64::try_from(current).unwrap_or(i64::MAX);
    if used < limit {
        return SlotDecision::Free;
    }
    let silent = counted
        .iter()
        .filter(|n| !younger_than(n.health.last_heartbeat, now, s.heartbeat_ttl_sec))
        .min_by_key(|n| (n.health.last_heartbeat, n.id.clone()));
    match silent {
        Some(n) if req.may_evict && used == limit => SlotDecision::Evict(n.id.clone()),
        _ => SlotDecision::Full { limit, current },
    }
}

/// Liveness: the heartbeat is younger than the TTL and the TCP latch is up.
pub fn is_alive(n: &NodeRecord, now: Ts, s: &Settings) -> bool {
    n.health.tcp.up && younger_than(n.health.last_heartbeat, now, s.heartbeat_ttl_sec)
}

/// Whether a sensor round should run for this node.
///
/// A reverify node always needs its decisive round. A node in `Quarantine` needs sensors only
/// while it is alive (a dead server is not a blocked one) and its last Globalping check is
/// failed or missing, or the quarantine is already `suspected`. Alive with a good Globalping
/// check throughout and another condition failing, a stale report included: no sensors.
pub fn needs_sensors(n: &NodeRecord, now: Ts, s: &Settings) -> bool {
    match n.state {
        NodeState::Pending => n.reverify,
        NodeState::Quarantine => {
            if !is_alive(n, now, s) {
                return false;
            }
            let failed = failed_conditions(&n.health, now, s);
            // The pool's secret and domain are not on this node: a sensor would learn nothing
            // about the address, and a vote made that way must not count towards a ban.
            if failed.contains(&FailedCondition::PoolUserTaken) {
                return false;
            }
            n.suspected || globalping_unproven(&n.health)
        }
        _ => false,
    }
}

/// The last Globalping check is failed or missing. A report that aged out is not: the
/// measurement may not have come for the limit of Globalping, and the first fresh one decides.
fn globalping_unproven(h: &NodeHealth) -> bool {
    matches!(h.gp, GpStatus::Unknown | GpStatus::Failed { .. })
}

/// Whether the registrar asks the agent to measure out of turn: its planned measurement is
/// late, or the node is suspected and only a fresh measurement can clear it. The agent keeps
/// its own limits, so asking again on every sync costs nothing.
pub fn wants_measurement(n: &NodeRecord, now: Ts, s: &Settings) -> bool {
    if !is_live_state(n.state) {
        return false;
    }
    let late = !younger_than(
        n.health.last_report,
        now,
        s.globalping_interval_sec.saturating_add(60),
    );
    late || (n.state == NodeState::Quarantine && n.suspected)
}

/// When the next round is due, if the node runs rounds at all. A quarantine runs them only
/// while it is suspected: a node out of the queue for a port, metrics or a stale report has no
/// round to wait for.
pub fn next_round_at(n: &NodeRecord, s: &Settings) -> Option<Ts> {
    let runs_rounds = match n.state {
        NodeState::Quarantine => n.suspected && !n.health.pool_user_taken,
        NodeState::Pending => n.reverify,
        _ => false,
    };
    if !runs_rounds {
        return None;
    }
    let interval = if n.undecided_rounds >= s.rounds_before_slow {
        s.round_interval_slow_sec
    } else {
        s.round_interval_sec
    };
    let paced = match n.last_round_at {
        Some(last) => add_sec(last, interval),
        None => n.state_since,
    };
    // A suspected node that measured green since its last round need not wait: a round can
    // confirm it at once. Measurements are at least 5 minutes apart, and so are such rounds.
    // Only when the round could be clean: every other condition held when it measured.
    if n.state == NodeState::Quarantine
        && n.suspected
        && let GpStatus::Ok { at } = n.health.gp
        && n.last_round_at.is_some_and(|last| at > last)
        && failed_conditions(&n.health, at, s).is_empty()
    {
        return Some(paced.min(at));
    }
    Some(paced)
}

/// Time-driven transitions: triggers, recovery, dead timer, prune timer.
///
/// At most one transition per call. `Banned`, `Dead` and `Pruned` are final here.
pub fn tick_node(n: &mut NodeRecord, now: Ts, s: &Settings) -> Vec<LifecycleEvent> {
    if !is_live_state(n.state) {
        return Vec::new();
    }
    if is_alive(n, now, s) {
        n.not_alive_since = None;
    } else if n.not_alive_since.is_none() {
        n.not_alive_since = Some(now);
    }
    // A healthy node first goes through quarantine; the dead timer is for the others.
    if n.state != NodeState::Healthy
        && let Some(since) = n.not_alive_since
        && elapsed_at_least(since, now, s.dead_after_sec)
    {
        n.enter_final(NodeState::Dead, now);
        return vec![LifecycleEvent::Dead];
    }
    match n.state {
        // Its decisive round, not a report, decides whether the block is lifted.
        NodeState::Pending if n.reverify => Vec::new(),
        // No verified measurement yet: the first report decides.
        NodeState::Pending if n.health.gp == GpStatus::Unknown => Vec::new(),
        NodeState::Pending | NodeState::Healthy => {
            let failed = failed_conditions(&n.health, now, s);
            if !failed.is_empty() {
                // A new node needs time to raise its TCP and metrics latches: that alone is
                // not a reason to quarantine it (and to send a webhook) during the grace.
                let only_latches = failed
                    .iter()
                    .all(|c| matches!(c, FailedCondition::Tcp | FailedCondition::Metrics));
                if n.state == NodeState::Pending
                    && only_latches
                    && !elapsed_at_least(n.state_since, now, s.report_max_age_sec)
                {
                    return Vec::new();
                }
                let suspected = is_alive(n, now, s) && globalping_unproven(&n.health);
                n.enter_quarantine(now, suspected, failed.clone());
                return vec![LifecycleEvent::Quarantined { reason: failed }];
            }
            if n.state == NodeState::Pending {
                n.enter_healthy(now);
                return vec![LifecycleEvent::Recovered];
            }
            Vec::new()
        }
        NodeState::Quarantine => tick_quarantine(n, now, s),
        NodeState::Banned | NodeState::Dead | NodeState::Pruned => Vec::new(),
    }
}

fn tick_quarantine(n: &mut NodeRecord, now: Ts, s: &Settings) -> Vec<LifecycleEvent> {
    let failed = failed_conditions(&n.health, now, s);
    if is_alive(n, now, s) && globalping_unproven(&n.health) {
        n.suspected = true;
    }
    if failed.is_empty() {
        // Conditions hold again. A node whose Globalping was fine throughout returns at once;
        // a suspected one needs its clean rounds, one good measurement proves nothing.
        if !n.suspected {
            n.enter_healthy(now);
            return vec![LifecycleEvent::Recovered];
        }
    } else {
        // A streak needs every condition to hold the whole time.
        n.clean_rounds = 0;
    }
    if elapsed_at_least(n.state_since, now, s.quarantine_max_sec) {
        n.enter_final(NodeState::Pruned, now);
        return vec![LifecycleEvent::Pruned];
    }
    Vec::new()
}

/// Applies the votes of one round. Adds or lifts IP blocks.
pub fn apply_round(
    n: &mut NodeRecord,
    result: &RoundResult,
    blocks: &mut Vec<IpBlock>,
    now: Ts,
    s: &Settings,
) -> Vec<LifecycleEvent> {
    if result.generation != n.generation {
        return Vec::new();
    }
    match n.state {
        // Blocked answers about a server that has gone down prove nothing.
        NodeState::Quarantine if is_alive(n, now, s) => quarantine_round(n, result, blocks, now, s),
        NodeState::Pending if n.reverify => reverify_round(n, result, blocks, now, s),
        _ => Vec::new(),
    }
}

fn quarantine_round(
    n: &mut NodeRecord,
    result: &RoundResult,
    blocks: &mut Vec<IpBlock>,
    now: Ts,
    s: &Settings,
) -> Vec<LifecycleEvent> {
    n.last_round_at = Some(now);
    match judge_round(&result.votes) {
        RoundVerdict::Confirmed => {
            n.enter_final(NodeState::Banned, now);
            if !blocks.iter().any(|b| b.ip == n.ip) {
                blocks.push(IpBlock {
                    ip: n.ip,
                    node_id: n.id.clone(),
                    blocked_at: now,
                    // The agent enrolls again as soon as its token is refused; a reverify right
                    // now would only spend the sensor quota on the verdict just reached.
                    next_reverify_at: Some(add_sec(now, s.reverify_pause_sec)),
                    owner: Some(n.owner),
                });
            }
            vec![LifecycleEvent::Banned {
                votes: result.votes.clone(),
            }]
        }
        RoundVerdict::Clean if failed_conditions(&n.health, now, s).is_empty() => {
            n.undecided_rounds = 0;
            n.clean_rounds += 1;
            if n.clean_rounds >= s.clean_rounds_to_exit {
                n.enter_healthy(now);
                return vec![LifecycleEvent::Recovered];
            }
            Vec::new()
        }
        // Clean votes next to a failed condition prove nothing: undecided.
        RoundVerdict::Clean | RoundVerdict::Undecided => {
            n.undecided_rounds += 1;
            n.clean_rounds = 0;
            Vec::new()
        }
    }
}

fn reverify_round(
    n: &mut NodeRecord,
    result: &RoundResult,
    blocks: &mut Vec<IpBlock>,
    now: Ts,
    s: &Settings,
) -> Vec<LifecycleEvent> {
    n.last_round_at = Some(now);
    if judge_round(&result.votes) == RoundVerdict::Clean {
        n.reverify = false;
        n.last_round_at = None;
        return lift_blocks(n.ip, blocks);
    }
    let retry_at = add_sec(now, s.reverify_pause_sec);
    for block in blocks.iter_mut().filter(|b| b.ip == n.ip) {
        block.next_reverify_at = Some(retry_at);
    }
    // Fail closed: a node that did not prove the IP clean does not stay in the pool.
    n.enter_final(NodeState::Banned, now);
    vec![LifecycleEvent::ReverifyFailed {
        retry_after_sec: s.reverify_pause_sec,
        votes: result.votes.clone(),
    }]
}

fn lift_blocks(ip: Ipv4Addr, blocks: &mut Vec<IpBlock>) -> Vec<LifecycleEvent> {
    let before = blocks.len();
    blocks.retain(|b| b.ip != ip);
    if blocks.len() == before {
        Vec::new()
    } else {
        vec![LifecycleEvent::BanLifted]
    }
}

/// Admin unban: removes the block and returns a banned node to `Pending` for a fresh check.
pub fn unban(n: &mut NodeRecord, blocks: &mut Vec<IpBlock>, now: Ts) -> Vec<LifecycleEvent> {
    let mut events = lift_blocks(n.ip, blocks);
    if n.state == NodeState::Banned {
        n.reset_for_recheck(now);
        if events.is_empty() {
            events.push(LifecycleEvent::BanLifted);
        }
    } else if !events.is_empty() {
        // A reverify node whose block was lifted by hand has nothing left to prove.
        n.reverify = false;
    }
    events
}

/// Removes the blocks the ban of `node` left, whatever has become of its record, and returns
/// them. A reverify node on one of those addresses has nothing left to prove; the second list
/// names the nodes that changed that way.
pub fn unban_by_node(
    node: &NodeId,
    nodes: &mut BTreeMap<NodeId, NodeRecord>,
    blocks: &mut Vec<IpBlock>,
) -> (Vec<IpBlock>, Vec<NodeId>) {
    let (lifted, kept): (Vec<IpBlock>, Vec<IpBlock>) =
        blocks.drain(..).partition(|b| b.node_id == *node);
    *blocks = kept;
    let mut released = Vec::new();
    for n in nodes.values_mut() {
        if n.reverify && is_live_state(n.state) && lifted.iter().any(|b| b.ip == n.ip) {
            n.reverify = false;
            released.push(n.id.clone());
        }
    }
    (lifted, released)
}

/// The record has no reason to stay: `Banned` at once (the ban lives in the block and the ban
/// log), `Dead` and `Pruned` after a day, while the panel still shows why they left.
pub fn record_expired(n: &NodeRecord, now: Ts) -> bool {
    match n.state {
        NodeState::Banned => true,
        NodeState::Dead | NodeState::Pruned => {
            now.0.saturating_sub(n.state_since.0) >= FINAL_RECORD_TTL_MS
        }
        NodeState::Pending | NodeState::Healthy | NodeState::Quarantine => false,
    }
}

/// States of a record that is still in the pool: not final.
fn is_live_state(state: NodeState) -> bool {
    matches!(
        state,
        NodeState::Pending | NodeState::Healthy | NodeState::Quarantine
    )
}

fn add_sec(at: Ts, sec: u32) -> Ts {
    Ts(at.0.saturating_add(i64::from(sec) * 1000))
}

fn elapsed_at_least(since: Ts, now: Ts, sec: u32) -> bool {
    now.0.saturating_sub(since.0) >= i64::from(sec) * 1000
}

/// Whole seconds until `at`, rounded up, at least 1.
fn secs_until(at: Ts, now: Ts) -> u32 {
    let ms = at.0.saturating_sub(now.0).max(0);
    u32::try_from((ms.saturating_add(999) / 1000).max(1)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::votes::AbstainReason;
    use SensorKind::{Cheburcheck as Chebur, Globalping as Gp, RedFlag as Rf};

    const T0: Ts = Ts(1_800_000_000_000);
    const ABSTAIN: Vote = Vote::Abstain(AbstainReason::Unavailable);

    fn at(sec: i64) -> Ts {
        Ts(T0.0 + sec * 1000)
    }

    fn s() -> Settings {
        Settings::default()
    }

    fn id(s: &str) -> NodeId {
        NodeId::parse(s).unwrap()
    }

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn pending(name: &str, addr: &str, now: Ts) -> NodeRecord {
        NodeRecord::new(
            id(name),
            OwnerId(1),
            ip(addr),
            443,
            EngineKind::Telemt,
            now,
            &s(),
        )
    }

    fn up() -> Latch {
        Latch {
            up: true,
            fails: 0,
            oks: 0,
        }
    }

    /// Every Healthy condition holds at `now`.
    fn good_health(now: Ts) -> NodeHealth {
        NodeHealth {
            tcp: up(),
            metrics: up(),
            gp: GpStatus::Ok { at: now },
            last_heartbeat: now,
            last_report: now,
            port_ok: true,
            pool_user_taken: false,
            ru_confirmed_at: None,
            port_filter: false,
        }
    }

    fn healthy(now: Ts) -> NodeRecord {
        NodeRecord {
            state: NodeState::Healthy,
            health: good_health(now),
            ..pending("node-aaaaa", "95.216.0.1", now)
        }
    }

    /// An alive node in `Quarantine` whose last Globalping check failed.
    fn quarantined_gp_failed(now: Ts) -> NodeRecord {
        let mut n = healthy(now);
        n.health.gp = GpStatus::Failed { at: now };
        assert!(matches!(
            tick_node(&mut n, now, &s()).as_slice(),
            [LifecycleEvent::Quarantined { .. }]
        ));
        assert_eq!(n.state, NodeState::Quarantine);
        n
    }

    /// The agent is alive at `now`: fresh heartbeat.
    fn beat(n: &mut NodeRecord, now: Ts) {
        n.health.last_heartbeat = now;
    }

    fn round(n: &NodeRecord, votes: &[(SensorKind, Vote)]) -> RoundResult {
        RoundResult {
            generation: n.generation,
            votes: votes.to_vec(),
        }
    }

    fn undecided_votes() -> [(SensorKind, Vote); 3] {
        [(Gp, Vote::Blocked), (Chebur, Vote::Clear), (Rf, ABSTAIN)]
    }

    fn clean_votes() -> [(SensorKind, Vote); 3] {
        [(Gp, Vote::Clear), (Chebur, Vote::Clear), (Rf, ABSTAIN)]
    }

    fn blocked_votes() -> [(SensorKind, Vote); 3] {
        [(Gp, Vote::Blocked), (Chebur, Vote::Blocked), (Rf, ABSTAIN)]
    }

    #[test]
    fn any_failed_condition_moves_healthy_to_quarantine() {
        type Break = fn(&mut NodeHealth);
        let cases: [(FailedCondition, Break); 6] = [
            (FailedCondition::Tcp, |h| h.tcp = Latch::new()),
            (FailedCondition::Metrics, |h| h.metrics = Latch::new()),
            (FailedCondition::Globalping, |h| {
                h.gp = GpStatus::Failed { at: T0 }
            }),
            (FailedCondition::Port, |h| h.port_ok = false),
            (FailedCondition::HeartbeatStale, |h| {
                h.last_heartbeat = Ts(T0.0 - 61_000)
            }),
            (FailedCondition::ReportStale, |h| {
                h.last_report = Ts(T0.0 - 16 * 60_000)
            }),
        ];
        for (condition, break_it) in cases {
            let mut n = healthy(T0);
            break_it(&mut n.health);
            assert_eq!(
                tick_node(&mut n, T0, &s()),
                vec![LifecycleEvent::Quarantined {
                    reason: vec![condition]
                }],
                "{condition:?}"
            );
            assert_eq!(n.state, NodeState::Quarantine, "{condition:?}");
            assert_eq!(n.state_since, T0);
        }
        let mut n = healthy(T0);
        assert!(tick_node(&mut n, T0, &s()).is_empty());
        assert_eq!(n.state, NodeState::Healthy);
    }

    #[test]
    fn not_alive_for_10min_becomes_dead_without_ip_block() {
        let mut n = healthy(T0);
        n.health.last_heartbeat = Ts(T0.0 - 120_000);
        tick_node(&mut n, T0, &s());
        assert_eq!(n.state, NodeState::Quarantine);
        assert_eq!(n.not_alive_since, Some(T0));
        assert!(
            !needs_sensors(&n, T0, &s()),
            "a dead server is not a blocked one"
        );

        assert!(tick_node(&mut n, at(599), &s()).is_empty());
        assert_eq!(n.state, NodeState::Quarantine);
        assert_eq!(tick_node(&mut n, at(600), &s()), vec![LifecycleEvent::Dead]);
        assert_eq!(n.state, NodeState::Dead);

        // No block, no tombstone: the same IP enrolls freely.
        let again = EnrollInput {
            node_id: id("node-bbbbb"),
            ip: n.ip,
        };
        assert_eq!(
            decide_enroll(&again, &[&n], &[], &[], at(601)),
            EnrollDecision::Accept
        );
        assert!(tick_node(&mut n, at(700), &s()).is_empty(), "Dead is final");
    }

    #[test]
    fn becoming_alive_resets_the_dead_timer() {
        let mut n = healthy(T0);
        n.health.tcp = Latch::new();
        tick_node(&mut n, T0, &s());
        assert_eq!(n.not_alive_since, Some(T0));

        n.health.tcp = up();
        beat(&mut n, at(500));
        tick_node(&mut n, at(500), &s());
        assert_eq!(n.not_alive_since, None);

        n.health.tcp = Latch::new();
        tick_node(&mut n, at(550), &s());
        assert!(tick_node(&mut n, at(1149), &s()).is_empty());
        assert_eq!(n.state, NodeState::Quarantine);
        assert_eq!(
            tick_node(&mut n, at(1150), &s()),
            vec![LifecycleEvent::Dead],
            "10 minutes counted from the second outage"
        );
    }

    #[test]
    fn a_node_that_never_comes_alive_does_not_stay_pending_forever() {
        let mut n = pending("node-aaaaa", "95.216.0.1", T0);
        assert!(tick_node(&mut n, T0, &s()).is_empty());
        assert!(tick_node(&mut n, at(599), &s()).is_empty());
        assert_eq!(tick_node(&mut n, at(600), &s()), vec![LifecycleEvent::Dead]);
    }

    #[test]
    fn alive_gp_ok_metrics_bad_runs_no_sensors_and_recovers_instantly() {
        let mut n = healthy(T0);
        n.health.metrics = Latch::new();
        tick_node(&mut n, T0, &s());
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(!needs_sensors(&n, T0, &s()));

        n.health.metrics = up();
        assert_eq!(
            tick_node(&mut n, at(5), &s()),
            vec![LifecycleEvent::Recovered]
        );
        assert_eq!(n.state, NodeState::Healthy);
        assert_eq!(n.queued_at, at(5));
    }

    #[test]
    fn no_data_after_success_with_port_failing_runs_no_sensors() {
        let mut n = healthy(T0);
        n.health.gp = GpStatus::Ok { at: T0 }.on_no_data(at(10));
        n.health.port_ok = false;
        tick_node(&mut n, at(10), &s());
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(!needs_sensors(&n, at(10), &s()));

        n.health.port_ok = true;
        assert_eq!(
            tick_node(&mut n, at(20), &s()),
            vec![LifecycleEvent::Recovered]
        );
    }

    #[test]
    fn a_stale_report_alone_runs_no_sensors() {
        let mut n = healthy(T0);
        n.health.gp = GpStatus::NoData { at: T0 };
        let now = at(16 * 60);
        beat(&mut n, now);
        assert_eq!(
            tick_node(&mut n, now, &s()),
            vec![LifecycleEvent::Quarantined {
                reason: vec![FailedCondition::ReportStale]
            }]
        );
        assert!(
            !n.suspected,
            "a measurement that did not come is no sign of a block"
        );
        assert!(!needs_sensors(&n, now, &s()));
        // Still alive a quarter of an hour later, and still no sensors.
        let later = at(30 * 60);
        beat(&mut n, later);
        assert!(is_alive(&n, later, &s()));
        assert!(!needs_sensors(&n, later, &s()));
        assert_eq!(next_round_at(&n, &s()), None, "no round is due either");
    }

    #[test]
    fn sensors_run_only_for_a_live_node_with_unproven_globalping() {
        let n = quarantined_gp_failed(T0);
        assert!(needs_sensors(&n, T0, &s()));

        let mut unknown = n.clone();
        unknown.health.gp = GpStatus::Unknown;
        assert!(needs_sensors(&unknown, T0, &s()));

        let mut down = n.clone();
        down.health.tcp = Latch::new();
        assert!(!needs_sensors(&down, T0, &s()), "not alive: no sensors");
        let mut silent = n.clone();
        assert!(!needs_sensors(&silent, at(61), &s()), "heartbeat expired");
        // A fresh measurement does not end a suspected quarantine: clean rounds do.
        silent.health.gp = GpStatus::Ok { at: T0 };
        assert!(needs_sensors(&silent, T0, &s()));

        let healthy_node = healthy(T0);
        assert!(!needs_sensors(&healthy_node, T0, &s()));
        let waiting = pending("node-aaaaa", "95.216.0.1", T0);
        assert!(!needs_sensors(&waiting, T0, &s()));
    }

    #[test]
    fn gp_failed_then_ok_needs_two_clean_rounds() {
        let mut n = quarantined_gp_failed(T0);
        assert!(n.suspected);
        let mut blocks = Vec::new();

        // One good measurement: every condition holds again, but the block suspicion is open.
        n.health = good_health(at(10));
        assert!(tick_node(&mut n, at(10), &s()).is_empty());
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(needs_sensors(&n, at(10), &s()));

        // A clean round after it counts, one is not enough.
        let r = round(&n, &clean_votes());
        assert!(apply_round(&mut n, &r, &mut blocks, at(10), &s()).is_empty());
        assert_eq!(n.clean_rounds, 1);
        n.health = good_health(at(20));
        assert!(tick_node(&mut n, at(20), &s()).is_empty());
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(needs_sensors(&n, at(20), &s()));

        n.health = good_health(at(310));
        let r = round(&n, &clean_votes());
        assert_eq!(
            apply_round(&mut n, &r, &mut blocks, at(310), &s()),
            vec![LifecycleEvent::Recovered]
        );
        assert_eq!(n.state, NodeState::Healthy);
        assert!(!n.suspected, "leaving quarantine clears the suspicion");
    }

    #[test]
    fn a_stale_report_ends_with_the_first_fresh_green_measurement() {
        let mut n = healthy(T0);
        let now = at(16 * 60);
        beat(&mut n, now);
        assert_eq!(
            tick_node(&mut n, now, &s()),
            vec![LifecycleEvent::Quarantined {
                reason: vec![FailedCondition::ReportStale]
            }]
        );

        let later = at(17 * 60);
        n.health = good_health(later);
        assert_eq!(
            tick_node(&mut n, later, &s()),
            vec![LifecycleEvent::Recovered]
        );
        assert_eq!(n.state, NodeState::Healthy);
    }

    #[test]
    fn a_stale_report_then_a_red_measurement_runs_rounds() {
        let mut n = healthy(T0);
        let now = at(16 * 60);
        beat(&mut n, now);
        tick_node(&mut n, now, &s());
        assert!(!n.suspected);

        let later = at(17 * 60);
        n.health = good_health(later);
        n.health.gp = GpStatus::Failed { at: later };
        assert!(tick_node(&mut n, later, &s()).is_empty());
        assert!(n.suspected);
        assert!(needs_sensors(&n, later, &s()));
    }

    #[test]
    fn a_fresh_green_measurement_makes_the_round_due_at_once() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        let r = round(&n, &undecided_votes());
        apply_round(&mut n, &r, &mut blocks, T0, &s());
        assert_eq!(next_round_at(&n, &s()), Some(at(300)));

        // Still red: the round waits for its interval.
        n.health.gp = GpStatus::Failed { at: at(60) };
        assert_eq!(next_round_at(&n, &s()), Some(at(300)));

        // Green: a round can confirm it now.
        n.health = good_health(at(120));
        assert_eq!(next_round_at(&n, &s()), Some(at(120)));

        // The round that used it paces the next one again.
        let r = round(&n, &clean_votes());
        apply_round(&mut n, &r, &mut blocks, at(120), &s());
        assert_eq!(next_round_at(&n, &s()), Some(at(420)));
    }

    #[test]
    fn a_green_measurement_with_another_condition_failing_keeps_the_pace() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        let r = round(&n, &undecided_votes());
        apply_round(&mut n, &r, &mut blocks, T0, &s());

        // Green, but the metrics are down: a round now could not be clean.
        n.health = good_health(at(120));
        n.health.metrics.up = false;
        assert_eq!(next_round_at(&n, &s()), Some(at(300)));
    }

    #[test]
    fn clean_votes_of_other_sensors_do_not_clear_a_red_globalping() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        let votes = [(Gp, ABSTAIN), (Chebur, Vote::Clear), (Rf, Vote::Clear)];
        for t in [0, 300] {
            beat(&mut n, at(t));
            let r = round(&n, &votes);
            assert!(apply_round(&mut n, &r, &mut blocks, at(t), &s()).is_empty());
        }
        assert_eq!(n.state, NodeState::Quarantine);
        assert_eq!(n.clean_rounds, 0);
    }

    #[test]
    fn the_registrar_asks_for_a_measurement_when_the_planned_one_is_late() {
        let n = healthy(T0);
        let interval = i64::from(s().globalping_interval_sec);
        assert!(!wants_measurement(&n, at(interval + 59), &s()));
        assert!(wants_measurement(&n, at(interval + 60), &s()));
    }

    #[test]
    fn the_registrar_asks_for_a_measurement_of_a_suspected_node() {
        let n = quarantined_gp_failed(T0);
        assert!(wants_measurement(&n, T0, &s()));

        let mut not_suspected = healthy(T0);
        not_suspected.health.port_ok = false;
        tick_node(&mut not_suspected, T0, &s());
        assert_eq!(not_suspected.state, NodeState::Quarantine);
        assert!(!wants_measurement(&not_suspected, T0, &s()));
    }

    #[test]
    fn a_gone_node_is_not_asked_to_measure() {
        let mut n = healthy(T0);
        n.enter_final(NodeState::Dead, T0);
        assert!(!wants_measurement(&n, at(3600), &s()));
    }

    #[test]
    fn gp_ok_throughout_does_not_become_suspected() {
        let mut n = healthy(T0);
        n.health.metrics = Latch::new();
        tick_node(&mut n, T0, &s());
        assert!(!n.suspected);
        n.health.metrics = up();
        assert_eq!(
            tick_node(&mut n, at(5), &s()),
            vec![LifecycleEvent::Recovered]
        );
    }

    #[test]
    fn gp_failing_while_quarantined_marks_the_node_suspected() {
        let mut n = healthy(T0);
        n.health.port_ok = false;
        tick_node(&mut n, T0, &s());
        assert!(!n.suspected);

        n.health.gp = GpStatus::Failed { at: at(5) };
        tick_node(&mut n, at(5), &s());
        assert!(n.suspected);
        n.health.gp = GpStatus::Ok { at: at(10) };
        tick_node(&mut n, at(10), &s());
        assert!(n.suspected, "stays set until the node leaves quarantine");
    }

    #[test]
    fn gp_failed_runs_round_every_5min_then_15min_after_3() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        assert_eq!(
            next_round_at(&n, &s()),
            Some(T0),
            "the first round is due at once"
        );

        let mut now = T0;
        let mut due = Vec::new();
        for _ in 0..4 {
            assert_eq!(next_round_at(&n, &s()).map(|t| t.0 <= now.0), Some(true));
            beat(&mut n, now);
            let r = round(&n, &undecided_votes());
            assert!(apply_round(&mut n, &r, &mut blocks, now, &s()).is_empty());
            let next = next_round_at(&n, &s()).unwrap();
            due.push((next.0 - now.0) / 1000);
            now = next;
        }
        assert_eq!(due, vec![300, 300, 900, 900]);
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(blocks.is_empty());
    }

    #[test]
    fn two_clean_rounds_recover_one_does_not() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        // The fresh Globalping result arrived with the round, so every condition holds.
        n.health.gp = GpStatus::Ok { at: at(10) };
        beat(&mut n, at(10));
        n.health.last_report = at(10);

        let r = round(&n, &clean_votes());
        assert!(apply_round(&mut n, &r, &mut blocks, at(10), &s()).is_empty());
        assert_eq!(n.state, NodeState::Quarantine);
        assert_eq!(n.clean_rounds, 1);

        // One clean round is not enough: the tick must not shortcut the streak.
        assert!(tick_node(&mut n, at(20), &s()).is_empty());
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(needs_sensors(&n, at(20), &s()), "the second round must run");
        assert_eq!(next_round_at(&n, &s()), Some(at(310)));

        beat(&mut n, at(310));
        n.health.last_report = at(310);
        let r = round(&n, &clean_votes());
        assert_eq!(
            apply_round(&mut n, &r, &mut blocks, at(310), &s()),
            vec![LifecycleEvent::Recovered]
        );
        assert_eq!(n.state, NodeState::Healthy);
        assert_eq!(n.queued_at, at(310));
        assert!(blocks.is_empty());
    }

    #[test]
    fn a_broken_streak_starts_over() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        n.health = good_health(at(10));
        for (i, votes) in [clean_votes(), undecided_votes(), clean_votes()]
            .into_iter()
            .enumerate()
        {
            let now = at(10 + 300 * i as i64);
            n.health = good_health(now);
            let r = round(&n, &votes);
            assert!(apply_round(&mut n, &r, &mut blocks, now, &s()).is_empty());
        }
        assert_eq!(n.state, NodeState::Quarantine);
        assert_eq!(n.clean_rounds, 1);
    }

    #[test]
    fn clean_votes_with_failed_condition_do_not_count() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        for i in 0..3 {
            let now = at(300 * i);
            beat(&mut n, now);
            let r = round(&n, &clean_votes());
            assert!(apply_round(&mut n, &r, &mut blocks, now, &s()).is_empty());
        }
        assert_eq!(n.state, NodeState::Quarantine);
        assert_eq!(n.clean_rounds, 0);
        assert_eq!(
            n.undecided_rounds, 3,
            "they count as rounds without a decision"
        );
        assert_eq!(next_round_at(&n, &s()), Some(at(600 + 900)));
    }

    #[test]
    fn confirmed_round_bans_and_blocks_ip() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        let votes = blocked_votes();
        let r = round(&n, &votes);
        let events = apply_round(&mut n, &r, &mut blocks, at(5), &s());
        assert_eq!(
            events,
            vec![LifecycleEvent::Banned {
                votes: votes.to_vec()
            }]
        );
        assert_eq!(n.state, NodeState::Banned);
        assert_eq!(
            blocks,
            vec![IpBlock {
                ip: n.ip,
                node_id: n.id.clone(),
                blocked_at: at(5),
                next_reverify_at: Some(at(5 + 900)),
                owner: Some(OwnerId(1)),
            }]
        );
        assert!(
            tick_node(&mut n, at(10_000), &s()).is_empty(),
            "Banned stays banned"
        );

        let again = EnrollInput {
            node_id: id("node-bbbbb"),
            ip: n.ip,
        };
        // The ban starts the pause: no reverify before it ends, however fast the agent is.
        assert_eq!(
            decide_enroll(&again, &[&n], &blocks, &[], at(20)),
            EnrollDecision::Reject {
                retry_after_sec: 885,
                reason: RejectReason::ReverifyPause,
            }
        );
        assert_eq!(
            decide_enroll(&again, &[&n], &blocks, &[], at(5 + 900)),
            EnrollDecision::Reverify
        );
    }

    #[test]
    fn a_single_blocked_vote_or_abstentions_never_ban() {
        let mut blocks = Vec::new();
        for votes in [
            vec![
                (Gp, Vote::Blocked),
                (Chebur, Vote::Clear),
                (Rf, Vote::Clear),
            ],
            vec![(Gp, Vote::Blocked), (Chebur, ABSTAIN), (Rf, ABSTAIN)],
            vec![(Gp, ABSTAIN), (Chebur, ABSTAIN), (Rf, ABSTAIN)],
            vec![(Gp, Vote::Blocked), (Gp, Vote::Blocked)],
        ] {
            let mut n = quarantined_gp_failed(T0);
            let r = round(&n, &votes);
            assert!(apply_round(&mut n, &r, &mut blocks, at(5), &s()).is_empty());
            assert_eq!(n.state, NodeState::Quarantine);
        }
        assert!(blocks.is_empty());
    }

    #[test]
    fn a_round_for_a_node_that_went_down_does_not_ban() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        let r = round(&n, &blocked_votes());
        // The server died while the sensors were running: the blocked answers prove nothing.
        let now = at(120);
        assert!(!is_alive(&n, now, &s()));
        assert!(apply_round(&mut n, &r, &mut blocks, now, &s()).is_empty());
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(blocks.is_empty());
        assert_eq!(n.undecided_rounds, 0);
    }

    #[test]
    fn stale_generation_round_result_is_ignored() {
        let mut n = quarantined_gp_failed(T0);
        n.generation = Generation(2);
        let mut blocks = Vec::new();
        let stale = RoundResult {
            generation: Generation(1),
            votes: blocked_votes().to_vec(),
        };
        assert!(apply_round(&mut n, &stale, &mut blocks, at(5), &s()).is_empty());
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(blocks.is_empty());
        assert_eq!(n.last_round_at, None);

        let current = RoundResult {
            generation: Generation(2),
            votes: blocked_votes().to_vec(),
        };
        assert_eq!(
            apply_round(&mut n, &current, &mut blocks, at(5), &s()).len(),
            1
        );
        assert_eq!(n.state, NodeState::Banned);
    }

    #[test]
    fn all_abstain_for_60min_prunes_without_ban() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        let mut now = T0;
        let mut events = Vec::new();
        while now.0 <= at(3600).0 {
            beat(&mut n, now);
            events.extend(tick_node(&mut n, now, &s()));
            if n.state != NodeState::Quarantine {
                break;
            }
            if next_round_at(&n, &s()).is_some_and(|t| t.0 <= now.0) {
                let r = round(&n, &[(Gp, ABSTAIN), (Chebur, ABSTAIN), (Rf, ABSTAIN)]);
                events.extend(apply_round(&mut n, &r, &mut blocks, now, &s()));
            }
            now = Ts(now.0 + 60_000);
        }
        assert_eq!(events, vec![LifecycleEvent::Pruned]);
        assert_eq!(n.state, NodeState::Pruned);
        assert_eq!(
            n.state_since,
            at(3600),
            "exactly 60 minutes after entering quarantine"
        );
        assert!(blocks.is_empty(), "no ban, no IP block");
    }

    #[test]
    fn a_pruned_node_leaves_a_tombstone_by_id_and_by_ip() {
        let mut n = quarantined_gp_failed(T0);
        beat(&mut n, at(3599));
        assert!(tick_node(&mut n, at(3599), &s()).is_empty());
        beat(&mut n, at(3600));
        assert_eq!(
            tick_node(&mut n, at(3600), &s()),
            vec![LifecycleEvent::Pruned]
        );

        let mut tombstones = Vec::new();
        record_prune(&mut tombstones, &n, at(3600));
        let same_id_other_ip = EnrollInput {
            node_id: n.id.clone(),
            ip: ip("95.216.0.99"),
        };
        let other_id_same_ip = EnrollInput {
            node_id: id("node-bbbbb"),
            ip: n.ip,
        };
        let stranger = EnrollInput {
            node_id: id("node-ccccc"),
            ip: ip("95.216.0.98"),
        };
        let now = at(3600 + 60);
        let reject = EnrollDecision::Reject {
            retry_after_sec: 840,
            reason: RejectReason::PrunePause,
        };
        assert_eq!(
            decide_enroll(&same_id_other_ip, &[], &[], &tombstones, now),
            reject
        );
        assert_eq!(
            decide_enroll(&other_id_same_ip, &[], &[], &tombstones, now),
            reject
        );
        assert_eq!(
            decide_enroll(&stranger, &[], &[], &tombstones, now),
            EnrollDecision::Accept
        );
    }

    #[test]
    fn prune_backoff_is_15m_30m_1h_2h_3h_and_resets_on_queue() {
        let mut tombstones = Vec::new();
        let mut now = T0;
        let mut node = pending("node-aaaaa", "95.216.0.1", now);
        node.state = NodeState::Pruned;
        let req = EnrollInput {
            node_id: node.id.clone(),
            ip: node.ip,
        };

        let expected_min = [15, 30, 60, 120, 180, 180, 180];
        for pause_min in expected_min {
            record_prune(&mut tombstones, &node, now);
            assert_eq!(tombstones.len(), 1, "one record per node and IP");
            let pause = i64::from(pause_min) * 60;
            assert_eq!(
                decide_enroll(&req, &[], &[], &tombstones, now),
                EnrollDecision::Reject {
                    retry_after_sec: pause as u32,
                    reason: RejectReason::PrunePause
                }
            );
            let almost = Ts(now.0 + pause * 1000 - 1);
            assert!(matches!(
                decide_enroll(&req, &[], &[], &tombstones, almost),
                EnrollDecision::Reject {
                    retry_after_sec: 1,
                    ..
                }
            ));
            now = Ts(now.0 + pause * 1000);
            assert_eq!(
                decide_enroll(&req, &[], &[], &tombstones, now),
                EnrollDecision::Accept,
                "the pause is over after {pause_min} minutes"
            );
        }

        reset_tombstones(&mut tombstones, &node);
        assert!(tombstones.is_empty());
        record_prune(&mut tombstones, &node, now);
        assert_eq!(
            decide_enroll(&req, &[], &[], &tombstones, now),
            EnrollDecision::Reject {
                retry_after_sec: 900,
                reason: RejectReason::PrunePause
            },
            "back to 15 minutes after returning to the queue"
        );
    }

    #[test]
    fn a_tombstone_expires_7_days_after_its_last_update() {
        let mut tombstones = Vec::new();
        let mut node = pending("node-aaaaa", "95.216.0.1", T0);
        node.state = NodeState::Pruned;
        for _ in 0..3 {
            record_prune(&mut tombstones, &node, T0);
        }
        let t = &tombstones[0];
        assert_eq!(t.strikes, 3);
        let week = 7 * 24 * 3600;
        assert!(!t.expired(at(week - 1)));
        assert!(t.expired(at(week)));

        // An expired record does not keep escalating the pause.
        record_prune(&mut tombstones, &node, at(week + 5));
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].strikes, 1);
    }

    #[test]
    fn enroll_from_blocked_ip_starts_reverify() {
        let banned_ip = ip("95.216.0.1");
        let blocks = vec![IpBlock {
            ip: banned_ip,
            node_id: id("node-aaaaa"),
            blocked_at: T0,
            next_reverify_at: None,
            owner: None,
        }];
        let from_blocked = EnrollInput {
            node_id: id("node-bbbbb"),
            ip: banned_ip,
        };
        let from_clean = EnrollInput {
            node_id: id("node-bbbbb"),
            ip: ip("95.216.0.2"),
        };
        assert_eq!(
            decide_enroll(&from_blocked, &[], &blocks, &[], at(60)),
            EnrollDecision::Reverify
        );
        assert_eq!(
            decide_enroll(&from_clean, &[], &blocks, &[], at(60)),
            EnrollDecision::Accept
        );
    }

    /// A reverify node for a blocked IP, with the block in place.
    fn blocked_ip_with_reverify_node(now: Ts) -> (NodeRecord, Vec<IpBlock>) {
        let blocks = vec![IpBlock {
            ip: ip("95.216.0.1"),
            node_id: id("node-aaaaa"),
            blocked_at: T0,
            next_reverify_at: None,
            owner: None,
        }];
        let node = NodeRecord::new_reverify(
            id("node-bbbbb"),
            OwnerId(1),
            ip("95.216.0.1"),
            443,
            EngineKind::Telemt,
            now,
            &s(),
        );
        (node, blocks)
    }

    #[test]
    fn reverify_clean_lifts_ban_dirty_keeps_and_returns_429_for_15min() {
        // Clean round.
        let now = at(100);
        let (mut n, mut blocks) = blocked_ip_with_reverify_node(now);
        n.health = good_health(now);
        assert!(
            tick_node(&mut n, now, &s()).is_empty(),
            "the tick must not skip the round"
        );
        assert_eq!(n.state, NodeState::Pending);
        assert!(needs_sensors(&n, now, &s()));
        assert_eq!(next_round_at(&n, &s()), Some(now), "decisive round at once");

        let r = round(&n, &clean_votes());
        assert_eq!(
            apply_round(&mut n, &r, &mut blocks, now, &s()),
            vec![LifecycleEvent::BanLifted]
        );
        assert!(blocks.is_empty());
        assert_eq!(n.state, NodeState::Pending);
        assert!(!n.reverify);
        assert_eq!(
            tick_node(&mut n, now, &s()),
            vec![LifecycleEvent::Recovered],
            "now the first valid report makes it Healthy"
        );

        // Dirty rounds: a confirmed block and an undecided round both keep the block.
        for votes in [blocked_votes(), undecided_votes()] {
            let (mut n, mut blocks) = blocked_ip_with_reverify_node(now);
            let r = round(&n, &votes);
            assert_eq!(
                apply_round(&mut n, &r, &mut blocks, now, &s()),
                vec![LifecycleEvent::ReverifyFailed {
                    retry_after_sec: 900,
                    votes: votes.to_vec(),
                }]
            );
            assert_eq!(
                n.state,
                NodeState::Banned,
                "a failed reverify does not stay in the pool"
            );
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].next_reverify_at, Some(at(100 + 900)));

            let req = EnrollInput {
                node_id: id("node-ccccc"),
                ip: n.ip,
            };
            assert_eq!(
                decide_enroll(&req, &[], &blocks, &[], at(101)),
                EnrollDecision::Reject {
                    retry_after_sec: 899,
                    reason: RejectReason::ReverifyPause
                }
            );
            assert_eq!(
                decide_enroll(&req, &[], &blocks, &[], at(999)),
                EnrollDecision::Reject {
                    retry_after_sec: 1,
                    reason: RejectReason::ReverifyPause
                }
            );
            assert_eq!(
                decide_enroll(&req, &[], &blocks, &[], at(1000)),
                EnrollDecision::Reverify,
                "the next attempt is allowed after 15 minutes"
            );
        }
    }

    #[test]
    fn admin_unban_lifts_the_block_and_returns_the_node_to_pending() {
        let mut n = quarantined_gp_failed(T0);
        let mut blocks = Vec::new();
        let r = round(&n, &blocked_votes());
        apply_round(&mut n, &r, &mut blocks, at(5), &s());
        let generation = n.generation;

        assert_eq!(
            unban(&mut n, &mut blocks, at(60)),
            vec![LifecycleEvent::BanLifted]
        );
        assert!(blocks.is_empty());
        assert_eq!(n.state, NodeState::Pending);
        assert_eq!(n.generation, generation.next());
        assert_eq!(n.health.gp, GpStatus::Unknown);
        assert!(
            unban(&mut n, &mut blocks, at(61)).is_empty(),
            "nothing left to lift"
        );
    }

    #[test]
    fn a_new_address_or_a_recheck_forgets_the_confirmation_and_the_filter() {
        let mark = |n: &mut NodeRecord| {
            n.health.ru_confirmed_at = Some(T0);
            n.health.port_filter = true;
        };
        let mut n = healthy(T0);
        mark(&mut n);
        assert!(n.change_ip(ip("95.216.0.7"), at(10)));
        assert_eq!(n.health.ru_confirmed_at, None);
        assert!(!n.health.port_filter);

        let mut n = healthy(T0);
        n.state = NodeState::Banned;
        mark(&mut n);
        unban(&mut n, &mut Vec::new(), at(10));
        assert_eq!(n.state, NodeState::Pending);
        assert_eq!(n.health.ru_confirmed_at, None);
        assert!(!n.health.port_filter);
    }

    #[test]
    fn ip_change_resets_state_and_bumps_generation() {
        let mut n = healthy(T0);
        n.undecided_rounds = 2;
        n.last_round_at = Some(T0);
        let old_generation = n.generation;

        assert!(!n.change_ip(n.ip, at(5)), "the same IP changes nothing");
        assert_eq!(n.state, NodeState::Healthy);

        assert!(n.change_ip(ip("95.216.0.7"), at(10)));
        assert_eq!(n.ip, ip("95.216.0.7"));
        assert_eq!(n.state, NodeState::Pending);
        assert_eq!(n.state_since, at(10));
        assert_eq!(n.generation, old_generation.next());
        assert!(!n.health.tcp.up);
        assert_eq!(n.health.gp, GpStatus::Unknown);
        assert_eq!(n.health.last_report, Ts(0));
        assert_eq!(n.undecided_rounds, 0);
        assert_eq!(n.last_round_at, None);
        assert!(!n.reverify);

        // A round started for the old IP is stale now.
        let stale = RoundResult {
            generation: old_generation,
            votes: blocked_votes().to_vec(),
        };
        assert!(apply_round(&mut n, &stale, &mut Vec::new(), at(11), &s()).is_empty());

        for state in [NodeState::Banned, NodeState::Dead, NodeState::Pruned] {
            n.state = state;
            assert!(!n.change_ip(ip("95.216.0.8"), at(20)), "{state:?} is final");
            assert_eq!(n.ip, ip("95.216.0.7"));
        }
    }

    #[test]
    fn reinstall_on_same_ip_replaces_old_node_no_duplicate() {
        let mut old = healthy(T0);
        old.generation = Generation(4);
        let mut pool = vec![old];
        let req = EnrollInput {
            node_id: id("node-bbbbb"),
            ip: ip("95.216.0.1"),
        };
        let now = at(30);

        let refs: Vec<&NodeRecord> = pool.iter().collect();
        let decision = decide_enroll(&req, &refs, &[], &[], now);
        assert_eq!(
            decision,
            EnrollDecision::Replace {
                old: id("node-aaaaa")
            }
        );

        let EnrollDecision::Replace { old: old_id } = decision else {
            unreachable!()
        };
        let old = pool.iter().position(|n| n.id == old_id).unwrap();
        let old = pool.remove(old);
        pool.push(
            NodeRecord::new(
                req.node_id.clone(),
                OwnerId(1),
                req.ip,
                443,
                EngineKind::Telemt,
                now,
                &s(),
            )
            .replacing(&old),
        );

        assert_eq!(pool.iter().filter(|n| n.ip == req.ip).count(), 1);
        assert_eq!(pool[0].id, req.node_id);
        assert_eq!(pool[0].state, NodeState::Pending);
        assert_eq!(pool[0].generation, Generation(5));

        // Records that left the pool, or live elsewhere, do not trigger a replacement.
        let mut gone = healthy(T0);
        gone.state = NodeState::Dead;
        let mut elsewhere = healthy(T0);
        elsewhere.ip = ip("95.216.0.2");
        assert_eq!(
            decide_enroll(&req, &[&gone, &elsewhere], &[], &[], now),
            EnrollDecision::Accept
        );
        for state in [NodeState::Pending, NodeState::Quarantine] {
            let mut live = healthy(T0);
            live.state = state;
            assert_eq!(
                decide_enroll(&req, &[&live], &[], &[], now),
                EnrollDecision::Replace {
                    old: live.id.clone()
                }
            );
        }
    }

    #[test]
    fn pending_first_valid_report_becomes_healthy() {
        let mut n = pending("node-aaaaa", "95.216.0.1", T0);
        assert!(tick_node(&mut n, at(5), &s()).is_empty(), "no report yet");
        assert_eq!(n.state, NodeState::Pending);

        n.health = good_health(at(20));
        assert_eq!(
            tick_node(&mut n, at(20), &s()),
            vec![LifecycleEvent::Recovered]
        );
        assert_eq!(n.state, NodeState::Healthy);
        assert_eq!(n.queued_at, at(20));
        assert_eq!(n.state_since, at(20));
    }

    #[test]
    fn pending_failed_condition_moves_to_quarantine() {
        let mut n = pending("node-aaaaa", "95.216.0.1", T0);
        n.health = good_health(at(20));
        n.health.gp = GpStatus::Failed { at: at(20) };
        assert_eq!(
            tick_node(&mut n, at(20), &s()),
            vec![LifecycleEvent::Quarantined {
                reason: vec![FailedCondition::Globalping]
            }]
        );
        assert_eq!(n.state, NodeState::Quarantine);
        assert_eq!(n.state_since, at(20));
        assert!(needs_sensors(&n, at(20), &s()));
    }

    #[test]
    fn a_taken_user_name_quarantines_a_pending_node_at_once() {
        let mut n = pending("node-aaaaa", "95.216.0.1", T0);
        n.health = good_health(at(20));
        n.health.pool_user_taken = true;
        assert_eq!(
            tick_node(&mut n, at(20), &s()),
            vec![LifecycleEvent::Quarantined {
                reason: vec![FailedCondition::PoolUserTaken]
            }]
        );
        assert_eq!(n.state, NodeState::Quarantine);
        assert!(!n.suspected);
    }

    #[test]
    fn a_node_with_a_taken_user_name_gets_no_sensors() {
        // The pool's secret and domain are not on this node, so a sensor learns nothing about the
        // address from it, even when Globalping fails as well.
        let mut n = quarantined_gp_failed(T0);
        assert!(needs_sensors(&n, T0, &s()));
        n.health.pool_user_taken = true;
        assert!(!needs_sensors(&n, T0, &s()));
    }

    #[test]
    fn a_node_returns_at_once_when_the_name_is_free_again() {
        let mut n = healthy(T0);
        n.health.pool_user_taken = true;
        assert_eq!(
            tick_node(&mut n, T0, &s()),
            vec![LifecycleEvent::Quarantined {
                reason: vec![FailedCondition::PoolUserTaken]
            }]
        );
        n.health.pool_user_taken = false;
        assert_eq!(
            tick_node(&mut n, at(1), &s()),
            vec![LifecycleEvent::Recovered]
        );
        assert_eq!(n.state, NodeState::Healthy);
    }

    /// A node that reports, with the metrics latch not yet risen.
    fn pending_with_unrisen_metrics(report_at: Ts) -> NodeRecord {
        let mut n = pending("node-aaaaa", "95.216.0.1", T0);
        n.health = good_health(report_at);
        n.health.metrics = Latch::new();
        n
    }

    #[test]
    fn pending_waits_for_latches_without_quarantine() {
        let mut n = pending_with_unrisen_metrics(at(20));
        assert!(tick_node(&mut n, at(20), &s()).is_empty());
        assert_eq!(n.state, NodeState::Pending);

        let now = at(899);
        n.health.last_heartbeat = now;
        n.health.last_report = now;
        assert!(tick_node(&mut n, now, &s()).is_empty());
        assert_eq!(n.state, NodeState::Pending);

        n.health.metrics = up();
        assert_eq!(
            tick_node(&mut n, now, &s()),
            vec![LifecycleEvent::Recovered]
        );
    }

    #[test]
    fn pending_latches_never_rising_quarantine_after_grace() {
        let mut n = pending_with_unrisen_metrics(at(20));
        let now = at(900);
        n.health.last_heartbeat = now;
        n.health.last_report = now;
        assert_eq!(
            tick_node(&mut n, now, &s()),
            vec![LifecycleEvent::Quarantined {
                reason: vec![FailedCondition::Metrics]
            }]
        );
        assert!(!n.suspected, "Globalping is fine, no sensors");
    }

    #[test]
    fn pending_port_mismatch_quarantines_at_once() {
        let mut n = pending("node-aaaaa", "95.216.0.1", T0);
        n.health = good_health(at(20));
        n.health.port_ok = false;
        assert_eq!(
            tick_node(&mut n, at(20), &s()),
            vec![LifecycleEvent::Quarantined {
                reason: vec![FailedCondition::Port]
            }]
        );
    }

    #[test]
    fn second_enroll_during_reverify_is_rejected() {
        let (node, blocks) = blocked_ip_with_reverify_node(at(100));
        let second = EnrollInput {
            node_id: id("node-ccccc"),
            ip: ip("95.216.0.1"),
        };
        assert_eq!(
            decide_enroll(&second, &[&node], &blocks, &[], at(110)),
            EnrollDecision::Reject {
                retry_after_sec: 60,
                reason: RejectReason::ReverifyInFlight
            }
        );

        // A reverify record that is no longer live does not hold the IP.
        let mut failed = node.clone();
        failed.state = NodeState::Banned;
        assert_eq!(
            decide_enroll(&second, &[&failed], &blocks, &[], at(110)),
            EnrollDecision::Reverify
        );
    }
}
