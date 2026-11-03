//! Stand-ins for the services outside the pool: Globalping, cheburcheck, RedFlag and Cloudflare,
//! plus the plain-text address service and the ipsum list the nodes ask. Every answer has the
//! shape of the recorded real one (the fixtures of the registrar tests are embedded and patched),
//! and `POST /__control` switches what a sensor answers. `GET /__state` shows what the stand-ins
//! have seen. Only for the end-to-end stand: they check almost nothing and keep everything in
//! memory.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use serde_json::{Map, Value, json};

const GP_OK: &str = include_str!("../../registrar/tests/fixtures/globalping/finished_tls_403.json");
const GP_BLOCKED: &str =
    include_str!("../../registrar/tests/fixtures/globalping/finished_no_tls.json");
const GP_WAITING: &str = include_str!("../../registrar/tests/fixtures/globalping/in_progress.json");
const GP_CREATE: &str = include_str!("../../registrar/tests/fixtures/globalping/create.json");
const GP_LIMITS: &str = include_str!("../../registrar/tests/fixtures/globalping/limits_anon.json");
const GP_NOT_FOUND: &str = include_str!("../../registrar/tests/fixtures/globalping/not_found.json");
const CHEBUR_CHECK: &str =
    include_str!("../../registrar/tests/fixtures/cheburcheck/check_1.1.1.1.json");
const CHEBUR_STREAM: &str =
    include_str!("../../registrar/tests/fixtures/cheburcheck/probe_stream_1.1.1.1.txt");
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

/// The one zone the fake Cloudflare knows.
const ZONE_NAME: &str = "example.com";
const ZONE_ID: &str = "023e105f4ecef8ad9ca31a8372d0c353";

/// The sensors and the modes each can be switched to; the first mode is the default.
const MODES: [(&str, &[&str]); 4] = [
    ("globalping", &["ok", "blocked", "timeout", "error"]),
    ("cheburcheck", &["ok", "blocked", "unavailable"]),
    ("redflag", &["ok", "blocked", "unauthorized"]),
    ("cloudflare", &["ok", "rate_limited"]),
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
    records: Vec<Record>,
    next_record: u64,
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
        &json!({"dns": dns, "measurements": inner.created, "calls": inner.calls}),
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
        "cheburcheck" => {
            fakes.count("cheburcheck");
            cheburcheck(&fakes, &method, &segments, &query)
        }
        "redflag" => {
            fakes.count("redflag");
            redflag(&fakes, &method, &segments, &headers, &body)
        }
        "cloudflare" => {
            fakes.count("cloudflare");
            cloudflare(&fakes, &method, &segments, &query, &headers, &body)
        }
        _ => not_found(),
    }
}

fn not_found() -> Response {
    json_response(StatusCode::NOT_FOUND, &json!({"error": "no such path"}))
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

/// A time as Globalping writes it: RFC 3339 with milliseconds (`rfc3339` already has them).
fn measurement_time(unix_ms: i64) -> String {
    mtpool_protocol::rfc3339(unix_ms)
}

// --- cheburcheck --------------------------------------------------------------------------------

fn cheburcheck(
    fakes: &Fakes,
    method: &Method,
    segments: &[&str],
    query: &HashMap<String, String>,
) -> Response {
    let mode = fakes.mode("cheburcheck");
    if mode == "unavailable" {
        return json_response(StatusCode::SERVICE_UNAVAILABLE, &json!({"error": "fake"}));
    }
    match (method, segments) {
        (&Method::GET, ["api", "v1", "check"]) => {
            let Some(target) = query.get("target") else {
                return json_response(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &json!({"error": "no target"}),
                );
            };
            let mut document = recorded(CHEBUR_CHECK);
            // The fast flag stays what the recording says (a provider network is listed): it is
            // never a vote.
            document["id"] = json!(format!("fake-{target}"));
            document["target"] = json!(target);
            document["ips"] = json!([target]);
            json_response(StatusCode::OK, &document)
        }
        (&Method::GET, ["api", "v1", "probe", id]) => {
            let stream = CHEBUR_STREAM
                .replace("820de57d-a653-4e23-8b86-c904d4ef67c7", id)
                .replace(
                    "\"target\":\"1.1.1.1\"",
                    &format!("\"target\":\"{}\"", target_of(id)),
                );
            let stream = if mode == "blocked" {
                stream.replace(r#""verdicts":["ok"]"#, r#""verdicts":["tspu_block"]"#)
            } else {
                stream.replace(r#""verdicts":["tspu_block"]"#, r#""verdicts":["ok"]"#)
            };
            text_response(StatusCode::OK, "text/event-stream", stream)
        }
        _ => not_found(),
    }
}

fn target_of(id: &str) -> &str {
    id.strip_prefix("fake-").unwrap_or(id)
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
            inner.records.push(record);
            json_response(StatusCode::OK, &document)
        }
        (&Method::DELETE, ["client", "v4", "zones", zone, "dns_records", id]) => {
            if *zone != ZONE_ID {
                return zone_missing();
            }
            let mut inner = fakes.lock();
            let before = inner.records.len();
            inner.records.retain(|r| r.id != *id);
            if inner.records.len() == before {
                return json_response(
                    StatusCode::NOT_FOUND,
                    &json!({"success": false, "errors": [{"code": 81044, "message": "Record does not exist."}], "messages": [], "result": null}),
                );
            }
            let mut document = recorded(CF_DELETE);
            document["result"]["id"] = json!(id);
            json_response(StatusCode::OK, &document)
        }
        _ => not_found(),
    }
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
            ("cheburcheck", "blocked"),
            ("cheburcheck", "unavailable"),
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
    async fn cheburcheck_streams_verdicts_in_the_recorded_shape() {
        let app = router(Fakes::default());
        let (_, check) = json_of(&app, get("/cheburcheck/api/v1/check?target=192.0.2.7")).await;
        assert_eq!(check["target"], "192.0.2.7");
        assert_eq!(check["blocked"], true, "the fast flag is not a vote");
        let id = check["id"].as_str().expect("an id");

        let (status, ok) = call(&app, get(&format!("/cheburcheck/api/v1/probe/{id}"))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(ok.contains("event:done"));
        assert!(!ok.contains("tspu_block"));
        assert!(ok.contains(r#""verdicts":["ok"]"#));

        assert_eq!(
            control(&app, "cheburcheck", "blocked").await,
            StatusCode::OK
        );
        let (_, blocked) = call(&app, get(&format!("/cheburcheck/api/v1/probe/{id}"))).await;
        assert!(blocked.contains("tspu_block"));
        assert!(!blocked.contains(r#""verdicts":["ok"]"#));

        assert_eq!(
            control(&app, "cheburcheck", "unavailable").await,
            StatusCode::OK
        );
        let (status, _) = call(&app, get("/cheburcheck/api/v1/check?target=192.0.2.7")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
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
                        "ttl":60,"proxied":false,"comment":"mtpool"}),
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
        assert_eq!(listed["result"][0]["comment"], "mtpool");
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
                    "content":"192.0.2.7","comment":"mtpool","id":id}])
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
}
