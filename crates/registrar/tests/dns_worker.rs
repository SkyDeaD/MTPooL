//! The DNS worker with the real state owner: it must never hold the owner up, and what it
//! reports must turn into the events and retries `core` decides on.
//!
//! The Cloudflare side is a local server that answers with the recorded shapes in
//! `tests/fixtures/cloudflare`.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::*;
use mtpool_core::{
    Command, Effect, Event, EventKind, Generation, NewRecord, RecordValue, Reply, Settings, Ts,
};
use mtpool_registrar::actor::{Actor, ActorHandle, DriverFuture, EffectDriver};
use mtpool_registrar::dns::cloudflare::CloudflareDns;
use mtpool_registrar::dns::worker::DnsWorker;
use mtpool_registrar::dns::{DnsError, DnsProvider, ProviderRecord};
use reqwest::Url;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const INVALID_TOKEN: &str = include_str!("fixtures/cloudflare/error_invalid_token.json");
const ZONES_FOUND: &str = include_str!("fixtures/cloudflare/zones_found.json");
const ZONES_NONE: &str = include_str!("fixtures/cloudflare/zones_none.json");
const CREATE: &str = include_str!("fixtures/cloudflare/create.json");
const DOMAIN: &str = "p.example.com";
const NODE_IP: &str = "45.33.1.5";
const RECORDS: &str = "/client/v4/zones/023e105f4ecef8ad9ca31a8372d0c353/dns_records";

fn now() -> Ts {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_millis();
    Ts(i64::try_from(ms).expect("fits"))
}

/// Keeps every event the state owner emits.
#[derive(Clone, Default)]
struct EventLog(Arc<Mutex<Vec<Event>>>);

impl EventLog {
    fn of(&self, kind: EventKind) -> Vec<Event> {
        self.0
            .lock()
            .expect("lock")
            .iter()
            .filter(|e| e.kind == kind)
            .cloned()
            .collect()
    }

    async fn wait_for(&self, kind: EventKind) -> Event {
        let started = Instant::now();
        loop {
            if let Some(event) = self.of(kind).into_iter().next() {
                return event;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "no {kind:?} event came"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl EffectDriver for EventLog {
    fn handles(&self, effect: &Effect) -> bool {
        matches!(effect, Effect::Emit(_))
    }

    fn run(&self, effect: Effect, _results: ActorHandle) -> DriverFuture {
        let log = Arc::clone(&self.0);
        Box::pin(async move {
            if let Effect::Emit(event) = effect {
                log.lock().expect("lock").push(event);
            }
        })
    }
}

fn settings() -> Settings {
    Settings {
        domains: vec![DOMAIN.to_owned()],
        dns_ttl: 120,
        dns_proxied: true,
        ..Settings::default()
    }
}

fn pool(drivers: Vec<Arc<dyn EffectDriver>>) -> (tempfile::TempDir, ActorHandle) {
    let (dir, store) = open_store();
    let state = state_with(healthy_node("alpha-00001", now(), 0), settings());
    (dir, Actor::spawn(state, store, drivers))
}

async fn dns_entry(handle: &ActorHandle) -> mtpool_core::DnsState {
    handle
        .inspect(|s| s.dns.get(DOMAIN).cloned().expect("a DNS entry"))
        .await
        .expect("actor runs")
}

async fn tick(handle: &ActorHandle) {
    let reply = tokio::time::timeout(Duration::from_millis(500), handle.call(Command::Tick))
        .await
        .expect("a tick is answered at once")
        .expect("actor runs");
    assert_eq!(reply, Reply::Ok);
}

/// A provider that answers like an empty zone and notes what it was asked.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl DnsProvider for Recorder {
    async fn zone_id(&self, _domain: &str) -> Result<Option<String>, DnsError> {
        Ok(Some("zone-1".to_owned()))
    }
    async fn list(&self, _zone: &str, _name: &str) -> Result<Vec<ProviderRecord>, DnsError> {
        Ok(vec![])
    }
    async fn create(
        &self,
        _zone: &str,
        record: NewRecord,
        ttl: u32,
        proxied: bool,
    ) -> Result<String, DnsError> {
        self.calls.lock().expect("lock").push(format!(
            "create {} {} ttl={ttl} proxied={proxied}",
            record.name, record.content
        ));
        Ok("new-id".to_owned())
    }
    async fn delete(&self, _zone: &str, _id: &str) -> Result<(), DnsError> {
        Ok(())
    }
}

/// A provider that is asked once and never answers.
struct Hanging {
    asked: Arc<AtomicBool>,
}

#[async_trait]
impl DnsProvider for Hanging {
    async fn zone_id(&self, _domain: &str) -> Result<Option<String>, DnsError> {
        self.asked.store(true, Ordering::SeqCst);
        std::future::pending().await
    }
    async fn list(&self, _zone: &str, _name: &str) -> Result<Vec<ProviderRecord>, DnsError> {
        std::future::pending().await
    }
    async fn create(&self, _: &str, _: NewRecord, _: u32, _: bool) -> Result<String, DnsError> {
        std::future::pending().await
    }
    async fn delete(&self, _zone: &str, _id: &str) -> Result<(), DnsError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn selection_continues_while_provider_hangs() {
    let asked = Arc::new(AtomicBool::new(false));
    let worker = DnsWorker::new(Arc::new(Hanging {
        asked: Arc::clone(&asked),
    }));
    let (_dir, handle) = pool(vec![Arc::new(worker)]);

    tick(&handle).await;
    let waited = Instant::now();
    while !asked.load(Ordering::SeqCst) {
        assert!(waited.elapsed() < Duration::from_secs(3), "no push started");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The push is stuck in the provider. The owner still takes ticks and heartbeats.
    tick(&handle).await;
    let answer = tokio::time::timeout(
        Duration::from_millis(200),
        handle.call(heartbeat("alpha-00001", [45, 33, 1, 5])),
    )
    .await
    .expect("a heartbeat is answered while the provider hangs")
    .expect("actor runs");
    assert_eq!(answer, Reply::Ok);
    let entry = dns_entry(&handle).await;
    assert!(
        entry.in_flight.is_some(),
        "core still counts the push as running"
    );
    assert_eq!(entry.applied, None);
}

#[tokio::test]
async fn a_push_ends_as_dns_applied_with_the_ttl_and_proxied_of_the_settings() {
    let provider = Arc::new(Recorder::default());
    let log = EventLog::default();
    let (_dir, handle) = pool(vec![
        Arc::new(DnsWorker::new(provider.clone())),
        Arc::new(log.clone()),
    ]);

    tick(&handle).await;
    let event = log.wait_for(EventKind::DnsApplied).await;

    assert_eq!(event.domain.as_deref(), Some(DOMAIN));
    assert_eq!(
        *provider.calls.lock().expect("lock"),
        [format!("create {DOMAIN} {NODE_IP} ttl=120 proxied=true")]
    );
    let entry = dns_entry(&handle).await;
    assert_eq!(
        entry.applied,
        Some(RecordValue::A(vec![NODE_IP.parse().expect("ip")]))
    );
    assert!(log.of(EventKind::DnsFailed).is_empty());
}

#[tokio::test]
async fn stale_generation_dropped() {
    let provider = Arc::new(Recorder::default());
    let log = EventLog::default();
    let worker = DnsWorker::new(provider.clone());
    let (_dir, handle) = pool(vec![Arc::new(log.clone())]);
    tick(&handle).await;
    let entry = dns_entry(&handle).await;
    let desired = entry.desired.clone();

    // A push of a generation `core` has moved past: the worker does its job and reports the
    // generation it was given; `core` does not apply it.
    let stale = Generation(entry.generation.0 + 5);
    worker
        .run(
            Effect::PushDns {
                domain: DOMAIN.to_owned(),
                generation: stale,
                desired: desired.clone(),
            },
            handle.clone(),
        )
        .await;
    handle.call(barrier()).await.expect("actor runs");
    assert_eq!(dns_entry(&handle).await.applied, None);
    assert!(log.of(EventKind::DnsApplied).is_empty());

    worker
        .run(
            Effect::PushDns {
                domain: DOMAIN.to_owned(),
                generation: entry.generation,
                desired,
            },
            handle.clone(),
        )
        .await;
    log.wait_for(EventKind::DnsApplied).await;
    assert!(dns_entry(&handle).await.applied.is_some());
}

// ---- Cloudflare failures, through the real client ----

fn cloudflare_at(server: &MockServer) -> Arc<CloudflareDns> {
    Arc::new(
        CloudflareDns::new(
            Url::parse(&server.uri()).expect("uri"),
            Arc::new(|| Some("test-token-not-a-real-credential".to_owned())),
        )
        .expect("client"),
    )
}

async fn mount_zone(server: &MockServer, name: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path("/client/v4/zones"))
        .and(query_param("name", name))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_string(body),
        )
        .mount(server)
        .await;
}

fn writes_seen(requests: &[wiremock::Request]) -> (usize, usize) {
    let count = |m: &str| requests.iter().filter(|r| r.method.as_str() == m).count();
    (count("POST"), count("DELETE"))
}

#[tokio::test]
async fn token_revoked_403_emits_event_and_slow_retry_no_deletes() {
    let server = MockServer::start().await;
    mount_zone(&server, DOMAIN, ZONES_NONE).await;
    mount_zone(&server, "example.com", ZONES_FOUND).await;
    // The domain holds a record the pool made earlier, at another address: a plain push would
    // create the new one and then delete this one.
    let old = json!({
        "result": [{
            "id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1", "name": DOMAIN, "type": "A",
            "content": "192.0.2.9", "proxied": false, "ttl": 60, "comment": "mtpool", "tags": []
        }],
        "result_info": {"page": 1, "per_page": 100, "count": 1, "total_count": 1, "total_pages": 1},
        "success": true, "errors": [], "messages": []
    })
    .to_string();
    Mock::given(method("GET"))
        .and(path(RECORDS))
        .respond_with(ResponseTemplate::new(200).set_body_string(old))
        .mount(&server)
        .await;
    // The token has been revoked since the lookup: the write is refused.
    Mock::given(method("POST"))
        .and(path(RECORDS))
        .respond_with(ResponseTemplate::new(403).set_body_string(INVALID_TOKEN))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200).set_body_string(CREATE))
        .expect(0)
        .mount(&server)
        .await;
    let log = EventLog::default();
    let (_dir, handle) = pool(vec![
        Arc::new(DnsWorker::new(cloudflare_at(&server))),
        Arc::new(log.clone()),
    ]);

    tick(&handle).await;
    let event = log.wait_for(EventKind::DnsFailed).await;

    assert_eq!(event.domain.as_deref(), Some(DOMAIN));
    assert_eq!(event.detail["error"], "auth");
    assert_eq!(event.detail["retry_in_sec"], 5, "the first backoff step");
    // The old record is still there: nothing was deleted.
    let requests = server.received_requests().await.expect("recording");
    assert_eq!(writes_seen(&requests), (1, 0));
    // Selection goes on, and the next push waits for the backoff instead of hammering.
    let entry = dns_entry(&handle).await;
    assert_eq!(entry.retries, 1);
    assert!(entry.next_try.0 > now().0, "the retry is in the future");
    assert_eq!(entry.applied, None);
    tick(&handle).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let requests = server.received_requests().await.expect("recording");
    assert_eq!(writes_seen(&requests), (1, 0), "no second attempt yet");
}

#[tokio::test]
async fn zone_not_found_emits_event() {
    let server = MockServer::start().await;
    mount_zone(&server, DOMAIN, ZONES_NONE).await;
    mount_zone(&server, "example.com", ZONES_NONE).await;
    let log = EventLog::default();
    let (_dir, handle) = pool(vec![
        Arc::new(DnsWorker::new(cloudflare_at(&server))),
        Arc::new(log.clone()),
    ]);

    tick(&handle).await;
    let event = log.wait_for(EventKind::DnsFailed).await;

    assert_eq!(event.domain.as_deref(), Some(DOMAIN));
    assert_eq!(event.detail["error"], "zone_not_found");
    let requests = server.received_requests().await.expect("recording");
    assert_eq!(writes_seen(&requests), (0, 0));
    assert!(
        requests.iter().all(|r| r.url.path() == "/client/v4/zones"),
        "no record call without a zone"
    );
    // The pool goes on: the node is still healthy and ticks are answered.
    tick(&handle).await;
    let state: Value = serde_json::from_str(&handle.snapshots().load().nodes_json).expect("json");
    assert_eq!(state[0]["state"], "healthy");
}

#[tokio::test]
async fn a_missing_token_is_reported_without_a_request() {
    let server = MockServer::start().await;
    let none = Arc::new(
        CloudflareDns::new(Url::parse(&server.uri()).expect("uri"), Arc::new(|| None))
            .expect("client"),
    );
    let log = EventLog::default();
    let (_dir, handle) = pool(vec![Arc::new(DnsWorker::new(none)), Arc::new(log.clone())]);

    tick(&handle).await;
    let event = log.wait_for(EventKind::DnsFailed).await;

    assert_eq!(event.detail["error"], "not_configured");
    assert!(
        server
            .received_requests()
            .await
            .expect("recording")
            .is_empty()
    );
}
