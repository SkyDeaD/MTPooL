//! The live event stream of the panel: server-sent events.
//!
//! [`EventFan`] is an effect driver. The actor offers it every `Emit`; it renders the event
//! once and hands the frame to every connected client through a broadcast channel. A client
//! that cannot keep up (more than [`CAPACITY`] frames behind) is dropped: it reconnects and
//! reads the event log for what it missed.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use futures_core::Stream;
use mtpool_core::{Effect, Event};
use mtpool_protocol::rfc3339;
use serde_json::json;
use tokio::sync::{broadcast, mpsc, watch};

use crate::actor::{ActorHandle, DriverFuture, EffectDriver};

/// How often an idle stream gets a comment line, so proxies keep the connection open.
pub const KEEPALIVE: Duration = Duration::from_secs(15);
/// Frames a client may be behind before it is dropped.
pub const CAPACITY: usize = 256;
/// Frames waiting for a slow connection to take them.
const CLIENT_BUFFER: usize = 16;
/// What the browser waits before it reconnects, in milliseconds.
const RETRY_MS: u64 = 5000;
/// How long a connection may take to accept a frame before it is dropped.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

struct Inner {
    tx: broadcast::Sender<Bytes>,
    /// The `id:` of the next frame. Held while a frame is sent so ids and order agree.
    next_id: Mutex<u64>,
    keepalive: Duration,
    closed: watch::Sender<bool>,
}

/// The fan-out of events to the connected panels. Clones share it.
#[derive(Clone)]
pub struct EventFan {
    inner: Arc<Inner>,
}

impl Default for EventFan {
    fn default() -> EventFan {
        EventFan::new()
    }
}

impl EventFan {
    pub fn new() -> EventFan {
        EventFan::with_keepalive(KEEPALIVE)
    }

    pub fn with_keepalive(keepalive: Duration) -> EventFan {
        let (tx, _) = broadcast::channel(CAPACITY);
        let (closed, _) = watch::channel(false);
        EventFan {
            inner: Arc::new(Inner {
                tx,
                next_id: Mutex::new(1),
                keepalive,
                closed,
            }),
        }
    }

    /// Sends the event to every connected client. Never waits.
    pub fn publish(&self, event: &Event) {
        let kind = serde_json::to_value(event.kind)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());
        let data = json!({
            "at": rfc3339(event.at.0),
            "kind": kind,
            "node": event.node.as_ref().map(|n| n.as_str()),
            "domain": event.domain,
            "detail": event.detail,
        });
        let mut next = self.inner.next_id.lock().unwrap_or_else(|e| e.into_inner());
        let frame = format!("id: {}\nevent: {kind}\ndata: {data}\n\n", *next);
        *next += 1;
        // No receiver is not an error: nobody is watching.
        let _ = self.inner.tx.send(Bytes::from(frame));
    }

    /// Ends every open stream and refuses new ones. Called when the service stops, so that
    /// open streams do not hold up the drain.
    pub fn close(&self) {
        self.inner.closed.send_replace(true);
    }

    /// The response of `GET /admin/stream`: `text/event-stream` that lives until the client
    /// leaves, the fan is closed or the actor stops.
    pub fn subscribe(&self, actor: ActorHandle) -> Response {
        event_stream(
            self.inner.tx.subscribe(),
            self.inner.closed.subscribe(),
            StreamTerms {
                keepalive: self.inner.keepalive,
                retry_ms: RETRY_MS,
                max_age: None,
            },
            actor,
            None,
            (),
        )
    }
}

/// What differs between one kind of stream and another.
pub(super) struct StreamTerms {
    pub keepalive: Duration,
    /// What the browser waits before it reconnects, in milliseconds.
    pub retry_ms: u64,
    /// When the stream ends by itself, however well the client reads it.
    pub max_age: Option<Duration>,
}

/// A `text/event-stream` response fed from a broadcast channel. `first` goes out before anything
/// else; `guard` is held for as long as the stream lives.
pub(super) fn event_stream<G: Send + 'static>(
    mut frames: broadcast::Receiver<Bytes>,
    mut closed: watch::Receiver<bool>,
    terms: StreamTerms,
    actor: ActorHandle,
    first: Option<Bytes>,
    guard: G,
) -> Response {
    let (out, chunks) = mpsc::channel::<Bytes>(CLIENT_BUFFER);
    tokio::spawn(async move {
        let _guard = guard;
        let hello = Bytes::from(format!("retry: {}\n\n", terms.retry_ms));
        if *closed.borrow() || out.send(hello).await.is_err() {
            return;
        }
        if let Some(first) = first
            && out.send(first).await.is_err()
        {
            return;
        }
        let mut tick = tokio::time::interval(terms.keepalive);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate.
        tick.tick().await;
        let aged = async {
            match terms.max_age {
                Some(age) => tokio::time::sleep(age).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(aged);
        loop {
            let chunk = tokio::select! {
                frame = frames.recv() => match frame {
                    Ok(frame) => frame,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "an event stream client fell behind and was dropped");
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = tick.tick() => Bytes::from_static(b": keepalive\n\n"),
                _ = closed.changed() => break,
                () = actor.stopped() => break,
                () = out.closed() => break,
                () = &mut aged => break,
            };
            // A connection that stopped reading must not hold this task or the drain.
            let sent = tokio::select! {
                sent = tokio::time::timeout(SEND_TIMEOUT, out.send(chunk)) => {
                    matches!(sent, Ok(Ok(())))
                }
                _ = closed.changed() => false,
                () = actor.stopped() => false,
            };
            if !sent {
                break;
            }
        }
    });
    let mut response = Response::new(Body::from_stream(Chunks(chunks)));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    // `no-transform` keeps a compressing proxy away: a frame must not wait in its compressor.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    // Tells nginx-style proxies not to hold the events back.
    headers.insert(
        HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

impl EffectDriver for EventFan {
    fn handles(&self, effect: &Effect) -> bool {
        matches!(effect, Effect::Emit(_))
    }

    fn run(&self, effect: Effect, _results: ActorHandle) -> DriverFuture {
        // Published here and not in the future: the future runs on its own task, and events
        // must reach the clients in the order the actor made them.
        if let Effect::Emit(event) = &effect {
            self.publish(event);
        }
        Box::pin(async {})
    }
}

/// The body of a stream: what the client's task sends.
struct Chunks(mpsc::Receiver<Bytes>);

impl Stream for Chunks {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx).map(|chunk| chunk.map(Ok))
    }
}

#[cfg(test)]
mod tests {
    use mtpool_core::{EventKind, Ts};
    use serde_json::Value;

    use super::*;

    fn event(kind: EventKind) -> Event {
        Event {
            kind,
            at: Ts(0),
            node: None,
            owner: None,
            domain: Some("proxy.example.com".to_owned()),
            detail: json!({ "n": 1 }),
        }
    }

    #[test]
    fn frames_carry_a_rising_id_the_kind_and_the_document() {
        let fan = EventFan::new();
        let mut rx = fan.inner.tx.subscribe();
        fan.publish(&event(EventKind::NodeDead));
        fan.publish(&event(EventKind::BanLifted));
        let first = String::from_utf8(rx.try_recv().unwrap().to_vec()).unwrap();
        let second = String::from_utf8(rx.try_recv().unwrap().to_vec()).unwrap();
        assert!(
            first.starts_with("id: 1\nevent: node_dead\ndata: "),
            "{first}"
        );
        assert!(
            second.starts_with("id: 2\nevent: ban_lifted\ndata: "),
            "{second}"
        );
        assert!(first.ends_with("\n\n"));
        let data = first
            .lines()
            .nth(2)
            .unwrap()
            .strip_prefix("data: ")
            .unwrap();
        let doc: Value = serde_json::from_str(data).unwrap();
        assert_eq!(doc["kind"], "node_dead");
        assert_eq!(doc["at"], "1970-01-01T00:00:00Z");
        assert_eq!(doc["domain"], "proxy.example.com");
        assert_eq!(doc["detail"]["n"], 1);
        assert!(doc["node"].is_null());
    }

    #[test]
    fn publishing_without_a_listener_is_fine() {
        EventFan::new().publish(&event(EventKind::NodeDead));
    }
}
