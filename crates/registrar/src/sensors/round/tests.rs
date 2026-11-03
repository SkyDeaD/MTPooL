//! The round runner with fake sensors and a real state owner.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use mtpool_core::{
    AbstainReason, EngineKind, NodeRecord, NodeState, OwnerId, PoolState, ProxyUser, Ts, Vote,
};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::*;
use crate::actor::{Actor, ActorOptions};
use crate::store::Store;

const NODE_IP: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
const POOL_SECRET: &str = "00112233445566778899aabbccddeeff";

/// What a fake sensor does when asked.
#[derive(Clone, Copy)]
enum Mode {
    /// Answers with the vote after the delay.
    Answer(Vote, Duration),
    /// Never answers.
    Hang,
    /// Panics.
    Panic,
}

struct FakeSensor {
    kind: SensorKind,
    mode: Mode,
    calls: AtomicUsize,
    targets: Mutex<Vec<Target>>,
}

impl FakeSensor {
    fn new(kind: SensorKind, mode: Mode) -> Arc<FakeSensor> {
        Arc::new(FakeSensor {
            kind,
            mode,
            calls: AtomicUsize::new(0),
            targets: Mutex::new(Vec::new()),
        })
    }

    fn answering(kind: SensorKind, vote: Vote, delay: Duration) -> Arc<FakeSensor> {
        FakeSensor::new(kind, Mode::Answer(vote, delay))
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Sensor for FakeSensor {
    fn kind(&self) -> SensorKind {
        self.kind
    }

    async fn vote(&self, target: &Target) -> SensorVote {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.targets.lock().expect("lock").push(target.clone());
        match self.mode {
            Mode::Answer(vote, delay) => {
                tokio::time::sleep(delay).await;
                SensorVote {
                    vote,
                    evidence: json!({"from": format!("{:?}", self.kind)}),
                }
            }
            Mode::Hang => std::future::pending().await,
            Mode::Panic => panic!("a sensor that is broken"),
        }
    }
}

fn dyn_of(sensors: &[&Arc<FakeSensor>]) -> Vec<Arc<dyn Sensor>> {
    sensors
        .iter()
        .map(|s| Arc::clone(*s) as Arc<dyn Sensor>)
        .collect()
}

fn id(name: &str) -> NodeId {
    NodeId::parse(name).expect("a node id")
}

fn now() -> Ts {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis();
    Ts(i64::try_from(ms).expect("fits"))
}

fn settings() -> Settings {
    Settings {
        tls_domain: "example.com".to_owned(),
        proxy_users: vec![ProxyUser {
            name: "main".to_owned(),
            secret_hex: POOL_SECRET.to_owned(),
        }],
        ..Settings::default()
    }
}

/// A node in quarantine whose heartbeat is fresh: two `Blocked` votes ban it.
fn quarantined(name: &str) -> NodeRecord {
    let s = settings();
    let mut node = NodeRecord::new(
        id(name),
        OwnerId(1),
        NODE_IP,
        443,
        EngineKind::Telemt,
        now(),
        &s,
    );
    node.state = NodeState::Quarantine;
    node.health.tcp.up = true;
    node.health.last_heartbeat = now();
    node
}

fn actor_with(
    nodes: Vec<NodeRecord>,
    drivers: Vec<Arc<dyn EffectDriver>>,
) -> (tempfile::TempDir, ActorHandle) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(&dir.path().join("registrar.db")).expect("store");
    let mut state = PoolState::new(settings());
    for node in nodes {
        state.nodes.insert(node.id.clone(), node);
    }
    // No tick comes during the test: rounds are started by the test alone.
    let day = Duration::from_secs(86_400);
    let handle = Actor::spawn_with(
        state,
        store,
        drivers,
        ActorOptions {
            selection_tick: day,
            clock_check_tick: day,
            ..ActorOptions::default()
        },
    );
    (dir, handle)
}

fn target() -> Target {
    Target {
        ip: NODE_IP,
        port: 443,
        sni: "example.com".to_owned(),
        secret: format!("ee{POOL_SECRET}"),
    }
}

fn votes_of(command: Command) -> (NodeId, Generation, Vec<(SensorKind, SensorVote)>) {
    match command {
        Command::RoundDone {
            node,
            generation,
            votes,
        } => (node, generation, votes),
        other => panic!("not a RoundDone: {other:?}"),
    }
}

fn kinds(votes: &[(SensorKind, SensorVote)]) -> Vec<SensorKind> {
    votes.iter().map(|(k, _)| *k).collect()
}

fn vote_of(votes: &[(SensorKind, SensorVote)], kind: SensorKind) -> &SensorVote {
    &votes
        .iter()
        .find(|(k, _)| *k == kind)
        .expect("a vote of the kind")
        .1
}

fn run_round(node: &str, generation: Generation, fresh_gp: Option<SensorVote>) -> Effect {
    Effect::RunRound {
        node: id(node),
        generation,
        ip: NODE_IP,
        port: 443,
        fresh_gp,
    }
}

async fn node_state(handle: &ActorHandle, node: &str) -> String {
    // A command after the one under test: the actor has applied everything queued before it.
    handle.inspect(|_| ()).await.expect("the actor runs");
    let snapshot = handle.snapshots().load();
    let nodes: Value = serde_json::from_str(&snapshot.nodes_json).expect("nodes_json is JSON");
    nodes
        .as_array()
        .expect("array")
        .iter()
        .find(|n| n["id"] == node)
        .map(|n| n["state"].as_str().expect("state").to_owned())
        .unwrap_or_else(|| "absent".to_owned())
}

#[tokio::test(start_paused = true)]
async fn round_runs_parallel_and_returns_all_votes() {
    let gp = FakeSensor::answering(SensorKind::Globalping, Vote::Clear, Duration::from_secs(40));
    let ch = FakeSensor::answering(
        SensorKind::Cheburcheck,
        Vote::Blocked,
        Duration::from_secs(30),
    );
    let rf = FakeSensor::answering(SensorKind::RedFlag, Vote::Clear, Duration::from_secs(20));
    let runner = RoundRunner::new(dyn_of(&[&gp, &ch, &rf]));

    let started = Instant::now();
    let command = runner
        .round(id("alpha-00001"), Generation(3), target(), None)
        .await;
    // One after another it would be 90 seconds.
    assert_eq!(started.elapsed(), Duration::from_secs(40));

    let (node, generation, votes) = votes_of(command);
    assert_eq!(node, id("alpha-00001"));
    assert_eq!(generation, Generation(3));
    assert_eq!(
        kinds(&votes),
        [
            SensorKind::Globalping,
            SensorKind::Cheburcheck,
            SensorKind::RedFlag
        ]
    );
    assert_eq!(vote_of(&votes, SensorKind::Globalping).vote, Vote::Clear);
    assert_eq!(vote_of(&votes, SensorKind::Cheburcheck).vote, Vote::Blocked);
    assert_eq!(vote_of(&votes, SensorKind::RedFlag).vote, Vote::Clear);
    // Each sensor got the target as it was.
    for sensor in [&gp, &ch, &rf] {
        assert_eq!(sensor.targets.lock().expect("lock").as_slice(), [target()]);
    }
}

#[tokio::test(start_paused = true)]
async fn one_sensor_timeout_does_not_lose_others() {
    let gp = FakeSensor::new(SensorKind::Globalping, Mode::Hang);
    let ch = FakeSensor::answering(
        SensorKind::Cheburcheck,
        Vote::Blocked,
        Duration::from_secs(30),
    );
    let rf = FakeSensor::answering(SensorKind::RedFlag, Vote::Blocked, Duration::from_secs(5));
    let runner = RoundRunner::new(dyn_of(&[&gp, &ch, &rf]));

    let started = Instant::now();
    let command = runner
        .round(id("alpha-00001"), Generation(0), target(), None)
        .await;
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(120),
        "the cut is 120 s"
    );

    let (_, _, votes) = votes_of(command);
    assert_eq!(votes.len(), 3, "no vote is lost");
    let hung = vote_of(&votes, SensorKind::Globalping);
    assert_eq!(hung.vote, Vote::Abstain(AbstainReason::Timeout));
    assert_eq!(hung.evidence["reason"], "timeout");
    assert_eq!(vote_of(&votes, SensorKind::Cheburcheck).vote, Vote::Blocked);
    assert_eq!(vote_of(&votes, SensorKind::RedFlag).vote, Vote::Blocked);
}

#[tokio::test(start_paused = true)]
async fn the_sensor_timeout_is_120_seconds_unless_changed() {
    assert_eq!(SENSOR_TIMEOUT, Duration::from_secs(120));
    let gp = FakeSensor::new(SensorKind::Globalping, Mode::Hang);
    let runner = RoundRunner::new(dyn_of(&[&gp])).with_sensor_timeout(Duration::from_secs(7));
    let started = Instant::now();
    runner
        .round(id("alpha-00001"), Generation(0), target(), None)
        .await;
    assert_eq!(started.elapsed(), Duration::from_secs(7));
}

#[tokio::test(start_paused = true)]
async fn a_panicking_sensor_abstains_and_the_others_stay() {
    let gp = FakeSensor::new(SensorKind::Globalping, Mode::Panic);
    let ch = FakeSensor::answering(SensorKind::Cheburcheck, Vote::Clear, Duration::from_secs(1));
    let runner = RoundRunner::new(dyn_of(&[&gp, &ch]));
    let command = runner
        .round(id("alpha-00001"), Generation(0), target(), None)
        .await;
    let (_, _, votes) = votes_of(command);
    assert_eq!(
        vote_of(&votes, SensorKind::Globalping).vote,
        Vote::Abstain(AbstainReason::Unavailable)
    );
    assert_eq!(vote_of(&votes, SensorKind::Cheburcheck).vote, Vote::Clear);
}

#[tokio::test(start_paused = true)]
async fn round_uses_fresh_agent_report_without_central_gp() {
    let gp = FakeSensor::answering(
        SensorKind::Globalping,
        Vote::Blocked,
        Duration::from_secs(1),
    );
    let ch = FakeSensor::answering(SensorKind::Cheburcheck, Vote::Clear, Duration::from_secs(1));
    let rf = FakeSensor::answering(SensorKind::RedFlag, Vote::Clear, Duration::from_secs(1));
    let runner = RoundRunner::new(dyn_of(&[&gp, &ch, &rf]));
    let fresh = SensorVote {
        vote: Vote::Clear,
        evidence: json!({"source": "agent_report", "at": 1_700_000_000_000_i64}),
    };

    let command = runner
        .round(
            id("alpha-00001"),
            Generation(0),
            target(),
            Some(fresh.clone()),
        )
        .await;

    assert_eq!(gp.calls(), 0, "the central measurement is not made");
    assert_eq!(ch.calls(), 1);
    assert_eq!(rf.calls(), 1);
    let (_, _, votes) = votes_of(command);
    assert_eq!(
        kinds(&votes),
        [
            SensorKind::Globalping,
            SensorKind::Cheburcheck,
            SensorKind::RedFlag
        ]
    );
    assert_eq!(*vote_of(&votes, SensorKind::Globalping), fresh);
}

#[tokio::test(start_paused = true)]
async fn without_a_fresh_report_the_central_globalping_runs() {
    let gp = FakeSensor::answering(
        SensorKind::Globalping,
        Vote::Blocked,
        Duration::from_secs(1),
    );
    let runner = RoundRunner::new(dyn_of(&[&gp]));
    let command = runner
        .round(id("alpha-00001"), Generation(0), target(), None)
        .await;
    assert_eq!(gp.calls(), 1);
    let (_, _, votes) = votes_of(command);
    assert_eq!(vote_of(&votes, SensorKind::Globalping).vote, Vote::Blocked);
}

#[tokio::test(start_paused = true)]
async fn a_fresh_report_stands_even_when_no_globalping_sensor_is_built() {
    let ch = FakeSensor::answering(SensorKind::Cheburcheck, Vote::Clear, Duration::from_secs(1));
    let runner = RoundRunner::new(dyn_of(&[&ch]));
    let fresh = SensorVote {
        vote: Vote::Blocked,
        evidence: json!({"source": "agent_report"}),
    };
    let (_, _, votes) = votes_of(
        runner
            .round(id("alpha-00001"), Generation(0), target(), Some(fresh))
            .await,
    );
    assert_eq!(votes.len(), 2);
    assert_eq!(vote_of(&votes, SensorKind::Globalping).vote, Vote::Blocked);
}

#[test]
fn the_target_carries_the_sni_and_the_secret_in_link_format() {
    let t = target_for(&settings(), NODE_IP, 8443);
    assert_eq!(t.ip, NODE_IP);
    assert_eq!(t.port, 8443);
    assert_eq!(t.sni, "example.com");
    assert_eq!(
        t.secret,
        "ee00112233445566778899aabbccddeeff6578616d706c652e636f6d"
    );
}

#[test]
fn the_first_proxy_user_gives_the_secret() {
    let mut s = settings();
    s.proxy_users.push(ProxyUser {
        name: "second".to_owned(),
        secret_hex: "ffffffffffffffffffffffffffffffff".to_owned(),
    });
    assert!(target_for(&s, NODE_IP, 443).secret.contains(POOL_SECRET));
}

#[test]
fn with_no_proxy_user_the_secret_is_empty() {
    let mut s = settings();
    s.proxy_users.clear();
    let t = target_for(&s, NODE_IP, 443);
    assert_eq!(t.secret, "");
    assert_eq!(t.sni, "example.com");
}

#[tokio::test]
async fn the_driver_takes_run_round_only() {
    let runner = RoundRunner::new(vec![]);
    assert!(runner.handles(&run_round("alpha-00001", Generation(0), None)));
    let emit = Effect::Emit(mtpool_core::Event {
        kind: mtpool_core::EventKind::SensorRound,
        at: Ts(0),
        node: None,
        domain: None,
        detail: Value::Null,
    });
    assert!(!runner.handles(&emit));
}

/// The whole way: the driver runs the sensors, sends `RoundDone`, and the state owner acts on
/// two `Blocked` votes.
#[tokio::test]
async fn the_driver_reports_the_votes_to_the_state_owner() {
    let gp = FakeSensor::answering(SensorKind::Globalping, Vote::Blocked, Duration::ZERO);
    let ch = FakeSensor::answering(SensorKind::Cheburcheck, Vote::Blocked, Duration::ZERO);
    let node = quarantined("alpha-00001");
    let generation = node.generation;
    let (_dir, handle) = actor_with(vec![node], vec![]);
    let runner = RoundRunner::new(dyn_of(&[&gp, &ch]));
    assert_eq!(node_state(&handle, "alpha-00001").await, "quarantine");

    runner
        .run(run_round("alpha-00001", generation, None), handle.clone())
        .await;

    assert_eq!(node_state(&handle, "alpha-00001").await, "banned");
    // The target came from the live settings, not from the effect.
    let seen = gp.targets.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].ip, NODE_IP);
    assert_eq!(seen[0].sni, "example.com");
    assert_eq!(
        seen[0].secret,
        format!("ee{POOL_SECRET}6578616d706c652e636f6d")
    );
}

#[tokio::test]
async fn a_result_of_an_old_generation_changes_nothing() {
    let gp = FakeSensor::answering(SensorKind::Globalping, Vote::Blocked, Duration::ZERO);
    let ch = FakeSensor::answering(SensorKind::Cheburcheck, Vote::Blocked, Duration::ZERO);
    let node = quarantined("alpha-00001");
    let stale = Generation(node.generation.0 + 7);
    let (_dir, handle) = actor_with(vec![node], vec![]);
    let runner = RoundRunner::new(dyn_of(&[&gp, &ch]));
    runner
        .run(run_round("alpha-00001", stale, None), handle.clone())
        .await;
    assert_eq!(node_state(&handle, "alpha-00001").await, "quarantine");
}

#[tokio::test]
async fn one_round_per_node_in_flight() {
    let slow = FakeSensor::answering(
        SensorKind::Globalping,
        Vote::Clear,
        Duration::from_millis(300),
    );
    let (_dir, handle) = actor_with(vec![], vec![]);
    let runner = RoundRunner::new(dyn_of(&[&slow]));

    let first = tokio::spawn(runner.run(
        run_round("alpha-00001", Generation(0), None),
        handle.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(slow.calls(), 1, "the first round is running");

    // The same node again: dropped at once, no second budget spent.
    let started = std::time::Instant::now();
    runner
        .run(
            run_round("alpha-00001", Generation(0), None),
            handle.clone(),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "dropped, not waited for"
    );
    assert_eq!(slow.calls(), 1);

    // Another node is not held back.
    let other =
        tokio::spawn(runner.run(run_round("beta-00002", Generation(0), None), handle.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(slow.calls(), 2);

    first.await.expect("the first round ends");
    other.await.expect("the other round ends");

    // Once a round is over the next one is taken.
    runner
        .run(
            run_round("alpha-00001", Generation(0), None),
            handle.clone(),
        )
        .await;
    assert_eq!(slow.calls(), 3);
}

#[tokio::test]
async fn a_round_that_is_dropped_by_its_owner_frees_the_node() {
    let hang = FakeSensor::new(SensorKind::Globalping, Mode::Hang);
    let (_dir, handle) = actor_with(vec![], vec![]);
    let runner = RoundRunner::new(dyn_of(&[&hang])).with_sensor_timeout(Duration::from_millis(100));
    // A round whose future is dropped half way (the task is aborted) must not lock the node.
    let task = tokio::spawn(runner.run(
        run_round("alpha-00001", Generation(0), None),
        handle.clone(),
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    task.abort();
    let _ = task.await;
    runner
        .run(
            run_round("alpha-00001", Generation(0), None),
            handle.clone(),
        )
        .await;
    assert_eq!(hang.calls(), 2);
}

#[tokio::test]
async fn a_stopped_state_owner_ends_the_round_quietly() {
    let gp = FakeSensor::answering(SensorKind::Globalping, Vote::Clear, Duration::ZERO);
    let (_dir, handle) = actor_with(vec![], vec![]);
    let runner = RoundRunner::new(dyn_of(&[&gp]));
    handle.stop().await;
    // Nothing to report to: no panic, no error.
    runner
        .run(
            run_round("alpha-00001", Generation(0), None),
            handle.clone(),
        )
        .await;
}

#[tokio::test(start_paused = true)]
async fn the_votes_of_a_round_are_one_per_sensor_in_order() {
    let rf = FakeSensor::answering(SensorKind::RedFlag, Vote::Clear, Duration::from_secs(1));
    let gp = FakeSensor::answering(SensorKind::Globalping, Vote::Clear, Duration::from_secs(9));
    // The order of the sensors is the order of the votes, not the order of arrival.
    let runner = RoundRunner::new(dyn_of(&[&rf, &gp]));
    let (_, _, votes) = votes_of(
        runner
            .round(id("alpha-00001"), Generation(0), target(), None)
            .await,
    );
    assert_eq!(kinds(&votes), [SensorKind::RedFlag, SensorKind::Globalping]);
}

#[tokio::test(start_paused = true)]
async fn the_round_has_one_deadline_not_one_per_sensor() {
    let gp = FakeSensor::new(SensorKind::Globalping, Mode::Hang);
    let ch = FakeSensor::new(SensorKind::Cheburcheck, Mode::Hang);
    let rf = FakeSensor::new(SensorKind::RedFlag, Mode::Hang);
    let runner = RoundRunner::new(dyn_of(&[&gp, &ch, &rf]));

    let started = Instant::now();
    let (_, _, votes) = votes_of(
        runner
            .round(id("alpha-00001"), Generation(0), target(), None)
            .await,
    );
    // Waited one after another it would be 360 seconds.
    assert_eq!(started.elapsed(), SENSOR_TIMEOUT);
    assert_eq!(votes.len(), 3);
    assert!(
        votes
            .iter()
            .all(|(_, v)| v.vote == Vote::Abstain(AbstainReason::Timeout))
    );
}

#[tokio::test(start_paused = true)]
async fn an_answer_that_arrives_before_the_deadline_is_kept_next_to_a_late_one() {
    let gp = FakeSensor::new(SensorKind::Globalping, Mode::Hang);
    // Collected second, but already done when the first one times out.
    let ch = FakeSensor::answering(
        SensorKind::Cheburcheck,
        Vote::Blocked,
        Duration::from_secs(100),
    );
    let runner = RoundRunner::new(dyn_of(&[&gp, &ch]));
    let started = Instant::now();
    let (_, _, votes) = votes_of(
        runner
            .round(id("alpha-00001"), Generation(0), target(), None)
            .await,
    );
    assert_eq!(started.elapsed(), SENSOR_TIMEOUT);
    assert_eq!(vote_of(&votes, SensorKind::Cheburcheck).vote, Vote::Blocked);
}
