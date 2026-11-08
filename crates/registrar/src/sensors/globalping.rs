//! The Globalping sensor.
//!
//! Globalping runs probes in real networks. A probe makes an HTTPS HEAD request to the node's
//! address with the node's SNI as the `Host`; what counts is that the TLS handshake finished
//! (spec section 5), not what the node answered.
//!
//! Two users share the client and the way a measurement is turned into a vote:
//!
//! - `GlobalpingVerifier` checks a measurement an agent made and named in its report. The agent
//!   says which measurement it made, never what it found.
//! - `GlobalpingSensor` makes a measurement itself when no fresh agent report exists, after
//!   looking at the account allowance.
//!
//! A vote is never `Blocked` because something went wrong on the way: an unreachable service, a
//! refusal, a rate limit or an answer that cannot be read all end as an abstention.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use mtpool_core::{AbstainReason, GlobalpingSettings, SensorKind, SensorVote, Vote};
use mtpool_protocol::parse_rfc3339;
use reqwest::{Method, RequestBuilder, Url, redirect};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Sensor, Target};
use crate::verifier::{GpExpect, GpRejected, MeasurementVerifier, VerifyFuture};

#[cfg(test)]
mod tests;

/// Where the token comes from, asked on every request so that a token changed in the panel
/// applies at once. `None` means there is none.
pub type TokenSource = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// The Globalping part of the settings, swapped in whenever the settings change.
pub type SharedSettings = Arc<ArcSwap<GlobalpingSettings>>;

pub const DEFAULT_BASE: &str = "https://api.globalping.io/";

/// Between two polls of one measurement: two requests a second at most.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// An answer larger than this is refused unread.
pub const MAX_ANSWER_BYTES: usize = 2 * 1024 * 1024;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the verifier waits for a measurement to finish. Under the 30 seconds the report
/// route gives the verifier, so that the abstention carries its reason.
pub const VERIFY_BUDGET: Duration = Duration::from_secs(25);
/// How long the sensor waits for the measurement it created.
pub const CENTRAL_BUDGET: Duration = Duration::from_secs(60);

/// Probes kept in the evidence of a vote.
const EVIDENCE_PROBES: usize = 50;
/// The one country a probe may be in, and the kind of network.
const COUNTRY: &str = "RU";
const NETWORK_TAG: &str = "eyeball-network";
/// `measurementOptions.port` when the service leaves it out: the default of HTTPS.
const DEFAULT_HTTPS_PORT: u16 = 443;

/// Why a call to Globalping gave nothing usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    #[error("not found")]
    NotFound,
    #[error("rate limited")]
    RateLimited,
    #[error("refused with status {0}")]
    Rejected(u16),
    #[error("the service is not reachable or failed")]
    Unavailable(Option<u16>),
    #[error("the answer cannot be read")]
    Malformed,
    #[error("the answer is too large")]
    TooLarge,
    #[error("no answer in time")]
    TimedOut,
}

impl ClientError {
    pub fn http_status(self) -> Option<u16> {
        match self {
            ClientError::NotFound => Some(404),
            ClientError::RateLimited => Some(429),
            ClientError::Rejected(status) => Some(status),
            ClientError::Unavailable(status) => status,
            ClientError::Malformed | ClientError::TooLarge | ClientError::TimedOut => None,
        }
    }

    /// The abstention this failure is for a sensor.
    fn abstain_reason(self) -> AbstainReason {
        match self {
            ClientError::RateLimited => AbstainReason::RateLimited,
            ClientError::Unavailable(_) => AbstainReason::Unavailable,
            ClientError::TimedOut => AbstainReason::Timeout,
            ClientError::NotFound
            | ClientError::Rejected(_)
            | ClientError::Malformed
            | ClientError::TooLarge => AbstainReason::UnknownAnswer,
        }
    }
}

/// The answer to creating a measurement.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    pub id: String,
    pub probes_count: u32,
}

/// What is left of the allowance for creating measurements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub create_remaining: u64,
}

#[derive(Deserialize)]
struct LimitsDocument {
    #[serde(rename = "rateLimit")]
    rate_limit: RateLimitDocument,
}

#[derive(Deserialize)]
struct RateLimitDocument {
    measurements: MeasurementLimits,
}

#[derive(Deserialize)]
struct MeasurementLimits {
    create: CreateLimit,
}

#[derive(Deserialize)]
struct CreateLimit {
    remaining: u64,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct RequestOptions {
    pub host: Option<String>,
}

/// The options a finished measurement echoes: what was sent, without the defaults (an absent
/// `protocol` is HTTPS).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct MeasurementOptions {
    pub port: Option<u16>,
    pub protocol: Option<String>,
    pub request: Option<RequestOptions>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct Probe {
    pub country: String,
    pub city: Option<String>,
    pub network: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ProbeOutcome {
    pub status: String,
    /// Present (an object) when the handshake finished; `null` or absent otherwise.
    pub tls: Option<Value>,
    /// What the probe saw, as text: `connect ECONNREFUSED <ip>:<port>` for a refused connection.
    #[serde(rename = "rawOutput")]
    pub raw_output: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct ProbeResult {
    pub probe: Probe,
    pub result: ProbeOutcome,
}

impl ProbeResult {
    /// The probe ran to an end, well or badly. A probe still running or offline has no result.
    fn has_result(&self) -> bool {
        matches!(self.result.status.as_str(), "finished" | "failed")
    }

    /// The connection was refused: a closed port answers so, a block by address need not.
    fn is_refused(&self) -> bool {
        self.result.status == "failed"
            && self
                .result
                .raw_output
                .as_deref()
                .is_some_and(|text| text.contains("ECONNREFUSED"))
    }

    /// Spec section 5: finished, and the TLS handshake is there. The HTTP code and the validity
    /// of the certificate do not count.
    fn is_success(&self) -> bool {
        self.result.status == "finished" && self.result.tls.as_ref().is_some_and(Value::is_object)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Measurement {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: String,
    pub created_at: String,
    pub target: String,
    pub probes_count: u32,
    #[serde(default)]
    pub measurement_options: MeasurementOptions,
    #[serde(default)]
    pub results: Vec<ProbeResult>,
}

/// Measurement ids are what an agent sends: only what can sit in a path segment unchanged.
fn is_measurement_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The Globalping HTTP API. Cheap to clone.
#[derive(Clone)]
pub struct GlobalpingClient {
    http: reqwest::Client,
    base: Url,
    token: TokenSource,
    poll_interval: Duration,
}

impl GlobalpingClient {
    /// `base` is the API root (`DEFAULT_BASE`); tests point it at a local server.
    pub fn new(mut base: Url, token: TokenSource) -> Result<GlobalpingClient, reqwest::Error> {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // The token goes to the one host it was made for.
            .redirect(redirect::Policy::none())
            .user_agent(concat!("mtpool-registrar/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(GlobalpingClient {
            http,
            base,
            token,
            poll_interval: DEFAULT_POLL_INTERVAL,
        })
    }

    pub fn default_base() -> Url {
        Url::parse(DEFAULT_BASE).expect("the default base is a URL")
    }

    /// Whether a token is available right now.
    pub fn has_token(&self) -> bool {
        (self.token)().is_some()
    }

    #[cfg(test)]
    fn with_poll_interval(mut self, interval: Duration) -> GlobalpingClient {
        self.poll_interval = interval;
        self
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let url = self.base.join(path).expect("a fixed path joins the base");
        let builder = self.http.request(method, url);
        match (self.token)() {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    /// Sends the request and returns the body of a 2xx answer, read up to the cap.
    async fn exchange(&self, request: RequestBuilder) -> Result<Vec<u8>, ClientError> {
        let mut response = request.send().await.map_err(|e| {
            let timed_out = e.is_timeout();
            tracing::debug!(error = %e.without_url(), "globalping request failed");
            if timed_out {
                ClientError::TimedOut
            } else {
                ClientError::Unavailable(None)
            }
        })?;
        let status = response.status();
        match status.as_u16() {
            200..=299 => {}
            404 => return Err(ClientError::NotFound),
            429 => return Err(ClientError::RateLimited),
            500..=599 => return Err(ClientError::Unavailable(Some(status.as_u16()))),
            other => return Err(ClientError::Rejected(other)),
        }
        let too_large =
            |length: u64| usize::try_from(length).map_or(true, |n| n > MAX_ANSWER_BYTES);
        if response.content_length().is_some_and(too_large) {
            return Err(ClientError::TooLarge);
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > MAX_ANSWER_BYTES {
                        return Err(ClientError::TooLarge);
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(body),
                Err(e) if e.is_timeout() => return Err(ClientError::TimedOut),
                Err(_) => return Err(ClientError::Unavailable(Some(status.as_u16()))),
            }
        }
    }

    /// The allowance for creating measurements. Free of charge.
    pub async fn limits(&self) -> Result<Limits, ClientError> {
        let body = self
            .exchange(self.request(Method::GET, "v1/limits"))
            .await?;
        let document: LimitsDocument =
            serde_json::from_slice(&body).map_err(|_| ClientError::Malformed)?;
        Ok(Limits {
            create_remaining: document.rate_limit.measurements.create.remaining,
        })
    }

    /// Asks for `limit` probes in Russian eyeball networks to make an HTTPS HEAD request to the
    /// node with its SNI as the host.
    pub async fn create(&self, target: &Target, limit: u32) -> Result<Created, ClientError> {
        let body = json!({
            "type": "http",
            "target": target.ip.to_string(),
            "locations": [{"country": COUNTRY, "tags": [NETWORK_TAG], "limit": limit}],
            "measurementOptions": {
                "port": target.port,
                "protocol": "HTTPS",
                "request": {"method": "HEAD", "host": target.sni, "path": "/"}
            }
        });
        let request = self
            .request(Method::POST, "v1/measurements")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        let answer = self.exchange(request).await?;
        let created: Created =
            serde_json::from_slice(&answer).map_err(|_| ClientError::Malformed)?;
        if is_measurement_id(&created.id) {
            Ok(created)
        } else {
            Err(ClientError::Malformed)
        }
    }

    /// One measurement as it is now. An id that is not an id is `NotFound` without a request.
    pub async fn measurement(&self, id: &str) -> Result<Measurement, ClientError> {
        if !is_measurement_id(id) {
            return Err(ClientError::NotFound);
        }
        let body = self
            .exchange(self.request(Method::GET, &format!("v1/measurements/{id}")))
            .await?;
        serde_json::from_slice(&body).map_err(|_| ClientError::Malformed)
    }

    /// Polls the measurement until it is no longer in progress: one request per poll interval,
    /// for at most `budget`.
    pub async fn wait_finished(
        &self,
        id: &str,
        budget: Duration,
    ) -> Result<Measurement, ClientError> {
        let poll = async {
            loop {
                let measurement = self.measurement(id).await?;
                match measurement.status.as_str() {
                    "finished" => return Ok(measurement),
                    "in-progress" => tokio::time::sleep(self.poll_interval).await,
                    _ => return Err(ClientError::Malformed),
                }
            }
        };
        tokio::time::timeout(budget, poll)
            .await
            .unwrap_or(Err(ClientError::TimedOut))
    }
}

/// Whether the measurement is the one the registrar expects: made against this node, from the
/// right country, at about the time the agent says.
fn check_measurement(
    measurement: &Measurement,
    expect: &GpExpect,
    settings: &GlobalpingSettings,
) -> Result<(), GpRejected> {
    let options = &measurement.measurement_options;
    let host = options.request.as_ref().and_then(|r| r.host.as_deref());
    let aimed_here = measurement.kind == "http"
        && measurement.target.parse::<Ipv4Addr>() == Ok(expect.ip)
        && options.port.unwrap_or(DEFAULT_HTTPS_PORT) == expect.port
        && options
            .protocol
            .as_deref()
            .is_none_or(|p| p.eq_ignore_ascii_case("HTTPS"))
        && host.is_some_and(|h| h.eq_ignore_ascii_case(&expect.sni))
        && measurement.probes_count >= 1;
    // A node could pick the probes that favour it: every probe has to be in Russia, in an
    // eyeball network.
    let probes_are_right = measurement.results.iter().all(|r| {
        r.probe.country.eq_ignore_ascii_case(COUNTRY)
            && r.probe.tags.iter().any(|t| t == NETWORK_TAG)
    });
    if !aimed_here || !probes_are_right {
        return Err(GpRejected::Mismatch);
    }
    let Some(created_at) = parse_rfc3339(&measurement.created_at) else {
        return Err(GpRejected::Unavailable);
    };
    let past = i64::from(settings.window_past_sec) * 1000;
    let future = i64::from(settings.window_future_sec) * 1000;
    let reported = expect.reported_at.0;
    if created_at < reported.saturating_sub(past) || created_at > reported.saturating_add(future) {
        return Err(GpRejected::Stale);
    }
    Ok(())
}

fn trimmed(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// Why a probe did not succeed, as a short code: the probe's own text can hold the node's
/// address, so only the code is kept as evidence.
fn failure_reason(probe: &ProbeResult) -> Option<&'static str> {
    if probe.is_success() {
        return None;
    }
    let text = probe.result.raw_output.as_deref().unwrap_or_default();
    Some(if text.contains("ECONNREFUSED") {
        "refused"
    } else if text.contains("timed out") || text.contains("ETIMEDOUT") {
        "timeout"
    } else if ["TLS", "certificate", "handshake"]
        .iter()
        .any(|word| text.contains(word))
    {
        "tls"
    } else {
        "other"
    })
}

fn probe_evidence(probe: &ProbeResult) -> Value {
    let mut entry = Map::new();
    entry.insert("country".to_owned(), json!(trimmed(&probe.probe.country)));
    if let Some(city) = &probe.probe.city {
        entry.insert("city".to_owned(), json!(trimmed(city)));
    }
    if let Some(network) = &probe.probe.network {
        entry.insert("network".to_owned(), json!(trimmed(network)));
    }
    entry.insert("status".to_owned(), json!(trimmed(&probe.result.status)));
    entry.insert(
        "tls".to_owned(),
        json!(probe.result.tls.as_ref().is_some_and(Value::is_object)),
    );
    if let Some(reason) = failure_reason(probe) {
        entry.insert("reason".to_owned(), json!(reason));
    }
    Value::Object(entry)
}

/// The vote of a finished measurement: too few probes with a result is no data; a share of
/// successful probes below the threshold is `Blocked`.
fn tally(id: &str, measurement: &Measurement, settings: &GlobalpingSettings) -> SensorVote {
    let with_result = measurement
        .results
        .iter()
        .filter(|r| r.has_result())
        .count();
    let ok = measurement
        .results
        .iter()
        .filter(|r| r.is_success())
        .count();
    let refused = measurement
        .results
        .iter()
        .filter(|r| r.is_refused())
        .count();
    let mut evidence = json!({
        "measurement_id": id,
        "probes_total": measurement.results.len(),
        "probes_with_result": with_result,
        "probes_ok": ok,
        "probes_refused": refused,
        "probes": measurement
            .results
            .iter()
            .take(EVIDENCE_PROBES)
            .map(probe_evidence)
            .collect::<Vec<_>>(),
    });
    let min_probes = usize::try_from(settings.min_probes).unwrap_or(usize::MAX);
    let vote = if with_result < min_probes {
        evidence["reason"] = json!(AbstainReason::NoData);
        Vote::Abstain(AbstainReason::NoData)
    } else if refused * 2 > with_result {
        evidence["reason"] = json!(AbstainReason::PortRefused);
        Vote::Abstain(AbstainReason::PortRefused)
    } else if (ok as f64) / (with_result as f64) < settings.success_threshold {
        Vote::Blocked
    } else {
        Vote::Clear
    };
    SensorVote { vote, evidence }
}

/// An abstention with the reason in the evidence, and what is known about the call that failed.
fn abstain(
    reason: AbstainReason,
    http_status: Option<u16>,
    measurement_id: Option<&str>,
) -> SensorVote {
    let mut evidence = Map::new();
    evidence.insert("reason".to_owned(), json!(reason));
    if let Some(status) = http_status {
        evidence.insert("http_status".to_owned(), json!(status));
    }
    if let Some(id) = measurement_id {
        evidence.insert("measurement_id".to_owned(), json!(id));
    }
    SensorVote {
        vote: Vote::Abstain(reason),
        evidence: Value::Object(evidence),
    }
}

fn failed(error: ClientError, measurement_id: Option<&str>) -> SensorVote {
    abstain(error.abstain_reason(), error.http_status(), measurement_id)
}

/// Checks the measurement an agent names in its report.
pub struct GlobalpingVerifier {
    client: GlobalpingClient,
    settings: SharedSettings,
    budget: Duration,
}

impl GlobalpingVerifier {
    pub fn new(client: GlobalpingClient, settings: SharedSettings) -> GlobalpingVerifier {
        GlobalpingVerifier {
            client,
            settings,
            budget: VERIFY_BUDGET,
        }
    }

    pub fn with_budget(mut self, budget: Duration) -> GlobalpingVerifier {
        self.budget = budget;
        self
    }
}

impl MeasurementVerifier for GlobalpingVerifier {
    fn verify<'a>(&'a self, id: &'a str, expect: &'a GpExpect) -> VerifyFuture<'a> {
        Box::pin(async move {
            let measurement = match self.client.wait_finished(id, self.budget).await {
                Ok(measurement) => measurement,
                Err(ClientError::NotFound) => return Err(GpRejected::UnknownMeasurement),
                Err(error) => {
                    tracing::debug!(%error, "cannot verify a measurement");
                    return Err(GpRejected::Unavailable);
                }
            };
            let settings = self.settings.load_full();
            check_measurement(&measurement, expect, &settings)?;
            Ok(tally(id, &measurement, &settings))
        })
    }
}

/// Makes a measurement itself: the round vote when no fresh agent report exists.
pub struct GlobalpingSensor {
    client: GlobalpingClient,
    settings: SharedSettings,
    budget: Duration,
}

impl GlobalpingSensor {
    pub fn new(client: GlobalpingClient, settings: SharedSettings) -> GlobalpingSensor {
        GlobalpingSensor {
            client,
            settings,
            budget: CENTRAL_BUDGET,
        }
    }

    pub fn with_budget(mut self, budget: Duration) -> GlobalpingSensor {
        self.budget = budget;
        self
    }
}

#[async_trait]
impl Sensor for GlobalpingSensor {
    fn kind(&self) -> SensorKind {
        SensorKind::Globalping
    }

    async fn vote(&self, target: &Target) -> SensorVote {
        let settings = self.settings.load_full();
        if !self.client.has_token() {
            return abstain(AbstainReason::NotConfigured, None, None);
        }
        // The allowance first: it costs nothing, a measurement does. Over the allowance
        // Globalping charges credits, and only the settings may spend those.
        let limits = match self.client.limits().await {
            Ok(limits) => limits,
            Err(error) => return failed(error, None),
        };
        if limits.create_remaining < u64::from(settings.min_limit_remaining)
            && !settings.allow_credits
        {
            let mut vote = abstain(AbstainReason::RateLimited, None, None);
            vote.evidence["limit_remaining"] = json!(limits.create_remaining);
            return vote;
        }
        let created = match self.client.create(target, settings.probe_limit).await {
            Ok(created) => created,
            Err(error) => return failed(error, None),
        };
        match self.client.wait_finished(&created.id, self.budget).await {
            Ok(measurement) => tally(&created.id, &measurement, &settings),
            Err(error) => failed(error, Some(&created.id)),
        }
    }
}
