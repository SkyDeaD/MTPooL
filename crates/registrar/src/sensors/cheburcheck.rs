//! The cheburcheck sensor.
//!
//! cheburcheck.ru checks an address in two steps. The fast one (`GET /api/v1/check`) looks the
//! address up in lists and answers with a `blocked` flag; it fires for whole provider networks
//! (Cloudflare and Hetzner addresses are `true`), so it only goes into the evidence as
//! `provider_listed` and never into the vote. The vote comes from the second step: probes in
//! Russian networks stream their verdicts over SSE (`GET /api/v1/probe/{id}`).
//!
//! A probe that says `tspu_block` or `sni_block` is a block, `ok` is clear, anything else
//! (`uncertain`, a word nobody knows) is not counted. More than half of the counted probes
//! blocked gives `Blocked`; a tie is `Clear`. Whatever goes wrong on the way (an unreachable
//! service, a refusal, an unreadable answer, no counted probe) is an abstention.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mtpool_core::{AbstainReason, SensorKind, SensorVote, Vote};
use reqwest::{Url, redirect};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{Sensor, Target};

#[cfg(test)]
mod tests;

pub const DEFAULT_BASE: &str = "https://cheburcheck.ru/";
/// The time the whole probe stream may take, connecting included.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(90);
/// How many probe streams the whole pool starts per minute.
pub const RATE_LIMIT: usize = 4;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CHECK_BYTES: usize = 1 << 20;
const MAX_STREAM_BYTES: usize = 1 << 20;
const MAX_EVIDENCE_PROBES: usize = 50;
const MAX_LABEL_CHARS: usize = 64;
const MAX_ID_BYTES: usize = 2048;

/// A sliding window: at most `max` stamps inside `window`.
pub struct Limiter {
    max: usize,
    window: Duration,
    stamps: Mutex<VecDeque<Instant>>,
}

impl Limiter {
    pub fn new(max: usize, window: Duration) -> Limiter {
        Limiter {
            max,
            window,
            stamps: Mutex::new(VecDeque::new()),
        }
    }

    /// Takes a place in the window, or says there is none. A refusal takes nothing.
    pub fn try_acquire(&self, now: Instant) -> bool {
        let mut stamps = self.stamps.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(&oldest) = stamps.front() {
            if oldest + self.window <= now {
                stamps.pop_front();
            } else {
                break;
            }
        }
        if stamps.len() < self.max {
            stamps.push_back(now);
            true
        } else {
            false
        }
    }
}

/// One event of an SSE stream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub name: Option<String>,
    pub data: String,
}

/// Cuts a byte stream into SSE events. Chunks may end anywhere, even inside a character.
#[derive(Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    name: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(event) = self.line(&line) {
                events.push(event);
            }
        }
        events
    }

    /// The end of the stream: an event that was not closed by an empty line still counts.
    pub fn finish(&mut self) -> Option<SseEvent> {
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if let Some(event) = self.line(&line) {
                return Some(event);
            }
        }
        self.dispatch()
    }

    fn line(&mut self, line: &[u8]) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        if line[0] == b':' {
            return None;
        }
        let text = String::from_utf8_lossy(line);
        let (field, value) = text.split_once(':').unwrap_or((&text, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.name = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let name = self.name.take();
        let data = std::mem::take(&mut self.data);
        if name.is_none() && data.is_empty() {
            return None;
        }
        Some(SseEvent {
            name,
            data: data.join("\n"),
        })
    }
}

/// Why a vote could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    RateLimited,
    Unavailable(Option<u16>),
    Timeout,
    /// An answer in a shape nobody knows; the text says which.
    Unknown(&'static str),
}

impl Fail {
    fn reason(self) -> AbstainReason {
        match self {
            Fail::RateLimited => AbstainReason::RateLimited,
            Fail::Unavailable(_) => AbstainReason::Unavailable,
            Fail::Timeout => AbstainReason::Timeout,
            Fail::Unknown(_) => AbstainReason::UnknownAnswer,
        }
    }

    fn evidence(self) -> Value {
        match self {
            Fail::RateLimited => json!({"reason": "rate_limited"}),
            Fail::Unavailable(Some(status)) => {
                json!({"reason": "unavailable", "http_status": status})
            }
            Fail::Unavailable(None) => json!({"reason": "unavailable"}),
            Fail::Timeout => json!({"reason": "timeout"}),
            Fail::Unknown(why) => json!({"reason": why}),
        }
    }

    fn from_send(error: reqwest::Error) -> Fail {
        let timed_out = error.is_timeout();
        tracing::debug!(error = %error.without_url(), "cheburcheck request failed");
        if timed_out {
            Fail::Timeout
        } else {
            Fail::Unavailable(None)
        }
    }
}

/// The answer of the fast step that the sensor uses.
struct Checked {
    id: String,
    provider_listed: bool,
}

/// What one probe said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Blocked,
    Clear,
    Uncertain,
}

/// One probe's verdicts as a single one. A block wins over everything; `ok` over a word nobody
/// knows; anything else is not counted.
fn classify(verdicts: &[&str]) -> (Class, &'static str) {
    for verdict in verdicts {
        match *verdict {
            "tspu_block" => return (Class::Blocked, "tspu_block"),
            "sni_block" => return (Class::Blocked, "sni_block"),
            _ => {}
        }
    }
    if verdicts.contains(&"ok") {
        (Class::Clear, "ok")
    } else if verdicts.contains(&"uncertain") || verdicts.is_empty() {
        (Class::Uncertain, "uncertain")
    } else {
        (Class::Uncertain, "unknown")
    }
}

#[derive(Default)]
struct Tally {
    blocked: u32,
    clear: u32,
    uncertain: u32,
    probes: Vec<Value>,
    /// The budget ran out before the `done` event.
    budget_exhausted: bool,
}

impl Tally {
    fn counted(&self) -> u32 {
        self.blocked + self.clear
    }

    /// Takes one event. `true` means the stream is over.
    fn event(&mut self, event: &SseEvent) -> Result<bool, Fail> {
        match event.name.as_deref() {
            Some("started") => Ok(false),
            Some("done") => Ok(true),
            Some("result") => {
                self.result(&event.data)?;
                Ok(false)
            }
            _ => Err(Fail::Unknown("unknown_event")),
        }
    }

    fn result(&mut self, data: &str) -> Result<(), Fail> {
        let bad = Fail::Unknown("bad_result_event");
        let document: Value = serde_json::from_str(data).map_err(|_| bad)?;
        let verdicts = document
            .get("verdicts")
            .and_then(Value::as_array)
            .ok_or(bad)?
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<&str>>>()
            .ok_or(bad)?;
        let (class, label) = classify(&verdicts);
        match class {
            Class::Blocked => self.blocked += 1,
            Class::Clear => self.clear += 1,
            Class::Uncertain => self.uncertain += 1,
        }
        if self.probes.len() < MAX_EVIDENCE_PROBES {
            let mut probe = serde_json::Map::new();
            probe.insert("verdict".into(), json!(label));
            for key in ["provider", "region"] {
                if let Some(text) = document.get(key).and_then(Value::as_str) {
                    let short: String = text.chars().take(MAX_LABEL_CHARS).collect();
                    probe.insert(key.into(), json!(short));
                }
            }
            self.probes.push(Value::Object(probe));
        }
        Ok(())
    }
}

/// An id is a path segment of the stream URL, so it must be one.
fn valid_id(id: &str) -> bool {
    (1..=MAX_ID_BYTES).contains(&id.len())
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[derive(Clone)]
pub struct CheburcheckSensor {
    http: reqwest::Client,
    base: Url,
    budget: Duration,
    limiter: Arc<Limiter>,
}

impl CheburcheckSensor {
    /// `base` is the site root (`DEFAULT_BASE`); tests point it at a local server.
    pub fn new(mut base: Url) -> Result<CheburcheckSensor, reqwest::Error> {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        // No total timeout here: the check has its own, the stream has the budget.
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(redirect::Policy::none())
            .user_agent(concat!("mtpool-registrar/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(CheburcheckSensor {
            http,
            base,
            budget: DEFAULT_BUDGET,
            limiter: Arc::new(Limiter::new(RATE_LIMIT, RATE_WINDOW)),
        })
    }

    pub fn default_base() -> Url {
        Url::parse(DEFAULT_BASE).expect("the default base is a URL")
    }

    /// The time the probe stream may take. The check before it has its own 30 seconds.
    pub fn with_budget(mut self, budget: Duration) -> CheburcheckSensor {
        self.budget = budget;
        self
    }

    fn status_failure(status: reqwest::StatusCode) -> Fail {
        match status.as_u16() {
            429 => Fail::RateLimited,
            other => Fail::Unavailable(Some(other)),
        }
    }

    /// The fast step. Only the address leaves the machine.
    async fn check(&self, target: &Target) -> Result<Checked, Fail> {
        let mut url = self
            .base
            .join("api/v1/check")
            .expect("a fixed path joins the base");
        url.query_pairs_mut()
            .append_pair("target", &target.ip.to_string());
        let mut response = self
            .http
            .get(url)
            .timeout(CHECK_TIMEOUT)
            .send()
            .await
            .map_err(Fail::from_send)?;
        if !response.status().is_success() {
            return Err(Self::status_failure(response.status()));
        }
        let status = response.status().as_u16();
        let too_large = |n: u64| usize::try_from(n).map_or(true, |n| n > MAX_CHECK_BYTES);
        if response.content_length().is_some_and(too_large) {
            return Err(Fail::Unknown("answer_too_large"));
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > MAX_CHECK_BYTES {
                        return Err(Fail::Unknown("answer_too_large"));
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(if e.is_timeout() {
                        Fail::Timeout
                    } else {
                        Fail::Unavailable(Some(status))
                    });
                }
            }
        }
        let bad = Fail::Unknown("bad_check_answer");
        let document: Value = serde_json::from_slice(&body).map_err(|_| bad)?;
        let provider_listed = document
            .get("blocked")
            .and_then(Value::as_bool)
            .ok_or(bad)?;
        let id = document.get("id").and_then(Value::as_str).ok_or(bad)?;
        if !valid_id(id) {
            return Err(Fail::Unknown("bad_probe_id"));
        }
        Ok(Checked {
            id: id.to_owned(),
            provider_listed,
        })
    }

    /// Reads the probe stream until `done`, the end of the budget or a failure.
    async fn stream(&self, id: &str) -> Result<Tally, Fail> {
        let deadline = Instant::now() + self.budget;
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| Fail::Unknown("bad_base"))?
            .pop_if_empty()
            .extend(["api", "v1", "probe", id]);
        let mut tally = Tally::default();
        let sent = tokio::time::timeout_at(deadline, self.http.get(url).send()).await;
        let mut response = match sent {
            Err(_) => {
                tally.budget_exhausted = true;
                return Ok(tally);
            }
            Ok(Err(e)) => return Err(Fail::from_send(e)),
            Ok(Ok(response)) => response,
        };
        let status = response.status();
        if !status.is_success() {
            return Err(Self::status_failure(status));
        }
        let mut parser = SseParser::default();
        let mut received = 0usize;
        loop {
            match tokio::time::timeout_at(deadline, response.chunk()).await {
                Err(_) => {
                    tally.budget_exhausted = true;
                    return Ok(tally);
                }
                Ok(Ok(Some(chunk))) => {
                    received += chunk.len();
                    if received > MAX_STREAM_BYTES {
                        return Err(Fail::Unknown("stream_too_large"));
                    }
                    for event in parser.push(&chunk) {
                        if tally.event(&event)? {
                            return Ok(tally);
                        }
                    }
                }
                Ok(Ok(None)) => {
                    // A stream that closes without `done` was cut short.
                    if let Some(event) = parser.finish()
                        && tally.event(&event)?
                    {
                        return Ok(tally);
                    }
                    return Err(Fail::Unavailable(Some(status.as_u16())));
                }
                Ok(Err(e)) => {
                    return Err(if e.is_timeout() {
                        Fail::Timeout
                    } else {
                        Fail::Unavailable(Some(status.as_u16()))
                    });
                }
            }
        }
    }
}

fn abstain(fail: Fail, extra: &[(&str, Value)]) -> SensorVote {
    let mut evidence = fail.evidence();
    for (key, value) in extra {
        evidence[*key] = value.clone();
    }
    SensorVote {
        vote: Vote::Abstain(fail.reason()),
        evidence,
    }
}

/// More than half of the counted probes blocked: `Blocked`. A tie is `Clear`.
fn decide(checked: &Checked, tally: Tally) -> SensorVote {
    let counted = tally.counted();
    let mut evidence = json!({
        "provider_listed": checked.provider_listed,
        "probe_id": checked.id,
        "counted": counted,
        "blocked": tally.blocked,
        "clear": tally.clear,
        "uncertain": tally.uncertain,
        "probes": tally.probes,
    });
    let vote = if counted == 0 {
        // The budget ran out before anything counted, or nothing counted at all.
        let (reason, code) = if tally.budget_exhausted {
            (AbstainReason::Timeout, "timeout")
        } else {
            (AbstainReason::NoData, "no_counted_probes")
        };
        evidence["reason"] = json!(code);
        Vote::Abstain(reason)
    } else if tally.blocked * 2 > counted {
        Vote::Blocked
    } else {
        Vote::Clear
    };
    if tally.budget_exhausted && counted > 0 {
        evidence["partial"] = json!(true);
    }
    SensorVote { vote, evidence }
}

#[async_trait]
impl Sensor for CheburcheckSensor {
    fn kind(&self) -> SensorKind {
        SensorKind::Cheburcheck
    }

    async fn vote(&self, target: &Target) -> SensorVote {
        // The limit is checked first: a limited vote sends nothing.
        if !self.limiter.try_acquire(Instant::now()) {
            return abstain(Fail::RateLimited, &[]);
        }
        let checked = match self.check(target).await {
            Ok(checked) => checked,
            Err(fail) => return abstain(fail, &[]),
        };
        match self.stream(&checked.id).await {
            Ok(tally) => decide(&checked, tally),
            Err(fail) => abstain(
                fail,
                &[
                    ("provider_listed", json!(checked.provider_listed)),
                    ("probe_id", json!(checked.id)),
                ],
            ),
        }
    }
}
