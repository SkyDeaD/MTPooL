//! The Cloudflare DNS provider: a thin client over `reqwest`.
//!
//! One request at a time goes through `call`: it asks for the token, keeps to the pace of three
//! requests a second for the whole provider, waits once and tries once more after a 429, and
//! turns what comes back into a `DnsError`. The token is read from its source on every request,
//! sent as a bearer and never put in an error or a log line.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use mtpool_core::{LiveRecord, NewRecord, RecordKind};
use mtpool_protocol::parse_http_date;
use reqwest::{Method, Url, redirect};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{DnsError, DnsProvider, MANAGED_COMMENT, ProviderRecord};
use crate::sensors::globalping::TokenSource;

#[cfg(test)]
mod tests;

pub const DEFAULT_BASE: &str = "https://api.cloudflare.com/";
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUESTS_PER_SECOND: usize = 3;
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(10);
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// Pages of records read for one name; 100 records a page.
pub const MAX_PAGES: usize = 20;
/// An answer larger than this is refused unread.
pub const MAX_ANSWER_BYTES: usize = 2 * 1024 * 1024;
const PER_PAGE: usize = 100;
/// The code Cloudflare gives a token of the wrong format.
const BAD_AUTH_HEADER_CODE: i64 = 6003;

/// How long to wait after a 429: the `Retry-After` header as seconds or as an HTTP date, ten
/// seconds when it is missing or unreadable, and never more than a minute.
pub fn retry_after_delay(header: Option<&str>, now_ms: i64) -> Duration {
    let wait = header
        .map(str::trim)
        .and_then(|text| {
            text.parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    parse_http_date(text).map(|at| {
                        Duration::from_millis(u64::try_from(at.saturating_sub(now_ms)).unwrap_or(0))
                    })
                })
        })
        .unwrap_or(DEFAULT_RETRY_AFTER);
    wait.min(MAX_RETRY_AFTER)
}

fn now_ms() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}

/// At most `max` requests in any `window`.
pub struct Pacer {
    max: usize,
    window: Duration,
    stamps: Mutex<VecDeque<Instant>>,
}

impl Pacer {
    pub fn new(max: usize, window: Duration) -> Pacer {
        Pacer {
            max,
            window,
            stamps: Mutex::new(VecDeque::new()),
        }
    }

    /// Returns when the caller may send one request.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut stamps = self.stamps.lock().expect("pacer lock");
                let now = Instant::now();
                while stamps
                    .front()
                    .is_some_and(|first| now.duration_since(*first) >= self.window)
                {
                    stamps.pop_front();
                }
                if stamps.len() < self.max {
                    stamps.push_back(now);
                    return;
                }
                let first = *stamps.front().expect("a full window has a first stamp");
                (first + self.window).saturating_duration_since(now)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// What came back, before anyone looks at the body.
struct Answer {
    status: u16,
    body: Vec<u8>,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    result_info: Option<ResultInfo>,
}

#[derive(Deserialize)]
struct ResultInfo {
    #[serde(default)]
    total_pages: usize,
}

#[derive(Deserialize)]
struct RawRecord {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    proxied: bool,
    #[serde(default)]
    comment: Option<String>,
}

impl RawRecord {
    fn into_provider(self) -> ProviderRecord {
        let kind = match self.kind.as_str() {
            "A" => RecordKind::A,
            "AAAA" => RecordKind::Aaaa,
            "CNAME" => RecordKind::Cname,
            other => RecordKind::Other(other.to_owned()),
        };
        ProviderRecord {
            managed: self.comment.as_deref() == Some(MANAGED_COMMENT),
            live: LiveRecord {
                id: self.id,
                name: self.name,
                kind,
                content: self.content,
                proxied: self.proxied,
            },
        }
    }
}

/// Ids are put in a path: only what can sit in a path segment unchanged.
fn is_plain_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn plain_id(id: &str) -> Result<&str, DnsError> {
    if is_plain_id(id) {
        Ok(id)
    } else {
        Err(DnsError::Unexpected("an id that is not plain".to_owned()))
    }
}

fn unexpected(what: &str) -> DnsError {
    DnsError::Unexpected(what.to_owned())
}

pub struct CloudflareDns {
    http: reqwest::Client,
    base: Url,
    token: TokenSource,
    timeout: Duration,
    pacer: Pacer,
    zones: Mutex<HashMap<String, String>>,
}

impl CloudflareDns {
    /// `base` is the API root (`DEFAULT_BASE`); tests point it at a local server.
    pub fn new(mut base: Url, token: TokenSource) -> Result<CloudflareDns, reqwest::Error> {
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            // The token goes to the one host it was made for.
            .redirect(redirect::Policy::none())
            .user_agent(concat!("mtpool-registrar/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(CloudflareDns {
            http,
            base,
            token,
            timeout: REQUEST_TIMEOUT,
            pacer: Pacer::new(REQUESTS_PER_SECOND, Duration::from_secs(1)),
            zones: Mutex::new(HashMap::new()),
        })
    }

    pub fn default_base() -> Url {
        Url::parse(DEFAULT_BASE).expect("the default base is a URL")
    }

    pub fn with_timeout(mut self, timeout: Duration) -> CloudflareDns {
        self.timeout = timeout;
        self
    }

    fn url(&self, path: &str) -> Url {
        self.base.join(path).expect("a fixed path joins the base")
    }

    fn token(&self) -> Result<String, DnsError> {
        match (self.token)() {
            Some(token) if !token.is_empty() => Ok(token),
            _ => Err(DnsError::NotConfigured),
        }
    }

    /// One request, with the pace kept and one retry after a 429. Statuses that mean the token
    /// is refused or the pool is limited end here; any other status goes back to the caller.
    async fn call(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<Answer, DnsError> {
        let mut retried = false;
        loop {
            let token = self.token()?;
            self.pacer.acquire().await;
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .timeout(self.timeout)
                .bearer_auth(token);
            if let Some(body) = &body {
                request = request
                    .header("content-type", "application/json")
                    .body(body.clone());
            }
            let mut response = request.send().await.map_err(|e| {
                if e.is_timeout() {
                    DnsError::Timeout
                } else {
                    DnsError::Transport(e.without_url().to_string())
                }
            })?;
            let status = response.status().as_u16();
            if status == 429 {
                let header = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let wait = retry_after_delay(header.as_deref(), now_ms());
                if retried {
                    return Err(DnsError::RateLimited { retry_after: wait });
                }
                retried = true;
                tracing::debug!(?wait, "cloudflare rate limited, waiting once");
                tokio::time::sleep(wait).await;
                continue;
            }
            if response
                .content_length()
                .is_some_and(|n| usize::try_from(n).map_or(true, |n| n > MAX_ANSWER_BYTES))
            {
                return Err(unexpected("the answer is too large"));
            }
            let mut bytes = Vec::new();
            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => {
                        if bytes.len() + chunk.len() > MAX_ANSWER_BYTES {
                            return Err(unexpected("the answer is too large"));
                        }
                        bytes.extend_from_slice(&chunk);
                    }
                    Ok(None) => break,
                    Err(e) if e.is_timeout() => return Err(DnsError::Timeout),
                    Err(e) => return Err(DnsError::Transport(e.without_url().to_string())),
                }
            }
            if matches!(status, 401 | 403) || (status == 400 && has_bad_header_code(&bytes)) {
                return Err(DnsError::Auth);
            }
            return Ok(Answer {
                status,
                body: bytes,
            });
        }
    }

    /// The `result` of a successful answer, and the page count it names.
    fn envelope(answer: &Answer) -> Result<(Value, usize), DnsError> {
        if !(200..300).contains(&answer.status) {
            return Err(DnsError::Http(answer.status));
        }
        let envelope: Envelope = serde_json::from_slice(&answer.body)
            .map_err(|_| unexpected("the answer is not the expected JSON"))?;
        if !envelope.success {
            return Err(unexpected("the answer says it did not succeed"));
        }
        let pages = envelope.result_info.map_or(1, |info| info.total_pages);
        Ok((envelope.result, pages))
    }

    fn forget_zone(&self, zone: &str) {
        self.zones
            .lock()
            .expect("zone cache lock")
            .retain(|_, id| id != zone);
    }
}

/// True when the answer lists the code Cloudflare uses for a token of the wrong format.
fn has_bad_header_code(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|doc| doc.get("errors").and_then(Value::as_array).cloned())
        .is_some_and(|errors| {
            errors
                .iter()
                .any(|e| e.get("code").and_then(Value::as_i64) == Some(BAD_AUTH_HEADER_CODE))
        })
}

#[async_trait]
impl DnsProvider for CloudflareDns {
    async fn zone_id(&self, domain: &str) -> Result<Option<String>, DnsError> {
        let name = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        let labels: Vec<&str> = name.split('.').collect();
        if labels.len() < 2 || labels.iter().any(|l| l.is_empty()) {
            return Ok(None);
        }
        if let Some(id) = self.zones.lock().expect("zone cache lock").get(&name) {
            return Ok(Some(id.clone()));
        }
        // The full name first, then each parent, never fewer than two labels.
        for start in 0..=labels.len() - 2 {
            let candidate = labels[start..].join(".");
            let mut url = self.url("client/v4/zones");
            url.query_pairs_mut()
                .append_pair("name", &candidate)
                .append_pair("status", "active");
            let answer = self.call(Method::GET, url, None).await?;
            let (result, _) = Self::envelope(&answer)?;
            let first = result
                .as_array()
                .and_then(|zones| zones.first())
                .and_then(|zone| zone.get("id"))
                .and_then(Value::as_str);
            if let Some(id) = first {
                plain_id(id)?;
                self.zones
                    .lock()
                    .expect("zone cache lock")
                    .insert(name, id.to_owned());
                return Ok(Some(id.to_owned()));
            }
        }
        Ok(None)
    }

    async fn list(&self, zone: &str, name: &str) -> Result<Vec<ProviderRecord>, DnsError> {
        let zone = plain_id(zone)?;
        let mut records = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut url = self.url(&format!("client/v4/zones/{zone}/dns_records"));
            url.query_pairs_mut()
                .append_pair("name", name)
                .append_pair("per_page", &PER_PAGE.to_string())
                .append_pair("page", &page.to_string());
            let answer = self.call(Method::GET, url, None).await?;
            if answer.status == 404 {
                self.forget_zone(zone);
                return Err(DnsError::ZoneNotFound);
            }
            let (result, pages) = Self::envelope(&answer)?;
            let raw: Vec<RawRecord> = serde_json::from_value(result)
                .map_err(|_| unexpected("the records are not the expected JSON"))?;
            records.extend(raw.into_iter().map(RawRecord::into_provider));
            if page >= pages {
                return Ok(records);
            }
        }
        Err(unexpected("more pages of records than the cap"))
    }

    async fn create(
        &self,
        zone: &str,
        record: NewRecord,
        ttl: u32,
        proxied: bool,
    ) -> Result<String, DnsError> {
        let zone = plain_id(zone)?;
        let kind = match record.kind {
            RecordKind::A => "A",
            RecordKind::Cname => "CNAME",
            _ => return Err(unexpected("the pool creates only A and CNAME records")),
        };
        let body = json!({
            "type": kind,
            "name": record.name,
            "content": record.content,
            "ttl": ttl,
            "proxied": proxied,
            "comment": MANAGED_COMMENT,
        });
        let body =
            serde_json::to_vec(&body).map_err(|_| unexpected("the record cannot be sent"))?;
        let url = self.url(&format!("client/v4/zones/{zone}/dns_records"));
        let answer = self.call(Method::POST, url, Some(body)).await?;
        if answer.status == 404 {
            self.forget_zone(zone);
            return Err(DnsError::ZoneNotFound);
        }
        let (result, _) = Self::envelope(&answer)?;
        let id = result
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| unexpected("the created record has no id"))?;
        Ok(plain_id(id)?.to_owned())
    }

    async fn delete(&self, zone: &str, id: &str) -> Result<(), DnsError> {
        let zone = plain_id(zone)?;
        let id = plain_id(id)?;
        let url = self.url(&format!("client/v4/zones/{zone}/dns_records/{id}"));
        let answer = self.call(Method::DELETE, url, None).await?;
        match answer.status {
            // Already gone: the state asked for is the state there is.
            404 | 200..=299 => Ok(()),
            other => Err(DnsError::Http(other)),
        }
    }
}
