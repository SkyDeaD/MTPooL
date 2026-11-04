//! Property tests: random command sequences against the invariants of the pool.
//!
//! A sequence is made of abstract operations. Each one is resolved against the current state
//! (which node, which generation, whose IP), so most commands hit a live node instead of an
//! unknown id, and `property_driver_reaches_interesting_states` checks the sequences do get
//! to masters, quarantine, bans and DNS pushes.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::net::Ipv4Addr;

use mtpool_core::{
    AbstainReason, Command, Effect, EngineKind, EnrollRequest, EventKind, Generation,
    HeartbeatRequest, NodeAction, NodeId, NodeState, OwnerId, PoolState, ReplyId, ReportRequest,
    SensorKind, SensorVote, Settings, SrmdManualAction, SrmdManualRequest, Ts, Vote,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

const START: i64 = 1_000_000;
const NODES: u8 = 8;
const IPS: u8 = 6;
const KINDS: [SensorKind; 3] = [
    SensorKind::Globalping,
    SensorKind::Cheburcheck,
    SensorKind::RedFlag,
];

fn ip(i: u8) -> Ipv4Addr {
    Ipv4Addr::new(95, 216, 0, 1 + i % IPS)
}

fn node_id(i: u8) -> NodeId {
    NodeId::parse(&format!("n{}-a1b2c", i % NODES)).unwrap()
}

fn base_settings() -> Settings {
    Settings {
        latch_fail_threshold: 2,
        latch_ok_threshold: 1,
        domains: vec!["p.example.com".to_owned()],
        ..Settings::default()
    }
}

fn vote(code: u8) -> Vote {
    match code % 4 {
        0 | 1 => Vote::Blocked,
        2 => Vote::Clear,
        _ => Vote::Abstain(AbstainReason::Timeout),
    }
}

#[derive(Debug, Clone)]
enum Op {
    Enroll {
        id: u8,
        ip: u8,
        odd_port: bool,
    },
    Heartbeat {
        node: u8,
        move_to: Option<u8>,
    },
    Report {
        node: u8,
        metrics_ok: bool,
        gp: u8,
        stale: bool,
    },
    TcpProbe {
        node: u8,
        open: bool,
        stale: bool,
    },
    Round {
        node: u8,
        votes: Vec<(u8, u8)>,
        stale: bool,
    },
    DnsDone {
        domain: u8,
        ok: bool,
        stale: bool,
    },
    Retire(u8),
    Unban(u8),
    Recheck(u8),
    Srmd {
        domain: u8,
        take: bool,
    },
    Settings {
        masters: u32,
        rotation: u32,
        srmd: bool,
        max_nodes: u32,
        odd_port: bool,
    },
    /// Heartbeat, open port, a clean report and a tick: the way a node gets to `Healthy`.
    Warmup(u8),
    /// A failed Globalping report, a heartbeat and a tick: the way a node gets to quarantine.
    Poison(u8),
    Advance(u32),
    Tick,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    let n = 0..NODES;
    prop_oneof![
        8 => n.clone().prop_map(Op::Warmup),
        4 => n.clone().prop_map(Op::Poison),
        8 => Just(Op::Tick),
        6 => (0u32..40_000).prop_map(Op::Advance),
        2 => (0u32..3_000_000).prop_map(Op::Advance),
        4 => (n.clone(), 0..IPS, any::<bool>())
            .prop_map(|(id, ip, odd_port)| Op::Enroll { id, ip, odd_port }),
        3 => (n.clone(), prop::option::of(0..IPS))
            .prop_map(|(node, move_to)| Op::Heartbeat { node, move_to }),
        3 => (n.clone(), any::<bool>(), 0u8..4, any::<bool>())
            .prop_map(|(node, metrics_ok, gp, stale)| Op::Report { node, metrics_ok, gp, stale }),
        2 => (n.clone(), any::<bool>(), any::<bool>())
            .prop_map(|(node, open, stale)| Op::TcpProbe { node, open, stale }),
        7 => (n.clone(), prop::collection::vec((0u8..3, 0u8..4), 0..5), any::<bool>())
            .prop_map(|(node, votes, stale)| Op::Round { node, votes, stale }),
        3 => (0u8..3, any::<bool>(), any::<bool>())
            .prop_map(|(domain, ok, stale)| Op::DnsDone { domain, ok, stale }),
        1 => n.clone().prop_map(Op::Retire),
        1 => n.clone().prop_map(Op::Unban),
        1 => n.prop_map(Op::Recheck),
        2 => (0u8..3, any::<bool>()).prop_map(|(domain, take)| Op::Srmd { domain, take }),
        2 => (1u32..=3, prop::sample::select(vec![0u32, 60, 1800]), any::<bool>(), 1u32..=3, any::<bool>())
            .prop_map(|(masters, rotation, srmd, max_nodes, odd_port)| Op::Settings {
                masters, rotation, srmd, max_nodes, odd_port,
            }),
    ]
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(op_strategy(), 1..80)
}

/// One applied command with the state around it.
struct Step<'a> {
    before: &'a PoolState,
    after: &'a PoolState,
    command: &'a Command,
    effects: &'a [Effect],
}

struct Driver {
    state: PoolState,
    now: Ts,
    reply: u64,
}

impl Driver {
    fn new() -> Driver {
        Driver {
            state: PoolState::new(base_settings()),
            now: Ts(START),
            reply: 0,
        }
    }

    fn reply_id(&mut self) -> ReplyId {
        self.reply += 1;
        ReplyId(self.reply)
    }

    /// An existing node when there is one, so most commands are not wasted on unknown ids.
    fn pick(&self, i: u8) -> NodeId {
        let ids: Vec<&NodeId> = self.state.nodes.keys().collect();
        if ids.is_empty() {
            node_id(i)
        } else {
            ids[usize::from(i) % ids.len()].clone()
        }
    }

    fn generation(&self, id: &NodeId, stale: bool) -> Generation {
        let g = self
            .state
            .nodes
            .get(id)
            .map_or(Generation(0), |n| n.generation);
        if stale { Generation(g.0 + 1) } else { g }
    }

    fn ip_of(&self, id: &NodeId) -> Ipv4Addr {
        self.state.nodes.get(id).map_or(ip(0), |n| n.ip)
    }

    fn domain(&self, i: u8) -> String {
        let d = &self.state.settings.domains;
        if d.is_empty() {
            "p.example.com".to_owned()
        } else {
            d[usize::from(i) % d.len()].clone()
        }
    }

    fn dns_generation(&self, domain: &str, stale: bool) -> Generation {
        let g = self
            .state
            .dns
            .get(domain)
            .map_or(Generation(0), |d| d.generation);
        if stale { Generation(g.0 + 1) } else { g }
    }

    fn commands(&mut self, op: &Op) -> Vec<Command> {
        match op {
            Op::Advance(ms) => {
                self.now = Ts(self.now.0 + i64::from(*ms));
                Vec::new()
            }
            Op::Tick => vec![Command::Tick],
            Op::Enroll {
                id,
                ip: i,
                odd_port,
            } => vec![Command::Enroll(EnrollRequest {
                reply: self.reply_id(),
                node_id: node_id(*id),
                owner: OwnerId(1),
                ip: ip(*i),
                port: if *odd_port { 8443 } else { 443 },
                engine: EngineKind::Telemt,
            })],
            Op::Heartbeat { node, move_to } => {
                let id = self.pick(*node);
                let addr = move_to.map_or_else(|| self.ip_of(&id), ip);
                vec![Command::Heartbeat(HeartbeatRequest {
                    reply: self.reply_id(),
                    node: id,
                    ip: addr,
                })]
            }
            Op::Report {
                node,
                metrics_ok,
                gp,
                stale,
            } => {
                let id = self.pick(*node);
                vec![self.report(&id, *metrics_ok, vote(*gp), *stale)]
            }
            Op::TcpProbe { node, open, stale } => {
                let id = self.pick(*node);
                vec![Command::TcpProbe {
                    generation: self.generation(&id, *stale),
                    node: id,
                    open: *open,
                }]
            }
            Op::Round { node, votes, stale } => {
                let id = self.pick(*node);
                let votes = votes
                    .iter()
                    .map(|&(kind, code)| {
                        (
                            KINDS[usize::from(kind) % 3],
                            SensorVote {
                                vote: vote(code),
                                evidence: serde_json::Value::Null,
                            },
                        )
                    })
                    .collect();
                vec![Command::RoundDone {
                    generation: self.generation(&id, *stale),
                    node: id,
                    votes,
                }]
            }
            Op::DnsDone { domain, ok, stale } => {
                let domain = self.domain(*domain);
                vec![Command::DnsDone {
                    generation: self.dns_generation(&domain, *stale),
                    result: if *ok {
                        Ok(())
                    } else {
                        Err("provider error".to_owned())
                    },
                    domain,
                }]
            }
            Op::Retire(n) => {
                let node = self.pick(*n);
                vec![Command::Retire(NodeAction {
                    reply: self.reply_id(),
                    node,
                })]
            }
            Op::Unban(n) => {
                let node = self.pick(*n);
                vec![Command::AdminUnban(NodeAction {
                    reply: self.reply_id(),
                    node,
                })]
            }
            Op::Recheck(n) => {
                let node = self.pick(*n);
                vec![Command::Recheck(NodeAction {
                    reply: self.reply_id(),
                    node,
                })]
            }
            Op::Srmd { domain, take } => vec![Command::SrmdManual(SrmdManualRequest {
                reply: self.reply_id(),
                domain: self.domain(*domain),
                action: if *take {
                    SrmdManualAction::Take
                } else {
                    SrmdManualAction::Release
                },
            })],
            Op::Settings {
                masters,
                rotation,
                srmd,
                max_nodes,
                odd_port,
            } => {
                let mut s = self.state.settings.clone();
                s.masters_per_domain = *masters;
                s.rotation_ttl_sec = *rotation;
                s.srmd.enabled = *srmd;
                s.srmd.max_nodes_per_domain = *max_nodes;
                s.srmd.diverge_ticks = 2;
                s.proxy_port = if *odd_port { 8443 } else { 443 };
                vec![Command::SettingsChanged(s)]
            }
            Op::Warmup(n) => {
                let id = self.pick(*n);
                vec![
                    Command::Heartbeat(HeartbeatRequest {
                        reply: self.reply_id(),
                        node: id.clone(),
                        ip: self.ip_of(&id),
                    }),
                    Command::TcpProbe {
                        generation: self.generation(&id, false),
                        node: id.clone(),
                        open: true,
                    },
                    self.report(&id, true, Vote::Clear, false),
                    Command::Tick,
                ]
            }
            Op::Poison(n) => {
                let id = self.pick(*n);
                vec![
                    Command::Heartbeat(HeartbeatRequest {
                        reply: self.reply_id(),
                        node: id.clone(),
                        ip: self.ip_of(&id),
                    }),
                    self.report(&id, true, Vote::Blocked, false),
                    Command::Tick,
                ]
            }
        }
    }

    fn report(&mut self, id: &NodeId, metrics_ok: bool, gp: Vote, stale: bool) -> Command {
        Command::Report(ReportRequest {
            reply: self.reply_id(),
            node: id.clone(),
            generation: self.generation(id, stale),
            metrics_ok,
            globalping: gp,
            traffic: None,
            clients: Some(3),
        })
    }
}

/// Runs the sequence; `observe` sees every command with the state before and after.
fn run(
    ops: &[Op],
    mut observe: impl FnMut(&Step) -> Result<(), TestCaseError>,
) -> Result<(PoolState, Vec<Vec<Effect>>), TestCaseError> {
    let mut driver = Driver::new();
    let mut all = Vec::new();
    for op in ops {
        for command in driver.commands(op) {
            let before = driver.state.clone();
            let effects = mtpool_core::apply(&mut driver.state, command.clone(), driver.now);
            observe(&Step {
                before: &before,
                after: &driver.state,
                command: &command,
                effects: &effects,
            })?;
            all.push(effects);
        }
    }
    Ok((driver.state, all))
}

fn announced(effects: &[Effect], id: &NodeId, kind: EventKind) -> bool {
    effects
        .iter()
        .any(|e| matches!(e, Effect::Emit(ev) if ev.kind == kind && ev.node.as_ref() == Some(id)))
}

fn check_masters_are_healthy(s: &PoolState) -> Result<(), TestCaseError> {
    for (domain, slots) in &s.assignments.0 {
        prop_assert!(
            slots.len() <= s.settings.masters_per_domain as usize,
            "{domain} holds more slots than masters_per_domain"
        );
        for (i, slot) in slots.iter().enumerate() {
            let healthy = s
                .nodes
                .get(&slot.node)
                .is_some_and(|n| n.state == NodeState::Healthy);
            if healthy {
                continue;
            }
            // A master that is not healthy is only allowed to stay when nobody could take
            // its place: an empty queue does not clear the assignments.
            let others: Vec<&NodeId> = slots
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, o)| &o.node)
                .collect();
            let taken: BTreeSet<Ipv4Addr> = slots
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .filter_map(|(_, o)| s.nodes.get(&o.node).map(|n| n.ip))
                .collect();
            let candidate = s.nodes.values().find(|n| {
                n.state == NodeState::Healthy
                    && n.id != slot.node
                    && !others.contains(&&n.id)
                    && !taken.contains(&n.ip)
            });
            prop_assert!(
                candidate.is_none(),
                "{domain}: {} holds a slot in state {:?} while {} is an eligible Healthy node",
                slot.node,
                s.nodes.get(&slot.node).map(|n| n.state),
                candidate.map(|c| c.id.to_string()).unwrap_or_default()
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn master_is_always_healthy(ops in ops_strategy()) {
        run(&ops, |step| {
            if matches!(step.command, Command::Tick) {
                check_masters_are_healthy(step.after)?;
            }
            Ok(())
        })?;
    }

    /// A ban leaves no record behind, so it is judged by what it writes and announces: the ban
    /// row, the event and the new block all need a round with two `Blocked` votes.
    #[test]
    fn banned_only_with_two_blocked_votes(ops in ops_strategy()) {
        run(&ops, |step| {
            let mut banned: Vec<&NodeId> = Vec::new();
            for effect in step.effects {
                match effect {
                    Effect::Persist(mtpool_core::PersistOp::RecordBan { node, reverify: false, .. }) => {
                        banned.push(node);
                    }
                    Effect::Emit(ev) if ev.kind == EventKind::NodeBanned => {
                        banned.extend(ev.node.as_ref());
                    }
                    _ => {}
                }
            }
            let new_blocks = step
                .after
                .blocks
                .iter()
                .filter(|b| !step.before.blocks.iter().any(|o| o.ip == b.ip));
            banned.extend(new_blocks.map(|b| &b.node_id));
            for id in banned {
                let Command::RoundDone { node, votes, .. } = step.command else {
                    return Err(TestCaseError::fail(format!(
                        "{id} was banned by {:?}", step.command
                    )));
                };
                prop_assert_eq!(node, id);
                // One voice per sensor: the first answer of a kind counts.
                let mut seen = BTreeSet::new();
                let blocked = votes
                    .iter()
                    .filter(|(kind, _)| seen.insert(*kind))
                    .filter(|(_, v)| v.vote == Vote::Blocked)
                    .count();
                prop_assert!(blocked >= 2, "{id} banned with {blocked} Blocked votes: {votes:?}");
            }
            Ok(())
        })?;
    }

    /// A live record leaves only through a ban. A record in a final state goes a day later.
    #[test]
    fn a_record_goes_only_after_a_ban_or_a_day_in_a_final_state(ops in ops_strategy()) {
        run(&ops, |step| {
            for (id, before) in &step.before.nodes {
                if step.after.nodes.contains_key(id) {
                    continue;
                }
                let deleted = step.effects.iter().any(
                    |e| matches!(e, Effect::Persist(mtpool_core::PersistOp::DeleteNode(n)) if n == id),
                );
                prop_assert!(deleted, "{id} left the state without a DeleteNode");
                prop_assert!(!step.after.stats.contains_key(id), "{id} left its stats behind");
                let live = matches!(
                    before.state,
                    NodeState::Pending | NodeState::Healthy | NodeState::Quarantine
                );
                if live {
                    prop_assert!(
                        announced(step.effects, id, EventKind::NodeBanned)
                            || announced(step.effects, id, EventKind::NodeReverifyFailed),
                        "{id} left from {:?} by {:?} without a ban",
                        before.state,
                        step.command
                    );
                } else {
                    prop_assert!(
                        matches!(step.command, Command::Tick),
                        "{id} left from {:?} by {:?}",
                        before.state,
                        step.command
                    );
                }
            }
            Ok(())
        })?;
    }

    #[test]
    fn no_domain_has_duplicate_ip(ops in ops_strategy()) {
        run(&ops, |step| {
            // The slots are rebuilt by the tick; between two ticks a node that left may still
            // sit in one, and its IP may already belong to another node.
            if !matches!(step.command, Command::Tick) {
                return Ok(());
            }
            let s = step.after;
            for (domain, slots) in &s.assignments.0 {
                let nodes: BTreeSet<&NodeId> = slots.iter().map(|x| &x.node).collect();
                prop_assert_eq!(nodes.len(), slots.len(), "{} holds a node twice", domain);
                let ips: Vec<Ipv4Addr> = slots
                    .iter()
                    .filter_map(|x| s.nodes.get(&x.node).map(|n| n.ip))
                    .collect();
                let unique: BTreeSet<&Ipv4Addr> = ips.iter().collect();
                prop_assert_eq!(unique.len(), ips.len(), "{} has a duplicate IP: {:?}", domain, ips);
            }
            for (domain, dns) in &s.dns {
                if let mtpool_core::RecordValue::A(ips) = &dns.desired {
                    let unique: BTreeSet<&Ipv4Addr> = ips.iter().collect();
                    prop_assert_eq!(unique.len(), ips.len(), "{} desired has a duplicate IP", domain);
                    prop_assert!(!ips.is_empty(), "{} desired A set is empty", domain);
                }
            }
            Ok(())
        })?;
    }

    #[test]
    fn apply_is_deterministic(ops in ops_strategy()) {
        let first = run(&ops, |_| Ok(()))?;
        let second = run(&ops, |_| Ok(()))?;
        prop_assert_eq!(first.0, second.0);
        prop_assert_eq!(first.1, second.1);
    }

    #[test]
    fn empty_state_tick_produces_no_dns(
        domains in prop::collection::vec("[a-z]{1,6}\\.example\\.com", 0..4),
        masters in 1u32..=3,
        srmd in any::<bool>(),
        times in prop::collection::vec(0i64..10_000_000, 1..6),
    ) {
        let mut settings = base_settings();
        let unique: BTreeSet<String> = domains.into_iter().collect();
        settings.domains = unique.into_iter().collect();
        settings.masters_per_domain = masters;
        settings.srmd.enabled = srmd;
        settings.srmd.diverge_ticks = 1;
        let mut state = PoolState::new(settings);
        for t in times {
            let effects = mtpool_core::apply(&mut state, Command::Tick, Ts(START + t));
            for e in &effects {
                prop_assert!(!matches!(e, Effect::PushDns { .. }), "empty pool pushed DNS: {e:?}");
                prop_assert!(
                    !matches!(e, Effect::Persist(mtpool_core::PersistOp::UpsertDns { .. })),
                    "empty pool stored DNS: {e:?}"
                );
            }
            prop_assert!(state.dns.is_empty());
        }
    }

    /// Every change of a node's state is announced with the event that names it. Two moves are
    /// silent by design: a first `Pending` to `Healthy`, and the reset an IP change causes.
    #[test]
    fn every_state_change_is_announced(ops in ops_strategy()) {
        run(&ops, |step| {
            for (id, after) in &step.after.nodes {
                let Some(before) = step.before.nodes.get(id) else {
                    prop_assert!(announced(step.effects, id, EventKind::NodeRegistered));
                    continue;
                };
                let replaced = matches!(step.command, Command::Enroll(r) if &r.node_id == id);
                if replaced {
                    let rejected = step
                        .effects
                        .iter()
                        .any(|e| matches!(e, Effect::Reply(_, mtpool_core::Reply::EnrollRejected { .. })));
                    if rejected {
                        prop_assert_eq!(before, after, "a rejected enrollment changed the node");
                    } else {
                        prop_assert!(announced(step.effects, id, EventKind::NodeRegistered));
                    }
                    continue;
                }
                if before.state == after.state {
                    continue;
                }
                let silent = (before.state == NodeState::Pending && after.state == NodeState::Healthy)
                    || (matches!(step.command, Command::Heartbeat(_)) && after.state == NodeState::Pending);
                if silent {
                    continue;
                }
                let expected = match (before.state, after.state) {
                    (_, NodeState::Quarantine) => EventKind::NodeQuarantined,
                    (_, NodeState::Healthy) => EventKind::NodeRecovered,
                    // A ban removes the record in the same `apply`.
                    (_, NodeState::Banned) => {
                        return Err(TestCaseError::fail(format!("{id} stayed Banned")));
                    }
                    (_, NodeState::Dead) => EventKind::NodeDead,
                    (_, NodeState::Pruned) => EventKind::NodePruned,
                    (_, NodeState::Pending) => EventKind::BanLifted,
                };
                prop_assert!(
                    announced(step.effects, id, expected),
                    "{id}: {:?} to {:?} by {:?} without {expected:?}",
                    before.state, after.state, step.command
                );
            }
            Ok(())
        })?;
    }
}

/// The generators must not be vacuous: the sequences have to reach the states the properties
/// are about.
#[test]
fn property_driver_reaches_interesting_states() {
    let seen = [const { Cell::new(false) }; 8];
    let mut runner = TestRunner::new_with_rng(
        Config {
            cases: 256,
            ..Config::default()
        },
        proptest::test_runner::TestRng::deterministic_rng(
            proptest::test_runner::RngAlgorithm::ChaCha,
        ),
    );
    runner
        .run(&ops_strategy(), |ops| {
            run(&ops, |step| {
                let s = step.after;
                let has = |state| s.nodes.values().any(|n| n.state == state);
                seen[0].set(seen[0].get() || has(NodeState::Healthy));
                seen[1].set(seen[1].get() || s.assignments.0.values().any(|v| !v.is_empty()));
                seen[2].set(seen[2].get() || has(NodeState::Quarantine));
                seen[3].set(
                    seen[3].get()
                        || step.effects.iter().any(
                            |e| matches!(e, Effect::Emit(ev) if ev.kind == EventKind::NodeBanned),
                        ),
                );
                seen[4].set(seen[4].get() || has(NodeState::Dead));
                seen[5].set(
                    seen[5].get()
                        || step
                            .effects
                            .iter()
                            .any(|e| matches!(e, Effect::PushDns { .. })),
                );
                seen[6].set(seen[6].get() || s.dns.values().any(|d| d.applied.is_some()));
                seen[7].set(
                    seen[7].get()
                        || step
                            .effects
                            .iter()
                            .any(|e| matches!(e, Effect::RunRound { .. })),
                );
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
    let names = [
        "healthy node",
        "master assigned",
        "quarantine",
        "ban",
        "dead node",
        "DNS push",
        "DNS applied",
        "sensor round",
    ];
    for (flag, name) in seen.iter().zip(names) {
        assert!(flag.get(), "no generated sequence reached: {name}");
    }
}
