//! The built web interface, embedded in the binary and served from memory.
//!
//! `web/dist` holds prerendered public pages at the root and the single page app of the panel
//! under `/panel`. Every file may come with a `.br` and a `.gz` twin made at build time; the
//! twin is chosen by `Accept-Encoding`. A build without a frontend embeds nothing and every
//! page answers 404.

use std::borrow::Cow;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::api;

/// One file of the interface.
pub struct Asset {
    pub data: Cow<'static, [u8]>,
    /// SHA-256 of `data`; the entity tag is made of it.
    pub hash: [u8; 32],
}

/// Where the files come from. The binary uses [`Embedded`]; tests use a small set of their own
/// so that they do not depend on a frontend build.
pub trait AssetSource: Send + Sync {
    fn get(&self, path: &str) -> Option<Asset>;
}

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/web"]
struct Files;

/// The files built into this binary.
pub struct Embedded;

impl AssetSource for Embedded {
    fn get(&self, path: &str) -> Option<Asset> {
        let file = Files::get(path)?;
        Some(Asset {
            hash: file.metadata.sha256_hash(),
            data: file.data,
        })
    }
}

/// Router fallback: the embedded interface for GET and HEAD, the usual JSON 404 for the rest.
pub async fn fallback(request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return not_found();
    }
    let headers = request.headers();
    let text = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let mut response = serve_asset(
        &Embedded,
        request.uri().path(),
        text(header::ACCEPT_ENCODING),
        text(header::IF_NONE_MATCH),
    );
    if request.method() == Method::HEAD {
        *response.body_mut() = Body::empty();
    }
    response
}

/// Answers a request for `path` from `source`: the file, a 304 when `if_none_match` still
/// names it, or the JSON 404 of the API when there is nothing to serve.
pub fn serve_asset(
    source: &dyn AssetSource,
    path: &str,
    accept_encoding: Option<&str>,
    if_none_match: Option<&str>,
) -> Response {
    let Some(name) = resolve(source, path) else {
        return not_found();
    };
    let accepted = Accepted::parse(accept_encoding.unwrap_or(""));
    let twin = |suffix: &str, coding: &'static str| {
        if !accepted.allows(coding) {
            return None;
        }
        source
            .get(&format!("{name}{suffix}"))
            .map(|a| (a, Some(coding)))
    };
    let Some((asset, coding)) = twin(".br", "br")
        .or_else(|| twin(".gz", "gzip"))
        .or_else(|| source.get(&name).map(|a| (a, None)))
    else {
        return not_found();
    };

    let etag = format!("\"{}\"", hex(&asset.hash));
    let cache = if is_hashed(&name) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = if if_none_match.is_some_and(|sent| etag_matches(sent, &etag)) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let mut ok = Response::new(Body::from(asset.data.into_owned()));
        let headers = ok.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(&name)),
        );
        if let Some(coding) = coding {
            headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(coding));
        }
        ok
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("hex in quotes"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn not_found() -> Response {
    api::ApiError::not_found("There is nothing at this path.").into_response()
}

/// The name of the file a request path stands for, without any encoding suffix.
fn resolve(source: &dyn AssetSource, path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let dir_like = rest.is_empty() || rest.ends_with('/');
    let trimmed = rest.trim_end_matches('/');
    let segments: Vec<&str> = if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split('/').collect()
    };
    let bad = |s: &&str| s.is_empty() || *s == "." || *s == ".." || s.contains(['\\', '\0']);
    if segments.iter().any(bad) {
        return None;
    }
    // The compressed twins are an implementation detail, not addresses.
    if segments
        .last()
        .is_some_and(|s| s.ends_with(".br") || s.ends_with(".gz"))
    {
        return None;
    }
    let base = segments.join("/");
    let index = if base.is_empty() {
        "index.html".to_string()
    } else {
        format!("{base}/index.html")
    };
    let wanted = if dir_like {
        vec![index]
    } else {
        vec![base, index]
    };
    if let Some(hit) = wanted.into_iter().find(|name| source.get(name).is_some()) {
        return Some(hit);
    }
    // The panel is one page that draws its own routes. A path with an extension is a file that
    // is missing and must not be answered with HTML.
    let in_panel = segments.first() == Some(&"panel");
    let names_a_file = segments.last().is_some_and(|s| s.contains('.'));
    if in_panel && !names_a_file {
        let entry = "panel/index.html";
        return source.get(entry).map(|_| entry.to_string());
    }
    None
}

/// Files under `assets/` end in `-` and a content hash of eight characters of the URL-safe base64
/// alphabet, like `app-AbCd1234.js`. The hash itself may hold a dash.
fn is_hashed(name: &str) -> bool {
    let mut parts = name.rsplit('/');
    let file = parts.next().unwrap_or("");
    if !parts.any(|dir| dir == "assets") {
        return false;
    }
    let stem = file
        .rsplit_once('.')
        .map_or(file, |(stem, _)| stem)
        .as_bytes();
    let Some(dash) = stem.len().checked_sub(9) else {
        return false;
    };
    stem[dash] == b'-'
        && stem[dash + 1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map_or("", |(_, ext)| ext) {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "txt" => "text/plain; charset=utf-8",
        "woff2" => "font/woff2",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// The codings an `Accept-Encoding` header allows.
struct Accepted(Vec<(String, f32)>);

impl Accepted {
    fn parse(header: &str) -> Accepted {
        Accepted(
            header
                .split(',')
                .filter_map(|item| {
                    let mut parts = item.split(';');
                    let coding = parts.next()?.trim().to_ascii_lowercase();
                    if coding.is_empty() {
                        return None;
                    }
                    let q = parts
                        .find_map(|p| p.trim().strip_prefix("q=").map(str::trim))
                        .and_then(|q| q.parse::<f32>().ok())
                        .unwrap_or(1.0);
                    Some((coding, q))
                })
                .collect(),
        )
    }

    fn allows(&self, coding: &str) -> bool {
        let q = |name: &str| self.0.iter().find(|(c, _)| c == name).map(|(_, q)| *q);
        q(coding).or_else(|| q("*")).is_some_and(|q| q > 0.0)
    }
}

/// Whether an `If-None-Match` value names `etag`; weak and strong forms compare alike here.
fn etag_matches(sent: &str, etag: &str) -> bool {
    sent.split(',').any(|item| {
        let item = item.trim();
        item == "*" || item.strip_prefix("W/").unwrap_or(item) == etag
    })
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use http_body_util::BodyExt;
    use sha2::{Digest, Sha256};

    struct Fixture(HashMap<&'static str, Vec<u8>>);

    impl Fixture {
        fn new() -> Fixture {
            let mut files: HashMap<&'static str, Vec<u8>> = HashMap::new();
            files.insert("index.html", b"<p>public</p>".to_vec());
            files.insert("index.html.br", b"br:public".to_vec());
            files.insert("index.html.gz", b"gz:public".to_vec());
            files.insert("statistics/index.html", b"<p>statistics</p>".to_vec());
            files.insert("panel/index.html", b"<p>panel</p>".to_vec());
            files.insert("panel/index.html.br", b"br:panel".to_vec());
            files.insert("assets/app-AbCd1234.js", b"console.log(1)".to_vec());
            files.insert("assets/app-AbCd1234.js.br", b"br:js".to_vec());
            files.insert("assets/app-AbCd1234.js.gz", b"gz:js".to_vec());
            files.insert("robots.txt", b"User-agent: *".to_vec());
            Fixture(files)
        }
    }

    impl AssetSource for Fixture {
        fn get(&self, path: &str) -> Option<Asset> {
            let data = self.0.get(path)?.clone();
            Some(Asset {
                hash: Sha256::digest(&data).into(),
                data: Cow::Owned(data),
            })
        }
    }

    async fn body(response: Response) -> Vec<u8> {
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec()
    }

    fn header(response: &Response, name: header::HeaderName) -> &str {
        response.headers().get(name).unwrap().to_str().unwrap()
    }

    fn get(path: &str, accept: Option<&str>) -> Response {
        serve_asset(&Fixture::new(), path, accept, None)
    }

    #[tokio::test]
    async fn br_chosen_when_accepted() {
        let response = get("/assets/app-AbCd1234.js", Some("gzip, deflate, br"));
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header(&response, header::CONTENT_ENCODING), "br");
        assert_eq!(header(&response, header::VARY), "Accept-Encoding");
        assert_eq!(
            header(&response, header::CONTENT_TYPE),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(body(response).await, b"br:js");

        let response = get("/assets/app-AbCd1234.js", Some("gzip"));
        assert_eq!(header(&response, header::CONTENT_ENCODING), "gzip");
        assert_eq!(body(response).await, b"gz:js");

        // br refused with q=0, so gzip wins; nothing accepted gives the plain file.
        let response = get("/assets/app-AbCd1234.js", Some("br;q=0, gzip;q=0.5"));
        assert_eq!(header(&response, header::CONTENT_ENCODING), "gzip");
        let response = get("/assets/app-AbCd1234.js", None);
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(body(response).await, b"console.log(1)");
    }

    #[tokio::test]
    async fn plain_file_when_no_twin_exists() {
        let response = get("/robots.txt", Some("br, gzip"));
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(body(response).await, b"User-agent: *");
    }

    #[test]
    fn hashed_assets_immutable() {
        let response = get("/assets/app-AbCd1234.js", None);
        assert_eq!(
            header(&response, header::CACHE_CONTROL),
            "public, max-age=31536000, immutable"
        );
        // The hash alphabet has a dash in it, so the last dash is not where the hash begins.
        assert!(is_hashed("assets/Geist-BTx-PElr.woff2"));
        assert!(!is_hashed("assets/app.js"));
        assert!(!is_hashed("robots-AbCd1234.txt"));
        // A name without a hash is not immutable.
        let response = get("/robots.txt", None);
        assert_eq!(header(&response, header::CACHE_CONTROL), "no-cache");
    }

    #[test]
    fn html_no_cache() {
        for path in ["/", "/statistics", "/statistics/", "/panel", "/panel/nodes"] {
            let response = get(path, None);
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                header(&response, header::CACHE_CONTROL),
                "no-cache",
                "{path}"
            );
            assert_eq!(
                header(&response, header::CONTENT_TYPE),
                "text/html; charset=utf-8",
                "{path}"
            );
        }
    }

    #[test]
    fn etag_304() {
        let first = get("/assets/app-AbCd1234.js", Some("br"));
        let etag = header(&first, header::ETAG).to_string();
        assert!(etag.starts_with('"') && etag.ends_with('"'));
        // Each encoding is its own representation.
        let plain = get("/assets/app-AbCd1234.js", None);
        assert_ne!(header(&plain, header::ETAG), etag);

        let fixture = Fixture::new();
        for sent in [
            etag.clone(),
            format!("W/{etag}"),
            format!("\"x\", {etag}"),
            "*".into(),
        ] {
            let response =
                serve_asset(&fixture, "/assets/app-AbCd1234.js", Some("br"), Some(&sent));
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{sent}");
            assert_eq!(header(&response, header::ETAG), etag);
            assert!(response.headers().get(header::CACHE_CONTROL).is_some());
        }
        let response = serve_asset(
            &fixture,
            "/assets/app-AbCd1234.js",
            Some("br"),
            Some("\"other\""),
        );
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn spa_fallback_only_under_panel() {
        let response = get("/panel/nodes/42", None);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await, b"<p>panel</p>");

        // Outside the panel an unknown path is the API's JSON 404, not an index page.
        for path in ["/nodes/42", "/unknown", "/statistics/unknown", "/panelx"] {
            let response = get(path, None);
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                header(&response, header::CONTENT_TYPE),
                "application/problem+json",
                "{path}"
            );
        }
        // A missing file under the panel is a 404 as well: it must not answer with HTML.
        let response = get("/panel/assets/missing-ZzZz9999.js", None);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn unsafe_paths_and_encoded_twins_are_not_served() {
        for path in [
            "/../Cargo.toml",
            "/assets/../index.html",
            "//index.html",
            "/index.html.br",
            "/assets/app-AbCd1234.js.gz",
            "/assets\\app.js",
        ] {
            assert_eq!(get(path, None).status(), StatusCode::NOT_FOUND, "{path}");
        }
    }

    #[test]
    fn nothing_embedded_means_404() {
        let empty = Fixture(HashMap::new());
        for path in ["/", "/panel", "/panel/x"] {
            let response = serve_asset(&empty, path, None, None);
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
    }
}
