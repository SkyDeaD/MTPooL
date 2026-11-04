//! The agent API: enrollment with the owner limit, heartbeats, reports with measurement
//! checks and the clock window, retirement and the node's config.

mod common;

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::*;
use mtpool_core::{
    Command, GpStatus, NodeRecord, NodeState, OwnerId, PoolState, ProxyUser, SensorKind,
    SensorVote, Settings, Tombstone, Ts, Vote,
};
use mtpool_protocol::problem::Problem;
use mtpool_protocol::rfc3339;
use mtpool_registrar::actor::{Actor, ActorHandle, ActorOptions, Clock};
use mtpool_registrar::auth::tokens::{self, NewToken, TokenHash, TokenKind};
use mtpool_registrar::server::{AppState, router};
use mtpool_registrar::store::Store;
use mtpool_registrar::verifier::{GpExpect, GpRejected, MeasurementVerifier, VerifyFuture};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;

const NOW: i64 = 1_800_000_000_000;
const SECRET: &str = "00112233445566778899aabbccddeeff";
const LIMIT: usize = 256 * 1024;
/// The address `healthy_node` gives its nodes.
const ALPHA_IP: &str = "45.33.1.5";
const PEER_ADMIN: &str = "198.51.100.7";

/// A verifier whose answer the test sets. With a gate it waits for `release` first.
struct FakeVerifier {
    answer: Mutex<Result<SensorVote, GpRejected>>,
    calls: Mutex<Vec<(String, GpExpect)>>,
    gate: Mutex<Option<Arc<Notify>>>,
}

impl FakeVerifier {
    fn new() -> Arc<FakeVerifier> {
        Arc::new(FakeVerifier {
            answer: Mutex::new(Err(GpRejected::Unavailable)),
            calls: Mutex::new(Vec::new()),
            gate: Mutex::new(None),
        })
    }

    fn answer(&self, answer: Result<SensorVote, GpRejected>) {
        *self.answer.lock().unwrap() = answer;
    }

    fn gate(&self) -> Arc<Notify> {
        let gate = Arc::new(Notify::new());
        *self.gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    fn calls(&self) -> Vec<(String, GpExpect)> {
        self.calls.lock().unwrap().clone()
    }
}

impl MeasurementVerifier for FakeVerifier {
    fn verify<'a>(&'a self, id: &'a str, expect: &'a GpExpect) -> VerifyFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((id.to_owned(), expect.clone()));
            let gate = self.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.notified().await;
            }
            self.answer.lock().unwrap().clone()
        })
    }
}

struct Env {
    app: Router,
    store: Store,
    actor: ActorHandle,
    verifier: Arc<FakeVerifier>,
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

    fn content_type(&self) -> String {
        self.headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    }

    fn retry_after(&self) -> Option<String> {
        self.headers
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_owned())
    }
}

fn settings() -> Settings {
    Settings {
        tls_domain: "tls.example.com".into(),
        proxy_port: 8443,
        proxy_users: vec![ProxyUser {
            name: "alice".into(),
            secret_hex: SECRET.into(),
        }],
        ..Settings::default()
    }
}

fn node_at(name: &str, ip: [u8; 4], state: NodeState) -> NodeRecord {
    let mut n = healthy_node(name, Ts(NOW), 0);
    n.ip = std::net::Ipv4Addr::from(ip);
    n.state = state;
    n
}

fn alpha() -> NodeRecord {
    node_at("alpha-00001", [45, 33, 1, 5], NodeState::Healthy)
}

fn beta() -> NodeRecord {
    node_at("beta-00002", [45, 33, 1, 6], NodeState::Healthy)
}

fn pool(nodes: Vec<NodeRecord>) -> PoolState {
    let mut state = PoolState::new(settings());
    for n in nodes {
        state.nodes.insert(n.id.clone(), n);
    }
    state
}

fn env(state: PoolState) -> Env {
    env_with(state, false)
}

/// An environment where the registrar was started with `MTPOOL_ALLOW_PRIVATE_IPS=true`.
fn env_allowing_private_ips(state: PoolState) -> Env {
    env_with(state, true)
}

fn env_with(state: PoolState, allow_private_ips: bool) -> Env {
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
    let verifier = FakeVerifier::new();
    let app = router(
        AppState::with_clock(store.clone(), actor.clone(), actor.snapshots(), clock_fn)
            .with_verifier(verifier.clone())
            .with_allow_private_ips(allow_private_ips),
    );
    Env {
        app,
        store,
        actor,
        verifier,
        clock,
        _dir: dir,
    }
}

fn owner(env: &Env, name: &str, limit: i64) -> OwnerId {
    tokens::create_owner(&env.store, name, limit, "").expect("owner")
}

fn token(env: &Env, kind: TokenKind, owner: Option<OwnerId>, node: Option<&str>) -> String {
    tokens::issue(
        &env.store,
        NewToken {
            kind,
            owner,
            node: node.map(node_id),
            expires_at: None,
            scope: String::new(),
        },
    )
    .expect("token")
}

fn node_token(env: &Env, name: &str) -> String {
    token(env, TokenKind::Node, None, Some(name))
}

fn enroll_token(env: &Env, owner: OwnerId) -> String {
    token(env, TokenKind::Enroll, Some(owner), None)
}

fn request(
    method: &str,
    path: &str,
    peer: &str,
    token: Option<&str>,
    body: Option<String>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let mut req = builder
        .body(body.map_or_else(Body::empty, Body::from))
        .unwrap();
    let ip: IpAddr = peer.parse().unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(ip, 40000)));
    req
}

async fn send(env: &Env, req: Request<Body>) -> Resp {
    let response = env.app.clone().oneshot(req).await.expect("response");
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.expect("body");
    Resp {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    }
}

async fn post(env: &Env, path: &str, peer: &str, token: &str, body: Option<Value>) -> Resp {
    let body = body.map(|b| b.to_string());
    send(env, request("POST", path, peer, Some(token), body)).await
}

async fn get(env: &Env, path: &str, peer: &str, token: &str) -> Resp {
    send(env, request("GET", path, peer, Some(token), None)).await
}

async fn enroll(env: &Env, token: &str, peer: &str, name: &str) -> Resp {
    post(
        env,
        "/api/v1/agent/enroll",
        peer,
        token,
        Some(json!({ "name": name, "port": 443, "engine": "telemt" })),
    )
    .await
}

fn assert_problem(resp: &Resp, status: StatusCode, code: &str) {
    assert_eq!(resp.status, status, "{}", resp.body);
    assert!(
        resp.content_type().starts_with("application/problem+json"),
        "content type {:?}",
        resp.content_type()
    );
    let problem: Problem = serde_json::from_str(&resp.body).expect("a Problem document");
    assert_eq!(problem.status, status.as_u16());
    assert_eq!(problem.code, code, "{}", resp.body);
    assert!(!problem.detail.is_empty(), "{}", resp.body);
}

fn assert_terminate(resp: &Resp, reason: &str) {
    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.body);
    let body = resp.json();
    assert_eq!(body["terminate"], true, "{body}");
    assert_eq!(body["reason"], reason, "{body}");
    assert!(
        body["message"].as_str().is_some_and(|m| !m.is_empty()),
        "{body}"
    );
}

async fn node_of(env: &Env, name: &str) -> Option<NodeRecord> {
    let id = node_id(name);
    env.actor
        .inspect(move |s| s.nodes.get(&id).cloned())
        .await
        .unwrap()
}

async fn node_count(env: &Env) -> usize {
    env.actor.inspect(|s| s.nodes.len()).await.unwrap()
}

/// Polls the live state until `done` holds for the node.
async fn wait_for(env: &Env, name: &str, done: impl Fn(&NodeRecord) -> bool) -> NodeRecord {
    for _ in 0..200 {
        if let Some(n) = node_of(env, name).await
            && done(&n)
        {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the node never reached the expected state");
}

fn count(env: &Env, sql: &str) -> i64 {
    env.store.flush().unwrap();
    env.store.read().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn report_at(checked_at_ms: i64, gp: Value) -> Value {
    json!({
        "ip": ALPHA_IP,
        "port": 443,
        "sni": "tls.example.com",
        "gp": gp,
        "metrics": { "ok": true, "from_client_total": 1000, "to_client_total": 2000, "clients": 3 },
        "dc2": { "ok": true, "reason": null },
        "host": {
            "load1": 0.5,
            "mem_used_bytes": 1,
            "mem_total_bytes": 2,
            "disk_used_bytes": 3,
            "disk_total_bytes": 4
        },
        "checked_at": rfc3339(checked_at_ms),
        "error": null
    })
}

fn no_data() -> Value {
    json!({ "kind": "no_data", "reason": "rate_limited" })
}

fn measured(id: &str) -> Value {
    json!({ "kind": "measured", "id": id })
}

async fn report(env: &Env, token: &str, body: Value) -> Resp {
    post(env, "/api/v1/agent/report", ALPHA_IP, token, Some(body)).await
}

// ----- enrollment ----------------------------------------------------------------------------

#[tokio::test]
async fn enroll_respects_owner_limit() {
    let env = env(pool(vec![]));
    let small = owner(&env, "small", 2);
    let other = owner(&env, "other", 1);
    let none = owner(&env, "none", 0);
    let small_token = enroll_token(&env, small);

    for (peer, name) in [("203.0.113.11", "one"), ("203.0.113.12", "two")] {
        let resp = enroll(&env, &small_token, peer, name).await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    }
    let third = enroll(&env, &small_token, "203.0.113.13", "three").await;
    assert_problem(&third, StatusCode::FORBIDDEN, "owner_limit");
    assert_eq!(node_count(&env).await, 2);
    assert_eq!(
        count(&env, "SELECT count(*) FROM tokens WHERE kind = 'node'"),
        2,
        "a refused enrollment gets no token"
    );

    // The limit is per owner, and 0 means no nodes at all.
    let other_token = enroll_token(&env, other);
    let ok = enroll(&env, &other_token, "203.0.113.14", "other").await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    let none_token = enroll_token(&env, none);
    let refused = enroll(&env, &none_token, "203.0.113.15", "none").await;
    assert_problem(&refused, StatusCode::FORBIDDEN, "owner_limit");

    // A node that left the pool leaves room.
    let state = |name: &str| {
        let id = node_id(name);
        move |s: &PoolState| s.nodes.get(&id).map(|n| n.owner)
    };
    let first_owner = env.actor.inspect(state("one-00000")).await.unwrap();
    assert!(first_owner.is_none(), "ids are random, not 00000");
}

#[tokio::test]
async fn enroll_limit_counts_only_live_nodes_of_the_owner() {
    let mut banned = node_at("gone-00003", [45, 33, 1, 7], NodeState::Banned);
    banned.owner = OwnerId(1);
    let mut mine = alpha();
    mine.owner = OwnerId(1);
    let env = env(pool(vec![banned, mine]));
    let me = owner(&env, "me", 2);
    assert_eq!(me, OwnerId(1));
    let enroll_token = enroll_token(&env, me);

    // One live node and one banned: the banned one does not count.
    let ok = enroll(&env, &enroll_token, "203.0.113.21", "fresh").await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    let full = enroll(&env, &enroll_token, "203.0.113.22", "late").await;
    assert_problem(&full, StatusCode::FORBIDDEN, "owner_limit");
}

/// The way from a confirmed ban to the node coming back, as the agent and the admin see it.
#[tokio::test]
async fn a_ban_takes_the_record_and_the_token_and_the_unban_works_without_them() {
    let mut node = alpha();
    node.state = NodeState::Quarantine;
    node.owner = OwnerId(1);
    let generation = node.generation;
    let env = env(pool(vec![node]));
    let me = owner(&env, "me", 2);
    assert_eq!(me, OwnerId(1));
    let enroll_with = enroll_token(&env, me);
    let node_tok = node_token(&env, "alpha-00001");
    let admin = token(&env, TokenKind::Admin, None, None);

    let blocked = SensorVote {
        vote: Vote::Blocked,
        evidence: json!(null),
    };
    env.actor
        .send(Command::RoundDone {
            node: node_id("alpha-00001"),
            generation,
            votes: vec![
                (SensorKind::Globalping, blocked.clone()),
                (SensorKind::Cheburcheck, blocked),
            ],
        })
        .await
        .unwrap();
    wait_until_gone(&env, "alpha-00001").await;
    env.store.flush().unwrap();

    assert_eq!(
        count(&env, "SELECT count(*) FROM tokens WHERE kind = 'node'"),
        0
    );
    assert_eq!(count(&env, "SELECT count(*) FROM bans"), 1);
    let beat = post(&env, "/api/v1/agent/heartbeat", ALPHA_IP, &node_tok, None).await;
    assert_problem(&beat, StatusCode::UNAUTHORIZED, "unauthorized");

    // The pause of the first ban: the agent enrolling at once does not reach the sensors.
    let early = enroll(&env, &enroll_with, ALPHA_IP, "alpha").await;
    assert_eq!(
        early.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        early.body
    );
    assert_eq!(node_count(&env).await, 0);

    let lifted = post(
        &env,
        "/api/v1/admin/nodes/alpha-00001/unban",
        PEER_ADMIN,
        &admin,
        None,
    )
    .await;
    assert_eq!(lifted.status, StatusCode::OK, "{}", lifted.body);
    let blocks = env.actor.inspect(|s| s.blocks.len()).await.unwrap();
    assert_eq!(blocks, 0);

    let back = enroll(&env, &enroll_with, ALPHA_IP, "alpha").await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);
    let again = post(
        &env,
        "/api/v1/admin/nodes/alpha-00001/unban",
        PEER_ADMIN,
        &admin,
        None,
    )
    .await;
    assert_problem(&again, StatusCode::NOT_FOUND, "not_found");
}

async fn wait_until_gone(env: &Env, name: &str) {
    for _ in 0..200 {
        if node_of(env, name).await.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{name} still has a record");
}

#[tokio::test]
async fn enroll_returns_node_token_once() {
    let env = env(pool(vec![]));
    let me = owner(&env, "me", 5);
    let enroll_token = enroll_token(&env, me);

    let resp = enroll(&env, &enroll_token, "203.0.113.31", "alpha").await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    let body = resp.json();
    let id = body["node_id"].as_str().unwrap().to_owned();
    let issued = body["node_token"].as_str().unwrap().to_owned();
    assert!(id.starts_with("alpha-"), "{id}");
    assert!(
        node_id_parses(&id),
        "{id} is a valid node id with a five character suffix"
    );
    assert!(issued.starts_with("mtp_node_"), "{issued}");
    assert_eq!(body["generation"], 0, "{body}");
    assert_eq!(body["replaced"], Value::Null, "{body}");
    assert_eq!(body["reverify"], false, "{body}");
    assert_eq!(
        body.as_object().unwrap().len(),
        5,
        "only the documented fields: {body}"
    );

    // The node is in the pool as Pending, owned by the enrolling owner, at the address the
    // request came from.
    let node = node_of(&env, &id).await.expect("node in the pool");
    assert_eq!(node.state, NodeState::Pending);
    assert_eq!(node.owner, me);
    assert_eq!(node.ip.to_string(), "203.0.113.31");
    assert_eq!(node.port, 443);

    // The token works for this node, and the registrar keeps its hash only.
    let row = tokens::lookup(&env.store.read(), &TokenHash::of(&issued))
        .unwrap()
        .expect("the token is stored by its hash");
    assert_eq!(row.kind, TokenKind::Node);
    assert_eq!(row.node.map(|n| n.to_string()), Some(id.clone()));
    let config = get(&env, "/api/v1/agent/config", "203.0.113.31", &issued).await;
    assert_eq!(config.status, StatusCode::OK, "{}", config.body);

    // Nobody sees it again: the public pages do not carry it, and the next enrollment gets
    // another token.
    for path in ["/api/v1/nodes", &format!("/api/v1/nodes/{id}")] {
        let page = send(&env, request("GET", path, "203.0.113.99", None, None)).await;
        assert!(!page.body.contains(&issued), "{path}");
    }
    let second = enroll(&env, &enroll_token, "203.0.113.32", "alpha").await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.body);
    assert_ne!(second.json()["node_token"], issued);
    assert_ne!(second.json()["node_id"], id, "the suffix is random");
}

fn node_id_parses(id: &str) -> bool {
    mtpool_core::NodeId::parse(id).is_ok()
}

#[tokio::test]
async fn enroll_private_ip_rejected() {
    let env = env(pool(vec![]));
    let me = owner(&env, "me", 50);
    let enroll_token = enroll_token(&env, me);

    for peer in [
        "10.0.0.5",
        "172.16.4.4",
        "192.168.1.1",
        "127.0.0.1",
        "100.64.0.1",
        "100.127.255.254",
        "169.254.1.1",
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "::1",
        "2001:db8::1",
    ] {
        let resp = enroll(&env, &enroll_token, peer, "alpha").await;
        assert_problem(&resp, StatusCode::BAD_REQUEST, "private_ip");
    }
    // Behind the local proxy the address comes from X-Forwarded-For, and is judged the same.
    let mut req = request(
        "POST",
        "/api/v1/agent/enroll",
        "127.0.0.1",
        Some(&enroll_token),
        Some(json!({ "name": "alpha", "port": 443, "engine": "telemt" }).to_string()),
    );
    req.headers_mut()
        .insert("x-forwarded-for", "192.168.7.7".parse().unwrap());
    assert_problem(
        &send(&env, req).await,
        StatusCode::BAD_REQUEST,
        "private_ip",
    );

    assert_eq!(node_count(&env).await, 0);
    assert_eq!(
        count(&env, "SELECT count(*) FROM tokens WHERE kind = 'node'"),
        0
    );

    // Documentation ranges count as public, so they are what the tests use.
    for peer in ["203.0.113.5", "198.51.100.5", "192.0.2.5", "100.63.255.255"] {
        let resp = enroll(&env, &enroll_token, peer, "alpha").await;
        assert_eq!(resp.status, StatusCode::OK, "{peer}: {}", resp.body);
    }
}

#[tokio::test]
async fn private_ip_rejected_by_default() {
    let env = env(pool(vec![]));
    let me = owner(&env, "me", 50);
    let enroll_token = enroll_token(&env, me);
    for peer in [
        "10.0.0.5",
        "172.18.0.3",
        "192.168.1.1",
        "127.0.0.1",
        "100.64.0.1",
    ] {
        let resp = enroll(&env, &enroll_token, peer, "alpha").await;
        assert_problem(&resp, StatusCode::BAD_REQUEST, "private_ip");
    }
    assert_eq!(node_count(&env).await, 0);
}

#[tokio::test]
async fn private_ip_accepted_with_flag() {
    let env = env_allowing_private_ips(pool(vec![]));
    let me = owner(&env, "me", 50);
    let enroll_token = enroll_token(&env, me);
    for (name, peer) in [
        ("alpha", "10.0.0.5"),
        ("bravo", "172.18.0.3"),
        ("charlie", "192.168.1.1"),
        ("delta", "127.0.0.1"),
        ("echo", "100.64.0.1"),
    ] {
        let resp = enroll(&env, &enroll_token, peer, name).await;
        assert_eq!(resp.status, StatusCode::OK, "{peer}: {}", resp.body);
        // The heartbeat of that node is judged by the same rule.
        let node_token = resp.json()["node_token"]
            .as_str()
            .expect("token")
            .to_owned();
        let beat = post(&env, "/api/v1/agent/heartbeat", peer, &node_token, None).await;
        assert_eq!(beat.status, StatusCode::OK, "{peer}: {}", beat.body);
    }
    // The flag opens private ranges only: addresses that are no address of a host stay refused.
    for peer in [
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "240.0.0.1",
        "::1",
    ] {
        let resp = enroll(&env, &enroll_token, peer, "foxtrot").await;
        assert_problem(&resp, StatusCode::BAD_REQUEST, "private_ip");
    }
}

#[tokio::test]
async fn enroll_validates_the_request() {
    let env = env(pool(vec![]));
    let me = owner(&env, "me", 50);
    let enroll_token = enroll_token(&env, me);
    let peer = "203.0.113.41";
    let post_enroll = |body: Value| {
        let env = &env;
        let token = enroll_token.clone();
        async move { post(env, "/api/v1/agent/enroll", peer, &token, Some(body)).await }
    };

    for name in ["", "-a", "a-", "a b", "名前", &"x".repeat(33)] {
        let resp = enroll(&env, &enroll_token, peer, name).await;
        assert_problem(&resp, StatusCode::BAD_REQUEST, "invalid_name");
    }
    let unknown = post_enroll(json!({ "name": "a", "port": 443, "engine": "telemt", "owner": 2 }));
    assert_problem(&unknown.await, StatusCode::BAD_REQUEST, "bad_request");
    let engine = post_enroll(json!({ "name": "a", "port": 443, "engine": "squid" }));
    assert_problem(&engine.await, StatusCode::BAD_REQUEST, "bad_request");
    let port = post_enroll(json!({ "name": "a", "port": 0, "engine": "meko" }));
    assert_problem(&port.await, StatusCode::BAD_REQUEST, "bad_request");
    let missing = post_enroll(json!({ "name": "a" }));
    assert_problem(&missing.await, StatusCode::BAD_REQUEST, "bad_request");
    let not_json = send(
        &env,
        request(
            "POST",
            "/api/v1/agent/enroll",
            peer,
            Some(&enroll_token),
            Some("not json".into()),
        ),
    )
    .await;
    assert_problem(&not_json, StatusCode::BAD_REQUEST, "bad_request");
    assert_eq!(node_count(&env).await, 0);

    // Every engine is accepted.
    for engine in ["telemt", "mt_proxy_l", "meko"] {
        let ip = format!("203.0.113.{}", 50 + engine.len());
        let resp = post(
            &env,
            "/api/v1/agent/enroll",
            &ip,
            &enroll_token,
            Some(json!({ "name": "a", "port": 443, "engine": engine })),
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "{engine}: {}", resp.body);
    }

    // Only an enroll token enrolls; a node token has no power here, and no token is 401.
    let node = node_token(&env, "alpha-00001");
    let wrong = enroll(&env, &node, peer, "a").await;
    assert_problem(&wrong, StatusCode::FORBIDDEN, "forbidden");
    let anon = send(
        &env,
        request(
            "POST",
            "/api/v1/agent/enroll",
            peer,
            None,
            Some(json!({ "name": "a", "port": 443, "engine": "telemt" }).to_string()),
        ),
    )
    .await;
    assert_problem(&anon, StatusCode::UNAUTHORIZED, "unauthorized");
}

#[tokio::test]
async fn enroll_tombstone_429_retry_after() {
    let mut state = pool(vec![]);
    state.tombstones.push(Tombstone {
        node_id: node_id("old-00001"),
        ip: "203.0.113.50".parse().unwrap(),
        strikes: 1,
        until: Ts(NOW + 120_000),
        updated_at: Ts(NOW),
    });
    let env = env(state);
    let me = owner(&env, "me", 5);
    let enroll_token = enroll_token(&env, me);

    let resp = enroll(&env, &enroll_token, "203.0.113.50", "fresh").await;
    assert_problem(&resp, StatusCode::TOO_MANY_REQUESTS, "enroll_paused");
    assert_eq!(resp.retry_after().as_deref(), Some("120"));
    assert!(
        resp.json()["detail"]
            .as_str()
            .unwrap()
            .contains("prune_pause")
    );
    assert_eq!(node_count(&env).await, 0);
    assert_eq!(
        count(&env, "SELECT count(*) FROM tokens WHERE kind = 'node'"),
        0,
        "a paused enrollment gets no token"
    );

    // Once the pause is over the same address enrolls.
    env.clock.store(NOW + 121_000, Ordering::SeqCst);
    let again = enroll(&env, &enroll_token, "203.0.113.50", "fresh").await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
}

// ----- heartbeat, retire and the replies of core ----------------------------------------------

#[tokio::test]
async fn banned_node_gets_403_terminate() {
    let env = env(pool(vec![
        node_at("banned-00001", [45, 33, 1, 5], NodeState::Banned),
        node_at("dead-00002", [45, 33, 1, 6], NodeState::Dead),
        node_at("pruned-00003", [45, 33, 1, 7], NodeState::Pruned),
    ]));
    for (name, peer, reason) in [
        ("banned-00001", "45.33.1.5", "banned"),
        ("dead-00002", "45.33.1.6", "dead"),
        ("pruned-00003", "45.33.1.7", "pruned"),
    ] {
        let token = node_token(&env, name);
        let beat = post(&env, "/api/v1/agent/heartbeat", peer, &token, None).await;
        assert_terminate(&beat, reason);
        assert!(beat.content_type().starts_with("application/json"));
        let rep = post(
            &env,
            "/api/v1/agent/report",
            peer,
            &token,
            Some(report_at(NOW, no_data())),
        )
        .await;
        assert_terminate(&rep, reason);
        let measured_rep = post(
            &env,
            "/api/v1/agent/report",
            peer,
            &token,
            Some(report_at(NOW, measured(&format!("m-{name}")))),
        )
        .await;
        assert_terminate(&measured_rep, reason);
        assert_terminate(
            &get(&env, "/api/v1/agent/config", peer, &token).await,
            reason,
        );
    }
    assert!(
        env.verifier.calls().is_empty(),
        "a node that is out of the pool is not worth a measurement"
    );
}

#[tokio::test]
async fn unknown_node_410() {
    let env = env(pool(vec![]));
    let token = node_token(&env, "ghost-00001");
    for (method, path) in [
        ("POST", "/api/v1/agent/heartbeat"),
        ("POST", "/api/v1/agent/retire"),
        ("GET", "/api/v1/agent/config"),
    ] {
        let resp = send(
            &env,
            request(method, path, "203.0.113.9", Some(&token), None),
        )
        .await;
        assert_problem(&resp, StatusCode::GONE, "unknown_node");
    }
    for gp in [no_data(), measured("m-ghost")] {
        let resp = report(&env, &token, report_at(NOW, gp)).await;
        assert_problem(&resp, StatusCode::GONE, "unknown_node");
    }
    assert!(env.verifier.calls().is_empty());
}

#[tokio::test]
async fn heartbeat_updates_the_node_and_refuses_extra_fields() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");
    env.clock.store(NOW + 7_000, Ordering::SeqCst);

    for body in [None, Some(json!({}))] {
        let resp = post(&env, "/api/v1/agent/heartbeat", ALPHA_IP, &token, body).await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
        assert_eq!(resp.json(), json!({}));
    }
    let node = node_of(&env, "alpha-00001").await.unwrap();
    assert_eq!(node.health.last_heartbeat, Ts(NOW + 7_000));

    let extra = post(
        &env,
        "/api/v1/agent/heartbeat",
        ALPHA_IP,
        &token,
        Some(json!({ "ip": "1.2.3.4" })),
    )
    .await;
    assert_problem(&extra, StatusCode::BAD_REQUEST, "bad_request");
}

#[tokio::test]
async fn retire_takes_the_node_out() {
    let env = env(pool(vec![alpha(), beta()]));
    let token = node_token(&env, "alpha-00001");
    let resp = post(&env, "/api/v1/agent/retire", ALPHA_IP, &token, None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert_eq!(resp.json(), json!({}));
    let alpha = node_of(&env, "alpha-00001").await.unwrap();
    assert_ne!(alpha.state, NodeState::Healthy);
    assert_eq!(
        node_of(&env, "beta-00002").await.unwrap().state,
        NodeState::Healthy
    );
}

#[tokio::test]
async fn node_token_cannot_act_for_other_node() {
    let env = env(pool(vec![alpha(), beta()]));
    let token_a = node_token(&env, "alpha-00001");
    env.clock.store(NOW + 9_000, Ordering::SeqCst);

    // A body that names the other node is not a thing the API knows.
    for (path, body) in [
        (
            "/api/v1/agent/heartbeat",
            json!({ "node_id": "beta-00002" }),
        ),
        ("/api/v1/agent/heartbeat", json!({ "node": "beta-00002" })),
        ("/api/v1/agent/retire", json!({ "node_id": "beta-00002" })),
    ] {
        let resp = post(&env, path, ALPHA_IP, &token_a, Some(body)).await;
        assert_problem(&resp, StatusCode::BAD_REQUEST, "bad_request");
    }
    let mut named = report_at(NOW + 9_000, no_data());
    named["node_id"] = json!("beta-00002");
    assert_problem(
        &report(&env, &token_a, named).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    let beta = node_of(&env, "beta-00002").await.unwrap();
    assert_eq!(beta.health.last_heartbeat, Ts(NOW), "beta was not touched");

    // What alpha's token does goes to alpha, whatever address it comes from.
    let resp = post(&env, "/api/v1/agent/heartbeat", ALPHA_IP, &token_a, None).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert_eq!(
        node_of(&env, "alpha-00001")
            .await
            .unwrap()
            .health
            .last_heartbeat,
        Ts(NOW + 9_000)
    );
    assert_eq!(
        node_of(&env, "beta-00002")
            .await
            .unwrap()
            .health
            .last_heartbeat,
        Ts(NOW)
    );
    // A report of alpha's token that is about beta's address is refused.
    let mut about_beta = report_at(NOW + 9_000, no_data());
    about_beta["ip"] = json!("45.33.1.6");
    assert_problem(
        &report(&env, &token_a, about_beta).await,
        StatusCode::CONFLICT,
        "report_mismatch",
    );

    // Retiring with alpha's token leaves beta in the pool.
    let retire = post(&env, "/api/v1/agent/retire", ALPHA_IP, &token_a, None).await;
    assert_eq!(retire.status, StatusCode::OK, "{}", retire.body);
    assert_eq!(
        node_of(&env, "beta-00002").await.unwrap().state,
        NodeState::Healthy
    );
}

// ----- reports -------------------------------------------------------------------------------

#[tokio::test]
async fn report_limit_256kib() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");

    let mut body = report_at(NOW, no_data());
    body["error"] = json!("");
    let base = body.to_string().len();
    let padded = |extra: usize| {
        let mut b = body.clone();
        b["error"] = json!("x".repeat(LIMIT - base + extra));
        b.to_string()
    };

    let at_limit = padded(0);
    assert_eq!(at_limit.len(), LIMIT);
    let ok = send(
        &env,
        request(
            "POST",
            "/api/v1/agent/report",
            ALPHA_IP,
            Some(&token),
            Some(at_limit),
        ),
    )
    .await;
    assert_eq!(
        ok.status,
        StatusCode::OK,
        "{}",
        &ok.body[..ok.body.len().min(200)]
    );

    let over = padded(1);
    assert_eq!(over.len(), LIMIT + 1);
    let resp = send(
        &env,
        request(
            "POST",
            "/api/v1/agent/report",
            ALPHA_IP,
            Some(&token),
            Some(over),
        ),
    )
    .await;
    assert_problem(&resp, StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
}

#[tokio::test]
async fn report_unknown_fields_rejected() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");

    let mut top = report_at(NOW, measured("m-1"));
    top["extra"] = json!(1);
    let mut metrics = report_at(NOW, measured("m-2"));
    metrics["metrics"]["extra"] = json!(1);
    let mut gp = report_at(NOW, measured("m-3"));
    gp["gp"]["extra"] = json!(1);
    let mut kind = report_at(NOW, measured("m-4"));
    kind["gp"] = json!({ "kind": "guess" });
    let mut dc2 = report_at(NOW, no_data());
    dc2["dc2"]["extra"] = json!(1);
    let mut host = report_at(NOW, no_data());
    host["host"]["extra"] = json!(1);
    let mut missing = report_at(NOW, no_data());
    missing.as_object_mut().unwrap().remove("metrics");
    for body in [top, metrics, gp, kind, dc2, host, missing] {
        let resp = report(&env, &token, body.clone()).await;
        assert_problem(&resp, StatusCode::BAD_REQUEST, "bad_request");
    }
    let mut stamp = report_at(NOW, no_data());
    stamp["checked_at"] = json!("yesterday");
    assert_problem(
        &report(&env, &token, stamp).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    );
    assert!(env.verifier.calls().is_empty());
    assert_eq!(
        count(&env, "SELECT count(*) FROM gp_measurements"),
        0,
        "a refused report records nothing"
    );
}

#[tokio::test]
async fn report_reused_measurement_id_409() {
    let env = env(pool(vec![alpha(), beta()]));
    let token_a = node_token(&env, "alpha-00001");
    let token_b = node_token(&env, "beta-00002");
    env.verifier.answer(Ok(SensorVote {
        vote: Vote::Clear,
        evidence: json!({}),
    }));

    let first = report(&env, &token_a, report_at(NOW, measured("m-shared"))).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    wait_for(&env, "alpha-00001", |n| {
        matches!(n.health.gp, GpStatus::Ok { .. })
    })
    .await;

    // The same node again, and another node, with the same id.
    let again = report(&env, &token_a, report_at(NOW, measured("m-shared"))).await;
    assert_problem(&again, StatusCode::CONFLICT, "measurement_reused");
    let mut other = report_at(NOW, measured("m-shared"));
    other["ip"] = json!("45.33.1.6");
    let stolen = post(
        &env,
        "/api/v1/agent/report",
        "45.33.1.6",
        &token_b,
        Some(other),
    )
    .await;
    assert_problem(&stolen, StatusCode::CONFLICT, "measurement_reused");

    assert_eq!(
        env.verifier.calls().len(),
        1,
        "a reused id is not verified again"
    );
    assert_eq!(count(&env, "SELECT count(*) FROM gp_measurements"), 1);
    assert_eq!(
        count(
            &env,
            "SELECT count(*) FROM gp_measurements WHERE id = 'm-shared' AND node = 'alpha-00001' AND at = 1800000000000"
        ),
        1
    );
    // Another id is fine.
    let fresh = report(&env, &token_a, report_at(NOW, measured("m-next"))).await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.body);
}

#[tokio::test]
async fn report_clock_skew_409_and_event_explains_skew() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");
    let minute = 60_000;

    // The window is 10 minutes back and one minute ahead, ends included.
    for (offset, ok) in [
        (-10 * minute, true),
        (-10 * minute - 1_000, false),
        (minute, true),
        (minute + 1_000, false),
        (0, true),
    ] {
        let resp = report(&env, &token, report_at(NOW + offset, no_data())).await;
        if ok {
            assert_eq!(resp.status, StatusCode::OK, "{offset}: {}", resp.body);
        } else {
            assert_problem(&resp, StatusCode::CONFLICT, "clock_skew");
            assert!(
                resp.json()["detail"].as_str().unwrap().contains("clock"),
                "{}",
                resp.body
            );
        }
    }

    // The first refusal left an event that says how far off the agent is; the second, within
    // the hour, left none.
    env.store.flush().unwrap();
    let conn = env.store.read();
    let mut stmt = conn
        .prepare("SELECT at, node, detail FROM events WHERE kind = 'clock_skew' ORDER BY id")
        .unwrap();
    let rows: Vec<(i64, Option<String>, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let detail = |i: usize| serde_json::from_str::<Value>(&rows[i].2).unwrap();
    for row in &rows {
        assert_eq!(row.0, NOW);
        assert_eq!(row.1.as_deref(), Some("alpha-00001"));
    }
    let behind = detail(0);
    assert_eq!(behind["skew_sec"], -601, "{behind}");
    assert_eq!(behind["checked_at"], rfc3339(NOW - 10 * minute - 1_000));
    assert_eq!(behind["server_now"], rfc3339(NOW));
}

#[tokio::test]
async fn clock_skew_event_once_per_hour() {
    let env = env(pool(vec![alpha(), beta()]));
    let token = node_token(&env, "alpha-00001");
    let other = node_token(&env, "beta-00002");
    let events = |env: &Env| {
        count(
            env,
            "SELECT count(*) FROM events WHERE kind = 'clock_skew' AND node = 'alpha-00001'",
        )
    };
    let hour = 60 * 60 * 1000;

    for _ in 0..2 {
        let resp = report(&env, &token, report_at(NOW - 20 * 60_000, no_data())).await;
        assert_problem(&resp, StatusCode::CONFLICT, "clock_skew");
    }
    assert_eq!(events(&env), 1, "two skewed reports, one event");

    // The answer stays the same just under the hour, and another node is not held back.
    env.clock.store(NOW + hour - 1, Ordering::SeqCst);
    let resp = report(&env, &token, report_at(NOW - 20 * 60_000, no_data())).await;
    assert_problem(&resp, StatusCode::CONFLICT, "clock_skew");
    assert_eq!(events(&env), 1);
    let resp = post(
        &env,
        "/api/v1/agent/report",
        "198.51.100.9",
        &other,
        Some(report_at(NOW - 20 * 60_000, no_data())),
    )
    .await;
    assert_problem(&resp, StatusCode::CONFLICT, "clock_skew");
    assert_eq!(
        count(
            &env,
            "SELECT count(*) FROM events WHERE kind = 'clock_skew'"
        ),
        2,
        "the limit is per node"
    );

    env.clock.store(NOW + hour, Ordering::SeqCst);
    let resp = report(&env, &token, report_at(NOW - 20 * 60_000, no_data())).await;
    assert_problem(&resp, StatusCode::CONFLICT, "clock_skew");
    assert_eq!(events(&env), 2, "an hour on, a second event");
}

#[tokio::test]
async fn second_measured_report_while_one_is_verified_is_429() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");
    let gate = env.verifier.gate();
    env.verifier.answer(Ok(SensorVote {
        vote: Vote::Clear,
        evidence: json!({}),
    }));
    env.clock.store(NOW + 1_000, Ordering::SeqCst);
    let at = NOW + 1_000;

    let first = report(&env, &token, report_at(at, measured("m-first"))).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);

    // The first measurement is still being verified.
    let second = report(&env, &token, report_at(at, measured("m-second"))).await;
    assert_problem(&second, StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    assert_eq!(
        second
            .headers
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("30")
    );
    assert_eq!(
        count(
            &env,
            "SELECT count(*) FROM gp_measurements WHERE id = 'm-second'"
        ),
        0,
        "a refused report claims no measurement id"
    );
    // A report with no measurement is not held back.
    let plain = report(&env, &token, report_at(at, no_data())).await;
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.body);

    gate.notify_one();
    wait_for(&env, "alpha-00001", |n| {
        matches!(n.health.gp, GpStatus::Ok { .. })
    })
    .await;

    // The slot is free again, and the id that was refused is still usable.
    let mut third = None;
    for _ in 0..100 {
        let resp = report(&env, &token, report_at(at, measured("m-second"))).await;
        if resp.status != StatusCode::TOO_MANY_REQUESTS {
            third = Some(resp);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let third = third.expect("the slot is freed once the verification ends");
    assert_eq!(third.status, StatusCode::OK, "{}", third.body);
    gate.notify_one();
}

#[tokio::test]
async fn gp_no_data_not_failure() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");
    env.clock.store(NOW + 30_000, Ordering::SeqCst);

    for reason in ["rate_limited", "unavailable", "timeout", "something_new"] {
        let resp = report(
            &env,
            &token,
            report_at(NOW + 30_000, json!({ "kind": "no_data", "reason": reason })),
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK, "{reason}: {}", resp.body);
        assert_eq!(resp.json(), json!({}));
    }
    let node = node_of(&env, "alpha-00001").await.unwrap();
    assert_eq!(
        node.state,
        NodeState::Healthy,
        "no data does not move the node"
    );
    assert!(
        matches!(node.health.gp, GpStatus::NoData { .. }),
        "no data after a success stays no data, not a failure: {:?}",
        node.health.gp
    );
    assert!(
        node.health.metrics.up,
        "the metrics of the report still count"
    );
    assert!(
        env.verifier.calls().is_empty(),
        "there is nothing to verify"
    );
    assert_eq!(count(&env, "SELECT count(*) FROM gp_measurements"), 0);
}

#[tokio::test]
async fn report_answers_before_the_measurement_is_verified() {
    let mut node = alpha();
    node.health.gp = GpStatus::Unknown;
    let env = env(pool(vec![node]));
    let token = node_token(&env, "alpha-00001");
    let gate = env.verifier.gate();
    env.verifier.answer(Ok(SensorVote {
        vote: Vote::Clear,
        evidence: json!({ "probes": 3 }),
    }));
    env.clock.store(NOW + 5_000, Ordering::SeqCst);

    let resp = report(&env, &token, report_at(NOW + 4_000, measured("m-clear"))).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert_eq!(resp.json(), json!({}));

    // The answer is out while the verifier still waits; nothing has changed yet.
    for _ in 0..100 {
        if !env.verifier.calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let calls = env.verifier.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "m-clear");
    assert_eq!(
        calls[0].1,
        GpExpect {
            ip: ALPHA_IP.parse().unwrap(),
            port: 443,
            sni: "tls.example.com".into(),
            reported_at: Ts(NOW + 4_000),
        }
    );
    assert_eq!(
        node_of(&env, "alpha-00001").await.unwrap().health.gp,
        GpStatus::Unknown
    );

    gate.notify_one();
    let node = wait_for(&env, "alpha-00001", |n| {
        matches!(n.health.gp, GpStatus::Ok { .. })
    })
    .await;
    assert_eq!(
        node.health.gp,
        GpStatus::Ok {
            at: Ts(NOW + 5_000)
        }
    );
}

#[tokio::test]
async fn measured_report_applies_the_verified_vote_only() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");

    // A measurement that proves a block is a failure of the check.
    env.verifier.answer(Ok(SensorVote {
        vote: Vote::Blocked,
        evidence: json!({}),
    }));
    let resp = report(&env, &token, report_at(NOW, measured("m-blocked"))).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    wait_for(&env, "alpha-00001", |n| {
        matches!(n.health.gp, GpStatus::Failed { .. })
    })
    .await;

    // One that cannot be verified is no data, whatever the agent claims: the failed check
    // stays failed and a success is not handed out.
    for (id, rejection) in [
        ("m-unavailable", GpRejected::Unavailable),
        ("m-unknown", GpRejected::UnknownMeasurement),
        ("m-mismatch", GpRejected::Mismatch),
        ("m-stale", GpRejected::Stale),
    ] {
        env.verifier.answer(Err(rejection));
        let resp = report(&env, &token, report_at(NOW, measured(id))).await;
        assert_eq!(resp.status, StatusCode::OK, "{id}: {}", resp.body);
    }
    for _ in 0..100 {
        if env.verifier.calls().len() == 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(env.verifier.calls().len(), 5);
    // Wait out the queued commands.
    env.actor.inspect(|_| ()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let node = node_of(&env, "alpha-00001").await.unwrap();
    assert!(
        matches!(node.health.gp, GpStatus::Failed { .. }),
        "{:?}",
        node.health.gp
    );
}

#[tokio::test]
async fn report_for_another_address_409() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");

    for (field, value) in [
        ("ip", json!("203.0.113.77")),
        ("port", json!(444)),
        ("sni", json!("other.example.com")),
    ] {
        let mut body = report_at(NOW, measured("m-1"));
        body[field] = value;
        let resp = report(&env, &token, body).await;
        assert_problem(&resp, StatusCode::CONFLICT, "report_mismatch");
    }
    assert!(env.verifier.calls().is_empty());
    assert_eq!(
        count(&env, "SELECT count(*) FROM gp_measurements"),
        0,
        "the id was not spent"
    );
    let ok = report(&env, &token, report_at(NOW, measured("m-1"))).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
}

#[tokio::test]
async fn report_feeds_traffic_and_clients() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");
    for (from, to) in [(1_000, 2_000), (1_500, 2_900)] {
        let mut body = report_at(NOW, no_data());
        body["metrics"]["from_client_total"] = json!(from);
        body["metrics"]["to_client_total"] = json!(to);
        let resp = report(&env, &token, body).await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    }
    let stats = env
        .actor
        .inspect(|s| s.stats.get(&node_id("alpha-00001")).cloned())
        .await
        .unwrap()
        .expect("stats");
    assert_eq!(stats.from_client_total, 500);
    assert_eq!(stats.to_client_total, 900);
    assert_eq!(stats.clients, Some(3));
}

// ----- config --------------------------------------------------------------------------------

#[tokio::test]
async fn config_returns_users_sni_port_intervals() {
    let mut state = pool(vec![alpha()]);
    state.settings.heartbeat_interval_sec = 20;
    state.settings.globalping_interval_sec = 400;
    state.settings.metrics_interval_sec = 30;
    state.settings.sync_interval_sec = 90;
    state.settings.proxy_users.push(ProxyUser {
        name: "bob".into(),
        secret_hex: "ffeeddccbbaa99887766554433221100".into(),
    });
    let env = env(state);
    let token = node_token(&env, "alpha-00001");

    let resp = get(&env, "/api/v1/agent/config", ALPHA_IP, &token).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.body);
    assert_eq!(
        resp.json(),
        json!({
            "users": [
                { "name": "alice", "secret_hex": SECRET },
                { "name": "bob", "secret_hex": "ffeeddccbbaa99887766554433221100" }
            ],
            "sni": "tls.example.com",
            "port": 8443,
            "intervals": {
                "heartbeat_sec": 20,
                "globalping_sec": 400,
                "metrics_sec": 30,
                "sync_sec": 90
            },
            "antiscan": true
        })
    );

    // Settings that change at runtime show up in the next answer.
    let mut changed = settings();
    changed.tls_domain = "new.example.com".into();
    changed.proxy_port = 9443;
    env.actor
        .call(mtpool_core::Command::SettingsChanged(changed))
        .await
        .unwrap();
    let next = get(&env, "/api/v1/agent/config", ALPHA_IP, &token)
        .await
        .json();
    assert_eq!(next["sni"], "new.example.com");
    assert_eq!(next["port"], 9443);
    assert_eq!(
        next["intervals"]["heartbeat_sec"], 15,
        "the defaults of the new settings"
    );

    // Admin and enroll tokens do not read a node's config.
    let me = owner(&env, "me", 1);
    let enroll = enroll_token(&env, me);
    let resp = get(&env, "/api/v1/agent/config", ALPHA_IP, &enroll).await;
    assert_problem(&resp, StatusCode::FORBIDDEN, "forbidden");
}

// ----- around the routes -----------------------------------------------------------------------

#[tokio::test]
async fn agent_routes_are_not_behind_the_public_request_limit() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");
    // The public routes answer 429 after 120 requests a minute from one address.
    for i in 0..130 {
        let resp = post(&env, "/api/v1/agent/heartbeat", ALPHA_IP, &token, None).await;
        assert_eq!(resp.status, StatusCode::OK, "request {i}: {}", resp.body);
    }
}

#[tokio::test]
async fn bad_agent_tokens_count_against_the_failed_attempt_limit() {
    let env = env(pool(vec![alpha()]));
    let bad = format!("mtp_node_{}", "A".repeat(43));
    let mut last = StatusCode::OK;
    for _ in 0..30 {
        last = post(&env, "/api/v1/agent/heartbeat", "203.0.113.200", &bad, None)
            .await
            .status;
        if last == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
        assert_eq!(last, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn agent_routes_refuse_other_methods() {
    let env = env(pool(vec![alpha()]));
    let token = node_token(&env, "alpha-00001");
    let resp = send(
        &env,
        request(
            "GET",
            "/api/v1/agent/heartbeat",
            ALPHA_IP,
            Some(&token),
            None,
        ),
    )
    .await;
    assert_problem(&resp, StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
}

#[derive(Clone)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The log of this test binary. Global: a scoped subscriber misses a line that another test
/// reaches first. Other tests write here too, so a test reads the lines of its own node.
fn journal() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOGS: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Journal(Arc::clone(&buf));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        buf
    })
}

#[tokio::test]
async fn a_report_without_a_measurement_says_why_in_the_log() {
    const NODE: &str = "gamma-00077";
    let logs = journal();
    let lines = || -> Vec<String> {
        String::from_utf8_lossy(&logs.lock().unwrap())
            .lines()
            .filter(|l| l.contains(NODE))
            .map(str::to_owned)
            .collect()
    };
    let env = env(pool(vec![node_at(
        NODE,
        [45, 33, 1, 5],
        NodeState::Healthy,
    )]));
    let token = node_token(&env, NODE);
    env.clock.store(NOW + 30_000, Ordering::SeqCst);
    let body =
        |reason: &str| report_at(NOW + 30_000, json!({ "kind": "no_data", "reason": reason }));

    // The usual report between measurements says nothing, and neither does an unknown word.
    report(&env, &token, body("no_data")).await;
    report(&env, &token, body("made-up-word")).await;
    assert_eq!(lines(), Vec::<String>::new());

    report(&env, &token, body("rate_limited")).await;
    let lines = lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("no Globalping measurement"), "{lines:?}");
    assert!(lines[0].contains("RateLimited"), "{lines:?}");
}
