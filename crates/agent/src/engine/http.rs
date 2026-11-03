//! The engine's HTTP interfaces (`/metrics`, the control API) are on the loopback of the node.
//! [`HttpGet`] is all the adapter needs from a client; tests answer from a script.

use std::future::Future;
use std::time::Duration;

/// How long one request may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

pub trait HttpGet: Send + Sync {
    /// `GET url`, with `authorization` as the `Authorization` header if given. `Err` is a failure
    /// to get any answer; an answer with an error status is an `Ok`.
    fn get(
        &self,
        url: &str,
        authorization: Option<&str>,
    ) -> impl Future<Output = Result<HttpResponse, String>> + Send;
}

/// The real client: no redirects and no proxy, so a request meant for the loopback stays there.
#[derive(Debug, Clone)]
pub struct ReqwestHttp {
    client: reqwest::Client,
}

impl ReqwestHttp {
    pub fn new() -> Result<ReqwestHttp, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(ReqwestHttp { client })
    }
}

impl HttpGet for ReqwestHttp {
    async fn get(&self, url: &str, authorization: Option<&str>) -> Result<HttpResponse, String> {
        let mut request = self.client.get(url);
        if let Some(value) = authorization {
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }
        let response = request
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|e| e.without_url().to_string())?;
        Ok(HttpResponse { status, body })
    }
}

/// A client that answers from a script, for tests of the adapters.
pub mod testing {
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    use super::{HttpGet, HttpResponse};

    /// One request the client received.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Request {
        pub url: String,
        pub authorization: Option<String>,
    }

    #[derive(Debug, Default)]
    pub struct ScriptedHttp {
        requests: Mutex<Vec<Request>>,
        queued: Mutex<BTreeMap<String, VecDeque<Result<HttpResponse, String>>>>,
        always: Mutex<BTreeMap<String, Result<HttpResponse, String>>>,
    }

    impl ScriptedHttp {
        pub fn new() -> ScriptedHttp {
            ScriptedHttp::default()
        }
        /// The next request for `url` gets this answer (answers queue up per URL).
        pub fn push(&self, url: &str, answer: Result<HttpResponse, String>) {
            lock(&self.queued)
                .entry(url.to_owned())
                .or_default()
                .push_back(answer);
        }
        /// Every request for `url` that has no queued answer gets this one.
        pub fn always(&self, url: &str, answer: Result<HttpResponse, String>) {
            lock(&self.always).insert(url.to_owned(), answer);
        }
        pub fn requests(&self) -> Vec<Request> {
            lock(&self.requests).clone()
        }
    }

    impl HttpGet for ScriptedHttp {
        async fn get(
            &self,
            url: &str,
            authorization: Option<&str>,
        ) -> Result<HttpResponse, String> {
            lock(&self.requests).push(Request {
                url: url.to_owned(),
                authorization: authorization.map(str::to_owned),
            });
            if let Some(answer) = lock(&self.queued)
                .get_mut(url)
                .and_then(VecDeque::pop_front)
            {
                return answer;
            }
            lock(&self.always)
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(format!("no scripted answer for {url}")))
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
