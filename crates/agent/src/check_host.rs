//! The check-host loop. When the registrar asks (and once after enrolling) the node asks
//! check-host.net to open the node's SSH port from two points in Moscow, and hands the id of the
//! check to the report. The agent does not read the result: the registrar fetches it by the id and
//! judges it itself. The service counts its limit by the address the request comes from, so the
//! request is made from the node over IPv4, and the address never goes to the log.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use mtpool_protocol::agent::CheckHostReport;
use serde_json::Value;
use tokio::time::Instant;

pub const DEFAULT_BASE: &str = "https://check-host.net";
/// The two points in Moscow the check runs from; they are in different autonomous systems.
pub const NODES: [&str; 2] = ["ru1.node.check-host.net", "ru2.node.check-host.net"];
/// No two checks begin closer than this.
pub const GAP: Duration = Duration::from_secs(300);
/// After `limit_exceeded` the service is left alone for this long.
pub const LIMIT_PAUSE: Duration = Duration::from_secs(1200);

pub const RATE_LIMITED: &str = "rate_limited";
pub const UNAVAILABLE: &str = "unavailable";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const TICK_EVERY: Duration = Duration::from_secs(1);

/// The public address of the node as the loop is to know it. The checks of the node fill it, the
/// loop reads it; a clone is the same cell.
#[derive(Debug, Clone, Default)]
pub struct IpSource(Arc<Mutex<Option<Ipv4Addr>>>);

impl IpSource {
    pub fn set(&self, ip: Option<Ipv4Addr>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = ip;
    }

    pub fn get(&self) -> Option<Ipv4Addr> {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[derive(Default)]
struct State {
    /// The registrar takes the result; until it says so the service is not asked at all.
    allowed: bool,
    /// A check is wanted, and not before `not_before` (`None`: at once).
    wanted: bool,
    not_before: Option<Instant>,
    result: Option<CheckHostReport>,
    /// Bumped by `restart`: the result of a check that began before it is dropped.
    epoch: u64,
    started_at: Option<Instant>,
    limited_until: Option<Instant>,
}

struct Inner {
    client: reqwest::Client,
    base: String,
    port: u16,
    ip: IpSource,
    state: Mutex<State>,
}

/// The loop; cheap to clone, every clone is the same loop.
#[derive(Clone)]
pub struct CheckHostLoop {
    inner: Arc<Inner>,
}

impl CheckHostLoop {
    /// `base` is the address of the service, `https://check-host.net` for the real one. `port` is
    /// the port to check, 0 switches the check off.
    pub fn new(base: &str, port: u16, ip: IpSource) -> Result<CheckHostLoop, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .user_agent(concat!("mtpool-agent/", env!("CARGO_PKG_VERSION")))
            .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .build()?;
        Ok(CheckHostLoop {
            inner: Arc::new(Inner {
                client,
                base: base.trim_end_matches('/').to_owned(),
                port,
                ip,
                state: Mutex::new(State::default()),
            }),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The handle the node's address is put in.
    pub fn ip_source(&self) -> IpSource {
        self.inner.ip.clone()
    }

    /// Whether the registrar takes the check at all. Without that the service is not asked.
    pub fn set_allowed(&self, allowed: bool) {
        self.state().allowed = allowed;
    }

    /// The node has enrolled (again): forget the old result and check at once.
    pub fn restart(&self) {
        let mut state = self.state();
        state.epoch += 1;
        state.result = None;
        state.started_at = None;
        state.limited_until = None;
        state.wanted = true;
        state.not_before = None;
    }

    /// The registrar asks for a check: it comes no sooner than [`GAP`] after the last one began,
    /// nor within [`LIMIT_PAUSE`] of a `limit_exceeded`.
    pub fn run_soon(&self, now: Instant) {
        let mut state = self.state();
        let mut at = now;
        if let Some(started) = state.started_at {
            at = at.max(started + GAP);
        }
        if let Some(limited) = state.limited_until {
            at = at.max(limited);
        }
        if !state.wanted {
            state.not_before = Some(at);
        } else if let Some(old) = state.not_before {
            state.not_before = Some(old.min(at));
        }
        state.wanted = true;
    }

    /// What goes in the next report. A started check is handed out once: its id is good for one
    /// report. With the port at 0 it is `Disabled` every time, or the registrar's record of the
    /// node would go stale.
    pub fn take_report(&self) -> Option<CheckHostReport> {
        if self.inner.port == 0 {
            return Some(CheckHostReport::Disabled);
        }
        self.state().result.take()
    }

    /// Whether a started check waits to be reported.
    pub fn has_result(&self) -> bool {
        matches!(self.state().result, Some(CheckHostReport::Started { .. }))
    }

    /// Starts the check that is due at `now`, if any, and keeps its outcome.
    pub async fn tick(&self, now: Instant) {
        if self.inner.port == 0 {
            return;
        }
        let Some(ip) = self.inner.ip.get() else {
            return;
        };
        let epoch = {
            let mut state = self.state();
            let paused = state.limited_until.is_some_and(|until| now < until);
            if !state.allowed
                || paused
                || !state.wanted
                || state.not_before.is_some_and(|at| now < at)
            {
                return;
            }
            state.wanted = false;
            state.started_at = Some(now);
            state.epoch
        };
        let report = match self.start(ip).await {
            Ok(id) => CheckHostReport::Started {
                id,
                port: self.inner.port,
            },
            Err(reason) => {
                if reason == RATE_LIMITED {
                    self.state().limited_until = Some(now + LIMIT_PAUSE);
                }
                tracing::warn!(reason, "no check-host check this time");
                CheckHostReport::NoData {
                    reason: reason.into(),
                }
            }
        };
        self.put(epoch, report);
    }

    /// Keeps `report` unless the loop was restarted since the check began. An id that has not been
    /// reported yet is not overwritten by a failure.
    fn put(&self, epoch: u64, report: CheckHostReport) {
        let mut state = self.state();
        if state.epoch != epoch {
            return;
        }
        let keeps = matches!(state.result, Some(CheckHostReport::Started { .. }))
            && matches!(report, CheckHostReport::NoData { .. });
        if !keeps {
            state.result = Some(report);
        }
    }

    /// The id of a started check, or the reason there is none.
    async fn start(&self, ip: Ipv4Addr) -> Result<String, &'static str> {
        let url = format!(
            "{}/check-tcp?host={ip}:{}&node={}&node={}",
            self.inner.base, self.inner.port, NODES[0], NODES[1]
        );
        let response = self
            .inner
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(error = %error.without_url(), "cannot reach check-host");
                UNAVAILABLE
            })?;
        let status = response.status();
        let text = response.text().await.map_err(|_| UNAVAILABLE)?;
        let body = serde_json::from_str::<Value>(&text).ok();
        // The shape of a limit answer was not recorded; any answer that says `limit_exceeded` in
        // its `error` field counts, whatever the status.
        if body
            .as_ref()
            .and_then(|v| v.get("error")?.as_str())
            .is_some_and(|error| error == "limit_exceeded")
        {
            return Err(RATE_LIMITED);
        }
        if !status.is_success() {
            tracing::warn!(status = status.as_u16(), "check-host refused the check");
            return Err(UNAVAILABLE);
        }
        body.and_then(|v| {
            (v.get("ok")?.as_i64()? == 1)
                .then(|| v.get("request_id")?.as_str().map(str::to_owned))
                .flatten()
        })
        .filter(|id| valid_id(id))
        .ok_or(UNAVAILABLE)
    }

    /// Ticks until the task is dropped.
    pub async fn run(self) {
        loop {
            self.tick(Instant::now()).await;
            tokio::time::sleep(TICK_EVERY).await;
        }
    }
}

/// An id goes to the registrar, which puts it in a URL: only what the service's ids are made of.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}
