//! The lifecycle of the agent against a registrar double. Time is passed to `step`, so none of
//! these tests sleeps.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use mtpool_agent::client::RegistrarClient;
use mtpool_agent::lifecycle::{
    AgentPhase, Backoff, DetectedEngine, Hooks, Lifecycle, NoReports, RngJitter, StaticEngine,
    Step, StopReason, TERMINATED_RETRY,
};
use mtpool_agent::state_files::{Fs, RealFs};
use mtpool_protocol::public::Engine;
use rand::SeedableRng;
use rand::rngs::StdRng;
use serde_json::json;
use tokio::time::Instant;
use wiremock::matchers::{body_json, header};
use wiremock::{Mock, ResponseTemplate};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

#[tokio::test]
async fn enroll_saves_id_and_token_0600() {
    let server = wiremock::MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("state");
    let enroll_file = state_dir.join("enroll_token");
    RealFs
        .write_private(&enroll_file, ENROLL_TOKEN.as_bytes())
        .unwrap();

    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(ENROLL))
        .and(header(
            "authorization",
            format!("Bearer {ENROLL_TOKEN}").as_str(),
        ))
        .and(body_json(
            json!({ "name": "alpha", "port": 443, "engine": "telemt" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(enroll_response(NODE_TOKEN)))
        .expect(1)
        .mount(&server)
        .await;

    let text = format!(
        "registrar = \"{}\"\nname = \"alpha\"\nstate_dir = \"{}\"\nport = 443\n",
        server.uri(),
        state_dir.display()
    );
    let config = mtpool_agent::config::LocalConfig::parse(&text, Path::new("agent.toml")).unwrap();
    let client = RegistrarClient::new(config.registrar.clone()).unwrap();
    let hooks = Hooks {
        engine: StaticEngine(Some(DetectedEngine {
            engine: Engine::Telemt,
            port: 443,
        })),
        reports: NoReports,
        sink: RecordingSink::default(),
    };
    let mut lifecycle = Lifecycle::new(
        &config,
        client,
        Arc::new(RealFs),
        hooks,
        Box::new(FixedJitter(1.0)),
    )
    .unwrap();
    assert_eq!(lifecycle.phase(), AgentPhase::Enrolling);

    assert_eq!(
        lifecycle.step(Instant::now()).await,
        Step::Wait(Duration::ZERO)
    );
    assert_eq!(lifecycle.phase(), AgentPhase::Active);

    let id_file = state_dir.join("node_id");
    let token_file = state_dir.join("node_token");
    assert_eq!(std::fs::read_to_string(&id_file).unwrap(), "alpha-a1b2c");
    assert_eq!(std::fs::read_to_string(&token_file).unwrap(), NODE_TOKEN);
    for file in [&id_file, &token_file, &enroll_file] {
        let mode = std::fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", file.display());
    }
    let names: Vec<_> = std::fs::read_dir(&state_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
}

#[tokio::test]
async fn http_410_reenrolls() {
    let h = Harness::new().await;
    h.enrolled();
    h.mock(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(410).set_body_json(problem_body(410, "unknown_node")),
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
    let mut lifecycle = h.lifecycle();
    assert_eq!(lifecycle.phase(), AgentPhase::Active);

    let t0 = Instant::now();
    assert_eq!(lifecycle.step(t0).await, Step::Wait(Duration::ZERO));
    assert_eq!(lifecycle.phase(), AgentPhase::Enrolling);
    assert_eq!(h.get("node_id"), None);
    assert_eq!(h.get("node_token"), None);
    assert_eq!(h.get("enroll_token").as_deref(), Some(ENROLL_TOKEN));

    assert_eq!(lifecycle.step(t0).await, Step::Wait(Duration::ZERO));
    assert_eq!(lifecycle.phase(), AgentPhase::Active);
    assert_eq!(h.hits(ENROLL).await, 1);
    assert_eq!(h.get("node_token").as_deref(), Some(NEW_NODE_TOKEN));
    assert_eq!(h.get("node_id").as_deref(), Some("alpha-a1b2c"));
}

#[tokio::test]
async fn http_401_removes_node_token_and_reenrolls() {
    let h = Harness::new().await;
    h.enrolled();
    h.mock(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(401).set_body_json(problem_body(401, "unauthorized")),
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
    let mut lifecycle = h.lifecycle();

    let t0 = Instant::now();
    lifecycle.step(t0).await;
    assert_eq!(lifecycle.phase(), AgentPhase::Enrolling);
    assert_eq!(h.get("node_token"), None);
    assert_eq!(h.hits(ENROLL).await, 0);

    lifecycle.step(t0).await;
    assert_eq!(lifecycle.phase(), AgentPhase::Active);
    assert_eq!(h.get("node_token").as_deref(), Some(NEW_NODE_TOKEN));
    // The enroll call carried the enroll token, not the refused node token.
    let requests = h.server.received_requests().await.unwrap();
    let enroll = requests.iter().find(|r| r.url.path() == ENROLL).unwrap();
    assert_eq!(
        enroll
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        format!("Bearer {ENROLL_TOKEN}")
    );
}

#[tokio::test]
async fn revoked_enroll_token_stops_with_clear_error() {
    let h = Harness::new().await;
    h.fresh();
    h.mock(
        "POST",
        ENROLL,
        ResponseTemplate::new(401).set_body_json(problem_body(401, "unauthorized")),
        false,
    )
    .await;
    let mut lifecycle = h.lifecycle();

    let step = lifecycle.step(Instant::now()).await;
    let expected = StopReason::EnrollTokenRevoked {
        path: Path::new(STATE_DIR).join("enroll_token"),
    };
    assert_eq!(step, Step::Stop(expected.clone()));
    let message = expected.to_string();
    assert!(message.contains("/state/enroll_token"), "{message}");
    assert!(message.contains("revoked"), "{message}");
    assert!(!message.contains(ENROLL_TOKEN), "{message}");
    assert_eq!(expected.exit_code(), 2);
    assert_eq!(h.hits(ENROLL).await, 1);

    // `run` ends with the same reason instead of retrying.
    let result = h.lifecycle().run(std::future::pending()).await;
    assert_eq!(result, Err(expected));
}

#[tokio::test]
async fn missing_enroll_token_stops_without_calling_the_registrar() {
    let h = Harness::new().await;
    let mut lifecycle = h.lifecycle();
    let step = lifecycle.step(Instant::now()).await;
    assert_eq!(
        step,
        Step::Stop(StopReason::EnrollTokenMissing {
            path: Path::new(STATE_DIR).join("enroll_token")
        })
    );
    assert_eq!(h.total_requests().await, 0);
}

#[tokio::test]
async fn http_403_terminate_stops_reports_leaves_engine_retries_15min() {
    let h = Harness::new().await;
    h.enrolled();
    h.mock(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(200).set_body_json(json!({})),
        true,
    )
    .await;
    h.mock(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(403).set_body_json(terminate_body()),
        false,
    )
    .await;
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(15, 60)),
        false,
    )
    .await;
    h.ok("POST", REPORT).await;
    // Each enroll attempt of a terminated node is refused again.
    h.mock(
        "POST",
        ENROLL,
        ResponseTemplate::new(403).set_body_json(terminate_body()),
        false,
    )
    .await;
    let mut lifecycle = h.lifecycle();

    // A normal first round: heartbeat, config, report.
    let t0 = Instant::now();
    assert_eq!(lifecycle.step(t0).await, Step::Wait(secs(15)));
    assert_eq!(
        (
            h.hits(HEARTBEAT).await,
            h.hits(CONFIG).await,
            h.hits(REPORT).await
        ),
        (1, 1, 1)
    );
    assert_eq!(h.sink.applied().len(), 1);

    // The next heartbeat is refused with the terminate document.
    let t1 = t0 + secs(15);
    assert_eq!(lifecycle.step(t1).await, Step::Wait(TERMINATED_RETRY));
    assert_eq!(lifecycle.phase(), AgentPhase::Terminated);
    let sent = h.total_requests().await;

    // Before 15 minutes pass nothing is sent: no heartbeat, no report, no sync.
    assert_eq!(
        lifecycle.step(t1 + secs(14 * 60)).await,
        Step::Wait(secs(60))
    );
    assert_eq!(h.total_requests().await, sent);

    // At 15 minutes the agent tries to enroll, and is refused again.
    assert_eq!(
        lifecycle.step(t1 + TERMINATED_RETRY).await,
        Step::Wait(TERMINATED_RETRY)
    );
    assert_eq!(lifecycle.phase(), AgentPhase::Terminated);
    assert_eq!(h.hits(ENROLL).await, 1);
    assert_eq!(
        (
            h.hits(HEARTBEAT).await,
            h.hits(CONFIG).await,
            h.hits(REPORT).await
        ),
        (2, 1, 1)
    );

    // The engine was left alone: no config was applied after the first round, no report built.
    assert_eq!(h.sink.applied().len(), 1);
    assert_eq!(h.reports.built(), 1);
}

#[tokio::test]
async fn terminated_node_that_enrolls_again_is_active() {
    let h = Harness::new().await;
    h.enrolled();
    h.mock(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(403).set_body_json(terminate_body()),
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
    let mut lifecycle = h.lifecycle();
    let t0 = Instant::now();
    lifecycle.step(t0).await;
    assert_eq!(lifecycle.phase(), AgentPhase::Terminated);
    assert_eq!(
        lifecycle.step(t0 + TERMINATED_RETRY).await,
        Step::Wait(Duration::ZERO)
    );
    assert_eq!(lifecycle.phase(), AgentPhase::Active);
    assert_eq!(h.get("node_token").as_deref(), Some(NEW_NODE_TOKEN));
}

#[tokio::test]
async fn http_429_silent_until_retry_after() {
    let h = Harness::new().await;
    h.enrolled();
    h.mock(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(429)
            .insert_header("Retry-After", "120")
            .set_body_json(problem_body(429, "rate_limited")),
        true,
    )
    .await;
    h.ok("POST", HEARTBEAT).await;
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(15, 60)),
        false,
    )
    .await;
    h.ok("POST", REPORT).await;
    let mut lifecycle = h.lifecycle();

    let t0 = Instant::now();
    assert_eq!(lifecycle.step(t0).await, Step::Wait(secs(120)));
    assert_eq!(lifecycle.phase(), AgentPhase::Silent);

    // Silent: nothing is sent while the time has not passed.
    assert_eq!(lifecycle.step(t0 + secs(60)).await, Step::Wait(secs(60)));
    assert_eq!(lifecycle.phase(), AgentPhase::Silent);
    assert_eq!(h.total_requests().await, 1);

    // After it, back to the previous phase, which now does its work.
    lifecycle.step(t0 + secs(120)).await;
    assert_eq!(lifecycle.phase(), AgentPhase::Active);
    assert_eq!(h.hits(HEARTBEAT).await, 2);
    assert_eq!(h.hits(CONFIG).await, 1);
}

#[tokio::test]
async fn http_429_while_enrolling_returns_to_enrolling() {
    let h = Harness::new().await;
    h.fresh();
    h.mock(
        "POST",
        ENROLL,
        ResponseTemplate::new(429)
            .insert_header("Retry-After", "30")
            .set_body_json(problem_body(429, "enroll_paused")),
        true,
    )
    .await;
    h.mock(
        "POST",
        ENROLL,
        ResponseTemplate::new(200).set_body_json(enroll_response(NODE_TOKEN)),
        false,
    )
    .await;
    let mut lifecycle = h.lifecycle();
    let t0 = Instant::now();
    assert_eq!(lifecycle.step(t0).await, Step::Wait(secs(30)));
    assert_eq!(lifecycle.phase(), AgentPhase::Silent);
    lifecycle.step(t0 + secs(30)).await;
    assert_eq!(lifecycle.phase(), AgentPhase::Active);
    assert_eq!(h.hits(ENROLL).await, 2);
}

#[tokio::test]
async fn http_429_without_retry_after_waits_a_minute() {
    let h = Harness::new().await;
    h.enrolled();
    h.mock(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(429).set_body_json(problem_body(429, "rate_limited")),
        false,
    )
    .await;
    let mut lifecycle = h.lifecycle();
    assert_eq!(lifecycle.step(Instant::now()).await, Step::Wait(secs(60)));
}

#[test]
fn backoff_5s_doubling_cap_5min_jitter_50pct() {
    // Without jitter: 5, 10, 20, 40, 80, 160, then the 300 s cap for good.
    let mut backoff = Backoff::new();
    let mut fixed = FixedJitter(1.0);
    let plain: Vec<u64> = (0..9)
        .map(|_| backoff.next_delay(&mut fixed).as_secs())
        .collect();
    assert_eq!(plain, [5, 10, 20, 40, 80, 160, 300, 300, 300]);
    backoff.reset();
    assert_eq!(backoff.next_delay(&mut fixed), secs(5));

    // With jitter every delay is the base times a factor in [0.5, 1.5], and the factor really
    // varies (a seeded generator keeps the test repeatable).
    let mut jitter = RngJitter(StdRng::seed_from_u64(0x6d74_706f_6f6c));
    let (mut lowest, mut highest) = (f64::MAX, f64::MIN);
    for attempt in 0..12 {
        let mut backoff = Backoff::new();
        for _ in 0..attempt {
            backoff.next_delay(&mut FixedJitter(1.0));
        }
        let base = backoff.base_delay().as_secs_f64();
        assert!(base <= 300.0, "{base}");
        for _ in 0..200 {
            let mut one = backoff.clone();
            let delay = one.next_delay(&mut jitter).as_secs_f64();
            assert!(
                delay >= base * 0.5 - 1e-9 && delay <= base * 1.5 + 1e-9,
                "{delay} for base {base}"
            );
            let factor = delay / base;
            lowest = lowest.min(factor);
            highest = highest.max(factor);
        }
    }
    assert!(
        lowest < 0.6 && highest > 1.4,
        "jitter spans [{lowest}, {highest}]"
    );
    // A misbehaving source cannot push the delay out of the bounds.
    assert_eq!(
        Backoff::new().next_delay(&mut FixedJitter(9.0)),
        secs(7) + Duration::from_millis(500)
    );
    assert_eq!(
        Backoff::new().next_delay(&mut FixedJitter(0.0)),
        Duration::from_millis(2500)
    );
}

#[tokio::test]
async fn enroll_failures_back_off() {
    let h = Harness::new().await;
    h.fresh();
    h.mock("POST", ENROLL, ResponseTemplate::new(503), false)
        .await;
    let mut lifecycle = h.lifecycle();
    let t0 = Instant::now();
    assert_eq!(lifecycle.step(t0).await, Step::Wait(secs(5)));
    assert_eq!(lifecycle.phase(), AgentPhase::Enrolling);
    // Called early, the lifecycle does not knock again.
    assert_eq!(lifecycle.step(t0 + secs(2)).await, Step::Wait(secs(3)));
    assert_eq!(h.hits(ENROLL).await, 1);
    assert_eq!(lifecycle.step(t0 + secs(5)).await, Step::Wait(secs(10)));
    assert_eq!(h.hits(ENROLL).await, 2);
}

#[tokio::test]
async fn enroll_waits_for_an_engine() {
    let h = Harness::new().await;
    h.fresh();
    let config = local_config(&h.server.uri());
    let client = RegistrarClient::new(config.registrar.clone()).unwrap();
    let hooks = Hooks {
        engine: StaticEngine(None),
        reports: NoReports,
        sink: RecordingSink::default(),
    };
    let mut lifecycle = Lifecycle::new(
        &config,
        client,
        h.fs.clone(),
        hooks,
        Box::new(FixedJitter(1.0)),
    )
    .unwrap();
    assert_eq!(lifecycle.step(Instant::now()).await, Step::Wait(secs(5)));
    assert_eq!(h.total_requests().await, 0);
}

#[tokio::test]
async fn node_route_failures_back_off_per_job() {
    let h = Harness::new().await;
    h.enrolled();
    h.mock("POST", HEARTBEAT, ResponseTemplate::new(500), false)
        .await;
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(15, 60)),
        false,
    )
    .await;
    h.ok("POST", REPORT).await;
    let mut lifecycle = h.lifecycle();
    let t0 = Instant::now();
    // The heartbeat failed and retries in 5 s; the config and the report went on as usual.
    assert_eq!(lifecycle.step(t0).await, Step::Wait(secs(5)));
    assert_eq!(lifecycle.phase(), AgentPhase::Active);
    assert_eq!(h.sink.applied().len(), 1);
    // The second failure doubles the delay.
    assert_eq!(lifecycle.step(t0 + secs(5)).await, Step::Wait(secs(10)));
}

#[tokio::test]
async fn a_refused_report_does_not_back_off() {
    let h = Harness::new().await;
    h.enrolled();
    h.ok("POST", HEARTBEAT).await;
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(15, 60)),
        false,
    )
    .await;
    h.mock(
        "POST",
        REPORT,
        ResponseTemplate::new(409).set_body_json(problem_body(409, "clock_skew")),
        false,
    )
    .await;
    let mut lifecycle = h.lifecycle();
    let t0 = Instant::now();
    assert_eq!(lifecycle.step(t0).await, Step::Wait(secs(15)));
    assert_eq!(lifecycle.step(t0 + secs(15)).await, Step::Wait(secs(15)));
    // The report is due at 60 s, as scheduled, not retried earlier.
    assert_eq!(h.hits(REPORT).await, 1);
    lifecycle.step(t0 + secs(60)).await;
    assert_eq!(h.hits(REPORT).await, 2);
}

#[tokio::test]
async fn intervals_updated_from_config() {
    let h = Harness::new().await;
    h.enrolled();
    h.ok("POST", HEARTBEAT).await;
    h.ok("POST", REPORT).await;
    let mut config = agent_config(5, 20);
    config.intervals.metrics_sec = 30;
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(&config),
        false,
    )
    .await;
    let mut lifecycle = h.lifecycle();
    assert_eq!(lifecycle.intervals().heartbeat_sec, 15);

    let t0 = Instant::now();
    // Round one starts on the defaults (heartbeat 15 s); the config it fetches is in force from
    // then on, so the sync is next due in 20 s and the report in 30 s.
    assert_eq!(lifecycle.step(t0).await, Step::Wait(secs(15)));
    assert_eq!(lifecycle.intervals(), config.intervals);
    assert_eq!(h.sink.applied(), vec![config.clone()]);

    // The new heartbeat interval applies from the next heartbeat on.
    assert_eq!(lifecycle.step(t0 + secs(15)).await, Step::Wait(secs(5)));
    assert_eq!((h.hits(CONFIG).await, h.hits(REPORT).await), (1, 1));
    assert_eq!(lifecycle.step(t0 + secs(20)).await, Step::Wait(secs(5)));
    assert_eq!(h.hits(HEARTBEAT).await, 3);
    assert_eq!((h.hits(CONFIG).await, h.hits(REPORT).await), (2, 1));

    // The sync then repeats every 20 s, the report every 30 s.
    lifecycle.step(t0 + secs(60)).await;
    assert_eq!((h.hits(CONFIG).await, h.hits(REPORT).await), (3, 2));
    let step = lifecycle.step(t0 + secs(80)).await;
    assert_eq!(h.hits(CONFIG).await, 4);
    assert_eq!(step, Step::Wait(secs(5)));
    lifecycle.step(t0 + secs(90)).await;
    assert_eq!(h.hits(REPORT).await, 3);
}

#[tokio::test]
async fn a_finished_measurement_goes_out_with_the_next_heartbeat() {
    let h = Harness::new().await;
    h.enrolled();
    h.ok("POST", HEARTBEAT).await;
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(15, 60)),
        false,
    )
    .await;
    h.ok("POST", REPORT).await;
    let mut lifecycle = h.lifecycle();
    let t0 = Instant::now();
    lifecycle.step(t0).await;
    lifecycle.step(t0 + secs(15)).await;
    assert_eq!(
        h.reports.built(),
        1,
        "nothing new: the report keeps its minute"
    );

    h.reports.finish_measurement();
    lifecycle.step(t0 + secs(30)).await;
    assert_eq!(h.reports.built(), 2, "the pool hears about it at once");

    // The minute starts over from that report.
    lifecycle.step(t0 + secs(75)).await;
    assert_eq!(h.reports.built(), 2);
    lifecycle.step(t0 + secs(90)).await;
    assert_eq!(h.reports.built(), 3);
}

#[tokio::test]
async fn a_finished_measurement_waits_while_the_proxy_is_sick() {
    let h = Harness::new().await;
    h.enrolled();
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(15, 60)),
        false,
    )
    .await;
    h.reports.make_sick();
    let mut lifecycle = h.lifecycle();
    let t0 = Instant::now();
    lifecycle.step(t0).await;
    h.reports.finish_measurement();
    lifecycle.step(t0 + secs(15)).await;
    assert_eq!(
        h.reports.built(),
        0,
        "nothing is said while the proxy is sick"
    );
}

#[tokio::test]
async fn the_report_goes_out_with_the_node_token() {
    let h = Harness::new().await;
    h.enrolled();
    h.ok("POST", HEARTBEAT).await;
    h.mock(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(15, 60)),
        false,
    )
    .await;
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(REPORT))
        .and(header(
            "authorization",
            format!("Bearer {NODE_TOKEN}").as_str(),
        ))
        .and(body_json(serde_json::to_value(sample_report()).unwrap()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&h.server)
        .await;
    let mut lifecycle = h.lifecycle();
    lifecycle.step(Instant::now()).await;
    assert_eq!(h.reports.built(), 1);
}

#[tokio::test]
async fn a_node_id_without_a_token_enrolls() {
    let h = Harness::new().await;
    h.put("node_id", "alpha-a1b2c");
    h.put("enroll_token", ENROLL_TOKEN);
    assert_eq!(h.lifecycle().phase(), AgentPhase::Enrolling);
    let h = Harness::new().await;
    h.enrolled();
    assert_eq!(h.lifecycle().phase(), AgentPhase::Active);
}

#[tokio::test]
async fn shutdown_ends_run_without_a_request() {
    let h = Harness::new().await;
    h.enrolled();
    assert_eq!(h.lifecycle().run(async {}).await, Ok(()));
    assert_eq!(h.total_requests().await, 0);
}
