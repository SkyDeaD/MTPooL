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
    EnrollDecision, EnrollInput, IpBlock, LifecycleEvent, NodeRecord, RoundResult, Tombstone,
    apply_round, decide_enroll, needs_sensors, next_round_at, record_prune, reset_tombstones,
    tick_node, unban,
};
use crate::selection::select;
use crate::settings::Settings;
use crate::srmd::{SrmdAction, SrmdError, rebalance, release, set_clients, take};
use crate::state::{
    Command, Effect, EnrollRequest, Event, EventKind, HeartbeatRequest, NodeAction, NodeStats,
    PersistOp, PoolState, Refusal, Reply, ReplyId, ReportRequest, SrmdManualAction,
    SrmdManualRequest, TerminateReason,
};
use crate::traffic::traffic_delta;
use crate::types::{Generation, NodeId, NodeState, Ts};
use crate::votes::{RoundVerdict, SensorKind, SensorVote, Vote, judge_round};

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
        domain: domain.map(str::to_owned),
        detail,
    }));
}

fn emit_node(out: &mut Out, now: Ts, kind: EventKind, node: &NodeId, detail: Value) {
    emit(out, now, kind, Some(node), None, detail);
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
    node.state = NodeState::Dead;
    node.state_since = now;
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
                emit_node(out, now, EventKind::NodeRegistered, &node.id, json!({}));
            }
            LifecycleEvent::Quarantined { reason } => {
                emit_node(
                    out,
                    now,
                    EventKind::NodeQuarantined,
                    &node.id,
                    json!({ "reason": reason }),
                );
            }
            LifecycleEvent::Recovered => {
                reset_tombstones(tombstones, node);
                // The first move from `Pending` into the queue is not a recovery.
                if before == NodeState::Quarantine {
                    emit_node(out, now, EventKind::NodeRecovered, &node.id, json!({}));
                }
            }
            LifecycleEvent::Banned { votes } => {
                emit_node(
                    out,
                    now,
                    EventKind::NodeBanned,
                    &node.id,
                    json!({ "votes": votes_json(&votes) }),
                );
            }
            LifecycleEvent::ReverifyFailed { retry_after_sec } => {
                emit_node(
                    out,
                    now,
                    EventKind::NodeBanned,
                    &node.id,
                    json!({ "reverify": true, "retry_after_sec": retry_after_sec }),
                );
            }
            LifecycleEvent::Dead => {
                emit_node(
                    out,
                    now,
                    EventKind::NodeDead,
                    &node.id,
                    json!({ "reason": "unreachable" }),
                );
            }
            LifecycleEvent::Pruned => {
                record_prune(tombstones, node, now);
                emit_node(out, now, EventKind::NodePruned, &node.id, json!({}));
            }
            LifecycleEvent::BanLifted => {
                emit_node(out, now, EventKind::BanLifted, &node.id, json!({}));
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
    if let Some(old) = displaced.as_ref().and_then(|id| state.nodes.get_mut(id)) {
        kill(old, now);
        persist(out, PersistOp::UpsertNode(old.clone()));
        emit_node(
            out,
            now,
            EventKind::NodeDead,
            &old.id,
            json!({ "replaced_by": r.node_id }),
        );
    }

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

    if reverify {
        // The banned record of this IP is what the new node proves wrong: it goes.
        let gone: Vec<NodeId> = state
            .nodes
            .values()
            .filter(|n| n.state == NodeState::Banned && n.ip == r.ip && n.id != r.node_id)
            .map(|n| n.id.clone())
            .collect();
        for id in gone {
            state.nodes.remove(&id);
            state.stats.remove(&id);
            persist(out, PersistOp::DeleteNode(id));
        }
    }

    let generation = record.generation;
    if reverify {
        // The decisive round starts at once; no report is waited for.
        record.last_round_at = Some(now);
    }
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
        &r.node_id,
        json!({ "reverify": reverify, "replaced": displaced }),
    );
    if reverify {
        out.push(Effect::RunRound {
            node: r.node_id.clone(),
            generation,
            ip: record.ip,
            port: record.port,
            fresh_gp: None,
        });
    }
    state.nodes.insert(r.node_id.clone(), record);
    (
        r.reply,
        Reply::Enrolled {
            node: r.node_id,
            generation,
            replaced: displaced,
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
            node.change_ip(r.ip, now);
            node.health.last_heartbeat = now;
            persist(out, PersistOp::UpsertNode(node.clone()));
        }
        Some(_) => {
            // The new address may not enroll: blocked, paused, or taken by another node.
            kill(node, now);
            persist(out, PersistOp::UpsertNode(node.clone()));
            emit_node(
                out,
                now,
                EventKind::NodeDead,
                &node.id,
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
    match r.globalping {
        Vote::Clear => {
            node.health.gp = GpStatus::Ok { at: now };
            node.health.last_report = now;
        }
        Vote::Blocked => {
            node.health.gp = GpStatus::Failed { at: now };
            node.health.last_report = now;
        }
        // No data neither proves nor refutes: the report time stays, so a long run of
        // them ages the last proof out.
        Vote::Abstain(_) => node.health.gp = node.health.gp.on_no_data(now),
    }
    persist(
        out,
        PersistOp::NodeHealth {
            node: node.id.clone(),
            health: node.health,
        },
    );

    if r.traffic.is_some() || r.clients.is_some() {
        let entry = stats.entry(r.node.clone()).or_default();
        if let Some(counters) = r.traffic {
            // The first reading only sets the base: what came before the registrar looked
            // is not traffic of this pool.
            if let Some(previous) = entry.counters {
                let from = traffic_delta(previous.from_client, counters.from_client);
                let to = traffic_delta(previous.to_client, counters.to_client);
                entry.from_client_total = entry.from_client_total.saturating_add(from);
                entry.to_client_total = entry.to_client_total.saturating_add(to);
                if from > 0 || to > 0 {
                    persist(
                        out,
                        PersistOp::Traffic {
                            node: r.node.clone(),
                            at: now,
                            from_client: from,
                            to_client: to,
                        },
                    );
                }
            }
            entry.counters = Some(counters);
        }
        if r.clients.is_some() {
            entry.clients = r.clients;
        }
        persist(
            out,
            PersistOp::NodeStats {
                node: r.node.clone(),
                stats: entry.clone(),
            },
        );
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
        &n.id,
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
            &node.id,
            json!({ "retired": true }),
        );
    }
    (a.reply, Reply::Ok)
}

fn admin_unban(state: &mut PoolState, a: NodeAction, now: Ts, out: &mut Out) -> Answer {
    let PoolState { nodes, blocks, .. } = state;
    let Some(node) = nodes.get_mut(&a.node) else {
        return (a.reply, Reply::UnknownNode);
    };
    if unban(node, blocks, now).is_empty() {
        return (a.reply, Reply::Refused(Refusal::NotBanned));
    }
    persist(out, PersistOp::UpsertNode(node.clone()));
    emit_node(
        out,
        now,
        EventKind::BanLifted,
        &node.id,
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
        let round_due = needs_sensors(node, now, settings)
            && next_round_at(node, settings).is_some_and(|at| at <= now);
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
            emit(
                out,
                now,
                EventKind::DnsApplied,
                None,
                Some(domain),
                json!({}),
            );
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
    use crate::health::GpStatus;
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

        // Quarantine to Banned.
        p.quarantine("n1");
        let effects = p.round("n1", &blocked());
        assert_eq!(p.state_of("n1"), NodeState::Banned);
        assert_eq!(
            kinds(&effects),
            [EventKind::SensorRound, EventKind::NodeBanned]
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::UpsertIpBlock(b) if b.ip == ip(1)))
        );

        // Banned to Pending by an admin.
        let action = p.node_action("n1");
        let effects = p.run(Command::AdminUnban(action));
        assert_eq!(p.state_of("n1"), NodeState::Pending);
        assert_eq!(kinds(&effects), [EventKind::BanLifted]);
        assert!(matches!(reply(&effects), Reply::Ok));
        assert!(p.state.blocks.is_empty());

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
        p.round("n1", &blocked());
        assert_eq!(p.state_of("n1"), NodeState::Banned);

        let effects = p.enroll("n2", ip(1));
        assert!(matches!(
            reply(&effects),
            Reply::Enrolled { reverify: true, .. }
        ));
        assert!(
            !p.state.nodes.contains_key(&id("n1")),
            "the banned record of the IP is dropped"
        );
        assert!(
            persists(&effects)
                .iter()
                .any(|op| matches!(op, PersistOp::DeleteNode(n) if *n == id("n1")))
        );
        assert!(effects.iter().any(|e| matches!(
            e,
            Effect::RunRound { node, fresh_gp: None, .. } if *node == id("n2")
        )));
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
}
