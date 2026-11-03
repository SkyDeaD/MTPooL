//! Webhooks: the fan-out of events into delivery rows and the sender that posts them.
//!
//! The fan-out is an effect driver: every `Emit` is offered to it, it looks up the hooks the
//! event is for and writes one `webhook_deliveries` row for each, with the payload and the
//! event id fixed at that moment. The sender polls the table. A retry posts the stored payload
//! again, so the receiver sees the same `id` every time.
//!
//! What never leaves this module: the secret of a webhook (it is used to sign and is not
//! logged, stored in an error or put into a row) and what a receiver answered (only the status
//! code is kept).

use std::borrow::Cow;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use data_encoding::HEXLOWER;
use hmac::{Hmac, KeyInit, Mac};
use mtpool_core::{Effect, Event, NodeId, Ts};
use mtpool_protocol::rfc3339;
use serde_json::json;
use sha2::Sha256;
use tokio::task::JoinSet;

use crate::actor::{ActorHandle, Clock, DriverFuture, EffectDriver};
use crate::api::util::random_hex;
use crate::api::webhook_url::{Resolver, is_public_ip};
use crate::store::{AdminOp, Store};

#[cfg(test)]
mod tests;

/// The wait after the n-th failed attempt, in seconds. A sixth failure is the last.
pub const RETRY_DELAYS_SEC: [i64; 5] = [60, 300, 1800, 7200, 21600];

/// Deliveries taken in one round.
pub const BATCH: i64 = 50;
/// How often the sender looks for due deliveries.
pub const SEND_TICK: Duration = Duration::from_secs(5);
/// A request is given this long, connecting included.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// The value of `X-MTPooL-Signature`: `t=<ts>,v1=<hex HMAC-SHA256(secret, "<ts>.<body>")>`.
pub fn sign(secret: &str, ts: i64, body: &str) -> String {
    // HMAC takes a key of any length, so this cannot fail.
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("any key length");
    mac.update(ts.to_string().as_bytes());
    mac.update(b".");
    mac.update(body.as_bytes());
    let digest = mac.finalize().into_bytes();
    format!("t={ts},v1={}", HEXLOWER.encode(&digest))
}

/// Turns events into delivery rows.
#[derive(Clone)]
pub struct WebhookFanOut {
    store: Store,
}

impl WebhookFanOut {
    pub fn new(store: Store) -> WebhookFanOut {
        WebhookFanOut { store }
    }
}

impl EffectDriver for WebhookFanOut {
    fn handles(&self, effect: &Effect) -> bool {
        matches!(effect, Effect::Emit(_))
    }

    fn run(&self, effect: Effect, results: ActorHandle) -> DriverFuture {
        let store = self.store.clone();
        Box::pin(async move {
            let Effect::Emit(event) = effect else { return };
            // Kinds without a webhook name (sensor rounds, DNS) are for nobody.
            let Some(name) = event.kind.webhook_name() else {
                return;
            };
            // The owner of the node decides which owner-scoped hooks get the event. A node the
            // pool does not know, and an event without a node, belong to nobody.
            let owner = match &event.node {
                Some(node) => {
                    let node: NodeId = node.clone();
                    match results
                        .inspect(move |s| s.nodes.get(&node).map(|n| n.owner.0))
                        .await
                    {
                        Ok(owner) => owner,
                        Err(_) => return,
                    }
                }
                None => None,
            };
            let queued =
                tokio::task::spawn_blocking(move || enqueue(&store, name, owner, &event)).await;
            match queued {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(%error, event = name, "webhook fan-out failed"),
                Err(error) => tracing::warn!(%error, event = name, "webhook fan-out panicked"),
            }
        })
    }
}

/// Writes a pending delivery for every hook the event is for. Returns how many.
fn enqueue(
    store: &Store,
    name: &'static str,
    owner: Option<i64>,
    event: &Event,
) -> Result<usize, String> {
    let hooks: Vec<i64> = {
        let conn = store.read();
        let mut statement = conn
            .prepare(
                "SELECT id FROM webhooks WHERE disabled_at IS NULL \
                 AND EXISTS (SELECT 1 FROM json_each(webhooks.events) WHERE value = ?1) \
                 AND (scope = 'pool' OR (scope = 'owner' AND owner_id = ?2)) \
                 ORDER BY id",
            )
            .map_err(|e| e.to_string())?;
        statement
            .query_map(rusqlite::params![name, owner], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?
    };
    if hooks.is_empty() {
        return Ok(0);
    }
    // One event, one id, whatever the number of hooks and the number of tries.
    let event_id = format!("evt_{}", random_hex(8));
    let payload = json!({
        "id": event_id,
        "event": name,
        "at": rfc3339(event.at.0),
        "node": event.node.as_ref().map(NodeId::as_str),
        "domain": event.domain,
        "detail": event.detail,
    })
    .to_string();
    for hook in &hooks {
        store
            .admin(AdminOp::InsertDelivery {
                webhook_id: *hook,
                event_id: event_id.clone(),
                event: name.to_owned(),
                payload: payload.clone(),
                at: event.at,
            })
            .map_err(|e| e.to_string())?;
    }
    Ok(hooks.len())
}

/// A delivery that is due, with what it takes to send it. No `Debug`: it holds a secret.
struct Due {
    id: i64,
    event: String,
    payload: String,
    attempt: i64,
    url: String,
    secret: String,
}

/// What a send came to.
enum Outcome {
    Delivered(i64),
    /// Try again later. The code is a fixed word, the status is the receiver's when it answered.
    Retry(Cow<'static, str>, Option<i64>),
    /// Never going to work (for example a private address): no retry.
    Failed(&'static str),
}

/// Posts the due deliveries.
pub struct WebhookSender {
    store: Store,
    resolver: Arc<dyn Resolver>,
    clock: Clock,
    #[cfg(test)]
    local: Option<SocketAddr>,
}

impl WebhookSender {
    pub fn new(store: Store, resolver: Arc<dyn Resolver>, clock: Clock) -> WebhookSender {
        WebhookSender {
            store,
            resolver,
            clock,
            #[cfg(test)]
            local: None,
        }
    }

    /// Tests only: plain HTTP, and every connection goes to `addr` whatever the name resolved
    /// to. The address check still runs on what the resolver said.
    #[cfg(test)]
    pub(crate) fn local_http(mut self, addr: SocketAddr) -> WebhookSender {
        self.local = Some(addr);
        self
    }

    /// Sends what is due now. Returns how many deliveries were tried.
    pub async fn deliver_due(&self) -> usize {
        let now = (self.clock)();
        let store = self.store.clone();
        let due = match tokio::task::spawn_blocking(move || load_due(&store, now)).await {
            Ok(Ok(due)) => due,
            Ok(Err(error)) => {
                tracing::warn!(%error, "reading due webhook deliveries failed");
                return 0;
            }
            Err(error) => {
                tracing::warn!(%error, "reading due webhook deliveries panicked");
                return 0;
            }
        };
        let tried = due.len();
        let mut round = JoinSet::new();
        for delivery in due {
            let job = Job {
                store: self.store.clone(),
                resolver: Arc::clone(&self.resolver),
                clock: Arc::clone(&self.clock),
                #[cfg(test)]
                local: self.local,
            };
            round.spawn(job.run(delivery));
        }
        while let Some(done) = round.join_next().await {
            if let Err(error) = done {
                tracing::warn!(%error, "a webhook delivery task panicked");
            }
        }
        tried
    }

    /// Runs until the state owner is gone.
    pub async fn run(self, actor: ActorHandle) {
        let mut ticker = tokio::time::interval(SEND_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = actor.stopped() => return,
                _ = ticker.tick() => {}
            }
            self.deliver_due().await;
        }
    }
}

fn load_due(store: &Store, now: Ts) -> Result<Vec<Due>, String> {
    let conn = store.read();
    let mut statement = conn
        .prepare(
            "SELECT d.id, d.event, d.payload, d.attempt, w.url, w.secret \
             FROM webhook_deliveries d JOIN webhooks w ON w.id = d.webhook_id \
             WHERE d.status = 'pending' AND d.next_at <= ?1 AND w.disabled_at IS NULL \
             ORDER BY d.next_at, d.id LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    statement
        .query_map(rusqlite::params![now.0, BATCH], |r| {
            Ok(Due {
                id: r.get(0)?,
                event: r.get(1)?,
                payload: r.get(2)?,
                attempt: r.get(3)?,
                url: r.get(4)?,
                secret: r.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())
}

/// What one delivery task needs.
struct Job {
    store: Store,
    resolver: Arc<dyn Resolver>,
    clock: Clock,
    #[cfg(test)]
    local: Option<SocketAddr>,
}

impl Job {
    async fn run(self, due: Due) {
        let outcome = self.send(&due).await;
        let now = (self.clock)();
        let attempt = due.attempt + 1;
        let op = match outcome {
            Outcome::Delivered(code) => AdminOp::DeliveryResult {
                id: due.id,
                status: "delivered".to_owned(),
                attempt,
                response_code: Some(code),
                last_error: None,
                next_at: None,
            },
            Outcome::Failed(error) => AdminOp::DeliveryResult {
                id: due.id,
                status: "failed".to_owned(),
                attempt,
                response_code: None,
                last_error: Some(error.to_owned()),
                next_at: None,
            },
            Outcome::Retry(error, code) => {
                let delay = usize::try_from(attempt - 1)
                    .ok()
                    .and_then(|n| RETRY_DELAYS_SEC.get(n));
                let (status, next_at) = match delay {
                    Some(seconds) => ("pending", Some(Ts(now.0 + seconds * 1000))),
                    None => ("failed", None),
                };
                AdminOp::DeliveryResult {
                    id: due.id,
                    status: status.to_owned(),
                    attempt,
                    response_code: code,
                    last_error: Some(error.into_owned()),
                    next_at,
                }
            }
        };
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || store.admin(op)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(%error, delivery = due.id, "recording a delivery failed")
            }
            Err(error) => {
                tracing::warn!(%error, delivery = due.id, "recording a delivery panicked")
            }
        }
    }

    async fn send(&self, due: &Due) -> Outcome {
        let Ok(url) = reqwest::Url::parse(&due.url) else {
            return Outcome::Failed("invalid_url");
        };
        #[cfg(test)]
        let local = self.local;
        #[cfg(not(test))]
        let local: Option<SocketAddr> = None;
        // Plain HTTP is only for the tests that talk to a local listener.
        if url.scheme() != "https" && !(cfg!(test) && url.scheme() == "http") {
            return Outcome::Failed("invalid_url");
        }
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return Outcome::Failed("invalid_url");
        };
        // The name is resolved again now: it may have moved to a private address since the
        // webhook was registered. The request is then pinned to what was checked.
        let addresses = match self.resolver.resolve(host, port).await {
            Ok(addresses) if !addresses.is_empty() => addresses,
            _ => return Outcome::Retry(Cow::Borrowed("dns_error"), None),
        };
        if addresses.iter().copied().any(|ip| !is_public_ip(ip)) {
            return Outcome::Failed("private_address");
        }
        let pinned: Vec<SocketAddr> = match local {
            Some(addr) => vec![addr],
            None => addresses
                .iter()
                .map(|ip| SocketAddr::new(*ip, port))
                .collect(),
        };
        let client = match reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(SEND_TIMEOUT)
            .no_proxy()
            .user_agent(concat!("mtpool-registrar/", env!("CARGO_PKG_VERSION")))
            .resolve_to_addrs(host, &pinned)
            .build()
        {
            Ok(client) => client,
            Err(_) => return Outcome::Retry(Cow::Borrowed("client_error"), None),
        };
        let signature = sign(&due.secret, (self.clock)().0 / 1000, &due.payload);
        let request = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-MTPooL-Event", &due.event)
            .header("X-MTPooL-Delivery", due.id.to_string())
            .header("X-MTPooL-Signature", signature)
            .body(due.payload.clone());
        match request.send().await {
            Ok(response) => {
                let code = response.status();
                if code.is_success() {
                    Outcome::Delivered(i64::from(code.as_u16()))
                } else {
                    Outcome::Retry(
                        Cow::Owned(format!("http_{}", code.as_u16())),
                        Some(i64::from(code.as_u16())),
                    )
                }
            }
            // reqwest's error text carries the URL, so only a fixed word is kept.
            Err(error) if error.is_timeout() => Outcome::Retry(Cow::Borrowed("timeout"), None),
            Err(error) if error.is_connect() => {
                Outcome::Retry(Cow::Borrowed("connect_error"), None)
            }
            Err(_) => Outcome::Retry(Cow::Borrowed("request_error"), None),
        }
    }
}
