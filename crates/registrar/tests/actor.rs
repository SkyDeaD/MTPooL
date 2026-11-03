mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use mtpool_core::{
    Command, Effect, NodeAction, PoolState, Refusal, Reply, ReplyId, Settings, SrmdManualAction,
    SrmdManualRequest, Ts,
};
use mtpool_registrar::actor::{
    Actor, ActorHandle, ActorOptions, Clock, DriverFuture, EffectDriver,
};
use serde_json::Value;

fn node_state(handle: &ActorHandle, node: &str) -> String {
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
async fn no_node_expires_during_first_ttl_after_restart() {
    let base = Ts(1_700_000_000_000);
    let started = tokio::time::Instant::now();
    let clock: Clock = Arc::new(move || Ts(base.0 + started.elapsed().as_millis() as i64));
    // The heartbeat is 50 s old at start: stale 10 s later, well inside the 60 s TTL grace.
    let state = state_with(
        healthy_node("alpha-00001", base, 50_000),
        Settings::default(),
    );
    let (_dir, store) = open_store();
    let handle = Actor::spawn_with(
        state,
        store,
        vec![],
        ActorOptions {
            clock,
            ..ActorOptions::default()
        },
    );

    tokio::time::sleep(Duration::from_secs(40)).await;
    handle.call(barrier()).await.expect("actor runs");
    assert_eq!(node_state(&handle, "alpha-00001"), "healthy");

    tokio::time::sleep(Duration::from_secs(30)).await;
    handle.call(barrier()).await.expect("actor runs");
    assert_eq!(
        node_state(&handle, "alpha-00001"),
        "quarantine",
        "after the TTL the stale heartbeat counts"
    );
}

struct SlowDns {
    started: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl EffectDriver for SlowDns {
    fn handles(&self, effect: &Effect) -> bool {
        matches!(effect, Effect::PushDns { .. })
    }

    fn run(&self, _effect: Effect, _results: ActorHandle) -> DriverFuture {
        let started = Arc::clone(&self.started);
        let finished = Arc::clone(&self.finished);
        Box::pin(async move {
            started.store(true, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(10)).await;
            finished.store(true, Ordering::SeqCst);
        })
    }
}

#[tokio::test]
async fn slow_driver_does_not_block_commands() {
    let now = Ts(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64);
    let settings = Settings {
        domains: vec!["p.example.com".to_owned()],
        ..Settings::default()
    };
    let state = state_with(healthy_node("alpha-00001", now, 0), settings);
    let (_dir, store) = open_store();
    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let driver = SlowDns {
        started: Arc::clone(&started),
        finished: Arc::clone(&finished),
    };
    let handle = Actor::spawn(state, store, vec![Arc::new(driver)]);

    // An actor that awaited the driver inline would take 10 s here.
    tokio::time::timeout(Duration::from_millis(500), handle.call(Command::Tick))
        .await
        .expect("the tick is answered while the driver is still working")
        .expect("tick");
    let waited = Instant::now();
    while !started.load(Ordering::SeqCst) {
        assert!(
            waited.elapsed() < Duration::from_secs(2),
            "the tick did not reach the DNS driver"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let answer = tokio::time::timeout(
        Duration::from_millis(100),
        handle.call(heartbeat("alpha-00001", [45, 33, 1, 5])),
    )
    .await
    .expect("heartbeat answers while the driver sleeps")
    .expect("actor runs");
    assert_eq!(answer, Reply::Ok);
    assert!(
        started.load(Ordering::SeqCst) && !finished.load(Ordering::SeqCst),
        "the driver is still sleeping when the heartbeat is answered"
    );
}

#[tokio::test]
async fn each_call_gets_its_own_reply() {
    let (_dir, store) = open_store();
    let handle = Actor::spawn(PoolState::default(), store, vec![]);
    let alpha = "alpha-00001";
    let ip = [45, 33, 1, 5];

    // `join!` polls in order, so the commands queue up in this order and the actor applies
    // them as one batch or several; every future must still get the reply of its own command.
    let (
        enrolled,
        unknown_beat,
        not_banned,
        not_rechecknable,
        unknown_domain,
        unknown_retire,
        retired,
        beat_after,
    ) = tokio::join!(
        handle.call(enroll(alpha, ip)),
        handle.call(barrier()),
        handle.call(Command::AdminUnban(node_action(alpha))),
        handle.call(Command::Recheck(node_action(alpha))),
        handle.call(Command::SrmdManual(SrmdManualRequest {
            reply: ReplyId(0),
            domain: "nowhere.example.com".to_owned(),
            action: SrmdManualAction::Take,
        })),
        handle.call(Command::Retire(node_action("ghost-00000"))),
        handle.call(Command::Retire(node_action(alpha))),
        handle.call(heartbeat(alpha, ip)),
    );
    assert!(matches!(enrolled.unwrap(), Reply::Enrolled { .. }));
    assert_eq!(unknown_beat.unwrap(), Reply::UnknownNode);
    assert_eq!(not_banned.unwrap(), Reply::Refused(Refusal::NotBanned));
    assert_eq!(
        not_rechecknable.unwrap(),
        Reply::Refused(Refusal::NotRecheckable)
    );
    assert_eq!(
        unknown_domain.unwrap(),
        Reply::Refused(Refusal::UnknownDomain)
    );
    assert_eq!(unknown_retire.unwrap(), Reply::UnknownNode);
    assert_eq!(retired.unwrap(), Reply::Ok);
    assert!(
        matches!(beat_after.unwrap(), Reply::Terminate(_)),
        "the retired node is told to stop"
    );
}

fn node_action(name: &str) -> NodeAction {
    NodeAction {
        reply: ReplyId(0),
        node: node_id(name),
    }
}

#[tokio::test]
async fn snapshot_updated_after_state_change() {
    let (_dir, store) = open_store();
    let handle = Actor::spawn(PoolState::default(), store, vec![]);
    let before = handle.snapshots().load();
    assert_eq!(node_state(&handle, "alpha-00001"), "absent");

    let reply = handle
        .call(enroll("alpha-00001", [45, 33, 1, 5]))
        .await
        .expect("enroll");
    assert!(matches!(reply, Reply::Enrolled { .. }), "{reply:?}");

    let after = handle.snapshots().load();
    assert_eq!(node_state(&handle, "alpha-00001"), "pending");
    assert_ne!(before.etag, after.etag);
}

#[test]
fn clock_jump_is_the_wall_delta_minus_the_monotonic_delta() {
    use mtpool_registrar::actor::clock_jump_ms;
    assert_eq!(
        clock_jump_ms(Ts(1_000), Ts(31_000), Duration::from_secs(30)),
        0
    );
    assert_eq!(
        clock_jump_ms(Ts(1_000), Ts(121_000), Duration::from_secs(30)),
        90_000
    );
    assert_eq!(
        clock_jump_ms(Ts(1_000), Ts(1_000), Duration::from_secs(30)),
        -30_000
    );
}

#[tokio::test]
async fn call_after_stop_reports_a_stopped_actor() {
    let (_dir, store) = open_store();
    let handle = Actor::spawn(PoolState::default(), store, vec![]);
    handle.stop().await;
    assert!(handle.call(barrier()).await.is_err());
    assert!(handle.send(Command::Tick).await.is_err());
}
