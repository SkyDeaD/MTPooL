//! The node's own look at its proxy: the run of good and bad checks, the dead-kill rule and what
//! the lifecycle does about them.

mod common;

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use mtpool_agent::dead_kill::{DEAD_AFTER, verdict};
use mtpool_agent::lifecycle::{AgentPhase, Health, ReportSource, Step, Watch};
use mtpool_agent::local_health::{LocalHealth, LocalTcp, PortCheck, check_once};
use mtpool_protocol::agent::ReportRequest;
use tokio::time::Instant;
use wiremock::ResponseTemplate;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[test]
fn local_health_3_bad_silent_2_good_back() {
    let t0 = Instant::now();
    let mut health = LocalHealth::new();
    assert!(!health.is_silent());

    health.observe(t0, false);
    health.observe(t0 + secs(30), false);
    assert!(!health.is_silent(), "two bad checks are not enough");
    health.observe(t0 + secs(60), false);
    assert!(health.is_silent(), "the third bad check silences the node");

    health.observe(t0 + secs(90), true);
    assert!(health.is_silent(), "one good check does not bring it back");
    health.observe(t0 + secs(120), false);
    health.observe(t0 + secs(150), true);
    assert!(health.is_silent(), "the good checks must follow each other");
    health.observe(t0 + secs(180), true);
    assert!(
        !health.is_silent(),
        "two good checks in a row bring it back"
    );
}

#[test]
fn a_good_check_breaks_a_short_bad_run() {
    let t0 = Instant::now();
    let mut health = LocalHealth::new();
    health.observe(t0, false);
    health.observe(t0 + secs(30), false);
    health.observe(t0 + secs(60), true);
    health.observe(t0 + secs(90), false);
    health.observe(t0 + secs(120), false);
    assert!(!health.is_silent());
    assert_eq!(health.bad_since(), Some(t0 + secs(90)));
}

#[test]
fn verdict_is_good_silent_then_dead() {
    let t0 = Instant::now();
    let mut health = LocalHealth::new();
    assert_eq!(verdict(t0, &health), Health::Good);
    for i in 0..3 {
        health.observe(t0 + secs(30 * i), false);
    }
    assert_eq!(verdict(t0 + secs(60), &health), Health::Silent);
    assert_eq!(DEAD_AFTER, secs(600));
    assert_eq!(verdict(t0 + DEAD_AFTER - secs(1), &health), Health::Silent);
    assert_eq!(verdict(t0 + DEAD_AFTER, &health), Health::Dead);
    for i in 0..2 {
        health.observe(t0 + DEAD_AFTER + secs(30 * i), true);
    }
    assert_eq!(verdict(t0 + DEAD_AFTER + secs(60), &health), Health::Good);
    assert_eq!(health.bad_since(), None);
}

#[tokio::test]
async fn a_check_needs_the_engine_and_the_port() {
    let engine = FakeEngine::new();
    let ports = FakePorts::new(true);
    let also: Option<Ipv4Addr> = Some("192.0.2.1".parse().unwrap());

    assert!(check_once(&*engine, &*ports, Some(443), also).await.ok);
    assert_eq!(ports.asked.lock().unwrap().as_slice(), &[(443, also)]);

    ports.set(false);
    assert!(!check_once(&*engine, &*ports, Some(443), also).await.ok);

    ports.set(true);
    engine.set_health(false, Some("not serving"));
    let check = check_once(&*engine, &*ports, Some(443), also).await;
    assert!(!check.ok);
    assert_eq!(check.health.reason.as_deref(), Some("not serving"));

    // A warning is not a failure.
    engine.set_health(true, Some("user_telemetry_disabled"));
    assert!(check_once(&*engine, &*ports, Some(443), also).await.ok);

    // No known port is nothing to connect to.
    engine.set_health(true, None);
    assert!(!check_once(&*engine, &*ports, None, also).await.ok);
}

#[tokio::test]
async fn tcp_connect_sees_a_listening_port() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(LocalTcp.listening(port, None).await);
    drop(listener);
    assert!(!LocalTcp.listening(port, None).await);
}

/// The node's checks, driven by a flag: good while it is set. The verdict is the real one.
#[derive(Clone)]
struct Scripted {
    healthy: Arc<AtomicBool>,
    health: Arc<Mutex<LocalHealth>>,
    ip_changed: Arc<AtomicBool>,
    enrolled: Arc<Mutex<u32>>,
}

impl Scripted {
    fn new() -> Scripted {
        Scripted {
            healthy: Arc::new(AtomicBool::new(true)),
            health: Arc::new(Mutex::new(LocalHealth::new())),
            ip_changed: Arc::new(AtomicBool::new(false)),
            enrolled: Arc::new(Mutex::new(0)),
        }
    }
}

impl ReportSource for Scripted {
    async fn build(&self) -> Option<ReportRequest> {
        Some(sample_report())
    }
    async fn watch(&self, now: Instant) -> Watch {
        let mut health = self.health.lock().unwrap();
        health.observe(now, self.healthy.load(Ordering::SeqCst));
        Watch {
            ip_changed: self.ip_changed.swap(false, Ordering::SeqCst),
            health: verdict(now, &health),
        }
    }
    fn enrolled(&self) {
        *self.enrolled.lock().unwrap() += 1;
        *self.health.lock().unwrap() = LocalHealth::new();
    }
}

struct Run {
    h: Harness,
    source: Scripted,
    lifecycle: mtpool_agent::lifecycle::Lifecycle<
        mtpool_agent::lifecycle::StaticEngine,
        Scripted,
        RecordingSink,
    >,
}

async fn run(retire_status: u16) -> Run {
    use mtpool_agent::lifecycle::{DetectedEngine, Hooks, Lifecycle, StaticEngine};
    let h = Harness::new().await;
    h.enrolled();
    h.ok("POST", HEARTBEAT).await;
    h.ok("POST", REPORT).await;
    h.mock(
        "POST",
        RETIRE,
        ResponseTemplate::new(retire_status).set_body_json(serde_json::json!({})),
        false,
    )
    .await;
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(30, 3600)),
        false,
    )
    .await;
    h.mock(
        "POST",
        ENROLL,
        ResponseTemplate::new(200).set_body_json(enroll_response(NEW_NODE_TOKEN)),
        false,
    )
    .await;
    let source = Scripted::new();
    let config = local_config(&h.server.uri());
    let client = mtpool_agent::client::RegistrarClient::new(config.registrar.clone()).unwrap();
    let hooks = Hooks {
        engine: StaticEngine(Some(DetectedEngine {
            engine: mtpool_protocol::public::Engine::Telemt,
            port: 443,
        })),
        reports: source.clone(),
        sink: h.sink.clone(),
    };
    let lifecycle = Lifecycle::new(
        &config,
        client,
        h.fs.clone(),
        hooks,
        Box::new(FixedJitter(1.0)),
    )
    .unwrap();
    Run {
        h,
        source,
        lifecycle,
    }
}

#[tokio::test]
async fn silent_node_sends_no_heartbeat_and_no_report() {
    let mut r = run(200).await;
    let t0 = Instant::now();

    // Healthy: heartbeat and report go out.
    r.lifecycle.step(t0).await;
    assert_eq!(r.h.hits(HEARTBEAT).await, 1);
    assert_eq!(r.h.hits(REPORT).await, 1);

    // Three bad checks (one per heartbeat interval): the third makes the node silent.
    r.source.healthy.store(false, Ordering::SeqCst);
    r.lifecycle.step(t0 + secs(30)).await;
    r.lifecycle.step(t0 + secs(60)).await;
    assert_eq!(r.h.hits(HEARTBEAT).await, 3, "bad, but not yet silent");
    r.lifecycle.step(t0 + secs(90)).await;
    assert_eq!(r.h.hits(HEARTBEAT).await, 3, "silent: no heartbeat");
    r.lifecycle.step(t0 + secs(120)).await;
    r.lifecycle.step(t0 + secs(150)).await;
    assert_eq!(r.h.hits(HEARTBEAT).await, 3);
    let reports = r.h.hits(REPORT).await;
    r.lifecycle.step(t0 + secs(180)).await;
    assert_eq!(r.h.hits(REPORT).await, reports, "silent: no report");
    assert_eq!(r.lifecycle.phase(), AgentPhase::Active);

    // Two good checks and it speaks again.
    r.source.healthy.store(true, Ordering::SeqCst);
    r.lifecycle.step(t0 + secs(210)).await;
    assert_eq!(r.h.hits(HEARTBEAT).await, 3, "one good check is not enough");
    r.lifecycle.step(t0 + secs(240)).await;
    assert_eq!(r.h.hits(HEARTBEAT).await, 4);
}

#[tokio::test]
async fn dead_after_10min_calls_retire() {
    let mut r = run(200).await;
    let t0 = Instant::now();
    r.lifecycle.step(t0).await;
    r.source.healthy.store(false, Ordering::SeqCst);

    let mut now = t0;
    // The first bad check is at t0+30s: ten minutes of bad checks end 10 min after it.
    let dead_at = t0 + secs(30) + DEAD_AFTER;
    while now + secs(30) < dead_at {
        now += secs(30);
        r.lifecycle.step(now).await;
        assert_eq!(r.h.hits(RETIRE).await, 0, "not dead yet at {:?}", now - t0);
    }
    now += secs(30);
    assert_eq!(now, dead_at);
    r.lifecycle.step(now).await;
    assert_eq!(r.h.hits(RETIRE).await, 1, "dead: the node leaves the pool");
    assert_eq!(r.lifecycle.phase(), AgentPhase::Retired);
    let requests = r.h.total_requests().await;

    // A retired node says nothing, and retires once.
    for i in 1..=10 {
        r.lifecycle.step(now + secs(30 * i)).await;
    }
    assert_eq!(r.h.hits(RETIRE).await, 1);
    assert_eq!(r.h.hits(HEARTBEAT).await, 3, "no heartbeat while silent");
    assert_eq!(r.h.total_requests().await, requests);
    assert_eq!(r.lifecycle.phase(), AgentPhase::Retired);

    // The proxy is back: two good checks and the node enrolls again.
    r.source.healthy.store(true, Ordering::SeqCst);
    now += secs(30 * 11);
    r.lifecycle.step(now).await;
    assert_eq!(r.lifecycle.phase(), AgentPhase::Retired);
    now += secs(30);
    r.lifecycle.step(now).await;
    assert_eq!(r.lifecycle.phase(), AgentPhase::Enrolling);
    assert_eq!(r.h.get("node_token"), None);
    assert_eq!(r.lifecycle.step(now).await, Step::Wait(Duration::ZERO));
    assert_eq!(r.lifecycle.phase(), AgentPhase::Active);
    assert_eq!(r.h.hits(ENROLL).await, 1);
    assert_eq!(r.h.get("node_token").as_deref(), Some(NEW_NODE_TOKEN));
    assert_eq!(*r.source.enrolled.lock().unwrap(), 1);
}

#[tokio::test]
async fn retire_that_fails_is_tried_again() {
    // A registrar that is down when the node tries to leave.
    let r = run(503).await;
    let mut lifecycle = r.lifecycle;
    let t0 = Instant::now();
    lifecycle.step(t0).await;
    r.source.healthy.store(false, Ordering::SeqCst);
    let mut now = t0;
    for _ in 0..25 {
        now += secs(30);
        lifecycle.step(now).await;
    }
    assert_eq!(
        lifecycle.phase(),
        AgentPhase::Active,
        "not retired while the call fails"
    );
    assert!(r.h.hits(RETIRE).await >= 2);
}

#[tokio::test]
async fn ip_change_triggers_reenroll() {
    let mut r = run(200).await;
    let t0 = Instant::now();
    r.lifecycle.step(t0).await;
    assert_eq!(*r.source.enrolled.lock().unwrap(), 0);

    r.source.ip_changed.store(true, Ordering::SeqCst);
    assert_eq!(
        r.lifecycle.step(t0 + secs(30)).await,
        Step::Wait(Duration::ZERO)
    );
    assert_eq!(r.lifecycle.phase(), AgentPhase::Enrolling);
    assert_eq!(r.h.get("node_id"), None);
    assert_eq!(r.h.get("node_token"), None);
    assert_eq!(r.h.get("enroll_token").as_deref(), Some(ENROLL_TOKEN));
    assert_eq!(
        r.h.hits(HEARTBEAT).await,
        1,
        "no heartbeat on the old address"
    );

    r.lifecycle.step(t0 + secs(30)).await;
    assert_eq!(r.lifecycle.phase(), AgentPhase::Active);
    assert_eq!(r.h.hits(ENROLL).await, 1);
    assert_eq!(r.h.get("node_token").as_deref(), Some(NEW_NODE_TOKEN));
    assert_eq!(*r.source.enrolled.lock().unwrap(), 1);
}
