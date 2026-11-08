//! The RedFlag sensor.
//!
//! RedFlag checks a proxy from two places at once and answers with one status word. The sensor
//! sends the node's address, port and secret in link format (never a domain), with the API key
//! in the `X-API-Key` header, and turns the status into a vote:
//!
//! * `OK` is `Clear`;
//! * `RKN_BLOCK` and `DATA_CUT` are `Blocked`;
//! * `MT_DEAD`, `DOWN` and `UNKNOWN` say the proxy itself does not work, which is not a
//!   statement about a block: `Abstain(NoData)`;
//! * any other word, or an answer that cannot be read, is `Abstain(UnknownAnswer)`.
//!
//! Anything that goes wrong on the way is an abstention too; a 401 or a 403 says the key was not
//! accepted. After a 429 the sensor sends
//! nothing until the `Retry-After` time has passed. The pool-wide limit is 20 requests a
//! minute. The key is read on every call and goes only into the request header: not into the
//! evidence, not into a log line.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mtpool_core::{AbstainReason, SensorKind, SensorVote, Vote};
use mtpool_protocol::parse_http_date;
use reqwest::header::{CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{Url, redirect};
use serde_json::{Map, Value, json};
use tokio::time::Instant;

use super::globalping::TokenSource;
use super::limiter::Limiter;
use super::{Sensor, Target};

#[cfg(test)]
mod tests;

pub const DEFAULT_BASE: &str = "https://api.yazaebalsyadelatproxy.cc/";
/// How many checks the whole pool asks for per minute.
pub const RATE_LIMIT: usize = 20;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);
/// The wait after a 429 that names none.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);
/// No `Retry-After` can silence the sensor for longer than this.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);
/// The service is asked to take 15 seconds (`timeout=15`); this is the time we give it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_ANSWER_BYTES: usize = 256 * 1024;
const MAX_STATUS_BYTES: usize = 32;
const MAX_CHECKED_AT_BYTES: usize = 64;

/// How long a 429 silences the sensor. `header` is the value of `Retry-After`: seconds or an
/// HTTP date. A missing or unreadable one is a minute; a date in the past is no wait at all.
pub fn retry_delay(header: Option<&str>, now_ms: i64) -> Duration {
    let wait = match header.map(str::trim) {
        Some(text) => {
            if let Ok(seconds) = text.parse::<u64>() {
                Duration::from_secs(seconds)
            } else if let Some(at_ms) = parse_http_date(text) {
                let ms = at_ms.saturating_sub(now_ms).max(0);
                Duration::from_millis(u64::try_from(ms).unwrap_or(0))
            } else {
                DEFAULT_RETRY_AFTER
            }
        }
        None => DEFAULT_RETRY_AFTER,
    };
    wait.min(MAX_RETRY_AFTER)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Why a vote could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    NotConfigured(&'static str),
    KeyRejected(u16),
    RateLimited,
    Unavailable(Option<u16>),
    Timeout,
    Unknown(&'static str),
}

impl Fail {
    fn reason(self) -> AbstainReason {
        match self {
            Fail::NotConfigured(_) => AbstainReason::NotConfigured,
            Fail::KeyRejected(_) => AbstainReason::KeyRejected,
            Fail::RateLimited => AbstainReason::RateLimited,
            Fail::Unavailable(_) => AbstainReason::Unavailable,
            Fail::Timeout => AbstainReason::Timeout,
            Fail::Unknown(_) => AbstainReason::UnknownAnswer,
        }
    }

    fn vote(self) -> SensorVote {
        let evidence = match self {
            Fail::NotConfigured(why) | Fail::Unknown(why) => json!({"reason": why}),
            Fail::KeyRejected(status) => json!({"reason": "key_rejected", "http_status": status}),
            Fail::RateLimited => json!({"reason": "rate_limited"}),
            Fail::Unavailable(Some(status)) => {
                json!({"reason": "unavailable", "http_status": status})
            }
            Fail::Unavailable(None) => json!({"reason": "unavailable"}),
            Fail::Timeout => json!({"reason": "timeout"}),
        };
        SensorVote {
            vote: Vote::Abstain(self.reason()),
            evidence,
        }
    }

    fn from_send(error: reqwest::Error) -> Fail {
        let timed_out = error.is_timeout();
        // `without_url`: the address of the service is not worth a log line either.
        tracing::debug!(error = %error.without_url(), "redflag request failed");
        if timed_out {
            Fail::Timeout
        } else {
            Fail::Unavailable(None)
        }
    }
}

/// `text` cut to at most `max` bytes, on a character border.
fn clip(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// One side of the check, keeping only the documented plain fields.
fn side(value: Option<&Value>) -> Option<Value> {
    let object = value?.as_object()?;
    let mut kept = Map::new();
    for key in ["tcp", "handshake", "data"] {
        if let Some(flag) = object.get(key).filter(|v| v.is_boolean()) {
            kept.insert(key.to_owned(), flag.clone());
        }
    }
    if let Some(latency) = object
        .get("latency_ms")
        .filter(|v| v.is_number() || v.is_null())
    {
        kept.insert("latency_ms".to_owned(), latency.clone());
    }
    Some(Value::Object(kept))
}

/// The vote that an answer from the service makes.
fn judge(document: &Value) -> SensorVote {
    let Some(status) = document.get("status").and_then(Value::as_str) else {
        return Fail::Unknown("bad_answer").vote();
    };
    let mut evidence = Map::new();
    evidence.insert("status".to_owned(), json!(clip(status, MAX_STATUS_BYTES)));
    if let Some(at) = document.get("checked_at").and_then(Value::as_str) {
        evidence.insert(
            "checked_at".to_owned(),
            json!(clip(at, MAX_CHECKED_AT_BYTES)),
        );
    }
    for key in ["local", "remote"] {
        if let Some(kept) = side(document.get(key)) {
            evidence.insert(key.to_owned(), kept);
        }
    }
    // The words are matched as they are: a word that only looks like one is not one.
    let vote = match status {
        "OK" => Vote::Clear,
        "RKN_BLOCK" | "DATA_CUT" => Vote::Blocked,
        "MT_DEAD" | "DOWN" | "UNKNOWN" => {
            evidence.insert("reason".to_owned(), json!("no_verdict"));
            Vote::Abstain(AbstainReason::NoData)
        }
        _ => {
            evidence.insert("reason".to_owned(), json!("unknown_status"));
            Vote::Abstain(AbstainReason::UnknownAnswer)
        }
    };
    SensorVote {
        vote,
        evidence: Value::Object(evidence),
    }
}

#[derive(Clone)]
pub struct RedFlagSensor {
    http: reqwest::Client,
    base: Url,
    token: TokenSource,
    limiter: Arc<Limiter>,
    /// After a 429: no request before this.
    not_before: Arc<Mutex<Option<Instant>>>,
    timeout: Duration,
}

impl RedFlagSensor {
    /// `base` is the API root (`DEFAULT_BASE`); tests point it at a local server. `token` gives
    /// the key, or none, on every call.
    pub fn new(mut base: Url, token: TokenSource) -> Result<RedFlagSensor, reqwest::Error> {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        // No redirects: the key goes to the one host it was meant for.
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(redirect::Policy::none())
            .user_agent(concat!("mtpool-registrar/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(RedFlagSensor {
            http,
            base,
            token,
            limiter: Arc::new(Limiter::new(RATE_LIMIT, RATE_WINDOW)),
            not_before: Arc::new(Mutex::new(None)),
            timeout: REQUEST_TIMEOUT,
        })
    }

    pub fn default_base() -> Url {
        Url::parse(DEFAULT_BASE).expect("the default base is a URL")
    }

    /// The time one request may take.
    pub fn with_timeout(mut self, timeout: Duration) -> RedFlagSensor {
        self.timeout = timeout;
        self
    }

    fn silenced(&self, now: Instant) -> bool {
        let mut guard = self.not_before.lock().unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some(until) if now < until => true,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }

    fn silence_for(&self, delay: Duration) {
        let mut guard = self.not_before.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Instant::now() + delay);
    }

    async fn check(&self, target: &Target) -> Result<SensorVote, Fail> {
        let key = (self.token)()
            .filter(|k| !k.is_empty())
            .ok_or(Fail::NotConfigured("no_api_key"))?;
        if target.secret.is_empty() {
            return Err(Fail::NotConfigured("no_secret"));
        }
        let mut key =
            HeaderValue::from_str(&key).map_err(|_| Fail::NotConfigured("bad_api_key"))?;
        key.set_sensitive(true);
        // A silenced or limited vote sends nothing, and the limit is asked after the silence so
        // that a refused vote takes no place in the window.
        let now = Instant::now();
        if self.silenced(now) || !self.limiter.try_acquire(now) {
            return Err(Fail::RateLimited);
        }
        let mut url = self
            .base
            .join("v1/check")
            .expect("a fixed path joins the base");
        url.query_pairs_mut()
            .append_pair("mode", "sync")
            .append_pair("timeout", "15");
        let body = json!({
            "server": target.ip.to_string(),
            "port": target.port,
            "secret": target.secret,
        });
        let body = serde_json::to_vec(&body).expect("a JSON value serialises");
        let mut response = self
            .http
            .post(url)
            .header("X-API-Key", key)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(Fail::from_send)?;
        let status = response.status();
        if status.as_u16() == 429 {
            let header = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok());
            self.silence_for(retry_delay(header, now_ms()));
            return Err(Fail::RateLimited);
        }
        if matches!(status.as_u16(), 401 | 403) {
            tracing::warn!(
                status = status.as_u16(),
                "redflag does not accept the API key"
            );
            return Err(Fail::KeyRejected(status.as_u16()));
        }
        if !status.is_success() {
            return Err(Fail::Unavailable(Some(status.as_u16())));
        }
        let too_large = |n: u64| usize::try_from(n).map_or(true, |n| n > MAX_ANSWER_BYTES);
        if response.content_length().is_some_and(too_large) {
            return Err(Fail::Unknown("answer_too_large"));
        }
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_ANSWER_BYTES {
                        return Err(Fail::Unknown("answer_too_large"));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    return Err(if e.is_timeout() {
                        Fail::Timeout
                    } else {
                        Fail::Unavailable(Some(status.as_u16()))
                    });
                }
            }
        }
        let document: Value =
            serde_json::from_slice(&bytes).map_err(|_| Fail::Unknown("bad_answer"))?;
        Ok(judge(&document))
    }
}

#[async_trait]
impl Sensor for RedFlagSensor {
    fn kind(&self) -> SensorKind {
        SensorKind::RedFlag
    }

    async fn vote(&self, target: &Target) -> SensorVote {
        match self.check(target).await {
            Ok(vote) => vote,
            Err(fail) => fail.vote(),
        }
    }
}
