//! The Cloudflare provider against a local server.
//!
//! The files in `tests/fixtures/cloudflare` keep the answer shapes of the real API, seen on
//! 2026-11-04 with every name, address and id replaced by a placeholder. `error_*.json` (except
//! `error_rate_limited.json`), `zones_none.json`, `records_none.json`, `create.json` and
//! `delete.json` are recordings; `zones_found.json` and the `records_page*.json` files are
//! rebuilt from the recorded shape; `error_rate_limited.json` is derived from the documented
//! envelope. Each of those carries a `_source` key; the parser ignores it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mtpool_core::RecordKind;
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

const ZONES_FOUND: &str = include_str!("../../../tests/fixtures/cloudflare/zones_found.json");
const ZONES_NONE: &str = include_str!("../../../tests/fixtures/cloudflare/zones_none.json");
const RECORDS_PAGE1: &str = include_str!("../../../tests/fixtures/cloudflare/records_page1.json");
const RECORDS_PAGE2: &str = include_str!("../../../tests/fixtures/cloudflare/records_page2.json");
const RECORDS_NONE: &str = include_str!("../../../tests/fixtures/cloudflare/records_none.json");
const CREATE: &str = include_str!("../../../tests/fixtures/cloudflare/create.json");
const DELETE: &str = include_str!("../../../tests/fixtures/cloudflare/delete.json");
const INVALID_TOKEN: &str =
    include_str!("../../../tests/fixtures/cloudflare/error_invalid_token.json");
const BAD_HEADER: &str = include_str!("../../../tests/fixtures/cloudflare/error_bad_header.json");
const ZONE_FORBIDDEN: &str =
    include_str!("../../../tests/fixtures/cloudflare/error_zone_forbidden.json");
const RATE_LIMITED: &str =
    include_str!("../../../tests/fixtures/cloudflare/error_rate_limited.json");

const TOKEN: &str = "test-token-not-a-real-credential";
const ZONE: &str = "023e105f4ecef8ad9ca31a8372d0c353";
const ZONES: &str = "/client/v4/zones";
const RECORDS: &str = "/client/v4/zones/023e105f4ecef8ad9ca31a8372d0c353/dns_records";

fn token() -> TokenSource {
    Arc::new(|| Some(TOKEN.to_owned()))
}

fn dns_for(server: &MockServer) -> CloudflareDns {
    CloudflareDns::new(Url::parse(&server.uri()).expect("uri"), token()).expect("client")
}

fn json_answer(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status)
        .insert_header("content-type", "application/json")
        .set_body_string(body)
}

async fn mount_zone(server: &MockServer, name: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path(ZONES))
        .and(query_param("name", name))
        .and(query_param("status", "active"))
        .respond_with(json_answer(200, body))
        .mount(server)
        .await;
}

fn a_record(name: &str, content: &str) -> NewRecord {
    NewRecord {
        name: name.to_owned(),
        kind: RecordKind::A,
        content: content.to_owned(),
    }
}

async fn paths_seen(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("recording is on")
        .iter()
        .map(|r| match r.url.query() {
            Some(q) => format!("{} {}?{q}", r.method, r.url.path()),
            None => format!("{} {}", r.method, r.url.path()),
        })
        .collect()
}

// ---- zone lookup ----

#[tokio::test]
async fn zone_resolved_from_domain_suffix() {
    let server = MockServer::start().await;
    mount_zone(&server, "a.b.example.com", ZONES_NONE).await;
    mount_zone(&server, "b.example.com", ZONES_NONE).await;
    mount_zone(&server, "example.com", ZONES_FOUND).await;
    let dns = dns_for(&server);

    let zone = dns.zone_id("a.b.example.com").await.expect("lookup");

    assert_eq!(zone.as_deref(), Some(ZONE));
    let seen = paths_seen(&server).await;
    assert_eq!(
        seen,
        [
            "GET /client/v4/zones?name=a.b.example.com&status=active",
            "GET /client/v4/zones?name=b.example.com&status=active",
            "GET /client/v4/zones?name=example.com&status=active",
        ],
        "the longest name first, and never the bare top-level domain"
    );
}

#[tokio::test]
async fn the_first_hit_wins() {
    let server = MockServer::start().await;
    mount_zone(&server, "b.example.com", ZONES_FOUND).await;
    let dns = dns_for(&server);
    assert_eq!(
        dns.zone_id("b.example.com")
            .await
            .expect("lookup")
            .as_deref(),
        Some(ZONE)
    );
    assert_eq!(paths_seen(&server).await.len(), 1);
}

#[tokio::test]
async fn a_found_zone_is_remembered() {
    let server = MockServer::start().await;
    mount_zone(&server, "p1.example.com", ZONES_NONE).await;
    mount_zone(&server, "example.com", ZONES_FOUND).await;
    let dns = dns_for(&server);
    dns.zone_id("p1.example.com").await.expect("first");
    let before = paths_seen(&server).await.len();

    let again = dns.zone_id("p1.example.com").await.expect("second");

    assert_eq!(again.as_deref(), Some(ZONE));
    assert_eq!(paths_seen(&server).await.len(), before, "no new request");
}

#[tokio::test]
async fn a_missing_zone_is_not_remembered() {
    let server = MockServer::start().await;
    mount_zone(&server, "example.com", ZONES_NONE).await;
    let dns = dns_for(&server);
    assert_eq!(dns.zone_id("example.com").await.expect("lookup"), None);
    assert_eq!(dns.zone_id("example.com").await.expect("lookup"), None);
    assert_eq!(paths_seen(&server).await.len(), 2, "asked again each time");
}

#[tokio::test]
async fn a_name_of_one_label_has_no_zone_and_costs_no_request() {
    let server = MockServer::start().await;
    let dns = dns_for(&server);
    assert_eq!(dns.zone_id("localhost").await.expect("lookup"), None);
    assert_eq!(dns.zone_id("").await.expect("lookup"), None);
    assert!(paths_seen(&server).await.is_empty());
}

#[tokio::test]
async fn the_domain_is_matched_in_lower_case_without_a_trailing_dot() {
    let server = MockServer::start().await;
    mount_zone(&server, "p1.example.com", ZONES_FOUND).await;
    let dns = dns_for(&server);
    assert_eq!(
        dns.zone_id("P1.Example.COM.")
            .await
            .expect("lookup")
            .as_deref(),
        Some(ZONE)
    );
}

// ---- records ----

#[tokio::test]
async fn pagination_handled() {
    let server = MockServer::start().await;
    for (page, body) in [("1", RECORDS_PAGE1), ("2", RECORDS_PAGE2)] {
        Mock::given(method("GET"))
            .and(path(RECORDS))
            .and(query_param("name", "p1.example.com"))
            .and(query_param("per_page", "100"))
            .and(query_param("page", page))
            .respond_with(json_answer(200, body))
            .mount(&server)
            .await;
    }
    let dns = dns_for(&server);

    let records = dns.list(ZONE, "p1.example.com").await.expect("list");

    let ids: Vec<&str> = records.iter().map(|r| r.live.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4"
        ]
    );
    assert_eq!(
        paths_seen(&server).await.len(),
        2,
        "two pages, two requests"
    );
}

#[tokio::test]
async fn only_the_exact_pool_comment_makes_a_record_managed() {
    let server = MockServer::start().await;
    for (page, body) in [("1", RECORDS_PAGE1), ("2", RECORDS_PAGE2)] {
        Mock::given(method("GET"))
            .and(path(RECORDS))
            .and(query_param("page", page))
            .respond_with(json_answer(200, body))
            .mount(&server)
            .await;
    }
    let dns = dns_for(&server);
    let records = dns.list(ZONE, "p1.example.com").await.expect("list");
    let managed: Vec<bool> = records.iter().map(|r| r.managed).collect();
    // `mtpool`, `mtpool`, no comment, another tool's comment.
    assert_eq!(managed, [true, true, false, false]);
}

#[tokio::test]
async fn a_record_is_read_with_its_kind_content_and_proxied_flag() {
    let server = MockServer::start().await;
    for (page, body) in [("1", RECORDS_PAGE1), ("2", RECORDS_PAGE2)] {
        Mock::given(method("GET"))
            .and(path(RECORDS))
            .and(query_param("page", page))
            .respond_with(json_answer(200, body))
            .mount(&server)
            .await;
    }
    let dns = dns_for(&server);
    let records = dns.list(ZONE, "p1.example.com").await.expect("list");
    let first = &records[0].live;
    assert_eq!(first.name, "p1.example.com");
    assert_eq!(first.kind, RecordKind::A);
    assert_eq!(first.content, "192.0.2.1");
    assert!(!first.proxied);
    assert_eq!(records[3].live.kind, RecordKind::Other("TXT".to_owned()));
}

#[tokio::test]
async fn a_name_without_records_lists_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(200, RECORDS_NONE))
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    assert!(
        dns.list(ZONE, "p1.example.com")
            .await
            .expect("list")
            .is_empty()
    );
    assert_eq!(paths_seen(&server).await.len(), 1);
}

#[tokio::test]
async fn a_list_that_never_ends_stops_at_the_page_cap() {
    let server = MockServer::start().await;
    let endless = RECORDS_PAGE1.replace("\"total_pages\": 2", "\"total_pages\": 100000");
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(200, &endless))
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    let error = dns.list(ZONE, "p1.example.com").await.expect_err("capped");
    assert!(matches!(error, DnsError::Unexpected(_)), "{error:?}");
    assert!(paths_seen(&server).await.len() <= MAX_PAGES);
}

#[tokio::test]
async fn create_sends_the_comment_ttl_and_proxied() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(RECORDS))
        .and(body_json(json!({
            "type": "A",
            "name": "p1.example.com",
            "content": "192.0.2.1",
            "ttl": 60,
            "proxied": false,
            "comment": "mtpool",
        })))
        .respond_with(json_answer(200, CREATE))
        .expect(1)
        .mount(&server)
        .await;
    let dns = dns_for(&server);

    let id = dns
        .create(ZONE, a_record("p1.example.com", "192.0.2.1"), 60, false)
        .await
        .expect("create");

    assert_eq!(id, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1");
}

#[tokio::test]
async fn a_cname_is_created_as_a_cname() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(RECORDS))
        .and(body_json(json!({
            "type": "CNAME",
            "name": "p2.example.com",
            "content": "p1.example.com",
            "ttl": 300,
            "proxied": true,
            "comment": "mtpool",
        })))
        .respond_with(json_answer(200, CREATE))
        .expect(1)
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    let record = NewRecord {
        name: "p2.example.com".to_owned(),
        kind: RecordKind::Cname,
        content: "p1.example.com".to_owned(),
    };
    dns.create(ZONE, record, 300, true).await.expect("create");
}

#[tokio::test]
async fn a_record_kind_the_pool_never_creates_is_refused_before_any_request() {
    let server = MockServer::start().await;
    let dns = dns_for(&server);
    let record = NewRecord {
        name: "p1.example.com".to_owned(),
        kind: RecordKind::Other("TXT".to_owned()),
        content: "x".to_owned(),
    };
    let error = dns
        .create(ZONE, record, 60, false)
        .await
        .expect_err("refused");
    assert!(matches!(error, DnsError::Unexpected(_)), "{error:?}");
    assert!(paths_seen(&server).await.is_empty());
}

#[tokio::test]
async fn delete_names_the_record_in_the_path() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("{RECORDS}/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1")))
        .respond_with(json_answer(200, DELETE))
        .expect(1)
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    dns.delete(ZONE, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1")
        .await
        .expect("delete");
}

#[tokio::test]
async fn deleting_a_record_that_is_gone_is_not_an_error() {
    let server = MockServer::start().await;
    let gone = json!({
        "success": false,
        "errors": [{"code": 81044, "message": "Record does not exist."}],
        "messages": [],
        "result": null,
    })
    .to_string();
    Mock::given(method("DELETE"))
        .respond_with(json_answer(404, &gone))
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    dns.delete(ZONE, "cccccccccccccccccccccccccccccccc")
        .await
        .expect("already gone");
}

#[tokio::test]
async fn an_id_that_is_not_plain_is_refused_before_any_request() {
    let server = MockServer::start().await;
    let dns = dns_for(&server);
    for bad in ["", "../zones", "a/b", "a?b"] {
        let error = dns.delete(ZONE, bad).await.expect_err("refused");
        assert!(matches!(error, DnsError::Unexpected(_)), "{bad}: {error:?}");
    }
    let error = dns
        .list("../x", "p1.example.com")
        .await
        .expect_err("a zone id with a slash");
    assert!(matches!(error, DnsError::Unexpected(_)), "{error:?}");
    assert!(paths_seen(&server).await.is_empty());
}

// ---- the token ----

#[tokio::test]
async fn the_token_goes_as_a_bearer_and_is_asked_for_on_every_call() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .and(header("authorization", "Bearer first"))
        .respond_with(json_answer(200, RECORDS_NONE))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .and(header("authorization", "Bearer second"))
        .respond_with(json_answer(200, RECORDS_NONE))
        .expect(1)
        .mount(&server)
        .await;
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let source: TokenSource = Arc::new(move || {
        Some(if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            "first".to_owned()
        } else {
            "second".to_owned()
        })
    });
    let dns = CloudflareDns::new(Url::parse(&server.uri()).expect("uri"), source).expect("client");
    dns.list(ZONE, "p1.example.com").await.expect("first call");
    dns.list(ZONE, "p1.example.com").await.expect("second call");
    assert_eq!(asked.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn without_a_token_nothing_is_sent() {
    let server = MockServer::start().await;
    let none: TokenSource = Arc::new(|| None);
    let dns = CloudflareDns::new(Url::parse(&server.uri()).expect("uri"), none).expect("client");
    assert_eq!(
        dns.zone_id("p1.example.com").await,
        Err(DnsError::NotConfigured)
    );
    assert_eq!(
        dns.list(ZONE, "p1.example.com").await,
        Err(DnsError::NotConfigured)
    );
    assert_eq!(
        dns.create(ZONE, a_record("p1.example.com", "192.0.2.1"), 60, false)
            .await,
        Err(DnsError::NotConfigured)
    );
    assert_eq!(dns.delete(ZONE, "x1").await, Err(DnsError::NotConfigured));
    assert!(paths_seen(&server).await.is_empty());
}

#[tokio::test]
async fn an_empty_token_is_no_token() {
    let server = MockServer::start().await;
    let empty: TokenSource = Arc::new(|| Some(String::new()));
    let dns = CloudflareDns::new(Url::parse(&server.uri()).expect("uri"), empty).expect("client");
    assert_eq!(
        dns.zone_id("p1.example.com").await,
        Err(DnsError::NotConfigured)
    );
    assert!(paths_seen(&server).await.is_empty());
}

// ---- errors ----

async fn list_answered_with(status: u16, body: &str) -> Result<Vec<ProviderRecord>, DnsError> {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(status, body))
        .mount(&server)
        .await;
    dns_for(&server).list(ZONE, "p1.example.com").await
}

#[tokio::test]
async fn a_refused_token_is_auth() {
    // A well-formed token that is not valid: 403 with code 9109.
    assert_eq!(
        list_answered_with(403, INVALID_TOKEN).await,
        Err(DnsError::Auth)
    );
    // A zone the token has no right to: 403 with code 10000.
    assert_eq!(
        list_answered_with(403, ZONE_FORBIDDEN).await,
        Err(DnsError::Auth)
    );
    assert_eq!(
        list_answered_with(401, INVALID_TOKEN).await,
        Err(DnsError::Auth)
    );
}

#[tokio::test]
async fn a_malformed_authorization_header_is_auth() {
    // 400 with code 6003 (chain 6111): what a token of the wrong format gets.
    assert_eq!(
        list_answered_with(400, BAD_HEADER).await,
        Err(DnsError::Auth)
    );
}

#[tokio::test]
async fn other_statuses_are_http_errors() {
    let nothing = "{}";
    assert_eq!(
        list_answered_with(500, nothing).await,
        Err(DnsError::Http(500))
    );
    assert_eq!(list_answered_with(502, "").await, Err(DnsError::Http(502)));
    assert_eq!(
        list_answered_with(400, nothing).await,
        Err(DnsError::Http(400))
    );
    assert_eq!(
        list_answered_with(418, nothing).await,
        Err(DnsError::Http(418))
    );
}

#[tokio::test]
async fn a_missing_zone_on_a_list_is_zone_not_found_and_is_forgotten() {
    let server = MockServer::start().await;
    mount_zone(&server, "p1.example.com", ZONES_FOUND).await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(404, "{}"))
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    let zone = dns.zone_id("p1.example.com").await.expect("zone");
    assert_eq!(
        dns.list(zone.as_deref().expect("found"), "p1.example.com")
            .await,
        Err(DnsError::ZoneNotFound)
    );
    // The remembered id is dropped: the next lookup goes to the service again.
    let before = paths_seen(&server).await.len();
    dns.zone_id("p1.example.com").await.expect("zone");
    assert_eq!(paths_seen(&server).await.len(), before + 1);
}

#[tokio::test]
async fn an_answer_that_is_not_json_is_unexpected() {
    let error = list_answered_with(200, "<html>oops</html>")
        .await
        .expect_err("bad");
    assert!(matches!(error, DnsError::Unexpected(_)), "{error:?}");
}

#[tokio::test]
async fn success_false_in_a_200_is_unexpected() {
    let body = json!({"success": false, "errors": [{"code": 1, "message": "no"}], "result": null})
        .to_string();
    let error = list_answered_with(200, &body).await.expect_err("bad");
    assert!(matches!(error, DnsError::Unexpected(_)), "{error:?}");
}

#[tokio::test]
async fn a_create_answer_without_an_id_is_unexpected() {
    let server = MockServer::start().await;
    let body = json!({"success": true, "errors": [], "messages": [], "result": {}}).to_string();
    Mock::given(method("POST"))
        .respond_with(json_answer(200, &body))
        .mount(&server)
        .await;
    let error = dns_for(&server)
        .create(ZONE, a_record("p1.example.com", "192.0.2.1"), 60, false)
        .await
        .expect_err("no id");
    assert!(matches!(error, DnsError::Unexpected(_)), "{error:?}");
}

#[tokio::test]
async fn an_answer_over_the_cap_is_refused() {
    let big = format!("{{\"pad\": \"{}\"}}", "x".repeat(3 * 1024 * 1024));
    let error = list_answered_with(200, &big).await.expect_err("too large");
    assert!(matches!(error, DnsError::Unexpected(_)), "{error:?}");
}

#[tokio::test]
async fn a_redirect_is_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "http://127.0.0.1:1/x"))
        .mount(&server)
        .await;
    let result = dns_for(&server).list(ZONE, "p1.example.com").await;
    assert_eq!(result, Err(DnsError::Http(302)));
}

#[tokio::test]
async fn an_unreachable_service_is_a_transport_error() {
    // Nothing listens on this port.
    let dns = CloudflareDns::new(Url::parse("http://127.0.0.1:1/").expect("url"), token())
        .expect("client");
    let error = dns.list(ZONE, "p1.example.com").await.expect_err("down");
    assert!(matches!(error, DnsError::Transport(_)), "{error:?}");
    assert!(
        !format!("{error:?}").contains(TOKEN),
        "the token never shows in an error"
    );
}

// ---- time ----

#[test]
fn request_timeout_15s() {
    assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(15));
}

#[tokio::test]
async fn a_request_that_never_answers_times_out_and_is_not_repeated() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(200, RECORDS_NONE).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let dns = dns_for(&server).with_timeout(Duration::from_millis(200));
    let started = std::time::Instant::now();
    let result = dns.list(ZONE, "p1.example.com").await;
    assert_eq!(result, Err(DnsError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(paths_seen(&server).await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_pacer_lets_three_through_and_holds_the_fourth_for_a_second() {
    let pacer = Pacer::new(3, Duration::from_secs(1));
    let start = Instant::now();
    for _ in 0..3 {
        pacer.acquire().await;
    }
    assert_eq!(start.elapsed(), Duration::ZERO, "three at once");
    pacer.acquire().await;
    assert_eq!(start.elapsed(), Duration::from_secs(1), "the fourth waits");
    for _ in 0..2 {
        pacer.acquire().await;
    }
    assert_eq!(start.elapsed(), Duration::from_secs(1), "two more fit");
    pacer.acquire().await;
    assert_eq!(start.elapsed(), Duration::from_secs(2));
}

#[tokio::test(start_paused = true)]
async fn the_pacer_is_fair_to_waiters_in_any_window() {
    let pacer = Arc::new(Pacer::new(3, Duration::from_secs(1)));
    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..9 {
        let pacer = Arc::clone(&pacer);
        tasks.push(tokio::spawn(async move {
            pacer.acquire().await;
            start.elapsed()
        }));
    }
    let mut at = Vec::new();
    for task in tasks {
        at.push(task.await.expect("task"));
    }
    at.sort();
    // Never more than three inside one second.
    for window in at.windows(4) {
        assert!(
            window[3] - window[0] >= Duration::from_secs(1),
            "{window:?}"
        );
    }
}

#[tokio::test]
async fn rate_limit_3_per_sec() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(200, RECORDS_NONE))
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    let started = std::time::Instant::now();
    for _ in 0..4 {
        dns.list(ZONE, "p1.example.com").await.expect("list");
    }
    // Three go out at once; the fourth waits for the first to leave the one-second window.
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "four calls took {:?}",
        started.elapsed()
    );
}

#[test]
fn the_default_pace_is_three_a_second() {
    assert_eq!(REQUESTS_PER_SECOND, 3);
}

#[tokio::test]
async fn http_429_honors_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(429, RATE_LIMITED).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(200, RECORDS_NONE))
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    let started = std::time::Instant::now();

    let records = dns.list(ZONE, "p1.example.com").await.expect("retried");

    assert!(records.is_empty());
    assert!(
        started.elapsed() >= Duration::from_millis(950),
        "waited the second"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(
        paths_seen(&server).await.len(),
        2,
        "one request and one retry"
    );
}

#[tokio::test]
async fn a_second_429_is_rate_limited_and_is_not_retried_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(json_answer(429, RATE_LIMITED).insert_header("retry-after", "1"))
        .mount(&server)
        .await;
    let dns = dns_for(&server);
    let result = dns.list(ZONE, "p1.example.com").await;
    assert_eq!(
        result,
        Err(DnsError::RateLimited {
            retry_after: Duration::from_secs(1)
        })
    );
    assert_eq!(paths_seen(&server).await.len(), 2);
}

#[test]
fn retry_after_takes_seconds_or_an_http_date() {
    let now = 1_700_000_000_000;
    assert_eq!(retry_after_delay(Some("7"), now), Duration::from_secs(7));
    assert_eq!(retry_after_delay(Some(" 7 "), now), Duration::from_secs(7));
    // 2023-11-14 22:13:25 GMT is 5 seconds after `now`.
    assert_eq!(
        retry_after_delay(Some("Tue, 14 Nov 2023 22:13:25 GMT"), now),
        Duration::from_secs(5)
    );
    // A date in the past is no wait.
    assert_eq!(
        retry_after_delay(Some("Tue, 14 Nov 2023 22:13:00 GMT"), now),
        Duration::ZERO
    );
}

#[test]
fn retry_after_defaults_to_ten_seconds_and_is_capped_at_a_minute() {
    let now = 1_700_000_000_000;
    assert_eq!(retry_after_delay(None, now), Duration::from_secs(10));
    assert_eq!(
        retry_after_delay(Some("soon"), now),
        Duration::from_secs(10)
    );
    assert_eq!(retry_after_delay(Some("61"), now), Duration::from_secs(60));
    assert_eq!(
        retry_after_delay(Some("86400"), now),
        Duration::from_secs(60)
    );
}
