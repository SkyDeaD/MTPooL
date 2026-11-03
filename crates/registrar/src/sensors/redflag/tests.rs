//! The RedFlag sensor against answers in the documented shape.
//!
//! No node of this project exists yet, so nothing could be recorded: the files in
//! `tests/fixtures/redflag` are built from the shape the design document gives (`status`,
//! `checked_at`, and the `local` and `remote` blocks with `tcp`, `handshake`, `data` and
//! `latency_ms`). Each carries a top-level `_source` key that says so, and the parser ignores it.
//! The real exchange is checked against the live service in the end-to-end task.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mtpool_core::{AbstainReason, SensorKind, SensorVote, Vote};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const OK: &str = include_str!("../../../tests/fixtures/redflag/ok.json");
const DATA_CUT: &str = include_str!("../../../tests/fixtures/redflag/data_cut.json");
const RKN_BLOCK: &str = include_str!("../../../tests/fixtures/redflag/rkn_block.json");
const MT_DEAD: &str = include_str!("../../../tests/fixtures/redflag/mt_dead.json");
const DOWN: &str = include_str!("../../../tests/fixtures/redflag/down.json");
const UNKNOWN: &str = include_str!("../../../tests/fixtures/redflag/unknown.json");
const UNRECOGNISED: &str = include_str!("../../../tests/fixtures/redflag/unrecognised_status.json");

const KEY: &str = "test-key-not-a-real-credential";
const NODE_IP: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);
const SECRET: &str = "ee00112233445566778899aabbccddeeff6578616d706c652e636f6d";

fn target() -> Target {
    Target {
        ip: NODE_IP,
        port: 443,
        sni: "example.com".to_owned(),
        secret: SECRET.to_owned(),
    }
}

fn key() -> TokenSource {
    Arc::new(|| Some(KEY.to_owned()))
}

fn no_key() -> TokenSource {
    Arc::new(|| None)
}

fn sensor_for(server: &MockServer) -> RedFlagSensor {
    RedFlagSensor::new(Url::parse(&server.uri()).expect("uri"), key()).expect("sensor")
}

async fn serve(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/check"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    server
}

async fn vote_on(body: &str) -> SensorVote {
    let server = serve(200, body).await;
    sensor_for(&server).vote(&target()).await
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.expect("recording").len()
}

fn reason(vote: &SensorVote) -> Option<&str> {
    vote.evidence.get("reason").and_then(Value::as_str)
}

#[tokio::test]
async fn the_sensor_has_the_redflag_kind() {
    let server = serve(200, OK).await;
    assert_eq!(sensor_for(&server).kind(), SensorKind::RedFlag);
}

#[tokio::test]
async fn status_table() {
    let cases = [
        (OK, Vote::Clear),
        (RKN_BLOCK, Vote::Blocked),
        (DATA_CUT, Vote::Blocked),
        (MT_DEAD, Vote::Abstain(AbstainReason::NoData)),
        (DOWN, Vote::Abstain(AbstainReason::NoData)),
        (UNKNOWN, Vote::Abstain(AbstainReason::NoData)),
        (UNRECOGNISED, Vote::Abstain(AbstainReason::UnknownAnswer)),
    ];
    for (body, expected) in cases {
        let vote = vote_on(body).await;
        assert_eq!(vote.vote, expected, "{body}");
    }
}

#[tokio::test]
async fn a_status_that_is_not_in_the_table_is_never_a_ban_vote() {
    for status in ["ok", "Rkn_Block", "BLOCKED", "", " OK", "OK\n"] {
        let body = json!({"status": status, "local": {}, "remote": {}}).to_string();
        let vote = vote_on(&body).await;
        assert_eq!(
            vote.vote,
            Vote::Abstain(AbstainReason::UnknownAnswer),
            "{status:?}"
        );
    }
}

#[tokio::test]
async fn answers_that_cannot_be_read_are_unknown() {
    let not_a_string = json!({"status": 7}).to_string();
    let bodies = [
        "",
        "not json",
        "[]",
        "{}",
        "null",
        r#"{"status": null}"#,
        not_a_string.as_str(),
    ];
    for body in bodies {
        let vote = vote_on(body).await;
        assert_eq!(
            vote.vote,
            Vote::Abstain(AbstainReason::UnknownAnswer),
            "{body:?}"
        );
    }
}

#[tokio::test]
async fn an_answer_that_has_only_a_status_still_votes() {
    let vote = vote_on(r#"{"status": "OK"}"#).await;
    assert_eq!(vote.vote, Vote::Clear);
    assert_eq!(vote.evidence["status"], "OK");
}

#[tokio::test]
async fn the_unknown_status_word_is_kept_clipped_in_the_evidence() {
    let word = "X".repeat(500);
    let vote = vote_on(&json!({"status": word}).to_string()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::UnknownAnswer));
    let kept = vote.evidence["status"].as_str().expect("status kept");
    assert!(kept.len() <= 32, "{}", kept.len());
}

#[tokio::test]
async fn sends_ip_not_domain() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/check"))
        .and(query_param("mode", "sync"))
        .and(query_param("timeout", "15"))
        .and(header("x-api-key", KEY))
        .and(header("content-type", "application/json"))
        .and(body_json(json!({
            "server": "192.0.2.10",
            "port": 443,
            "secret": SECRET,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK))
        .expect(1)
        .mount(&server)
        .await;
    let vote = sensor_for(&server).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Clear, "{:?}", vote.evidence);
    // `expect(1)` is checked when the server is dropped, and also here.
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn nothing_but_the_ip_port_and_secret_is_sent() {
    let server = serve(200, OK).await;
    sensor_for(&server).vote(&target()).await;
    let seen = server.received_requests().await.expect("recording");
    let body: Value = serde_json::from_slice(&seen[0].body).expect("a JSON body");
    let mut keys: Vec<&String> = body.as_object().expect("object").keys().collect();
    keys.sort();
    assert_eq!(keys, ["port", "secret", "server"]);
    assert!(
        !seen[0].url.as_str().contains(KEY),
        "the key is not a parameter"
    );
}

#[tokio::test]
async fn codes_401_403_429_502_503_abstain() {
    let cases = [
        (401, AbstainReason::Unavailable),
        (403, AbstainReason::Unavailable),
        (429, AbstainReason::RateLimited),
        (502, AbstainReason::Unavailable),
        (503, AbstainReason::Unavailable),
        (500, AbstainReason::Unavailable),
        (400, AbstainReason::Unavailable),
        (404, AbstainReason::Unavailable),
    ];
    for (code, expected) in cases {
        // An error body that looks like a verdict must not be read as one.
        let server = serve(code, RKN_BLOCK).await;
        let vote = sensor_for(&server).vote(&target()).await;
        assert_eq!(vote.vote, Vote::Abstain(expected), "HTTP {code}");
        if code != 429 {
            assert_eq!(vote.evidence["http_status"], code, "HTTP {code}");
        }
    }
}

#[tokio::test]
async fn a_redirect_is_not_followed_and_does_not_vote() {
    let target_server = serve(200, RKN_BLOCK).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/v1/check", target_server.uri())),
        )
        .mount(&server)
        .await;
    let vote = sensor_for(&server).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::Unavailable));
    assert_eq!(
        requests(&target_server).await,
        0,
        "the key goes to one host"
    );
}

#[tokio::test]
async fn an_unreachable_service_is_unavailable() {
    let server = MockServer::start().await;
    let base = Url::parse(&server.uri()).expect("uri");
    drop(server);
    let sensor = RedFlagSensor::new(base, key()).expect("sensor");
    let vote = sensor.vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::Unavailable));
}

#[tokio::test]
async fn a_slow_service_is_a_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(RKN_BLOCK)
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let sensor = sensor_for(&server).with_timeout(Duration::from_millis(200));
    let vote = sensor.vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::Timeout));
}

#[tokio::test]
async fn the_default_request_timeout_is_25_seconds() {
    assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(25));
}

#[tokio::test]
async fn an_answer_over_256_kib_is_refused() {
    let padding = "x".repeat(300 * 1024);
    let body = json!({"status": "RKN_BLOCK", "padding": padding}).to_string();
    let vote = vote_on(&body).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::UnknownAnswer));
}

#[tokio::test]
async fn retry_after_respected() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK))
        .mount(&server)
        .await;
    let sensor = sensor_for(&server);

    let first = sensor.vote(&target()).await;
    assert_eq!(first.vote, Vote::Abstain(AbstainReason::RateLimited));
    assert_eq!(requests(&server).await, 1);

    // Inside the delay nothing is sent.
    let second = sensor.vote(&target()).await;
    assert_eq!(second.vote, Vote::Abstain(AbstainReason::RateLimited));
    assert_eq!(
        requests(&server).await,
        1,
        "no request while the delay runs"
    );

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let third = sensor.vote(&target()).await;
    assert_eq!(third.vote, Vote::Clear);
    assert_eq!(requests(&server).await, 2);
}

#[tokio::test]
async fn a_429_without_retry_after_waits_a_minute() {
    let server = serve(429, "").await;
    let sensor = sensor_for(&server);
    sensor.vote(&target()).await;
    let again = sensor.vote(&target()).await;
    assert_eq!(again.vote, Vote::Abstain(AbstainReason::RateLimited));
    assert_eq!(requests(&server).await, 1);
    // The clones of a sensor are one sensor: the delay is shared.
    let clone = sensor.clone();
    clone.vote(&target()).await;
    assert_eq!(requests(&server).await, 1);
}

#[test]
fn retry_delay_takes_seconds_or_an_http_date() {
    let now = 1_700_000_000_000;
    assert_eq!(retry_delay(Some("120"), now), Duration::from_secs(120));
    assert_eq!(retry_delay(Some(" 7 "), now), Duration::from_secs(7));
    assert_eq!(
        retry_delay(Some("Tue, 14 Nov 2023 22:15:20 GMT"), now),
        Duration::from_secs(120)
    );
    // A date in the past asks for no wait.
    assert_eq!(
        retry_delay(Some("Tue, 14 Nov 2023 22:00:00 GMT"), now),
        Duration::ZERO
    );
}

#[test]
fn retry_delay_defaults_to_a_minute_and_is_capped() {
    let now = 1_700_000_000_000;
    assert_eq!(retry_delay(None, now), Duration::from_secs(60));
    assert_eq!(retry_delay(Some("soon"), now), Duration::from_secs(60));
    assert_eq!(retry_delay(Some(""), now), Duration::from_secs(60));
    assert_eq!(retry_delay(Some("-5"), now), Duration::from_secs(60));
    assert_eq!(retry_delay(Some("999999999"), now), MAX_RETRY_AFTER);
    assert_eq!(
        retry_delay(Some("Tue, 14 Nov 2033 22:13:20 GMT"), now),
        MAX_RETRY_AFTER
    );
    assert_eq!(MAX_RETRY_AFTER, Duration::from_secs(3600));
}

#[tokio::test]
async fn limiter_20_per_minute() {
    let server = serve(200, OK).await;
    let sensor = sensor_for(&server);
    for n in 0..20 {
        let vote = sensor.vote(&target()).await;
        assert_eq!(vote.vote, Vote::Clear, "vote {n}");
    }
    let refused = sensor.vote(&target()).await;
    assert_eq!(refused.vote, Vote::Abstain(AbstainReason::RateLimited));
    assert_eq!(reason(&refused), Some("rate_limited"));
    assert_eq!(requests(&server).await, 20, "the refused vote sent nothing");
    assert_eq!(RATE_LIMIT, 20);
    assert_eq!(RATE_WINDOW, Duration::from_secs(60));
}

#[tokio::test]
async fn a_vote_refused_for_the_delay_takes_no_place_in_the_window() {
    let server = serve(429, "").await;
    let sensor = sensor_for(&server);
    sensor.vote(&target()).await;
    for _ in 0..30 {
        sensor.vote(&target()).await;
    }
    // Only the first vote used a place: 19 are left once the delay is over.
    *sensor.not_before.lock().expect("lock") = None;
    server.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK))
        .mount(&server)
        .await;
    let mut answered = 0;
    for _ in 0..25 {
        if sensor.vote(&target()).await.vote == Vote::Clear {
            answered += 1;
        }
    }
    assert_eq!(answered, 19);
}

#[tokio::test]
async fn evidence_contains_local_and_remote() {
    let vote = vote_on(RKN_BLOCK).await;
    assert_eq!(vote.vote, Vote::Blocked);
    assert_eq!(
        vote.evidence,
        json!({
            "status": "RKN_BLOCK",
            "checked_at": "2026-11-03T12:00:00Z",
            "local": {"tcp": false, "handshake": false, "data": false, "latency_ms": null},
            "remote": {"tcp": true, "handshake": true, "data": true, "latency_ms": 29},
        })
    );
}

#[tokio::test]
async fn the_evidence_keeps_only_the_documented_fields() {
    let body = json!({
        "_source": "x",
        "status": "OK",
        "checked_at": "2026-11-03T12:00:00Z",
        "server": "192.0.2.10",
        "secret": SECRET,
        "local": {"tcp": true, "handshake": true, "data": true, "latency_ms": 1,
                  "dc": {"deep": [1, 2, 3]}, "ip": "192.0.2.10"},
        "remote": {"tcp": true, "handshake": true, "data": {"nested": true}, "latency_ms": 2},
        "extra": {"anything": 1},
    })
    .to_string();
    let vote = vote_on(&body).await;
    let text = vote.evidence.to_string();
    assert!(
        !text.contains(SECRET) && !text.contains("192.0.2.10"),
        "{text}"
    );
    assert!(
        !text.contains("extra") && !text.contains("_source"),
        "{text}"
    );
    assert_eq!(
        vote.evidence["local"],
        json!({"tcp": true, "handshake": true, "data": true, "latency_ms": 1})
    );
    // A block that is not made of plain values loses the field, not the vote.
    assert!(vote.evidence["remote"].get("data").is_none());
    assert_eq!(vote.vote, Vote::Clear);
}

#[tokio::test]
async fn evidence_never_holds_the_key() {
    let server = serve(200, OK).await;
    let sensor = sensor_for(&server);
    let mut all = vec![sensor.vote(&target()).await];
    let failing = serve(401, KEY).await;
    all.push(sensor_for(&failing).vote(&target()).await);
    let garbage = serve(200, KEY).await;
    all.push(sensor_for(&garbage).vote(&target()).await);
    for vote in all {
        assert!(
            !vote.evidence.to_string().contains(KEY),
            "{:?}",
            vote.evidence
        );
    }
}

#[tokio::test]
async fn not_configured_abstains() {
    let server = serve(200, OK).await;
    let sensor =
        RedFlagSensor::new(Url::parse(&server.uri()).expect("uri"), no_key()).expect("sensor");
    let vote = sensor.vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::NotConfigured));
    assert_eq!(requests(&server).await, 0, "nothing is sent without a key");
}

#[tokio::test]
async fn an_empty_key_is_no_key() {
    let server = serve(200, OK).await;
    let token: TokenSource = Arc::new(|| Some(String::new()));
    let sensor = RedFlagSensor::new(Url::parse(&server.uri()).expect("uri"), token).expect("s");
    let vote = sensor.vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::NotConfigured));
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn without_a_pool_secret_there_is_nothing_to_check() {
    let server = serve(200, OK).await;
    let mut bare = target();
    bare.secret.clear();
    let vote = sensor_for(&server).vote(&bare).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::NotConfigured));
    assert_eq!(requests(&server).await, 0);
}

#[tokio::test]
async fn the_key_is_asked_for_on_every_call() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-api-key", "second"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK))
        .mount(&server)
        .await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let token: TokenSource = Arc::new(move || {
        // The first call is the sensor's own check for a key; the later ones are requests.
        let n = counter.fetch_add(1, Ordering::SeqCst);
        Some(if n < 1 { "first" } else { "second" }.to_owned())
    });
    let sensor = RedFlagSensor::new(Url::parse(&server.uri()).expect("uri"), token).expect("s");
    sensor.vote(&target()).await;
    let seen = sensor.vote(&target()).await;
    assert_eq!(seen.vote, Vote::Clear, "{:?}", seen.evidence);
    assert!(calls.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn the_base_may_carry_a_path_prefix() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/check"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK))
        .mount(&server)
        .await;
    let base = Url::parse(&format!("{}/api", server.uri())).expect("uri");
    let sensor = RedFlagSensor::new(base, key()).expect("sensor");
    assert_eq!(sensor.vote(&target()).await.vote, Vote::Clear);
}

#[test]
fn the_default_base_is_the_documented_one() {
    let base = RedFlagSensor::default_base();
    assert_eq!(base.scheme(), "https");
    assert_eq!(base.as_str(), DEFAULT_BASE);
}
