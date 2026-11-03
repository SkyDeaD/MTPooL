//! `GET /install.sh` (the node installer with the pool's values written in) and
//! `GET /releases/{name}` (what the installer downloads).

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::*;
use mtpool_core::{PoolState, Settings, Ts};
use mtpool_protocol::problem::Problem;
use mtpool_registrar::actor::{Actor, ActorOptions, Clock};
use mtpool_registrar::server::{AppState, router};
use tower::ServiceExt;

const NOW: i64 = 1_700_000_000_000;
/// The example key of the minisign documentation.
const KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";

struct Env {
    app: Router,
    releases: tempfile::TempDir,
    _db: tempfile::TempDir,
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).expect("utf-8 body")
    }

    fn header(&self, name: header::HeaderName) -> String {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    }
}

fn env_with(key: &'static str, tweak: impl FnOnce(&mut Settings)) -> Env {
    let (db, store) = open_store();
    let releases = tempfile::tempdir().expect("releases dir");
    let mut settings = Settings {
        proxy_port: 8443,
        tls_domain: "tls.example.com".into(),
        ..Settings::default()
    };
    tweak(&mut settings);
    let clock = Arc::new(AtomicI64::new(NOW));
    let clock_fn: Clock = Arc::new(move || Ts(clock.load(Ordering::SeqCst)));
    let actor = Actor::spawn_with(
        PoolState::new(settings),
        store.clone(),
        vec![],
        ActorOptions {
            clock: Arc::clone(&clock_fn),
            ..ActorOptions::default()
        },
    );
    let state = AppState::with_clock(store, actor.clone(), actor.snapshots(), clock_fn)
        .with_releases_dir(releases.path().to_path_buf())
        .with_minisign_pub(key);
    Env {
        app: router(state),
        releases,
        _db: db,
    }
}

fn env() -> Env {
    env_with(KEY, |_| {})
}

async fn send(env: &Env, request: Request<Body>) -> Resp {
    let response = env.app.clone().oneshot(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.expect("body");
    Resp {
        status: parts.status,
        headers: parts.headers,
        body: bytes.to_vec(),
    }
}

/// A GET from `peer` with the given headers.
fn get(path: &str, peer: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder.body(Body::empty()).unwrap();
    let addr: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

fn problem(resp: &Resp) -> Problem {
    assert!(
        resp.header(header::CONTENT_TYPE)
            .starts_with("application/problem+json"),
        "{}",
        resp.text()
    );
    serde_json::from_slice(&resp.body).expect("a problem document")
}

#[tokio::test]
async fn install_script_has_the_pool_values_written_in() {
    let env = env();
    let resp = send(
        &env,
        get(
            "/install.sh",
            "127.0.0.1:5000",
            &[
                ("host", "pool.example.test"),
                ("x-forwarded-proto", "https"),
            ],
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.header(header::CONTENT_TYPE),
        "text/plain; charset=utf-8"
    );
    let script = resp.text();
    assert!(
        script.contains("POOL_URL='https://pool.example.test'"),
        "{script}"
    );
    assert!(script.contains(&format!("PUBKEY='{KEY}'")));
    assert!(script.contains("POOL_PORT='8443'"));
    assert!(script.contains("POOL_TLS_DOMAIN='tls.example.com'"));
    // The units and the engine's files come from the same files the installer of the pool
    // itself uses.
    assert!(script.contains("ExecStart=/usr/local/bin/mtpool-agent"));
    assert!(script.contains("ExecStart=/usr/local/bin/telemt"));
    assert!(script.contains("[general.modes]"));
    assert!(
        !script.contains("__MTPOOL_"),
        "a marker is left in the script"
    );
    assert!(script.starts_with("#!/bin/sh"));
}

#[tokio::test]
async fn install_script_trusts_forwarded_proto_only_from_the_local_proxy() {
    let env = env();
    let headers = [
        ("host", "pool.example.test"),
        ("x-forwarded-proto", "https"),
    ];
    let resp = send(&env, get("/install.sh", "203.0.113.9:5000", &headers)).await;
    assert!(
        resp.text().contains("POOL_URL='http://pool.example.test'"),
        "a stranger's header must not pick the scheme"
    );
    // The proxy without the header: the request's own scheme.
    let resp = send(
        &env,
        get(
            "/install.sh",
            "127.0.0.1:5000",
            &[("host", "pool.example.test")],
        ),
    )
    .await;
    assert!(resp.text().contains("POOL_URL='http://pool.example.test'"));
    // An unknown scheme from the proxy is not written into a script.
    let resp = send(
        &env,
        get(
            "/install.sh",
            "127.0.0.1:5000",
            &[("host", "pool.example.test"), ("x-forwarded-proto", "ftp")],
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn install_script_keeps_a_port_in_the_host() {
    let env = env();
    let resp = send(
        &env,
        get(
            "/install.sh",
            "127.0.0.1:5000",
            &[("host", "pool.example.test:8080")],
        ),
    )
    .await;
    assert!(
        resp.text()
            .contains("POOL_URL='http://pool.example.test:8080'")
    );
}

#[tokio::test]
async fn install_script_refuses_a_host_that_could_break_out_of_the_quotes() {
    let env = env();
    for host in [
        "a'; rm -rf /; '",
        "pool example",
        "pool.example.test/x",
        "$(id)",
        "a`id`",
    ] {
        let resp = send(
            &env,
            get("/install.sh", "127.0.0.1:5000", &[("host", host)]),
        )
        .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{host}");
        assert_eq!(problem(&resp).code, "bad_request", "{host}");
    }
    // No Host header at all.
    let resp = send(&env, get("/install.sh", "127.0.0.1:5000", &[])).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_tls_domain_that_is_not_a_hostname_is_not_written_into_the_script() {
    let env = env_with(KEY, |s| s.tls_domain = "x'; id; '".into());
    let resp = send(
        &env,
        get(
            "/install.sh",
            "127.0.0.1:5000",
            &[("host", "pool.example.test")],
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn install_script_without_a_signing_key_is_unavailable() {
    let env = env_with("", |_| {});
    let resp = send(
        &env,
        get(
            "/install.sh",
            "127.0.0.1:5000",
            &[("host", "pool.example.test")],
        ),
    )
    .await;
    assert_eq!(resp.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(problem(&resp).code, "not_configured");
}

#[tokio::test]
async fn install_script_is_public_and_limited_like_the_public_api() {
    let env = env();
    let mut last = StatusCode::OK;
    for _ in 0..121 {
        last = send(
            &env,
            get(
                "/install.sh",
                "198.51.100.4:5000",
                &[("host", "pool.example.test")],
            ),
        )
        .await
        .status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn a_release_file_is_served_without_caching() {
    let env = env();
    std::fs::write(
        env.releases.path().join("manifest.json"),
        b"{\"version\":\"1\"}",
    )
    .unwrap();
    std::fs::write(
        env.releases.path().join("mtpool-agent-x86_64"),
        [0u8, 1, 2, 255],
    )
    .unwrap();

    let resp = send(&env, get("/releases/manifest.json", "198.51.100.4:1", &[])).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.header(header::CACHE_CONTROL), "no-cache");
    assert_eq!(resp.header(header::CONTENT_TYPE), "application/json");
    assert_eq!(resp.body, b"{\"version\":\"1\"}");

    let resp = send(
        &env,
        get("/releases/mtpool-agent-x86_64", "198.51.100.4:1", &[]),
    )
    .await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.header(header::CONTENT_TYPE),
        "application/octet-stream"
    );
    assert_eq!(resp.body, [0u8, 1, 2, 255]);
}

#[tokio::test]
async fn a_missing_release_file_is_a_not_found_problem() {
    let env = env();
    let resp = send(&env, get("/releases/nothing.json", "198.51.100.4:1", &[])).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    assert_eq!(problem(&resp).code, "not_found");
}

#[tokio::test]
async fn release_names_cannot_leave_the_directory() {
    let env = env();
    std::fs::write(env.releases.path().join("..secret"), b"x").unwrap();
    std::fs::write(
        env.releases.path().parent().unwrap().join("outside.txt"),
        b"x",
    )
    .ok();
    for name in [
        "..",
        ".",
        "...",
        "%2e%2e",
        "..%2foutside.txt",
        "a%2fb",
        "a%00b",
        "has%20space",
    ] {
        let resp = send(
            &env,
            get(&format!("/releases/{name}"), "198.51.100.4:1", &[]),
        )
        .await;
        assert_eq!(resp.status, StatusCode::NOT_FOUND, "{name}");
    }
    // A name of 121 characters is over the limit even when the file exists.
    let long = "a".repeat(121);
    std::fs::write(env.releases.path().join(&long), b"x").unwrap();
    let resp = send(
        &env,
        get(&format!("/releases/{long}"), "198.51.100.4:1", &[]),
    )
    .await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
    // A directory is not a file.
    std::fs::create_dir(env.releases.path().join("sub")).unwrap();
    let resp = send(&env, get("/releases/sub", "198.51.100.4:1", &[])).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_symlink_in_the_releases_directory_is_not_followed() {
    let env = env();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), b"secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), env.releases.path().join("link")).unwrap();
    let resp = send(&env, get("/releases/link", "198.51.100.4:1", &[])).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}
