//! The Globalping loop against a double of the API (wiremock) with the recorded answers of the
//! real one. A measurement of the node is a POST without a token, a few GETs and the id.

mod common;

use std::time::Duration;

use common::FakePorts;
use mtpool_agent::gp::{GpJob, GpLoop};
use mtpool_protocol::agent::GpReport;
use serde_json::{Value, json};
use tokio::time::Instant;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const CREATE: &str = include_str!("fixtures/globalping/create.json");
const IN_PROGRESS: &str = include_str!("fixtures/globalping/in_progress.json");
const FINISHED: &str = include_str!("fixtures/globalping/finished.json");
const NOT_FOUND: &str = include_str!("fixtures/globalping/not_found.json");

const MEASUREMENTS: &str = "/v1/measurements";
const ID: &str = "2BSGWjCSdcIMKXZfj00021RC5";

fn job() -> GpJob {
    GpJob {
        ip: "192.0.2.1".parse().unwrap(),
        port: 8443,
        sni: "tls.example.test".into(),
        interval: Duration::from_secs(300),
    }
}

fn gp(server: &MockServer, ports: &std::sync::Arc<FakePorts>) -> GpLoop {
    let gp = GpLoop::new(&server.uri(), ports.clone())
        .unwrap()
        .with_timing(Duration::from_millis(10), Duration::from_millis(400));
    gp.set_job(Some(job()));
    gp
}

fn json_body(text: &str, status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(text.as_bytes().to_vec(), "application/json")
}

async fn mount_create(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path(MEASUREMENTS))
        .respond_with(json_body(body, status))
        .mount(server)
        .await;
}

async fn mount_get(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("GET"))
        .and(path(format!("{MEASUREMENTS}/{ID}")))
        .respond_with(json_body(body, status))
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

fn no_data(reason: &str) -> GpReport {
    GpReport::NoData {
        reason: reason.into(),
    }
}

#[tokio::test]
async fn gp_request_has_exact_shape() {
    let server = MockServer::start().await;
    mount_create(&server, 202, CREATE).await;
    mount_get(&server, 200, FINISHED).await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);

    gp.tick(Instant::now()).await;

    let seen = requests(&server).await;
    let post = seen.iter().find(|r| r.method.as_str() == "POST").unwrap();
    assert_eq!(post.url.path(), MEASUREMENTS);
    let body: Value = serde_json::from_slice(&post.body).unwrap();
    assert_eq!(
        body,
        json!({
            "type": "http",
            "target": "192.0.2.1",
            "locations": [{ "country": "RU", "tags": ["eyeball-network"], "limit": 20 }],
            "measurementOptions": {
                "port": 8443,
                "protocol": "HTTPS",
                "request": { "method": "HEAD", "host": "tls.example.test", "path": "/" }
            }
        })
    );
    for request in &seen {
        assert!(
            request.headers.get("authorization").is_none(),
            "no token goes to Globalping: {request:?}"
        );
    }
    assert_eq!(gp.take_result(), Some(GpReport::Measured { id: ID.into() }));
    assert_eq!(gp.take_result(), None, "an id is handed out once");
}

#[tokio::test]
async fn running_measurement_is_polled_until_it_finishes() {
    let server = MockServer::start().await;
    mount_create(&server, 202, CREATE).await;
    Mock::given(method("GET"))
        .and(path(format!("{MEASUREMENTS}/{ID}")))
        .respond_with(json_body(IN_PROGRESS, 200))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    mount_get(&server, 200, FINISHED).await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);

    gp.tick(Instant::now()).await;

    let gets = requests(&server)
        .await
        .iter()
        .filter(|r| r.method.as_str() == "GET")
        .count();
    assert_eq!(gets, 3);
    assert_eq!(gp.take_result(), Some(GpReport::Measured { id: ID.into() }));
}

#[tokio::test]
async fn measurement_that_never_finishes_is_a_timeout() {
    let server = MockServer::start().await;
    mount_create(&server, 202, CREATE).await;
    mount_get(&server, 200, IN_PROGRESS).await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);

    gp.tick(Instant::now()).await;

    assert_eq!(gp.take_result(), Some(no_data("timeout")));
}

#[tokio::test]
async fn first_measure_runs_immediately_after_enroll() {
    let server = MockServer::start().await;
    mount_create(&server, 202, CREATE).await;
    mount_get(&server, 200, FINISHED).await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);
    let t0 = Instant::now();
    let posts = |seen: Vec<Request>| seen.iter().filter(|r| r.method.as_str() == "POST").count();

    // The first tick measures at once: nobody waits for an interval after enrolling.
    gp.tick(t0).await;
    assert_eq!(posts(requests(&server).await), 1);
    assert!(gp.take_result().is_some());

    // Then the interval is kept.
    gp.tick(t0 + Duration::from_secs(299)).await;
    assert_eq!(posts(requests(&server).await), 1);
    gp.tick(t0 + Duration::from_secs(300)).await;
    assert_eq!(posts(requests(&server).await), 2);
    assert!(gp.take_result().is_some());

    // Enrolling again starts over, inside the interval.
    gp.restart();
    assert_eq!(gp.take_result(), None, "the result of before is forgotten");
    gp.tick(t0 + Duration::from_secs(301)).await;
    assert_eq!(posts(requests(&server).await), 3);
}

#[tokio::test]
async fn only_a_finished_measurement_waits_to_be_reported() {
    let server = MockServer::start().await;
    mount_create(&server, 202, CREATE).await;
    mount_get(&server, 200, FINISHED).await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);
    assert!(!gp.has_measurement());

    gp.tick(Instant::now()).await;
    assert!(gp.has_measurement());
    assert!(matches!(gp.take_result(), Some(GpReport::Measured { .. })));
    assert!(!gp.has_measurement(), "an id is reported once");

    // A failure has no id for the registrar to read: it keeps to the usual report.
    let server = MockServer::start().await;
    mount_create(&server, 429, "{}").await;
    let gp = self::gp(&server, &ports);
    gp.tick(Instant::now()).await;
    assert_eq!(gp.take_result(), Some(no_data("rate_limited")));
    gp.tick(Instant::now()).await;
    assert!(!gp.has_measurement());
}

#[tokio::test]
async fn gp_429_reported_as_no_data() {
    let server = MockServer::start().await;
    // The body of a 429 was not recorded: the status alone says it.
    Mock::given(method("POST"))
        .and(path(MEASUREMENTS))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
        .mount(&server)
        .await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);
    let t0 = Instant::now();

    gp.tick(t0).await;

    assert_eq!(gp.take_result(), Some(no_data("rate_limited")));
    // A limited node does not hammer the API: the next try is at the interval.
    gp.tick(t0 + Duration::from_secs(10)).await;
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn gp_5xx_and_garbage_are_unavailable() {
    for (status, body) in [(503, "{}"), (202, "not json"), (202, r#"{"id":"../x"}"#)] {
        let server = MockServer::start().await;
        mount_create(&server, status, body).await;
        let ports = FakePorts::new(true);
        let gp = gp(&server, &ports);

        gp.tick(Instant::now()).await;

        assert_eq!(
            gp.take_result(),
            Some(no_data("unavailable")),
            "{status} {body}"
        );
    }
}

#[tokio::test]
async fn gp_api_is_reached_over_ipv4_only() {
    // Without a token Globalping counts tests by address, and for IPv6 one counter covers a whole
    // /64: two nodes of one host network would eat one quota. A node is always reachable over IPv4.
    let listener = std::net::TcpListener::bind("[::1]:0").expect("IPv6 loopback");
    let server = MockServer::builder().listener(listener).start().await;
    mount_create(&server, 202, r#"{"id":"m1"}"#).await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);

    gp.tick(Instant::now()).await;

    assert!(requests(&server).await.is_empty());
    assert_eq!(gp.take_result(), Some(no_data("unavailable")));
}

#[tokio::test]
async fn gp_unreachable_api_is_unavailable() {
    // Port 1 refuses. A dropped mock server would not do: its port goes back to a pool and the
    // next test's server answers on it.
    let ports = FakePorts::new(true);
    let gp = GpLoop::new("http://127.0.0.1:1", ports)
        .unwrap()
        .with_timing(Duration::from_millis(10), Duration::from_millis(100));
    gp.set_job(Some(job()));

    gp.tick(Instant::now()).await;

    assert_eq!(gp.take_result(), Some(no_data("unavailable")));
}

#[tokio::test]
async fn vanished_measurement_is_unavailable() {
    let server = MockServer::start().await;
    mount_create(&server, 202, CREATE).await;
    mount_get(&server, 404, NOT_FOUND).await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);

    gp.tick(Instant::now()).await;

    assert_eq!(gp.take_result(), Some(no_data("unavailable")));
}

#[tokio::test]
async fn gp_skipped_until_port_listens() {
    let server = MockServer::start().await;
    mount_create(&server, 202, CREATE).await;
    mount_get(&server, 200, FINISHED).await;
    let ports = FakePorts::new(false);
    let gp = gp(&server, &ports);
    let t0 = Instant::now();

    gp.tick(t0).await;
    assert!(
        requests(&server).await.is_empty(),
        "nothing listens: no measurement"
    );
    assert_eq!(gp.take_result(), Some(no_data("no_data")));
    assert_eq!(
        ports.asked.lock().unwrap().first().copied(),
        Some((8443, Some("192.0.2.1".parse().unwrap())))
    );

    // As soon as the port answers, the measurement is made, not after an interval.
    ports.set(true);
    gp.tick(t0 + Duration::from_secs(1)).await;
    assert_eq!(requests(&server).await.len(), 2);
    assert_eq!(gp.take_result(), Some(GpReport::Measured { id: ID.into() }));
}

#[tokio::test]
async fn an_unreported_id_is_not_overwritten_by_no_data() {
    let server = MockServer::start().await;
    mount_create(&server, 202, CREATE).await;
    mount_get(&server, 200, FINISHED).await;
    let ports = FakePorts::new(true);
    let gp = gp(&server, &ports);
    let t0 = Instant::now();
    gp.tick(t0).await;

    ports.set(false);
    gp.tick(t0 + Duration::from_secs(300)).await;

    assert_eq!(gp.take_result(), Some(GpReport::Measured { id: ID.into() }));
}

#[tokio::test]
async fn no_job_no_measurement() {
    let server = MockServer::start().await;
    let ports = FakePorts::new(true);
    let gp = GpLoop::new(&server.uri(), ports).unwrap();

    gp.tick(Instant::now()).await;

    assert!(requests(&server).await.is_empty());
    assert_eq!(gp.take_result(), None);
}
