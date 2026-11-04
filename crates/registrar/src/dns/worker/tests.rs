//! The worker with a fake provider: what it asks for, in which order, and what it reports.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mtpool_core::{LiveRecord, NewRecord, RecordKind};

use super::*;
use crate::dns::{DnsError, ProviderRecord};

const DOMAIN: &str = "p1.example.com";

struct FakeProvider {
    calls: Mutex<Vec<String>>,
    zone: Result<Option<String>, DnsError>,
    live: Vec<ProviderRecord>,
    list_error: Option<DnsError>,
    /// The answers of the next creates, one each; an empty queue answers `Ok`.
    create_answers: Mutex<VecDeque<Result<String, DnsError>>>,
    delete_error: Option<DnsError>,
    delay: Duration,
    active: AtomicUsize,
    most_active: AtomicUsize,
}

impl FakeProvider {
    fn new(live: Vec<ProviderRecord>) -> FakeProvider {
        FakeProvider {
            calls: Mutex::new(Vec::new()),
            zone: Ok(Some("zone-1".to_owned())),
            live,
            list_error: None,
            create_answers: Mutex::new(VecDeque::new()),
            delete_error: None,
            delay: Duration::ZERO,
            active: AtomicUsize::new(0),
            most_active: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("lock").clone()
    }

    fn note(&self, call: String) {
        self.calls.lock().expect("lock").push(call);
    }

    async fn busy(&self) {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_active.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl DnsProvider for FakeProvider {
    async fn zone_id(&self, domain: &str) -> Result<Option<String>, DnsError> {
        self.note(format!("zone {domain}"));
        self.busy().await;
        self.zone.clone()
    }

    async fn list(&self, zone: &str, name: &str) -> Result<Vec<ProviderRecord>, DnsError> {
        self.note(format!("list {zone} {name}"));
        self.busy().await;
        match &self.list_error {
            Some(error) => Err(error.clone()),
            None => Ok(self.live.clone()),
        }
    }

    async fn create(
        &self,
        zone: &str,
        record: NewRecord,
        ttl: u32,
        proxied: bool,
    ) -> Result<String, DnsError> {
        self.note(format!(
            "create {zone} {:?} {} {} ttl={ttl} proxied={proxied}",
            record.kind, record.name, record.content
        ));
        self.busy().await;
        self.create_answers
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| Ok("new-id".to_owned()))
    }

    async fn delete(&self, zone: &str, id: &str) -> Result<(), DnsError> {
        self.note(format!("delete {zone} {id}"));
        self.busy().await;
        match &self.delete_error {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

fn live(id: &str, kind: RecordKind, content: &str, managed: bool) -> ProviderRecord {
    ProviderRecord {
        live: LiveRecord {
            id: id.to_owned(),
            name: DOMAIN.to_owned(),
            kind,
            content: content.to_owned(),
            proxied: false,
        },
        managed,
    }
}

fn a_set(ips: &[[u8; 4]]) -> RecordValue {
    RecordValue::A(ips.iter().map(|o| Ipv4Addr::from(*o)).collect())
}

fn done(command: &Command) -> (&str, Generation, &Result<(), String>) {
    match command {
        Command::DnsDone {
            domain,
            generation,
            result,
        } => (domain.as_str(), *generation, result),
        other => panic!("not a DnsDone: {other:?}"),
    }
}

async fn push(provider: &Arc<FakeProvider>, desired: RecordValue, generation: u64) -> Command {
    DnsWorker::new(provider.clone())
        .push(
            DOMAIN.to_owned(),
            Generation(generation),
            desired,
            60,
            false,
        )
        .await
}

#[tokio::test]
async fn create_before_delete_order_observed() {
    // One managed record at an old address; the new set has two others.
    let provider = Arc::new(FakeProvider::new(vec![live(
        "old-1",
        RecordKind::A,
        "192.0.2.9",
        true,
    )]));

    let command = push(&provider, a_set(&[[192, 0, 2, 1], [192, 0, 2, 2]]), 3).await;

    assert_eq!(
        provider.calls(),
        [
            "zone p1.example.com",
            "list zone-1 p1.example.com",
            "create zone-1 A p1.example.com 192.0.2.1 ttl=60 proxied=false",
            "create zone-1 A p1.example.com 192.0.2.2 ttl=60 proxied=false",
            "delete zone-1 old-1",
        ],
        "every create comes before the delete"
    );
    assert_eq!(done(&command), (DOMAIN, Generation(3), &Ok(())));
}

#[tokio::test]
async fn ttl_and_proxied_are_the_ones_given() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    DnsWorker::new(provider.clone())
        .push(
            DOMAIN.to_owned(),
            Generation(1),
            a_set(&[[192, 0, 2, 1]]),
            300,
            true,
        )
        .await;
    assert_eq!(
        provider.calls()[2],
        "create zone-1 A p1.example.com 192.0.2.1 ttl=300 proxied=true"
    );
}

#[tokio::test]
async fn nothing_to_change_is_a_success_without_writes() {
    let provider = Arc::new(FakeProvider::new(vec![live(
        "keep-1",
        RecordKind::A,
        "192.0.2.1",
        true,
    )]));
    let command = push(&provider, a_set(&[[192, 0, 2, 1]]), 1).await;
    assert_eq!(provider.calls().len(), 2, "a zone lookup and a list only");
    assert_eq!(done(&command).2, &Ok(()));
}

#[tokio::test]
async fn a_cname_is_planned_and_created() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let command = push(
        &provider,
        RecordValue::Cname("p2.example.com".to_owned()),
        1,
    )
    .await;
    assert_eq!(
        provider.calls()[2],
        "create zone-1 Cname p1.example.com p2.example.com ttl=60 proxied=false"
    );
    assert_eq!(done(&command).2, &Ok(()));
}

#[tokio::test]
async fn records_the_pool_did_not_make_are_never_deleted() {
    let provider = Arc::new(FakeProvider::new(vec![
        live("mine-1", RecordKind::A, "192.0.2.9", true),
        live("theirs-1", RecordKind::A, "192.0.2.8", false),
        live("txt-1", RecordKind::Other("TXT".to_owned()), "hello", true),
    ]));
    push(&provider, a_set(&[[192, 0, 2, 1]]), 1).await;
    let deletes: Vec<String> = provider
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("delete"))
        .collect();
    assert_eq!(deletes, ["delete zone-1 mine-1"]);
}

#[tokio::test]
async fn the_first_error_stops_the_sequence() {
    let provider = FakeProvider::new(vec![live("old-1", RecordKind::A, "192.0.2.9", true)]);
    *provider.create_answers.lock().expect("lock") =
        VecDeque::from([Ok("new-1".to_owned()), Err(DnsError::Auth)]);
    let provider = Arc::new(provider);

    let command = push(&provider, a_set(&[[192, 0, 2, 1], [192, 0, 2, 2]]), 1).await;

    let calls = provider.calls();
    assert_eq!(calls.len(), 4, "zone, list and two creates: {calls:?}");
    assert!(
        !calls.iter().any(|c| c.starts_with("delete")),
        "no delete after a failed create: {calls:?}"
    );
    assert_eq!(done(&command).2, &Err("auth".to_owned()));
}

#[tokio::test]
async fn a_failed_delete_is_reported_and_stops_there() {
    let mut provider = FakeProvider::new(vec![
        live("old-1", RecordKind::A, "192.0.2.9", true),
        live("old-2", RecordKind::A, "192.0.2.8", true),
    ]);
    provider.delete_error = Some(DnsError::Http(500));
    let provider = Arc::new(provider);
    let command = push(&provider, a_set(&[[192, 0, 2, 1]]), 1).await;
    let deletes = provider
        .calls()
        .iter()
        .filter(|c| c.starts_with("delete"))
        .count();
    assert_eq!(deletes, 1, "the second delete never runs");
    assert_eq!(done(&command).2, &Err("http_500".to_owned()));
}

#[tokio::test]
async fn a_zone_that_is_not_found_is_reported_and_nothing_else_runs() {
    let mut provider = FakeProvider::new(vec![]);
    provider.zone = Ok(None);
    let provider = Arc::new(provider);
    let command = push(&provider, a_set(&[[192, 0, 2, 1]]), 1).await;
    assert_eq!(provider.calls(), ["zone p1.example.com"]);
    assert_eq!(done(&command).2, &Err("zone_not_found".to_owned()));
}

#[tokio::test]
async fn every_provider_error_has_its_short_reason() {
    let cases = [
        (DnsError::NotConfigured, "not_configured"),
        (DnsError::Auth, "auth"),
        (DnsError::ZoneNotFound, "zone_not_found"),
        (
            DnsError::RateLimited {
                retry_after: Duration::from_secs(3),
            },
            "rate_limited",
        ),
        (DnsError::Timeout, "timeout"),
        (DnsError::Http(503), "http_503"),
        (DnsError::Transport("refused".to_owned()), "transport"),
        (DnsError::Unexpected("shape".to_owned()), "unexpected"),
    ];
    for (error, reason) in cases {
        let mut provider = FakeProvider::new(vec![]);
        provider.zone = Err(error.clone());
        let provider = Arc::new(provider);
        let command = push(&provider, a_set(&[[192, 0, 2, 1]]), 1).await;
        assert_eq!(done(&command).2, &Err(reason.to_owned()), "{error:?}");
        assert_eq!(provider.calls().len(), 1, "stops at the first call");
    }
}

#[tokio::test]
async fn an_unmanaged_record_in_the_way_is_a_plan_error_with_no_write() {
    let provider = Arc::new(FakeProvider::new(vec![live(
        "theirs-1",
        RecordKind::Cname,
        "elsewhere.example.net",
        false,
    )]));
    let command = push(&provider, a_set(&[[192, 0, 2, 1]]), 1).await;
    let result = done(&command).2;
    let Err(message) = result else {
        panic!("a conflict is not applied: {result:?}")
    };
    assert!(message.starts_with("plan:"), "{message}");
    assert!(message.contains("theirs-1"), "{message}");
    assert_eq!(
        provider.calls().len(),
        2,
        "a zone lookup and a list, no write"
    );
}

#[tokio::test]
async fn an_empty_desired_state_is_a_plan_error_with_no_write() {
    let provider = Arc::new(FakeProvider::new(vec![live(
        "mine-1",
        RecordKind::A,
        "192.0.2.9",
        true,
    )]));
    let command = push(&provider, RecordValue::A(vec![]), 1).await;
    let Err(message) = done(&command).2 else {
        panic!("an empty state is never applied")
    };
    assert!(message.starts_with("plan:"), "{message}");
    assert!(!provider.calls().iter().any(|c| c.starts_with("delete")));
}

#[tokio::test]
async fn the_result_carries_the_generation_it_was_given() {
    // What the result is worth is `core`'s business; the worker only hands the generation
    // back unchanged, whatever the outcome.
    for generation in [0, 1, 41, u64::MAX] {
        let provider = Arc::new(FakeProvider::new(vec![]));
        let command = push(&provider, a_set(&[[192, 0, 2, 1]]), generation).await;
        assert_eq!(done(&command).1, Generation(generation));
    }
    let mut broken = FakeProvider::new(vec![]);
    broken.zone = Err(DnsError::Timeout);
    let command = push(&Arc::new(broken), a_set(&[[192, 0, 2, 1]]), 9).await;
    assert_eq!(done(&command).1, Generation(9));
}

#[tokio::test]
async fn pushes_of_one_domain_do_not_overlap() {
    let mut provider = FakeProvider::new(vec![]);
    provider.delay = Duration::from_millis(20);
    let provider = Arc::new(provider);
    let worker = DnsWorker::new(provider.clone());
    let a = worker.push(
        DOMAIN.to_owned(),
        Generation(1),
        a_set(&[[192, 0, 2, 1]]),
        60,
        false,
    );
    let b = worker.push(
        DOMAIN.to_owned(),
        Generation(2),
        a_set(&[[192, 0, 2, 2]]),
        60,
        false,
    );
    let _ = tokio::join!(a, b);
    assert_eq!(provider.most_active.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pushes_of_different_domains_run_together() {
    let mut provider = FakeProvider::new(vec![]);
    provider.delay = Duration::from_millis(50);
    let provider = Arc::new(provider);
    let worker = DnsWorker::new(provider.clone());
    let a = worker.push(
        "p1.example.com".to_owned(),
        Generation(1),
        a_set(&[[192, 0, 2, 1]]),
        60,
        false,
    );
    let b = worker.push(
        "p2.example.com".to_owned(),
        Generation(1),
        a_set(&[[192, 0, 2, 2]]),
        60,
        false,
    );
    let _ = tokio::join!(a, b);
    assert_eq!(provider.most_active.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn the_driver_takes_push_dns_only() {
    let worker = DnsWorker::new(Arc::new(FakeProvider::new(vec![])));
    let push_dns = Effect::PushDns {
        domain: DOMAIN.to_owned(),
        generation: Generation(1),
        desired: a_set(&[[192, 0, 2, 1]]),
    };
    assert!(worker.handles(&push_dns));
    let tick_effect = Effect::Emit(mtpool_core::Event {
        kind: mtpool_core::EventKind::DnsApplied,
        at: mtpool_core::Ts(0),
        node: None,
        domain: None,
        detail: serde_json::json!({}),
    });
    assert!(!worker.handles(&tick_effect));
}

#[test]
fn a_provider_error_never_prints_more_than_its_reason() {
    // The reason is what goes into the event; it holds no address and no token.
    for error in [
        DnsError::Transport("connect error to 203.0.113.5".to_owned()),
        DnsError::Unexpected("body: {...}".to_owned()),
    ] {
        let reason = error.reason();
        assert!(
            !reason.contains("203.0.113.5") && !reason.contains('{'),
            "{reason}"
        );
    }
}
