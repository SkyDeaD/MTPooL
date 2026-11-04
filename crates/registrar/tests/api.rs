//! The public read-only API: lists from the snapshot, bans and statistics from the database,
//! QR codes, errors, the request limit and the OpenAPI document.

mod common;

use std::collections::{BTreeSet, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use common::*;
use mtpool_core::{
    Assignments, DnsState, Event, EventKind, Generation, PersistOp, PoolState, RecordValue,
    RoundVerdict, SensorKind, SensorVote, Settings, Slot, Ts, Vote, build_proxy_link,
};
use mtpool_protocol::problem::Problem;
use mtpool_protocol::rfc3339;
use mtpool_registrar::actor::{Actor, ActorHandle, ActorOptions, Clock};
use mtpool_registrar::server::{AppState, router};
use mtpool_registrar::store::{HistoryOp, Store};
use serde_json::{Value, json};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000_000;
const SECRET_A: &str = "00112233445566778899aabbccddeeff";
const SECRET_B: &str = "ffeeddccbbaa99887766554433221100";

struct Env {
    app: Router,
    store: Store,
    actor: ActorHandle,
    clock: Arc<AtomicI64>,
    _dir: tempfile::TempDir,
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    fn etag(&self) -> String {
        self.headers
            .get(header::ETAG)
            .expect("ETag header")
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn content_type(&self) -> String {
        self.headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    }
}

fn settings() -> Settings {
    Settings {
        domains: vec!["proxy.example.com".into(), "edge.example.com".into()],
        tls_domain: "tls.example.com".into(),
        proxy_users: vec![
            mtpool_core::ProxyUser {
                name: "alice".into(),
                secret_hex: SECRET_A.into(),
            },
            mtpool_core::ProxyUser {
                name: "bob".into(),
                secret_hex: SECRET_B.into(),
            },
        ],
        ..Settings::default()
    }
}

fn node(name: &str, ip: [u8; 4], registered_at: i64) -> mtpool_core::NodeRecord {
    let mut n = healthy_node(name, Ts(NOW), 0);
    n.ip = Ipv4Addr::from(ip);
    n.registered_at = Ts(registered_at);
    n.state_since = Ts(registered_at + 1_000);
    n
}

fn alpha() -> mtpool_core::NodeRecord {
    node("alpha-00001", [203, 0, 113, 7], NOW - 20_000_000)
}

fn beta() -> mtpool_core::NodeRecord {
    node("beta-00002", [198, 51, 100, 9], NOW - 10_000_000)
}

/// Two nodes, alpha master of `proxy.example.com`, DNS for both domains, two proxy users.
fn pool() -> PoolState {
    let mut state = PoolState::new(settings());
    for n in [alpha(), beta()] {
        state.nodes.insert(n.id.clone(), n);
    }
    state.assignments = Assignments(
        [(
            "proxy.example.com".to_owned(),
            vec![Slot {
                node: node_id("alpha-00001"),
                since: Ts(NOW - 1_000_000),
            }],
        )]
        .into(),
    );
    let dns = |value| DnsState {
        desired: value,
        applied: None,
        generation: Generation(1),
        retries: 0,
        next_try: Ts(0),
        in_flight: None,
    };
    state.dns.insert(
        "proxy.example.com".into(),
        dns(RecordValue::A(vec![Ipv4Addr::new(203, 0, 113, 7)])),
    );
    state.dns.insert(
        "edge.example.com".into(),
        dns(RecordValue::Cname("proxy.example.com".into())),
    );
    state
}

fn env(state: PoolState) -> Env {
    let (dir, store) = open_store();
    let clock = Arc::new(AtomicI64::new(NOW));
    let shared = Arc::clone(&clock);
    let clock_fn: Clock = Arc::new(move || Ts(shared.load(Ordering::SeqCst)));
    let actor = Actor::spawn_with(
        state,
        store.clone(),
        vec![],
        ActorOptions {
            clock: Arc::clone(&clock_fn),
            ..ActorOptions::default()
        },
    );
    let app = router(AppState::with_clock(
        store.clone(),
        actor.clone(),
        actor.snapshots(),
        clock_fn,
    ));
    Env {
        app,
        store,
        actor,
        clock,
        _dir: dir,
    }
}

async fn send(env: &Env, request: Request<Body>) -> Resp {
    let response = env.app.clone().oneshot(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.expect("body");
    Resp {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    }
}

async fn get(env: &Env, path: &str) -> Resp {
    send(env, request(Method::GET, path)).await
}

fn request(method: Method, path: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap()
}

/// A GET that arrives from `peer`, optionally with an `X-Forwarded-For` header.
fn request_from(path: &str, peer: &str, forwarded: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    if let Some(value) = forwarded {
        builder = builder.header("x-forwarded-for", value);
    }
    let mut req = builder.body(Body::empty()).unwrap();
    let addr: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

fn conditional(path: &str, if_none_match: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header(header::IF_NONE_MATCH, if_none_match)
        .body(Body::empty())
        .unwrap()
}

fn assert_problem(resp: &Resp, status: StatusCode, code: &str) {
    assert_eq!(resp.status, status, "{}", resp.body);
    assert!(
        resp.content_type().starts_with("application/problem+json"),
        "content type {:?}",
        resp.content_type()
    );
    let problem: Problem = serde_json::from_str(&resp.body).expect("a Problem document");
    assert_eq!(problem.kind, "about:blank");
    assert_eq!(problem.status, status.as_u16());
    assert_eq!(problem.code, code, "{}", resp.body);
    assert!(!problem.title.is_empty(), "{}", resp.body);
    assert!(!problem.detail.is_empty(), "{}", resp.body);
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

fn ban(
    store: &Store,
    node: &str,
    ip: [u8; 4],
    at: i64,
    votes: &[(SensorKind, Vote)],
    reverify: bool,
) {
    store.send(PersistOp::RecordBan {
        ip: Ipv4Addr::from(ip),
        node: node_id(node),
        at: Ts(at),
        votes: votes.to_vec(),
        reverify,
    });
}

fn traffic(store: &Store, node: &str, at: i64, from_client: u64, to_client: u64) {
    store.send(PersistOp::Traffic {
        node: node_id(node),
        at: Ts(at),
        from_client,
        to_client,
    });
}

/// The history the statistics tests share:
///
/// ```text
/// id  at (s before now)  node   reverify  lifetime  gap
///  1  100000             gamma  no        none      none      (gamma is not in the database)
///  2  5000               alpha  no        15000     95000
///  3  3000               beta   no        7000      2000
///  4  2000               beta   yes       none      none
/// ```
fn seed_history(env: &Env) {
    use SensorKind::{Cheburcheck, Globalping, RedFlag};
    env.store.send(PersistOp::UpsertNode(alpha()));
    env.store.send(PersistOp::UpsertNode(beta()));
    ban(
        &env.store,
        "gamma-00003",
        [192, 0, 2, 33],
        NOW - 100_000_000,
        &[(Globalping, Vote::Blocked), (RedFlag, Vote::Blocked)],
        false,
    );
    ban(
        &env.store,
        "alpha-00001",
        [203, 0, 113, 7],
        NOW - 5_000_000,
        &[(Globalping, Vote::Blocked), (Cheburcheck, Vote::Blocked)],
        false,
    );
    ban(
        &env.store,
        "beta-00002",
        [198, 51, 100, 9],
        NOW - 3_000_000,
        &[(Globalping, Vote::Blocked), (RedFlag, Vote::Blocked)],
        false,
    );
    ban(
        &env.store,
        "beta-00002",
        [198, 51, 100, 9],
        NOW - 2_000_000,
        &[(Globalping, Vote::Blocked), (Cheburcheck, Vote::Clear)],
        true,
    );
    traffic(&env.store, "alpha-00001", NOW - 100_000_000, 50, 150);
    traffic(&env.store, "alpha-00001", NOW - 5_000_000, 1000, 4000);
    traffic(&env.store, "beta-00002", NOW - 3_000_000, 200, 800);
    env.store.flush().expect("history written");
}

// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn nodes_mask_ip_and_hide_measurement_id() {
    use SensorKind::{Globalping, RedFlag};
    let env = env(pool());
    env.store.send(PersistOp::UpsertNode(alpha()));
    env.store.send(PersistOp::RecordRound {
        node: node_id("alpha-00001"),
        generation: Generation(1),
        at: Ts(NOW - 60_000),
        verdict: RoundVerdict::Confirmed,
        votes: vec![
            (
                RedFlag,
                SensorVote {
                    vote: Vote::Abstain(mtpool_core::AbstainReason::Timeout),
                    evidence: json!({"measurement_id": "m-secret-1", "raw": "203.0.113.7"}),
                },
            ),
            (
                Globalping,
                SensorVote {
                    vote: Vote::Blocked,
                    evidence: json!({"measurement_id": "m-secret-2", "probes": 12}),
                },
            ),
        ],
    });
    env.store.flush().unwrap();

    let list = get(&env, "/api/v1/nodes").await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.body);
    assert_eq!(list.content_type(), "application/json");
    let nodes = list.json();
    let nodes = nodes.as_array().expect("an array");
    assert_eq!(nodes.len(), 2);
    for forbidden in [
        "203.0.113.7",
        "198.51.100.9",
        "m-secret",
        "measurement",
        "evidence",
    ] {
        assert!(
            !list.body.contains(forbidden),
            "{forbidden} in {}",
            list.body
        );
    }
    let a = &nodes[0];
    assert_eq!(
        keys(a),
        set(&[
            "id",
            "state",
            "engine",
            "ip",
            "port",
            "registered_at",
            "state_since",
            "is_master",
            "domains"
        ]),
        "no last_round in the list"
    );
    assert_eq!(a["id"], "alpha-00001");
    assert_eq!(a["state"], "healthy");
    assert_eq!(a["engine"], "telemt");
    assert_eq!(a["ip"], "203.x.x.7");
    assert_eq!(a["port"], 443);
    assert_eq!(a["registered_at"], rfc3339(NOW - 20_000_000));
    assert_eq!(a["state_since"], rfc3339(NOW - 20_000_000 + 1_000));
    assert_eq!(a["is_master"], true);
    assert_eq!(a["domains"], json!(["proxy.example.com"]));
    assert_eq!(nodes[1]["id"], "beta-00002");
    assert_eq!(nodes[1]["ip"], "198.x.x.9");
    assert_eq!(nodes[1]["is_master"], false);
    assert_eq!(nodes[1]["domains"], json!([]));

    let one = get(&env, "/api/v1/nodes/alpha-00001").await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.body);
    for forbidden in ["203.0.113.7", "m-secret", "measurement", "evidence", "raw"] {
        assert!(!one.body.contains(forbidden), "{forbidden} in {}", one.body);
    }
    let one = one.json();
    assert_eq!(one["id"], "alpha-00001");
    assert_eq!(one["ip"], "203.x.x.7");
    assert_eq!(
        one["last_round"],
        json!({
            "at": rfc3339(NOW - 60_000),
            "verdict": "confirmed",
            "votes": [
                {"sensor": "globalping", "vote": "blocked"},
                {"sensor": "red_flag", "vote": "abstain", "reason": "timeout"},
            ],
        })
    );

    let quiet = get(&env, "/api/v1/nodes/beta-00002").await.json();
    assert!(quiet.get("last_round").is_none(), "no rounds, no field");
}

#[tokio::test]
async fn etag_304() {
    let env = env(pool());
    for path in ["/api/v1/nodes", "/api/v1/domains", "/api/v1/links"] {
        let first = get(&env, path).await;
        assert_eq!(first.status, StatusCode::OK, "{path}");
        let etag = first.etag();
        assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");

        for matching in [
            etag.clone(),
            format!("W/{etag}"),
            format!("\"other\", {etag}"),
            "*".to_owned(),
        ] {
            let again = send(&env, conditional(path, &matching)).await;
            assert_eq!(again.status, StatusCode::NOT_MODIFIED, "{path} {matching}");
            assert!(again.body.is_empty());
            assert_eq!(again.etag(), etag);
        }
        let stale = send(&env, conditional(path, "\"0000000000000000\"")).await;
        assert_eq!(stale.status, StatusCode::OK, "{path}");
        assert_eq!(stale.body, first.body);
    }

    // A change of the pool changes the tag, and the old tag no longer matches.
    let before = get(&env, "/api/v1/nodes").await;
    env.actor
        .call(enroll("gamma-00003", [45, 33, 9, 1]))
        .await
        .unwrap();
    let changed = send(&env, conditional("/api/v1/nodes", &before.etag())).await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    assert_ne!(changed.etag(), before.etag());
    assert!(changed.body.contains("gamma-00003"));
}

#[tokio::test]
async fn rate_limit_429_with_retry_after() {
    let env = env(pool());
    for i in 0..120 {
        let resp = get(&env, "/api/v1/domains").await;
        assert_eq!(resp.status, StatusCode::OK, "request {i}");
    }
    let limited = get(&env, "/api/v1/domains").await;
    assert_problem(&limited, StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    assert_eq!(
        limited.headers.get(header::RETRY_AFTER).unwrap(),
        "60",
        "the whole window is left"
    );
    // The limit covers every public route, not the probes.
    for path in ["/api/v1/nodes", "/api/v1/bans", "/api/v1/stats/summary"] {
        assert_eq!(get(&env, path).await.status, StatusCode::TOO_MANY_REQUESTS);
    }
    assert_eq!(get(&env, "/healthz").await.status, StatusCode::OK);
    assert_eq!(get(&env, "/readyz").await.status, StatusCode::OK);

    // The wait shrinks with the clock.
    env.clock.store(NOW + 45_000, Ordering::SeqCst);
    let limited = get(&env, "/api/v1/domains").await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers.get(header::RETRY_AFTER).unwrap(), "15");

    // Another client has its own count: the proxy (loopback peer) names it in X-Forwarded-For.
    let other = send(
        &env,
        request_from("/api/v1/domains", "127.0.0.1:5000", Some("203.0.113.50")),
    )
    .await;
    assert_eq!(other.status, StatusCode::OK);

    // A new window starts when the old one ends.
    env.clock.store(NOW + 60_000, Ordering::SeqCst);
    assert_eq!(get(&env, "/api/v1/domains").await.status, StatusCode::OK);
}

#[tokio::test]
async fn errors_are_problem_json_with_code() {
    let env = env(pool());
    let cases: [(Method, &str, StatusCode, &str); 10] = [
        (
            Method::GET,
            "/api/v1/nope",
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (Method::GET, "/nope", StatusCode::NOT_FOUND, "not_found"),
        (
            Method::POST,
            "/api/v1/nodes",
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        ),
        (
            Method::GET,
            "/api/v1/nodes/missing-00000",
            StatusCode::NOT_FOUND,
            "not_found",
        ),
        (
            Method::GET,
            "/api/v1/stats/summary?range=1y",
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            Method::GET,
            "/api/v1/stats/series?range=",
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            Method::GET,
            "/api/v1/stats/clients?range=30d",
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            Method::GET,
            "/api/v1/bans?limit=abc",
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            Method::GET,
            "/api/v1/bans?limit=0",
            StatusCode::BAD_REQUEST,
            "bad_request",
        ),
        (
            Method::GET,
            "/api/v1/admin/session",
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        ),
    ];
    for (method, path, status, code) in cases {
        let resp = send(&env, request(method.clone(), path)).await;
        assert_problem(&resp, status, code);
        eprintln!("{method} {path}: {}", resp.body);
    }
}

#[tokio::test]
async fn unknown_query_ignored_unknown_id_404() {
    let env = env(pool());
    seed_history(&env);
    for path in [
        "/api/v1/nodes",
        "/api/v1/nodes/alpha-00001",
        "/api/v1/domains",
        "/api/v1/links",
        "/api/v1/bans",
        "/api/v1/stats/summary",
        "/api/v1/stats/series?range=7d",
        "/api/v1/stats/clients?range=1h",
    ] {
        let plain = get(&env, path).await;
        assert_eq!(plain.status, StatusCode::OK, "{path}: {}", plain.body);
        let sep = if path.contains('?') { '&' } else { '?' };
        let noisy = get(&env, &format!("{path}{sep}foo=bar&x=1&limit_typo=2")).await;
        assert_eq!(noisy.status, StatusCode::OK, "{path}");
        assert_eq!(noisy.body, plain.body, "{path}");
    }
    let flag_only = get(&env, "/api/v1/nodes?debug").await;
    assert_eq!(flag_only.status, StatusCode::OK);

    for path in [
        "/api/v1/nodes/nobody-00000",
        "/api/v1/nodes/Not%20An%20Id",
        "/api/v1/links/nobody@nowhere.example.com/qr.svg",
    ] {
        assert_problem(&get(&env, path).await, StatusCode::NOT_FOUND, "not_found");
    }
}

#[tokio::test]
async fn links_public_without_token() {
    let env = env(pool());
    let resp = get(&env, "/api/v1/links").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert_eq!(resp.content_type(), "application/json");
    let links = resp.json();
    let links = links.as_array().expect("array");
    assert_eq!(links.len(), 4, "two users on two domains");
    let first = &links[0];
    assert_eq!(
        keys(first),
        set(&["id", "domain", "user", "link", "qr_url"])
    );
    assert_eq!(first["id"], "alice@proxy.example.com");
    assert_eq!(first["domain"], "proxy.example.com");
    assert_eq!(first["user"], "alice");
    assert_eq!(
        first["link"],
        build_proxy_link("proxy.example.com", 443, SECRET_A, "tls.example.com")
    );
    assert_eq!(
        first["qr_url"],
        "/api/v1/links/alice@proxy.example.com/qr.svg"
    );
    let ids: Vec<&str> = links.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "alice@proxy.example.com",
            "bob@proxy.example.com",
            "alice@edge.example.com",
            "bob@edge.example.com"
        ]
    );

    let domains = get(&env, "/api/v1/domains").await;
    assert_eq!(domains.status, StatusCode::OK);
    assert_eq!(
        domains.json(),
        json!([
            {
                "domain": "proxy.example.com",
                "masters": ["alpha-00001"],
                "dns": {"type": "A", "values": ["203.x.x.7"]},
            },
            {
                "domain": "edge.example.com",
                "masters": [],
                "dns": {"type": "CNAME", "values": ["proxy.example.com"]},
            },
        ])
    );
}

/// The black modules of an SVG made by `fast_qr`: its `M<x>,<y>` path commands.
fn svg_modules(svg: &str) -> (usize, HashSet<(usize, usize)>) {
    let view = svg
        .split("viewBox=\"0 0 ")
        .nth(1)
        .expect("viewBox")
        .split(' ')
        .next()
        .unwrap();
    let size: usize = view.parse().expect("integer size");
    let d = svg.split(" d=\"").nth(1).expect("path data");
    let d = d.split('"').next().unwrap();
    let mut black = HashSet::new();
    for part in d.split('M').skip(1) {
        let coords: String = part
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ',')
            .collect();
        let (x, y) = coords.split_once(',').expect("x,y");
        black.insert((x.parse().unwrap(), y.parse().unwrap()));
    }
    (size, black)
}

#[tokio::test]
async fn qr_svg_encodes_link() {
    let env = env(pool());
    let links = get(&env, "/api/v1/links").await;
    let entry = links
        .json()
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["id"] == "bob@edge.example.com")
        .cloned()
        .expect("bob on edge");
    let url = entry["qr_url"].as_str().unwrap();
    assert_eq!(url, "/api/v1/links/bob@edge.example.com/qr.svg");

    let qr = get(&env, url).await;
    assert_eq!(qr.status, StatusCode::OK, "{}", qr.body);
    assert_eq!(qr.content_type(), "image/svg+xml");
    assert_eq!(qr.etag(), links.etag(), "the tag of the links document");
    assert!(qr.body.starts_with("<svg"), "{}", &qr.body[..40]);
    assert_eq!(
        send(&env, conditional(url, &qr.etag())).await.status,
        StatusCode::NOT_MODIFIED
    );

    // Draw the modules and read them back with a QR decoder.
    let (size, black) = svg_modules(&qr.body);
    let scale = 6;
    let side = size * scale;
    let mut image = rqrr::PreparedImage::prepare_from_greyscale(side, side, |x, y| {
        if black.contains(&(x / scale, y / scale)) {
            0
        } else {
            255
        }
    });
    let grids = image.detect_grids();
    assert_eq!(grids.len(), 1, "one QR code");
    let (_, content) = grids[0].decode().expect("decodes");
    assert_eq!(content, entry["link"].as_str().unwrap());
    assert!(content.starts_with("tg://proxy?server=edge.example.com&port=443&secret=ee"));
}

#[tokio::test]
async fn empty_pool_returns_empty_lists_not_500() {
    let env = env(PoolState::default());
    for path in [
        "/api/v1/nodes",
        "/api/v1/domains",
        "/api/v1/links",
        "/api/v1/bans",
    ] {
        let resp = get(&env, path).await;
        assert_eq!(resp.status, StatusCode::OK, "{path}: {}", resp.body);
        assert_eq!(resp.json(), json!([]), "{path}");
    }
    assert_problem(
        &get(&env, "/api/v1/nodes/alpha-00001").await,
        StatusCode::NOT_FOUND,
        "not_found",
    );

    let zero_point = json!({
        "bans": 0,
        "avg_lifetime_sec": null,
        "avg_gap_sec": null,
        "traffic_ingress_bytes": 0,
        "traffic_egress_bytes": 0,
        "traffic_total_bytes": 0,
    });
    let summary = get(&env, "/api/v1/stats/summary").await;
    assert_eq!(summary.status, StatusCode::OK, "{}", summary.body);
    let summary = summary.json();
    assert_eq!(
        keys(&summary),
        set(&[
            "range",
            "now",
            "from",
            "step_sec",
            "kpi",
            "by_reason",
            "recent",
            "history_ok"
        ])
    );
    assert_eq!(summary["range"], "24h");
    assert_eq!(summary["now"], rfc3339(NOW));
    assert_eq!(summary["kpi"], zero_point);
    assert_eq!(summary["by_reason"], json!({}));
    assert_eq!(summary["recent"], json!([]));
    assert_eq!(summary["history_ok"], true);

    for (range, buckets, step) in [("24h", 25, 3600), ("7d", 29, 21600), ("30d", 31, 86400)] {
        let series = get(&env, &format!("/api/v1/stats/series?range={range}")).await;
        assert_eq!(series.status, StatusCode::OK, "{}", series.body);
        let series = series.json();
        assert_eq!(
            keys(&series),
            set(&["range", "now", "from", "step_sec", "buckets", "history_ok"])
        );
        assert_eq!(series["step_sec"], step, "{range}");
        let list = series["buckets"].as_array().unwrap();
        assert_eq!(list.len(), buckets, "{range}");
        for bucket in list {
            let mut expected = zero_point.clone();
            expected["start_ts"] = bucket["start_ts"].clone();
            assert_eq!(bucket, &expected);
        }
    }
}

#[tokio::test]
async fn openapi_json_matches_committed_file() {
    let generated = mtpool_registrar::api::openapi_pretty();
    let committed = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.json"),
    )
    .expect("docs/openapi.json exists; regenerate it with `mtpool-registrar openapi`");
    // The command prints the document and a newline.
    assert_eq!(
        committed,
        format!("{generated}\n"),
        "docs/openapi.json is stale: cargo run -p mtpool-registrar -- openapi > docs/openapi.json"
    );

    let doc: Value = serde_json::from_str(generated).expect("JSON");
    let paths = keys(&doc["paths"]);
    for path in [
        "/api/v1/nodes",
        "/api/v1/nodes/{id}",
        "/api/v1/domains",
        "/api/v1/links",
        "/api/v1/links/{id}/qr.svg",
        "/api/v1/bans",
        "/api/v1/stats/summary",
        "/api/v1/stats/series",
        "/api/v1/stats/clients",
        "/api/v1/openapi.json",
        "/api/v1/agent/enroll",
        "/api/v1/agent/heartbeat",
        "/api/v1/agent/report",
        "/api/v1/agent/retire",
        "/api/v1/agent/config",
    ] {
        assert!(paths.contains(path), "{path} is documented: {paths:?}");
    }
    let schemas = keys(&doc["components"]["schemas"]);
    for schema in [
        "Problem",
        "PublicNode",
        "PublicDomain",
        "ProxyLink",
        "BanRecord",
        "StatsSummary",
        "StatsSeries",
        "ClientsSeries",
        "LastRound",
        "EnrollRequest",
        "EnrollResponse",
        "ReportRequest",
        "AgentConfig",
        "Terminate",
    ] {
        assert!(
            schemas.contains(schema),
            "{schema} is a schema: {schemas:?}"
        );
    }

    // The running service serves the same document.
    let env = env(PoolState::default());
    let served = get(&env, "/api/v1/openapi.json").await;
    assert_eq!(served.status, StatusCode::OK, "{}", served.body);
    assert_eq!(served.content_type(), "application/json");
    assert_eq!(served.json(), doc);
}

// ----- the parts the brief's list does not name ---------------------------------------------

#[tokio::test]
async fn bans_newest_first_with_paging_and_fields() {
    let env = env(pool());
    seed_history(&env);
    env.store.send(PersistOp::LiftBan {
        ip: Ipv4Addr::new(203, 0, 113, 7),
        at: Ts(NOW - 1_000_000),
        admin: true,
    });
    env.store.flush().unwrap();

    let all = get(&env, "/api/v1/bans").await;
    assert_eq!(all.status, StatusCode::OK, "{}", all.body);
    assert!(!all.body.contains("203.0.113.7"), "{}", all.body);
    let all = all.json();
    let ids: Vec<i64> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [4, 3, 2, 1]);
    let by_id = |id: i64| {
        all.as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(
        keys(&by_id(2)),
        set(&[
            "id",
            "ip",
            "node_id",
            "at",
            "votes",
            "reverify",
            "lifted_at",
            "lifted_by_admin",
            "lifetime_sec",
            "gap_sec"
        ])
    );
    assert_eq!(
        by_id(2),
        json!({
            "id": 2,
            "ip": "203.x.x.7",
            "node_id": "alpha-00001",
            "at": rfc3339(NOW - 5_000_000),
            "votes": [
                {"sensor": "globalping", "vote": "blocked"},
                {"sensor": "cheburcheck", "vote": "blocked"},
            ],
            "reverify": false,
            "lifted_at": rfc3339(NOW - 1_000_000),
            "lifted_by_admin": true,
            "lifetime_sec": 15000,
            "gap_sec": 95000,
        })
    );
    let third = by_id(3);
    assert_eq!(third["lifetime_sec"], 7000);
    assert_eq!(third["gap_sec"], 2000);
    assert_eq!(third["lifted_at"], Value::Null);
    assert_eq!(third["lifted_by_admin"], false);
    let reverify = by_id(4);
    assert_eq!(reverify["reverify"], true);
    assert_eq!(reverify["lifetime_sec"], Value::Null);
    assert_eq!(reverify["gap_sec"], Value::Null);
    assert_eq!(
        reverify["votes"],
        json!([
            {"sensor": "globalping", "vote": "blocked"},
            {"sensor": "cheburcheck", "vote": "clear"},
        ])
    );
    let first = by_id(1);
    assert_eq!(first["lifetime_sec"], Value::Null, "its node is gone");
    assert_eq!(
        first["gap_sec"],
        Value::Null,
        "the first ban has no previous"
    );

    let page = |query: &'static str| {
        let env = &env;
        async move {
            let resp = get(env, &format!("/api/v1/bans{query}")).await;
            assert_eq!(resp.status, StatusCode::OK, "{query}: {}", resp.body);
            resp.json()
                .as_array()
                .unwrap()
                .iter()
                .map(|b| b["id"].as_i64().unwrap())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(page("?limit=2").await, [4, 3]);
    assert_eq!(page("?limit=2&before=3").await, [2, 1]);
    assert_eq!(page("?before=1").await, Vec::<i64>::new());
    assert_eq!(
        page("?limit=100000").await,
        [4, 3, 2, 1],
        "a big limit is clamped"
    );
}

#[tokio::test]
async fn stats_count_bans_traffic_and_reasons() {
    let env = env(pool());
    seed_history(&env);
    let from = rfc3339(1_699_912_800_000);

    let summary = get(&env, "/api/v1/stats/summary?range=24h").await;
    assert_eq!(summary.status, StatusCode::OK, "{}", summary.body);
    let summary = summary.json();
    assert_eq!(summary["range"], "24h");
    assert_eq!(summary["now"], rfc3339(NOW));
    assert_eq!(summary["from"], from);
    assert_eq!(summary["step_sec"], 3600);
    assert_eq!(
        summary["kpi"],
        json!({
            "bans": 2,
            "avg_lifetime_sec": 11000,
            "avg_gap_sec": 48500,
            "traffic_ingress_bytes": 1200,
            "traffic_egress_bytes": 4800,
            "traffic_total_bytes": 6000,
        })
    );
    assert_eq!(
        summary["by_reason"],
        json!({"cheburcheck+globalping": 1, "globalping+red_flag": 1})
    );
    let recent = summary["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 3, "the reverify entry is not a ban");
    assert_eq!(
        keys(&recent[0]),
        set(&["ts", "node_id", "ip_masked", "lifetime_sec", "gap_sec"])
    );
    assert_eq!(
        recent[0],
        json!({
            "ts": rfc3339(NOW - 3_000_000),
            "node_id": "beta-00002",
            "ip_masked": "198.x.x.9",
            "lifetime_sec": 7000,
            "gap_sec": 2000,
        })
    );
    assert_eq!(recent[1]["node_id"], "alpha-00001");
    assert_eq!(
        recent[2],
        json!({
            "ts": rfc3339(NOW - 100_000_000),
            "node_id": "gamma-00003",
            "ip_masked": "192.x.x.33",
            "lifetime_sec": null,
            "gap_sec": null,
        }),
        "the feed reaches back past the range"
    );
    assert_eq!(summary["history_ok"], true);

    let series = get(&env, "/api/v1/stats/series?range=24h").await.json();
    assert_eq!(series["from"], from);
    let buckets = series["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 25);
    assert_eq!(buckets[0]["start_ts"], from);
    assert_eq!(
        buckets[1]["start_ts"],
        rfc3339(1_699_912_800_000 + 3_600_000)
    );
    for (i, bucket) in buckets.iter().enumerate() {
        match i {
            22 => assert_eq!(
                bucket,
                &json!({
                    "start_ts": rfc3339(1_699_912_800_000 + 22 * 3_600_000),
                    "bans": 1,
                    "avg_lifetime_sec": 15000,
                    "avg_gap_sec": 95000,
                    "traffic_ingress_bytes": 1000,
                    "traffic_egress_bytes": 4000,
                    "traffic_total_bytes": 5000,
                })
            ),
            23 => assert_eq!(
                bucket,
                &json!({
                    "start_ts": rfc3339(1_699_912_800_000 + 23 * 3_600_000),
                    "bans": 1,
                    "avg_lifetime_sec": 7000,
                    "avg_gap_sec": 2000,
                    "traffic_ingress_bytes": 200,
                    "traffic_egress_bytes": 800,
                    "traffic_total_bytes": 1000,
                })
            ),
            _ => {
                assert_eq!(bucket["bans"], 0, "bucket {i}");
                assert_eq!(bucket["avg_lifetime_sec"], Value::Null, "bucket {i}");
                assert_eq!(bucket["traffic_total_bytes"], 0, "bucket {i}");
            }
        }
    }

    // The week also holds the old ban and the old traffic.
    let week = get(&env, "/api/v1/stats/summary?range=7d").await.json();
    assert_eq!(week["step_sec"], 21600);
    assert_eq!(week["kpi"]["bans"], 3);
    assert_eq!(week["kpi"]["traffic_ingress_bytes"], 1250);
    assert_eq!(week["kpi"]["traffic_egress_bytes"], 4950);
    assert_eq!(week["kpi"]["traffic_total_bytes"], 6200);
    let week_series = get(&env, "/api/v1/stats/series?range=7d").await.json();
    let sum = |field: &str| -> i64 {
        week_series["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b[field].as_i64().unwrap())
            .sum()
    };
    assert_eq!(week_series["buckets"].as_array().unwrap().len(), 29);
    assert_eq!(sum("bans"), 3);
    assert_eq!(sum("traffic_total_bytes"), 6200);
}

#[tokio::test]
async fn stats_are_cached_for_ten_seconds_with_their_own_etag() {
    let env = env(pool());
    seed_history(&env);
    let path = "/api/v1/stats/summary?range=24h";

    let first = get(&env, path).await;
    assert_eq!(first.json()["kpi"]["bans"], 2);
    assert_eq!(
        send(&env, conditional(path, &first.etag())).await.status,
        StatusCode::NOT_MODIFIED
    );

    ban(
        &env.store,
        "alpha-00001",
        [203, 0, 113, 7],
        NOW - 1_000_000,
        &[
            (SensorKind::Globalping, Vote::Blocked),
            (SensorKind::RedFlag, Vote::Blocked),
        ],
        false,
    );
    env.store.flush().unwrap();

    env.clock.store(NOW + 9_999, Ordering::SeqCst);
    let cached = get(&env, path).await;
    assert_eq!(cached.body, first.body, "still the cached figures");
    assert_eq!(cached.etag(), first.etag());

    env.clock.store(NOW + 10_000, Ordering::SeqCst);
    let fresh = get(&env, path).await;
    assert_eq!(fresh.json()["kpi"]["bans"], 3);
    assert_ne!(fresh.etag(), first.etag());
    assert_eq!(fresh.json()["now"], rfc3339(NOW + 10_000));

    // Another range has its own entry and the series its own.
    let series = get(&env, "/api/v1/stats/series?range=24h").await;
    assert_eq!(series.status, StatusCode::OK);
    assert_ne!(series.etag(), fresh.etag());
    let stale = send(&env, conditional(path, &first.etag())).await;
    assert_eq!(
        stale.status,
        StatusCode::OK,
        "the old tag no longer matches"
    );
}

// ---- client history ----------------------------------------------------------------------------

/// The start of the last full 5-minute step before `NOW`, in milliseconds.
fn step_start() -> i64 {
    NOW.div_euclid(300_000) * 300_000 - 300_000
}

fn samples(env: &Env, at: i64, total: u64, domains: &[(&str, u64)]) {
    let mut rows = vec![(String::new(), total)];
    rows.extend(domains.iter().map(|(d, n)| ((*d).to_owned(), *n)));
    env.store
        .send(HistoryOp::ClientSamples { at: Ts(at), rows });
}

fn event(env: &Env, kind: EventKind, at: i64, node: Option<&str>, detail: Value) {
    env.store.send(Event {
        kind,
        at: Ts(at),
        node: node.map(node_id),
        domain: Some("proxy.example.com".to_owned()),
        detail,
    });
}

#[tokio::test]
async fn invalid_range_is_400() {
    let env = env(pool());
    for path in [
        "/api/v1/stats/clients?range=1y",
        "/api/v1/stats/clients?range=30d",
        "/api/v1/stats/clients?range=",
        "/api/v1/stats/clients?range=1H",
    ] {
        assert_problem(
            &get(&env, path).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        );
    }
}

#[tokio::test]
async fn empty_table_is_200_with_no_points() {
    let env = env(pool());
    let resp = get(&env, "/api/v1/stats/clients").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    assert_eq!(body["range"], "24h", "24h is the default");
    assert_eq!(body["step_sec"], 300);
    assert_eq!(body["points"], json!([]));
    assert_eq!(body["marks"], json!([]));
}

#[tokio::test]
async fn range_steps_are_30_300_1800() {
    let env = env(pool());
    for (range, step) in [("10m", 30), ("1h", 30), ("24h", 300), ("7d", 1800)] {
        let body = get(&env, &format!("/api/v1/stats/clients?range={range}"))
            .await
            .json();
        assert_eq!(body["range"], range);
        assert_eq!(body["step_sec"], step, "{range}");
    }
}

#[tokio::test]
async fn points_average_over_their_step() {
    let env = env(pool());
    let at = step_start();
    samples(
        &env,
        at,
        10,
        &[("proxy.example.com", 4), ("edge.example.com", 1)],
    );
    samples(&env, at + 30_000, 21, &[("proxy.example.com", 7)]);
    // The next step has its own single sample.
    samples(&env, at + 300_000, 40, &[("proxy.example.com", 40)]);
    env.store.flush().unwrap();

    let body = get(&env, "/api/v1/stats/clients?range=24h").await.json();
    let points = body["points"].as_array().unwrap();
    assert_eq!(points.len(), 2, "{body}");
    assert_eq!(points[0]["at"], rfc3339(at));
    // 15.5 rounds to 16, 5.5 to 6; a domain is averaged over the samples that had it.
    assert_eq!(points[0]["total"], 16);
    assert_eq!(
        points[0]["domains"],
        json!({"edge.example.com": 1, "proxy.example.com": 6})
    );
    assert_eq!(points[1]["at"], rfc3339(at + 300_000));
    assert_eq!(points[1]["total"], 40);
}

#[tokio::test]
async fn gap_stays_a_gap() {
    let env = env(pool());
    let at = step_start();
    samples(&env, at - 900_000, 5, &[]);
    samples(&env, at, 9, &[]);
    env.store.flush().unwrap();

    let body = get(&env, "/api/v1/stats/clients?range=24h").await.json();
    let times: Vec<&str> = body["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["at"].as_str().unwrap())
        .collect();
    // Nothing between the two steps: no zeros, no interpolation.
    assert_eq!(times, [rfc3339(at - 900_000), rfc3339(at)]);
    assert_eq!(body["points"][0]["total"], 5);
    assert_eq!(body["points"][1]["total"], 9);
}

#[tokio::test]
async fn old_samples_are_outside_the_range() {
    let env = env(pool());
    samples(&env, NOW - 11 * 60_000, 1, &[]);
    samples(&env, NOW - 2 * 3_600_000, 2, &[]);
    samples(&env, NOW - 60_000, 3, &[]);
    env.store.flush().unwrap();

    let total = |body: &Value| -> Vec<i64> {
        body["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["total"].as_i64().unwrap())
            .collect()
    };
    let ten = get(&env, "/api/v1/stats/clients?range=10m").await.json();
    assert_eq!(total(&ten), [3]);
    let hour = get(&env, "/api/v1/stats/clients?range=1h").await.json();
    assert_eq!(total(&hour), [1, 3]);
    let day = get(&env, "/api/v1/stats/clients?range=24h").await.json();
    assert_eq!(total(&day), [2, 1, 3]);
}

#[tokio::test]
async fn marks_cover_master_change_fold_and_ban_and_nothing_else() {
    let env = env(pool());
    let at = NOW - 3_000_000;
    event(&env, EventKind::MasterChanged, at, None, json!({"slot": 0}));
    event(
        &env,
        EventKind::NodeBanned,
        at + 1_000,
        Some("alpha-00001"),
        json!({}),
    );
    event(
        &env,
        EventKind::SrmdAction,
        at + 2_000,
        None,
        json!({"action": "fold", "target": "edge.example.com"}),
    );
    // Not marks.
    event(
        &env,
        EventKind::SrmdAction,
        at + 3_000,
        None,
        json!({"action": "create"}),
    );
    event(
        &env,
        EventKind::SrmdAction,
        at + 4_000,
        None,
        json!({"action": "unfold"}),
    );
    event(
        &env,
        EventKind::NodeRegistered,
        at + 5_000,
        Some("beta-00002"),
        json!({}),
    );
    event(
        &env,
        EventKind::NodeQuarantined,
        at + 6_000,
        Some("beta-00002"),
        json!({}),
    );
    event(
        &env,
        EventKind::NodeDead,
        at + 7_000,
        Some("beta-00002"),
        json!({}),
    );
    event(
        &env,
        EventKind::BanLifted,
        at + 8_000,
        Some("beta-00002"),
        json!({}),
    );
    event(
        &env,
        EventKind::NodeReverifyFailed,
        at + 9_000,
        Some("beta-00002"),
        json!({}),
    );
    event(&env, EventKind::DnsApplied, at + 10_000, None, json!({}));
    event(&env, EventKind::SensorRound, at + 11_000, None, json!({}));
    // A mark outside the range is not shown.
    event(
        &env,
        EventKind::MasterChanged,
        NOW - 3 * 86_400_000,
        None,
        json!({}),
    );
    env.store.flush().unwrap();

    let body = get(&env, "/api/v1/stats/clients?range=24h").await.json();
    assert_eq!(
        body["marks"],
        json!([
            {"at": rfc3339(at), "kind": "master_change"},
            {"at": rfc3339(at + 1_000), "kind": "ban"},
            {"at": rfc3339(at + 2_000), "kind": "fold"},
        ])
    );
    let week = get(&env, "/api/v1/stats/clients?range=7d").await.json();
    assert_eq!(week["marks"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn answer_has_no_ip_or_node_name() {
    let env = env(pool());
    event(
        &env,
        EventKind::NodeBanned,
        NOW - 60_000,
        Some("alpha-00001"),
        json!({"ip": "203.0.113.7", "node": "alpha-00001"}),
    );
    event(
        &env,
        EventKind::SrmdAction,
        NOW - 50_000,
        Some("beta-00002"),
        json!({"action": "fold", "target": "198.51.100.9"}),
    );
    samples(&env, NOW - 60_000, 3, &[("proxy.example.com", 3)]);
    env.store.flush().unwrap();

    let resp = get(&env, "/api/v1/stats/clients?range=1h").await;
    assert_eq!(resp.status, StatusCode::OK);
    for secret in ["alpha-00001", "beta-00002", "203.0.113.7", "198.51.100.9"] {
        assert!(!resp.body.contains(secret), "{secret} in {}", resp.body);
    }
    let body = resp.json();
    for mark in body["marks"].as_array().unwrap() {
        let mut keys: Vec<&String> = mark.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["at", "kind"]);
    }
}

#[tokio::test]
async fn clients_etag_304() {
    let env = env(pool());
    samples(&env, NOW - 60_000, 3, &[]);
    env.store.flush().unwrap();
    let path = "/api/v1/stats/clients?range=1h";
    let first = get(&env, path).await;
    assert_eq!(first.status, StatusCode::OK);
    let etag = first.etag();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    let again = send(&env, conditional(path, &etag)).await;
    assert_eq!(again.status, StatusCode::NOT_MODIFIED);
    assert!(again.body.is_empty());
    let stale = send(&env, conditional(path, "\"0000000000000000\"")).await;
    assert_eq!(stale.status, StatusCode::OK);
    assert_eq!(stale.body, first.body);
}

#[tokio::test]
async fn clients_answer_is_cached_for_ten_seconds() {
    let env = env(pool());
    let path = "/api/v1/stats/clients?range=1h";
    let first = get(&env, path).await;
    samples(&env, NOW - 5_000, 8, &[]);
    env.store.flush().unwrap();
    assert_eq!(
        get(&env, path).await.body,
        first.body,
        "served from the cache"
    );
    env.clock.store(NOW + 10_001, Ordering::SeqCst);
    let later = get(&env, path).await;
    assert_ne!(later.body, first.body, "recomputed once the cache is old");
    assert_eq!(later.json()["points"][0]["total"], 8);
}
