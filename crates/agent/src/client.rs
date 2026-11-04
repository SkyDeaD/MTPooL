//! The registrar's agent API, decoded into [`Answer`] so the lifecycle matches on what an answer
//! means, not on status codes.

use std::path::Path;
use std::time::Duration;

use mtpool_protocol::agent::{
    AgentConfig, Empty, EmptyRequest, EnrollRequest, EnrollResponse, ReportRequest, Terminate,
    UpdateOffer, UpdatePoll, UpdateStatus,
};
use mtpool_protocol::problem::Problem;
use reqwest::Url;
use serde::de::DeserializeOwned;

/// Used when a 429 carries no usable `Retry-After`.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);
/// No pause the registrar asks for is taken as longer than this.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, PartialEq)]
pub enum Answer<T> {
    /// 200 with a body that decoded.
    Ok(T),
    /// 403 with the terminate document: the node is out of the pool.
    Terminate(Terminate),
    /// 410: the registrar does not know this node.
    Unknown,
    /// 401: the token is not valid.
    Unauthorized,
    /// 429: stay silent for this long.
    Paused { retry_after: Duration },
    /// Any other answer of the registrar (4xx, 5xx).
    Problem(Problem),
    /// No answer, or one that cannot be read.
    Transport(String),
}

#[derive(Debug, thiserror::Error)]
#[error("cannot build the HTTP client: {0}")]
pub struct ClientError(String);

/// The largest answer the client reads.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// A build may be some megabytes on a slow link.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct RegistrarClient {
    http: reqwest::Client,
    base: Url,
}

impl RegistrarClient {
    /// `base` is the address of the registrar, for example `https://pool.example.com`.
    pub fn new(base: Url) -> Result<RegistrarClient, ClientError> {
        if base.cannot_be_a_base() {
            return Err(ClientError(format!("{base} is not a usable base address")));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            // A registrar that redirects is misconfigured: the token must not follow it.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("mtpool-agent/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ClientError(e.to_string()))?;
        Ok(RegistrarClient { http, base })
    }

    fn url(&self, route: &str) -> Url {
        let mut url = self.base.clone();
        url.set_query(None);
        url.set_fragment(None);
        if let Ok(mut segments) = url.path_segments_mut() {
            segments
                .pop_if_empty()
                .extend(["api", "v1", "agent", route]);
        }
        url
    }

    pub async fn enroll(
        &self,
        enroll_token: &str,
        request: &EnrollRequest,
    ) -> Answer<EnrollResponse> {
        self.send(
            self.http
                .post(self.url("enroll"))
                .bearer_auth(enroll_token)
                .json(request),
        )
        .await
    }

    pub async fn heartbeat(&self, node_token: &str) -> Answer<Empty> {
        self.send(
            self.http
                .post(self.url("heartbeat"))
                .bearer_auth(node_token)
                .json(&EmptyRequest {}),
        )
        .await
    }

    pub async fn report(&self, node_token: &str, request: &ReportRequest) -> Answer<Empty> {
        self.send(
            self.http
                .post(self.url("report"))
                .bearer_auth(node_token)
                .json(request),
        )
        .await
    }

    pub async fn retire(&self, node_token: &str) -> Answer<Empty> {
        self.send(
            self.http
                .post(self.url("retire"))
                .bearer_auth(node_token)
                .json(&EmptyRequest {}),
        )
        .await
    }

    pub async fn config(&self, node_token: &str) -> Answer<AgentConfig> {
        self.send(self.http.get(self.url("config")).bearer_auth(node_token))
            .await
    }

    /// Asks which version the registrar wants this node on.
    pub async fn update(&self, node_token: &str, request: &UpdatePoll) -> Answer<UpdateOffer> {
        self.send(
            self.http
                .post(self.url("update"))
                .bearer_auth(node_token)
                .json(request),
        )
        .await
    }

    /// Tells the registrar how the update went.
    pub async fn update_status(&self, node_token: &str, request: &UpdateStatus) -> Answer<Empty> {
        self.send(
            self.http
                .post(self.url("update-status"))
                .bearer_auth(node_token)
                .json(request),
        )
        .await
    }

    /// Downloads `/releases/{name}` into `dest`, at most `max` bytes. The route is public: no
    /// token goes with it.
    pub async fn release_file(&self, name: &str, dest: &Path, max: u64) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let mut url = self.base.clone();
        url.set_query(None);
        url.set_fragment(None);
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty().extend(["releases", name]);
        }
        let mut response = self
            .http
            .get(url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|e| describe(&e))?;
        if !response.status().is_success() {
            return Err(format!("the registrar answered {}", response.status()));
        }
        if response.content_length().is_some_and(|n| n > max) {
            return Err("the file is too large".to_owned());
        }
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| e.to_string())?;
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|e| describe(&e))? {
            size += chunk.len() as u64;
            if size > max {
                return Err("the file is too large".to_owned());
            }
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())
    }

    async fn send<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Answer<T> {
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Answer::Transport(describe(&error)),
        };
        let status = response.status();
        let retry_after = retry_after(response.headers().get(reqwest::header::RETRY_AFTER));

        let mut body = Vec::new();
        if response
            .content_length()
            .is_some_and(|n| n > MAX_ANSWER_BYTES as u64)
        {
            return Answer::Transport("the answer is too large".to_owned());
        }
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > MAX_ANSWER_BYTES {
                        return Answer::Transport("the answer is too large".to_owned());
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => return Answer::Transport(describe(&error)),
            }
        }

        match status.as_u16() {
            _ if status.is_success() => match serde_json::from_slice(&body) {
                Ok(value) => Answer::Ok(value),
                Err(error) => Answer::Transport(format!("cannot read the answer: {error}")),
            },
            401 => Answer::Unauthorized,
            410 => Answer::Unknown,
            429 => Answer::Paused { retry_after },
            403 => match serde_json::from_slice::<Terminate>(&body) {
                Ok(terminate) if terminate.terminate => Answer::Terminate(terminate),
                _ => Answer::Problem(problem(status.as_u16(), &body)),
            },
            code => Answer::Problem(problem(code, &body)),
        }
    }
}

/// The problem document in `body`, or one made from the status if there is none.
fn problem(status: u16, body: &[u8]) -> Problem {
    serde_json::from_slice::<Problem>(body).unwrap_or_else(|_| {
        let reason = reqwest::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("Unexpected status");
        Problem::new(
            status,
            "http_error",
            reason,
            "The answer has no problem document.",
        )
    })
}

/// `Retry-After` in seconds; anything else (a date, text, nothing) is the default. At least one
/// second, at most [`MAX_RETRY_AFTER`].
fn retry_after(value: Option<&reqwest::header::HeaderValue>) -> Duration {
    value
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER, |seconds| {
            Duration::from_secs(seconds).clamp(Duration::from_secs(1), MAX_RETRY_AFTER)
        })
}

/// The error and its causes, without the address (it is in the configuration anyway).
fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
