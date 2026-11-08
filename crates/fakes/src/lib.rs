//! Stand-ins for the services outside the pool: Globalping, check-host.net, RedFlag, Cloudflare and
//! GitHub Releases, plus the plain-text address service and the ipsum list the nodes ask. Every answer has the
//! shape of the recorded real one (the fixtures of the registrar tests are embedded and patched),
//! and `POST /__control` switches what a sensor answers. `GET /__state` shows what the stand-ins
//! have seen. Only for the end-to-end stand: they check almost nothing and keep everything in
//! memory.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use serde_json::{Map, Value, json};

const GP_OK: &str = include_str!("../../registrar/tests/fixtures/globalping/finished_tls_403.json");
const GP_BLOCKED: &str =
    include_str!("../../registrar/tests/fixtures/globalping/finished_no_tls.json");
const GP_WAITING: &str = include_str!("../../registrar/tests/fixtures/globalping/in_progress.json");
const GP_CREATE: &str = include_str!("../../registrar/tests/fixtures/globalping/create.json");
const GP_LIMITS: &str = include_str!("../../registrar/tests/fixtures/globalping/limits_anon.json");
const GP_NOT_FOUND: &str = include_str!("../../registrar/tests/fixtures/globalping/not_found.json");
const CH_START: &str = include_str!("../../registrar/tests/fixtures/check-host/start.json");
const CH_CONNECTED: &str = include_str!("../../registrar/tests/fixtures/check-host/connected.json");
const CH_TIMED_OUT: &str = include_str!("../../registrar/tests/fixtures/check-host/timed_out.json");
const RF_OK: &str = include_str!("../../registrar/tests/fixtures/redflag/ok.json");
const RF_BLOCKED: &str = include_str!("../../registrar/tests/fixtures/redflag/rkn_block.json");
const RF_UNAUTHORIZED: &str =
    include_str!("../../registrar/tests/fixtures/redflag/unauthorized.json");
const CF_ZONES: &str = include_str!("../../registrar/tests/fixtures/cloudflare/zones_found.json");
const CF_ZONES_NONE: &str =
    include_str!("../../registrar/tests/fixtures/cloudflare/zones_none.json");
const CF_RECORDS: &str =
    include_str!("../../registrar/tests/fixtures/cloudflare/records_none.json");
const CF_CREATE: &str = include_str!("../../registrar/tests/fixtures/cloudflare/create.json");
const CF_DELETE: &str = include_str!("../../registrar/tests/fixtures/cloudflare/delete.json");
const CF_RATE_LIMITED: &str =
    include_str!("../../registrar/tests/fixtures/cloudflare/error_rate_limited.json");
const CF_MISSING_AUTH: &str =
    include_str!("../../registrar/tests/fixtures/cloudflare/error_missing_auth.json");

const GH_RELEASE: &str = include_str!("../../registrar/tests/fixtures/github/release_latest.json");
const GH_RATE_LIMITED: &str =
    include_str!("../../registrar/tests/fixtures/github/rate_limited.json");

/// The one zone the fake Cloudflare knows.
const ZONE_NAME: &str = "example.com";
const ZONE_ID: &str = "023e105f4ecef8ad9ca31a8372d0c353";

/// The sensors and the modes each can be switched to; the first mode is the default.
const MODES: [(&str, &[&str]); 5] = [
    ("globalping", &["ok", "blocked", "timeout", "error"]),
    (
        "check_host",
        &[
            "ok", "blocked", "refused", "mixed", "limited", "garbage", "foreign", "same_id",
        ],
    ),
    ("redflag", &["ok", "blocked", "unauthorized"]),
    ("cloudflare", &["ok", "rate_limited"]),
    ("github", &["ok", "rate_limited", "error"]),
];

/// What a fake Globalping measurement was asked to do.
struct Measurement {
    target: String,
    port: u64,
    host: String,
    created_ms: i64,
}

struct Record {
    id: String,
    zone: String,
    name: String,
    kind: String,
    content: String,
    comment: Option<String>,
    ttl: u64,
    proxied: bool,
}

#[derive(Default)]
struct Inner {
    modes: HashMap<&'static str, &'static str>,
    calls: BTreeMap<&'static str, u64>,
    measurements: BTreeMap<String, Measurement>,
    created: u64,
    /// The check-host starts by id (the latest start owns a shared id), and in order.
    check_host_starts: BTreeMap<String, CheckHostStart>,
    check_host_log: Vec<Value>,
    /// Starts the stub turned away with `limit_exceeded`.
    check_host_limited: Vec<Value>,
    /// Every read of an answer, with the mode it was answered in.
    check_host_fetches: Vec<Value>,
    check_host_serial: u64,
    records: Vec<Record>,
    next_record: u64,
    /// Every create and delete of a DNS record, in the order they happened.
    dns_log: Vec<Value>,
    /// Files uploaded for the next release, by name.
    github_staged: BTreeMap<String, Vec<u8>>,
    /// The published release: its tag, its files, and a counter that changes with every publish.
    github_release: Option<GithubRelease>,
    github_publishes: u64,
    /// The name of every file that was downloaded, in order.
    github_downloads: Vec<String>,
}

struct GithubRelease {
    tag: String,
    files: BTreeMap<String, Vec<u8>>,
    etag: String,
}

/// The state of the stand-ins; cheap to clone, every clone is the same one.
#[derive(Clone, Default)]
pub struct Fakes {
    inner: Arc<Mutex<Inner>>,
}

impl Fakes {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn mode(&self, sensor: &'static str) -> &'static str {
        let inner = self.lock();
        inner.modes.get(sensor).copied().unwrap_or_else(|| {
            MODES
                .iter()
                .find(|(name, _)| *name == sensor)
                .map_or("ok", |(_, modes)| modes[0])
        })
    }

    fn count(&self, sensor: &'static str) {
        *self.lock().calls.entry(sensor).or_default() += 1;
    }
}

/// The stand-ins as one HTTP service.
pub fn router(fakes: Fakes) -> Router {
    Router::new()
        .route("/__control", post(control))
        .route("/__state", get(state))
        .route(
            "/__github/files/{name}",
            put(github_upload).layer(DefaultBodyLimit::max(GITHUB_FILE_LIMIT)),
        )
        .route("/__github/publish", post(github_publish))
        .route("/__github/reset", post(github_reset))
        .route("/ip", get(ip))
        .route("/ipsum", get(ipsum))
        .route("/{*path}", any(dispatch))
        .with_state(fakes)
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body.to_string(),
    )
        .into_response()
}

fn text_response(status: StatusCode, content_type: &'static str, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body,
    )
        .into_response()
}

/// A recorded document as a JSON value, without the note on where it came from.
fn recorded(text: &str) -> Value {
    let mut value: Value = serde_json::from_str(text).expect("an embedded fixture is JSON");
    if let Some(object) = value.as_object_mut() {
        object.remove("_source");
    }
    value
}

fn now_ms() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}

async fn control(State(fakes): State<Fakes>, body: String) -> Response {
    let Ok(request) = serde_json::from_str::<Value>(&body) else {
        return json_response(StatusCode::BAD_REQUEST, &json!({"error": "not JSON"}));
    };
    let sensor = request.get("sensor").and_then(Value::as_str).unwrap_or("");
    let mode = request.get("mode").and_then(Value::as_str).unwrap_or("");
    let Some((name, modes)) = MODES.iter().find(|(name, _)| *name == sensor) else {
        return json_response(StatusCode::BAD_REQUEST, &json!({"error": "unknown sensor"}));
    };
    let Some(mode) = modes.iter().find(|m| **m == mode) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": "unknown mode", "modes": modes}),
        );
    };
    fakes.lock().modes.insert(name, mode);
    json_response(StatusCode::OK, &json!({"sensor": name, "mode": mode}))
}

async fn state(State(fakes): State<Fakes>) -> Response {
    let inner = fakes.lock();
    let dns: Vec<Value> = inner
        .records
        .iter()
        .map(|r| {
            json!({
                "zone": r.zone, "name": r.name, "type": r.kind, "content": r.content,
                "comment": r.comment, "id": r.id,
            })
        })
        .collect();
    json_response(
        StatusCode::OK,
        &json!({
            "dns": dns, "dns_log": inner.dns_log,
            "check_host": {
                "starts": inner.check_host_log,
                "limited": inner.check_host_limited,
                "fetches": inner.check_host_fetches,
            },
            "measurements": inner.created, "calls": inner.calls,
            "github": {
                "tag": inner.github_release.as_ref().map(|r| r.tag.clone()),
                "files": inner.github_release.as_ref().map(|r| r.files.keys().cloned().collect::<Vec<_>>()),
                "downloads": inner.github_downloads,
                "publishes": inner.github_publishes,
            },
        }),
    )
}

/// The address the caller has, as the plain-text services of the nodes tell it.
async fn ip(request: Request) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    match peer {
        Some(std::net::IpAddr::V4(ip)) => {
            text_response(StatusCode::OK, "text/plain", format!("{ip}\n"))
        }
        Some(std::net::IpAddr::V6(ip)) => match ip.to_ipv4_mapped() {
            Some(ip) => text_response(StatusCode::OK, "text/plain", format!("{ip}\n")),
            None => text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain",
                String::new(),
            ),
        },
        None => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain",
            String::new(),
        ),
    }
}

/// A list in the IPsum format: comments, then an address and a count on each line.
async fn ipsum() -> Response {
    text_response(
        StatusCode::OK,
        "text/plain",
        "# a list for the test stand, not a real one\n203.0.113.9\t3\n198.51.100.9\t2\n".to_owned(),
    )
}

async fn dispatch(State(fakes): State<Fakes>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query: HashMap<String, String> = Query::try_from_uri(request.uri())
        .map(|q: Query<HashMap<String, String>>| q.0)
        .unwrap_or_default();
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), 1 << 20).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => return json_response(StatusCode::BAD_REQUEST, &json!({"error": "bad body"})),
    };
    let Some((prefix, rest)) = path.trim_start_matches('/').split_once('/') else {
        return not_found();
    };
    let rest = rest.to_owned();
    let segments: Vec<&str> = rest.split('/').collect();
    match prefix {
        "globalping" => {
            fakes.count("globalping");
            globalping(&fakes, &method, &segments, &body)
        }
        "check-host" => {
            fakes.count("check_host");
            check_host(&fakes, &method, &segments, &query)
        }
        "redflag" => {
            fakes.count("redflag");
            redflag(&fakes, &method, &segments, &headers, &body)
        }
        "cloudflare" => {
            fakes.count("cloudflare");
            cloudflare(&fakes, &method, &segments, &query, &headers, &body)
        }
        "github" => {
            fakes.count("github");
            github(&fakes, &method, &segments, &headers)
        }
        _ => not_found(),
    }
}

fn not_found() -> Response {
    json_response(StatusCode::NOT_FOUND, &json!({"error": "no such path"}))
}

// --- GitHub Releases ----------------------------------------------------------------------------

/// The biggest file the stand takes for a release.
const GITHUB_FILE_LIMIT: usize = 512 * 1024 * 1024;

/// A stand-in for the release files, uploaded before the release is published (the way a
/// workflow uploads the assets of a release).
async fn github_upload(
    State(fakes): State<Fakes>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    fakes.lock().github_staged.insert(name, body.to_vec());
    json_response(StatusCode::OK, &json!({"ok": true}))
}

/// Publishes the uploaded files as the newest release `?tag=`.
async fn github_publish(
    State(fakes): State<Fakes>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(tag) = query.get("tag").filter(|t| !t.is_empty()) else {
        return json_response(
            StatusCode::BAD_REQUEST,
            &json!({"error": "tag is required"}),
        );
    };
    let mut inner = fakes.lock();
    inner.github_publishes += 1;
    let etag = format!("\"fake-{}\"", inner.github_publishes);
    let files = std::mem::take(&mut inner.github_staged);
    inner.github_release = Some(GithubRelease {
        tag: tag.clone(),
        files,
        etag,
    });
    json_response(StatusCode::OK, &json!({"tag": tag}))
}

/// Forgets the release, the uploads and the list of downloads.
async fn github_reset(State(fakes): State<Fakes>) -> Response {
    let mut inner = fakes.lock();
    inner.github_staged.clear();
    inner.github_release = None;
    inner.github_downloads.clear();
    json_response(StatusCode::OK, &json!({"ok": true}))
}

fn github(
    fakes: &Fakes,
    method: &Method,
    segments: &[&str],
    headers: &axum::http::HeaderMap,
) -> Response {
    let mode = fakes.mode("github");
    if *method != Method::GET {
        return not_found();
    }
    if mode == "error" {
        return json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &json!({"message": "fake"}),
        );
    }
    match segments {
        ["repos", _owner, _repo, "releases", "latest"] => {
            if mode == "rate_limited" {
                let reset = now_ms() / 1000 + 1200;
                let mut response = json_response(StatusCode::FORBIDDEN, &recorded(GH_RATE_LIMITED));
                let h = response.headers_mut();
                h.insert("x-ratelimit-limit", HeaderValue::from_static("60"));
                h.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
                h.insert("x-ratelimit-used", HeaderValue::from_static("60"));
                h.insert("x-ratelimit-resource", HeaderValue::from_static("core"));
                if let Ok(value) = HeaderValue::from_str(&reset.to_string()) {
                    h.insert("x-ratelimit-reset", value);
                }
                return response;
            }
            let inner = fakes.lock();
            let Some(release) = &inner.github_release else {
                return json_response(
                    StatusCode::NOT_FOUND,
                    &json!({"message": "Not Found", "status": "404"}),
                );
            };
            let etag =
                HeaderValue::from_str(&release.etag).unwrap_or(HeaderValue::from_static("\"x\""));
            if headers.get(header::IF_NONE_MATCH) == Some(&etag) {
                let mut response = StatusCode::NOT_MODIFIED.into_response();
                response.headers_mut().insert(header::ETAG, etag);
                return response;
            }
            let mut document = recorded(GH_RELEASE);
            let template = document["assets"][0].clone();
            document["tag_name"] = json!(release.tag);
            document["name"] = json!(release.tag);
            document["draft"] = json!(false);
            let assets: Vec<Value> = release
                .files
                .keys()
                .map(|name| {
                    let mut asset = template.clone();
                    asset["name"] = json!(name);
                    asset
                })
                .collect();
            document["assets"] = json!(assets);
            let mut response = json_response(StatusCode::OK, &document);
            response.headers_mut().insert(header::ETAG, etag);
            response
        }
        // The download host: /github/dl/{owner}/{repo}/releases/download/{tag}/{name}
        ["dl", _owner, _repo, "releases", "download", tag, name] => {
            let mut inner = fakes.lock();
            inner.github_downloads.push((*name).to_owned());
            let found = inner
                .github_release
                .as_ref()
                .filter(|r| r.tag == *tag)
                .and_then(|r| r.files.get(*name))
                .cloned();
            match found {
                Some(bytes) => (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    )],
                    bytes,
                )
                    .into_response(),
                None => not_found(),
            }
        }
        _ => not_found(),
    }
}

// --- Globalping ---------------------------------------------------------------------------------

fn globalping(fakes: &Fakes, method: &Method, segments: &[&str], body: &str) -> Response {
    let mode = fakes.mode("globalping");
    match (method, segments) {
        (&Method::GET, ["v1", "limits"]) => json_response(StatusCode::OK, &recorded(GP_LIMITS)),
        (&Method::POST, ["v1", "measurements"]) => {
            if mode == "error" {
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &json!({"error": "fake"}));
            }
            let asked: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let options = &asked["measurementOptions"];
            let mut inner = fakes.lock();
            inner.created += 1;
            let id = format!("fakegp{:014}", inner.created);
            inner.measurements.insert(
                id.clone(),
                Measurement {
                    target: asked["target"].as_str().unwrap_or("").to_owned(),
                    port: options["port"].as_u64().unwrap_or(443),
                    host: options["request"]["host"].as_str().unwrap_or("").to_owned(),
                    created_ms: now_ms(),
                },
            );
            let mut created = recorded(GP_CREATE);
            created["id"] = json!(id);
            json_response(StatusCode::ACCEPTED, &created)
        }
        (&Method::GET, ["v1", "measurements", id]) => {
            if mode == "error" {
                return json_response(StatusCode::INTERNAL_SERVER_ERROR, &json!({"error": "fake"}));
            }
            let inner = fakes.lock();
            let Some(measurement) = inner.measurements.get(*id) else {
                return json_response(StatusCode::NOT_FOUND, &recorded(GP_NOT_FOUND));
            };
            let fixture = match mode {
                "blocked" => GP_BLOCKED,
                "timeout" => GP_WAITING,
                _ => GP_OK,
            };
            let mut document = recorded(fixture);
            document["id"] = json!(id);
            document["target"] = json!(measurement.target);
            document["createdAt"] = json!(measurement_time(measurement.created_ms));
            document["updatedAt"] = json!(measurement_time(now_ms()));
            document["measurementOptions"] =
                json!({"port": measurement.port, "request": {"host": measurement.host}});
            json_response(StatusCode::OK, &document)
        }
        _ => not_found(),
    }
}

/// A time as Globalping writes it: RFC 3339 with always three digits of milliseconds
/// (`rfc3339` drops trailing zeros).
fn measurement_time(unix_ms: i64) -> String {
    let whole_seconds = mtpool_protocol::rfc3339(unix_ms.div_euclid(1000) * 1000);
    let stem = whole_seconds.trim_end_matches('Z');
    format!("{stem}.{:03}Z", unix_ms.rem_euclid(1000))
}

// --- check-host.net -----------------------------------------------------------------------------

/// The nodes check-host.net is asked to measure from.
const CH_NODES: [&str; 2] = ["ru1.node.check-host.net", "ru2.node.check-host.net"];

/// The one id every start gets in the `same_id` mode.
const CH_SHARED_ID: &str = "sharedcheckid0001";

/// What a check start asked for: the address and port the registrar will see in the answer.
struct CheckHostStart {
    host: String,
    port: u64,
}

fn check_host(
    fakes: &Fakes,
    method: &Method,
    segments: &[&str],
    query: &HashMap<String, String>,
) -> Response {
    let mode = fakes.mode("check_host");
    match (method, segments) {
        (&Method::GET, ["check-tcp"]) => {
            let target = query.get("host").map(String::as_str).unwrap_or_default();
            let Some((host, port)) = target.rsplit_once(':') else {
                return json_response(StatusCode::OK, &json!({"error": "invalid_host"}));
            };
            let Ok(port) = port.parse::<u64>() else {
                return json_response(StatusCode::OK, &json!({"error": "invalid_host"}));
            };
            if mode == "limited" {
                fakes
                    .lock()
                    .check_host_limited
                    .push(json!({"host": host, "port": port}));
                return json_response(StatusCode::OK, &json!({"error": "limit_exceeded"}));
            }
            let mut inner = fakes.lock();
            inner.check_host_serial += 1;
            let id = if mode == "same_id" {
                CH_SHARED_ID.to_owned()
            } else {
                format!("fakecheck{:06}", inner.check_host_serial)
            };
            inner
                .check_host_log
                .push(json!({"id": id, "host": host, "port": port, "mode": mode}));
            inner.check_host_starts.insert(
                id.clone(),
                CheckHostStart {
                    host: host.to_owned(),
                    port,
                },
            );
            let mut document = recorded(CH_START);
            document["request_id"] = json!(id);
            document["permanent_link"] = json!(format!("https://check-host.net/check-report/{id}"));
            json_response(StatusCode::OK, &document)
        }
        (&Method::GET, ["check-result-extended", id]) => {
            let mut inner = fakes.lock();
            inner
                .check_host_fetches
                .push(json!({"id": id, "mode": mode}));
            let Some(start) = inner.check_host_starts.get(*id) else {
                return json_response(StatusCode::OK, &json!({}));
            };
            if mode == "garbage" {
                return text_response(
                    StatusCode::OK,
                    "text/html",
                    "<html><body>check-host.net is busy</body></html>".to_owned(),
                );
            }
            let (template, results) = match mode {
                "blocked" => (CH_TIMED_OUT, [Some("Connection timed out"); 2]),
                "refused" => (CH_TIMED_OUT, [Some("Connection refused"); 2]),
                "mixed" => (CH_TIMED_OUT, [None, Some("Connection timed out")]),
                _ => (CH_CONNECTED, [None, None]),
            };
            let host = if mode == "foreign" {
                "198.51.100.77"
            } else {
                start.host.as_str()
            };
            let mut document = recorded(template);
            document["created"] = json!(now_ms() / 1000);
            document["host"] = json!(host);
            document["port"] = json!(start.port.to_string());
            for (node, error) in CH_NODES.iter().zip(results) {
                document["results"][*node] = match error {
                    Some(error) => json!([{"error": error}]),
                    None => json!([{"address": host, "time": 0.0123}]),
                };
            }
            json_response(StatusCode::OK, &document)
        }
        _ => not_found(),
    }
}

// --- RedFlag ------------------------------------------------------------------------------------

fn redflag(
    fakes: &Fakes,
    method: &Method,
    segments: &[&str],
    headers: &header::HeaderMap,
    body: &str,
) -> Response {
    if (method, segments) != (&Method::POST, &["v1", "check"][..]) {
        return not_found();
    }
    let mode = fakes.mode("redflag");
    if mode == "unauthorized" || !headers.contains_key("x-api-key") {
        return json_response(StatusCode::UNAUTHORIZED, &recorded(RF_UNAUTHORIZED));
    }
    let asked: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let mut document = recorded(if mode == "blocked" { RF_BLOCKED } else { RF_OK });
    if let Some(server) = asked.get("server") {
        document["server"] = server.clone();
    }
    if let Some(port) = asked.get("port") {
        document["port"] = port.clone();
    }
    json_response(StatusCode::OK, &document)
}

// --- Cloudflare ---------------------------------------------------------------------------------

fn cloudflare(
    fakes: &Fakes,
    method: &Method,
    segments: &[&str],
    query: &HashMap<String, String>,
    headers: &header::HeaderMap,
    body: &str,
) -> Response {
    if fakes.mode("cloudflare") == "rate_limited" {
        let mut response = json_response(StatusCode::TOO_MANY_REQUESTS, &recorded(CF_RATE_LIMITED));
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    }
    if !headers.contains_key(header::AUTHORIZATION) {
        return json_response(StatusCode::UNAUTHORIZED, &recorded(CF_MISSING_AUTH));
    }
    match (method, segments) {
        (&Method::GET, ["client", "v4", "zones"]) => {
            let name = query.get("name").map_or("", String::as_str);
            if name == ZONE_NAME {
                let mut zones = recorded(CF_ZONES);
                zones["result"][0]["name"] = json!(ZONE_NAME);
                zones["result"][0]["id"] = json!(ZONE_ID);
                json_response(StatusCode::OK, &zones)
            } else {
                json_response(StatusCode::OK, &recorded(CF_ZONES_NONE))
            }
        }
        (&Method::GET, ["client", "v4", "zones", zone, "dns_records"]) => {
            if *zone != ZONE_ID {
                return zone_missing();
            }
            let name = query.get("name");
            let inner = fakes.lock();
            let found: Vec<Value> = inner
                .records
                .iter()
                .filter(|r| name.is_none_or(|n| n.eq_ignore_ascii_case(&r.name)))
                .map(record_json)
                .collect();
            let mut document = recorded(CF_RECORDS);
            document["result_info"]["count"] = json!(found.len());
            document["result_info"]["total_count"] = json!(found.len());
            document["result"] = Value::Array(found);
            json_response(StatusCode::OK, &document)
        }
        (&Method::POST, ["client", "v4", "zones", zone, "dns_records"]) => {
            if *zone != ZONE_ID {
                return zone_missing();
            }
            let asked: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let text = |key: &str| asked.get(key).and_then(Value::as_str).map(str::to_owned);
            let (Some(name), Some(kind), Some(content)) =
                (text("name"), text("type"), text("content"))
            else {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    &json!({"success": false, "errors": [{"code": 9005, "message": "bad record"}], "messages": [], "result": null}),
                );
            };
            let mut inner = fakes.lock();
            inner.next_record += 1;
            let record = Record {
                id: format!("{:032x}", inner.next_record),
                zone: ZONE_NAME.to_owned(),
                name,
                kind,
                content,
                comment: text("comment"),
                ttl: asked.get("ttl").and_then(Value::as_u64).unwrap_or(1),
                proxied: asked
                    .get("proxied")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            let mut document = recorded(CF_CREATE);
            document["result"] = record_json(&record);
            inner.dns_log.push(change("create", &record));
            inner.records.push(record);
            json_response(StatusCode::OK, &document)
        }
        (&Method::DELETE, ["client", "v4", "zones", zone, "dns_records", id]) => {
            if *zone != ZONE_ID {
                return zone_missing();
            }
            let mut inner = fakes.lock();
            let gone = inner
                .records
                .iter()
                .find(|r| r.id == *id)
                .map(|r| change("delete", r));
            inner.records.retain(|r| r.id != *id);
            let Some(gone) = gone else {
                return json_response(
                    StatusCode::NOT_FOUND,
                    &json!({"success": false, "errors": [{"code": 81044, "message": "Record does not exist."}], "messages": [], "result": null}),
                );
            };
            inner.dns_log.push(gone);
            let mut document = recorded(CF_DELETE);
            document["result"]["id"] = json!(id);
            json_response(StatusCode::OK, &document)
        }
        _ => not_found(),
    }
}

/// One entry of the DNS change log.
fn change(op: &str, record: &Record) -> Value {
    json!({
        "op": op, "name": record.name, "type": record.kind,
        "content": record.content, "id": record.id,
    })
}

fn zone_missing() -> Response {
    json_response(
        StatusCode::NOT_FOUND,
        &json!({"success": false, "errors": [{"code": 7003, "message": "no such zone"}], "messages": [], "result": null}),
    )
}

fn record_json(record: &Record) -> Value {
    let mut object = Map::new();
    object.insert("id".into(), json!(record.id));
    object.insert("name".into(), json!(record.name));
    object.insert("type".into(), json!(record.kind));
    object.insert("content".into(), json!(record.content));
    object.insert("proxiable".into(), json!(true));
    object.insert("proxied".into(), json!(record.proxied));
    object.insert("ttl".into(), json!(record.ttl));
    object.insert("settings".into(), json!({}));
    object.insert("meta".into(), json!({}));
    object.insert("comment".into(), json!(record.comment));
    object.insert("tags".into(), json!([]));
    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::{Fakes, router};

    async fn call(app: &axum::Router, request: Request<Body>) -> (StatusCode, String) {
        let response = app.clone().oneshot(request).await.expect("an answer");
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("a body")
            .to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).expect("a request")
    }

    fn get_with_token(uri: &str) -> Request<Body> {
        Request::get(uri)
            .header("authorization", "Bearer e2e-fake-token")
            .body(Body::empty())
            .expect("a request")
    }

    fn post(uri: &str, body: &Value) -> Request<Body> {
        Request::post(uri)
            .header("content-type", "application/json")
            .header("authorization", "Bearer e2e-fake-token")
            .header("x-api-key", "e2e-fake-token")
            .body(Body::from(body.to_string()))
            .expect("a request")
    }

    async fn json_of(app: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
        let (status, body) = call(app, request).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    async fn control(app: &axum::Router, sensor: &str, mode: &str) -> StatusCode {
        call(
            app,
            post("/__control", &json!({"sensor": sensor, "mode": mode})),
        )
        .await
        .0
    }

    #[tokio::test]
    async fn control_switches_known_modes_and_refuses_the_rest() {
        let app = router(Fakes::default());
        for (sensor, mode) in [
            ("globalping", "blocked"),
            ("globalping", "timeout"),
            ("globalping", "error"),
            ("globalping", "ok"),
            ("check_host", "blocked"),
            ("check_host", "garbage"),
            ("redflag", "unauthorized"),
            ("cloudflare", "rate_limited"),
            ("cloudflare", "ok"),
        ] {
            assert_eq!(
                control(&app, sensor, mode).await,
                StatusCode::OK,
                "{sensor} {mode}"
            );
        }
        assert_eq!(
            control(&app, "globalping", "rate_limited").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            control(&app, "cloudflare", "blocked").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(control(&app, "nobody", "ok").await, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn measurement_times_always_have_three_digits_of_milliseconds() {
        for (ms, text) in [
            (1_700_000_000_670, "2023-11-14T22:13:20.670Z"),
            (1_700_000_000_600, "2023-11-14T22:13:20.600Z"),
            (1_700_000_000_000, "2023-11-14T22:13:20.000Z"),
            (1_700_000_000_005, "2023-11-14T22:13:20.005Z"),
        ] {
            assert_eq!(crate::measurement_time(ms), text);
            assert_eq!(mtpool_protocol::parse_rfc3339(text), Some(ms));
        }
    }

    #[tokio::test]
    async fn measurement_times_are_rfc3339_with_milliseconds_and_now() {
        let app = router(Fakes::default());
        let created = json_of(
            &app,
            post(
                "/globalping/v1/measurements",
                &json!({"type": "http", "target": "203.0.113.5",
                        "measurementOptions": {"port": 443, "request": {"host": "www.example.com"}}}),
            ),
        )
        .await
        .1;
        let id = created["id"].as_str().expect("an id");
        let (_, measurement) =
            json_of(&app, get(&format!("/globalping/v1/measurements/{id}"))).await;
        for field in ["createdAt", "updatedAt"] {
            let text = measurement[field].as_str().expect("a time");
            let (stem, millis) = text.rsplit_once('.').expect("milliseconds");
            assert!(
                millis.len() == 4 && millis.ends_with('Z'),
                "{field} has no three-digit milliseconds: {text}"
            );
            assert!(!stem.contains('.'), "{field} has two fractions: {text}");
            let at = mtpool_protocol::parse_rfc3339(text)
                .unwrap_or_else(|| panic!("{field} is not RFC 3339: {text}"));
            let now = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("after the epoch")
                    .as_millis(),
            )
            .expect("fits");
            assert!(
                (now - at).abs() < 10_000,
                "{field} is not about now: {text}"
            );
        }
    }

    #[tokio::test]
    async fn the_mode_changes_the_answer() {
        let app = router(Fakes::default());
        let created = json_of(
            &app,
            post(
                "/globalping/v1/measurements",
                &json!({"type":"http","target":"192.0.2.7",
                        "measurementOptions":{"port":443,"protocol":"HTTPS",
                        "request":{"method":"HEAD","host":"www.example.com","path":"/"}}}),
            ),
        )
        .await;
        assert_eq!(created.0, StatusCode::ACCEPTED);
        let id = created.1["id"].as_str().expect("an id").to_owned();

        let (_, ok) = json_of(&app, get(&format!("/globalping/v1/measurements/{id}"))).await;
        assert_eq!(ok["status"], "finished");
        assert_eq!(ok["target"], "192.0.2.7");
        assert_eq!(
            ok["measurementOptions"]["request"]["host"],
            "www.example.com"
        );
        assert_eq!(ok["results"][0]["result"]["tls"]["authorized"], true);

        assert_eq!(control(&app, "globalping", "blocked").await, StatusCode::OK);
        let (_, blocked) = json_of(&app, get(&format!("/globalping/v1/measurements/{id}"))).await;
        assert_eq!(blocked["status"], "finished");
        assert!(blocked["results"][0]["result"]["tls"].is_null());

        assert_eq!(control(&app, "globalping", "timeout").await, StatusCode::OK);
        let (_, waiting) = json_of(&app, get(&format!("/globalping/v1/measurements/{id}"))).await;
        assert_eq!(waiting["status"], "in-progress");

        assert_eq!(control(&app, "globalping", "error").await, StatusCode::OK);
        let (status, _) = call(&app, get(&format!("/globalping/v1/measurements/{id}"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

        let state = json_of(&app, get("/__state")).await.1;
        assert_eq!(state["measurements"], 1);
        assert!(state["calls"]["globalping"].as_u64() >= Some(5));
    }

    #[tokio::test]
    async fn an_unknown_measurement_is_not_found() {
        let app = router(Fakes::default());
        let (status, _) = call(&app, get("/globalping/v1/measurements/nothing")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn check_host_answers_in_the_recorded_shape() {
        let app = router(Fakes::default());
        let start = "/check-host/check-tcp?host=192.0.2.7:443&node=ru1.node.check-host.net&node=ru2.node.check-host.net";
        let (status, started) = json_of(&app, get(start)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(started["ok"], 1);
        let id = started["request_id"].as_str().expect("an id");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()) && id.len() <= 64);
        assert!(started["nodes"]["ru1.node.check-host.net"].is_array());

        let (_, ok) = json_of(
            &app,
            get(&format!("/check-host/check-result-extended/{id}")),
        )
        .await;
        assert_eq!(ok["command"], "tcp");
        assert_eq!(ok["host"], "192.0.2.7");
        assert_eq!(ok["port"], "443", "the port is a string, as recorded");
        assert!(ok["created"].as_i64().expect("created") > 1_700_000_000);
        assert_eq!(
            ok["results"]["ru1.node.check-host.net"][0]["address"],
            "192.0.2.7"
        );
        assert!(ok["results"]["ru2.node.check-host.net"][0]["time"].is_number());

        assert_eq!(control(&app, "check_host", "blocked").await, StatusCode::OK);
        let (_, blocked) = json_of(
            &app,
            get(&format!("/check-host/check-result-extended/{id}")),
        )
        .await;
        assert_eq!(blocked["host"], "192.0.2.7");
        assert_eq!(
            blocked["results"]["ru1.node.check-host.net"][0]["error"],
            "Connection timed out"
        );

        assert_eq!(control(&app, "check_host", "refused").await, StatusCode::OK);
        let (_, refused) = json_of(
            &app,
            get(&format!("/check-host/check-result-extended/{id}")),
        )
        .await;
        assert_eq!(
            refused["results"]["ru2.node.check-host.net"][0]["error"],
            "Connection refused"
        );

        assert_eq!(control(&app, "check_host", "mixed").await, StatusCode::OK);
        let (_, mixed) = json_of(
            &app,
            get(&format!("/check-host/check-result-extended/{id}")),
        )
        .await;
        assert!(mixed["results"]["ru1.node.check-host.net"][0]["time"].is_number());
        assert!(mixed["results"]["ru2.node.check-host.net"][0]["error"].is_string());

        assert_eq!(control(&app, "check_host", "foreign").await, StatusCode::OK);
        let (_, foreign) = json_of(
            &app,
            get(&format!("/check-host/check-result-extended/{id}")),
        )
        .await;
        assert_ne!(foreign["host"], "192.0.2.7");

        assert_eq!(control(&app, "check_host", "garbage").await, StatusCode::OK);
        let (_, garbage) = call(
            &app,
            get(&format!("/check-host/check-result-extended/{id}")),
        )
        .await;
        assert!(serde_json::from_str::<Value>(&garbage).is_err());

        let state = json_of(&app, get("/__state")).await.1;
        assert_eq!(state["check_host"]["starts"][0]["host"], "192.0.2.7");
        assert_eq!(state["check_host"]["starts"][0]["port"], 443);
        assert!(state["calls"]["check_host"].as_u64() >= Some(7));
        // Every read of an answer is kept with the mode it was answered in.
        let fetches = state["check_host"]["fetches"].as_array().expect("fetches");
        assert_eq!(fetches.len(), 6, "{fetches:?}");
        assert_eq!(fetches[0], json!({"id": id, "mode": "ok"}));
        assert_eq!(fetches[5], json!({"id": id, "mode": "garbage"}));
    }

    #[tokio::test]
    async fn a_limited_start_is_kept_apart_from_the_starts() {
        let app = router(Fakes::default());
        assert_eq!(control(&app, "check_host", "limited").await, StatusCode::OK);
        let (_, answer) = json_of(
            &app,
            get("/check-host/check-tcp?host=192.0.2.9:443&node=ru1.node.check-host.net"),
        )
        .await;
        assert_eq!(answer["error"], "limit_exceeded");
        let state = json_of(&app, get("/__state")).await.1;
        assert_eq!(state["check_host"]["starts"], json!([]));
        assert_eq!(
            state["check_host"]["limited"],
            json!([{"host": "192.0.2.9", "port": 443}])
        );
    }

    #[tokio::test]
    async fn check_host_control_switches_modes() {
        let app = router(Fakes::default());
        let start = "/check-host/check-tcp?host=192.0.2.7:443";
        for mode in [
            "ok", "blocked", "refused", "mixed", "limited", "garbage", "foreign", "same_id",
        ] {
            assert_eq!(
                control(&app, "check_host", mode).await,
                StatusCode::OK,
                "{mode}"
            );
        }
        assert_eq!(
            control(&app, "check_host", "unavailable").await,
            StatusCode::BAD_REQUEST
        );

        assert_eq!(control(&app, "check_host", "limited").await, StatusCode::OK);
        let (_, limited) = json_of(&app, get(start)).await;
        assert_eq!(limited["error"], "limit_exceeded");

        assert_eq!(control(&app, "check_host", "same_id").await, StatusCode::OK);
        let (_, first) = json_of(&app, get(start)).await;
        let (_, second) = json_of(&app, get("/check-host/check-tcp?host=192.0.2.8:443")).await;
        assert_eq!(first["request_id"], second["request_id"]);
        let id = first["request_id"].as_str().expect("an id");
        let (_, result) = json_of(
            &app,
            get(&format!("/check-host/check-result-extended/{id}")),
        )
        .await;
        assert_eq!(
            result["host"], "192.0.2.8",
            "the latest start owns the shared id"
        );

        assert_eq!(control(&app, "check_host", "ok").await, StatusCode::OK);
        let (_, a) = json_of(&app, get(start)).await;
        let (_, b) = json_of(&app, get(start)).await;
        assert_ne!(a["request_id"], b["request_id"]);
    }

    #[tokio::test]
    async fn redflag_needs_a_key_and_follows_the_mode() {
        let app = router(Fakes::default());
        let body = json!({"server":"192.0.2.7","port":443,"secret":"tg://proxy"});
        let uri = "/redflag/v1/check?mode=sync&timeout=15";

        let without_key = Request::post(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("a request");
        assert_eq!(call(&app, without_key).await.0, StatusCode::UNAUTHORIZED);

        let (_, ok) = json_of(&app, post(uri, &body)).await;
        assert_eq!(ok["status"], "OK");
        assert_eq!(ok["server"], "192.0.2.7");
        assert!(ok.get("_source").is_none());

        assert_eq!(control(&app, "redflag", "blocked").await, StatusCode::OK);
        assert_eq!(
            json_of(&app, post(uri, &body)).await.1["status"],
            "RKN_BLOCK"
        );

        assert_eq!(
            control(&app, "redflag", "unauthorized").await,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, post(uri, &body)).await.0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn cloudflare_keeps_records_until_they_are_deleted() {
        let app = router(Fakes::default());

        let (_, zones) = json_of(
            &app,
            get_with_token("/cloudflare/client/v4/zones?name=example.com&status=active"),
        )
        .await;
        let zone = zones["result"][0]["id"]
            .as_str()
            .expect("a zone id")
            .to_owned();
        let (_, none) = json_of(
            &app,
            get_with_token("/cloudflare/client/v4/zones?name=example.net&status=active"),
        )
        .await;
        assert_eq!(none["result"], json!([]));

        let records = format!("/cloudflare/client/v4/zones/{zone}/dns_records");
        let (status, created) = json_of(
            &app,
            post(
                &records,
                &json!({"type":"A","name":"pool.example.com","content":"192.0.2.7",
                        "ttl":60,"proxied":false,"comment":"MTPooL"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let id = created["result"]["id"]
            .as_str()
            .expect("a record id")
            .to_owned();
        assert_eq!(created["result"]["content"], "192.0.2.7");

        let (_, listed) = json_of(
            &app,
            get_with_token(&format!(
                "{records}?name=pool.example.com&per_page=100&page=1"
            )),
        )
        .await;
        assert_eq!(listed["result"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["result"][0]["comment"], "MTPooL");
        let (_, other) = json_of(
            &app,
            get_with_token(&format!(
                "{records}?name=other.example.com&per_page=100&page=1"
            )),
        )
        .await;
        assert_eq!(other["result"], json!([]));

        let state = json_of(&app, get_with_token("/__state")).await.1;
        assert_eq!(
            state["dns"],
            json!([{"zone":"example.com","name":"pool.example.com","type":"A",
                    "content":"192.0.2.7","comment":"MTPooL","id":id}])
        );

        let (status, _) = json_of(
            &app,
            Request::delete(format!("{records}/{id}"))
                .header("authorization", "Bearer e2e-fake-token")
                .body(Body::empty())
                .expect("a request"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json_of(&app, get_with_token("/__state")).await.1["dns"],
            json!([])
        );
    }

    #[tokio::test]
    async fn cloudflare_changes_are_logged_in_the_order_they_happened() {
        let app = router(Fakes::default());
        let records = format!("/cloudflare/client/v4/zones/{}/dns_records", crate::ZONE_ID);
        let mut ids = Vec::new();
        for content in ["192.0.2.7", "192.0.2.8"] {
            let (_, created) = json_of(
                &app,
                post(
                    &records,
                    &json!({"type":"A","name":"pool.example.com","content":content}),
                ),
            )
            .await;
            ids.push(created["result"]["id"].as_str().expect("an id").to_owned());
        }
        let delete = |id: &str| {
            Request::delete(format!("{records}/{id}"))
                .header("authorization", "Bearer e2e-fake-token")
                .body(Body::empty())
                .expect("a request")
        };
        assert_eq!(json_of(&app, delete(&ids[0])).await.0, StatusCode::OK);
        // A delete of a record that is not there changes nothing and is not logged.
        assert_eq!(
            json_of(&app, delete(&ids[0])).await.0,
            StatusCode::NOT_FOUND
        );
        // A list is not a change.
        json_of(
            &app,
            get_with_token(&format!("{records}?name=pool.example.com")),
        )
        .await;

        let state = json_of(&app, get_with_token("/__state")).await.1;
        assert_eq!(
            state["dns_log"],
            json!([
                {"op":"create","name":"pool.example.com","type":"A","content":"192.0.2.7","id":ids[0]},
                {"op":"create","name":"pool.example.com","type":"A","content":"192.0.2.8","id":ids[1]},
                {"op":"delete","name":"pool.example.com","type":"A","content":"192.0.2.7","id":ids[0]},
            ])
        );
    }

    #[tokio::test]
    async fn cloudflare_can_rate_limit_and_wants_a_token() {
        let app = router(Fakes::default());
        let no_token = get("/cloudflare/client/v4/zones?name=example.com&status=active");
        assert_eq!(call(&app, no_token).await.0, StatusCode::UNAUTHORIZED);

        assert_eq!(
            control(&app, "cloudflare", "rate_limited").await,
            StatusCode::OK
        );
        let request = Request::get("/cloudflare/client/v4/zones?name=example.com&status=active")
            .header("authorization", "Bearer e2e-fake-token")
            .body(Body::empty())
            .expect("a request");
        assert_eq!(call(&app, request).await.0, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn the_address_services_tell_the_caller_its_address() {
        let app = router(Fakes::default());
        let mut request = get("/ip?3");
        let peer: std::net::SocketAddr = "192.0.2.50:41000".parse().expect("an address");
        request.extensions_mut().insert(ConnectInfo(peer));
        assert_eq!(
            call(&app, request).await,
            (StatusCode::OK, "192.0.2.50\n".to_owned())
        );

        let (status, list) = call(&app, get("/ipsum")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(list.lines().any(|l| l.starts_with('#')));
    }

    #[tokio::test]
    async fn unknown_paths_are_not_found() {
        let app = router(Fakes::default());
        assert_eq!(call(&app, get("/nowhere")).await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            call(&app, get("/globalping/v1/nowhere")).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn github_serves_the_published_release_and_its_files() {
        let app = router(Fakes::default());
        let (status, _) = call(&app, get("/github/repos/o/r/releases/latest")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no release yet");

        let put = |name: &str, body: &str| {
            Request::put(format!("/__github/files/{name}"))
                .body(Body::from(body.to_owned()))
                .expect("a request")
        };
        call(&app, put("manifest.json", "{}")).await;
        call(&app, put("mtpool-registrar-x", "binary")).await;
        let publish = Request::post("/__github/publish?tag=v9.9.9")
            .body(Body::empty())
            .expect("a request");
        assert_eq!(call(&app, publish).await.0, StatusCode::OK);

        let response = app
            .clone()
            .oneshot(get("/github/repos/o/r/releases/latest"))
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::OK);
        let etag = response.headers()["etag"].clone();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("a body")
            .to_bytes();
        let release: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(release["tag_name"], "v9.9.9");
        let names: Vec<&str> = release["assets"]
            .as_array()
            .expect("assets")
            .iter()
            .map(|a| a["name"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["manifest.json", "mtpool-registrar-x"]);

        let again = Request::get("/github/repos/o/r/releases/latest")
            .header("if-none-match", etag)
            .body(Body::empty())
            .expect("a request");
        assert_eq!(call(&app, again).await.0, StatusCode::NOT_MODIFIED);

        let (status, body) = call(
            &app,
            get("/github/dl/o/r/releases/download/v9.9.9/mtpool-registrar-x"),
        )
        .await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "binary"));
        let (status, _) = call(
            &app,
            get("/github/dl/o/r/releases/download/v1/mtpool-registrar-x"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "another tag has no such file"
        );

        let (_, state) = call(&app, get("/__state")).await;
        let state: Value = serde_json::from_str(&state).expect("JSON");
        assert_eq!(
            state["github"]["downloads"],
            json!(["mtpool-registrar-x", "mtpool-registrar-x"])
        );
    }

    #[tokio::test]
    async fn github_can_be_rate_limited_or_broken() {
        let app = router(Fakes::default());
        let control = |mode: &str| {
            Request::post("/__control")
                .body(Body::from(
                    json!({"sensor": "github", "mode": mode}).to_string(),
                ))
                .expect("a request")
        };
        assert_eq!(call(&app, control("rate_limited")).await.0, StatusCode::OK);
        let response = app
            .clone()
            .oneshot(get("/github/repos/o/r/releases/latest"))
            .await
            .expect("an answer");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers()["x-ratelimit-remaining"], "0");
        assert!(response.headers().contains_key("x-ratelimit-reset"));
        call(&app, control("error")).await;
        let (status, _) = call(&app, get("/github/repos/o/r/releases/latest")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
