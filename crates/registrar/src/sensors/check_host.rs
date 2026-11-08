//! The check-host sensor.
//!
//! The node's agent starts a TCP check of the node's address from two points in Moscow and
//! reports the id of the check. The registrar downloads the answer itself and checks that it is
//! about this node, then turns it into a vote:
//!
//! * both points connected is `Clear`;
//! * both points timed out is `Blocked`, but only while the registrar can open the same port
//!   itself: a port that is closed for everyone is not a block;
//! * anything else, an answer that cannot be read included, is an abstention.
//!
//! The evidence holds the result code and the connection time of each point. The node's address
//! and the text of the service's errors stay out of it: it is shown publicly.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mtpool_core::{AbstainReason, SensorKind, SensorVote, Ts, Vote};
use reqwest::{Url, redirect};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::time::Instant;

use super::{Sensor, Target};

#[cfg(test)]
mod tests;

pub const DEFAULT_BASE: &str = "https://check-host.net";
/// The two points, in different autonomous systems. The list is not configurable.
pub const POINTS: [&str; 2] = ["ru1.node.check-host.net", "ru2.node.check-host.net"];
/// How long the registrar reads an answer again while a point is still working.
pub const VERIFY_WINDOW: Duration = Duration::from_secs(30);
/// The time the sensor waits for a result of the node; it fits in the round's limit.
pub const SENSOR_BUDGET: Duration = Duration::from_secs(110);
/// How long the registrar's own connection to the checked port may take.
pub const PORT_TIMEOUT: Duration = Duration::from_secs(3);

/// A result is no use to a round once it is older than this.
const MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// How long a used id is remembered.
const ID_MEMORY: Duration = Duration::from_secs(15 * 60);
/// A result may be created this long before the agent's report, or this long after it.
const WINDOW_BEFORE_MS: i64 = 10 * 60 * 1000;
const WINDOW_AFTER_MS: i64 = 60 * 1000;
const POLL_EVERY: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ANSWER_BYTES: usize = 64 * 1024;
const MAX_ID_BYTES: usize = 64;

/// What the answer must be about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckExpect {
    pub ip: Ipv4Addr,
    pub port: u16,
    /// When the agent reported the check.
    pub reported_at: Ts,
}

/// Why an answer is not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckRejected {
    /// A point has not finished yet.
    Pending,
    /// The answer is about another command, address or port.
    Mismatch,
    /// The check was made outside the window around the report.
    Stale,
    /// The answer cannot be read.
    Unknown,
}

/// What one point saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointResult {
    Connected { ms: u64 },
    TimedOut,
    Refused,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// The port that was checked, as the agent reported it.
    pub port: u16,
    /// When the service created the check.
    pub created: Ts,
    /// In the order of `POINTS`.
    pub points: [PointResult; 2],
}

impl CheckResult {
    pub fn vote(&self) -> Vote {
        match self.points {
            [PointResult::Connected { .. }, PointResult::Connected { .. }] => Vote::Clear,
            [PointResult::TimedOut, PointResult::TimedOut] => Vote::Blocked,
            _ => Vote::Abstain(AbstainReason::UnknownAnswer),
        }
    }

    /// The result code and the connection time of each point; nothing else of the answer.
    pub fn evidence(&self) -> Value {
        let mut points = serde_json::Map::new();
        for (name, point) in POINTS.iter().zip(self.points) {
            let label = name.split('.').next().unwrap_or(name);
            let shown = match point {
                PointResult::Connected { ms } => json!({"result": "connected", "ms": ms}),
                PointResult::TimedOut => json!({"result": "timeout"}),
                PointResult::Refused => json!({"result": "refused"}),
                PointResult::Other => json!({"result": "other"}),
            };
            points.insert(label.to_owned(), shown);
        }
        json!({"points": points})
    }
}

/// What a point answered: `None` while it is still working (`null`).
fn parse_point(value: &Value) -> Result<Option<PointResult>, CheckRejected> {
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => {
            let first = items.first().and_then(Value::as_object);
            let point = match first {
                Some(item) => {
                    if let Some(error) = item.get("error").and_then(Value::as_str) {
                        match error {
                            "Connection timed out" => PointResult::TimedOut,
                            "Connection refused" => PointResult::Refused,
                            _ => PointResult::Other,
                        }
                    } else if let (Some(_), Some(time)) = (
                        item.get("address"),
                        item.get("time").and_then(Value::as_f64),
                    ) {
                        PointResult::Connected {
                            ms: seconds_to_ms(time),
                        }
                    } else {
                        PointResult::Other
                    }
                }
                None => PointResult::Other,
            };
            Ok(Some(point))
        }
        _ => Err(CheckRejected::Unknown),
    }
}

fn seconds_to_ms(seconds: f64) -> u64 {
    let ms = (seconds * 1000.0).round();
    if ms.is_finite() && ms >= 0.0 {
        // Saturates for a value that does not fit; the time is only shown.
        ms as u64
    } else {
        0
    }
}

/// Checks an answer of `check-result-extended` against what the agent reported and reads the
/// two points. Whatever does not have the expected shape is `Unknown`; it never panics.
pub fn parse_result(body: &Value, expect: &CheckExpect) -> Result<CheckResult, CheckRejected> {
    let object = body.as_object().ok_or(CheckRejected::Unknown)?;
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .ok_or(CheckRejected::Unknown)
    };
    let command = text("command")?;
    let host = text("host")?;
    let port: u16 = text("port")?.parse().map_err(|_| CheckRejected::Unknown)?;
    let created = object
        .get("created")
        .and_then(Value::as_i64)
        .ok_or(CheckRejected::Unknown)?;
    let results = object
        .get("results")
        .and_then(Value::as_object)
        .ok_or(CheckRejected::Unknown)?;

    if command != "tcp" || host != expect.ip.to_string() || port != expect.port {
        return Err(CheckRejected::Mismatch);
    }
    let created_ms = created.saturating_mul(1000);
    let reported = expect.reported_at.0;
    if created_ms < reported.saturating_sub(WINDOW_BEFORE_MS)
        || created_ms > reported.saturating_add(WINDOW_AFTER_MS)
    {
        return Err(CheckRejected::Stale);
    }

    let mut points = [PointResult::Other; 2];
    let mut pending = false;
    for (slot, name) in points.iter_mut().zip(POINTS) {
        let value = results.get(name).ok_or(CheckRejected::Unknown)?;
        match parse_point(value)? {
            Some(point) => *slot = point,
            None => pending = true,
        }
    }
    if pending {
        return Err(CheckRejected::Pending);
    }
    Ok(CheckResult {
        port,
        created: Ts(created_ms),
        points,
    })
}

#[derive(Clone)]
pub struct CheckHostVerifier {
    http: reqwest::Client,
    base: Url,
}

impl CheckHostVerifier {
    pub fn new(base: Url) -> Result<CheckHostVerifier, reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(redirect::Policy::none())
            .user_agent(concat!("mtpool-registrar/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(CheckHostVerifier { http, base })
    }

    pub fn default_base() -> Url {
        Url::parse(DEFAULT_BASE).expect("the default base is a URL")
    }

    /// One read of the answer for `id`: the JSON, or nothing when it could not be had.
    async fn fetch(&self, id: &str) -> Option<Value> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .extend(["check-result-extended", id]);
        let mut response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .ok()?;
        if response.status() != reqwest::StatusCode::OK {
            return None;
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if body.len() + chunk.len() > MAX_ANSWER_BYTES {
                return None;
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).ok()
    }

    /// Downloads the answer for `id` and checks it. While a point is still working the answer
    /// is read again, for up to `VERIFY_WINDOW`.
    pub async fn verify(
        &self,
        id: &str,
        expect: &CheckExpect,
    ) -> Result<CheckResult, CheckRejected> {
        let plain = !id.is_empty()
            && id.len() <= MAX_ID_BYTES
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !plain {
            return Err(CheckRejected::Unknown);
        }
        read_until_done(|| self.fetch(id), expect, VERIFY_WINDOW).await
    }
}

/// Reads the answer again while a point works: the loop of `verify` with the fetching left out.
async fn read_until_done<F, Fut>(
    fetch: F,
    expect: &CheckExpect,
    window: Duration,
) -> Result<CheckResult, CheckRejected>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<Value>>,
{
    let mut fetch = fetch;
    let deadline = Instant::now() + window;
    let mut last = CheckRejected::Unknown;
    loop {
        match tokio::time::timeout_at(deadline, fetch()).await {
            Err(_) => return Err(last),
            Ok(None) => last = CheckRejected::Unknown,
            Ok(Some(body)) => match parse_result(&body, expect) {
                Ok(result) => return Ok(result),
                Err(CheckRejected::Pending) => last = CheckRejected::Pending,
                Err(other) => return Err(other),
            },
        }
        if Instant::now() + POLL_EVERY > deadline {
            return Err(last);
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Result(CheckResult),
    /// The agent has no result: the text is the reason it gave.
    NoData(String),
    /// The check is switched off on the node.
    Disabled,
}

type Clock = Arc<dyn Fn() -> Ts + Send + Sync>;

struct Slot {
    entry: Entry,
    at: Ts,
}

#[derive(Default)]
struct Inner {
    slots: HashMap<Ipv4Addr, Slot>,
    ids: HashMap<String, Ts>,
    /// Addresses whose answer is being fetched.
    flying: HashSet<Ipv4Addr>,
}

/// What the agents reported, waiting for the round of their node.
pub struct CheckHostInbox {
    inner: Mutex<Inner>,
    notify: Notify,
    clock: Clock,
}

/// The place of a fetch for one address.
pub struct Flight {
    inbox: Arc<CheckHostInbox>,
    ip: Ipv4Addr,
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.inbox.lock().flying.remove(&self.ip);
    }
}

impl CheckHostInbox {
    pub fn new() -> CheckHostInbox {
        CheckHostInbox::with_clock(Arc::new(system_now))
    }

    /// The clock tells how old an entry is; tests set their own.
    pub fn with_clock(clock: Clock) -> CheckHostInbox {
        CheckHostInbox {
            inner: Mutex::new(Inner::default()),
            notify: Notify::new(),
            clock,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes the only place for a fetch for this address; false while another is under way. The
    /// place is given back when the returned guard is dropped, however the fetch ends.
    pub fn begin(self: &Arc<Self>, ip: Ipv4Addr) -> Option<Flight> {
        self.lock().flying.insert(ip).then(|| Flight {
            inbox: Arc::clone(self),
            ip,
        })
    }

    /// True for the first claim of an id. An id is remembered for 15 minutes.
    pub fn claim(&self, id: &str, now: Ts) -> bool {
        let mut inner = self.lock();
        let memory = millis(ID_MEMORY);
        inner
            .ids
            .retain(|_, at| now.0.saturating_sub(at.0) <= memory);
        if inner.ids.contains_key(id) {
            return false;
        }
        inner.ids.insert(id.to_owned(), now);
        true
    }

    /// A result or a no-data note replaces what the node had; `Disabled` replaces everything. A
    /// no-data note does not push out a result nobody has taken yet.
    pub fn put(&self, ip: Ipv4Addr, entry: Entry) {
        let now = (self.clock)();
        {
            let mut inner = self.lock();
            if matches!(entry, Entry::NoData(_))
                && inner.slots.get(&ip).is_some_and(|slot| {
                    matches!(slot.entry, Entry::Result(_)) && slot.is_fresh(now)
                })
            {
                return;
            }
            inner.slots.insert(ip, Slot { entry, at: now });
        }
        self.notify.notify_waiters();
    }

    fn try_take(&self, ip: Ipv4Addr) -> Option<Entry> {
        let now = (self.clock)();
        let mut inner = self.lock();
        let slot = inner.slots.get(&ip)?;
        if matches!(slot.entry, Entry::Disabled) {
            return Some(Entry::Disabled);
        }
        let fresh = slot.is_fresh(now);
        let slot = inner.slots.remove(&ip)?;
        fresh.then_some(slot.entry)
    }

    /// The entry for the node. A result and a no-data note are handed out once; `Disabled`
    /// stays. Waits up to `budget` for one to arrive.
    pub async fn take(&self, ip: Ipv4Addr, budget: Duration) -> Option<Entry> {
        let deadline = Instant::now() + budget;
        loop {
            // Registered before the look, so an entry put in between is not missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(entry) = self.try_take(ip) {
                return Some(entry);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.try_take(ip);
            }
        }
    }
}

impl Slot {
    /// A result counts from its creation by the service; a note from its arrival.
    fn is_fresh(&self, now: Ts) -> bool {
        let since = match &self.entry {
            Entry::Result(result) => result.created,
            Entry::NoData(_) | Entry::Disabled => self.at,
        };
        now.0.saturating_sub(since.0) <= millis(MAX_AGE)
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

fn system_now() -> Ts {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0);
    Ts(ms)
}

impl Default for CheckHostInbox {
    fn default() -> CheckHostInbox {
        CheckHostInbox::new()
    }
}

/// Opens a connection to a port, to tell whether it is open from here.
#[async_trait]
pub trait PortConnector: Send + Sync {
    async fn open(&self, ip: Ipv4Addr, port: u16, timeout: Duration) -> bool;
}

/// The connector that really opens a TCP connection.
pub struct TcpConnector;

#[async_trait]
impl PortConnector for TcpConnector {
    async fn open(&self, ip: Ipv4Addr, port: u16, timeout: Duration) -> bool {
        matches!(
            tokio::time::timeout(timeout, tokio::net::TcpStream::connect((ip, port))).await,
            Ok(Ok(_))
        )
    }
}

pub struct CheckHostSensor {
    inbox: Arc<CheckHostInbox>,
    connector: Arc<dyn PortConnector>,
}

impl CheckHostSensor {
    pub fn new(inbox: Arc<CheckHostInbox>, connector: Arc<dyn PortConnector>) -> CheckHostSensor {
        CheckHostSensor { inbox, connector }
    }
}

#[async_trait]
impl Sensor for CheckHostSensor {
    fn kind(&self) -> SensorKind {
        SensorKind::CheckHost
    }

    async fn vote(&self, target: &Target) -> SensorVote {
        let abstain = |reason: AbstainReason, why: &str| SensorVote {
            vote: Vote::Abstain(reason),
            evidence: json!({"reason": why}),
        };
        match self.inbox.take(target.ip, SENSOR_BUDGET).await {
            None => abstain(AbstainReason::NoData, "no_data"),
            Some(Entry::Disabled) => abstain(AbstainReason::NotConfigured, "not_configured"),
            Some(Entry::NoData(why)) => match why.as_str() {
                "rate_limited" => abstain(AbstainReason::RateLimited, "rate_limited"),
                "unavailable" => abstain(AbstainReason::Unavailable, "unavailable"),
                _ => abstain(AbstainReason::NoData, "no_data"),
            },
            Some(Entry::Result(result)) => {
                let vote = result.vote();
                let decides = matches!(vote, Vote::Blocked | Vote::Clear);
                if decides && !self.port_open(target.ip, result.port).await {
                    let mut evidence = result.evidence();
                    evidence["reason"] = json!("port_closed");
                    return SensorVote {
                        vote: Vote::Abstain(AbstainReason::PortClosed),
                        evidence,
                    };
                }
                SensorVote {
                    vote,
                    evidence: result.evidence(),
                }
            }
        }
    }
}

impl CheckHostSensor {
    /// Whether the checked port opens from here. A connector that does not answer in time says
    /// no.
    async fn port_open(&self, ip: Ipv4Addr, port: u16) -> bool {
        let limit = PORT_TIMEOUT + Duration::from_secs(1);
        tokio::time::timeout(limit, self.connector.open(ip, port, PORT_TIMEOUT))
            .await
            .unwrap_or(false)
    }
}
