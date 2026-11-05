//! The live numbers of the public pages: server-sent events anyone may read.
//!
//! One worker makes each frame and a broadcast channel hands it to every open stream, so the
//! work does not grow with the audience. The numbers come from the published snapshot: neither
//! the worker nor a new stream talks to the state owner. A client that falls behind is dropped
//! and reconnects; the numbers it missed are of no use to it. A client that reads well leaves
//! too when its stream is old, so that no one holds a place for good. The browser reconnects by
//! itself, and each stream has its own age and its own wait, so that the tabs of one moment do
//! not leave or come back at one moment.

use std::collections::HashMap;
use std::net::IpAddr;
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::response::Response;
use mtpool_core::{Effect, EventKind, LiveCounts, Ts};
use mtpool_protocol::public::LiveClients;
use mtpool_protocol::rfc3339;
use rand::RngExt;
use tokio::sync::{broadcast, watch};
use tokio::time::MissedTickBehavior;

use super::stream::{KEEPALIVE, StreamTerms, event_stream};
use crate::actor::{ActorHandle, Clock, DriverFuture, EffectDriver};
use crate::auth::ratelimit::key;

/// Frames a client may be behind before it is dropped.
const CAPACITY: usize = 8;
/// What the browser waits before it reconnects, in milliseconds.
const RETRY_MS: RangeInclusive<u64> = 5_000..=15_000;

/// How the streams tick and how many may be open.
#[derive(Debug, Clone, Copy)]
pub struct LiveOptions {
    /// How often the numbers go out.
    pub clients_every: Duration,
    /// How long a change of the pool may wait for its frame. Changes in one such wait become
    /// one frame.
    pub changed_every: Duration,
    pub keepalive: Duration,
    /// Streams of the whole service.
    pub max_streams: usize,
    /// Streams of one address. Not one: many people may share an address behind CGNAT.
    pub max_per_address: usize,
    /// The longest a stream stays open. Each one lives between half of this and all of it.
    pub max_age: Duration,
}

impl Default for LiveOptions {
    fn default() -> LiveOptions {
        LiveOptions {
            clients_every: Duration::from_secs(5),
            changed_every: Duration::from_secs(1),
            keepalive: KEEPALIVE,
            max_streams: 2000,
            max_per_address: 20,
            max_age: Duration::from_secs(30 * 60),
        }
    }
}

#[derive(Debug, Default)]
struct Slots {
    total: usize,
    by_address: HashMap<IpAddr, usize>,
}

struct Inner {
    tx: broadcast::Sender<Bytes>,
    closed: watch::Sender<bool>,
    changed: AtomicBool,
    options: LiveOptions,
    slots: Mutex<Slots>,
}

/// A place taken by one open stream, given back when the stream ends.
struct Slot {
    inner: Arc<Inner>,
    address: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut slots = self.inner.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.total = slots.total.saturating_sub(1);
        if let Some(count) = slots.by_address.get_mut(&self.address) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                slots.by_address.remove(&self.address);
            }
        }
    }
}

/// The fan-out of the live numbers. Clones share it.
#[derive(Clone)]
pub struct LiveFan {
    inner: Arc<Inner>,
}

impl Default for LiveFan {
    fn default() -> LiveFan {
        LiveFan::new()
    }
}

impl LiveFan {
    pub fn new() -> LiveFan {
        LiveFan::with_options(LiveOptions::default())
    }

    pub fn with_options(options: LiveOptions) -> LiveFan {
        let (tx, _) = broadcast::channel(CAPACITY);
        let (closed, _) = watch::channel(false);
        LiveFan {
            inner: Arc::new(Inner {
                tx,
                closed,
                changed: AtomicBool::new(false),
                options,
                slots: Mutex::new(Slots::default()),
            }),
        }
    }

    /// Ends every open stream and every later one at once. Called when the service stops, so
    /// that open streams do not hold up the drain.
    pub fn close(&self) {
        self.inner.closed.send_replace(true);
    }

    /// The response of `GET /stream`, or `None` when no more streams may be open.
    pub fn subscribe(&self, actor: ActorHandle, now: Ts, ip: IpAddr) -> Option<Response> {
        let slot = self.take_slot(ip)?;
        let first = clients_frame(&actor.snapshots().load().live, now);
        let options = &self.inner.options;
        let mut rng = rand::rng();
        let terms = StreamTerms {
            keepalive: options.keepalive,
            retry_ms: rng.random_range(RETRY_MS),
            max_age: Some(rng.random_range(options.max_age / 2..=options.max_age)),
        };
        Some(event_stream(
            self.inner.tx.subscribe(),
            self.inner.closed.subscribe(),
            terms,
            actor,
            Some(first),
            slot,
        ))
    }

    fn take_slot(&self, ip: IpAddr) -> Option<Slot> {
        let address = key(ip);
        let options = &self.inner.options;
        let mut slots = self.inner.slots.lock().unwrap_or_else(|e| e.into_inner());
        let held = slots.by_address.get(&address).copied().unwrap_or(0);
        if slots.total >= options.max_streams || held >= options.max_per_address {
            return None;
        }
        slots.total += 1;
        slots.by_address.insert(address, held + 1);
        Some(Slot {
            inner: Arc::clone(&self.inner),
            address,
        })
    }

    /// The worker: makes the frames until the fan is closed or the state owner is gone.
    pub async fn run(self, actor: ActorHandle, clock: Clock) {
        let options = self.inner.options;
        let snapshots = actor.snapshots();
        let mut closed = self.inner.closed.subscribe();
        let mut numbers = tokio::time::interval(options.clients_every);
        numbers.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut changes = tokio::time::interval(options.changed_every);
        changes.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = numbers.tick() => {
                    // Nobody is watching: nothing to make the frame for.
                    if self.inner.tx.receiver_count() > 0 {
                        let frame = clients_frame(&snapshots.load().live, clock());
                        let _ = self.inner.tx.send(frame);
                    }
                }
                _ = changes.tick() => {
                    if self.inner.changed.swap(false, Ordering::Relaxed) {
                        let _ = self.inner.tx.send(Bytes::from_static(CHANGED_FRAME));
                    }
                }
                _ = closed.changed() => break,
                () = actor.stopped() => break,
            }
        }
    }
}

const CHANGED_FRAME: &[u8] = b"event: changed\ndata: {}\n\n";

fn clients_frame(counts: &LiveCounts, now: Ts) -> Bytes {
    let by_id = |map: &std::collections::BTreeMap<mtpool_core::NodeId, u64>| {
        map.iter()
            .map(|(id, n)| (id.as_str().to_owned(), *n))
            .collect()
    };
    let document = LiveClients {
        at: rfc3339(now.0),
        total: counts.total,
        domains: counts.domains.clone(),
        nodes: by_id(&counts.nodes),
        other_nodes: by_id(&counts.other_nodes),
    };
    let data = serde_json::to_string(&document).expect("the numbers serialize");
    Bytes::from(format!("event: clients\ndata: {data}\n\n"))
}

/// The events after which a public page shows something else than it did.
fn changes_the_pages(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::NodeRegistered
            | EventKind::NodeQuarantined
            | EventKind::NodeRecovered
            | EventKind::NodeBanned
            | EventKind::NodeReverifyFailed
            | EventKind::NodeIpChanged
            | EventKind::NodeDead
            | EventKind::NodePruned
            | EventKind::BanLifted
            | EventKind::MasterChanged
            | EventKind::SensorRound
            | EventKind::DnsApplied
            | EventKind::SrmdAction
    )
}

impl EffectDriver for LiveFan {
    fn handles(&self, effect: &Effect) -> bool {
        matches!(effect, Effect::Emit(event) if changes_the_pages(event.kind))
    }

    fn run(&self, _effect: Effect, _results: ActorHandle) -> DriverFuture {
        // Only a mark: the worker sends one frame for all the changes of a moment.
        self.inner.changed.store(true, Ordering::Relaxed);
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn limited(max_streams: usize, max_per_address: usize) -> LiveFan {
        LiveFan::with_options(LiveOptions {
            max_streams,
            max_per_address,
            ..LiveOptions::default()
        })
    }

    #[test]
    fn places_are_counted_per_address_and_given_back() {
        let fan = limited(10, 2);
        let a = IpAddr::V4(Ipv4Addr::new(45, 33, 1, 5));
        let b = IpAddr::V4(Ipv4Addr::new(45, 33, 1, 6));
        let first = fan.take_slot(a).unwrap();
        let _second = fan.take_slot(a).unwrap();
        assert!(fan.take_slot(a).is_none());
        let _other = fan.take_slot(b).unwrap();
        drop(first);
        assert!(fan.take_slot(a).is_some());
    }

    #[test]
    fn an_ipv6_network_counts_as_one_address() {
        let fan = limited(10, 1);
        let one = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 1, 0, 0, 0, 1));
        let two = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 1, 0, 0, 0, 2));
        let _held = fan.take_slot(one).unwrap();
        assert!(fan.take_slot(two).is_none());
    }

    #[test]
    fn the_whole_service_has_a_limit_too() {
        let fan = limited(1, 5);
        let _held = fan
            .take_slot(IpAddr::V4(Ipv4Addr::new(45, 33, 1, 5)))
            .unwrap();
        assert!(
            fan.take_slot(IpAddr::V4(Ipv4Addr::new(45, 33, 1, 6)))
                .is_none()
        );
    }

    #[test]
    fn the_noise_of_the_service_is_not_a_change() {
        assert!(changes_the_pages(EventKind::NodeBanned));
        assert!(!changes_the_pages(EventKind::RolloutChanged));
        assert!(!changes_the_pages(EventKind::DnsFailed));
    }

    #[test]
    fn a_frame_says_only_the_numbers() {
        let frame = clients_frame(&LiveCounts::default(), Ts(0));
        assert_eq!(
            std::str::from_utf8(&frame).unwrap(),
            "event: clients\ndata: {\"at\":\"1970-01-01T00:00:00Z\",\"total\":0,\"domains\":{},\"nodes\":{}}\n\n"
        );
    }
}
