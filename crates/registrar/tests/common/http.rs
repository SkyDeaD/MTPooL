//! A router over a temp store for the tests of the bot and admin APIs: requests without a
//! socket, a fixed clock, a fake name resolver and the event fan registered as a driver.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use mtpool_core::{GpStatus, NodeRecord, NodeState, OwnerId, PoolState, ProxyUser, Settings, Ts};
use mtpool_protocol::problem::Problem;
use mtpool_registrar::actor::{Actor, ActorHandle, ActorOptions, Clock};
use mtpool_registrar::api::stream::EventFan;
use mtpool_registrar::api::webhook_url::{ResolveFuture, Resolver};
use mtpool_registrar::auth::tokens::{self, NewToken, TokenKind};
use mtpool_registrar::server::{AppState, router};
use mtpool_registrar::store::Store;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::{healthy_node, node_id, open_store};

pub const NOW: i64 = 1_800_000_000_000;
pub const SECRET: &str = "00112233445566778899aabbccddeeff";
/// The address `healthy_node` gives its nodes.
pub const ALPHA_IP: &str = "45.33.1.5";
pub const PEER: &str = "198.51.100.7";

/// Answers names from a table; a name that is not in it does not resolve.
#[derive(Default)]
pub struct FakeResolver {
    names: Mutex<HashMap<String, Vec<IpAddr>>>,
}

impl FakeResolver {
    pub fn set(&self, name: &str, addresses: &[&str]) {
        self.names.lock().unwrap().insert(
            name.to_owned(),
            addresses.iter().map(|a| a.parse().unwrap()).collect(),
        );
    }
}

impl Resolver for FakeResolver {
    fn resolve<'a>(&'a self, host: &'a str, _port: u16) -> ResolveFuture<'a> {
        Box::pin(async move {
            self.names
                .lock()
                .unwrap()
                .get(host)
                .cloned()
                .ok_or_else(|| std::io::Error::other("no such name"))
        })
    }
}

pub struct Env {
    pub app: Router,
    pub store: Store,
    pub actor: ActorHandle,
    pub clock: Arc<AtomicI64>,
    pub events: EventFan,
    pub resolver: Arc<FakeResolver>,
    pub _dir: tempfile::TempDir,
}

impl Env {
    pub fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    pub fn content_type(&self) -> String {
        self.headers
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    }

    pub fn retry_after(&self) -> Option<String> {
        self.headers
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_owned())
    }
}

pub fn settings() -> Settings {
    Settings {
        tls_domain: "tls.example.com".into(),
        proxy_port: 8443,
        domains: vec!["proxy.example.com".into()],
        proxy_users: vec![ProxyUser {
            name: "alice".into(),
            secret_hex: SECRET.into(),
        }],
        ..Settings::default()
    }
}

pub fn node_at(name: &str, ip: [u8; 4], owner: i64, state: NodeState) -> NodeRecord {
    let mut n = healthy_node(name, Ts(NOW), 0);
    n.ip = std::net::Ipv4Addr::from(ip);
    n.owner = OwnerId(owner);
    n.state = state;
    if state == NodeState::Quarantine {
        n.health.gp = GpStatus::Unknown;
    }
    n
}

pub fn pool(nodes: Vec<NodeRecord>) -> PoolState {
    let mut state = PoolState::new(settings());
    for n in nodes {
        state.nodes.insert(n.id.clone(), n);
    }
    state
}

pub fn env(state: PoolState) -> Env {
    env_with_keepalive(state, Duration::from_secs(15))
}

pub fn env_with_keepalive(state: PoolState, keepalive: Duration) -> Env {
    let (dir, store) = open_store();
    let clock = Arc::new(AtomicI64::new(NOW));
    let shared = Arc::clone(&clock);
    let clock_fn: Clock = Arc::new(move || Ts(shared.load(Ordering::SeqCst)));
    let events = EventFan::with_keepalive(keepalive);
    let actor = Actor::spawn_with(
        state,
        store.clone(),
        vec![Arc::new(events.clone())],
        ActorOptions {
            clock: Arc::clone(&clock_fn),
            ..ActorOptions::default()
        },
    );
    let resolver = Arc::new(FakeResolver::default());
    let app = router(
        AppState::with_clock(store.clone(), actor.clone(), actor.snapshots(), clock_fn)
            .with_events(events.clone())
            .with_resolver(resolver.clone()),
    );
    Env {
        app,
        store,
        actor,
        clock,
        events,
        resolver,
        _dir: dir,
    }
}

pub fn owner(env: &Env, name: &str, limit: i64) -> OwnerId {
    tokens::create_owner(&env.store, name, limit, "").expect("owner")
}

pub fn token(env: &Env, kind: TokenKind, owner: Option<OwnerId>) -> String {
    tokens::issue(
        &env.store,
        NewToken {
            kind,
            owner,
            node: None,
            expires_at: None,
            scope: String::new(),
        },
    )
    .expect("token")
}

pub fn bot_token(env: &Env, owner: OwnerId) -> String {
    token(env, TokenKind::Bot, Some(owner))
}

pub fn admin_token(env: &Env) -> String {
    token(env, TokenKind::Admin, None)
}

pub fn request(
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let mut req = builder
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let ip: IpAddr = PEER.parse().unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(ip, 40000)));
    req
}

pub async fn send(env: &Env, req: Request<Body>) -> Resp {
    let response = env.app.clone().oneshot(req).await.expect("response");
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.expect("body");
    Resp {
        status: parts.status,
        headers: parts.headers,
        body: String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    }
}

pub async fn call(env: &Env, method: &str, path: &str, token: &str, body: Option<Value>) -> Resp {
    send(env, request(method, path, Some(token), body)).await
}

pub async fn get(env: &Env, path: &str, token: &str) -> Resp {
    call(env, "GET", path, token, None).await
}

pub async fn post(env: &Env, path: &str, token: &str, body: Option<Value>) -> Resp {
    call(env, "POST", path, token, body).await
}

pub fn assert_problem(resp: &Resp, status: StatusCode, code: &str) {
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

pub async fn node_state(env: &Env, name: &str) -> Option<NodeState> {
    let id = node_id(name);
    env.actor
        .inspect(move |s| s.nodes.get(&id).map(|n| n.state))
        .await
        .unwrap()
}

pub async fn live_settings(env: &Env) -> Settings {
    env.actor.inspect(|s| s.settings.clone()).await.unwrap()
}

pub fn count(env: &Env, sql: &str) -> i64 {
    env.store.flush().unwrap();
    env.store.read().query_row(sql, [], |r| r.get(0)).unwrap()
}

pub fn json_of(value: Value) -> Option<Value> {
    Some(value)
}

pub fn empty() -> Value {
    json!({})
}
