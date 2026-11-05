//! The Globalping sensor against recorded answers of the real service.
//!
//! The files in `tests/fixtures/globalping` were recorded from the live API on 2026-11-03:
//!
//! - `create.json`: the answer to `POST /v1/measurements` (202).
//! - `finished_tls_403.json`: 20 Russian eyeball probes, HTTPS HEAD to a Cloudflare address with
//!   another `Host`. Every probe finished, got HTTP 403 and has a `tls` object. The 403 is the
//!   expected answer of a server that does not know the host; the handshake is what counts.
//! - `finished_invalid_cert.json`: the same against `self-signed.badssl.com`. The certificate is
//!   not valid (`tls.authorized: false`, `tls.error: DEPTH_ZERO_SELF_SIGNED_CERT`) but the `tls`
//!   object is there and the status is `finished`. The recording is why the success criterion
//!   does not look at `authorized`: a fake-TLS node cannot show a valid certificate for the
//!   masked domain, only finish the handshake.
//! - `finished_no_tls.json`: port 81, where nothing listens. Every probe has status `failed`
//!   and `"tls": null` after a TCP connect timeout.
//! - `failed_refused.json`: a node whose proxy was down, recorded in production and made
//!   anonymous (the address of the node, its domain and the id are replaced). Every probe has
//!   status `failed`, `"tls": null` and `rawOutput` `connect ECONNREFUSED <ip>:443`: the port
//!   answered with a refusal, which a closed port does and a block by address does not need to.
//! - `in_progress.json`: one polling answer before the end (port 81 as the file above): status
//!   `in-progress`, every probe `in-progress`, no `tls` key.
//! - `not_found.json`: the answer to an unknown measurement id (404).
//! - `limits_anon.json`: `GET /v1/limits` without a token (`type: "ip"`, no `credits`).
//! - `limits_token.json`: the same with a token (`type: "user"`, plus `credits`).
//!
//! The options a finished measurement echoes leave out the defaults: `protocol` is absent for
//! HTTPS (present as `HTTP` when HTTP was asked), `method` and `path` are absent for `HEAD` and
//! `/`. `port` and `request.host` are echoed when they were sent.
//!
//! Success criterion of one probe: `status == "finished"` and a `tls` object (not null).

use std::net::Ipv4Addr;
use std::time::Duration;

use mtpool_core::{AbstainReason, SensorKind, SensorVote, Vote};
use mtpool_protocol::parse_rfc3339;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::verifier::GpRejected;

const TLS_403: &str = include_str!("../../../tests/fixtures/globalping/finished_tls_403.json");
const INVALID_CERT: &str =
    include_str!("../../../tests/fixtures/globalping/finished_invalid_cert.json");
const NO_TLS: &str = include_str!("../../../tests/fixtures/globalping/finished_no_tls.json");
const REFUSED: &str = include_str!("../../../tests/fixtures/globalping/failed_refused.json");
const IN_PROGRESS: &str = include_str!("../../../tests/fixtures/globalping/in_progress.json");
const NOT_FOUND: &str = include_str!("../../../tests/fixtures/globalping/not_found.json");
const CREATE: &str = include_str!("../../../tests/fixtures/globalping/create.json");
const LIMITS_ANON: &str = include_str!("../../../tests/fixtures/globalping/limits_anon.json");
const LIMITS_TOKEN: &str = include_str!("../../../tests/fixtures/globalping/limits_token.json");

/// The id of the measurement `finished_tls_403.json` is and `create.json` answers with.
const ID: &str = "2BSGWjCSdcIMKXZfj00021RC5";
const TEST_TOKEN: &str = "test-token-for-the-fake-server";
/// The hostname the recordings were made with.
const SNI: &str = "example.com";
const NODE_IP: Ipv4Addr = Ipv4Addr::new(104, 16, 132, 229);

fn json_of(text: &str) -> Value {
    serde_json::from_str(text).expect("a recorded fixture is JSON")
}

fn created_at(document: &Value) -> i64 {
    parse_rfc3339(document["createdAt"].as_str().expect("createdAt")).expect("RFC 3339")
}

fn expect_for(document: &Value, port: u16, offset_ms: i64) -> GpExpect {
    GpExpect {
        ip: NODE_IP,
        port,
        sni: SNI.to_owned(),
        reported_at: mtpool_core::Ts(created_at(document) + offset_ms),
    }
}

fn token_source(token: Option<&'static str>) -> TokenSource {
    Arc::new(move || token.map(str::to_owned))
}

fn shared(settings: GlobalpingSettings) -> SharedSettings {
    Arc::new(ArcSwap::from_pointee(settings))
}

fn client(server: &MockServer, token: Option<&'static str>) -> GlobalpingClient {
    let base = Url::parse(&server.uri()).expect("mock server uri");
    GlobalpingClient::new(base, token_source(token))
        .expect("client")
        .with_poll_interval(Duration::from_millis(10))
}

fn verifier(server: &MockServer) -> GlobalpingVerifier {
    GlobalpingVerifier::new(client(server, Some(TEST_TOKEN)), shared(Default::default()))
}

fn sensor(
    server: &MockServer,
    token: Option<&'static str>,
    settings: &SharedSettings,
) -> GlobalpingSensor {
    GlobalpingSensor::new(client(server, token), settings.clone())
}

fn target() -> Target {
    Target {
        ip: NODE_IP,
        port: 443,
        sni: SNI.to_owned(),
        secret: "ee00112233445566778899aabbccddeeff".to_owned(),
    }
}

async fn serve_measurement(server: &MockServer, id: &str, status: u16, body: &Value) {
    Mock::given(method("GET"))
        .and(path(format!("/v1/measurements/{id}")))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn serve_limits(server: &MockServer, remaining: u64) {
    let mut limits = json_of(LIMITS_TOKEN);
    limits["rateLimit"]["measurements"]["create"]["remaining"] = json!(remaining);
    Mock::given(method("GET"))
        .and(path("/v1/limits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(limits))
        .mount(server)
        .await;
}

async fn serve_create(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/measurements"))
        .respond_with(ResponseTemplate::new(202).set_body_json(json_of(CREATE)))
        .mount(server)
        .await;
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.expect("recording is on")
}

fn creates(requests: &[Request]) -> usize {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .count()
}

/// `fixture` with `change` applied to its probe results.
fn changed(fixture: &str, change: impl FnOnce(&mut Vec<Value>)) -> Value {
    let mut document = json_of(fixture);
    let results = document["results"].as_array_mut().expect("results");
    change(results);
    document
}

fn failed_probe() -> Value {
    json!({"status": "failed", "tls": null})
}

async fn verified(server: &MockServer, expect: &GpExpect) -> Result<SensorVote, GpRejected> {
    verifier(server).verify(ID, expect).await
}

fn abstained(vote: &SensorVote) -> AbstainReason {
    match vote.vote {
        Vote::Abstain(reason) => reason,
        other => panic!("expected an abstention, got {other:?}"),
    }
}

// ----- the success criterion ---------------------------------------------------------------

#[tokio::test]
async fn status_403_with_tls_counts_success() {
    let server = MockServer::start().await;
    let document = json_of(TLS_403);
    // The recording really is a 403 with a finished handshake on every probe.
    assert_eq!(document["results"][0]["result"]["statusCode"], 403);
    serve_measurement(&server, ID, 200, &document).await;

    let vote = verified(&server, &expect_for(&document, 443, 5_000))
        .await
        .expect("a measurement made against this node");

    assert_eq!(vote.vote, Vote::Clear);
    assert_eq!(vote.evidence["measurement_id"], ID);
    assert_eq!(vote.evidence["probes_total"], 20);
    assert_eq!(vote.evidence["probes_with_result"], 20);
    assert_eq!(vote.evidence["probes_ok"], 20);
    assert_eq!(vote.evidence["probes"][0]["country"], "RU");
    assert_eq!(vote.evidence["probes"][0]["tls"], true);
}

#[tokio::test]
async fn invalid_certificate_still_counts_success() {
    // `tls.authorized` is false on every probe: only the handshake is required.
    let server = MockServer::start().await;
    let document = json_of(INVALID_CERT);
    assert_eq!(document["results"][0]["result"]["tls"]["authorized"], false);
    serve_measurement(&server, ID, 200, &document).await;

    // The recording was made against a hostname; aimed at an address with a `Host`, the
    // handshake still counts.
    let mut aimed = document.clone();
    aimed["target"] = json!(NODE_IP.to_string());
    aimed["measurementOptions"]["request"] = json!({"host": SNI});
    // The recording was taken while the measurement still ran; what the probes had finished is
    // what counts here.
    aimed["status"] = json!("finished");
    let server = MockServer::start().await;
    serve_measurement(&server, ID, 200, &aimed).await;
    let vote = verified(&server, &expect_for(&aimed, 443, 5_000))
        .await
        .expect("accepted");
    assert_eq!(vote.vote, Vote::Clear);
}

#[tokio::test]
async fn no_tls_block_counts_failure() {
    let server = MockServer::start().await;
    let document = json_of(NO_TLS);
    assert!(document["results"][0]["result"]["tls"].is_null());
    serve_measurement(&server, ID, 200, &document).await;

    let vote = verified(&server, &expect_for(&document, 81, 5_000))
        .await
        .expect("a measurement made against this node");

    assert_eq!(vote.vote, Vote::Blocked);
    assert_eq!(vote.evidence["probes_with_result"], 20);
    assert_eq!(vote.evidence["probes_ok"], 0);
    assert_eq!(vote.evidence["probes"][0]["status"], "failed");
    assert_eq!(vote.evidence["probes"][0]["tls"], false);
}

#[tokio::test]
async fn finished_probe_without_a_tls_key_is_a_failure() {
    // A probe that finished but shows no handshake did not get through.
    let server = MockServer::start().await;
    let document = changed(TLS_403, |results| {
        for probe in results {
            probe["result"]
                .as_object_mut()
                .expect("result")
                .remove("tls");
        }
    });
    serve_measurement(&server, ID, 200, &document).await;

    let vote = verified(&server, &expect_for(&document, 443, 5_000))
        .await
        .expect("accepted");

    assert_eq!(vote.vote, Vote::Blocked);
    assert_eq!(vote.evidence["probes_ok"], 0);
}

#[tokio::test]
async fn under_10_probes_abstains() {
    let server = MockServer::start().await;
    let nine = changed(TLS_403, |results| results.truncate(9));
    serve_measurement(&server, ID, 200, &nine).await;
    let vote = verified(&server, &expect_for(&nine, 443, 5_000))
        .await
        .expect("accepted");
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::NoData));
    assert_eq!(vote.evidence["reason"], "no_data");
    assert_eq!(vote.evidence["probes_with_result"], 9);

    // Probes that are offline have no result: nine finished and eleven offline is nine.
    let server = MockServer::start().await;
    let offline = changed(TLS_403, |results| {
        for probe in results.iter_mut().skip(9) {
            probe["result"] = json!({"status": "offline"});
        }
    });
    serve_measurement(&server, ID, 200, &offline).await;
    let vote = verified(&server, &expect_for(&offline, 443, 5_000))
        .await
        .expect("accepted");
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::NoData));

    // Ten is enough, and ten failures are an answer: Blocked.
    let server = MockServer::start().await;
    let ten = changed(NO_TLS, |results| results.truncate(10));
    serve_measurement(&server, ID, 200, &ten).await;
    let vote = verified(&server, &expect_for(&ten, 81, 5_000))
        .await
        .expect("accepted");
    assert_eq!(vote.vote, Vote::Blocked);
}

#[tokio::test]
async fn ratio_below_half_blocked() {
    // 9 of 20 got a handshake: 45 percent.
    let server = MockServer::start().await;
    let below = changed(TLS_403, |results| {
        for probe in results.iter_mut().take(11) {
            probe["result"] = failed_probe();
        }
    });
    serve_measurement(&server, ID, 200, &below).await;
    let vote = verified(&server, &expect_for(&below, 443, 5_000))
        .await
        .expect("accepted");
    assert_eq!(vote.vote, Vote::Blocked);
    assert_eq!(vote.evidence["probes_ok"], 9);

    // Exactly half is not below the threshold.
    let server = MockServer::start().await;
    let half = changed(TLS_403, |results| {
        for probe in results.iter_mut().take(10) {
            probe["result"] = failed_probe();
        }
    });
    serve_measurement(&server, ID, 200, &half).await;
    let vote = verified(&server, &expect_for(&half, 443, 5_000))
        .await
        .expect("accepted");
    assert_eq!(vote.vote, Vote::Clear);
    assert_eq!(vote.evidence["probes_ok"], 10);
}

#[tokio::test]
async fn the_threshold_and_the_probe_minimum_come_from_the_settings() {
    let server = MockServer::start().await;
    let document = changed(TLS_403, |results| {
        for probe in results.iter_mut().take(8) {
            probe["result"] = failed_probe();
        }
    });
    serve_measurement(&server, ID, 200, &document).await;
    let settings = shared(GlobalpingSettings {
        success_threshold: 0.7,
        ..Default::default()
    });
    let verifier = GlobalpingVerifier::new(client(&server, Some(TEST_TOKEN)), settings);
    let vote = verifier
        .verify(ID, &expect_for(&document, 443, 5_000))
        .await
        .expect("accepted");
    // 12 of 20 is 60 percent, below 70.
    assert_eq!(vote.vote, Vote::Blocked);
}

// ----- what a measurement has to be --------------------------------------------------------

#[tokio::test]
async fn wrong_country_probe_rejected() {
    let server = MockServer::start().await;
    let document = changed(TLS_403, |results| {
        results[3]["probe"]["country"] = json!("DE");
    });
    serve_measurement(&server, ID, 200, &document).await;

    let verdict = verified(&server, &expect_for(&document, 443, 5_000)).await;

    assert_eq!(verdict, Err(GpRejected::Mismatch));
}

#[tokio::test]
async fn probe_outside_eyeball_networks_rejected() {
    let server = MockServer::start().await;
    let document = changed(TLS_403, |results| {
        results[3]["probe"]["tags"] = json!(["datacenter-network"]);
    });
    serve_measurement(&server, ID, 200, &document).await;

    let verdict = verified(&server, &expect_for(&document, 443, 5_000)).await;

    assert_eq!(verdict, Err(GpRejected::Mismatch));
}

#[tokio::test]
async fn wrong_target_or_sni_rejected() {
    let document = json_of(TLS_403);
    let aimed_at = |change: &dyn Fn(&mut Value)| {
        let mut changed = document.clone();
        change(&mut changed);
        changed
    };
    let cases: Vec<(&str, Value)> = vec![
        (
            "another address",
            aimed_at(&|d| d["target"] = json!("203.0.113.9")),
        ),
        (
            "a name as the target",
            aimed_at(&|d| d["target"] = json!(SNI)),
        ),
        (
            "another port",
            aimed_at(&|d| d["measurementOptions"]["port"] = json!(8443)),
        ),
        (
            "another Host",
            aimed_at(&|d| d["measurementOptions"]["request"]["host"] = json!("other.example.org")),
        ),
        (
            "no Host",
            aimed_at(&|d| {
                d["measurementOptions"]["request"] = json!({});
            }),
        ),
        (
            "plain HTTP",
            aimed_at(&|d| d["measurementOptions"]["protocol"] = json!("HTTP")),
        ),
        ("another kind", aimed_at(&|d| d["type"] = json!("ping"))),
        (
            "no probes asked for",
            aimed_at(&|d| d["probesCount"] = json!(0)),
        ),
    ];
    for (what, measurement) in cases {
        let server = MockServer::start().await;
        serve_measurement(&server, ID, 200, &measurement).await;
        let verdict = verified(&server, &expect_for(&document, 443, 5_000)).await;
        assert_eq!(verdict, Err(GpRejected::Mismatch), "{what}");
    }

    // The defaults the service leaves out are the defaults it applied.
    let server = MockServer::start().await;
    let explicit = aimed_at(&|d| {
        d["measurementOptions"]["protocol"] = json!("https");
    });
    serve_measurement(&server, ID, 200, &explicit).await;
    let vote = verified(&server, &expect_for(&document, 443, 5_000))
        .await
        .expect("HTTPS in any case is HTTPS");
    assert_eq!(vote.vote, Vote::Clear);
}

#[tokio::test]
async fn measurement_outside_window_rejected() {
    let document = json_of(TLS_403);
    let minute = 60_000;
    // Defaults: 600 s into the past and 60 s into the future of the report time.
    for (offset_ms, accepted) in [
        (11 * minute, false),
        (10 * minute + 1, false),
        (10 * minute, true),
        (0, true),
        (-minute, true),
        (-(minute + 1), false),
        (-2 * minute, false),
    ] {
        let server = MockServer::start().await;
        serve_measurement(&server, ID, 200, &document).await;
        let verdict = verified(&server, &expect_for(&document, 443, offset_ms)).await;
        if accepted {
            assert!(verdict.is_ok(), "offset {offset_ms} ms: {verdict:?}");
        } else {
            assert_eq!(verdict, Err(GpRejected::Stale), "offset {offset_ms} ms");
        }
    }
}

#[tokio::test]
async fn a_mismatch_wins_over_a_stale_one() {
    let server = MockServer::start().await;
    let document = changed(TLS_403, |results| {
        results[0]["probe"]["country"] = json!("DE");
    });
    serve_measurement(&server, ID, 200, &document).await;
    let verdict = verified(&server, &expect_for(&document, 443, 30 * 60_000)).await;
    assert_eq!(verdict, Err(GpRejected::Mismatch));
}

// ----- the service misbehaves --------------------------------------------------------------

#[tokio::test]
async fn unknown_measurement_is_a_404() {
    let server = MockServer::start().await;
    serve_measurement(&server, ID, 404, &json_of(NOT_FOUND)).await;
    let verdict = verified(&server, &expect_for(&json_of(TLS_403), 443, 0)).await;
    assert_eq!(verdict, Err(GpRejected::UnknownMeasurement));
}

#[tokio::test]
async fn a_measurement_id_is_never_a_path() {
    let server = MockServer::start().await;
    let expect = expect_for(&json_of(TLS_403), 443, 0);
    let verifier = verifier(&server);
    for id in [
        "",
        "../limits",
        "a/b",
        "a?x=1",
        "a#b",
        "id with space",
        &"x".repeat(65),
    ] {
        assert_eq!(
            verifier.verify(id, &expect).await,
            Err(GpRejected::UnknownMeasurement),
            "{id:?}"
        );
    }
    assert!(
        requests(&server).await.is_empty(),
        "nothing reached the service"
    );
}

#[tokio::test]
async fn http_429_5xx_and_garbage_are_unavailable_for_the_verifier() {
    let expect = expect_for(&json_of(TLS_403), 443, 0);
    let answers = [
        ResponseTemplate::new(429).insert_header("retry-after", "30"),
        ResponseTemplate::new(500),
        ResponseTemplate::new(503).set_body_string("down"),
        ResponseTemplate::new(200).set_body_string("<html>not json</html>"),
        ResponseTemplate::new(200).set_body_json(json!(["not", "a", "measurement"])),
        ResponseTemplate::new(401),
    ];
    for answer in answers {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/v1/measurements/{ID}")))
            .respond_with(answer)
            .mount(&server)
            .await;
        assert_eq!(
            verified(&server, &expect).await,
            Err(GpRejected::Unavailable)
        );
    }
}

#[tokio::test]
async fn an_unreachable_service_is_unavailable() {
    // Nothing listens on port 1.
    let base = Url::parse("http://127.0.0.1:1/").expect("uri");
    let client = GlobalpingClient::new(base, token_source(None)).expect("client");
    let verifier = GlobalpingVerifier::new(client, shared(Default::default()));
    let verdict = verifier
        .verify(ID, &expect_for(&json_of(TLS_403), 443, 0))
        .await;
    assert_eq!(verdict, Err(GpRejected::Unavailable));
}

#[tokio::test]
async fn a_running_measurement_is_polled_until_it_finishes() {
    let server = MockServer::start().await;
    let running = json_of(IN_PROGRESS);
    let finished = json_of(NO_TLS);
    Mock::given(method("GET"))
        .and(path(format!("/v1/measurements/{ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(&running))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    serve_measurement(&server, ID, 200, &finished).await;

    let vote = verified(&server, &expect_for(&finished, 81, 5_000))
        .await
        .expect("accepted once it finished");

    assert_eq!(vote.vote, Vote::Blocked);
    assert_eq!(
        requests(&server).await.len(),
        3,
        "two running answers, then the end"
    );
}

#[tokio::test]
async fn a_measurement_that_never_finishes_is_unavailable() {
    let server = MockServer::start().await;
    let running = json_of(IN_PROGRESS);
    serve_measurement(&server, ID, 200, &running).await;
    let verifier = verifier(&server).with_budget(Duration::from_millis(150));

    let verdict = verifier.verify(ID, &expect_for(&running, 81, 0)).await;

    assert_eq!(verdict, Err(GpRejected::Unavailable));
}

#[tokio::test]
async fn polling_is_at_most_two_requests_a_second() {
    assert!(DEFAULT_POLL_INTERVAL >= Duration::from_millis(500));
}

#[tokio::test]
async fn an_answer_over_2_mib_is_refused() {
    let server = MockServer::start().await;
    let huge = json!({"pad": "x".repeat(3 * 1024 * 1024)});
    serve_measurement(&server, ID, 200, &huge).await;
    let verdict = verified(&server, &expect_for(&json_of(TLS_403), 443, 0)).await;
    assert_eq!(verdict, Err(GpRejected::Unavailable));

    let error = client(&server, None).measurement(ID).await;
    assert_eq!(error.unwrap_err(), ClientError::TooLarge);
}

#[tokio::test]
async fn at_most_50_probes_are_kept_as_evidence() {
    let server = MockServer::start().await;
    let one = json_of(TLS_403)["results"][0].clone();
    let many = changed(TLS_403, |results| {
        *results = vec![one; 120];
    });
    serve_measurement(&server, ID, 200, &many).await;

    let vote = verified(&server, &expect_for(&many, 443, 0))
        .await
        .expect("accepted");

    assert_eq!(vote.vote, Vote::Clear);
    assert_eq!(vote.evidence["probes_total"], 120);
    assert_eq!(
        vote.evidence["probes"].as_array().expect("probes").len(),
        50
    );
}

#[tokio::test]
async fn the_verifier_sends_the_token_and_never_follows_a_redirect() {
    let server = MockServer::start().await;
    serve_measurement(&server, ID, 200, &json_of(TLS_403)).await;
    verified(&server, &expect_for(&json_of(TLS_403), 443, 0))
        .await
        .expect("accepted");
    let sent = requests(&server).await;
    let header = sent[0].headers.get("authorization").expect("authorization");
    assert_eq!(
        header.to_str().expect("ascii"),
        format!("Bearer {TEST_TOKEN}")
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/v1/measurements/{ID}")))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/v1/limits"))
        .mount(&server)
        .await;
    let verdict = verified(&server, &expect_for(&json_of(TLS_403), 443, 0)).await;
    assert_eq!(verdict, Err(GpRejected::Unavailable));
    assert_eq!(requests(&server).await.len(), 1);
}

// ----- the central measurement -------------------------------------------------------------

#[tokio::test]
async fn central_measure_checks_limits_first() {
    let server = MockServer::start().await;
    serve_limits(&server, 500).await;
    serve_create(&server).await;
    serve_measurement(&server, ID, 200, &json_of(TLS_403)).await;
    let settings = shared(Default::default());

    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;

    assert_eq!(vote.vote, Vote::Clear);
    assert_eq!(vote.evidence["measurement_id"], ID);
    assert_eq!(vote.evidence["probes_ok"], 20);
    let sent = requests(&server).await;
    let order: Vec<(String, String)> = sent
        .iter()
        .map(|r| (r.method.to_string(), r.url.path().to_owned()))
        .collect();
    assert_eq!(
        order,
        [
            ("GET".to_owned(), "/v1/limits".to_owned()),
            ("POST".to_owned(), "/v1/measurements".to_owned()),
            ("GET".to_owned(), format!("/v1/measurements/{ID}")),
        ],
        "the free limits call comes before the measurement that costs"
    );
    // What was asked for: Russian eyeball networks, HTTPS HEAD to the address with the SNI as
    // the host, and nothing about the token in the body.
    let body: Value = serde_json::from_slice(&sent[1].body).expect("json body");
    assert_eq!(
        body,
        json!({
            "type": "http",
            "target": "104.16.132.229",
            "locations": [{"country": "RU", "tags": ["eyeball-network"], "limit": 20}],
            "measurementOptions": {
                "port": 443,
                "protocol": "HTTPS",
                "request": {"method": "HEAD", "host": SNI, "path": "/"}
            }
        })
    );
    for request in &sent {
        let header = request.headers.get("authorization").expect("authorization");
        assert_eq!(
            header.to_str().expect("ascii"),
            format!("Bearer {TEST_TOKEN}")
        );
    }
    let evidence = vote.evidence.to_string();
    assert!(!evidence.contains(TEST_TOKEN), "the token is not evidence");
    assert!(!evidence.to_lowercase().contains("authorization"));
}

#[tokio::test]
async fn credits_off_remaining_19_abstains_no_create() {
    let server = MockServer::start().await;
    serve_limits(&server, 19).await;
    serve_create(&server).await;
    serve_measurement(&server, ID, 200, &json_of(TLS_403)).await;
    let settings = shared(GlobalpingSettings {
        allow_credits: false,
        ..Default::default()
    });

    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;

    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::RateLimited));
    assert_eq!(vote.evidence["reason"], "rate_limited");
    let sent = requests(&server).await;
    assert_eq!(creates(&sent), 0, "no measurement was created");
    assert_eq!(sent.len(), 1, "only the limits were read");

    // 20 is not below the minimum.
    let server = MockServer::start().await;
    serve_limits(&server, 20).await;
    serve_create(&server).await;
    serve_measurement(&server, ID, 200, &json_of(TLS_403)).await;
    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;
    assert_eq!(vote.vote, Vote::Clear);
}

#[tokio::test]
async fn credits_on_allows_measure() {
    let server = MockServer::start().await;
    serve_limits(&server, 19).await;
    serve_create(&server).await;
    serve_measurement(&server, ID, 200, &json_of(TLS_403)).await;
    let settings = shared(GlobalpingSettings {
        allow_credits: true,
        ..Default::default()
    });

    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;

    assert_eq!(vote.vote, Vote::Clear);
    assert_eq!(creates(&requests(&server).await), 1);
}

#[tokio::test]
async fn the_sensor_reads_its_settings_on_every_call() {
    let server = MockServer::start().await;
    serve_limits(&server, 19).await;
    serve_create(&server).await;
    serve_measurement(&server, ID, 200, &json_of(TLS_403)).await;
    let settings = shared(Default::default());
    let sensor = sensor(&server, Some(TEST_TOKEN), &settings);

    assert_eq!(
        sensor.vote(&target()).await.vote,
        Vote::Abstain(AbstainReason::RateLimited)
    );
    settings.store(Arc::new(GlobalpingSettings {
        allow_credits: true,
        probe_limit: 12,
        ..Default::default()
    }));
    assert_eq!(sensor.vote(&target()).await.vote, Vote::Clear);

    let sent = requests(&server).await;
    let post = sent
        .iter()
        .find(|r| r.method.as_str() == "POST")
        .expect("a create");
    let body: Value = serde_json::from_slice(&post.body).expect("json");
    assert_eq!(body["locations"][0]["limit"], 12);
}

#[tokio::test]
async fn http_429_abstains() {
    // On the create.
    let server = MockServer::start().await;
    serve_limits(&server, 500).await;
    Mock::given(method("POST"))
        .and(path("/v1/measurements"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "60"))
        .mount(&server)
        .await;
    let settings = shared(Default::default());
    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::RateLimited));
    assert_eq!(vote.evidence["http_status"], 429);

    // On the limits.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/limits"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::RateLimited));
    assert_eq!(creates(&requests(&server).await), 0);

    // On a poll.
    let server = MockServer::start().await;
    serve_limits(&server, 500).await;
    serve_create(&server).await;
    serve_measurement(&server, ID, 429, &json!({})).await;
    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::RateLimited));
    assert_eq!(vote.evidence["measurement_id"], ID);
}

#[tokio::test]
async fn without_a_token_the_sensor_abstains_and_sends_nothing() {
    let server = MockServer::start().await;
    let settings = shared(Default::default());
    let vote = sensor(&server, None, &settings).vote(&target()).await;
    assert_eq!(vote.vote, Vote::Abstain(AbstainReason::NotConfigured));
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn the_sensor_has_the_globalping_kind() {
    let server = MockServer::start().await;
    let settings = shared(Default::default());
    assert_eq!(
        sensor(&server, None, &settings).kind(),
        SensorKind::Globalping
    );
}

#[tokio::test]
async fn other_failures_of_the_central_measurement_are_abstentions() {
    let settings = shared(Default::default());

    // Limits down.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/limits"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;
    assert_eq!(abstained(&vote), AbstainReason::Unavailable);
    assert_eq!(creates(&requests(&server).await), 0);

    // Limits that say nothing usable.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/limits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"rateLimit": {}})))
        .mount(&server)
        .await;
    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;
    assert_eq!(abstained(&vote), AbstainReason::UnknownAnswer);
    assert_eq!(creates(&requests(&server).await), 0);

    // The create is refused (a 4xx that is not 429), or fails.
    for (status, reason) in [
        (422, AbstainReason::UnknownAnswer),
        (401, AbstainReason::UnknownAnswer),
        (500, AbstainReason::Unavailable),
    ] {
        let server = MockServer::start().await;
        serve_limits(&server, 500).await;
        Mock::given(method("POST"))
            .and(path("/v1/measurements"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let vote = sensor(&server, Some(TEST_TOKEN), &settings)
            .vote(&target())
            .await;
        assert_eq!(abstained(&vote), reason, "status {status}");
        assert_eq!(vote.evidence["http_status"], status);
    }

    // Created, never finished.
    let server = MockServer::start().await;
    serve_limits(&server, 500).await;
    serve_create(&server).await;
    serve_measurement(&server, ID, 200, &json_of(IN_PROGRESS)).await;
    let sensor =
        sensor(&server, Some(TEST_TOKEN), &settings).with_budget(Duration::from_millis(150));
    let vote = sensor.vote(&target()).await;
    assert_eq!(abstained(&vote), AbstainReason::Timeout);
    assert_eq!(vote.evidence["measurement_id"], ID);
}

#[tokio::test]
async fn the_central_measurement_votes_blocked_when_nothing_gets_through() {
    let server = MockServer::start().await;
    serve_limits(&server, 500).await;
    serve_create(&server).await;
    serve_measurement(&server, ID, 200, &json_of(NO_TLS)).await;
    let settings = shared(Default::default());
    let vote = sensor(&server, Some(TEST_TOKEN), &settings)
        .vote(&target())
        .await;
    assert_eq!(vote.vote, Vote::Blocked);
    assert_eq!(vote.evidence["probes_ok"], 0);
}

#[tokio::test]
async fn the_limits_shapes_with_and_without_a_token_both_parse() {
    for (fixture, remaining) in [(LIMITS_ANON, 250), (LIMITS_TOKEN, 500)] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/limits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json_of(fixture)))
            .mount(&server)
            .await;
        let limits = client(&server, None).limits().await.expect("limits");
        assert_eq!(limits.create_remaining, remaining);
    }
}

#[test]
fn the_target_does_not_show_the_secret() {
    let shown = format!("{:?}", target());
    assert!(!shown.contains("ee0011"));
    assert!(shown.contains("104.16.132.229"));
}

/// The recording of a closed port, served under the id the tests ask for.
fn refused(change: impl FnOnce(&mut Vec<Value>)) -> Value {
    let mut document = changed(REFUSED, change);
    document["id"] = json!(ID);
    document
}

#[tokio::test]
async fn refused_connections_are_a_closed_port_not_a_block() {
    let server = MockServer::start().await;
    let document = refused(|_| {});
    serve_measurement(&server, ID, 200, &document).await;

    let vote = verified(&server, &expect_for(&document, 443, 5_000))
        .await
        .expect("a measurement made against this node");

    assert_eq!(abstained(&vote), AbstainReason::PortRefused);
    assert_eq!(vote.evidence["reason"], "port_refused");
    assert_eq!(vote.evidence["probes_with_result"], 20);
    assert_eq!(vote.evidence["probes_refused"], 20);
}

#[tokio::test]
async fn a_few_refusals_do_not_hide_a_block() {
    // 10 of 20 refused: not more than half. The other failures still count against the node.
    let server = MockServer::start().await;
    let document = refused(|results| {
        for probe in results.iter_mut().take(10) {
            probe["result"] = failed_probe();
        }
    });
    serve_measurement(&server, ID, 200, &document).await;

    let vote = verified(&server, &expect_for(&document, 443, 5_000))
        .await
        .expect("accepted");

    assert_eq!(vote.vote, Vote::Blocked);
    assert_eq!(vote.evidence["probes_refused"], 10);
}
