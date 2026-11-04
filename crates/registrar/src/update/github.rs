//! GitHub Releases as a [`ReleaseSource`](super::ReleaseSource).
//!
//! The newest release comes from `GET {api}/repos/{repo}/releases/latest`, asked with the ETag
//! of the last full answer so that an unchanged release costs nothing against the limit. The
//! files are fetched by name from `{download}/{repo}/releases/download/{tag}/{name}`, a URL built
//! here from the tag and the names in the signed manifest, never one taken from the API answer.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{ETAG, HeaderMap, IF_NONE_MATCH, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use super::{Latest, MANIFEST_NAME, Offer, ReleaseSource, SIGNATURE_NAME, SourceError};

pub const DEFAULT_API: &str = "https://api.github.com";
pub const DEFAULT_DOWNLOAD: &str = "https://github.com";
const MAX_API_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_SIGNATURE_BYTES: u64 = 4 * 1024;
const API_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long to stay away from GitHub, from the answer that refused the request. `None` when the
/// answer is not about a limit.
///
/// GitHub answers 403 (primary limit: `x-ratelimit-remaining: 0` and the reset time as epoch
/// seconds; secondary limit: `retry-after`) or 429. `retry-after` is obeyed when present.
pub fn rate_limit_wait(status: u16, headers: &HeaderMap, now_epoch: u64) -> Option<Duration> {
    if status != 403 && status != 429 {
        return None;
    }
    let number =
        |name: &str| -> Option<u64> { headers.get(name)?.to_str().ok()?.trim().parse().ok() };
    if let Some(secs) = number(RETRY_AFTER.as_str()) {
        return Some(Duration::from_secs(secs));
    }
    if number("x-ratelimit-remaining") == Some(0)
        && let Some(reset) = number("x-ratelimit-reset")
    {
        return Some(Duration::from_secs(reset.saturating_sub(now_epoch)));
    }
    (status == 429).then_some(Duration::from_secs(60))
}

#[derive(Deserialize)]
struct ReleaseDoc {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<AssetDoc>,
}

#[derive(Deserialize)]
struct AssetDoc {
    name: String,
}

/// The tag and the names of the files of a release document; `None` for a draft.
pub fn parse_release(body: &[u8]) -> Result<Option<(String, Vec<String>)>, String> {
    let doc: ReleaseDoc = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if doc.draft {
        return Ok(None);
    }
    let tag_ok = (1..=100).contains(&doc.tag_name.len())
        && doc
            .tag_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
        && !doc.tag_name.starts_with('.');
    if !tag_ok {
        return Err("the release tag is not a plain name".to_owned());
    }
    Ok(Some((
        doc.tag_name,
        doc.assets.into_iter().map(|a| a.name).collect(),
    )))
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub struct GithubSource {
    http: reqwest::Client,
    api: Url,
    download: Url,
    repo: String,
    etag: Mutex<Option<String>>,
}

impl GithubSource {
    /// `repo` is `owner/name`.
    pub fn new(api: Url, download: Url, repo: &str) -> Result<GithubSource, reqwest::Error> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mtpool-registrar/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(GithubSource {
            http,
            api,
            download,
            repo: repo.to_owned(),
            etag: Mutex::new(None),
        })
    }

    fn file_url(&self, tag: &str, name: &str) -> String {
        format!(
            "{}/{}/releases/download/{tag}/{name}",
            self.download.as_str().trim_end_matches('/'),
            self.repo
        )
    }

    async fn get_limited(
        &self,
        url: &str,
        max: u64,
        timeout: Duration,
    ) -> Result<reqwest::Response, SourceError> {
        let response = self
            .http
            .get(url)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                SourceError::Failed(format!(
                    "cannot reach the release server: {}",
                    e.without_url()
                ))
            })?;
        let status = response.status();
        if let Some(wait) = rate_limit_wait(status.as_u16(), response.headers(), now_epoch()) {
            return Err(SourceError::Limited(wait));
        }
        if !status.is_success() {
            return Err(SourceError::Failed(format!(
                "the release server answered {status}"
            )));
        }
        if response.content_length().is_some_and(|n| n > max) {
            return Err(SourceError::Failed(
                "the file is larger than allowed".to_owned(),
            ));
        }
        Ok(response)
    }

    async fn small_file(&self, tag: &str, name: &str, max: u64) -> Result<Vec<u8>, SourceError> {
        let mut response = self
            .get_limited(&self.file_url(tag, name), max, API_TIMEOUT)
            .await?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| SourceError::Failed(e.without_url().to_string()))?
        {
            if (body.len() + chunk.len()) as u64 > max {
                return Err(SourceError::Failed(format!(
                    "{name} is larger than allowed"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[async_trait]
impl ReleaseSource for GithubSource {
    async fn latest(&self) -> Result<Latest, SourceError> {
        let url = format!(
            "{}/repos/{}/releases/latest",
            self.api.as_str().trim_end_matches('/'),
            self.repo
        );
        let mut request = self
            .http
            .get(&url)
            .timeout(API_TIMEOUT)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        let known = self.etag.lock().ok().and_then(|e| e.clone());
        if let Some(etag) = &known {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let mut response = request.send().await.map_err(|e| {
            SourceError::Failed(format!(
                "cannot reach the release server: {}",
                e.without_url()
            ))
        })?;
        let status = response.status();
        if let Some(wait) = rate_limit_wait(status.as_u16(), response.headers(), now_epoch()) {
            return Err(SourceError::Limited(wait));
        }
        match status {
            StatusCode::NOT_MODIFIED => return Ok(Latest::Unchanged),
            StatusCode::NOT_FOUND => {
                if let Ok(mut etag) = self.etag.lock() {
                    *etag = None;
                }
                return Ok(Latest::None);
            }
            s if !s.is_success() => {
                return Err(SourceError::Failed(format!("the release API answered {s}")));
            }
            _ => {}
        }
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| SourceError::Failed(e.without_url().to_string()))?
        {
            if (body.len() + chunk.len()) as u64 > MAX_API_BYTES {
                return Err(SourceError::Failed(
                    "the release document is too large".to_owned(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let Some((tag, assets)) = parse_release(&body).map_err(SourceError::Failed)? else {
            return Ok(Latest::None);
        };
        // A release whose files are still being uploaded has no manifest yet: look again later.
        if !assets.iter().any(|a| a == MANIFEST_NAME) || !assets.iter().any(|a| a == SIGNATURE_NAME)
        {
            return Ok(Latest::None);
        }
        let manifest = self
            .small_file(&tag, MANIFEST_NAME, MAX_MANIFEST_BYTES)
            .await?;
        let signature = self
            .small_file(&tag, SIGNATURE_NAME, MAX_SIGNATURE_BYTES)
            .await?;
        let signature = String::from_utf8(signature)
            .map_err(|_| SourceError::Failed("the signature file is not text".to_owned()))?;
        // Only now is the answer fully used: a failure above is asked again, not skipped as
        // "unchanged".
        if let Ok(mut slot) = self.etag.lock() {
            *slot = etag;
        }
        Ok(Latest::Release(Offer {
            tag,
            manifest,
            signature,
            assets,
        }))
    }

    async fn download(
        &self,
        tag: &str,
        name: &str,
        dest: &std::path::Path,
        max: u64,
    ) -> Result<(), SourceError> {
        let mut response = self
            .get_limited(&self.file_url(tag, name), max, DOWNLOAD_TIMEOUT)
            .await?;
        let io = |e: std::io::Error| {
            SourceError::Failed(format!("cannot write {}: {e}", dest.display()))
        };
        let mut file = tokio::fs::File::create(dest).await.map_err(io)?;
        let mut written = 0u64;
        let result: Result<(), SourceError> = async {
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| SourceError::Failed(e.without_url().to_string()))?
            {
                written += chunk.len() as u64;
                if written > max {
                    return Err(SourceError::Failed(format!(
                        "{name} is larger than allowed"
                    )));
                }
                file.write_all(&chunk).await.map_err(io)?;
            }
            file.flush().await.map_err(io)?;
            file.sync_all().await.map_err(io)
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(dest).await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    const RATE_LIMITED_HEADERS: &str =
        include_str!("../../tests/fixtures/github/rate_limited.headers.txt");

    fn headers(text: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        for line in text.lines().skip(1) {
            let (name, value) = line.split_once(':').unwrap();
            map.insert(
                HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes()).unwrap(),
                HeaderValue::from_str(value.trim()).unwrap(),
            );
        }
        map
    }

    #[test]
    fn recorded_rate_limit_answer_gives_the_wait() {
        let h = headers(RATE_LIMITED_HEADERS);
        // x-ratelimit-reset in the recording is 1793758723.
        let wait = rate_limit_wait(403, &h, 1_793_758_723 - 600);
        assert_eq!(wait, Some(Duration::from_secs(600)));
        // Past the reset: a minimal pause, not a negative one.
        assert_eq!(
            rate_limit_wait(403, &h, 1_793_758_723 + 5),
            Some(Duration::ZERO)
        );
        // The same status with requests left is some other refusal, not a limit.
        let mut h2 = h.clone();
        h2.insert("x-ratelimit-remaining", HeaderValue::from_static("12"));
        assert_eq!(rate_limit_wait(403, &h2, 0), None);
        assert_eq!(rate_limit_wait(200, &h, 0), None);
    }

    #[test]
    fn retry_after_wins_on_429_and_secondary_limits() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("90"));
        assert_eq!(rate_limit_wait(429, &h, 0), Some(Duration::from_secs(90)));
        assert_eq!(rate_limit_wait(403, &h, 0), Some(Duration::from_secs(90)));
        // 429 without any hint still backs off.
        assert_eq!(
            rate_limit_wait(429, &HeaderMap::new(), 0),
            Some(Duration::from_secs(60))
        );
    }

    const RELEASE_JSON: &str = include_str!("../../tests/fixtures/github/release_latest.json");
    const RATE_LIMITED_JSON: &str = include_str!("../../tests/fixtures/github/rate_limited.json");

    #[test]
    fn recorded_release_document_parses() {
        let (tag, assets) = parse_release(RELEASE_JSON.as_bytes()).unwrap().unwrap();
        assert_eq!(tag, "0.12");
        assert_eq!(assets.len(), 10);
        assert!(assets.iter().any(|a| a == "minisign-0.12-linux.tar.gz"));
        // The rate-limit answer is not a release.
        assert!(parse_release(RATE_LIMITED_JSON.as_bytes()).is_err());
        // A draft is no release; a tag that could climb out of a URL is refused.
        assert_eq!(
            parse_release(br#"{"tag_name":"1.0.0","draft":true}"#),
            Ok(None)
        );
        assert!(parse_release(br#"{"tag_name":"../x","assets":[]}"#).is_err());
    }

    /// The recorded document with the assets of a registrar release.
    fn release_doc(extra: &[&str]) -> String {
        let mut doc: serde_json::Value = serde_json::from_str(RELEASE_JSON).unwrap();
        doc["tag_name"] = "9.9.9".into();
        doc["assets"] = extra
            .iter()
            .map(|n| serde_json::json!({"name": n}))
            .collect();
        doc.to_string()
    }

    async fn source(server: &wiremock::MockServer) -> GithubSource {
        let base = Url::parse(&server.uri()).unwrap();
        GithubSource::new(base.clone(), base.join("dl").unwrap(), "owner/repo").unwrap()
    }

    #[tokio::test]
    async fn latest_uses_the_etag_and_fetches_the_manifest_files() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let doc = release_doc(&[
            "manifest.json",
            "manifest.json.minisig",
            "mtpool-registrar-x",
        ]);
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/releases/latest"))
            .and(header("if-none-match", "W/\"abc\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/releases/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "W/\"abc\"")
                    .set_body_string(doc),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/owner/repo/releases/download/9.9.9/manifest.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/dl/owner/repo/releases/download/9.9.9/manifest.json.minisig",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("sig"))
            .mount(&server)
            .await;
        let source = source(&server).await;
        match source.latest().await.unwrap() {
            Latest::Release(offer) => {
                assert_eq!(offer.tag, "9.9.9");
                assert_eq!(offer.manifest, b"{}");
                assert_eq!(offer.signature, "sig");
                assert!(offer.assets.contains(&"mtpool-registrar-x".to_owned()));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(source.latest().await.unwrap(), Latest::Unchanged);
    }

    #[tokio::test]
    async fn no_release_unfinished_release_and_limit() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let source = source(&server).await;
        // 404: the repository has no release.
        assert_eq!(source.latest().await.unwrap(), Latest::None);
        server.reset().await;
        // Files still being uploaded: nothing yet.
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/releases/latest"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(release_doc(&["mtpool-registrar-x"])),
            )
            .mount(&server)
            .await;
        assert_eq!(source.latest().await.unwrap(), Latest::None);
        server.reset().await;
        // The limit is used up.
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/releases/latest"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", (now_epoch() + 900).to_string())
                    .set_body_string(RATE_LIMITED_JSON),
            )
            .mount(&server)
            .await;
        match source.latest().await {
            Err(SourceError::Limited(wait)) => {
                assert!((890..=900).contains(&wait.as_secs()), "{wait:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn download_stops_at_the_size_limit_and_leaves_no_file() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/dl/owner/repo/releases/download/9.9.9/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 4096]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/owner/repo/releases/download/9.9.9/small"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![7u8; 100]))
            .mount(&server)
            .await;
        let source = source(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        assert!(source.download("9.9.9", "big", &big, 1000).await.is_err());
        assert!(!big.exists());
        let small = dir.path().join("small");
        source
            .download("9.9.9", "small", &small, 1000)
            .await
            .unwrap();
        assert_eq!(std::fs::read(small).unwrap().len(), 100);
    }
}
