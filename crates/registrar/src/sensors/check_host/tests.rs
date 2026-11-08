//! The check-host sensor against recorded answers.
//!
//! `connected.json`, `timed_out.json`, `pending.json` and `start.json` in
//! `tests/fixtures/check-host` are real answers of the service, recorded on 2026-11-08 for the
//! public address 1.1.1.1 (port 443 connected; port 81 timed out on both points; `pending.json`
//! is the first read of the port 81 check, while both points were still working). The two
//! points named in `start.json` are the service's own. Nothing here names a server of the pool.
//!
//! No real `Connection refused` and no real mixed answer could be recorded: the public
//! addresses tried within the service's limit all connected or timed out. Both are therefore
//! assembled from values of the recorded answers, and the test says so where it does.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use mtpool_core::SensorKind;
use serde_json::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const CONNECTED: &str = include_str!("../../../tests/fixtures/check-host/connected.json");
const TIMED_OUT: &str = include_str!("../../../tests/fixtures/check-host/timed_out.json");
const PENDING: &str = include_str!("../../../tests/fixtures/check-host/pending.json");
const START: &str = include_str!("../../../tests/fixtures/check-host/start.json");

const PUBLIC: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);
/// `created` of `connected.json`, in milliseconds.
const CONNECTED_AT: i64 = 1_794_143_356_000;
/// `created` of `timed_out.json` and `pending.json`, in milliseconds.
const TIMED_OUT_AT: i64 = 1_794_143_374_000;

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("a fixture is JSON")
}

fn expect(port: u16, created_ms: i64) -> CheckExpect {
    CheckExpect {
        ip: PUBLIC,
        port,
        reported_at: Ts(created_ms + 5_000),
    }
}

fn connected_expect() -> CheckExpect {
    expect(443, CONNECTED_AT)
}

fn timed_out_expect() -> CheckExpect {
    expect(81, TIMED_OUT_AT)
}

fn parsed(text: &str, expect: &CheckExpect) -> Result<CheckResult, CheckRejected> {
    parse_result(&json(text), expect)
}

#[test]
fn both_connected_is_clear() {
    let result = parsed(CONNECTED, &connected_expect()).expect("the answer is valid");
    assert_eq!(result.vote(), Vote::Clear);
    assert_eq!(result.port, 443);
    assert_eq!(result.created, Ts(CONNECTED_AT));
    assert_eq!(
        result.points,
        [
            PointResult::Connected { ms: 2 },
            PointResult::Connected { ms: 32 }
        ]
    );
}

#[test]
fn both_timed_out_is_blocked() {
    let result = parsed(TIMED_OUT, &timed_out_expect()).expect("the answer is valid");
    assert_eq!(result.vote(), Vote::Blocked);
    assert_eq!(result.port, 81);
}

/// Built from the recorded `timed_out.json` with the error text of the second point replaced by
/// the one the specification names for a refused connection: it was not recorded.
fn refused() -> Value {
    let mut body = json(TIMED_OUT);
    body["results"]["ru2.node.check-host.net"][0]["error"] = json!("Connection refused");
    body
}

#[test]
fn a_refused_connection_abstains() {
    let result = parse_result(&refused(), &timed_out_expect()).expect("the answer is valid");
    assert_eq!(result.points[1], PointResult::Refused);
    assert_eq!(result.vote(), Vote::Abstain(AbstainReason::UnknownAnswer));

    let mut both = refused();
    both["results"]["ru1.node.check-host.net"][0]["error"] = json!("Connection refused");
    let result = parse_result(&both, &timed_out_expect()).expect("the answer is valid");
    assert_eq!(result.vote(), Vote::Abstain(AbstainReason::UnknownAnswer));

    let mut strange = json(TIMED_OUT);
    strange["results"]["ru1.node.check-host.net"][0]["error"] = json!("No route to host");
    let result = parse_result(&strange, &timed_out_expect()).expect("the answer is valid");
    assert_eq!(result.points[0], PointResult::Other);
    assert_eq!(result.vote(), Vote::Abstain(AbstainReason::UnknownAnswer));
}

#[test]
fn mixed_abstains() {
    // The first point from `connected.json`, the second from `timed_out.json`. No such answer
    // was recorded.
    let mut body = json(TIMED_OUT);
    body["results"]["ru1.node.check-host.net"] =
        json(CONNECTED)["results"]["ru1.node.check-host.net"].clone();
    let result = parse_result(&body, &timed_out_expect()).expect("the answer is valid");
    assert_eq!(
        result.points,
        [PointResult::Connected { ms: 2 }, PointResult::TimedOut]
    );
    assert_eq!(result.vote(), Vote::Abstain(AbstainReason::UnknownAnswer));
}

#[test]
fn a_null_point_is_pending() {
    assert_eq!(
        parsed(PENDING, &timed_out_expect()),
        Err(CheckRejected::Pending)
    );
    let mut one = json(TIMED_OUT);
    one["results"]["ru2.node.check-host.net"] = Value::Null;
    assert_eq!(
        parse_result(&one, &timed_out_expect()),
        Err(CheckRejected::Pending)
    );
}

#[test]
fn another_address_or_port_is_refused() {
    let mut other_ip = connected_expect();
    other_ip.ip = Ipv4Addr::new(1, 0, 0, 1);
    assert_eq!(parsed(CONNECTED, &other_ip), Err(CheckRejected::Mismatch));

    let mut other_port = connected_expect();
    other_port.port = 22;
    assert_eq!(parsed(CONNECTED, &other_port), Err(CheckRejected::Mismatch));
}

#[test]
fn another_command_is_refused() {
    let mut body = json(CONNECTED);
    body["command"] = json!("ping");
    assert_eq!(
        parse_result(&body, &connected_expect()),
        Err(CheckRejected::Mismatch)
    );
}

#[test]
fn a_missing_point_is_unknown() {
    let mut body = json(CONNECTED);
    body["results"]
        .as_object_mut()
        .expect("results is an object")
        .remove("ru2.node.check-host.net");
    assert_eq!(
        parse_result(&body, &connected_expect()),
        Err(CheckRejected::Unknown)
    );
}

#[test]
fn a_port_as_a_number_or_html_does_not_panic() {
    let mut number = json(CONNECTED);
    number["port"] = json!(443);
    assert_eq!(
        parse_result(&number, &connected_expect()),
        Err(CheckRejected::Unknown)
    );
    for body in [
        json!("<html><body>502 Bad Gateway</body></html>"),
        json!(null),
        json!([]),
        json!({}),
        json!({"command": "tcp"}),
        json!({"command": "tcp", "host": "1.1.1.1", "port": "443", "results": "none"}),
        json!({"command": "tcp", "host": "1.1.1.1", "port": "443", "created": "x", "results": {}}),
        json!({"error": "limit_exceeded"}),
    ] {
        assert_eq!(
            parse_result(&body, &connected_expect()),
            Err(CheckRejected::Unknown),
            "{body}"
        );
    }
    let mut odd_point = json(CONNECTED);
    odd_point["results"]["ru1.node.check-host.net"] = json!("down");
    assert_eq!(
        parse_result(&odd_point, &connected_expect()),
        Err(CheckRejected::Unknown)
    );
}

#[test]
fn outside_the_window_is_stale() {
    // Created 11 minutes before the report, and 2 minutes after it.
    let early = CheckExpect {
        reported_at: Ts(CONNECTED_AT + 11 * 60_000),
        ..connected_expect()
    };
    assert_eq!(parsed(CONNECTED, &early), Err(CheckRejected::Stale));
    let late = CheckExpect {
        reported_at: Ts(CONNECTED_AT - 2 * 60_000),
        ..connected_expect()
    };
    assert_eq!(parsed(CONNECTED, &late), Err(CheckRejected::Stale));

    let edge_early = CheckExpect {
        reported_at: Ts(CONNECTED_AT + 10 * 60_000),
        ..connected_expect()
    };
    assert!(parsed(CONNECTED, &edge_early).is_ok());
    let edge_late = CheckExpect {
        reported_at: Ts(CONNECTED_AT - 60_000),
        ..connected_expect()
    };
    assert!(parsed(CONNECTED, &edge_late).is_ok());
}

#[test]
fn evidence_carries_no_address() {
    let result = parsed(CONNECTED, &connected_expect()).expect("the answer is valid");
    assert_eq!(
        result.evidence(),
        json!({"points": {
            "ru1": {"result": "connected", "ms": 2},
            "ru2": {"result": "connected", "ms": 32},
        }})
    );

    let mut body = json(TIMED_OUT);
    body["results"]["ru2.node.check-host.net"][0]["error"] = json!("Connection refused");
    let result = parse_result(&body, &timed_out_expect()).expect("the answer is valid");
    assert_eq!(
        result.evidence(),
        json!({"points": {"ru1": {"result": "timeout"}, "ru2": {"result": "refused"}}})
    );

    for text in [CONNECTED, TIMED_OUT] {
        let expect = if text == CONNECTED {
            connected_expect()
        } else {
            timed_out_expect()
        };
        let shown = parsed(text, &expect)
            .expect("the answer is valid")
            .evidence()
            .to_string();
        assert!(!shown.contains("1.1.1.1"), "{shown}");
        assert!(!shown.contains("timed out"), "{shown}");
    }
    assert!(START.contains("request_id"));
}

// --- the verifier -------------------------------------------------------------------------

/// Answers one after another; the last one repeats.
fn scripted(
    answers: Vec<&'static str>,
) -> (
    impl FnMut() -> std::future::Ready<Option<Value>>,
    Arc<AtomicUsize>,
) {
    let reads = Arc::new(AtomicUsize::new(0));
    let counter = reads.clone();
    let mut queue: VecDeque<&'static str> = answers.into();
    let fetch = move || {
        counter.fetch_add(1, Ordering::SeqCst);
        let text = if queue.len() > 1 {
            queue.pop_front()
        } else {
            queue.front().copied()
        };
        std::future::ready(text.map(json))
    };
    (fetch, reads)
}

#[tokio::test(start_paused = true)]
async fn verify_reads_a_null_point_again_for_30_seconds() {
    let (fetch, reads) = scripted(vec![PENDING, PENDING, TIMED_OUT]);
    let result = read_until_done(fetch, &timed_out_expect(), VERIFY_WINDOW)
        .await
        .expect("the third read is complete");
    assert_eq!(result.vote(), Vote::Blocked);
    assert_eq!(reads.load(Ordering::SeqCst), 3);

    let (fetch, reads) = scripted(vec![PENDING]);
    let started = Instant::now();
    let verdict = read_until_done(fetch, &timed_out_expect(), VERIFY_WINDOW).await;
    assert_eq!(verdict, Err(CheckRejected::Pending));
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(30) && waited < Duration::from_secs(33),
        "{waited:?}"
    );
    assert!(reads.load(Ordering::SeqCst) >= 15);
}

#[tokio::test(start_paused = true)]
async fn a_mismatch_is_not_read_again() {
    let (fetch, reads) = scripted(vec![CONNECTED]);
    let verdict = read_until_done(fetch, &timed_out_expect(), VERIFY_WINDOW).await;
    assert_eq!(verdict, Err(CheckRejected::Mismatch));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_answer_ends_unknown_after_the_window() {
    let fetch = || std::future::ready(None);
    let verdict = read_until_done(fetch, &timed_out_expect(), VERIFY_WINDOW).await;
    assert_eq!(verdict, Err(CheckRejected::Unknown));
}

#[tokio::test]
async fn verify_asks_for_the_id_with_the_headers_of_the_service() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/check-result-extended/56626c28k235"))
        .and(header("accept", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CONNECTED))
        .expect(1)
        .mount(&server)
        .await;
    let verifier =
        CheckHostVerifier::new(Url::parse(&server.uri()).expect("a URL")).expect("a client");
    let result = verifier
        .verify("56626c28k235", &connected_expect())
        .await
        .expect("the answer is valid");
    assert_eq!(result.vote(), Vote::Clear);
    let seen = server
        .received_requests()
        .await
        .expect("requests are recorded");
    let agent = seen[0]
        .headers
        .get("user-agent")
        .expect("a user agent")
        .to_str()
        .expect("text");
    assert!(agent.starts_with("mtpool-registrar/"), "{agent}");
}

#[tokio::test]
async fn verify_refuses_an_id_that_is_not_an_id() {
    let server = MockServer::start().await;
    let verifier =
        CheckHostVerifier::new(Url::parse(&server.uri()).expect("a URL")).expect("a client");
    for id in ["", "../x", "a/b", "a?b=c", &"a".repeat(65)] {
        assert_eq!(
            verifier.verify(id, &connected_expect()).await,
            Err(CheckRejected::Unknown),
            "{id}"
        );
    }
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty()
    );
}

#[tokio::test]
async fn verify_does_not_follow_a_redirect_or_read_an_http_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/check-result-extended/redirected"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "http://127.0.0.1:9/x"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/check-result-extended/forbidden"))
        .respond_with(ResponseTemplate::new(403).set_body_string(CONNECTED))
        .mount(&server)
        .await;
    let verifier =
        CheckHostVerifier::new(Url::parse(&server.uri()).expect("a URL")).expect("a client");
    // One read is enough to see that no body is taken from a redirect or an error status.
    assert_eq!(verifier.fetch("redirected").await, None);
    assert_eq!(verifier.fetch("forbidden").await, None);
}

// --- the inbox ----------------------------------------------------------------------------

const NODE: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);

fn clock_at(ms: &Arc<AtomicI64>) -> Clock {
    let ms = ms.clone();
    Arc::new(move || Ts(ms.load(Ordering::SeqCst)))
}

fn inbox() -> (Arc<CheckHostInbox>, Arc<AtomicI64>) {
    let now = Arc::new(AtomicI64::new(CONNECTED_AT + 60_000));
    (Arc::new(CheckHostInbox::with_clock(clock_at(&now))), now)
}

fn clear_result() -> CheckResult {
    parsed(CONNECTED, &connected_expect()).expect("the answer is valid")
}

fn blocked_result() -> CheckResult {
    parsed(TIMED_OUT, &timed_out_expect()).expect("the answer is valid")
}

#[tokio::test(start_paused = true)]
async fn a_result_goes_into_one_round_only() {
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Result(clear_result()));
    assert_eq!(
        inbox.take(NODE, Duration::from_secs(5)).await,
        Some(Entry::Result(clear_result()))
    );
    let started = Instant::now();
    assert_eq!(inbox.take(NODE, Duration::from_secs(5)).await, None);
    assert!(started.elapsed() >= Duration::from_secs(5));
}

#[tokio::test(start_paused = true)]
async fn a_result_older_than_10_minutes_is_not_taken() {
    let (inbox, now) = inbox();
    now.store(CONNECTED_AT + 11 * 60_000, Ordering::SeqCst);
    inbox.put(NODE, Entry::Result(clear_result()));
    assert_eq!(inbox.take(NODE, Duration::from_secs(5)).await, None);
    // The same result, 9 minutes old, is taken.
    now.store(CONNECTED_AT + 9 * 60_000, Ordering::SeqCst);
    inbox.put(NODE, Entry::Result(clear_result()));
    assert!(inbox.take(NODE, Duration::from_secs(5)).await.is_some());
}

#[tokio::test(start_paused = true)]
async fn a_result_arriving_during_the_wait_is_taken() {
    let (inbox, _) = inbox();
    let waiting = {
        let inbox = inbox.clone();
        tokio::spawn(async move { inbox.take(NODE, Duration::from_secs(60)).await })
    };
    tokio::time::sleep(Duration::from_secs(20)).await;
    inbox.put(Ipv4Addr::new(192, 0, 2, 99), Entry::Result(clear_result()));
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !waiting.is_finished(),
        "another node's result does not wake this one"
    );
    inbox.put(NODE, Entry::Result(blocked_result()));
    assert_eq!(
        waiting.await.expect("the task finishes"),
        Some(Entry::Result(blocked_result()))
    );
}

#[test]
fn an_id_is_claimed_once() {
    let (inbox, _) = inbox();
    assert!(inbox.claim("56626c28k235", Ts(0)));
    assert!(!inbox.claim("56626c28k235", Ts(1_000)));
    assert!(inbox.claim("another", Ts(1_000)));
    // Forgotten after 15 minutes.
    assert!(!inbox.claim("56626c28k235", Ts(14 * 60_000)));
    assert!(inbox.claim("56626c28k235", Ts(16 * 60_000)));
}

#[tokio::test(start_paused = true)]
async fn no_data_does_not_push_out_a_result() {
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Result(clear_result()));
    inbox.put(NODE, Entry::NoData("rate_limited".to_owned()));
    assert_eq!(
        inbox.take(NODE, Duration::from_secs(1)).await,
        Some(Entry::Result(clear_result()))
    );
    // With no result waiting, no-data is kept and handed out once.
    inbox.put(NODE, Entry::NoData("rate_limited".to_owned()));
    assert_eq!(
        inbox.take(NODE, Duration::from_secs(1)).await,
        Some(Entry::NoData("rate_limited".to_owned()))
    );
    assert_eq!(inbox.take(NODE, Duration::from_secs(1)).await, None);
    // A newer result replaces no-data.
    inbox.put(NODE, Entry::NoData("unavailable".to_owned()));
    inbox.put(NODE, Entry::Result(clear_result()));
    assert_eq!(
        inbox.take(NODE, Duration::from_secs(1)).await,
        Some(Entry::Result(clear_result()))
    );
}

#[tokio::test(start_paused = true)]
async fn disabled_stays_until_something_replaces_it() {
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Disabled);
    for _ in 0..3 {
        assert_eq!(
            inbox.take(NODE, Duration::from_secs(1)).await,
            Some(Entry::Disabled)
        );
    }
    inbox.put(NODE, Entry::Result(clear_result()));
    assert_eq!(
        inbox.take(NODE, Duration::from_secs(1)).await,
        Some(Entry::Result(clear_result()))
    );
}

// --- the sensor ---------------------------------------------------------------------------

struct Connector {
    open: AtomicBool,
    calls: std::sync::Mutex<Vec<(Ipv4Addr, u16, Duration)>>,
}

impl Connector {
    fn new(open: bool) -> Arc<Connector> {
        Arc::new(Connector {
            open: AtomicBool::new(open),
            calls: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(Ipv4Addr, u16, Duration)> {
        self.calls.lock().expect("the lock is not poisoned").clone()
    }
}

#[async_trait]
impl PortConnector for Connector {
    async fn open(&self, ip: Ipv4Addr, port: u16, timeout: Duration) -> bool {
        self.calls
            .lock()
            .expect("the lock is not poisoned")
            .push((ip, port, timeout));
        self.open.load(Ordering::SeqCst)
    }
}

fn target() -> Target {
    Target {
        ip: NODE,
        port: 443,
        sni: "example.com".to_owned(),
        secret: String::new(),
    }
}

fn sensor(inbox: &Arc<CheckHostInbox>, connector: &Arc<Connector>) -> CheckHostSensor {
    CheckHostSensor::new(inbox.clone(), connector.clone())
}

#[tokio::test(start_paused = true)]
async fn no_result_within_the_budget_is_no_data() {
    let (inbox, _) = inbox();
    let connector = Connector::new(true);
    let sensor = sensor(&inbox, &connector);
    assert_eq!(sensor.kind(), SensorKind::CheckHost);
    let started = Instant::now();
    let vote = sensor.vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::NoData));
    assert_eq!(started.elapsed(), Duration::from_secs(110));
    assert!(started.elapsed() < super::super::round::SENSOR_TIMEOUT);
    assert!(connector.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn disabled_is_not_configured_at_once() {
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Disabled);
    let connector = Connector::new(true);
    let started = Instant::now();
    let vote = sensor(&inbox, &connector).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::NotConfigured));
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(connector.calls().is_empty());
    assert_eq!(
        inbox.take(NODE, Duration::from_secs(1)).await,
        Some(Entry::Disabled)
    );
}

#[tokio::test(start_paused = true)]
async fn blocked_with_a_closed_port_is_port_closed() {
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Result(blocked_result()));
    let connector = Connector::new(false);
    let vote = sensor(&inbox, &connector).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::PortClosed));
    // The port of the check (81), not the port of the pool in `Target` (443).
    assert_eq!(connector.calls(), vec![(NODE, 81, PORT_TIMEOUT)]);
}

#[tokio::test(start_paused = true)]
async fn no_data_answers_at_once() {
    for (reason, expected) in [
        ("rate_limited", AbstainReason::RateLimited),
        ("unavailable", AbstainReason::Unavailable),
        ("anything else", AbstainReason::NoData),
    ] {
        let (inbox, _) = inbox();
        inbox.put(NODE, Entry::NoData(reason.to_owned()));
        let connector = Connector::new(true);
        let started = Instant::now();
        let vote = sensor(&inbox, &connector).vote(&target()).await;
        assert_eq!(vote.vote, Vote::Abstain(expected), "{reason}");
        assert_eq!(started.elapsed(), Duration::ZERO, "{reason}");
        assert!(!vote.evidence.to_string().contains("anything else"));
        assert!(
            connector.calls().is_empty(),
            "no data does not touch the port"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn blocked_with_an_open_port_stays_blocked() {
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Result(blocked_result()));
    let connector = Connector::new(true);
    let vote = sensor(&inbox, &connector).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Blocked);
    assert_eq!(vote.evidence, blocked_result().evidence());
    assert_eq!(connector.calls().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn clear_with_a_closed_port_is_port_closed() {
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Result(clear_result()));
    let connector = Connector::new(false);
    let vote = sensor(&inbox, &connector).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::PortClosed));
    assert_eq!(connector.calls(), vec![(NODE, 443, PORT_TIMEOUT)]);
}

#[tokio::test(start_paused = true)]
async fn clear_with_an_open_port_is_clear() {
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Result(clear_result()));
    let connector = Connector::new(true);
    let vote = sensor(&inbox, &connector).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Clear);
}

#[tokio::test(start_paused = true)]
async fn a_result_that_decides_nothing_does_not_touch_the_port() {
    let (inbox, _) = inbox();
    let mixed = CheckResult {
        points: [PointResult::Connected { ms: 2 }, PointResult::TimedOut],
        ..clear_result()
    };
    inbox.put(NODE, Entry::Result(mixed));
    let connector = Connector::new(true);
    let vote = sensor(&inbox, &connector).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::UnknownAnswer));
    assert!(connector.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_port_check_that_hangs_counts_as_closed() {
    struct Hangs;
    #[async_trait]
    impl PortConnector for Hangs {
        async fn open(&self, _: Ipv4Addr, _: u16, _: Duration) -> bool {
            std::future::pending().await
        }
    }
    let (inbox, _) = inbox();
    inbox.put(NODE, Entry::Result(blocked_result()));
    let sensor = CheckHostSensor::new(inbox, Arc::new(Hangs));
    let vote = sensor.vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::PortClosed));
}

#[tokio::test]
async fn the_tcp_connector_tells_open_from_closed() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("a port");
    let port = listener.local_addr().expect("an address").port();
    assert!(
        TcpConnector
            .open(Ipv4Addr::LOCALHOST, port, Duration::from_secs(3))
            .await
    );
    drop(listener);
    assert!(
        !TcpConnector
            .open(Ipv4Addr::LOCALHOST, port, Duration::from_secs(3))
            .await
    );
}
