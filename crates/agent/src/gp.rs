//! The Globalping loop. From time to time the node asks Globalping to open its proxy port from
//! Russian eyeball networks and hands the id of the measurement to the report. The agent does
//! not read the result: the registrar fetches the measurement by its id and judges it itself.
//! No token is sent, the anonymous limit is enough. That limit is counted by address, and for
//! IPv6 by the whole /64, which neighbouring nodes share, so the API is reached over IPv4 only.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use mtpool_protocol::agent::GpReport;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::local_health::PortCheck;

pub const DEFAULT_BASE: &str = "https://api.globalping.io";
/// How often a running measurement is asked about.
pub const POLL_EVERY: Duration = Duration::from_millis(500);
/// How long a measurement may take before the node gives up on it.
pub const POLL_BUDGET: Duration = Duration::from_secs(60);

pub const NO_DATA: &str = "no_data";
pub const RATE_LIMITED: &str = "rate_limited";
pub const UNAVAILABLE: &str = "unavailable";
pub const TIMEOUT: &str = "timeout";

/// What to measure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpJob {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub sni: String,
    pub interval: Duration,
}

/// A measurement asked for out of turn comes no sooner than this after the last one began: at
/// most 12 an hour, 240 probes of the 250 Globalping gives an address.
pub const OUT_OF_TURN_GAP: Duration = Duration::from_secs(300);

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the loop sleeps between two ticks.
const TICK_EVERY: Duration = Duration::from_secs(1);

/// The body of `POST /v1/measurements` for `job`: an HTTPS `HEAD` of the proxy port, as an
/// ordinary visitor to `sni` would send it, from 20 Russian eyeball networks.
pub fn measurement_body(job: &GpJob) -> Value {
    json!({
        "type": "http",
        "target": job.ip.to_string(),
        "locations": [{ "country": "RU", "tags": ["eyeball-network"], "limit": 20 }],
        "measurementOptions": {
            "port": job.port,
            "protocol": "HTTPS",
            "request": { "method": "HEAD", "host": job.sni, "path": "/" }
        }
    })
}

#[derive(Default)]
struct State {
    job: Option<GpJob>,
    /// `None`: due at once.
    next_at: Option<Instant>,
    result: Option<GpReport>,
    /// Bumped by `restart`: the result of a measurement that began before it is dropped.
    epoch: u64,
    /// When the last measurement began.
    started_at: Option<Instant>,
    /// After a 429 the registrar's requests wait for the plan: the limit is not pressed.
    limited_until: Option<Instant>,
}

struct Inner {
    client: reqwest::Client,
    base: String,
    ports: Arc<dyn PortCheck>,
    state: Mutex<State>,
}

/// The loop; cheap to clone, every clone is the same loop.
#[derive(Clone)]
pub struct GpLoop {
    inner: Arc<Inner>,
    poll_every: Duration,
    budget: Duration,
}

impl GpLoop {
    /// `base` is the address of the Globalping API, `https://api.globalping.io` for the real one.
    pub fn new(base: &str, ports: Arc<dyn PortCheck>) -> Result<GpLoop, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .local_address(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .build()?;
        Ok(GpLoop {
            inner: Arc::new(Inner {
                client,
                base: base.trim_end_matches('/').to_owned(),
                ports,
                state: Mutex::new(State::default()),
            }),
            poll_every: POLL_EVERY,
            budget: POLL_BUDGET,
        })
    }

    /// How often a running measurement is asked about, and how long it may take.
    #[must_use]
    pub fn with_timing(mut self, poll_every: Duration, budget: Duration) -> GpLoop {
        self.poll_every = poll_every;
        self.budget = budget;
        self
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// What to measure from now on; `None` pauses the loop.
    pub fn set_job(&self, job: Option<GpJob>) {
        self.state().job = job;
    }

    /// The node has enrolled (again): forget the old result and measure at once.
    pub fn restart(&self) {
        let mut state = self.state();
        state.epoch += 1;
        state.next_at = None;
        state.result = None;
    }

    /// The registrar asks for a measurement out of turn. It moves the next planned one to `now`,
    /// but no sooner than [`OUT_OF_TURN_GAP`] after the last one began, nor before the plan after
    /// a 429. The plan then counts from it.
    pub fn measure_soon(&self, now: Instant) {
        let mut state = self.state();
        let mut earliest = now;
        if let Some(started) = state.started_at {
            earliest = earliest.max(started + OUT_OF_TURN_GAP);
        }
        if let Some(limited) = state.limited_until {
            earliest = earliest.max(limited);
        }
        if let Some(next) = state.next_at
            && earliest < next
        {
            state.next_at = Some(earliest);
        }
    }

    /// The result that has not been reported yet. A measurement id is good for one report, so it
    /// is handed out once.
    pub fn take_result(&self) -> Option<GpReport> {
        self.state().result.take()
    }

    /// Whether a finished measurement waits to be reported.
    pub fn has_measurement(&self) -> bool {
        matches!(self.state().result, Some(GpReport::Measured { .. }))
    }

    /// Does the measurement that is due at `now`, if any, and keeps its result.
    pub async fn tick(&self, now: Instant) {
        let (job, epoch) = {
            let state = self.state();
            let Some(job) = state.job.clone() else {
                return;
            };
            if state.next_at.is_some_and(|at| now < at) {
                return;
            }
            (job, state.epoch)
        };

        if !self.inner.ports.listening(job.port, Some(job.ip)).await {
            // The proxy does not answer on its own port: Globalping would only say so too. The
            // measurement stays due and is made as soon as the port listens.
            self.put(
                epoch,
                GpReport::NoData {
                    reason: NO_DATA.into(),
                },
                None,
            );
            return;
        }

        self.state().started_at = Some(now);
        let report = match self.measure(&job).await {
            Ok(id) => GpReport::Measured { id },
            Err(reason) => {
                if reason == RATE_LIMITED {
                    self.state().limited_until = Some(now + job.interval);
                }
                tracing::warn!(
                    reason,
                    next_try_sec = job.interval.as_secs(),
                    "no Globalping measurement this time"
                );
                GpReport::NoData {
                    reason: reason.into(),
                }
            }
        };
        self.put(epoch, report, Some(now + job.interval));
    }

    /// Keeps `report` unless the loop was restarted since the measurement began. An id that has
    /// not been reported yet is not overwritten by a failure.
    fn put(&self, epoch: u64, report: GpReport, next_at: Option<Instant>) {
        let mut state = self.state();
        if state.epoch != epoch {
            return;
        }
        if next_at.is_some() {
            state.next_at = next_at;
        }
        let keeps = matches!(state.result, Some(GpReport::Measured { .. }))
            && matches!(report, GpReport::NoData { .. });
        if !keeps {
            state.result = Some(report);
        }
    }

    /// The id of a finished measurement, or the reason there is none.
    async fn measure(&self, job: &GpJob) -> Result<String, &'static str> {
        let url = format!("{}/v1/measurements", self.inner.base);
        let response = self
            .inner
            .client
            .post(&url)
            .json(&measurement_body(job))
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(error = %error.without_url(), "cannot reach Globalping");
                UNAVAILABLE
            })?;
        let status = response.status();
        if status.as_u16() == 429 {
            return Err(RATE_LIMITED);
        }
        if !status.is_success() {
            tracing::warn!(
                status = status.as_u16(),
                "Globalping refused the measurement"
            );
            return Err(UNAVAILABLE);
        }
        let text = response.text().await.map_err(|_| UNAVAILABLE)?;
        let id = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("id")?.as_str().map(str::to_owned))
            .filter(|id| valid_id(id))
            .ok_or(UNAVAILABLE)?;

        // The budget is a hard one: a request that hangs does not stretch it.
        match tokio::time::timeout(self.budget, self.wait_finished(&url, &id)).await {
            Ok(Ok(())) => Ok(id),
            Ok(Err(reason)) => Err(reason),
            Err(_) => Err(TIMEOUT),
        }
    }

    /// Asks about the measurement until it is no longer in progress. The caller bounds the time.
    async fn wait_finished(&self, url: &str, id: &str) -> Result<(), &'static str> {
        let url = format!("{url}/{id}");
        loop {
            match self.inner.client.get(&url).send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        let text = response.text().await.map_err(|_| UNAVAILABLE)?;
                        let state = serde_json::from_str::<Value>(&text)
                            .ok()
                            .and_then(|v| v.get("status")?.as_str().map(str::to_owned))
                            .ok_or(UNAVAILABLE)?;
                        if state != "in-progress" {
                            return Ok(());
                        }
                    } else if status.is_client_error() && status.as_u16() != 429 {
                        // The measurement is gone (404) or never was ours to read.
                        return Err(UNAVAILABLE);
                    }
                    // A 5xx or a 429 is a hiccup: ask again until the budget is spent.
                }
                Err(error) => {
                    tracing::debug!(error = %error.without_url(), "Globalping poll failed");
                }
            }
            tokio::time::sleep(self.poll_every).await;
        }
    }

    /// Ticks until the task is dropped.
    pub async fn run(self) {
        loop {
            self.tick(Instant::now()).await;
            tokio::time::sleep(TICK_EVERY).await;
        }
    }
}

/// An id is put in the path of a URL: only what Globalping's ids are made of.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
