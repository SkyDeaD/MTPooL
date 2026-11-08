//! The check-host loop against a double of the service (wiremock). The start answer is the
//! recorded one of the real service; the real service is never called.

use std::net::Ipv4Addr;
use std::time::Duration;

use mtpool_agent::check_host::{CheckHostLoop, GAP, IpSource, LIMIT_PAUSE};
use mtpool_protocol::agent::CheckHostReport;
use tokio::time::Instant;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const START: &str = include_str!("../../registrar/tests/fixtures/check-host/start.json");
const ID: &str = "56626c28k235";

fn started(port: u16) -> CheckHostReport {
    CheckHostReport::Started {
        id: ID.into(),
        port,
    }
}

fn node_loop(server: &MockServer, port: u16) -> CheckHostLoop {
    let ip = IpSource::default();
    ip.set(Some("1.1.1.1".parse::<Ipv4Addr>().unwrap()));
    let check = CheckHostLoop::new(&server.uri(), port, ip).unwrap();
    check.set_allowed(true);
    check
}

async fn answer(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("GET"))
        .and(path("/check-tcp"))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_raw(body.as_bytes().to_vec(), "application/json"),
        )
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn it_checks_at_once_after_enrolling() {
    let server = MockServer::start().await;
    answer(&server, 200, START).await;
    let check = node_loop(&server, 22);
    check.restart();

    check.tick(Instant::now()).await;

    assert!(check.has_result());
    assert_eq!(check.take_report(), Some(started(22)));
    assert_eq!(check.take_report(), None, "an id is reported once");
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    let url = &received[0].url;
    let query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert!(
        query.contains(&("host".into(), "1.1.1.1:22".into())),
        "{query:?}"
    );
    let nodes: Vec<&str> = query
        .iter()
        .filter(|(k, _)| k == "node")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(
        nodes,
        ["ru1.node.check-host.net", "ru2.node.check-host.net"]
    );
}

#[tokio::test]
async fn a_request_waits_for_the_5_minute_gap() {
    let server = MockServer::start().await;
    answer(&server, 200, START).await;
    let check = node_loop(&server, 22);
    let t0 = Instant::now();
    check.restart();
    check.tick(t0).await;
    assert_eq!(requests(&server).await, 1);

    check.run_soon(t0 + Duration::from_secs(10));
    check.tick(t0 + Duration::from_secs(10)).await;
    check.tick(t0 + GAP - Duration::from_secs(1)).await;
    assert_eq!(requests(&server).await, 1, "too early");

    check.tick(t0 + GAP).await;
    assert_eq!(requests(&server).await, 2);

    // Nothing is asked for unless the registrar asks.
    check.tick(t0 + GAP * 3).await;
    assert_eq!(requests(&server).await, 2);
}

#[tokio::test]
async fn limit_exceeded_pauses_for_20_minutes_and_reports_no_data() {
    let server = MockServer::start().await;
    // Shape not recorded: the real service's limit answer has not been seen yet.
    answer(&server, 200, r#"{"error":"limit_exceeded"}"#).await;
    let check = node_loop(&server, 22);
    let t0 = Instant::now();
    check.restart();
    check.tick(t0).await;
    assert_eq!(
        check.take_report(),
        Some(CheckHostReport::NoData {
            reason: "rate_limited".into()
        })
    );
    assert!(!check.has_result());

    check.run_soon(t0 + GAP);
    check.tick(t0 + GAP).await;
    check.tick(t0 + LIMIT_PAUSE - Duration::from_secs(1)).await;
    assert_eq!(requests(&server).await, 1, "paused");

    check.tick(t0 + LIMIT_PAUSE).await;
    assert_eq!(requests(&server).await, 2);
}

#[tokio::test]
async fn port_zero_never_calls_the_service_and_reports_disabled_every_time() {
    let server = MockServer::start().await;
    answer(&server, 200, START).await;
    let check = node_loop(&server, 0);
    check.restart();
    check.run_soon(Instant::now());
    check.tick(Instant::now() + GAP).await;

    assert_eq!(requests(&server).await, 0);
    for _ in 0..3 {
        assert_eq!(check.take_report(), Some(CheckHostReport::Disabled));
    }
    assert!(!check.has_result(), "nothing to hurry for");
}

#[tokio::test]
async fn a_custom_user_agent_is_sent() {
    let server = MockServer::start().await;
    answer(&server, 200, START).await;
    let check = node_loop(&server, 22);
    check.restart();
    check.tick(Instant::now()).await;

    let received = server.received_requests().await.unwrap();
    let agent = received[0]
        .headers
        .get("user-agent")
        .expect("a user agent")
        .to_str()
        .unwrap();
    assert!(agent.starts_with("mtpool-agent/"), "{agent}");
    assert!(!agent.to_lowercase().contains("python"));
    assert_eq!(
        received[0].headers.get("accept").unwrap(),
        "application/json"
    );
}

#[tokio::test]
async fn a_service_answer_it_does_not_know_is_no_data() {
    for (status, body) in [
        (200, "<html><body>502 Bad Gateway</body></html>"),
        (500, "oops"),
        (200, r#"{"ok":0}"#),
        (200, r#"{"ok":1,"request_id":"../x"}"#),
        (200, "null"),
        (403, r#"{"error":"forbidden"}"#),
    ] {
        let server = MockServer::start().await;
        answer(&server, status, body).await;
        let check = node_loop(&server, 22);
        check.restart();
        check.tick(Instant::now()).await;
        assert_eq!(
            check.take_report(),
            Some(CheckHostReport::NoData {
                reason: "unavailable".into()
            }),
            "{status} {body}"
        );
    }
}

#[tokio::test]
async fn the_service_is_left_alone_until_the_registrar_takes_the_result() {
    let server = MockServer::start().await;
    answer(&server, 200, START).await;
    let check = node_loop(&server, 22);
    check.set_allowed(false);
    check.restart();
    check.tick(Instant::now()).await;
    assert_eq!(requests(&server).await, 0);

    check.set_allowed(true);
    check.tick(Instant::now()).await;
    assert_eq!(requests(&server).await, 1, "the wish was kept");
}
