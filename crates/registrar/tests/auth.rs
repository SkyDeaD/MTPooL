mod common;

use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, Response, StatusCode, header};
use axum::routing::get;
use common::*;
use data_encoding::BASE64URL_NOPAD;
use mtpool_core::{NodeId, OwnerId, PoolState, Ts};
use mtpool_registrar::actor::Actor;
use mtpool_registrar::auth::middleware::{
    AdminAuth, BotAuth, EnrollAuth, MetricsAuth, NodeAuth, client_ip,
};
use mtpool_registrar::auth::session;
use mtpool_registrar::auth::tokens::{
    self, NewToken, TokenHash, TokenKind, new_token, rotate_admin,
};
use mtpool_registrar::server::{AppState, router};
use mtpool_registrar::store::Store;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn app_state(store: &Store) -> AppState {
    let actor = Actor::spawn(PoolState::default(), store.clone(), vec![]);
    AppState::new(store.clone(), actor.clone(), actor.snapshots())
}

/// The auth extractors behind real URLs, plus the real session routes.
fn test_app(state: AppState) -> Router {
    Router::new()
        .route(
            "/t/admin",
            get(|_: AdminAuth| async { "ok" }).post(|_: AdminAuth| async { "ok" }),
        )
        .route("/t/bot", get(|_: BotAuth| async { "ok" }))
        .route(
            "/t/node",
            get(|n: NodeAuth| async move { n.node_id.as_str().to_owned() }),
        )
        .route(
            "/t/enroll",
            get(|e: EnrollAuth| async move { e.owner_id.0.to_string() }),
        )
        .route("/t/metrics", get(|_: MetricsAuth| async { "ok" }))
        .merge(session::routes())
        .with_state(state)
}

fn request(
    method: &str,
    uri: &str,
    peer: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let mut req = builder
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_owned())))
        .unwrap();
    let addr = SocketAddr::new(peer.parse().unwrap(), 40000);
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

fn bearer(token: &str) -> (&'static str, String) {
    ("authorization", format!("Bearer {token}"))
}

async fn send(app: &Router, req: Request<Body>) -> Response<Body> {
    app.clone().oneshot(req).await.expect("response")
}

async fn get_with(app: &Router, path: &str, peer: &str, token: &str) -> Response<Body> {
    let (k, v) = bearer(token);
    send(app, request("GET", path, peer, &[(k, v.as_str())], None)).await
}

async fn body_string(resp: Response<Body>) -> String {
    let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn owner(store: &Store, name: &str) -> OwnerId {
    tokens::create_owner(store, name, 10, "").expect("owner")
}

fn issue(store: &Store, kind: TokenKind, owner: Option<OwnerId>, node: Option<&str>) -> String {
    tokens::issue(
        store,
        NewToken {
            kind,
            owner,
            node: node.map(node_id),
            expires_at: None,
            scope: String::new(),
        },
    )
    .expect("issue token")
}

fn sha256(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// Logs in with `token`; returns (cookie value, csrf, the Set-Cookie header).
async fn login(app: &Router, peer: &str, token: &str) -> (String, String, String) {
    let body = format!(r#"{{"token":"{token}"}}"#);
    let resp = send(
        app,
        request(
            "POST",
            "/api/v1/admin/session",
            peer,
            &[("content-type", "application/json")],
            Some(&body),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .expect("Set-Cookie")
        .to_str()
        .unwrap()
        .to_owned();
    let value = set_cookie
        .strip_prefix("mtpool_session=")
        .expect("cookie name")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
    let csrf = json["csrf"].as_str().expect("csrf in body").to_owned();
    (value, csrf, set_cookie)
}

fn cookie_header(value: &str) -> String {
    format!("mtpool_session={value}")
}

// ---- tokens ----

#[test]
fn token_format_has_prefix_and_32_random_bytes() {
    for (kind, name) in [
        (TokenKind::Admin, "admin"),
        (TokenKind::Enroll, "enroll"),
        (TokenKind::Node, "node"),
        (TokenKind::Bot, "bot"),
        (TokenKind::Metrics, "metrics"),
    ] {
        let (token, hash) = new_token(kind);
        let random = token
            .strip_prefix(&format!("mtp_{name}_"))
            .unwrap_or_else(|| panic!("{token} has no mtp_{name}_ prefix"));
        let bytes = BASE64URL_NOPAD
            .decode(random.as_bytes())
            .expect("base64url");
        assert_eq!(bytes.len(), 32);
        assert_eq!(hash.as_bytes(), &sha256(&token));
        assert_eq!(tokens::parse_kind(&token), Some(kind));
        assert_ne!(new_token(kind).0, token);
    }
    assert_eq!(tokens::parse_kind("mtp_root_abc"), None);
    assert_eq!(tokens::parse_kind("admin_abc"), None);
    assert_eq!(tokens::parse_kind(""), None);
}

#[test]
fn token_stored_only_as_sha256() {
    let (_dir, store) = open_store();
    let o = owner(&store, "alice");
    let token = issue(&store, TokenKind::Bot, Some(o), None);
    let random = token.strip_prefix("mtp_bot_").unwrap();

    let conn = store.read();
    let (hash, prefix): (Vec<u8>, String) = conn
        .query_row("SELECT hash, prefix FROM tokens", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(hash, sha256(&token).to_vec());
    assert_eq!(prefix, token[..12]);

    // The token appears in no column of any auth table.
    for table in ["tokens", "sessions", "owners"] {
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let cols = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for i in 0..cols {
                let v: rusqlite::types::Value = row.get(i).unwrap();
                let rendered = match v {
                    rusqlite::types::Value::Text(t) => t.into_bytes(),
                    rusqlite::types::Value::Blob(b) => b,
                    _ => continue,
                };
                assert!(
                    !String::from_utf8_lossy(&rendered).contains(random),
                    "token leaked into {table}"
                );
            }
        }
    }
}

#[tokio::test]
async fn each_kind_works_on_its_own_route() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let o = owner(&store, "alice");
    let admin = rotate_admin(&store).unwrap();
    let enroll = issue(&store, TokenKind::Enroll, Some(o), None);
    let node = issue(&store, TokenKind::Node, None, Some("node-abc12"));
    let bot = issue(&store, TokenKind::Bot, Some(o), None);
    let metrics = issue(&store, TokenKind::Metrics, None, None);

    for (path, token) in [
        ("/t/admin", &admin),
        ("/t/bot", &bot),
        ("/t/node", &node),
        ("/t/enroll", &enroll),
        ("/t/metrics", &metrics),
    ] {
        let resp = get_with(&app, path, "10.0.0.1", token).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
    }
    // The scope of a token reaches the extractor.
    let resp = get_with(&app, "/t/node", "10.0.0.1", &node).await;
    assert_eq!(body_string(resp).await, "node-abc12");
    let resp = get_with(&app, "/t/enroll", "10.0.0.1", &enroll).await;
    assert_eq!(body_string(resp).await, o.0.to_string());
    assert_eq!(NodeId::parse("node-abc12").unwrap().as_str(), "node-abc12");
}

#[tokio::test]
async fn wrong_kind_rejected() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let o = owner(&store, "alice");
    let all = [
        (TokenKind::Admin, rotate_admin(&store).unwrap()),
        (
            TokenKind::Enroll,
            issue(&store, TokenKind::Enroll, Some(o), None),
        ),
        (
            TokenKind::Node,
            issue(&store, TokenKind::Node, None, Some("node-abc12")),
        ),
        (TokenKind::Bot, issue(&store, TokenKind::Bot, Some(o), None)),
        (
            TokenKind::Metrics,
            issue(&store, TokenKind::Metrics, None, None),
        ),
    ];
    let routes = [
        ("/t/admin", TokenKind::Admin),
        ("/t/enroll", TokenKind::Enroll),
        ("/t/node", TokenKind::Node),
        ("/t/bot", TokenKind::Bot),
        ("/t/metrics", TokenKind::Metrics),
    ];
    for (path, allowed) in routes {
        for (kind, token) in &all {
            if *kind == allowed {
                continue;
            }
            let resp = get_with(&app, path, "10.0.0.1", token).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{kind:?} on {path}");
            assert_eq!(
                resp.headers()[header::CONTENT_TYPE],
                "application/problem+json"
            );
            let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
            assert_eq!(json["code"], "forbidden");
            assert_eq!(json["status"], 403);
        }
    }
    // A wrong kind is not a guess: it does not count towards the attempt limit.
    for _ in 0..15 {
        let resp = get_with(&app, "/t/bot", "10.0.0.1", &all[4].1).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test]
async fn missing_and_garbage_credentials_are_401() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let resp = send(&app, request("GET", "/t/admin", "10.0.0.1", &[], None)).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
    assert_eq!(json["code"], "unauthorized");
    for bad in [
        "Bearer",
        "Bearer ",
        "Basic abc",
        "Bearer nonsense",
        "Bearer mtp_admin_x",
    ] {
        let resp = send(
            &app,
            request(
                "GET",
                "/t/admin",
                "10.0.0.1",
                &[("authorization", bad)],
                None,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{bad}");
    }
}

#[tokio::test]
async fn expired_and_revoked_rejected() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let o = owner(&store, "alice");

    let future = tokens::issue(
        &store,
        NewToken {
            kind: TokenKind::Enroll,
            owner: Some(o),
            node: None,
            expires_at: Some(Ts(tokens::now().0 + 60_000)),
            scope: String::new(),
        },
    )
    .unwrap();
    let past = tokens::issue(
        &store,
        NewToken {
            kind: TokenKind::Enroll,
            owner: Some(o),
            node: None,
            expires_at: Some(Ts(tokens::now().0 - 1)),
            scope: String::new(),
        },
    )
    .unwrap();
    let revoked = issue(&store, TokenKind::Enroll, Some(o), None);
    let id = tokens::lookup(&store.read(), &TokenHash::of(&revoked))
        .unwrap()
        .unwrap()
        .id;
    tokens::revoke(&store, id, tokens::now()).unwrap();

    assert_eq!(
        get_with(&app, "/t/enroll", "10.0.0.1", &future)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        get_with(&app, "/t/enroll", "10.0.0.1", &past)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_with(&app, "/t/enroll", "10.0.0.1", &revoked)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn last_use_is_recorded() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let metrics = issue(&store, TokenKind::Metrics, None, None);
    let hash = TokenHash::of(&metrics);
    assert!(
        tokens::lookup(&store.read(), &hash)
            .unwrap()
            .unwrap()
            .last_used_at
            .is_none()
    );
    assert_eq!(
        get_with(&app, "/t/metrics", "10.0.0.1", &metrics)
            .await
            .status(),
        StatusCode::OK
    );
    store.flush().unwrap();
    assert!(
        tokens::lookup(&store.read(), &hash)
            .unwrap()
            .unwrap()
            .last_used_at
            .is_some()
    );
}

// ---- attempt limit and client address ----

#[tokio::test]
async fn failed_attempts_limited_per_ip_429() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let metrics = issue(&store, TokenKind::Metrics, None, None);
    let (wrong, _) = new_token(TokenKind::Metrics);

    for i in 0..10 {
        let resp = get_with(&app, "/t/metrics", "203.0.113.7", &wrong).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "attempt {i}");
    }
    let resp = get_with(&app, "/t/metrics", "203.0.113.7", &wrong).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = resp.headers()[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .expect("Retry-After in seconds");
    assert!((1..=600).contains(&retry), "{retry}");
    let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
    assert_eq!(json["code"], "rate_limited");

    // A valid token from the blocked address waits too; another address is not affected.
    assert_eq!(
        get_with(&app, "/t/metrics", "203.0.113.7", &metrics)
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        get_with(&app, "/t/metrics", "203.0.113.8", &metrics)
            .await
            .status(),
        StatusCode::OK
    );
    // The limit covers every token kind and the login route.
    let bot_wrong = new_token(TokenKind::Bot).0;
    assert_eq!(
        get_with(&app, "/t/bot", "203.0.113.7", &bot_wrong)
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let resp = send(
        &app,
        request(
            "POST",
            "/api/v1/admin/session",
            "203.0.113.7",
            &[],
            Some(&format!(
                r#"{{"token":"{}"}}"#,
                new_token(TokenKind::Admin).0
            )),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn requests_without_credentials_do_not_use_up_the_limit() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let metrics = issue(&store, TokenKind::Metrics, None, None);
    for _ in 0..30 {
        let resp = send(&app, request("GET", "/t/metrics", "203.0.113.9", &[], None)).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        get_with(&app, "/t/metrics", "203.0.113.9", &metrics)
            .await
            .status(),
        StatusCode::OK
    );
}

#[test]
fn xff_trusted_only_from_loopback() {
    fn ip(peer: &str, xff: &[&str]) -> String {
        let mut b = Request::builder().uri("/");
        for v in xff {
            b = b.header("x-forwarded-for", *v);
        }
        let (parts, ()) = b.body(()).unwrap().into_parts();
        let peer = SocketAddr::new(peer.parse().unwrap(), 1);
        client_ip(&parts, peer).to_string()
    }
    // Caddy appends the address it saw: the last entry is the one to trust.
    assert_eq!(
        ip("127.0.0.1", &["198.51.100.1, 203.0.113.5"]),
        "203.0.113.5"
    );
    assert_eq!(ip("::1", &["203.0.113.5"]), "203.0.113.5");
    assert_eq!(ip("127.0.0.1", &["1.1.1.1", "203.0.113.5"]), "203.0.113.5");
    assert_eq!(ip("127.0.0.1", &["2001:db8::1"]), "2001:db8::1");
    // Not from a loopback peer: the header is ignored.
    assert_eq!(ip("198.51.100.77", &["203.0.113.5"]), "198.51.100.77");
    assert_eq!(ip("10.1.2.3", &["203.0.113.5"]), "10.1.2.3");
    // No header, or one that does not parse: the peer.
    assert_eq!(ip("127.0.0.1", &[]), "127.0.0.1");
    assert_eq!(ip("127.0.0.1", &["not-an-ip"]), "127.0.0.1");
    assert_eq!(ip("127.0.0.1", &["1.1.1.1, "]), "127.0.0.1");
}

#[tokio::test]
async fn xff_header_cannot_dodge_the_limit_from_outside() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let (wrong, _) = new_token(TokenKind::Metrics);
    let (k, v) = bearer(&wrong);
    // From a non-loopback peer each request claims a new address: it must not help.
    for i in 0..10 {
        let xff = format!("192.0.2.{i}");
        let resp = send(
            &app,
            request(
                "GET",
                "/t/metrics",
                "198.51.100.50",
                &[(k, v.as_str()), ("x-forwarded-for", xff.as_str())],
                None,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let resp = send(
        &app,
        request(
            "GET",
            "/t/metrics",
            "198.51.100.50",
            &[(k, v.as_str()), ("x-forwarded-for", "192.0.2.99")],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);

    // Behind the proxy (loopback peer) the forwarded address is the client: separate buckets.
    for _ in 0..10 {
        let resp = send(
            &app,
            request(
                "GET",
                "/t/metrics",
                "127.0.0.1",
                &[(k, v.as_str()), ("x-forwarded-for", "192.0.2.200")],
                None,
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let resp = send(
        &app,
        request(
            "GET",
            "/t/metrics",
            "127.0.0.1",
            &[(k, v.as_str()), ("x-forwarded-for", "192.0.2.200")],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    let resp = send(
        &app,
        request(
            "GET",
            "/t/metrics",
            "127.0.0.1",
            &[(k, v.as_str()), ("x-forwarded-for", "192.0.2.201")],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ---- sessions ----

#[tokio::test]
async fn session_cookie_flags_httponly_secure_strict_7d() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let admin = rotate_admin(&store).unwrap();
    let (value, csrf, set_cookie) = login(&app, "10.0.0.1", &admin).await;

    for flag in [
        "HttpOnly",
        "Secure",
        "SameSite=Strict",
        "Path=/",
        "Max-Age=604800",
    ] {
        assert!(set_cookie.contains(flag), "{flag} missing in {set_cookie}");
    }
    assert_eq!(
        BASE64URL_NOPAD.decode(value.as_bytes()).unwrap().len(),
        32,
        "32 random bytes"
    );
    assert_ne!(value, csrf);

    // The database holds the hash of the cookie, not the cookie, and 7 days of life.
    let conn = store.read();
    let (hash, stored_csrf, created, expires): (Vec<u8>, String, i64, i64) = conn
        .query_row(
            "SELECT hash, csrf, created_at, expires_at FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(hash, sha256(&value).to_vec());
    assert_eq!(stored_csrf, csrf);
    assert_eq!(expires - created, WEEK_MS);

    // And the cookie opens the admin routes.
    let resp = send(
        &app,
        request(
            "GET",
            "/t/admin",
            "10.0.0.1",
            &[("cookie", &cookie_header(&value))],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn login_rejects_bad_tokens() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let o = owner(&store, "alice");
    let bot = issue(&store, TokenKind::Bot, Some(o), None);
    let post = |body: &str| {
        request(
            "POST",
            "/api/v1/admin/session",
            "10.0.0.1",
            &[("content-type", "application/json")],
            Some(body),
        )
    };
    let wrong = format!(r#"{{"token":"{}"}}"#, new_token(TokenKind::Admin).0);
    assert_eq!(
        send(&app, post(&wrong)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let other = format!(r#"{{"token":"{bot}"}}"#);
    assert_eq!(
        send(&app, post(&other)).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, post("{}")).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&app, post("nope")).await.status(),
        StatusCode::BAD_REQUEST
    );
    let none = send(&app, post(&wrong)).await;
    assert!(none.headers().get(header::SET_COOKIE).is_none());

    // Unknown fields are refused as a bad request, and are not counted as failed guesses.
    let admin = rotate_admin(&store).unwrap();
    let extra = format!(r#"{{"token":"{admin}","x":1}}"#);
    for _ in 0..12 {
        let resp = send(
            &app,
            request(
                "POST",
                "/api/v1/admin/session",
                "10.0.0.2",
                &[("content-type", "application/json")],
                Some(&extra),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
    }
    let ok = format!(r#"{{"token":"{admin}"}}"#);
    let resp = send(
        &app,
        request(
            "POST",
            "/api/v1/admin/session",
            "10.0.0.2",
            &[("content-type", "application/json")],
            Some(&ok),
        ),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "bad bodies did not use up the limit"
    );
}

#[tokio::test]
async fn get_session_returns_csrf_for_a_live_session() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let admin = rotate_admin(&store).unwrap();
    let (value, csrf, _) = login(&app, "10.0.0.1", &admin).await;
    let get = |headers: &[(&str, &str)]| {
        request("GET", "/api/v1/admin/session", "10.0.0.1", headers, None)
    };

    // A live cookie gets its CSRF token back, without a CSRF header, and nothing is cached.
    let resp = send(&app, get(&[("cookie", &cookie_header(&value))])).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
    let body = to_bytes(resp.into_body(), 4096).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["csrf"], csrf);

    // A bearer admin token has no session: 204, nothing to return.
    let (k, v) = bearer(&admin);
    let resp = send(&app, get(&[(k, &v)])).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // No credentials, or a cookie that matches no session: 401.
    assert_eq!(
        send(&app, get(&[])).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let resp = send(&app, get(&[("cookie", &cookie_header("nope"))])).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn csrf_required_on_mutations() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let admin = rotate_admin(&store).unwrap();
    let (value, csrf, _) = login(&app, "10.0.0.1", &admin).await;
    let cookie = cookie_header(&value);

    let post = |extra: &[(&str, &str)]| {
        let mut h = vec![("cookie", cookie.as_str())];
        h.extend_from_slice(extra);
        request("POST", "/t/admin", "10.0.0.1", &h, None)
    };
    // Reads need no CSRF token.
    let resp = send(
        &app,
        request("GET", "/t/admin", "10.0.0.1", &[("cookie", &cookie)], None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Mutations do.
    let resp = send(&app, post(&[])).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let json: serde_json::Value = serde_json::from_str(&body_string(resp).await).unwrap();
    assert_eq!(json["code"], "csrf");
    assert_eq!(
        send(&app, post(&[("x-csrf-token", "wrong")]))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, post(&[("x-csrf-token", &csrf)])).await.status(),
        StatusCode::OK
    );

    // A bearer admin token is not a browser credential: no CSRF token needed.
    let (k, v) = bearer(&admin);
    let resp = send(
        &app,
        request("POST", "/t/admin", "10.0.0.1", &[(k, v.as_str())], None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Logging out is a mutation too.
    let logout = |extra: &[(&str, &str)]| {
        let mut h = vec![("cookie", cookie.as_str())];
        h.extend_from_slice(extra);
        request("DELETE", "/api/v1/admin/session", "10.0.0.1", &h, None)
    };
    assert_eq!(
        send(&app, logout(&[])).await.status(),
        StatusCode::FORBIDDEN
    );
    let resp = send(&app, logout(&[("x-csrf-token", &csrf)])).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let cleared = resp.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    let resp = send(
        &app,
        request("GET", "/t/admin", "10.0.0.1", &[("cookie", &cookie)], None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn expired_session_rejected() {
    let (dir, store) = open_store();
    let app = test_app(app_state(&store));
    let admin = rotate_admin(&store).unwrap();
    let (value, _, _) = login(&app, "10.0.0.1", &admin).await;
    // Move the end of the session into the past, as the writer would have recorded it.
    let rw = rusqlite::Connection::open(dir.path().join("registrar.db")).unwrap();
    rw.execute("UPDATE sessions SET expires_at = 1", [])
        .unwrap();
    let resp = send(
        &app,
        request(
            "GET",
            "/t/admin",
            "10.0.0.1",
            &[("cookie", &cookie_header(&value))],
            None,
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rotating_admin_token_kills_sessions() {
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let first = rotate_admin(&store).unwrap();
    let (v1, _, _) = login(&app, "10.0.0.1", &first).await;
    let (v2, _, _) = login(&app, "10.0.0.2", &first).await;
    let cookie_ok = |v: &str| {
        request(
            "GET",
            "/t/admin",
            "10.0.0.1",
            &[("cookie", &cookie_header(v))],
            None,
        )
    };
    assert_eq!(send(&app, cookie_ok(&v1)).await.status(), StatusCode::OK);

    let second = rotate_admin(&store).unwrap();
    assert_ne!(first, second);

    assert_eq!(
        send(&app, cookie_ok(&v1)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&app, cookie_ok(&v2)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_with(&app, "/t/admin", "10.0.0.1", &first)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get_with(&app, "/t/admin", "10.0.0.1", &second)
            .await
            .status(),
        StatusCode::OK
    );
    let n: i64 = store
        .read()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    // Other kinds are not touched by the rotation.
    let m = issue(&store, TokenKind::Metrics, None, None);
    rotate_admin(&store).unwrap();
    assert_eq!(
        get_with(&app, "/t/metrics", "10.0.0.1", &m).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn full_router_serves_login() {
    let (_dir, store) = open_store();
    let app = router(app_state(&store));
    let admin = rotate_admin(&store).unwrap();
    let (_value, csrf, _) = login(&app, "10.0.0.1", &admin).await;
    assert!(!csrf.is_empty());
}

// ---- secrets stay out of logs and responses ----

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn captured_logs() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Capture(Arc::clone(&buf));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        // Global: the store and request work also runs on other threads.
        tracing::subscriber::set_global_default(subscriber).expect("one global subscriber");
        buf
    })
}

/// A response read to the end: status, headers and body as text.
struct Seen {
    headers: String,
    body: String,
    status: StatusCode,
    set_cookie: Option<String>,
}

async fn seen(resp: Response<Body>) -> Seen {
    let status = resp.status();
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().to_owned());
    let headers = format!("{:?}", resp.headers());
    let body = body_string(resp).await;
    Seen {
        headers,
        body,
        status,
        set_cookie,
    }
}

#[tokio::test]
async fn token_values_absent_from_logs_and_responses() {
    let logs = captured_logs();
    let (_dir, store) = open_store();
    let app = test_app(app_state(&store));
    let admin = rotate_admin(&store).unwrap();
    let wrong_admin = new_token(TokenKind::Admin).0;
    let wrong_bearer = new_token(TokenKind::Bot).0;

    let login_body = format!(r#"{{"token":"{admin}"}}"#);
    let login = seen(
        send(
            &app,
            request(
                "POST",
                "/api/v1/admin/session",
                "10.0.0.5",
                &[],
                Some(&login_body),
            ),
        )
        .await,
    )
    .await;
    assert_eq!(login.status, StatusCode::OK);
    let set_cookie = login.set_cookie.clone().expect("Set-Cookie");
    let cookie_value = set_cookie
        .strip_prefix("mtpool_session=")
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let csrf = serde_json::from_str::<serde_json::Value>(&login.body).unwrap()["csrf"]
        .as_str()
        .unwrap()
        .to_owned();

    // The session value shows in the cookie only, the CSRF token in the body only, the admin
    // token nowhere in the answer.
    assert!(login.headers.contains(&cookie_value));
    assert!(!login.body.contains(&cookie_value));
    assert!(!login.headers.contains(&csrf));
    assert!(!login.headers.contains(&admin) && !login.body.contains(&admin));

    let failed_login = seen(
        send(
            &app,
            request(
                "POST",
                "/api/v1/admin/session",
                "10.0.0.5",
                &[],
                Some(&format!(r#"{{"token":"{wrong_admin}"}}"#)),
            ),
        )
        .await,
    )
    .await;
    let failed_bearer = seen(get_with(&app, "/t/bot", "10.0.0.5", &wrong_bearer).await).await;
    let with_session = seen(
        send(
            &app,
            request(
                "POST",
                "/t/admin",
                "10.0.0.5",
                &[
                    ("cookie", &cookie_header(&cookie_value)),
                    ("x-csrf-token", &csrf),
                ],
                None,
            ),
        )
        .await,
    )
    .await;
    assert_eq!(with_session.status, StatusCode::OK);
    for _ in 0..10 {
        let _ = get_with(&app, "/t/bot", "10.0.0.6", &wrong_bearer).await;
    }
    let rate_limited = seen(get_with(&app, "/t/bot", "10.0.0.6", &wrong_bearer).await).await;
    assert_eq!(rate_limited.status, StatusCode::TOO_MANY_REQUESTS);

    let secrets = [&admin, &wrong_admin, &wrong_bearer, &cookie_value, &csrf];
    for (name, r) in [
        ("failed login", &failed_login),
        ("failed bearer", &failed_bearer),
        ("request with a session", &with_session),
        ("rate limited", &rate_limited),
    ] {
        for secret in secrets {
            assert!(
                !r.headers.contains(secret.as_str()) && !r.body.contains(secret.as_str()),
                "the answer to {name} leaks a secret"
            );
        }
    }

    store.flush().unwrap();
    let log = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
    assert!(
        log.contains("authentication failed"),
        "the capture works and failed attempts are logged"
    );
    for secret in secrets {
        assert!(!log.contains(secret.as_str()), "a secret reached the logs");
    }
    // Not even the random part of a token, which would identify it as well.
    for token in [&admin, &wrong_admin, &wrong_bearer] {
        assert!(!log.contains(token.rsplit('_').next().unwrap()));
    }
}

// ---- the command line ----

#[test]
fn cli_reset_token_replaces_hash_and_prints_once() {
    let (_dir, path) = temp_db();
    let run = || {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_mtpool-registrar"))
            .args(["admin", "reset-token"])
            .env("MTPOOL_DB_PATH", &path)
            .env_remove("MTPOOL_LOG")
            .output()
            .expect("run the binary");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        (stdout, stderr)
    };

    let (first_out, first_err) = run();
    // Exactly the token and a newline, nothing else.
    let first = first_out.strip_suffix('\n').expect("newline-terminated");
    assert!(first.starts_with("mtp_admin_") && !first.contains(char::is_whitespace));
    assert_eq!(first_out.matches("mtp_").count(), 1, "printed once");
    assert!(!first_err.contains(first), "no token on stderr");

    let (second_out, second_err) = run();
    let second = second_out.strip_suffix('\n').expect("newline-terminated");
    assert_ne!(first, second);
    assert!(!second_err.contains(second));

    let store = Store::open(&path).unwrap();
    let conn = store.read();
    let active: Vec<Vec<u8>> = conn
        .prepare("SELECT hash FROM tokens WHERE kind = 'admin' AND revoked_at IS NULL")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        active,
        vec![sha256(second).to_vec()],
        "one active admin token"
    );
    let old_revoked: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tokens WHERE hash = ?1 AND revoked_at IS NOT NULL",
            [sha256(first).to_vec()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(old_revoked, 1, "the old token is revoked");
}
