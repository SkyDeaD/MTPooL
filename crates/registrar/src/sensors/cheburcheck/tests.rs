//! The cheburcheck sensor against recorded answers of the real service.
//!
//! The files in `tests/fixtures/cheburcheck` were recorded from the live site on 2026-11-03,
//! anonymously (no key is needed):
//!
//! - `check_8.8.8.8.json`: `GET /api/v1/check?target=8.8.8.8`, `blocked: false`.
//! - `check_1.1.1.1.json`: the same for `1.1.1.1`, `blocked: true` with a `cdn_providers`
//!   entry. The flag comes from a provider list, not from a probe.
//! - `probe_stream.txt`: the SSE stream for the first one: a `started` event, 45 `result`
//!   events (30 `ok`, 11 `uncertain`, 4 `tspu_block`, one verdict each in a `verdicts` array),
//!   then `done`. Keep-alive comments (`:`) sit between events. About 15 seconds end to end.
//! - `probe_stream_1.1.1.1.txt`: the stream for the second. Same verdicts for the same probes.
//! - `check_error_500.json`: an invalid target (500). `check_error_422.json`: no target (422).
//!   `probe_error_401.json`: an id the service did not issue (401).
//!
//! No field in any answer shows the age of a cached probe result: a `result` event carries no
//! time, and a second check of the same address started a new job with a new id.

use std::net::Ipv4Addr;
use std::time::Duration;

use mtpool_core::{AbstainReason, SensorKind, Vote};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const CHECK_CLEAR: &str = include_str!("../../../tests/fixtures/cheburcheck/check_8.8.8.8.json");
const CHECK_LISTED: &str = include_str!("../../../tests/fixtures/cheburcheck/check_1.1.1.1.json");
const STREAM: &str = include_str!("../../../tests/fixtures/cheburcheck/probe_stream.txt");
const STREAM_LISTED: &str =
    include_str!("../../../tests/fixtures/cheburcheck/probe_stream_1.1.1.1.txt");
const ERROR_500: &str = include_str!("../../../tests/fixtures/cheburcheck/check_error_500.json");
const ERROR_401: &str = include_str!("../../../tests/fixtures/cheburcheck/probe_error_401.json");

const NODE_IP: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);

fn target() -> Target {
    Target {
        ip: NODE_IP,
        port: 443,
        sni: "example.com".to_owned(),
        secret: "ee00112233445566778899aabbccddeeff".to_owned(),
    }
}

fn id_of(check: &str) -> String {
    let document: Value = serde_json::from_str(check).expect("a recorded fixture is JSON");
    document["id"].as_str().expect("id").to_owned()
}

/// A check answer in the recorded shape with the given flag and id.
fn check_body(blocked: bool, id: &str) -> String {
    format!(
        r#"{{"id":"{id}","target":"192.0.2.10","target_type":"IPv4","blocked":{blocked},"ips":["192.0.2.10"]}}"#
    )
}

fn result_event(n: usize, verdict: &str) -> String {
    format!(
        "event:result\ndata:{{\"asn\":\"AS64496\",\"cdn_unblocked\":false,\"dns\":null,\"dpi_hop\":1,\"host_results\":[],\"job_id\":\"job\",\"probe_id\":\"{n}\",\"provider\":\"Example ISP\",\"region\":\"Example region\",\"target_hop\":2,\"verdicts\":[\"{verdict}\"]}}\n\n"
    )
}

const STARTED: &str =
    "event:started\ndata:{\"id\":\"job\",\"online_probes\":46,\"target\":\"192.0.2.10\"}\n\n:\n";
const DONE: &str = "event:done\ndata:{\"id\":\"job\",\"online_probes\":46,\"response_count\":0,\"status\":\"done\"}\n\n";

/// A complete stream with the given verdicts, in order.
fn stream_of(verdicts: &[&str]) -> String {
    let mut text = STARTED.to_owned();
    for (n, verdict) in verdicts.iter().enumerate() {
        text.push_str(&result_event(n, verdict));
    }
    text.push_str(DONE);
    text
}

fn counts(blocked: usize, ok: usize, uncertain: usize) -> Vec<&'static str> {
    let mut all = vec!["tspu_block"; blocked];
    all.extend(vec!["ok"; ok]);
    all.extend(vec!["uncertain"; uncertain]);
    all
}

fn sensor_for(base: &str) -> CheburcheckSensor {
    CheburcheckSensor::new(Url::parse(base).expect("uri")).expect("sensor")
}

/// A server that answers the check with `check` and the stream with `stream`.
async fn serve(
    check_status: u16,
    check: &str,
    id: &str,
    stream_status: u16,
    stream: &str,
) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/check"))
        .and(query_param("target", "192.0.2.10"))
        .respond_with(ResponseTemplate::new(check_status).set_body_string(check))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/probe/{id}")))
        .respond_with(
            ResponseTemplate::new(stream_status)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(stream),
        )
        .mount(&server)
        .await;
    server
}

async fn vote_on(stream: &str, listed: bool) -> SensorVote {
    let server = serve(200, &check_body(listed, "job-1"), "job-1", 200, stream).await;
    sensor_for(&server.uri()).vote(&target()).await
}

fn abstained(vote: &SensorVote, reason: AbstainReason) {
    assert_eq!(vote.vote, Vote::Abstain(reason), "{}", vote.evidence);
}

/// A server that answers the check and then sends a part of the stream and stalls.
async fn stalling_server(check: String, stream_prefix: String) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let check = check.clone();
            let prefix = stream_prefix.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buffer = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&buffer[..n]),
                    }
                }
                let line = String::from_utf8_lossy(&head);
                if line.starts_with("GET /api/v1/check") {
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{check}",
                        check.len()
                    );
                    let _ = socket.write_all(answer.as_bytes()).await;
                } else {
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
                    let _ = socket.write_all(head.as_bytes()).await;
                    if !prefix.is_empty() {
                        let chunk = format!("{:x}\r\n{prefix}\r\n", prefix.len());
                        let _ = socket.write_all(chunk.as_bytes()).await;
                    }
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            });
        }
    });
    Url::parse(&format!("http://127.0.0.1:{port}/")).expect("url")
}

fn requests_to(server_requests: &[wiremock::Request], prefix: &str) -> usize {
    server_requests
        .iter()
        .filter(|r| r.url.path().starts_with(prefix))
        .count()
}

// ---- the vote ----

#[tokio::test]
async fn fast_blocked_true_alone_gives_no_vote() {
    // The flag is true and every probe that counts says clear: Clear, with the flag as a hint.
    let vote = vote_on(&stream_of(&counts(0, 5, 3)), true).await;
    assert_eq!(vote.vote, Vote::Clear);
    assert_eq!(vote.evidence["provider_listed"], true);
    // The flag is true and no probe counts: no vote at all, not a block.
    let vote = vote_on(&stream_of(&counts(0, 0, 6)), true).await;
    abstained(&vote, AbstainReason::NoData);
    assert_eq!(vote.evidence["provider_listed"], true);
    // The recorded answers of 1.1.1.1 (flag true): the probes decide.
    let id = id_of(CHECK_LISTED);
    let server = serve(200, CHECK_LISTED, &id, 200, STREAM_LISTED).await;
    let vote = sensor_for(&server.uri()).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Clear, "{}", vote.evidence);
    assert_eq!(vote.evidence["provider_listed"], true);
}

#[tokio::test]
async fn probes_majority_blocked() {
    let vote = vote_on(&stream_of(&counts(3, 2, 4)), false).await;
    assert_eq!(vote.vote, Vote::Blocked, "{}", vote.evidence);
    assert_eq!(vote.evidence["counted"], 5);
    assert_eq!(vote.evidence["blocked"], 3);
    assert_eq!(vote.evidence["clear"], 2);
    assert_eq!(vote.evidence["uncertain"], 4);
    // sni_block counts as a block too.
    let vote = vote_on(&stream_of(&["sni_block", "sni_block", "ok"]), false).await;
    assert_eq!(vote.vote, Vote::Blocked, "{}", vote.evidence);
    // One more than half is enough, one probe less is not.
    assert_eq!(
        vote_on(&stream_of(&counts(4, 3, 0)), false).await.vote,
        Vote::Blocked
    );
    assert_eq!(
        vote_on(&stream_of(&counts(3, 4, 0)), false).await.vote,
        Vote::Clear
    );
}

#[tokio::test]
async fn tie_is_clear() {
    let vote = vote_on(&stream_of(&counts(2, 2, 0)), false).await;
    assert_eq!(vote.vote, Vote::Clear, "{}", vote.evidence);
    let vote = vote_on(&stream_of(&counts(1, 1, 7)), false).await;
    assert_eq!(vote.vote, Vote::Clear, "{}", vote.evidence);
}

#[tokio::test]
async fn uncertain_not_counted() {
    // One block among many uncertain probes is a majority of the counted ones.
    let vote = vote_on(&stream_of(&counts(1, 0, 20)), false).await;
    assert_eq!(vote.vote, Vote::Blocked, "{}", vote.evidence);
    assert_eq!(vote.evidence["counted"], 1);
    assert_eq!(vote.evidence["uncertain"], 20);
    // A verdict nobody knows is not counted either.
    let vote = vote_on(
        &stream_of(&["ok", "ok", "mystery", "mystery", "mystery"]),
        false,
    )
    .await;
    assert_eq!(vote.vote, Vote::Clear, "{}", vote.evidence);
    assert_eq!(vote.evidence["counted"], 2);
    assert_eq!(vote.evidence["uncertain"], 3);
    // A probe with two verdicts, one of them a block, is a block; none known is not counted.
    let mixed = format!(
        "{STARTED}{}{}{}{DONE}",
        "event:result\ndata:{\"probe_id\":\"1\",\"verdicts\":[\"ok\",\"tspu_block\"]}\n\n",
        "event:result\ndata:{\"probe_id\":\"2\",\"verdicts\":[]}\n\n",
        result_event(3, "ok"),
    );
    let vote = vote_on(&mixed, false).await;
    assert_eq!(vote.evidence["blocked"], 1);
    assert_eq!(vote.evidence["clear"], 1);
    assert_eq!(vote.evidence["uncertain"], 1);
}

#[tokio::test]
async fn no_counted_abstains() {
    abstained(
        &vote_on(&stream_of(&counts(0, 0, 8)), false).await,
        AbstainReason::NoData,
    );
    abstained(
        &vote_on(&stream_of(&[]), false).await,
        AbstainReason::NoData,
    );
}

#[tokio::test]
async fn the_recorded_streams_vote_clear() {
    let id = id_of(CHECK_CLEAR);
    let server = serve(200, CHECK_CLEAR, &id, 200, STREAM).await;
    let vote = sensor_for(&server.uri()).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Clear, "{}", vote.evidence);
    assert_eq!(vote.evidence["provider_listed"], false);
    assert_eq!(vote.evidence["probe_id"], id);
    assert_eq!(vote.evidence["counted"], 34);
    assert_eq!(vote.evidence["blocked"], 4);
    assert_eq!(vote.evidence["clear"], 30);
    assert_eq!(vote.evidence["uncertain"], 11);
    assert!(vote.evidence.get("partial").is_none());
    let probes = vote.evidence["probes"].as_array().expect("probes");
    assert_eq!(probes.len(), 45);
    assert_eq!(probes[0]["verdict"], "ok");
    assert_eq!(probes[0]["provider"], "SkyNet");
}

// ---- the budget ----

#[test]
fn the_default_budget_is_90_seconds() {
    assert_eq!(DEFAULT_BUDGET, Duration::from_secs(90));
    let sensor =
        CheburcheckSensor::new(Url::parse("http://127.0.0.1:9/").expect("url")).expect("sensor");
    assert_eq!(sensor.budget, Duration::from_secs(90));
}

#[tokio::test]
async fn budget_90s_abstains() {
    // The stream starts and then says nothing until the budget is gone: Timeout.
    let base = stalling_server(check_body(false, "job-1"), STARTED.to_owned()).await;
    let sensor = CheburcheckSensor::new(base)
        .expect("sensor")
        .with_budget(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let vote = sensor.vote(&target()).await;
    abstained(&vote, AbstainReason::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(vote.evidence["provider_listed"], false);
}

#[tokio::test]
async fn budget_exhausted_after_some_probes_votes_on_them() {
    let prefix = format!(
        "{STARTED}{}{}{}",
        result_event(1, "tspu_block"),
        result_event(2, "tspu_block"),
        result_event(3, "ok")
    );
    let base = stalling_server(check_body(false, "job-1"), prefix).await;
    let sensor = CheburcheckSensor::new(base)
        .expect("sensor")
        .with_budget(Duration::from_millis(300));
    let vote = sensor.vote(&target()).await;
    assert_eq!(vote.vote, Vote::Blocked, "{}", vote.evidence);
    assert_eq!(vote.evidence["partial"], true);
    assert_eq!(vote.evidence["counted"], 3);
    // Only uncertain probes before the budget ends: still nothing counted.
    let prefix = format!("{STARTED}{}", result_event(1, "uncertain"));
    let base = stalling_server(check_body(false, "job-1"), prefix).await;
    let sensor = CheburcheckSensor::new(base)
        .expect("sensor")
        .with_budget(Duration::from_millis(300));
    abstained(&sensor.vote(&target()).await, AbstainReason::Timeout);
}

#[tokio::test]
async fn a_stream_that_never_connects_is_inside_the_budget() {
    // The server accepts and never answers the stream request.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let check = check_body(false, "job-1");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let check = check.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                let n = socket.read(&mut buffer).await.unwrap_or(0);
                if buffer[..n].starts_with(b"GET /api/v1/check") {
                    let answer = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{check}",
                        check.len()
                    );
                    let _ = socket.write_all(answer.as_bytes()).await;
                } else {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                }
            });
        }
    });
    let base = Url::parse(&format!("http://127.0.0.1:{port}/")).expect("url");
    let sensor = CheburcheckSensor::new(base)
        .expect("sensor")
        .with_budget(Duration::from_millis(300));
    abstained(&sensor.vote(&target()).await, AbstainReason::Timeout);
}

// ---- the limiter ----

#[test]
fn the_window_lets_4_in_and_slides() {
    let limiter = Limiter::new(4, Duration::from_secs(60));
    let t0 = Instant::now();
    for second in 0..4 {
        assert!(
            limiter.try_acquire(t0 + Duration::from_secs(second)),
            "place {second}"
        );
    }
    // A refusal takes nothing and does not move the window.
    assert!(!limiter.try_acquire(t0 + Duration::from_secs(30)));
    assert!(!limiter.try_acquire(t0 + Duration::from_secs(59)));
    // The first place frees up 60 seconds after it was taken, not earlier.
    assert!(limiter.try_acquire(t0 + Duration::from_secs(60)));
    assert!(!limiter.try_acquire(t0 + Duration::from_secs(60)));
    assert!(limiter.try_acquire(t0 + Duration::from_secs(61)));
    assert!(limiter.try_acquire(t0 + Duration::from_secs(62)));
    assert!(limiter.try_acquire(t0 + Duration::from_secs(63)));
    assert!(!limiter.try_acquire(t0 + Duration::from_secs(63)));
    // After a quiet minute everything is free again.
    assert!(limiter.try_acquire(t0 + Duration::from_secs(200)));
}

#[tokio::test]
async fn limiter_4_per_minute() {
    let server = serve(
        200,
        &check_body(false, "job-1"),
        "job-1",
        200,
        &stream_of(&counts(0, 3, 0)),
    )
    .await;
    let sensor = sensor_for(&server.uri());
    for n in 0..4 {
        let vote = sensor.vote(&target()).await;
        assert_eq!(vote.vote, Vote::Clear, "vote {n}: {}", vote.evidence);
    }
    let before = server.received_requests().await.expect("requests");
    assert_eq!(requests_to(&before, "/api/v1/check"), 4);
    // The fifth one in the same minute does not leave the machine.
    let vote = sensor.vote(&target()).await;
    abstained(&vote, AbstainReason::RateLimited);
    let after = server.received_requests().await.expect("requests");
    assert_eq!(after.len(), before.len(), "no request for a limited vote");
    // The limit is shared by every clone, that is by the whole pool.
    abstained(
        &sensor.clone().vote(&target()).await,
        AbstainReason::RateLimited,
    );
    // A limited vote is not a block, and its evidence says why.
    assert_eq!(vote.evidence["reason"], "rate_limited");
}

// ---- the evidence ----

#[tokio::test]
async fn provider_listed_saved_in_evidence() {
    let id = id_of(CHECK_LISTED);
    let server = serve(200, CHECK_LISTED, &id, 200, STREAM_LISTED).await;
    let vote = sensor_for(&server.uri()).vote(&target()).await;
    assert_eq!(vote.evidence["provider_listed"], true);
    let id = id_of(CHECK_CLEAR);
    let server = serve(200, CHECK_CLEAR, &id, 200, STREAM).await;
    let vote = sensor_for(&server.uri()).vote(&target()).await;
    assert_eq!(vote.evidence["provider_listed"], false);
    // An abstention after the check keeps the hint too.
    let vote = vote_on(&stream_of(&counts(0, 0, 2)), true).await;
    assert_eq!(vote.evidence["provider_listed"], true);
}

#[tokio::test]
async fn at_most_50_probes_are_kept_as_evidence() {
    let vote = vote_on(&stream_of(&counts(0, 120, 0)), false).await;
    assert_eq!(vote.vote, Vote::Clear);
    assert_eq!(vote.evidence["counted"], 120);
    assert_eq!(
        vote.evidence["probes"].as_array().expect("probes").len(),
        50
    );
}

#[tokio::test]
async fn the_evidence_keeps_nothing_but_the_tally_and_the_probe_labels() {
    let vote = vote_on(&stream_of(&counts(1, 1, 0)), false).await;
    let text = vote.evidence.to_string();
    assert!(!text.contains("ee0011"), "the secret stays out: {text}");
    assert!(
        !text.contains("192.0.2.10"),
        "the address stays out: {text}"
    );
    assert!(
        !text.contains("AS64496"),
        "raw events are not stored: {text}"
    );
    let probe = &vote.evidence["probes"][0];
    assert_eq!(probe["provider"], "Example ISP");
    assert_eq!(probe["region"], "Example region");
}

// ---- what is sent ----

#[tokio::test]
async fn only_the_address_is_sent_and_redirects_are_not_followed() {
    let server = serve(
        200,
        &check_body(false, "job-1"),
        "job-1",
        200,
        &stream_of(&counts(0, 1, 0)),
    )
    .await;
    let sensor = sensor_for(&server.uri());
    sensor.vote(&target()).await;
    let requests = server.received_requests().await.expect("requests");
    let check = requests
        .iter()
        .find(|r| r.url.path() == "/api/v1/check")
        .expect("check");
    assert_eq!(check.url.query(), Some("target=192.0.2.10"));
    let everything = format!("{requests:?}");
    assert!(!everything.contains("ee0011"), "the secret is not sent");
    assert!(!everything.contains("example.com"), "the SNI is not sent");
    // A redirect is a refusal, not a place to follow.
    let other = MockServer::start().await;
    let target_server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/elsewhere", target_server.uri())),
        )
        .mount(&other)
        .await;
    abstained(
        &sensor_for(&other.uri()).vote(&target()).await,
        AbstainReason::Unavailable,
    );
    assert!(
        target_server
            .received_requests()
            .await
            .expect("requests")
            .is_empty()
    );
}

// ---- failures ----

#[tokio::test]
async fn http_error_abstains() {
    // The check fails.
    for status in [400, 401, 403, 404, 422, 500, 502, 503] {
        let server = serve(
            status,
            ERROR_500,
            "job-1",
            200,
            &stream_of(&counts(9, 0, 0)),
        )
        .await;
        let vote = sensor_for(&server.uri()).vote(&target()).await;
        abstained(&vote, AbstainReason::Unavailable);
        assert_eq!(vote.evidence["http_status"], status, "status {status}");
    }
    // The stream fails after a good check.
    for status in [401, 404, 500, 503] {
        let server = serve(200, &check_body(true, "job-1"), "job-1", status, ERROR_401).await;
        let vote = sensor_for(&server.uri()).vote(&target()).await;
        abstained(&vote, AbstainReason::Unavailable);
        assert_eq!(vote.evidence["http_status"], status, "status {status}");
        assert_eq!(
            vote.evidence["provider_listed"], true,
            "the hint survives a failed stream"
        );
    }
    // 429 is the service saying slow down.
    let server = serve(429, "{}", "job-1", 200, "").await;
    abstained(
        &sensor_for(&server.uri()).vote(&target()).await,
        AbstainReason::RateLimited,
    );
    let server = serve(200, &check_body(false, "job-1"), "job-1", 429, "").await;
    abstained(
        &sensor_for(&server.uri()).vote(&target()).await,
        AbstainReason::RateLimited,
    );
}

#[tokio::test]
async fn an_unreachable_service_is_unavailable() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let vote = sensor_for(&format!("http://127.0.0.1:{port}/"))
        .vote(&target())
        .await;
    abstained(&vote, AbstainReason::Unavailable);
}

#[tokio::test]
async fn answers_that_cannot_be_read_are_unknown() {
    // The check is not JSON, or lacks the fields the sensor needs.
    for body in [
        "<html>nope</html>",
        "[]",
        "{}",
        r#"{"id":"job-1"}"#,
        r#"{"id":"job-1","blocked":"yes"}"#,
        r#"{"blocked":false}"#,
        r#"{"id":7,"blocked":false}"#,
    ] {
        let server = serve(200, body, "job-1", 200, &stream_of(&counts(9, 0, 0))).await;
        abstained(
            &sensor_for(&server.uri()).vote(&target()).await,
            AbstainReason::UnknownAnswer,
        );
    }
    // A stream event without a known shape.
    for bad in [
        "event:result\ndata:not json\n\n",
        "event:result\ndata:{\"verdicts\":\"ok\"}\n\n",
        "event:result\ndata:{\"probe_id\":\"1\"}\n\n",
        "event:result\ndata:{\"verdicts\":[1]}\n\n",
        "event:result\ndata:\n\n",
        "event:surprise\ndata:{}\n\n",
        "event:error\ndata:{\"message\":\"x\"}\n\n",
        "data:{\"verdicts\":[\"tspu_block\"]}\n\n",
    ] {
        let stream = format!("{STARTED}{}{bad}{DONE}", result_event(1, "tspu_block"));
        let vote = vote_on(&stream, false).await;
        abstained(&vote, AbstainReason::UnknownAnswer);
    }
}

#[tokio::test]
async fn an_id_that_is_not_a_path_segment_is_refused_without_a_request() {
    for id in ["../admin", "a/b", "a?b", "", "a b", "."] {
        let server = MockServer::start().await;
        let body = serde_json::json!({"id": id, "blocked": false}).to_string();
        Mock::given(method("GET"))
            .and(path("/api/v1/check"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let vote = sensor_for(&server.uri()).vote(&target()).await;
        abstained(&vote, AbstainReason::UnknownAnswer);
        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests_to(&requests, "/api/v1/probe"), 0, "id {id:?}");
    }
}

#[tokio::test]
async fn a_stream_that_ends_without_done_is_not_a_vote() {
    let stream = format!(
        "{STARTED}{}{}",
        result_event(1, "tspu_block"),
        result_event(2, "tspu_block")
    );
    abstained(&vote_on(&stream, false).await, AbstainReason::Unavailable);
}

#[tokio::test]
async fn a_stream_over_1_mib_is_refused() {
    let mut stream = STARTED.to_owned();
    while stream.len() <= MAX_STREAM_BYTES {
        stream.push_str(&result_event(1, "tspu_block"));
    }
    stream.push_str(DONE);
    abstained(&vote_on(&stream, false).await, AbstainReason::UnknownAnswer);
}

#[tokio::test]
async fn a_check_answer_over_1_mib_is_refused() {
    let big = format!(
        r#"{{"id":"job-1","blocked":false,"pad":"{}"}}"#,
        "x".repeat(MAX_CHECK_BYTES)
    );
    let server = serve(200, &big, "job-1", 200, &stream_of(&counts(0, 3, 0))).await;
    abstained(
        &sensor_for(&server.uri()).vote(&target()).await,
        AbstainReason::UnknownAnswer,
    );
}

#[test]
fn the_sensor_has_the_cheburcheck_kind() {
    let sensor = CheburcheckSensor::new(CheburcheckSensor::default_base()).expect("sensor");
    assert_eq!(sensor.kind(), SensorKind::Cheburcheck);
}

#[test]
fn the_target_does_not_show_the_secret() {
    let text = format!("{:?}", target());
    assert!(!text.contains("ee0011"));
}

// ---- the SSE reader ----

fn events_of(chunks: impl IntoIterator<Item = Vec<u8>>) -> Vec<SseEvent> {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    for chunk in chunks {
        events.extend(parser.push(&chunk));
    }
    events.extend(parser.finish());
    events
}

#[test]
fn the_reader_cuts_the_recorded_stream_into_events() {
    let events = events_of([STREAM.as_bytes().to_vec()]);
    assert_eq!(events.len(), 47);
    assert_eq!(events[0].name.as_deref(), Some("started"));
    assert_eq!(events[46].name.as_deref(), Some("done"));
    assert!(
        events[1..46]
            .iter()
            .all(|e| e.name.as_deref() == Some("result"))
    );
    let first: Value = serde_json::from_str(&events[1].data).expect("json");
    assert_eq!(first["provider"], "SkyNet");
    // The keep-alive comment is not an event.
    assert!(events.iter().all(|e| !e.data.is_empty()));
}

#[test]
fn the_reader_gives_the_same_events_for_any_chunking() {
    let whole = events_of([STREAM.as_bytes().to_vec()]);
    for size in [1, 2, 3, 7, 64, 1000] {
        let chunks = STREAM.as_bytes().chunks(size).map(<[u8]>::to_vec);
        assert_eq!(events_of(chunks), whole, "chunks of {size} bytes");
    }
}

#[test]
fn the_reader_takes_crlf_and_a_space_after_the_colon() {
    let text = "event: result\r\ndata: {\"a\":1}\r\n\r\nevent:done\r\ndata:x\r\n\r\n";
    let events = events_of([text.as_bytes().to_vec()]);
    assert_eq!(
        events,
        vec![
            SseEvent {
                name: Some("result".into()),
                data: "{\"a\":1}".into()
            },
            SseEvent {
                name: Some("done".into()),
                data: "x".into()
            },
        ]
    );
}

#[test]
fn the_reader_joins_data_lines_and_ignores_other_fields() {
    let text = "id: 5\nretry: 10\nevent:result\ndata:one\ndata:two\n\n";
    let events = events_of([text.as_bytes().to_vec()]);
    assert_eq!(
        events,
        vec![SseEvent {
            name: Some("result".into()),
            data: "one\ntwo".into()
        }]
    );
}

#[test]
fn the_reader_keeps_an_event_the_stream_ended_in() {
    let events = events_of([b"event:result\ndata:{\"a\":1}".to_vec()]);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "{\"a\":1}");
}
