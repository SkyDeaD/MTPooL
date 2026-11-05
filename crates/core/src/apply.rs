//! `apply`: the one pure entry point of the pool.
//!
//! `apply(state, command, now)` changes the state and returns what the caller has to do:
//! store rows, start workers, send events, answer the request. There is no I/O in here and no
//! clock: `now` is a parameter, and worker results come back as commands that carry the
//! generation they were started under. A result with another generation changes nothing.
//!
//! The effects of one call come in this order: persist operations, events, worker requests,
//! and the reply last, so a caller that answers only after the writes are queued is correct.

use std::net::Ipv4Addr;

use serde_json::{Value, json};

use crate::dns::{DnsState, RecordValue, backoff};
use crate::health::{GpStatus, younger_than};
use crate::lifecycle::{
    EnrollDecision, EnrollInput, IpBlock, LifecycleEvent, NodeRecord, RoundResult, SlotDecision,
    SlotRequest, Tombstone, apply_round, decide_enroll, decide_slot, needs_sensors, next_round_at,
    record_expired, record_prune, reset_tombstones, tick_node, unban, unban_by_node,
};
use crate::selection::{Assignments, select};
use crate::settings::Settings;
use crate::srmd::{SrmdAction, SrmdError, rebalance, release, set_clients, take};
use crate::state::{
    Command, Effect, EnrollRequest, Event, EventKind, HeartbeatRequest, NodeAction, NodeStats,
    PersistOp, PoolState, Refusal, Reply, ReplyId, ReportRequest, SrmdManualAction,
    SrmdManualRequest, TerminateReason,
};
use crate::traffic::traffic_delta;
use crate::types::{Generation, NodeId, NodeState, OwnerId, Ts};
use crate::votes::{AbstainReason, RoundVerdict, SensorKind, SensorVote, Vote, judge_round};

type Out = Vec<Effect>;
type Answer = (ReplyId, Reply);

/// Applies one command. See the module documentation for the order of the effects.
pub fn apply(state: &mut PoolState, command: Command, now: Ts) -> Vec<Effect> {
    let mut out = Vec::new();
    // Blocks and tombstones change inside the lifecycle functions; the hot commands never
    // touch them, so only the others pay for the comparison.
    let hot = matches!(
        command,
        Command::Heartbeat(_) | Command::Report(_) | Command::TcpProbe { .. }
    );
    let lists = (!hot).then(|| (state.blocks.clone(), state.tombstones.clone()));

    let answer = match command {
        Command::Enroll(r) => Some(enroll(state, r, now, &mut out)),
        Command::Heartbeat(r) => Some(heartbeat(state, r, now, &mut out)),
        Command::Report(r) => Some(report(state, r, now, &mut out)),
        Command::TcpProbe {
            node,
            generation,
            open,
        } => {
            tcp_probe(state, &node, generation, open, &mut out);
            None
        }
        Command::RoundDone {
            node,
            generation,
            votes,
        } => {
            round_done(state, &node, generation, votes, now, &mut out);
            None
        }
        Command::DnsDone {
            domain,
            generation,
            result,
        } => {
            dns_done(state, &domain, generation, result, now, &mut out);
            None
        }
        Command::Retire(a) => Some(retire(state, a, now, &mut out)),
        Command::AdminUnban(a) => Some(admin_unban(state, a, now, &mut out)),
        Command::Recheck(a) => Some(recheck(state, a, now, &mut out)),
        Command::SrmdManual(r) => Some(srmd_manual(state, r, now, &mut out)),
        Command::AvailabilityRefreshed(list) => {
            availability_refreshed(state, list);
            None
        }
        Command::DnsPushAll => {
            dns_push_all(state, now, &mut out);
            None
        }
        Command::SettingsChanged(s) => {
            settings_changed(state, s, &mut out);
            None
        }
        Command::Tick => {
            tick(state, now, &mut out);
            None
        }
    };

    if let Some((blocks, tombstones)) = lists {
        persist_list_changes(
            &mut out,
            &blocks,
            &state.blocks,
            &tombstones,
            &state.tombstones,
        );
        // Last, so that a crash between the writes never leaves a ban without its block.
        forget_expired(state, now, &mut out);
    }
    if let Some((id, reply)) = answer {
        out.push(Effect::Reply(id, reply));
    }
    out
}

// ---- helpers ----

fn persist(out: &mut Out, op: PersistOp) {
    out.push(Effect::Persist(op));
}

fn emit(
    out: &mut Out,
    now: Ts,
    kind: EventKind,
    node: Option<&NodeId>,
    domain: Option<&str>,
    detail: Value,
) {
    out.push(Effect::Emit(Event {
        kind,
        at: now,
        node: node.cloned(),
        owner: None,
        domain: domain.map(str::to_owned),
        detail,
    }));
}

fn emit_node(out: &mut Out, now: Ts, kind: EventKind, node: &NodeRecord, detail: Value) {
    emit_owned(out, now, kind, &node.id, Some(node.owner), None, detail);
}

/// An event with the owner of the node written in, for when the record is about to go.
fn emit_owned(
    out: &mut Out,
    now: Ts,
    kind: EventKind,
    node: &NodeId,
    owner: Option<OwnerId>,
    domain: Option<&str>,
    detail: Value,
) {
    out.push(Effect::Emit(Event {
        kind,
        at: now,
        node: Some(node.clone()),
        owner,
        domain: domain.map(str::to_owned),
        detail,
    }));
}

/// Removes the records that have no reason to stay. See `record_expired`.
fn forget_expired(state: &mut PoolState, now: Ts, out: &mut Out) {
    let expired: Vec<NodeId> = state
        .nodes
        .values()
        .filter(|n| record_expired(n, now))
        .map(|n| n.id.clone())
        .collect();
    for id in expired {
        forget(state, &id, now, out);
    }
}

/// The record goes for good. A master gives up its slots here, while its owner is known; the
/// next tick fills them and plans DNS as for any master that left.
fn forget(state: &mut PoolState, id: &NodeId, now: Ts, out: &mut Out) {
    let owner = state.nodes.get(id).map(|n| n.owner);
    let Assignments(assignments) = &mut state.assignments;
    for (domain, slots) in assignments.iter_mut() {
        let before = slots.len();
        slots.retain(|slot| slot.node != *id);
        if slots.len() == before {
            continue;
        }
        emit_owned(
            out,
            now,
            EventKind::MasterChanged,
            id,
            owner,
            Some(domain),
            json!({ "added": null, "removed": id }),
        );
        persist(
            out,
            if slots.is_empty() {
                PersistOp::DeleteMasters {
                    domain: domain.clone(),
                }
            } else {
                PersistOp::SetMasters {
                    domain: domain.clone(),
                    slots: slots.clone(),
                }
            },
        );
    }
    assignments.retain(|_, slots| !slots.is_empty());
    state.nodes.remove(id);
    state.stats.remove(id);
    persist(out, PersistOp::DeleteNode(id.clone()));
}

/// A record that is still in the pool: not `Banned`, `Dead` or `Pruned`.
fn is_live(state: NodeState) -> bool {
    matches!(
        state,
        NodeState::Pending | NodeState::Healthy | NodeState::Quarantine
    )
}

fn terminate_reason(state: NodeState) -> Option<TerminateReason> {
    match state {
        NodeState::Banned => Some(TerminateReason::Banned),
        NodeState::Dead => Some(TerminateReason::Dead),
        NodeState::Pruned => Some(TerminateReason::Pruned),
        NodeState::Pending | NodeState::Healthy | NodeState::Quarantine => None,
    }
}

fn kill(node: &mut NodeRecord, now: Ts) {
    node.enter_final(NodeState::Dead, now);
}

fn votes_json(votes: &[(SensorKind, Vote)]) -> Value {
    Value::Array(
        votes
            .iter()
            .map(|(kind, vote)| json!({ "sensor": kind, "vote": vote }))
            .collect(),
    )
}

fn add_ms(at: Ts, ms: i64) -> Ts {
    Ts(at.0.saturating_add(ms))
}

/// The recent verified Globalping result of the agent, as a vote: the round does not have to
/// spend a measurement of its own.
fn fresh_gp(node: &NodeRecord, now: Ts, s: &Settings) -> Option<SensorVote> {
    let (vote, at) = match node.health.gp {
        GpStatus::Ok { at } => (Vote::Clear, at),
        GpStatus::Failed { at } => (Vote::Blocked, at),
        GpStatus::Unknown | GpStatus::NoData { .. } => return None,
    };
    younger_than(at, now, s.globalping.vote_report_max_age_sec).then(|| SensorVote {
        vote,
        evidence: json!({ "source": "agent_report", "at": at.0 }),
    })
}

fn run_round(node: &mut NodeRecord, now: Ts, s: &Settings, out: &mut Out) {
    // The round is paced from its start, so a lost result is retried after the interval.
    node.last_round_at = Some(now);
    out.push(Effect::RunRound {
        node: node.id.clone(),
        generation: node.generation,
        ip: node.ip,
        port: node.port,
        fresh_gp: fresh_gp(node, now, s),
    });
}

/// Turns lifecycle transitions into events and the bookkeeping that belongs to them.
fn announce(
    out: &mut Out,
    tombstones: &mut Vec<Tombstone>,
    node: &NodeRecord,
    before: NodeState,
    events: Vec<LifecycleEvent>,
    now: Ts,
) {
    for event in events {
        match event {
            LifecycleEvent::Registered => {
                emit_node(out, now, EventKind::NodeRegistered, node, json!({}));
            }
            LifecycleEvent::Quarantined { reason } => {
                emit_node(
                    out,
                    now,
                    EventKind::NodeQuarantined,
                    node,
                    json!({ "reason": reason }),
                );
            }
            LifecycleEvent::Recovered => {
                reset_tombstones(tombstones, node);
                // The first move from `Pending` into the queue is not a recovery.
                if before == NodeState::Quarantine {
                    emit_node(out, now, EventKind::NodeRecovered, node, json!({}));
                }
            }
            LifecycleEvent::Banned { votes } => {
                persist(
                    out,
                    PersistOp::RecordBan {
                        ip: node.ip,
                        node: node.id.clone(),
                        at: now,
                        votes: votes.clone(),
                        reverify: false,
                        node_registered_at: Some(node.registered_at),
                    },
                );
                emit_node(
                    out,
                    now,
                    EventKind::NodeBanned,
                    node,
                    json!({ "votes": votes_json(&votes) }),
                );
            }
            LifecycleEvent::ReverifyFailed {
                retry_after_sec,
                votes,
            } => {
                persist(
                    out,
                    PersistOp::RecordBan {
                        ip: node.ip,
                        node: node.id.clone(),
                        at: now,
                        votes,
                        reverify: true,
                        node_registered_at: None,
                    },
                );
                // Not `NodeBanned`: the agent registers again every few minutes from a
                // blocked IP, and each failed round must not become a webhook.
                emit_node(
                    out,
                    now,
                    EventKind::NodeReverifyFailed,
                    node,
                    json!({ "retry_after_sec": retry_after_sec }),
                );
            }
            LifecycleEvent::Dead => {
                emit_node(
                    out,
                    now,
                    EventKind::NodeDead,
                    node,
                    json!({ "reason": "unreachable" }),
                );
            }
            LifecycleEvent::Pruned => {
                record_prune(tombstones, node, now);
                emit_node(out, now, EventKind::NodePruned, node, json!({}));
            }
            LifecycleEvent::BanLifted => {
                persist(
                    out,
                    PersistOp::LiftBan {
                        ip: node.ip,
                        at: now,
                        admin: false,
                    },
                );
                emit_node(out, now, EventKind::BanLifted, node, json!({}));
            }
        }
    }
}

fn persist_list_changes(
    out: &mut Out,
    old_blocks: &[IpBlock],
    new_blocks: &[IpBlock],
    old_tombstones: &[Tombstone],
    new_tombstones: &[Tombstone],
) {
    for block in new_blocks {
        if old_blocks.iter().find(|o| o.ip == block.ip) != Some(block) {
            persist(out, PersistOp::UpsertIpBlock(block.clone()));
        }
    }
    for old in old_blocks {
        if !new_blocks.iter().any(|b| b.ip == old.ip) {
            persist(out, PersistOp::DeleteIpBlock { ip: old.ip });
        }
    }
    let same = |a: &Tombstone, b: &Tombstone| a.node_id == b.node_id && a.ip == b.ip;
    for t in new_tombstones {
        if old_tombstones.iter().find(|o| same(o, t)) != Some(t) {
            persist(out, PersistOp::UpsertTombstone(t.clone()));
        }
    }
    for old in old_tombstones {
        if !new_tombstones.iter().any(|t| same(t, old)) {
            persist(
                out,
                PersistOp::DeleteTombstone {
                    node_id: old.node_id.clone(),
                    ip: old.ip,
                },
            );
        }
    }
}

// ---- enrollment and agent traffic ----

fn enroll(state: &mut PoolState, r: EnrollRequest, now: Ts, out: &mut Out) -> Answer {
    let input = EnrollInput {
        node_id: r.node_id.clone(),
        ip: r.ip,
    };
    let decision = {
        let existing: Vec<&NodeRecord> = state.nodes.values().collect();
        decide_enroll(&input, &existing, &state.blocks, &state.tombstones, now)
    };
    let (old_on_ip, reverify) = match decision {
        EnrollDecision::Reject {
            retry_after_sec,
            reason,
        } => {
            return (
                r.reply,
                Reply::EnrollRejected {
                    retry_after_sec,
                    reason,
                },
            );
        }
        EnrollDecision::Accept => (None, false),
        EnrollDecision::Replace { old } => (Some(old), false),
        EnrollDecision::Reverify => (None, true),
    };

    // Another node on this IP gives way: one live record per IP. The same id enrolling again
    // is not a displaced node, its record is replaced below.
    let displaced = old_on_ip.filter(|old| *old != r.node_id);
    let slot = decide_slot(
        &SlotRequest {
            node_id: &r.node_id,
            owner: r.owner,
            limit: r.limit,
            displaced: displaced.as_ref(),
            may_evict: !reverify && state.evictions_from.is_none_or(|from| from <= now),
        },
        state.nodes.values(),
        now,
        &state.settings,
    );
    let evicted = match slot {
        SlotDecision::Free => None,
        SlotDecision::Evict(silent) => Some(silent),
        SlotDecision::Full { limit, current } => {
            return (r.reply, Reply::OwnerLimit { limit, current });
        }
    };
    if let Some(old) = evicted.as_ref().and_then(|id| state.nodes.get_mut(id)) {
        kill(old, now);
        persist(out, PersistOp::UpsertNode(old.clone()));
        emit_node(
            out,
            now,
            EventKind::NodeDead,
            old,
            json!({ "replaced_by": r.node_id, "reason": "silent" }),
        );
    }
    if let Some(old) = displaced.as_ref().and_then(|id| state.nodes.get_mut(id)) {
        kill(old, now);
        persist(out, PersistOp::UpsertNode(old.clone()));
        emit_node(
            out,
            now,
            EventKind::NodeDead,
            old,
            json!({ "replaced_by": r.node_id }),
        );
    }
    let replaced = displaced.or(evicted);

    let make = if reverify {
        NodeRecord::new_reverify
    } else {
        NodeRecord::new
    };
    let mut record = make(
        r.node_id.clone(),
        r.owner,
        r.ip,
        r.port,
        r.engine,
        now,
        &state.settings,
    );
    if let Some(previous) = state.nodes.get(&r.node_id) {
        record = record.replacing(previous);
    }

    let generation = record.generation;
    persist(out, PersistOp::UpsertNode(record.clone()));
    state.stats.insert(r.node_id.clone(), NodeStats::default());
    persist(
        out,
        PersistOp::NodeStats {
            node: r.node_id.clone(),
            stats: NodeStats::default(),
        },
    );
    emit_node(
        out,
        now,
        EventKind::NodeRegistered,
        &record,
        json!({ "reverify": reverify, "replaced": replaced, "ip": r.ip.to_string() }),
    );
    // A reverify node starts its decisive round with the first measurement of the agent, which
    // the agent makes right after registering (see `report`); the tick is the fallback.
    state.nodes.insert(r.node_id.clone(), record);
    (
        r.reply,
        Reply::Enrolled {
            node: r.node_id,
            generation,
            replaced,
            reverify,
        },
    )
}

fn heartbeat(state: &mut PoolState, r: HeartbeatRequest, now: Ts, out: &mut Out) -> Answer {
    let Some(current) = state.nodes.get(&r.node) else {
        return (r.reply, Reply::UnknownNode);
    };
    if let Some(reason) = terminate_reason(current.state) {
        return (r.reply, Reply::Terminate(reason));
    }
    let moved = current.ip != r.ip;
    let decision = moved.then(|| {
        let others: Vec<&NodeRecord> = state.nodes.values().filter(|n| n.id != r.node).collect();
        decide_enroll(
            &EnrollInput {
                node_id: r.node.clone(),
                ip: r.ip,
            },
            &others,
            &state.blocks,
            &state.tombstones,
            now,
        )
    });
    let Some(node) = state.nodes.get_mut(&r.node) else {
        return (r.reply, Reply::UnknownNode);
    };
    match decision {
        None => {
            node.health.last_heartbeat = now;
            persist(
                out,
                PersistOp::NodeHealth {
                    node: node.id.clone(),
                    health: node.health,
                },
            );
        }
        Some(EnrollDecision::Accept) => {
            // A new IP is a new check: the node starts over as `Pending`.
            let from = node.ip;
            node.change_ip(r.ip, now);
            node.health.last_heartbeat = now;
            persist(out, PersistOp::UpsertNode(node.clone()));
            emit_node(
                out,
                now,
                EventKind::NodeIpChanged,
                node,
                json!({ "from": from.to_string(), "to": r.ip.to_string() }),
            );
        }
        Some(_) => {
            // The new address may not enroll: blocked, paused, or taken by another node.
            kill(node, now);
            persist(out, PersistOp::UpsertNode(node.clone()));
            emit_node(
                out,
                now,
                EventKind::NodeDead,
                node,
                json!({ "reason": "address_refused" }),
            );
            return (r.reply, Reply::Terminate(TerminateReason::AddressRefused));
        }
    }
    (r.reply, Reply::Ok)
}

fn report(state: &mut PoolState, r: ReportRequest, now: Ts, out: &mut Out) -> Answer {
    let PoolState {
        settings,
        nodes,
        stats,
        ..
    } = state;
    let Some(node) = nodes.get_mut(&r.node) else {
        return (r.reply, Reply::UnknownNode);
    };
    if let Some(reason) = terminate_reason(node.state) {
        return (r.reply, Reply::Terminate(reason));
    }
    if r.generation != node.generation {
        return (r.reply, Reply::Stale);
    }
    node.health.metrics.observe(
        r.metrics_ok,
        settings.latch_fail_threshold,
        settings.latch_ok_threshold,
    );
    node.health.pool_user_taken = r.pool_user_taken;
    match r.globalping {
        Vote::Clear => {
            node.health.gp = GpStatus::Ok { at: now };
            node.health.last_report = now;
        }
        Vote::Blocked => {
            node.health.gp = GpStatus::Failed { at: now };
            node.health.last_report = now;
        }
        // No measurement this time: the agent reports more often than it measures, so the
        // last verdict stands. The report time stays, and a long run of them ages it out.
        Vote::Abstain(AbstainReason::NoData) => {}
        // A measurement that could not be made neither proves nor refutes.
        Vote::Abstain(_) => node.health.gp = node.health.gp.on_no_data(now),
    }
    persist(
        out,
        PersistOp::NodeHealth {
            node: node.id.clone(),
            health: node.health,
        },
    );

    if r.traffic.is_some() || r.clients.is_some() || r.other_clients.is_some() {
        let entry = stats.entry(r.node.clone()).or_default();
        if let Some(counters) = r.traffic {
            // An agent that tells the pool's users from the others reports another number than
            // one that sums them all. When that changes, a counter going up or down says nothing
            // about traffic: the reading only sets the base, like the first one ever does.
            let same_kind = r.other_traffic.is_some() == entry.other_counters.is_some();
            // The first reading only sets the base: what came before the registrar looked
            // is not traffic of this pool.
            let (from, to) = entry
                .counters
                .filter(|_| same_kind)
                .map_or((0, 0), |previous| {
                    (
                        traffic_delta(previous.from_client, counters.from_client),
                        traffic_delta(previous.to_client, counters.to_client),
                    )
                });
            entry.from_client_total = entry.from_client_total.saturating_add(from);
            entry.to_client_total = entry.to_client_total.saturating_add(to);
            entry.counters = Some(counters);
            let (other_from, other_to) = match (entry.other_counters, r.other_traffic) {
                (Some(previous), Some(current)) => (
                    traffic_delta(previous.from_client, current.from_client),
                    traffic_delta(previous.to_client, current.to_client),
                ),
                _ => (0, 0),
            };
            entry.other_from_client_total =
                entry.other_from_client_total.saturating_add(other_from);
            entry.other_to_client_total = entry.other_to_client_total.saturating_add(other_to);
            entry.other_counters = r.other_traffic;
            if from > 0 || to > 0 || other_from > 0 || other_to > 0 {
                persist(
                    out,
                    PersistOp::Traffic {
                        node: r.node.clone(),
                        at: now,
                        from_client: from,
                        to_client: to,
                        other_from_client: other_from,
                        other_to_client: other_to,
                    },
                );
            }
            if r.other_traffic.is_none() {
                entry.other_clients = None;
            }
        }
        if r.clients.is_some() {
            entry.clients = r.clients;
        }
        if r.other_clients.is_some() {
            entry.other_clients = r.other_clients;
        }
        persist(
            out,
            PersistOp::NodeStats {
                node: r.node.clone(),
                stats: entry.clone(),
            },
        );
    }

    // The first verdict of the agent on a reverify node is the vote of its decisive round.
    if node.reverify
        && is_live(node.state)
        && node.last_round_at.is_none()
        && !matches!(r.globalping, Vote::Abstain(_))
    {
        run_round(node, now, settings, out);
        persist(out, PersistOp::UpsertNode(node.clone()));
    }
    (r.reply, Reply::Ok)
}

fn tcp_probe(
    state: &mut PoolState,
    node: &NodeId,
    generation: Generation,
    open: bool,
    out: &mut Out,
) {
    let PoolState {
        settings, nodes, ..
    } = state;
    let Some(n) = nodes.get_mut(node) else { return };
    if n.generation != generation || !is_live(n.state) {
        return;
    }
    n.health.tcp.observe(
        open,
        settings.latch_fail_threshold,
        settings.latch_ok_threshold,
    );
    persist(
        out,
        PersistOp::NodeHealth {
            node: n.id.clone(),
            health: n.health,
        },
    );
}

fn round_done(
    state: &mut PoolState,
    node: &NodeId,
    generation: Generation,
    votes: Vec<(SensorKind, SensorVote)>,
    now: Ts,
    out: &mut Out,
) {
    let PoolState {
        settings,
        nodes,
        blocks,
        tombstones,
        ..
    } = state;
    let Some(n) = nodes.get_mut(node) else { return };
    if n.generation != generation {
        return;
    }
    let before = n.clone();
    let result = RoundResult {
        generation,
        votes: votes.iter().map(|(kind, v)| (*kind, v.vote)).collect(),
    };
    let events = apply_round(n, &result, blocks, now, settings);
    if events.is_empty() && *n == before {
        // The node is not in a state that takes a round: a late result.
        return;
    }
    let verdict: RoundVerdict = judge_round(&result.votes);
    persist(
        out,
        PersistOp::RecordRound {
            node: n.id.clone(),
            generation,
            at: now,
            verdict,
            votes,
        },
    );
    persist(out, PersistOp::UpsertNode(n.clone()));
    emit_node(
        out,
        now,
        EventKind::SensorRound,
        n,
        json!({ "verdict": verdict, "votes": votes_json(&result.votes) }),
    );
    announce(out, tombstones, n, before.state, events, now);
}

// ---- commands from people ----

fn retire(state: &mut PoolState, a: NodeAction, now: Ts, out: &mut Out) -> Answer {
    let Some(node) = state.nodes.get_mut(&a.node) else {
        return (a.reply, Reply::UnknownNode);
    };
    if is_live(node.state) {
        kill(node, now);
        persist(out, PersistOp::UpsertNode(node.clone()));
        emit_node(
            out,
            now,
            EventKind::NodeDead,
            node,
            json!({ "retired": true }),
        );
    }
    (a.reply, Reply::Ok)
}

/// Lifts the ban whether or not the record is still there: a banned node has none, its block
/// remembers the id.
fn admin_unban(state: &mut PoolState, a: NodeAction, now: Ts, out: &mut Out) -> Answer {
    let PoolState { nodes, blocks, .. } = state;
    let known = nodes.contains_key(&a.node) || blocks.iter().any(|b| b.node_id == a.node);
    if !known {
        return (a.reply, Reply::UnknownNode);
    }
    let mut owner = None;
    let mut lifted = Vec::new();
    if let Some(node) = nodes.get_mut(&a.node) {
        owner = Some(node.owner);
        if !unban(node, blocks, now).is_empty() {
            persist(out, PersistOp::UpsertNode(node.clone()));
            lifted.push(node.ip);
        }
    }
    let (by_id, released) = unban_by_node(&a.node, nodes, blocks);
    for id in released {
        if let Some(node) = nodes.get(&id) {
            persist(out, PersistOp::UpsertNode(node.clone()));
        }
    }
    for block in by_id {
        owner = owner.or(block.owner);
        lifted.push(block.ip);
    }
    if lifted.is_empty() {
        return (a.reply, Reply::Refused(Refusal::NotBanned));
    }
    for ip in lifted {
        persist(
            out,
            PersistOp::LiftBan {
                ip,
                at: now,
                admin: true,
            },
        );
    }
    emit_owned(
        out,
        now,
        EventKind::BanLifted,
        &a.node,
        owner,
        None,
        json!({ "admin": true }),
    );
    (a.reply, Reply::Ok)
}

fn recheck(state: &mut PoolState, a: NodeAction, now: Ts, out: &mut Out) -> Answer {
    let PoolState {
        settings, nodes, ..
    } = state;
    let Some(node) = nodes.get_mut(&a.node) else {
        return (a.reply, Reply::UnknownNode);
    };
    if !needs_sensors(node, now, settings) {
        return (a.reply, Reply::Refused(Refusal::NotRecheckable));
    }
    run_round(node, now, settings, out);
    persist(out, PersistOp::UpsertNode(node.clone()));
    (a.reply, Reply::Ok)
}

fn srmd_manual(state: &mut PoolState, r: SrmdManualRequest, now: Ts, out: &mut Out) -> Answer {
    if !state.settings.domains.contains(&r.domain) {
        return (r.reply, Reply::Refused(Refusal::UnknownDomain));
    }
    let outcome = match r.action {
        SrmdManualAction::Take => take(&mut state.srmd, &r.domain).map(|()| false),
        SrmdManualAction::Release => {
            release(&mut state.srmd, &r.domain).map(|unfolded| unfolded.is_some())
        }
    };
    match outcome {
        Err(SrmdError::AlreadyManaged(_)) => (r.reply, Reply::Refused(Refusal::AlreadyManaged)),
        Err(SrmdError::NotManaged(_)) => (r.reply, Reply::Refused(Refusal::NotManaged)),
        Ok(unfolded) => {
            let action = match r.action {
                SrmdManualAction::Take => "take",
                SrmdManualAction::Release => "release",
            };
            emit(
                out,
                now,
                EventKind::SrmdAction,
                None,
                Some(&r.domain),
                json!({ "action": action, "unfolded": unfolded }),
            );
            persist(out, PersistOp::SetSrmd(state.srmd.clone()));
            (r.reply, Reply::Ok)
        }
    }
}

fn settings_changed(state: &mut PoolState, new: Settings, out: &mut Out) {
    // The caller reports validation errors to the admin; the pool never runs on bad settings.
    if new.validate().is_err() || new == state.settings {
        return;
    }
    state.settings = new;
    persist(out, PersistOp::SetSettings(state.settings.clone()));
    for node in state.nodes.values_mut() {
        let port_ok = node.port == state.settings.proxy_port;
        if node.health.port_ok != port_ok {
            node.health.port_ok = port_ok;
            persist(
                out,
                PersistOp::NodeHealth {
                    node: node.id.clone(),
                    health: node.health,
                },
            );
        }
    }
}

// ---- the tick ----

fn tick(state: &mut PoolState, now: Ts, out: &mut Out) {
    tick_nodes(state, now, out);
    let active: Vec<String> = state
        .settings
        .domains
        .iter()
        .filter(|d| !state.srmd.cnames.contains_key(d.as_str()))
        .cloned()
        .collect();
    update_masters(state, &active, now, out);
    run_srmd(state, &active, now, out);
    update_dns(state, now, out);
}

fn tick_nodes(state: &mut PoolState, now: Ts, out: &mut Out) {
    let PoolState {
        settings,
        nodes,
        tombstones,
        ..
    } = state;
    for node in nodes.values_mut() {
        let before = node.clone();
        let events = tick_node(node, now, settings);
        announce(out, tombstones, node, before.state, events, now);
        // A reverify node waits for the measurement of its agent; without one, the round
        // starts when a report would have gone stale.
        let waits_for_report = node.reverify && node.last_round_at.is_none();
        let wait_over = add_ms(
            node.state_since,
            i64::from(settings.report_max_age_sec) * 1000,
        );
        let round_due = needs_sensors(node, now, settings)
            && next_round_at(node, settings).is_some_and(|at| at <= now)
            && (!waits_for_report || wait_over <= now);
        if round_due {
            run_round(node, now, settings, out);
        }
        if *node != before {
            persist(out, PersistOp::UpsertNode(node.clone()));
        }
    }
}

fn update_masters(state: &mut PoolState, active: &[String], now: Ts, out: &mut Out) {
    let nodes: Vec<NodeRecord> = state.nodes.values().cloned().collect();
    // `select` keeps a master that is not healthy while nobody can replace it. That is meant
    // for a node that may come back; a node that is gone for good (and may share its IP with
    // the node that took the address over) never does.
    let mut previous = state.assignments.clone();
    for (domain, slots) in &mut previous.0 {
        slots.retain(|slot| {
            let gone = state
                .nodes
                .get(&slot.node)
                .is_none_or(|n| !is_live(n.state));
            if gone {
                emit(
                    out,
                    now,
                    EventKind::MasterChanged,
                    Some(&slot.node),
                    Some(domain),
                    json!({ "added": null, "removed": slot.node }),
                );
            }
            !gone
        });
    }
    let result = select(&previous, &nodes, active, now, &state.settings);
    let mut assignments = result.assignments;
    assignments.0.retain(|_, slots| !slots.is_empty());

    for change in &result.changes {
        emit(
            out,
            now,
            EventKind::MasterChanged,
            change.added.as_ref().or(change.removed.as_ref()),
            Some(&change.domain),
            json!({ "added": change.added, "removed": change.removed }),
        );
    }
    // `select` leaves the nodes alone: a node that lost its slot to the rotation TTL goes to
    // the back of the queue here.
    for id in &result.rotated_out {
        if let Some(node) = state.nodes.get_mut(id) {
            node.queued_at = now;
            persist(out, PersistOp::UpsertNode(node.clone()));
        }
    }
    for (domain, slots) in &assignments.0 {
        if state.assignments.0.get(domain) != Some(slots) {
            persist(
                out,
                PersistOp::SetMasters {
                    domain: domain.clone(),
                    slots: slots.clone(),
                },
            );
        }
    }
    for domain in state.assignments.0.keys() {
        if !assignments.0.contains_key(domain) {
            persist(
                out,
                PersistOp::DeleteMasters {
                    domain: domain.clone(),
                },
            );
        }
    }
    state.assignments = assignments;
}

fn run_srmd(state: &mut PoolState, active: &[String], now: Ts, out: &mut Out) {
    let PoolState {
        settings,
        nodes,
        stats,
        assignments,
        srmd,
        ..
    } = state;
    // The clients of a domain are what its masters report. A node that masters several
    // domains is counted in each of them.
    for domain in active {
        let reported = assignments
            .0
            .get(domain)
            .into_iter()
            .flatten()
            .filter_map(|slot| stats.get(&slot.node)?.clients)
            .reduce(u64::saturating_add);
        set_clients(srmd, domain, reported);
    }
    let queue = nodes
        .values()
        .filter(|n| n.state == NodeState::Healthy)
        .count();
    let before = (srmd.created.clone(), srmd.cnames.clone());
    let actions = rebalance(srmd, active, queue, settings);

    let mut settings_changed = false;
    for action in actions {
        let (domain, detail) = match action {
            SrmdAction::Create(domain) => {
                if !settings.domains.contains(&domain) {
                    settings.domains.push(domain.clone());
                    settings_changed = true;
                }
                (domain, json!({ "action": "create" }))
            }
            SrmdAction::Unfold(domain) => (domain, json!({ "action": "unfold" })),
            SrmdAction::Fold { domain, target } => {
                (domain, json!({ "action": "fold", "target": target }))
            }
        };
        emit(out, now, EventKind::SrmdAction, None, Some(&domain), detail);
    }
    if settings_changed {
        persist(out, PersistOp::SetSettings(settings.clone()));
    }
    if (&srmd.created, &srmd.cnames) != (&before.0, &before.1) {
        persist(out, PersistOp::SetSrmd(srmd.clone()));
    }
}

/// What a domain should resolve to, or `None` when there is nothing to publish: an empty set
/// of addresses is never pushed.
fn desired_record(domain: &str, state: &PoolState) -> Option<RecordValue> {
    if let Some(target) = state.srmd.cnames.get(domain) {
        return Some(RecordValue::Cname(target.clone()));
    }
    // A master that left the pool is not an address to publish.
    let masters: Vec<&NodeRecord> = state
        .assignments
        .0
        .get(domain)?
        .iter()
        .filter_map(|slot| state.nodes.get(&slot.node))
        .collect();
    let healthy: Vec<&&NodeRecord> = masters
        .iter()
        .filter(|n| n.state == NodeState::Healthy)
        .collect();
    // Selection keeps a master that is not healthy while nobody can take its place; the
    // domain then still resolves to it rather than to nothing.
    let mut ips: Vec<Ipv4Addr> = if healthy.is_empty() {
        masters.iter().map(|n| n.ip).collect()
    } else {
        healthy.iter().map(|n| n.ip).collect()
    };
    ips.sort_unstable();
    ips.dedup();
    (!ips.is_empty()).then_some(RecordValue::A(ips))
}

fn update_dns(state: &mut PoolState, now: Ts, out: &mut Out) {
    let domains = state.settings.domains.clone();
    for domain in domains {
        let Some(desired) = desired_record(&domain, state) else {
            continue;
        };
        let mut created = false;
        let entry = state.dns.entry(domain.clone()).or_insert_with(|| {
            created = true;
            DnsState {
                desired: desired.clone(),
                applied: None,
                generation: Generation(1),
                retries: 0,
                next_try: now,
                in_flight: None,
            }
        });
        let mut changed = created;
        if entry.desired != desired {
            // A new target: the push in flight and the backoff belong to the old one.
            entry.desired = desired;
            entry.generation = entry.generation.next();
            entry.retries = 0;
            entry.next_try = now;
            entry.in_flight = None;
            changed = true;
        }
        let push = entry.applied.as_ref() != Some(&entry.desired)
            && now >= entry.next_try
            && entry.in_flight != Some(entry.generation);
        if push {
            entry.in_flight = Some(entry.generation);
        }
        if changed {
            persist(
                out,
                PersistOp::UpsertDns {
                    domain: domain.clone(),
                    dns: entry.clone(),
                },
            );
        }
        if push {
            out.push(Effect::PushDns {
                domain,
                generation: entry.generation,
                desired: entry.desired.clone(),
            });
        }
    }
}

/// Marks the record of every domain as not applied, so the next tick writes all of them.
fn dns_push_all(state: &mut PoolState, now: Ts, out: &mut Out) {
    for (domain, entry) in &mut state.dns {
        if entry.in_flight.is_some() {
            continue;
        }
        entry.applied = None;
        entry.retries = 0;
        entry.next_try = now;
        persist(
            out,
            PersistOp::UpsertDns {
                domain: domain.clone(),
                dns: entry.clone(),
            },
        );
    }
}

/// Stores the fresh availability. Nodes without samples lose a stale value.
fn availability_refreshed(state: &mut PoolState, list: Vec<(NodeId, u32)>) {
    for stats in state.stats.values_mut() {
        stats.availability_bp = None;
    }
    for (node, bp) in list {
        if let Some(stats) = state.stats.get_mut(&node) {
            stats.availability_bp = Some(bp);
        }
    }
}

fn dns_done(
    state: &mut PoolState,
    domain: &str,
    generation: Generation,
    result: Result<(), String>,
    now: Ts,
    out: &mut Out,
) {
    let Some(entry) = state.dns.get_mut(domain) else {
        return;
    };
    if entry.generation != generation {
        return;
    }
    entry.in_flight = None;
    match result {
        Ok(()) => {
            entry.applied = Some(entry.desired.clone());
            entry.retries = 0;
            let detail = match &entry.desired {
                RecordValue::A(addresses) => json!({
                    "records": addresses.len(),
                    "addresses": addresses.iter().map(ToString::to_string).collect::<Vec<_>>(),
                }),
                RecordValue::Cname(target) => json!({ "records": 1, "target": target }),
            };
            emit(out, now, EventKind::DnsApplied, None, Some(domain), detail);
        }
        Err(message) => {
            let wait = backoff(entry.retries);
            entry.next_try = add_ms(now, i64::try_from(wait.as_millis()).unwrap_or(i64::MAX));
            entry.retries = entry.retries.saturating_add(1);
            emit(
                out,
                now,
                EventKind::DnsFailed,
                None,
                Some(domain),
                json!({ "error": message, "retry_in_sec": wait.as_secs() }),
            );
        }
    }
    persist(
        out,
        PersistOp::UpsertDns {
            domain: domain.to_owned(),
            dns: entry.clone(),
        },
    );
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use serde_json::json;

    use super::*;
    use crate::dns::RecordValue;
    use crate::health::{FailedCondition, GpStatus};
    use crate::lifecycle::{IpBlock, RejectReason};
    use crate::selection::Slot;
    use crate::settings::Settings;
    use crate::state::{
        EnrollRequest, EventKind, HeartbeatRequest, NodeAction, PersistOp, Refusal, Reply, ReplyId,
        ReportRequest, SrmdManualAction, SrmdManualRequest, TerminateReason,
    };
    use crate::traffic::TrafficCounters;
    use crate::types::{EngineKind, Generation, NodeId, NodeState, OwnerId};
    use crate::votes::{AbstainReason, SensorKind, SensorVote, Vote};

    const SEC: i64 = 1000;
    const DOMAIN: &str = "p.example.com";

    fn ip(last: u8) -> Ipv4Addr {
        Ipv4Addr::new(95, 216, 0, last)
    }

    fn id(name: &str) -> NodeId {
        NodeId::parse(&format!("{name}-a1b2c")).unwrap()
    }

    /// Latches that flip on one probe, one clean round to leave quarantine, one master.
    fn settings() -> Settings {
        Settings {
            latch_fail_threshold: 1,
            latch_ok_threshold: 1,
            clean_rounds_to_exit: 1,
            masters_per_domain: 1,
            rotation_ttl_sec: 0,
            domains: vec![DOMAIN.to_owned()],
            ..Settings::default()
        }
    }

    struct Pool {
        state: PoolState,
        now: Ts,
        reply: u64,
    }

    impl Pool {
        fn new() -> Pool {
            Pool::with(settings())
        }

        fn with(s: Settings) -> Pool {
            Pool {
                state: PoolState::new(s),
                now: Ts(1_000_000),
                reply: 0,
            }
        }

        fn advance(&mut self, ms: i64) {
            self.now = Ts(self.now.0 + ms);
        }

        fn run(&mut self, command: Command) -> Vec<Effect> {
            apply(&mut self.state, command, self.now)
        }

        fn rid(&mut self) -> ReplyId {
            self.reply += 1;
            ReplyId(self.reply)
        }

        fn enroll(&mut self, name: &str, addr: Ipv4Addr) -> Vec<Effect> {
            let reply = self.rid();
            self.run(Command::Enroll(EnrollRequest {
                reply,
                node_id: id(name),
                owner: OwnerId(1),
                ip: addr,
                port: 443,
                engine: EngineKind::Telemt,
                limit: None,
            }))
        }

        fn beat(&mut self, name: &str) -> Vec<Effect> {
            let addr = self.state.nodes[&id(name)].ip;
            self.beat_from(name, addr)
        }

        fn beat_from(&mut self, name: &str, addr: Ipv4Addr) -> Vec<Effect> {
            let reply = self.rid();
            self.run(Command::Heartbeat(HeartbeatRequest {
                reply,
                node: id(name),
                ip: addr,
            }))
        }

        fn generation(&self, name: &str) -> Generation {
            self.state.nodes[&id(name)].generation
        }

        fn probe(&mut self, name: &str, open: bool) -> Vec<Effect> {
            let generation = self.generation(name);
            self.run(Command::TcpProbe {
                node: id(name),
                generation,
                open,
            })
        }

        fn report(&mut self, name: &str, gp: Vote) -> Vec<Effect> {
            let reply = self.rid();
            let generation = self.generation(name);
            self.run(Command::Report(ReportRequest {
                reply,
                node: id(name),
                generation,
                metrics_ok: true,
                globalping: gp,
                traffic: None,
                clients: None,
                other_traffic: None,
                other_clients: None,
                pool_user_taken: false,
            }))
        }

        fn round(&mut self, name: &str, votes: &[(SensorKind, Vote)]) -> Vec<Effect> {
            let generation = self.generation(name);
            let votes = votes
                .iter()
                .map(|&(k, vote)| {
                    (
                        k,
                        SensorVote {
                            vote,
                            evidence: json!({ "test": true }),
                        },
                    )
                })
                .collect();
            self.run(Command::RoundDone {
                node: id(name),
                generation,
                votes,
            })
        }

        fn node_action(&mut self, name: &str) -> NodeAction {
            NodeAction {
                reply: self.rid(),
                node: id(name),
            }
        }

        /// Heartbeat, open port, a clean report and a tick: the node ends `Healthy`.
        fn make_healthy(&mut self, name: &str, addr: Ipv4Addr) {
            self.make_healthy_with_effects(name, addr);
        }

        /// Like `make_healthy`, and returns the effects of the tick that finished the job.
        fn make_healthy_with_effects(&mut self, name: &str, addr: Ipv4Addr) -> Vec<Effect> {
            self.enroll(name, addr);
            self.beat(name);
            self.probe(name, true);
            self.report(name, Vote::Clear);
            self.advance(SEC);
            let effects = self.run(Command::Tick);
            assert_eq!(self.state.nodes[&id(name)].state, NodeState::Healthy);
            effects
        }

        /// A failed Globalping report and a tick: the node ends in `Quarantine`.
        fn quarantine(&mut self, name: &str) -> Vec<Effect> {
            self.advance(SEC);
            self.beat(name);
            self.report(name, Vote::Blocked);
            let effects = self.run(Command::Tick);
            assert_eq!(self.state.nodes[&id(name)].state, NodeState::Quarantine);
            effects
        }

        fn state_of(&self, name: &str) -> NodeState {
            self.state.nodes[&id(name)].state
        }
    }

    fn kinds(effects: &[Effect]) -> Vec<EventKind> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Emit(ev) => Some(ev.kind),
                _ => None,
            })
            .collect()
    }

    fn reply(effects: &[Effect]) -> &Reply {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::Reply(_, r) => Some(r),
                _ => None,
            })
            .expect("a reply")
    }

    fn persists(effects: &[Effect]) -> Vec<&PersistOp> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Persist(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn pushes(effects: &[Effect]) -> Vec<&Effect> {
        effects
            .iter()
            .filter(|e| matches!(e, Effect::PushDns { .. }))
            .collect()
    }

    fn clean() -> [(SensorKind, Vote); 2] {
        [
            (SensorKind::Globalping, Vote::Clear),
            (SensorKind::Cheburcheck, Vote::Clear),
        ]
    }

    fn blocked() -> [(SensorKind, Vote); 2] {
        [
            (SensorKind::Globalping, Vote::Blocked),
            (SensorKind::Cheburcheck, Vote::Blocked),
        ]
    }

    #[test]
    fn event_emitted_for_every_state_change() {
        let mut p = Pool::new();

        // Registered.
        let effects = p.enroll("n1", ip(1));
        assert_eq!(kinds(&effects), [EventKind::NodeRegistered]);
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled {
                reverify: false,
                ..
            }
        ));

        // Pending to Healthy for the first time: silent, but the master is announced.
        p.beat("n1");
        p.probe("n1", true);
        p.report("n1", Vote::Clear);
        p.advance(SEC);
        let effects = p.run(Command::Tick);
        assert_eq!(p.state_of("n1"), NodeState::Healthy);
        assert_eq!(kinds(&effects), [EventKind::MasterChanged]);

        // Healthy to Quarantine, and the failed check is proven by a round right away.
        let effects = p.quarantine("n1");
        assert!(kinds(&effects).contains(&EventKind::NodeQuarantined));
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::RunRound {
                fresh_gp: Some(SensorVote {
                    vote: Vote::Blocked,
                    ..
                }),
                ..
            }
        )));

        // Quarantine to Healthy: Recovered, and the round is recorded.
        p.advance(SEC);
        p.beat("n1");
        p.report("n1", Vote::Clear);
        let effects = p.round("n1", &clean());
        assert_eq!(p.state_of("n1"), NodeState::Healthy);
        assert_eq!(
            kinds(&effects),
            [EventKind::SensorRound, EventKind::NodeRecovered]
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::RecordRound { .. }))
        );

        // Quarantine to Banned: the block stays, the record goes.
        p.quarantine("n1");
        let effects = p.round("n1", &blocked());
        assert!(!p.state.nodes.contains_key(&id("n1")));
        assert_eq!(
            kinds(&effects),
            [
                EventKind::SensorRound,
                EventKind::NodeBanned,
                EventKind::MasterChanged
            ]
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::UpsertIpBlock(b) if b.ip == ip(1)))
        );

        // An admin lifts the ban; the agent enrolls again by itself.
        let action = p.node_action("n1");
        let effects = p.run(Command::AdminUnban(action));
        assert_eq!(kinds(&effects), [EventKind::BanLifted]);
        assert!(matches!(reply(&effects), Reply::Ok));
        assert!(p.state.blocks.is_empty());
        p.enroll("n1", ip(1));
        assert_eq!(p.state_of("n1"), NodeState::Pending);

        // Pending to Dead: no heartbeat for the dead timer.
        p.advance(SEC);
        p.run(Command::Tick);
        p.advance(601 * SEC);
        let effects = p.run(Command::Tick);
        assert_eq!(p.state_of("n1"), NodeState::Dead);
        assert!(kinds(&effects).contains(&EventKind::NodeDead));

        // Quarantine to Pruned, and the pause that follows.
        p.make_healthy("n2", ip(2));
        p.quarantine("n2");
        p.advance(3601 * SEC);
        p.beat("n2");
        let effects = p.run(Command::Tick);
        assert_eq!(p.state_of("n2"), NodeState::Pruned);
        assert!(kinds(&effects).contains(&EventKind::NodePruned));
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::UpsertTombstone(t) if t.node_id == id("n2")))
        );
        let effects = p.enroll("n2", ip(2));
        assert!(matches!(
            reply(&effects),
            Reply::EnrollRejected {
                reason: RejectReason::PrunePause,
                ..
            }
        ));
        assert!(kinds(&effects).is_empty());

        // A retired node is Dead.
        p.make_healthy("n3", ip(3));
        let action = p.node_action("n3");
        let effects = p.run(Command::Retire(action));
        assert_eq!(p.state_of("n3"), NodeState::Dead);
        assert!(kinds(&effects).contains(&EventKind::NodeDead));
        let action = p.node_action("n3");
        let effects = p.run(Command::Retire(action));
        assert!(
            kinds(&effects).is_empty(),
            "a second retire changes nothing"
        );
        assert!(matches!(reply(&effects), Reply::Ok));
    }

    #[test]
    fn recovery_resets_the_tombstones_of_the_node() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.state.tombstones.push(crate::lifecycle::Tombstone {
            node_id: id("n1"),
            ip: ip(1),
            strikes: 2,
            until: Ts(0),
            updated_at: p.now,
        });
        p.quarantine("n1");
        p.advance(SEC);
        p.beat("n1");
        p.report("n1", Vote::Clear);
        let effects = p.round("n1", &clean());
        assert_eq!(p.state_of("n1"), NodeState::Healthy);
        assert!(p.state.tombstones.is_empty());
        assert!(persists(&effects).iter().any(
            |op| matches!(op, PersistOp::DeleteTombstone { node_id, .. } if *node_id == id("n1"))
        ));
    }

    #[test]
    fn stale_generation_changes_nothing() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.run(Command::Tick);
        let before = p.state.clone();
        let old = Generation(p.generation("n1").0 + 7);

        let effects = p.run(Command::TcpProbe {
            node: id("n1"),
            generation: old,
            open: false,
        });
        assert!(effects.is_empty());
        let effects = p.run(Command::RoundDone {
            node: id("n1"),
            generation: old,
            votes: vec![(
                SensorKind::Globalping,
                SensorVote {
                    vote: Vote::Blocked,
                    evidence: json!(null),
                },
            )],
        });
        assert!(effects.is_empty());
        let effects = p.run(Command::DnsDone {
            domain: DOMAIN.to_owned(),
            generation: Generation(99),
            result: Ok(()),
        });
        assert!(effects.is_empty());
        let reply_id = p.rid();
        let effects = p.run(Command::Report(ReportRequest {
            reply: reply_id,
            node: id("n1"),
            generation: old,
            metrics_ok: false,
            globalping: Vote::Blocked,
            traffic: None,
            clients: None,
            other_traffic: None,
            other_clients: None,
            pool_user_taken: false,
        }));
        assert_eq!(effects, [Effect::Reply(reply_id, Reply::Stale)]);
        assert_eq!(p.state, before);
    }

    #[test]
    fn report_without_data_keeps_the_proof_and_does_not_refresh_the_report_time() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        // Nothing proven yet: no data proves nothing.
        p.report("n1", Vote::Abstain(AbstainReason::RateLimited));
        let h = p.state.nodes[&id("n1")].health;
        assert_eq!(h.gp, GpStatus::Unknown);
        assert_eq!(h.last_report, Ts(0));
        // Metrics are still taken from the report.
        assert!(h.metrics.up);

        p.advance(10 * SEC);
        p.report("n1", Vote::Clear);
        let verified = p.state.nodes[&id("n1")].health.last_report;
        assert_eq!(verified, p.now);
        p.advance(10 * SEC);
        p.report("n1", Vote::Abstain(AbstainReason::RateLimited));
        let h = p.state.nodes[&id("n1")].health;
        assert_eq!(h.gp, GpStatus::NoData { at: p.now });
        assert_eq!(h.last_report, verified);
    }

    #[test]
    fn a_closed_port_is_no_verdict_of_globalping() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        p.report("n1", Vote::Clear);
        let verified = p.now;
        p.advance(60 * SEC);
        p.report("n1", Vote::Abstain(AbstainReason::PortRefused));
        let h = p.state.nodes[&id("n1")].health;
        assert_eq!(
            h.gp,
            GpStatus::NoData { at: p.now },
            "not Failed: no block was seen"
        );
        assert_eq!(h.last_report, verified);
    }

    #[test]
    fn a_report_between_measurements_keeps_the_last_verdict() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        p.report("n1", Vote::Clear);
        let verified = p.now;

        // The agent reports every minute and measures every five: most reports carry nothing.
        p.advance(60 * SEC);
        p.report("n1", Vote::Abstain(AbstainReason::NoData));
        let h = p.state.nodes[&id("n1")].health;
        assert_eq!(h.gp, GpStatus::Ok { at: verified });
        assert_eq!(h.last_report, verified);

        // A measurement that Globalping refused does say there is no data.
        p.advance(60 * SEC);
        p.report("n1", Vote::Abstain(AbstainReason::RateLimited));
        let h = p.state.nodes[&id("n1")].health;
        assert_eq!(h.gp, GpStatus::NoData { at: p.now });
        assert_eq!(h.last_report, verified);
    }

    #[test]
    fn traffic_counts_deltas_and_the_first_reading_is_a_baseline() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        let send = |p: &mut Pool, from: u64, to: u64, clients: Option<u64>| {
            let reply = p.rid();
            let generation = p.generation("n1");
            p.run(Command::Report(ReportRequest {
                reply,
                node: id("n1"),
                generation,
                metrics_ok: true,
                globalping: Vote::Clear,
                traffic: Some(TrafficCounters {
                    from_client: from,
                    to_client: to,
                }),
                clients,
                other_traffic: None,
                other_clients: None,
                pool_user_taken: false,
            }))
        };
        let effects = send(&mut p, 1000, 2000, Some(4));
        assert!(
            !persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::Traffic { .. }))
        );
        let effects = send(&mut p, 1500, 2100, None);
        assert!(persists(&effects).iter().any(|op| matches!(
            op,
            PersistOp::Traffic {
                from_client: 500,
                to_client: 100,
                ..
            }
        )));
        // The agent restarted: counters start again from zero.
        send(&mut p, 30, 20, None);
        let stats = &p.state.stats[&id("n1")];
        assert_eq!(stats.from_client_total, 500 + 30);
        assert_eq!(stats.to_client_total, 100 + 20);
        assert_eq!(
            stats.clients,
            Some(4),
            "no clients in a report keeps the last value"
        );
    }

    /// One report of the kind `other` says: `None` is an agent that does not tell the users apart.
    fn traffic_report(
        p: &mut Pool,
        pool: (u64, u64),
        other: Option<(u64, u64)>,
        clients: Option<u64>,
        other_clients: Option<u64>,
    ) -> Vec<Effect> {
        let reply = p.rid();
        let generation = p.generation("n1");
        let counters = |(from_client, to_client)| TrafficCounters {
            from_client,
            to_client,
        };
        p.run(Command::Report(ReportRequest {
            reply,
            node: id("n1"),
            generation,
            metrics_ok: true,
            globalping: Vote::Clear,
            traffic: Some(counters(pool)),
            clients,
            other_traffic: other.map(counters),
            other_clients,
            pool_user_taken: false,
        }))
    }

    fn deltas(effects: &[Effect]) -> Vec<(u64, u64)> {
        persists(effects)
            .iter()
            .filter_map(|op| match op {
                PersistOp::Traffic {
                    from_client,
                    to_client,
                    ..
                } => Some((*from_client, *to_client)),
                _ => None,
            })
            .collect()
    }

    fn pool_with_node() -> Pool {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        p
    }

    #[test]
    fn the_first_report_that_tells_users_apart_only_sets_the_base() {
        let mut p = pool_with_node();
        // An older agent: the counter holds every user of the engine.
        traffic_report(&mut p, (9_000, 90_000), None, Some(50), None);
        let effects = traffic_report(&mut p, (9_100, 90_200), None, Some(51), None);
        assert_eq!(deltas(&effects), [(100, 200)]);

        // The updated agent counts the pool's users alone: the counter is lower, but nothing was
        // reset and nothing has to be added.
        let effects = traffic_report(
            &mut p,
            (1_000, 2_000),
            Some((8_200, 88_400)),
            Some(5),
            Some(46),
        );
        assert_eq!(deltas(&effects), []);
        let stats = &p.state.stats[&id("n1")];
        assert_eq!((stats.from_client_total, stats.to_client_total), (100, 200));
        assert_eq!(
            (stats.other_from_client_total, stats.other_to_client_total),
            (0, 0)
        );
        assert_eq!((stats.clients, stats.other_clients), (Some(5), Some(46)));

        // From here on each side has its own deltas.
        let effects = traffic_report(&mut p, (1_010, 2_020), Some((8_230, 88_440)), None, None);
        assert_eq!(deltas(&effects), [(10, 20)]);
        let stats = &p.state.stats[&id("n1")];
        assert_eq!((stats.from_client_total, stats.to_client_total), (110, 220));
        assert_eq!(
            (stats.other_from_client_total, stats.other_to_client_total),
            (30, 40)
        );
        assert_eq!(
            (stats.clients, stats.other_clients),
            (Some(5), Some(46)),
            "no number in a report keeps the last value"
        );
    }

    /// `(pool from, pool to, other from, other to)` of every stored delta.
    fn stored(effects: &[Effect]) -> Vec<(u64, u64, u64, u64)> {
        persists(effects)
            .iter()
            .filter_map(|op| match op {
                PersistOp::Traffic {
                    from_client,
                    to_client,
                    other_from_client,
                    other_to_client,
                    ..
                } => Some((
                    *from_client,
                    *to_client,
                    *other_from_client,
                    *other_to_client,
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_traffic_of_the_other_users_is_stored_beside_the_pools() {
        let mut p = pool_with_node();
        let effects = traffic_report(&mut p, (1_000, 2_000), Some((8_000, 88_000)), None, None);
        assert_eq!(stored(&effects), [], "the first reading only sets the base");

        let effects = traffic_report(&mut p, (1_010, 2_020), Some((8_030, 88_040)), None, None);
        assert_eq!(stored(&effects), [(10, 20, 30, 40)]);

        // Only the owner's users were active: that is still traffic worth a point.
        let effects = traffic_report(&mut p, (1_010, 2_020), Some((8_035, 88_046)), None, None);
        assert_eq!(stored(&effects), [(0, 0, 5, 6)]);

        // Nobody was active: nothing to store.
        let effects = traffic_report(&mut p, (1_010, 2_020), Some((8_035, 88_046)), None, None);
        assert_eq!(stored(&effects), []);

        // The engine restarted: each side counts what it read since.
        let effects = traffic_report(&mut p, (3, 4), Some((5, 6)), None, None);
        assert_eq!(stored(&effects), [(3, 4, 5, 6)]);
    }

    #[test]
    fn a_report_of_another_kind_stores_no_traffic_at_all() {
        let mut p = pool_with_node();
        traffic_report(&mut p, (9_000, 90_000), None, None, None);
        let effects = traffic_report(&mut p, (1_000, 2_000), Some((8_000, 88_000)), None, None);
        assert_eq!(stored(&effects), [], "the agent began to tell users apart");

        traffic_report(&mut p, (1_100, 2_100), Some((8_100, 88_100)), None, None);
        let effects = traffic_report(&mut p, (9_300, 90_300), None, None, None);
        assert_eq!(stored(&effects), [], "the agent went back to summing them");
    }

    #[test]
    fn a_real_reset_after_the_change_is_still_counted() {
        let mut p = pool_with_node();
        traffic_report(&mut p, (9_000, 90_000), None, None, None);
        traffic_report(&mut p, (1_000, 2_000), Some((8_000, 88_000)), None, None);
        traffic_report(&mut p, (1_100, 2_100), Some((8_100, 88_100)), None, None);

        // The engine restarted: every counter starts from zero again.
        let effects = traffic_report(&mut p, (30, 40), Some((5, 6)), None, None);
        assert_eq!(deltas(&effects), [(30, 40)]);
        let stats = &p.state.stats[&id("n1")];
        assert_eq!(
            (stats.from_client_total, stats.to_client_total),
            (100 + 30, 100 + 40)
        );
        assert_eq!(
            (stats.other_from_client_total, stats.other_to_client_total),
            (100 + 5, 100 + 6)
        );
    }

    #[test]
    fn a_node_whose_first_report_tells_users_apart_starts_from_that_base() {
        let mut p = pool_with_node();
        let effects = traffic_report(
            &mut p,
            (1_000, 2_000),
            Some((8_000, 88_000)),
            Some(1),
            Some(2),
        );
        assert_eq!(deltas(&effects), []);
        let effects = traffic_report(
            &mut p,
            (1_001, 2_002),
            Some((8_003, 88_004)),
            Some(1),
            Some(2),
        );
        assert_eq!(deltas(&effects), [(1, 2)]);
        let stats = &p.state.stats[&id("n1")];
        assert_eq!(
            (stats.other_from_client_total, stats.other_to_client_total),
            (3, 4)
        );
    }

    #[test]
    fn an_agent_rolled_back_to_one_that_sums_all_users_makes_no_spike() {
        let mut p = pool_with_node();
        traffic_report(
            &mut p,
            (1_000, 2_000),
            Some((8_000, 88_000)),
            Some(5),
            Some(46),
        );
        traffic_report(
            &mut p,
            (1_100, 2_100),
            Some((8_100, 88_100)),
            Some(5),
            Some(46),
        );

        // The old agent's counter holds the owner's users too: far above the pool's alone.
        let effects = traffic_report(&mut p, (9_300, 90_300), None, Some(51), None);
        assert_eq!(deltas(&effects), []);
        let stats = &p.state.stats[&id("n1")];
        assert_eq!((stats.from_client_total, stats.to_client_total), (100, 100));
        assert_eq!(
            stats.other_clients, None,
            "nobody says how many there are any more"
        );
        assert_eq!(stats.other_counters, None);
        assert_eq!(
            (stats.other_from_client_total, stats.other_to_client_total),
            (100, 100),
            "what was counted stays"
        );

        // The old agent goes on with plain deltas, and the new one comes back without a spike.
        let effects = traffic_report(&mut p, (9_310, 90_320), None, None, None);
        assert_eq!(deltas(&effects), [(10, 20)]);
        let effects = traffic_report(&mut p, (1_200, 2_200), Some((8_110, 88_120)), None, None);
        assert_eq!(deltas(&effects), []);
        assert_eq!(p.state.stats[&id("n1")].from_client_total, 110);
    }

    #[test]
    fn a_report_without_the_split_before_the_pool_users_are_known_makes_no_spike() {
        let mut p = pool_with_node();
        // The agent knows no pool users yet: zeros and no split.
        traffic_report(&mut p, (0, 0), None, None, None);
        // The real counters arrive with the first report that tells the users apart.
        let effects = traffic_report(&mut p, (20_000, 30_000), Some((1, 2)), Some(7), Some(0));
        assert_eq!(deltas(&effects), []);
        assert_eq!(p.state.stats[&id("n1")].from_client_total, 0);

        // A later moment without the list (a lost state file) changes nothing either.
        traffic_report(&mut p, (0, 0), None, None, None);
        let effects = traffic_report(&mut p, (20_010, 30_010), Some((1, 2)), None, None);
        assert_eq!(deltas(&effects), []);
        assert_eq!(p.state.stats[&id("n1")].from_client_total, 0);
    }

    #[test]
    fn stats_stored_before_the_split_load_as_an_agent_that_does_not_split() {
        let stored = r#"{"counters":{"from_client":7,"to_client":8},"from_client_total":1,"to_client_total":2,"clients":3}"#;
        let stats: NodeStats = serde_json::from_str(stored).unwrap();
        assert_eq!(stats.other_counters, None);
        assert_eq!(stats.other_clients, None);
        assert_eq!(
            (stats.other_from_client_total, stats.other_to_client_total),
            (0, 0)
        );
    }

    #[test]
    fn domain_clients_are_kept_while_srmd_is_off() {
        let mut p = Pool::new();
        assert!(!p.state.settings.srmd.enabled);
        p.make_healthy("n1", ip(1));
        let reply = p.rid();
        let generation = p.generation("n1");
        p.run(Command::Report(ReportRequest {
            reply,
            node: id("n1"),
            generation,
            metrics_ok: true,
            globalping: Vote::Clear,
            traffic: None,
            clients: Some(12),
            other_traffic: None,
            other_clients: None,
            pool_user_taken: false,
        }));
        p.advance(SEC);
        p.run(Command::Tick);
        assert_eq!(p.state.srmd.domain_clients.get(DOMAIN), Some(&12));
    }

    #[test]
    fn heartbeat_from_a_new_ip_starts_a_new_check() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        let g = p.generation("n1");
        p.advance(SEC);
        let effects = p.beat_from("n1", ip(9));
        let n = &p.state.nodes[&id("n1")];
        assert_eq!((n.ip, n.state), (ip(9), NodeState::Pending));
        assert!(n.generation > g);
        assert!(matches!(reply(&effects), Reply::Ok));
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::UpsertNode(r) if r.ip == ip(9)))
        );
    }

    #[test]
    fn heartbeat_from_a_blocked_ip_ends_the_node() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.state.blocks.push(IpBlock {
            ip: ip(9),
            node_id: id("old"),
            blocked_at: p.now,
            next_reverify_at: None,
            owner: None,
        });
        let effects = p.beat_from("n1", ip(9));
        assert_eq!(p.state_of("n1"), NodeState::Dead);
        assert_eq!(p.state.nodes[&id("n1")].ip, ip(1));
        assert_eq!(
            *reply(&effects),
            Reply::Terminate(TerminateReason::AddressRefused)
        );
        assert_eq!(kinds(&effects), [EventKind::NodeDead]);
    }

    #[test]
    fn heartbeat_of_a_final_or_unknown_node_is_answered_without_changes() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        let action = p.node_action("n1");
        p.run(Command::Retire(action));
        let before = p.state.clone();
        let effects = p.beat("n1");
        assert_eq!(*reply(&effects), Reply::Terminate(TerminateReason::Dead));
        assert_eq!(p.state, before);
        let effects = p.beat_from("ghost", ip(5));
        assert_eq!(*reply(&effects), Reply::UnknownNode);
    }

    #[test]
    fn enrolling_on_a_blocked_ip_runs_a_decisive_round() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        let effects = p.round("n1", &blocked());
        assert!(
            !p.state.nodes.contains_key(&id("n1")),
            "the banned record is dropped at once"
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::DeleteNode(n) if *n == id("n1")))
        );

        p.advance(900 * SEC);
        let effects = p.enroll("n2", ip(1));
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { reverify: true, .. }
        ));
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::RunRound { .. })),
            "the round waits for the measurement of the agent"
        );
        // A second registration does not start a second round.
        let effects = p.enroll("n3", ip(1));
        assert!(matches!(
            reply(&effects),
            Reply::EnrollRejected {
                reason: RejectReason::ReverifyInFlight,
                ..
            }
        ));
        assert!(!effects.iter().any(|e| matches!(e, Effect::RunRound { .. })));

        // A clean round lifts the block.
        let effects = p.round("n2", &clean());
        assert!(p.state.blocks.is_empty());
        assert!(kinds(&effects).contains(&EventKind::BanLifted));
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::DeleteIpBlock { ip: a } if *a == ip(1)))
        );
    }

    /// A banned IP, and a new node on it that waits for its reverify round.
    fn reverify_pool() -> Pool {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        p.round("n1", &blocked());
        assert!(!p.state.nodes.contains_key(&id("n1")));
        p.advance(900 * SEC);
        p.enroll("n2", ip(1));
        assert!(p.state.nodes[&id("n2")].reverify);
        p
    }

    fn run_rounds(effects: &[Effect]) -> Vec<&Option<SensorVote>> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::RunRound { fresh_gp, .. } => Some(fresh_gp),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_reverify_round_starts_with_the_first_measurement_of_the_agent() {
        let mut p = reverify_pool();
        // A report without a verdict does not start it.
        let effects = p.report("n2", Vote::Abstain(AbstainReason::NoData));
        assert!(run_rounds(&effects).is_empty());
        assert_eq!(p.state.nodes[&id("n2")].last_round_at, None);

        let effects = p.report("n2", Vote::Clear);
        let rounds = run_rounds(&effects);
        assert_eq!(rounds.len(), 1);
        assert!(matches!(
            rounds[0],
            Some(SensorVote {
                vote: Vote::Clear,
                ..
            })
        ));
        assert_eq!(
            effects.last().map(|e| matches!(e, Effect::Reply(..))),
            Some(true),
            "the reply is still last"
        );
        // The round is out: the next report does not start another one.
        p.advance(SEC);
        let effects = p.report("n2", Vote::Clear);
        assert!(run_rounds(&effects).is_empty());
    }

    #[test]
    fn a_blocked_measurement_of_the_agent_is_the_vote_of_the_reverify_round() {
        let mut p = reverify_pool();
        let effects = p.report("n2", Vote::Blocked);
        let rounds = run_rounds(&effects);
        assert!(matches!(
            rounds[..],
            [Some(SensorVote {
                vote: Vote::Blocked,
                ..
            })]
        ));
    }

    #[test]
    fn without_a_report_the_tick_starts_the_reverify_round_after_the_report_age() {
        let mut p = reverify_pool();
        let max_age = i64::from(p.state.settings.report_max_age_sec) * SEC;
        p.advance(max_age - SEC);
        p.beat("n2");
        assert!(run_rounds(&p.run(Command::Tick)).is_empty());
        p.advance(SEC);
        p.beat("n2");
        let effects = p.run(Command::Tick);
        assert!(matches!(run_rounds(&effects)[..], [None]));
        // Once started it is paced like any round.
        p.advance(SEC);
        p.beat("n2");
        assert!(run_rounds(&p.run(Command::Tick)).is_empty());
    }

    #[test]
    fn a_failed_reverify_is_logged_but_is_not_a_ban_event() {
        let mut p = reverify_pool();
        p.report("n2", Vote::Blocked);
        let effects = p.round("n2", &blocked());
        assert_eq!(
            kinds(&effects),
            [EventKind::SensorRound, EventKind::NodeReverifyFailed]
        );
        assert_eq!(EventKind::NodeReverifyFailed.webhook_name(), None);
        let detail = effects
            .iter()
            .find_map(|e| match e {
                Effect::Emit(ev) if ev.kind == EventKind::NodeReverifyFailed => Some(&ev.detail),
                _ => None,
            })
            .unwrap();
        assert_eq!(detail["retry_after_sec"], 900);
        let record = persists(&effects)
            .into_iter()
            .find(|op| matches!(op, PersistOp::RecordBan { .. }))
            .expect("the failed reverify is recorded");
        assert!(matches!(
            record,
            PersistOp::RecordBan { ip: a, node, reverify: true, votes, node_registered_at: None, .. }
                if *a == ip(1) && *node == id("n2") && votes.len() == 2
        ));
    }

    #[test]
    fn a_ban_is_recorded_with_its_votes_and_lifted_ways_are_recorded() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        let registered_at = p.state.nodes[&id("n1")].registered_at;
        let effects = p.round("n1", &blocked());
        let record = persists(&effects)
            .into_iter()
            .find(|op| matches!(op, PersistOp::RecordBan { .. }))
            .expect("the ban is recorded");
        assert!(matches!(
            record,
            PersistOp::RecordBan { ip: a, node, at, votes, reverify: false, node_registered_at }
                if *a == ip(1)
                    && *node == id("n1")
                    && *at == p.now
                    && *node_registered_at == Some(registered_at)
                    && votes[..] == [
                        (SensorKind::Globalping, Vote::Blocked),
                        (SensorKind::Cheburcheck, Vote::Blocked),
                    ]
        ));

        // An admin lifts it.
        let action = p.node_action("n1");
        let effects = p.run(Command::AdminUnban(action));
        assert!(persists(&effects).iter().any(|op| matches!(
            op,
            PersistOp::LiftBan { ip: a, at, admin: true } if *a == ip(1) && *at == p.now
        )));

        // A clean reverify lifts another one.
        let mut p = reverify_pool();
        p.report("n2", Vote::Clear);
        let effects = p.round("n2", &clean());
        assert!(persists(&effects).iter().any(|op| matches!(
            op,
            PersistOp::LiftBan { ip: a, admin: false, .. } if *a == ip(1)
        )));
        assert!(
            !persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::RecordBan { .. }))
        );
    }

    #[test]
    fn replacing_a_node_on_the_same_ip_ends_the_old_one() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        let effects = p.enroll("n2", ip(1));
        assert_eq!(p.state_of("n1"), NodeState::Dead);
        assert!(kinds(&effects).contains(&EventKind::NodeDead));
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { replaced: Some(r), .. } if *r == id("n1")
        ));
    }

    #[test]
    fn enrolling_the_same_id_again_moves_the_generation_on() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        let g = p.generation("n1");
        let effects = p.enroll("n1", ip(1));
        assert_eq!(p.state_of("n1"), NodeState::Pending);
        assert!(p.generation("n1") > g);
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { replaced: None, .. }
        ));
    }

    #[test]
    fn dns_push_waits_for_the_outcome_and_backs_off_after_a_failure() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        // make_healthy's tick already pushed generation 1.
        let dns = &p.state.dns[DOMAIN];
        assert_eq!(dns.generation, Generation(1));
        assert_eq!(dns.desired, RecordValue::A(vec![ip(1)]));
        assert_eq!(dns.in_flight, Some(Generation(1)));

        // Still outstanding: no second push.
        p.advance(SEC);
        p.beat("n1");
        assert!(pushes(&p.run(Command::Tick)).is_empty());

        // A failure backs off 5 s.
        let failed_at = p.now;
        let effects = p.run(Command::DnsDone {
            domain: DOMAIN.to_owned(),
            generation: Generation(1),
            result: Err("provider said no".to_owned()),
        });
        assert_eq!(kinds(&effects), [EventKind::DnsFailed]);
        assert_eq!(p.state.dns[DOMAIN].retries, 1);
        assert_eq!(p.state.dns[DOMAIN].next_try, Ts(failed_at.0 + 5 * SEC));
        p.advance(4 * SEC);
        p.beat("n1");
        assert!(pushes(&p.run(Command::Tick)).is_empty());
        p.advance(2 * SEC);
        p.beat("n1");
        let effects = p.run(Command::Tick);
        assert_eq!(
            pushes(&effects),
            [&Effect::PushDns {
                domain: DOMAIN.to_owned(),
                generation: Generation(1),
                desired: RecordValue::A(vec![ip(1)]),
            }]
        );

        // Success: applied, and nothing more to push.
        let effects = p.run(Command::DnsDone {
            domain: DOMAIN.to_owned(),
            generation: Generation(1),
            result: Ok(()),
        });
        assert_eq!(kinds(&effects), [EventKind::DnsApplied]);
        assert_eq!(
            p.state.dns[DOMAIN].applied,
            Some(RecordValue::A(vec![ip(1)]))
        );
        assert_eq!(p.state.dns[DOMAIN].retries, 0);
        p.advance(SEC);
        p.beat("n1");
        assert!(pushes(&p.run(Command::Tick)).is_empty());
    }

    #[test]
    fn dns_push_all_marks_records_unapplied_and_skips_the_one_in_flight() {
        let mut p = Pool::with(Settings {
            domains: vec![DOMAIN.to_owned(), "q.example.com".to_owned()],
            ..settings()
        });
        p.make_healthy("n1", ip(1));
        for domain in [DOMAIN, "q.example.com"] {
            let generation = p.state.dns[domain].generation;
            p.run(Command::DnsDone {
                domain: domain.to_owned(),
                generation,
                result: Err("later".to_owned()),
            });
        }
        // The first domain succeeds, the second stays failing with a backoff.
        p.run(Command::DnsDone {
            domain: DOMAIN.to_owned(),
            generation: Generation(1),
            result: Ok(()),
        });
        assert!(p.state.dns[DOMAIN].applied.is_some());
        p.state.dns.get_mut("q.example.com").unwrap().in_flight = Some(Generation(1));
        let q_before = p.state.dns["q.example.com"].clone();

        let effects = p.run(Command::DnsPushAll);

        let dns = &p.state.dns[DOMAIN];
        assert_eq!(dns.applied, None);
        assert_eq!(dns.retries, 0);
        assert_eq!(dns.next_try, p.now);
        assert_eq!(p.state.dns["q.example.com"], q_before);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Persist(PersistOp::UpsertDns { domain, .. })] if domain == DOMAIN
        ));
        // The next tick pushes it again.
        p.advance(SEC);
        p.beat("n1");
        assert_eq!(pushes(&p.run(Command::Tick)).len(), 1);
    }

    #[test]
    fn availability_refreshed_sets_listed_nodes_clears_others_and_ignores_strangers() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.make_healthy("n2", ip(2));
        let stranger = id("gone");
        p.run(Command::AvailabilityRefreshed(vec![
            (id("n1"), 9997),
            (stranger.clone(), 5000),
        ]));
        assert_eq!(p.state.stats[&id("n1")].availability_bp, Some(9997));
        assert_eq!(p.state.stats[&id("n2")].availability_bp, None);
        assert!(!p.state.stats.contains_key(&stranger));

        p.run(Command::AvailabilityRefreshed(vec![(id("n2"), 1)]));
        assert_eq!(p.state.stats[&id("n1")].availability_bp, None);
        assert_eq!(p.state.stats[&id("n2")].availability_bp, Some(1));
    }

    #[test]
    fn a_changed_desired_set_bumps_the_generation_and_old_results_are_ignored() {
        let mut p = Pool::with(Settings {
            masters_per_domain: 2,
            ..settings()
        });
        p.make_healthy("n1", ip(1));
        p.run(Command::DnsDone {
            domain: DOMAIN.to_owned(),
            generation: Generation(1),
            result: Ok(()),
        });
        p.advance(SEC);
        p.beat("n1");
        let effects = p.make_healthy_with_effects("n2", ip(2));
        let dns = &p.state.dns[DOMAIN];
        assert_eq!(dns.generation, Generation(2));
        assert_eq!(dns.desired, RecordValue::A(vec![ip(1), ip(2)]));
        assert_eq!(pushes(&effects).len(), 1);
        assert_eq!(dns.in_flight, Some(Generation(2)));

        let before = p.state.clone();
        let effects = p.run(Command::DnsDone {
            domain: DOMAIN.to_owned(),
            generation: Generation(1),
            result: Ok(()),
        });
        assert!(effects.is_empty());
        assert_eq!(p.state, before);
    }

    #[test]
    fn a_node_that_is_gone_for_good_does_not_stay_a_master_beside_the_node_on_its_ip() {
        let mut p = Pool::with(Settings {
            masters_per_domain: 2,
            ..settings()
        });
        p.make_healthy("n1", ip(1));
        p.make_healthy("n2", ip(2));
        // n2 moves onto the address of n1: n1 is replaced and ends `Dead`, n2 starts over.
        p.advance(SEC);
        p.enroll("n2", ip(1));
        assert_eq!(p.state_of("n1"), NodeState::Dead);
        p.advance(SEC);
        let effects = p.run(Command::Tick);
        let masters: Vec<&NodeId> = p.state.assignments.0[DOMAIN]
            .iter()
            .map(|s| &s.node)
            .collect();
        assert_eq!(masters, [&id("n2")], "the dead node left its slot");
        assert!(kinds(&effects).contains(&EventKind::MasterChanged));
    }

    #[test]
    fn desired_dns_skips_nodes_that_left_the_pool_and_never_pushes_an_empty_set() {
        let mut p = Pool::new();
        p.state.assignments.0.insert(
            DOMAIN.to_owned(),
            vec![Slot {
                node: id("ghost"),
                since: p.now,
            }],
        );
        let effects = p.run(Command::Tick);
        assert!(pushes(&effects).is_empty());
        assert!(p.state.dns.is_empty());
    }

    #[test]
    fn a_folded_domain_is_a_cname_and_an_unfolded_one_goes_back_to_a_records() {
        let mut p = Pool::with(Settings {
            domains: vec![DOMAIN.to_owned(), "q.example.com".to_owned()],
            ..settings()
        });
        p.make_healthy("n1", ip(1));
        p.state
            .srmd
            .cnames
            .insert("q.example.com".to_owned(), DOMAIN.to_owned());
        p.advance(SEC);
        p.beat("n1");
        p.run(Command::Tick);
        assert_eq!(
            p.state.dns["q.example.com"].desired,
            RecordValue::Cname(DOMAIN.to_owned())
        );
        assert_eq!(p.state.dns[DOMAIN].desired, RecordValue::A(vec![ip(1)]));
    }

    #[test]
    fn settings_changed_recomputes_the_port_check_of_every_node() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        assert!(p.state.nodes[&id("n1")].health.port_ok);
        let mut s = p.state.settings.clone();
        s.proxy_port = 8443;
        let effects = p.run(Command::SettingsChanged(s));
        assert!(!p.state.nodes[&id("n1")].health.port_ok);
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::SetSettings(s) if s.proxy_port == 8443))
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::NodeHealth { .. }))
        );
    }

    #[test]
    fn invalid_or_unchanged_settings_are_ignored() {
        let mut p = Pool::new();
        let before = p.state.clone();
        assert!(
            p.run(Command::SettingsChanged(p.state.settings.clone()))
                .is_empty()
        );
        let mut bad = p.state.settings.clone();
        bad.masters_per_domain = 0;
        assert!(bad.validate().is_err());
        assert!(p.run(Command::SettingsChanged(bad)).is_empty());
        assert_eq!(p.state, before);
    }

    #[test]
    fn a_rotated_out_node_goes_to_the_back_of_the_queue() {
        let mut p = Pool::with(Settings {
            rotation_ttl_sec: 60,
            ..settings()
        });
        p.make_healthy("n1", ip(1));
        p.make_healthy("n2", ip(2));
        p.advance(SEC);
        p.beat("n1");
        p.beat("n2");
        p.run(Command::Tick);
        let master = p.state.assignments.0[DOMAIN][0].node.clone();
        p.advance(61 * SEC);
        p.beat("n1");
        p.beat("n2");
        let effects = p.run(Command::Tick);
        let new_master = p.state.assignments.0[DOMAIN][0].node.clone();
        assert_ne!(master, new_master);
        assert_eq!(p.state.nodes[&master].queued_at, p.now);
        assert!(kinds(&effects).contains(&EventKind::MasterChanged));
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::UpsertNode(n) if n.id == master))
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::SetMasters { domain, .. } if domain == DOMAIN))
        );
    }

    #[test]
    fn recheck_runs_a_round_only_when_one_is_due() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        let action = p.node_action("n1");
        let effects = p.run(Command::Recheck(action));
        assert_eq!(*reply(&effects), Reply::Refused(Refusal::NotRecheckable));

        p.quarantine("n1");
        p.advance(SEC);
        p.beat("n1");
        let action = p.node_action("n1");
        let effects = p.run(Command::Recheck(action));
        assert_eq!(*reply(&effects), Reply::Ok);
        assert!(effects.iter().any(|e| matches!(e, Effect::RunRound { .. })));
        let action = p.node_action("ghost");
        let effects = p.run(Command::Recheck(action));
        assert_eq!(*reply(&effects), Reply::UnknownNode);
    }

    #[test]
    fn unban_of_a_node_that_is_not_banned_is_refused() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        let action = p.node_action("n1");
        let effects = p.run(Command::AdminUnban(action));
        assert_eq!(*reply(&effects), Reply::Refused(Refusal::NotBanned));
        assert!(kinds(&effects).is_empty());
    }

    #[test]
    fn srmd_take_and_release_check_the_domain_first() {
        let mut p = Pool::new();
        let manual = |p: &mut Pool, domain: &str, action| {
            let reply = p.rid();
            p.run(Command::SrmdManual(SrmdManualRequest {
                reply,
                domain: domain.to_owned(),
                action,
            }))
        };
        let effects = manual(&mut p, "nope.example.com", SrmdManualAction::Take);
        assert_eq!(*reply(&effects), Reply::Refused(Refusal::UnknownDomain));
        assert!(p.state.srmd.created.is_empty());

        let effects = manual(&mut p, DOMAIN, SrmdManualAction::Take);
        assert_eq!(*reply(&effects), Reply::Ok);
        assert_eq!(kinds(&effects), [EventKind::SrmdAction]);
        assert!(p.state.srmd.created.contains(DOMAIN));
        let effects = manual(&mut p, DOMAIN, SrmdManualAction::Take);
        assert_eq!(*reply(&effects), Reply::Refused(Refusal::AlreadyManaged));

        let effects = manual(&mut p, DOMAIN, SrmdManualAction::Release);
        assert_eq!(*reply(&effects), Reply::Ok);
        let effects = manual(&mut p, DOMAIN, SrmdManualAction::Release);
        assert_eq!(*reply(&effects), Reply::Refused(Refusal::NotManaged));
    }

    #[test]
    fn srmd_creates_a_domain_in_the_settings_when_the_queue_outgrows_the_domains() {
        let mut s = settings();
        s.srmd.enabled = true;
        s.srmd.max_nodes_per_domain = 1;
        s.srmd.diverge_ticks = 1;
        s.srmd.max_domains = 4;
        let mut p = Pool::with(s);
        p.make_healthy("n1", ip(1));
        let effects = p.make_healthy_with_effects("n2", ip(2));
        assert_eq!(p.state.settings.domains.len(), 2);
        let created = p.state.settings.domains[1].clone();
        assert!(p.state.srmd.created.contains(&created));
        assert!(kinds(&effects).contains(&EventKind::SrmdAction));
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::SetSettings(s) if s.domains.contains(&created)))
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::SetSrmd(_)))
        );
    }

    #[test]
    fn every_command_with_a_reply_answers_exactly_once_and_last() {
        let mut p = Pool::new();
        let effects = p.enroll("n1", ip(1));
        assert!(matches!(effects.last(), Some(Effect::Reply(..))));
        assert_eq!(
            effects
                .iter()
                .filter(|e| matches!(e, Effect::Reply(..)))
                .count(),
            1
        );
        let effects = p.beat("n1");
        assert!(matches!(effects.last(), Some(Effect::Reply(..))));
    }

    fn event_of(effects: &[Effect], kind: EventKind) -> &Event {
        effects
            .iter()
            .find_map(|e| match e {
                Effect::Emit(ev) if ev.kind == kind => Some(ev),
                _ => None,
            })
            .expect("the event")
    }

    fn deletes(effects: &[Effect], name: &str) -> bool {
        persists(effects)
            .iter()
            .any(|op| matches!(op, PersistOp::DeleteNode(n) if *n == id(name)))
    }

    // ----- the journal ------------------------------------------------------------------------

    #[test]
    fn a_heartbeat_from_a_new_address_says_where_from_and_where_to() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.advance(SEC);
        let effects = p.beat_from("n1", ip(9));
        let event = event_of(&effects, EventKind::NodeIpChanged);
        assert_eq!(event.node, Some(id("n1")));
        assert_eq!(event.owner, Some(OwnerId(1)));
        assert_eq!(
            event.detail,
            json!({ "from": ip(1).to_string(), "to": ip(9).to_string() })
        );
        assert_eq!(EventKind::NodeIpChanged.webhook_name(), None);
    }

    #[test]
    fn a_heartbeat_from_the_same_address_and_a_refused_new_one_change_nothing_to_tell() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.make_healthy("n2", ip(2));
        p.advance(SEC);
        assert!(!kinds(&p.beat("n1")).contains(&EventKind::NodeIpChanged));
        let refused = p.beat_from("n1", ip(2));
        assert_eq!(kinds(&refused), [EventKind::NodeDead]);
    }

    #[test]
    fn node_registered_names_the_address() {
        let mut p = Pool::new();
        let effects = p.enroll("n1", ip(1));
        assert_eq!(
            event_of(&effects, EventKind::NodeRegistered).detail["ip"],
            ip(1).to_string()
        );
    }

    #[test]
    fn dns_applied_says_what_was_written() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        let generation = p.state.dns[DOMAIN].generation;
        let effects = p.run(Command::DnsDone {
            domain: DOMAIN.to_owned(),
            generation,
            result: Ok(()),
        });
        assert_eq!(
            event_of(&effects, EventKind::DnsApplied).detail,
            json!({ "records": 1, "addresses": [ip(1).to_string()] })
        );

        let entry = p.state.dns.get_mut(DOMAIN).unwrap();
        entry.desired = RecordValue::Cname("target.example.com".to_owned());
        let generation = entry.generation;
        let effects = p.run(Command::DnsDone {
            domain: DOMAIN.to_owned(),
            generation,
            result: Ok(()),
        });
        assert_eq!(
            event_of(&effects, EventKind::DnsApplied).detail,
            json!({ "records": 1, "target": "target.example.com" })
        );
    }

    #[test]
    fn a_node_remembers_why_it_is_in_quarantine_until_it_leaves() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        assert_eq!(
            p.state.nodes[&id("n1")].quarantine_reasons,
            [FailedCondition::Globalping]
        );

        // A new address is a new check: it starts with no verdict.
        p.advance(SEC);
        p.beat_from("n1", ip(9));
        assert!(p.state.nodes[&id("n1")].quarantine_reasons.is_empty());
    }

    #[test]
    fn the_reasons_go_when_the_node_recovers_or_ends() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        p.state.nodes.get_mut(&id("n1")).unwrap().suspected = false;
        p.advance(SEC);
        p.beat("n1");
        p.report("n1", Vote::Clear);
        p.run(Command::Tick);
        assert_eq!(p.state_of("n1"), NodeState::Healthy);
        assert!(p.state.nodes[&id("n1")].quarantine_reasons.is_empty());

        p.quarantine("n1");
        let action = p.node_action("n1");
        p.run(Command::Retire(action));
        assert_eq!(p.state_of("n1"), NodeState::Dead);
        assert!(p.state.nodes[&id("n1")].quarantine_reasons.is_empty());
    }

    #[test]
    fn a_record_stored_before_the_reasons_existed_still_loads() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        let mut stored = serde_json::to_value(&p.state.nodes[&id("n1")]).unwrap();
        stored.as_object_mut().unwrap().remove("quarantine_reasons");
        let loaded: NodeRecord = serde_json::from_value(stored).unwrap();
        assert!(loaded.quarantine_reasons.is_empty());
    }

    // ----- the owner limit ------------------------------------------------------------------

    impl Pool {
        fn enroll_limited(&mut self, name: &str, addr: Ipv4Addr, limit: i64) -> Vec<Effect> {
            let reply = self.rid();
            self.run(Command::Enroll(EnrollRequest {
                reply,
                node_id: id(name),
                owner: OwnerId(1),
                ip: addr,
                port: 443,
                engine: EngineKind::Telemt,
                limit: Some(limit),
            }))
        }

        /// Two live nodes of the owner, of which `n1` went silent first and `n2` after it.
        fn two_silent(&mut self) {
            self.enroll("n1", ip(1));
            self.advance(SEC);
            self.enroll("n2", ip(2));
            self.advance(61 * SEC);
        }
    }

    #[test]
    fn a_full_limit_with_a_silent_node_lets_the_new_one_in_and_ends_the_longest_silent() {
        let mut p = Pool::new();
        p.two_silent();
        let effects = p.enroll_limited("n3", ip(3), 2);

        assert_eq!(p.state_of("n1"), NodeState::Dead);
        assert_eq!(p.state_of("n2"), NodeState::Pending);
        assert_eq!(p.state_of("n3"), NodeState::Pending);
        assert_eq!(
            event_of(&effects, EventKind::NodeDead).detail,
            json!({ "replaced_by": id("n3"), "reason": "silent" })
        );
        assert_eq!(event_of(&effects, EventKind::NodeDead).node, Some(id("n1")));
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::UpsertNode(r) if r.id == id("n1") && r.state == NodeState::Dead))
        );
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { replaced: Some(old), .. } if *old == id("n1")
        ));
    }

    #[test]
    fn a_full_limit_of_live_nodes_refuses_and_changes_nothing() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        p.advance(SEC);
        p.enroll("n2", ip(2));
        p.advance(58 * SEC);
        let before = p.state.clone();
        let effects = p.enroll_limited("n3", ip(3), 2);

        assert_eq!(
            *reply(&effects),
            Reply::OwnerLimit {
                limit: 2,
                current: 2
            }
        );
        assert_eq!(p.state, before);
        assert!(persists(&effects).is_empty());
        assert!(kinds(&effects).is_empty());
    }

    #[test]
    fn a_node_is_silent_only_from_the_end_of_the_heartbeat_ttl() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        p.advance(59 * SEC);
        let early = p.enroll_limited("n2", ip(2), 1);
        assert!(matches!(reply(&early), Reply::OwnerLimit { .. }));
        p.advance(SEC);
        let due = p.enroll_limited("n2", ip(2), 1);
        assert!(matches!(reply(&due), Reply::Enrolled { .. }));
    }

    #[test]
    fn a_heartbeat_keeps_a_node_from_being_silent() {
        let mut p = Pool::new();
        p.two_silent();
        p.beat("n1");
        let effects = p.enroll_limited("n3", ip(3), 2);
        assert_eq!(p.state_of("n1"), NodeState::Pending);
        assert_eq!(p.state_of("n2"), NodeState::Dead);
        assert_eq!(event_of(&effects, EventKind::NodeDead).node, Some(id("n2")));
    }

    #[test]
    fn the_same_ip_at_a_full_limit_is_a_replacement_and_ends_nobody_else() {
        let mut p = Pool::new();
        p.two_silent();
        let effects = p.enroll_limited("n3", ip(2), 2);

        assert_eq!(p.state_of("n1"), NodeState::Pending, "n1 keeps its place");
        assert_eq!(p.state_of("n2"), NodeState::Dead);
        assert_eq!(p.state_of("n3"), NodeState::Pending);
        assert_eq!(
            event_of(&effects, EventKind::NodeDead).detail,
            json!({ "replaced_by": id("n3") }),
            "the node gave way to its address, not for being silent"
        );
    }

    #[test]
    fn the_same_ip_at_a_full_limit_needs_no_silent_node() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        let effects = p.enroll_limited("n2", ip(1), 1);
        assert_eq!(p.state_of("n1"), NodeState::Dead);
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { replaced: Some(old), .. } if *old == id("n1")
        ));
    }

    #[test]
    fn taking_the_address_of_another_owner_still_needs_a_place_of_ones_own() {
        let mut p = Pool::new();
        p.enroll("n1", ip(1));
        p.state.nodes.get_mut(&id("n1")).unwrap().owner = OwnerId(2);
        let effects = p.enroll_limited("n2", ip(1), 0);
        assert!(matches!(reply(&effects), Reply::OwnerLimit { .. }));
        assert_eq!(
            p.state_of("n1"),
            NodeState::Pending,
            "a refused enrollment takes nothing from anybody"
        );
    }

    #[test]
    fn a_blocked_address_at_a_full_limit_evicts_nobody() {
        let mut p = Pool::new();
        p.two_silent();
        let block = |at: Option<Ts>, p: &mut Pool| {
            p.state.blocks = vec![IpBlock {
                ip: ip(9),
                node_id: id("old"),
                blocked_at: Ts(0),
                next_reverify_at: at,
                owner: None,
            }];
        };

        // Within the pause: the pause is the answer, whatever the limit.
        block(Some(Ts(p.now.0 + 900 * SEC)), &mut p);
        let paused = p.enroll_limited("n3", ip(9), 2);
        assert!(matches!(reply(&paused), Reply::EnrollRejected { .. }));

        // The pause is over: a decisive round would need a place, and a node that may turn out
        // blocked again does not get one at the cost of another.
        block(None, &mut p);
        let due = p.enroll_limited("n3", ip(9), 2);
        assert!(matches!(reply(&due), Reply::OwnerLimit { .. }));

        assert_eq!(p.state_of("n1"), NodeState::Pending);
        assert_eq!(p.state_of("n2"), NodeState::Pending);
    }

    #[test]
    fn a_blocked_address_with_a_free_place_goes_to_its_round() {
        let mut p = Pool::new();
        p.state.blocks.push(IpBlock {
            ip: ip(9),
            node_id: id("old"),
            blocked_at: Ts(0),
            next_reverify_at: None,
            owner: None,
        });
        let effects = p.enroll_limited("n1", ip(9), 1);
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { reverify: true, .. }
        ));
    }

    #[test]
    fn a_limit_lowered_below_the_nodes_evicts_nobody() {
        let mut p = Pool::new();
        p.two_silent();
        let effects = p.enroll_limited("n3", ip(3), 1);
        assert_eq!(
            *reply(&effects),
            Reply::OwnerLimit {
                limit: 1,
                current: 2
            }
        );
        assert_eq!(p.state_of("n1"), NodeState::Pending);
    }

    #[test]
    fn a_silent_node_of_another_owner_does_not_make_room() {
        let mut p = Pool::new();
        p.two_silent();
        p.state.nodes.get_mut(&id("n1")).unwrap().owner = OwnerId(2);
        p.state.nodes.get_mut(&id("n2")).unwrap().owner = OwnerId(2);
        let effects = p.enroll_limited("n3", ip(3), 0);
        assert!(matches!(reply(&effects), Reply::OwnerLimit { .. }));
    }

    #[test]
    fn nobody_is_evicted_while_the_heartbeats_after_a_restart_are_still_coming() {
        let mut p = Pool::new();
        p.two_silent();
        p.state.evictions_from = Some(Ts(p.now.0 + SEC));
        let early = p.enroll_limited("n3", ip(3), 2);
        assert!(matches!(reply(&early), Reply::OwnerLimit { .. }));
        assert_eq!(p.state_of("n1"), NodeState::Pending);

        p.advance(SEC);
        let due = p.enroll_limited("n3", ip(3), 2);
        assert!(matches!(reply(&due), Reply::Enrolled { .. }));
    }

    #[test]
    fn a_node_without_a_limit_enrolls_as_before() {
        let mut p = Pool::new();
        p.two_silent();
        let effects = p.enroll("n3", ip(3));
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { replaced: None, .. }
        ));
        assert_eq!(p.state_of("n1"), NodeState::Pending);
    }

    #[test]
    fn a_confirmed_round_removes_the_record_and_keeps_the_block() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        let effects = p.round("n1", &blocked());

        assert!(!p.state.nodes.contains_key(&id("n1")));
        assert!(!p.state.stats.contains_key(&id("n1")));
        assert!(deletes(&effects, "n1"));
        assert_eq!(
            kinds(&effects),
            [
                EventKind::SensorRound,
                EventKind::NodeBanned,
                EventKind::MasterChanged
            ],
            "the only master was still holding its domain"
        );
        assert_eq!(
            event_of(&effects, EventKind::NodeBanned).owner,
            Some(OwnerId(1)),
            "the webhook fan-out cannot look up a record that is gone"
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::RecordBan { node, .. } if *node == id("n1")))
        );
        assert_eq!(p.state.blocks.len(), 1);
        assert_eq!(p.state.blocks[0].node_id, id("n1"));
        assert_eq!(p.state.blocks[0].owner, Some(OwnerId(1)));
        assert_eq!(
            p.state.blocks[0].next_reverify_at,
            Some(Ts(p.now.0 + 900 * SEC)),
            "no reverify straight after the ban: it would spend the sensor quota"
        );
        // The agent that comes back within the pause is told to wait.
        let effects = p.enroll("n2", ip(1));
        assert!(matches!(
            reply(&effects),
            Reply::EnrollRejected {
                reason: RejectReason::ReverifyPause,
                retry_after_sec: 900,
            }
        ));
        p.advance(900 * SEC);
        let effects = p.enroll("n2", ip(1));
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { reverify: true, .. }
        ));
    }

    #[test]
    fn a_failed_reverify_removes_the_record_and_keeps_the_block() {
        let mut p = reverify_pool();
        p.report("n2", Vote::Blocked);
        let effects = p.round("n2", &blocked());
        assert!(!p.state.nodes.contains_key(&id("n2")));
        assert!(deletes(&effects, "n2"));
        assert_eq!(p.state.blocks.len(), 1);
        assert_eq!(p.state.blocks[0].node_id, id("n1"));
        let effects = p.enroll("n3", ip(1));
        assert!(matches!(
            reply(&effects),
            Reply::EnrollRejected {
                reason: RejectReason::ReverifyPause,
                ..
            }
        ));
    }

    #[test]
    fn a_banned_master_leaves_its_domain_in_the_same_apply() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        // Nobody can replace it: selection keeps it as the master through the quarantine.
        p.run(Command::Tick);
        assert_eq!(p.state.assignments.0[DOMAIN][0].node, id("n1"));

        let effects = p.round("n1", &blocked());
        assert!(!p.state.assignments.0.contains_key(DOMAIN));
        let event = event_of(&effects, EventKind::MasterChanged);
        assert_eq!(event.node, Some(id("n1")));
        assert_eq!(event.owner, Some(OwnerId(1)));
        assert_eq!(event.detail["removed"], json!(id("n1")));
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::DeleteMasters { domain } if domain == DOMAIN))
        );

        // The next tick has nothing left to do for it, and DNS never goes empty.
        let effects = p.run(Command::Tick);
        assert!(!kinds(&effects).contains(&EventKind::MasterChanged));
        assert!(pushes(&effects).is_empty());
    }

    #[test]
    fn a_banned_master_with_a_replacement_gives_the_domain_to_it_on_the_next_tick() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.make_healthy("n2", ip(2));
        p.run(Command::Tick);
        assert_eq!(p.state.assignments.0[DOMAIN][0].node, id("n1"));
        // Banned before the selection had a tick to move the domain.
        p.state.nodes.get_mut(&id("n1")).unwrap().state = NodeState::Quarantine;
        p.beat("n1");
        p.probe("n1", true);
        p.round("n1", &blocked());
        assert!(!p.state.assignments.0.contains_key(DOMAIN));

        p.advance(SEC);
        p.beat("n2");
        p.report("n2", Vote::Clear);
        let effects = p.run(Command::Tick);
        assert_eq!(p.state.assignments.0[DOMAIN][0].node, id("n2"));
        assert!(pushes(&effects).iter().any(|e| matches!(
            e,
            Effect::PushDns { desired: RecordValue::A(ips), .. } if *ips == [ip(2)]
        )));
    }

    #[test]
    fn dead_and_pruned_records_are_removed_a_day_after_they_ended() {
        const DAY: i64 = 24 * 3600 * SEC;
        let mut p = Pool::new();
        p.make_healthy("dead", ip(1));
        let action = p.node_action("dead");
        p.run(Command::Retire(action));

        p.make_healthy("pruned", ip(2));
        p.quarantine("pruned");
        p.advance(3601 * SEC);
        p.beat("pruned");
        p.run(Command::Tick);
        assert_eq!(p.state_of("pruned"), NodeState::Pruned);
        let pruned_at = p.now;
        assert_eq!(p.state.tombstones.len(), 1);

        let retired_at = p.state.nodes[&id("dead")].state_since;
        p.now = Ts(retired_at.0 + DAY - 1);
        let effects = p.run(Command::Tick);
        assert!(p.state.nodes.contains_key(&id("dead")));
        assert!(!deletes(&effects, "dead"));

        p.now = Ts(retired_at.0 + DAY);
        let effects = p.run(Command::Tick);
        assert!(!p.state.nodes.contains_key(&id("dead")));
        assert!(!p.state.stats.contains_key(&id("dead")));
        assert!(deletes(&effects, "dead"));
        assert!(kinds(&effects).is_empty(), "removal is not an event");

        p.now = Ts(pruned_at.0 + DAY);
        let effects = p.run(Command::Tick);
        assert!(deletes(&effects, "pruned"));
        assert!(!p.state.nodes.contains_key(&id("pruned")));
        assert_eq!(p.state.tombstones.len(), 1, "the pause outlives the record");
    }

    #[test]
    fn a_banned_record_of_an_old_database_is_removed_by_the_first_tick() {
        let mut p = Pool::new();
        p.make_healthy("old", ip(1));
        let node = p.state.nodes.get_mut(&id("old")).unwrap();
        node.state = NodeState::Banned;
        node.state_since = p.now;
        let effects = p.run(Command::Tick);
        assert!(!p.state.nodes.contains_key(&id("old")));
        assert!(deletes(&effects, "old"));
    }

    #[test]
    fn unban_without_a_record_finds_the_block_by_the_node_id() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        p.round("n1", &blocked());
        assert!(!p.state.nodes.contains_key(&id("n1")));

        let action = p.node_action("n1");
        let effects = p.run(Command::AdminUnban(action));
        assert_eq!(*reply(&effects), Reply::Ok);
        assert!(p.state.blocks.is_empty());
        assert!(
            !p.state.nodes.contains_key(&id("n1")),
            "no record comes back"
        );
        let event = event_of(&effects, EventKind::BanLifted);
        assert_eq!(event.node, Some(id("n1")));
        assert_eq!(event.detail, json!({ "admin": true }));
        assert_eq!(
            event.owner,
            Some(OwnerId(1)),
            "the owner's hooks get it although the record is gone"
        );
        let ops = persists(&effects);
        assert!(ops.iter().any(|op| matches!(
            op,
            PersistOp::LiftBan { ip: a, admin: true, .. } if *a == ip(1)
        )));
        assert!(
            ops.iter()
                .any(|op| matches!(op, PersistOp::DeleteIpBlock { ip: a } if *a == ip(1)))
        );

        // The address enrolls as a normal node again.
        let effects = p.enroll("n2", ip(1));
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled {
                reverify: false,
                ..
            }
        ));
    }

    #[test]
    fn unban_of_an_unknown_node_is_not_found_and_changes_nothing() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.quarantine("n1");
        p.round("n1", &blocked());
        let action = p.node_action("ghost");
        let effects = p.run(Command::AdminUnban(action));
        assert_eq!(*reply(&effects), Reply::UnknownNode);
        assert_eq!(p.state.blocks.len(), 1);
        assert!(kinds(&effects).is_empty());
    }

    #[test]
    fn unban_by_the_banned_id_ends_the_reverify_of_the_node_on_that_ip() {
        let mut p = reverify_pool();
        let action = p.node_action("n1");
        let effects = p.run(Command::AdminUnban(action));
        assert_eq!(*reply(&effects), Reply::Ok);
        assert!(p.state.blocks.is_empty());
        let n2 = &p.state.nodes[&id("n2")];
        assert!(!n2.reverify, "nothing is left for it to prove");
        assert!(
            persists(&effects).iter().any(
                |op| matches!(op, PersistOp::UpsertNode(n) if n.id == id("n2") && !n.reverify)
            )
        );
    }

    #[test]
    fn unban_of_a_live_node_lifts_the_block_its_old_ban_left() {
        let mut p = Pool::new();
        p.make_healthy("n1", ip(1));
        p.state.blocks.push(IpBlock {
            ip: ip(9),
            node_id: id("n1"),
            blocked_at: p.now,
            next_reverify_at: None,
            owner: Some(OwnerId(1)),
        });
        let action = p.node_action("n1");
        let effects = p.run(Command::AdminUnban(action));
        assert_eq!(*reply(&effects), Reply::Ok);
        assert!(p.state.blocks.is_empty());
        assert_eq!(
            p.state_of("n1"),
            NodeState::Healthy,
            "the record is untouched"
        );
    }
}
