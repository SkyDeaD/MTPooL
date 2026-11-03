use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use mtpool_core::{
    Effect, EngineKind, Event, EventKind, NodeId, NodeRecord, OwnerId, PoolState, Settings, Ts,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::actor::{Actor, ActorOptions};
use crate::api::webhook_url::ResolveFuture;
use crate::auth::tokens;
use crate::store::AdminOp;

const NOW: i64 = 1_800_000_000_000;
const SECRET: &str = "whsec_test_secret_0123456789";
const PUBLIC: &str = "93.184.216.34";

struct Fixed(Vec<IpAddr>);

impl Fixed {
    fn of(addresses: &[&str]) -> Arc<Fixed> {
        Arc::new(Fixed(
            addresses.iter().map(|a| a.parse().unwrap()).collect(),
        ))
    }
}

impl Resolver for Fixed {
    fn resolve<'a>(&'a self, _host: &'a str, _port: u16) -> ResolveFuture<'a> {
        let out = self.0.clone();
        Box::pin(async move { Ok(out) })
    }
}

struct Env {
    store: Store,
    db: PathBuf,
    clock: Arc<AtomicI64>,
    _dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("registrar.db");
        Env {
            store: Store::open(&db).unwrap(),
            db,
            clock: Arc::new(AtomicI64::new(NOW)),
            _dir: dir,
        }
    }

    fn clock(&self) -> Clock {
        let shared = Arc::clone(&self.clock);
        Arc::new(move || Ts(shared.load(Ordering::SeqCst)))
    }

    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn sender(&self, resolver: Arc<Fixed>, to: SocketAddr) -> WebhookSender {
        WebhookSender::new(self.store.clone(), resolver, self.clock()).local_http(to)
    }

    fn owner(&self, name: &str) -> OwnerId {
        tokens::create_owner(&self.store, name, 5, "").unwrap()
    }

    fn webhook(&self, owner: OwnerId, scope: &str, url: &str, events: &[&str]) -> i64 {
        self.store
            .admin(AdminOp::InsertWebhook {
                owner_id: owner.0,
                scope: scope.to_owned(),
                url: url.to_owned(),
                secret: SECRET.to_owned(),
                events: serde_json::to_string(events).unwrap(),
                created_at: Ts(NOW),
            })
            .unwrap()
    }

    fn delivery(&self, webhook: i64, event_id: &str, event: &str) -> i64 {
        let payload = json!({ "id": event_id, "event": event, "at": "2027-01-15T08:00:00Z",
            "node": "alpha-00001", "domain": null, "detail": {} });
        self.store
            .admin(AdminOp::InsertDelivery {
                webhook_id: webhook,
                event_id: event_id.to_owned(),
                event: event.to_owned(),
                payload: payload.to_string(),
                at: Ts(self.now()),
            })
            .unwrap()
    }

    fn rows(&self) -> Vec<Row> {
        let conn = self.store.read();
        conn.prepare(
            "SELECT id, webhook_id, event_id, event, payload, attempt, status, response_code, \
             last_error, next_at FROM webhook_deliveries ORDER BY id",
        )
        .unwrap()
        .query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                webhook_id: r.get(1)?,
                event_id: r.get(2)?,
                event: r.get(3)?,
                payload: r.get(4)?,
                attempt: r.get(5)?,
                status: r.get(6)?,
                response_code: r.get(7)?,
                last_error: r.get(8)?,
                next_at: r.get(9)?,
            })
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }

    fn row(&self, id: i64) -> Row {
        self.rows().into_iter().find(|r| r.id == id).unwrap()
    }
}

#[derive(Debug, Clone)]
struct Row {
    id: i64,
    webhook_id: i64,
    event_id: String,
    event: String,
    payload: String,
    attempt: i64,
    status: String,
    response_code: Option<i64>,
    last_error: Option<String>,
    next_at: Option<i64>,
}

fn url_of(server: &MockServer) -> String {
    format!("http://hooks.example.com:{}/hook", server.address().port())
}

fn header(request: &wiremock::Request, name: &str) -> String {
    request
        .headers
        .get(name)
        .unwrap_or_else(|| panic!("no header {name}"))
        .to_str()
        .unwrap()
        .to_owned()
}

// ----- signature ---------------------------------------------------------------------------

#[test]
fn signature_format_t_v1_hmac_sha256() {
    let body = r#"{"id":"evt_00112233445566778899aabb"}"#;
    // The expected value comes from `openssl dgst -sha256 -hmac` over "<t>.<body>".
    assert_eq!(
        sign(SECRET, 1_800_000_000, body),
        "t=1800000000,v1=2dbd653c3d30068af16211d58f1663884a5a9ad26f9b079f18ec5bcd584953cc"
    );
    let signature = sign(SECRET, 1_800_000_000, body);
    let (t, v1) = signature.split_once(',').unwrap();
    assert_eq!(t, "t=1800000000");
    let digest = v1.strip_prefix("v1=").unwrap();
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    // Every input counts, and the secret itself is not in the value.
    assert_ne!(signature, sign(SECRET, 1_800_000_001, body));
    assert_ne!(signature, sign("another-secret-value", 1_800_000_000, body));
    assert_ne!(signature, sign(SECRET, 1_800_000_000, "{}"));
    assert!(!signature.contains(SECRET));
}

// ----- sender ------------------------------------------------------------------------------

#[tokio::test]
async fn delivered_on_2xx_with_the_documented_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200).set_body_string("BODY-OF-THE-RECEIVER"))
        .mount(&server)
        .await;
    let env = Env::new();
    let owner = env.owner("alice");
    let hook = env.webhook(owner, "owner", &url_of(&server), &["node.banned"]);
    let id = env.delivery(hook, "evt_00112233445566778899aabb", "node.banned");
    let sender = env.sender(Fixed::of(&[PUBLIC]), *server.address());

    assert_eq!(sender.deliver_due().await, 1);

    let row = env.row(id);
    assert_eq!(row.status, "delivered");
    assert_eq!(row.attempt, 1);
    assert_eq!(row.response_code, Some(200));
    assert_eq!(row.last_error, None);
    assert_eq!(row.next_at, None);
    // The receiver's answer is not kept.
    assert!(!format!("{row:?}").contains("BODY-OF-THE-RECEIVER"));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(header(request, "content-type"), "application/json");
    assert_eq!(header(request, "x-mtpool-event"), "node.banned");
    assert_eq!(header(request, "x-mtpool-delivery"), id.to_string());
    assert_eq!(
        header(request, "user-agent"),
        format!("mtpool-registrar/{}", env!("CARGO_PKG_VERSION"))
    );
    let body = String::from_utf8(request.body.clone()).unwrap();
    assert_eq!(body, row.payload);
    let signature = header(request, "x-mtpool-signature");
    let t: i64 = signature
        .strip_prefix("t=")
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse().ok())
        .expect("t=<unix seconds>");
    assert_eq!(t, NOW / 1000);
    assert_eq!(signature, sign(SECRET, t, &body));

    // Delivered is final.
    env.clock.fetch_add(24 * 3_600_000, Ordering::SeqCst);
    assert_eq!(sender.deliver_due().await, 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn retries_1m_5m_30m_2h_6h_then_failed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("BODY-OF-THE-RECEIVER"))
        .mount(&server)
        .await;
    let env = Env::new();
    let owner = env.owner("alice");
    let hook = env.webhook(owner, "owner", &url_of(&server), &["node.banned"]);
    let id = env.delivery(hook, "evt_00112233445566778899aabb", "node.banned");
    let sender = env.sender(Fixed::of(&[PUBLIC]), *server.address());

    for (n, delay) in [60, 300, 1800, 7200, 21600].into_iter().enumerate() {
        assert_eq!(sender.deliver_due().await, 1, "attempt {}", n + 1);
        let row = env.row(id);
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempt, i64::try_from(n + 1).unwrap());
        assert_eq!(row.response_code, Some(500));
        assert_eq!(row.last_error.as_deref(), Some("http_500"));
        assert_eq!(row.next_at, Some(env.now() + delay * 1000), "delay {delay}");
        // Not due yet: nothing is sent a second early.
        env.clock.fetch_add(delay * 1000 - 1, Ordering::SeqCst);
        assert_eq!(sender.deliver_due().await, 0);
        env.clock.fetch_add(1, Ordering::SeqCst);
    }
    // The sixth failure is the last.
    assert_eq!(sender.deliver_due().await, 1);
    let row = env.row(id);
    assert_eq!(row.status, "failed");
    assert_eq!(row.attempt, 6);
    assert_eq!(row.next_at, None);
    assert!(!format!("{row:?}").contains("BODY-OF-THE-RECEIVER"));
    env.clock.fetch_add(48 * 3_600_000, Ordering::SeqCst);
    assert_eq!(sender.deliver_due().await, 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 6);
}

#[tokio::test]
async fn event_id_stable_across_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let env = Env::new();
    let owner = env.owner("alice");
    let hook = env.webhook(owner, "owner", &url_of(&server), &["node.banned"]);
    let id = env.delivery(hook, "evt_00112233445566778899aabb", "node.banned");
    let sender = env.sender(Fixed::of(&[PUBLIC]), *server.address());
    for _ in 0..3 {
        assert_eq!(sender.deliver_due().await, 1);
        let next = env.row(id).next_at.unwrap();
        env.clock.store(next, Ordering::SeqCst);
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["id"], "evt_00112233445566778899aabb");
        assert_eq!(header(request, "x-mtpool-delivery"), id.to_string());
    }
    // Same body each time; only the signature's time moves on.
    assert_eq!(requests[0].body, requests[2].body);
    assert_ne!(
        header(&requests[0], "x-mtpool-signature"),
        header(&requests[2], "x-mtpool-signature")
    );
    assert_eq!(env.row(id).event_id, "evt_00112233445566778899aabb");
}

#[tokio::test]
async fn private_ip_after_dns_resolve_blocked() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let env = Env::new();
    let owner = env.owner("alice");
    // The URL was fine when it was registered; now the name resolves to a private address, or
    // to a public one together with one.
    for (n, resolved) in [vec!["10.0.0.5"], vec![PUBLIC, "127.0.0.1"], vec!["::1"]]
        .into_iter()
        .enumerate()
    {
        let hook = env.webhook(owner, "owner", &url_of(&server), &["node.banned"]);
        let id = env.delivery(hook, &format!("evt_{n:024}"), "node.banned");
        let sender = env.sender(Fixed::of(&resolved), *server.address());
        assert_eq!(sender.deliver_due().await, 1);
        let row = env.row(id);
        assert_eq!(row.status, "failed", "{resolved:?}");
        assert_eq!(row.last_error.as_deref(), Some("private_address"));
        assert_eq!(row.response_code, None);
        assert_eq!(row.next_at, None);
        assert_eq!(sender.deliver_due().await, 0);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_name_that_does_not_resolve_is_retried() {
    let env = Env::new();
    let owner = env.owner("alice");
    let hook = env.webhook(
        owner,
        "owner",
        "https://hooks.example.com/hook",
        &["node.banned"],
    );
    let id = env.delivery(hook, "evt_00112233445566778899aabb", "node.banned");
    let sender = WebhookSender::new(env.store.clone(), Fixed::of(&[]), env.clock());
    assert_eq!(sender.deliver_due().await, 1);
    let row = env.row(id);
    assert_eq!(row.status, "pending");
    assert_eq!(row.attempt, 1);
    assert_eq!(row.last_error.as_deref(), Some("dns_error"));
    assert_eq!(row.next_at, Some(NOW + 60_000));
}

#[tokio::test]
async fn redirect_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/elsewhere", server.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(path("/elsewhere"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let env = Env::new();
    let owner = env.owner("alice");
    let hook = env.webhook(owner, "owner", &url_of(&server), &["node.banned"]);
    let id = env.delivery(hook, "evt_00112233445566778899aabb", "node.banned");
    let sender = env.sender(Fixed::of(&[PUBLIC]), *server.address());
    assert_eq!(sender.deliver_due().await, 1);
    let row = env.row(id);
    assert_eq!(row.status, "pending");
    assert_eq!(row.attempt, 1);
    assert_eq!(row.response_code, Some(302));
    assert_eq!(row.last_error.as_deref(), Some("http_302"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/hook");
}

#[tokio::test]
async fn a_refused_connection_is_a_short_error_without_the_url() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let env = Env::new();
    let owner = env.owner("alice");
    let url = format!(
        "http://hooks.example.com:{}/hook?key=URLSECRET",
        closed.port()
    );
    let hook = env.webhook(owner, "owner", &url, &["node.banned"]);
    let id = env.delivery(hook, "evt_00112233445566778899aabb", "node.banned");
    let sender = env.sender(Fixed::of(&[PUBLIC]), closed);
    assert_eq!(sender.deliver_due().await, 1);
    let row = env.row(id);
    assert_eq!(row.status, "pending");
    assert_eq!(row.last_error.as_deref(), Some("connect_error"));
    assert_eq!(row.response_code, None);
    assert!(!format!("{row:?}").contains("URLSECRET"));
    assert!(!format!("{row:?}").contains(SECRET));
}

#[tokio::test]
async fn disabled_and_deleted_webhooks_are_skipped() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let env = Env::new();
    let owner = env.owner("alice");
    let off = env.webhook(owner, "owner", &url_of(&server), &["node.banned"]);
    let on = env.webhook(owner, "owner", &url_of(&server), &["node.banned"]);
    let off_row = env.delivery(off, "evt_00000000000000000000000a", "node.banned");
    let on_row = env.delivery(on, "evt_00000000000000000000000b", "node.banned");
    let conn = rusqlite::Connection::open(&env.db).unwrap();
    conn.execute(
        "UPDATE webhooks SET disabled_at = ?1 WHERE id = ?2",
        [NOW, off],
    )
    .unwrap();
    let sender = env.sender(Fixed::of(&[PUBLIC]), *server.address());
    assert_eq!(sender.deliver_due().await, 1);
    assert_eq!(env.row(on_row).status, "delivered");
    assert_eq!(env.row(off_row).status, "pending");
    assert_eq!(env.row(off_row).attempt, 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn at_most_fifty_rows_a_round() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let env = Env::new();
    let owner = env.owner("alice");
    let hook = env.webhook(owner, "owner", &url_of(&server), &["node.banned"]);
    for n in 0..60 {
        env.delivery(hook, &format!("evt_{n:024}"), "node.banned");
    }
    let sender = env.sender(Fixed::of(&[PUBLIC]), *server.address());
    assert_eq!(sender.deliver_due().await, 50);
    assert_eq!(sender.deliver_due().await, 10);
    assert_eq!(sender.deliver_due().await, 0);
    assert!(env.rows().iter().all(|r| r.status == "delivered"));
}

// ----- fan-out -----------------------------------------------------------------------------

fn node(name: &str, owner: i64) -> NodeRecord {
    NodeRecord::new(
        NodeId::parse(name).unwrap(),
        OwnerId(owner),
        "45.33.1.5".parse().unwrap(),
        443,
        EngineKind::Telemt,
        Ts(NOW),
        &Settings::default(),
    )
}

fn event(kind: EventKind, node: Option<&str>, domain: Option<&str>) -> Effect {
    Effect::Emit(Event {
        kind,
        at: Ts(NOW),
        node: node.map(|n| NodeId::parse(n).unwrap()),
        domain: domain.map(str::to_owned),
        detail: json!({ "reason": "test" }),
    })
}

struct Fan {
    env: Env,
    actor: ActorHandle,
    fanout: WebhookFanOut,
    alice: OwnerId,
    bob: OwnerId,
}

fn fan() -> Fan {
    let env = Env::new();
    let alice = env.owner("alice");
    let bob = env.owner("bob");
    let mut state = PoolState::new(Settings::default());
    for n in [node("alpha-00001", alice.0), node("bravo-00001", bob.0)] {
        state.nodes.insert(n.id.clone(), n);
    }
    let actor = Actor::spawn_with(
        state,
        env.store.clone(),
        Vec::new(),
        ActorOptions::default(),
    );
    let fanout = WebhookFanOut::new(env.store.clone());
    Fan {
        env,
        actor,
        fanout,
        alice,
        bob,
    }
}

impl Fan {
    async fn emit(&self, effect: Effect) {
        assert!(self.fanout.handles(&effect));
        self.fanout.run(effect, self.actor.clone()).await;
    }

    fn events_of(&self, hook: i64) -> Vec<String> {
        self.env
            .rows()
            .into_iter()
            .filter(|r| r.webhook_id == hook)
            .map(|r| r.event)
            .collect()
    }
}

#[tokio::test]
async fn bot_receives_only_own_events() {
    let f = fan();
    let url = "https://hooks.example.com/hook";
    let all = &["node.banned", "node.registered", "master.changed"];
    let alice = f.env.webhook(f.alice, "owner", url, all);
    let bob = f.env.webhook(f.bob, "owner", url, all);

    f.emit(event(EventKind::NodeBanned, Some("alpha-00001"), None))
        .await;
    assert_eq!(f.events_of(alice), vec!["node.banned"]);
    assert!(f.events_of(bob).is_empty());

    f.emit(event(EventKind::NodeRegistered, Some("bravo-00001"), None))
        .await;
    assert_eq!(f.events_of(alice), vec!["node.banned"]);
    assert_eq!(f.events_of(bob), vec!["node.registered"]);

    // An event that belongs to no node (a master change) is for the pool-wide hooks only.
    f.emit(event(EventKind::MasterChanged, None, Some("p.example.com")))
        .await;
    assert_eq!(f.events_of(alice).len(), 1);
    assert_eq!(f.events_of(bob).len(), 1);
    // And so is an event of a node the pool does not know.
    f.emit(event(EventKind::NodeBanned, Some("ghost-00001"), None))
        .await;
    assert_eq!(f.env.rows().len(), 2);
}

#[tokio::test]
async fn pool_read_scope_receives_all() {
    let f = fan();
    let url = "https://hooks.example.com/hook";
    let pool = f
        .env
        .webhook(f.alice, "pool", url, &["node.banned", "master.changed"]);
    let own = f.env.webhook(f.alice, "owner", url, &["node.banned"]);

    // Bob's node, and a domain event with no node: the pool hook gets both.
    f.emit(event(EventKind::NodeBanned, Some("bravo-00001"), None))
        .await;
    f.emit(event(EventKind::MasterChanged, None, Some("p.example.com")))
        .await;
    f.emit(event(EventKind::NodeBanned, Some("alpha-00001"), None))
        .await;
    assert_eq!(
        f.events_of(pool),
        vec!["node.banned", "master.changed", "node.banned"]
    );
    assert_eq!(f.events_of(own), vec!["node.banned"]);
}

#[tokio::test]
async fn only_named_events_of_subscribed_hooks_are_queued() {
    let f = fan();
    let hook = f.env.webhook(
        f.alice,
        "owner",
        "https://hooks.example.com/hook",
        &["node.banned"],
    );
    // Not subscribed to this one, and kinds without a webhook name never are.
    f.emit(event(EventKind::NodeRegistered, Some("alpha-00001"), None))
        .await;
    f.emit(event(EventKind::SensorRound, Some("alpha-00001"), None))
        .await;
    f.emit(event(
        EventKind::NodeReverifyFailed,
        Some("alpha-00001"),
        None,
    ))
    .await;
    f.emit(event(EventKind::DnsApplied, None, Some("p.example.com")))
        .await;
    assert!(f.events_of(hook).is_empty());
    let second = f.env.webhook(
        f.alice,
        "owner",
        "https://other.example.com/hook",
        &["node.banned", "node.registered"],
    );
    f.emit(event(EventKind::NodeBanned, Some("alpha-00001"), None))
        .await;
    let rows = f.env.rows();
    assert_eq!(rows.len(), 2);
    // One event, one id, whatever the number of hooks.
    assert_eq!(rows[0].event_id, rows[1].event_id);
    assert_eq!(rows.iter().filter(|r| r.webhook_id == second).count(), 1);
    assert!(rows.iter().all(|r| r.status == "pending" && r.attempt == 0));
}

#[tokio::test]
async fn the_payload_carries_the_event() {
    let f = fan();
    f.env.webhook(
        f.alice,
        "owner",
        "https://hooks.example.com/hook",
        &["node.banned"],
    );
    f.emit(event(
        EventKind::NodeBanned,
        Some("alpha-00001"),
        Some("p.example.com"),
    ))
    .await;
    let row = f.env.rows().remove(0);
    let payload: Value = serde_json::from_str(&row.payload).unwrap();
    let id = payload["id"].as_str().unwrap();
    assert_eq!(id, row.event_id);
    let hex = id.strip_prefix("evt_").expect("evt_ prefix");
    assert_eq!(hex.len(), 16);
    assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(payload["event"], "node.banned");
    assert_eq!(payload["at"], mtpool_protocol::rfc3339(NOW));
    assert_eq!(payload["node"], "alpha-00001");
    assert_eq!(payload["domain"], "p.example.com");
    assert_eq!(payload["detail"], json!({ "reason": "test" }));
    assert!(!row.payload.contains(SECRET));
}

#[tokio::test]
async fn every_core_mapping_is_delivered_by_name() {
    let f = fan();
    let names = [
        "node.registered",
        "node.quarantined",
        "node.recovered",
        "node.banned",
        "node.dead",
        "node.pruned",
        "ban.lifted",
        "master.changed",
    ];
    let hook = f
        .env
        .webhook(f.alice, "pool", "https://hooks.example.com/hook", &names);
    for kind in [
        EventKind::NodeRegistered,
        EventKind::NodeQuarantined,
        EventKind::NodeRecovered,
        EventKind::NodeBanned,
        EventKind::NodeDead,
        EventKind::NodePruned,
        EventKind::BanLifted,
        EventKind::MasterChanged,
    ] {
        f.emit(event(kind, Some("alpha-00001"), None)).await;
    }
    assert_eq!(f.events_of(hook), names);
}
