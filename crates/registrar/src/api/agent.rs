//! The agent API under `/api/v1/agent`: enrollment, heartbeat, report, retire and config.
//!
//! The node an agent acts for always comes from its token and the node's address from the
//! connection, never from a body. Requests refuse fields they do not name. Answers of `core`
//! map to HTTP like this:
//!
//! | reply                | answer                                              |
//! |----------------------|-----------------------------------------------------|
//! | `Ok`                 | 200 `{}`                                            |
//! | `Terminate(reason)`  | 403 `{"terminate": true, "reason": .., "message": ..}` |
//! | `UnknownNode`        | 410 `unknown_node`                                  |
//! | `Stale`              | 409 `stale`                                         |
//! | `Refused`            | 409 `refused`                                       |
//! | `EnrollRejected`     | 429 `enroll_paused` with `Retry-After`              |

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::rejection::BytesRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mtpool_core::{
    AbstainReason, Command, EngineKind, Event, EventKind, Generation, NodeAction, NodeId,
    NodeState, Reply, ReplyId, TrafficCounters, Ts, Vote, lifecycle::RejectReason,
};
use mtpool_protocol::agent::{
    AgentConfig, AgentUser, Empty, EmptyRequest, EnrollRequest, EnrollResponse, GpReport,
    Intervals, ReportRequest, Terminate, TerminateReason, UpdateOffer, UpdateOutcome, UpdatePoll,
    UpdateStatus,
};
use mtpool_protocol::problem::Problem;
use mtpool_protocol::public::Engine;
use mtpool_protocol::{parse_rfc3339, rfc3339};
use serde::de::DeserializeOwned;
use serde_json::json;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::ApiError;
use crate::actor::EnrollOutcome;
use crate::auth::middleware::{ClientIp, EnrollAuth, NodeAuth};
use crate::auth::tokens::{self, NewToken, TokenKind};
use crate::server::AppState;
use crate::store::{ReportOp, measurement_seen};
use crate::verifier::{GpExpect, GpRejected};

/// A report may be made this long before the registrar's clock ...
const CHECKED_AT_PAST_MS: i64 = 10 * 60 * 1000;
/// ... and this long after it.
const CHECKED_AT_FUTURE_MS: i64 = 60 * 1000;
/// Measurement ids stay in memory this long; the database keeps them longer.
const LEDGER_KEEP_MS: i64 = 60 * 60 * 1000;
/// How long the verifier may take before the measurement counts as no data.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);
/// Tries at a free node id before enrollment gives up.
const ID_ATTEMPTS: usize = 8;

/// A `ClockSkew` event is written at most this often per node.
const SKEW_EVENT_EVERY_MS: i64 = 60 * 60 * 1000;
/// `Retry-After` of a measured report refused because the last one is still being verified.
const VERIFY_BUSY_RETRY_SEC: u64 = 30;

/// The measurement ids claimed lately. Claiming is atomic, so two reports that name the same
/// id cannot both pass, whichever nodes they come from.
#[derive(Default)]
pub struct Ledger {
    seen: Mutex<HashMap<String, Ts>>,
    /// Nodes with a measured report being verified: one at a time per node.
    in_flight: Mutex<HashSet<NodeId>>,
    /// When a `ClockSkew` event was last written for a node.
    skew_logged: Mutex<HashMap<NodeId, Ts>>,
}

/// The verification slot of a node; frees it on drop, a panic of the task included.
struct VerifySlot {
    ledger: Arc<Ledger>,
    node: NodeId,
}

impl Drop for VerifySlot {
    fn drop(&mut self) {
        let mut busy = self
            .ledger
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        busy.remove(&self.node);
    }
}

impl Ledger {
    pub fn new() -> Ledger {
        Ledger::default()
    }

    /// The node's verification slot, or `None` while one of its measurements is being verified.
    fn take_slot(self: &Arc<Ledger>, node: &NodeId) -> Option<VerifySlot> {
        let mut busy = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        busy.insert(node.clone()).then(|| VerifySlot {
            ledger: Arc::clone(self),
            node: node.clone(),
        })
    }

    /// Whether a `ClockSkew` event for `node` is due now (and notes it): at most one an hour.
    fn skew_event_due(&self, node: &NodeId, now: Ts) -> bool {
        let mut logged = self.skew_logged.lock().unwrap_or_else(|e| e.into_inner());
        logged.retain(|_, at| now.0.saturating_sub(at.0) < SKEW_EVENT_EVERY_MS);
        if logged.contains_key(node) {
            return false;
        }
        logged.insert(node.clone(), now);
        true
    }

    /// Whether `id` is new. Forgets what is older than `LEDGER_KEEP_MS` first.
    fn claim(&self, id: &str, now: Ts) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, at| now.0.saturating_sub(at.0) < LEDGER_KEEP_MS);
        if seen.contains_key(id) {
            return false;
        }
        seen.insert(id.to_owned(), now);
        true
    }
}

#[derive(OpenApi)]
#[openapi(
    tags((name = "agent", description = "Routes of the node agent; they need a node token, enrollment an enroll token")),
    modifiers(&AgentSecurity),
    components(schemas(Problem))
)]
struct AgentDoc;

/// Adds the bearer scheme the agent routes name.
struct AgentSecurity;

impl Modify for AgentSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "`mtp_enroll_...` to enroll and `mtp_node_...` for the rest of the agent routes; `mtp_bot_...` for `/me`; `mtp_admin_...` for `/admin`",
                    ))
                    .build(),
            ),
        );
    }
}

/// The routes of the agent API, with their documentation. Not behind the public request
/// limit: agents send a request every few seconds and are held to the limit on failed
/// attempts instead.
pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(AgentDoc::openapi())
        .routes(routes!(enroll))
        .routes(routes!(heartbeat))
        .routes(routes!(report))
        .routes(routes!(retire))
        .routes(routes!(config))
        .routes(routes!(update))
        .routes(routes!(update_status))
}

// ----- answers -------------------------------------------------------------------------------

/// An error answer of a handler: a problem, or the terminate document.
pub struct Failure(pub(crate) Box<Response>);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        *self.0
    }
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Failure {
        Failure(Box::new(e.into_response()))
    }
}

pub(crate) fn problem(status: StatusCode, code: &str, title: &str, detail: &str) -> Failure {
    ApiError::new(status, code, title, detail).into()
}

pub(crate) fn json_response(status: StatusCode, body: String) -> Response {
    let mut response = (status, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

pub(crate) fn empty_ok() -> Response {
    json_response(StatusCode::OK, "{}".to_owned())
}

fn terminate(reason: mtpool_core::TerminateReason) -> Response {
    let (reason, message) = match reason {
        mtpool_core::TerminateReason::Banned => (
            TerminateReason::Banned,
            "The address of this node is blocked; stop the proxy.",
        ),
        mtpool_core::TerminateReason::Dead => (
            TerminateReason::Dead,
            "The node was silent for too long and was removed; stop the proxy and enroll again.",
        ),
        mtpool_core::TerminateReason::Pruned => (
            TerminateReason::Pruned,
            "The node was removed from the pool; stop the proxy and enroll again later.",
        ),
        mtpool_core::TerminateReason::AddressRefused => (
            TerminateReason::AddressRefused,
            "The node moved to an address that may not enroll; stop the proxy and enroll again.",
        ),
    };
    let body = Terminate {
        terminate: true,
        reason,
        message: message.to_owned(),
    };
    json_response(
        StatusCode::FORBIDDEN,
        serde_json::to_string(&body).expect("a terminate document serializes"),
    )
}

/// The terminal reason of a node state, if it is one.
fn terminal(state: NodeState) -> Option<mtpool_core::TerminateReason> {
    match state {
        NodeState::Banned => Some(mtpool_core::TerminateReason::Banned),
        NodeState::Dead => Some(mtpool_core::TerminateReason::Dead),
        NodeState::Pruned => Some(mtpool_core::TerminateReason::Pruned),
        NodeState::Pending | NodeState::Healthy | NodeState::Quarantine => None,
    }
}

fn unknown_node() -> Failure {
    problem(
        StatusCode::GONE,
        "unknown_node",
        "Unknown node",
        "The registrar does not know this node; enroll again.",
    )
}

pub(crate) fn actor_stopped() -> Failure {
    tracing::error!("agent API: the state owner is not running");
    ApiError::internal().into()
}

/// The answer to a command that has no content of its own.
fn reply_response(reply: Reply) -> Response {
    match reply {
        Reply::Ok => empty_ok(),
        Reply::Terminate(reason) => terminate(reason),
        Reply::UnknownNode => unknown_node().into_response(),
        Reply::Stale => ApiError::new(
            StatusCode::CONFLICT,
            "stale",
            "Stale",
            "The node changed since this request was made; try again.",
        )
        .into_response(),
        Reply::Refused(_) => ApiError::new(
            StatusCode::CONFLICT,
            "refused",
            "Refused",
            "The pool does not accept this request now.",
        )
        .into_response(),
        other => {
            tracing::error!(reply = ?other, "agent API: unexpected reply of the state owner");
            ApiError::internal().into_response()
        }
    }
}

// ----- request helpers -------------------------------------------------------------------------

/// The body as bytes. Too large is the 413 problem, anything else a bad request.
pub(crate) fn body_bytes(body: Result<Bytes, BytesRejection>) -> Result<Bytes, Failure> {
    body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            payload_too_large()
        } else {
            ApiError::bad_request("The request body cannot be read.").into()
        }
    })
}

/// The 413 problem: bodies are limited to `MAX_BODY_BYTES`.
pub fn payload_too_large() -> Failure {
    problem(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        "Payload too large",
        "The request body is larger than 256 KiB.",
    )
}

pub(crate) fn parse<T: DeserializeOwned>(bytes: &Bytes) -> Result<T, Failure> {
    serde_json::from_slice(bytes).map_err(|e| {
        tracing::debug!(error = %e, "agent API: bad request body");
        ApiError::bad_request("The body is not the JSON document this route takes.").into()
    })
}

/// `{}` or nothing at all.
pub(crate) fn parse_empty(bytes: &Bytes) -> Result<(), Failure> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    parse::<EmptyRequest>(bytes).map(|_| ())
}

/// An address a node can have: public IPv4. Documentation ranges count as public, so that
/// tests and examples can use them.
fn is_node_address(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let shared = a == 100 && (64..=127).contains(&b);
    let reserved = a >= 240;
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.is_multicast()
        || shared
        || reserved)
}

/// An address a node has on a private test network: what `is_node_address` refuses for being
/// private, loopback, link-local or shared. Addresses that are no address of a host (unspecified,
/// broadcast, multicast, reserved) stay refused.
fn is_private_node_address(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    let shared = a == 100 && (64..=127).contains(&b);
    is_node_address(ip) || ip.is_private() || ip.is_loopback() || ip.is_link_local() || shared
}

/// The IPv4 address a node request comes from, if it is one a node can have. With
/// `allow_private` (`MTPOOL_ALLOW_PRIVATE_IPS`) the addresses of a private network count too.
fn node_address(ip: IpAddr, allow_private: bool) -> Result<Ipv4Addr, Failure> {
    let v4 = match ip {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(v6) => v6.to_ipv4_mapped(),
    };
    match v4 {
        Some(v4) if is_node_address(v4) || (allow_private && is_private_node_address(v4)) => Ok(v4),
        _ => Err(problem(
            StatusCode::BAD_REQUEST,
            "private_ip",
            "Address not allowed",
            "A node must connect from a public IPv4 address.",
        )),
    }
}

/// A job off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work).await.map_err(|e| {
        tracing::error!(error = %e, "agent API: blocking task failed");
        ApiError::internal().into()
    })
}

fn engine_kind(engine: Engine) -> EngineKind {
    match engine {
        Engine::Telemt => EngineKind::Telemt,
        Engine::MtProxyL => EngineKind::MtProxyL,
        Engine::Meko => EngineKind::Meko,
    }
}

/// What a handler needs of a live node.
struct Live {
    generation: Generation,
    ip: Ipv4Addr,
    port: u16,
    tls_domain: String,
}

/// The node as the registry has it: 410 when it is not there, the terminate document when it
/// is out of the pool.
async fn live_node(app: &AppState, id: &NodeId) -> Result<Live, Failure> {
    let id = id.clone();
    let seen = app
        .actor
        .inspect(move |state| {
            state.nodes.get(&id).map(|n| {
                (
                    n.state,
                    Live {
                        generation: n.generation,
                        ip: n.ip,
                        port: n.port,
                        tls_domain: state.settings.tls_domain.clone(),
                    },
                )
            })
        })
        .await
        .map_err(|_| actor_stopped())?;
    match seen {
        None => Err(unknown_node()),
        Some((state, node)) => match terminal(state) {
            Some(reason) => Err(Failure(Box::new(terminate(reason)))),
            None => Ok(node),
        },
    }
}

// ----- enroll ----------------------------------------------------------------------------------

fn random_suffix() -> String {
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 5];
    rand::fill(&mut bytes);
    bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
        .collect()
}

fn pause_detail(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::PrunePause => {
            "prune_pause: a node of this address was removed lately; enroll again after the pause."
        }
        RejectReason::ReverifyPause => {
            "reverify_pause: this address is blocked and its last check was not clean; enroll again after the pause."
        }
        RejectReason::ReverifyInFlight => {
            "reverify_in_flight: this address is blocked and another node of it is being checked."
        }
    }
}

/// Registers a node of the owner of the enroll token, at the address the request comes from.
#[utoipa::path(
    post,
    path = "/api/v1/agent/enroll",
    tag = "agent",
    security(("bearer" = [])),
    request_body(content = EnrollRequest, description = "The node's name, port and engine"),
    responses(
        (status = 200, description = "The node is enrolled. The node token is shown here once.", body = EnrollResponse),
        (status = 400, description = "Bad name, port or body, or an address that is not public IPv4 (`bad_request`, `invalid_name`, `private_ip`)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid enroll token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The token is of another kind, or the owner is at its node limit (`forbidden`, `owner_limit`)", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is larger than 256 KiB", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Enrollment from this address is paused (`enroll_paused`, `Retry-After`), or too many failed attempts (`rate_limited`)", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until enrollment may be tried again"))),
    )
)]
pub async fn enroll(
    State(app): State<AppState>,
    auth: EnrollAuth,
    ClientIp(peer): ClientIp,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let request: EnrollRequest = parse(&body_bytes(body)?)?;
    if NodeId::parse(&format!("{}-00000", request.name)).is_err() {
        return Err(problem(
            StatusCode::BAD_REQUEST,
            "invalid_name",
            "Invalid name",
            "The name is 1 to 32 letters, digits and `._-`, starting and ending with a letter or digit.",
        ));
    }
    if request.port == 0 {
        return Err(ApiError::bad_request("The port must not be 0.").into());
    }
    let ip = node_address(peer, app.allow_private_ips)?;

    let owner = auth.owner_id;
    let store = app.store.clone();
    let limit = blocking(move || {
        store.read().query_row(
            "SELECT node_limit FROM owners WHERE id = ?1",
            [owner.0],
            |r| r.get::<_, i64>(0),
        )
    })
    .await?;
    let limit = match limit {
        Ok(limit) => limit,
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            return Err(problem(
                StatusCode::FORBIDDEN,
                "forbidden",
                "Forbidden",
                "The owner of this token does not exist.",
            ));
        }
        Err(e) => {
            tracing::error!(error = %e, "agent API: cannot read the owner's limit");
            return Err(ApiError::internal().into());
        }
    };

    let mut node_id = None;
    for _ in 0..ID_ATTEMPTS {
        let candidate = NodeId::parse(&format!("{}-{}", request.name, random_suffix()))
            .expect("a checked name and a generated suffix make a node id");
        let probe = candidate.clone();
        let taken = app
            .actor
            .inspect(move |state| state.nodes.contains_key(&probe))
            .await
            .map_err(|_| actor_stopped())?;
        if !taken {
            node_id = Some(candidate);
            break;
        }
    }
    let Some(node_id) = node_id else {
        tracing::error!("agent API: no free node id after {ID_ATTEMPTS} tries");
        return Err(ApiError::internal().into());
    };

    let command = mtpool_core::EnrollRequest {
        reply: ReplyId(0),
        node_id: node_id.clone(),
        owner,
        ip,
        port: request.port,
        engine: engine_kind(request.engine),
    };
    let outcome = app
        .actor
        .enroll(command, Some(limit))
        .await
        .map_err(|_| actor_stopped())?;
    match outcome {
        EnrollOutcome::OwnerLimit { limit, current } => {
            tracing::info!(
                owner = owner.0,
                limit,
                current,
                "enrollment over the node limit"
            );
            Err(problem(
                StatusCode::FORBIDDEN,
                "owner_limit",
                "Node limit reached",
                "The owner of this token has as many nodes as it may have.",
            ))
        }
        EnrollOutcome::Reply(Reply::EnrollRejected {
            retry_after_sec,
            reason,
        }) => Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "enroll_paused",
            "Enrollment paused",
            pause_detail(reason),
        )
        .retry_after(u64::from(retry_after_sec))
        .into()),
        EnrollOutcome::Reply(Reply::Enrolled {
            node,
            generation,
            replaced,
            reverify,
        }) => {
            let store = app.store.clone();
            let for_node = node.clone();
            let issued = blocking(move || {
                tokens::issue(
                    &store,
                    NewToken {
                        kind: TokenKind::Node,
                        owner: None,
                        node: Some(for_node),
                        expires_at: None,
                        scope: String::new(),
                    },
                )
            })
            .await?;
            match issued {
                Ok(node_token) => {
                    let body = EnrollResponse {
                        node_id: node.to_string(),
                        node_token,
                        generation: generation.0,
                        replaced: replaced.map(|n| n.to_string()),
                        reverify,
                    };
                    Ok(json_response(
                        StatusCode::OK,
                        serde_json::to_string(&body).expect("an enroll answer serializes"),
                    ))
                }
                Err(e) => {
                    // A node nobody holds the token of would only wait to be pruned.
                    tracing::error!(error = %e, node = %node, "agent API: cannot issue the node token");
                    let _ = app
                        .actor
                        .send(Command::Retire(NodeAction {
                            reply: ReplyId(0),
                            node,
                        }))
                        .await;
                    Err(ApiError::internal().into())
                }
            }
        }
        EnrollOutcome::Reply(other) => {
            tracing::error!(reply = ?other, "agent API: unexpected reply to an enrollment");
            Err(ApiError::internal().into())
        }
    }
}

// ----- heartbeat and retire --------------------------------------------------------------------

/// Tells the registrar the node is alive, from the address it connects from.
#[utoipa::path(
    post,
    path = "/api/v1/agent/heartbeat",
    tag = "agent",
    security(("bearer" = [])),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "Noted", body = Empty),
        (status = 400, description = "Bad body, or an address that is not public IPv4", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid node token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The node is out of the pool: `{\"terminate\": true, ...}`", body = Terminate),
        (status = 410, description = "The registrar does not know the node; enroll again", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is larger than 256 KiB", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn heartbeat(
    State(app): State<AppState>,
    auth: NodeAuth,
    ClientIp(peer): ClientIp,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    let ip = node_address(peer, app.allow_private_ips)?;
    let reply = app
        .actor
        .call(Command::Heartbeat(mtpool_core::HeartbeatRequest {
            reply: ReplyId(0),
            node: auth.node_id,
            ip,
        }))
        .await
        .map_err(|_| actor_stopped())?;
    Ok(reply_response(reply))
}

/// The node leaves the pool.
#[utoipa::path(
    post,
    path = "/api/v1/agent/retire",
    tag = "agent",
    security(("bearer" = [])),
    request_body(content = EmptyRequest, description = "Empty, or `{}`"),
    responses(
        (status = 200, description = "The node is out of the pool", body = Empty),
        (status = 400, description = "Bad body", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid node token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The node was out of the pool already: `{\"terminate\": true, ...}`", body = Terminate),
        (status = 410, description = "The registrar does not know the node", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is larger than 256 KiB", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn retire(
    State(app): State<AppState>,
    auth: NodeAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    parse_empty(&body_bytes(body)?)?;
    let reply = app
        .actor
        .call(Command::Retire(NodeAction {
            reply: ReplyId(0),
            node: auth.node_id,
        }))
        .await
        .map_err(|_| actor_stopped())?;
    Ok(reply_response(reply))
}

// ----- report ----------------------------------------------------------------------------------

fn gp_abstain(reason: &str) -> AbstainReason {
    match reason {
        "rate_limited" => AbstainReason::RateLimited,
        "unavailable" => AbstainReason::Unavailable,
        "timeout" => AbstainReason::Timeout,
        _ => AbstainReason::NoData,
    }
}

fn mismatch(what: &str) -> Failure {
    problem(
        StatusCode::CONFLICT,
        "report_mismatch",
        "Report does not match the node",
        &format!("The reported {what} is not the one this node is registered with."),
    )
}

/// What a report hands to `core` once its Globalping part is settled.
struct Settled {
    node: NodeId,
    generation: Generation,
    metrics_ok: bool,
    globalping: Vote,
    traffic: Option<TrafficCounters>,
    clients: Option<u64>,
}

impl Settled {
    fn command(self) -> Command {
        Command::Report(mtpool_core::ReportRequest {
            reply: ReplyId(0),
            node: self.node,
            generation: self.generation,
            metrics_ok: self.metrics_ok,
            globalping: self.globalping,
            traffic: self.traffic,
            clients: self.clients,
        })
    }
}

/// Claims the measurement id: `false` when it was used before, by any node.
async fn claim_measurement(
    app: &AppState,
    id: String,
    node: NodeId,
    now: Ts,
) -> Result<bool, Failure> {
    let ledger = app.ledger.clone();
    let store = app.store.clone();
    let claimed = blocking(move || -> Result<bool, crate::store::StoreError> {
        // The ledger first: it knows the ids whose write is still queued.
        if !ledger.claim(&id, now) {
            return Ok(false);
        }
        if measurement_seen(&store.read(), &id)? {
            return Ok(false);
        }
        store.send(ReportOp::RecordMeasurement { id, node, at: now });
        Ok(true)
    })
    .await?;
    claimed.map_err(|e| {
        tracing::error!(error = %e, "agent API: cannot check the measurement id");
        ApiError::internal().into()
    })
}

/// Verifies the measurement and hands the report to `core` with the verified vote. Runs after
/// the agent got its answer: the measurement service may take seconds.
async fn settle_measured(
    app: AppState,
    settled: Settled,
    id: String,
    expect: GpExpect,
    _slot: VerifySlot,
) {
    let verified = tokio::time::timeout(VERIFY_TIMEOUT, app.verifier.verify(&id, &expect)).await;
    let vote = match verified {
        Ok(Ok(sensor_vote)) => sensor_vote.vote,
        Ok(Err(rejected)) => {
            let reason = match rejected {
                GpRejected::Unavailable => AbstainReason::Unavailable,
                GpRejected::UnknownMeasurement | GpRejected::Mismatch | GpRejected::Stale => {
                    AbstainReason::UnknownAnswer
                }
            };
            match rejected {
                // A node named a measurement that was not made against it.
                GpRejected::UnknownMeasurement | GpRejected::Mismatch => {
                    tracing::warn!(node = %settled.node, %rejected, "measurement refused")
                }
                GpRejected::Unavailable | GpRejected::Stale => {
                    tracing::info!(node = %settled.node, %rejected, "measurement not used")
                }
            }
            Vote::Abstain(reason)
        }
        Err(_) => {
            tracing::warn!(node = %settled.node, "measurement check timed out");
            Vote::Abstain(AbstainReason::Timeout)
        }
    };
    let settled = Settled {
        globalping: vote,
        ..settled
    };
    match app.actor.call(settled.command()).await {
        Ok(Reply::Ok) => {}
        Ok(other) => tracing::info!(reply = ?other, "verified report not applied"),
        Err(_) => tracing::warn!("verified report dropped: the state owner stopped"),
    }
}

/// The node's checks of the last round. A measured Globalping result is checked by the
/// registrar before it counts; the agent gets its answer without waiting for that.
#[utoipa::path(
    post,
    path = "/api/v1/agent/report",
    tag = "agent",
    security(("bearer" = [])),
    request_body(content = ReportRequest, description = "At most 256 KiB"),
    responses(
        (status = 200, description = "Accepted. A measurement is verified afterwards.", body = Empty),
        (status = 400, description = "Not the document this route takes (unknown fields included)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid node token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The node is out of the pool: `{\"terminate\": true, ...}`", body = Terminate),
        (status = 429, description = "A measurement of this node is still being verified; retry after the `Retry-After` seconds (`rate_limited`)", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the next measured report may be sent"))),
        (status = 409, description = "`clock_skew` (the report is dated outside the window of 10 minutes back and 1 minute ahead), `report_mismatch` (address, port or SNI are not the node's), `measurement_reused`, or `stale`", body = Problem, content_type = "application/problem+json"),
        (status = 410, description = "The registrar does not know the node; enroll again", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is larger than 256 KiB", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn report(
    State(app): State<AppState>,
    auth: NodeAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let request: ReportRequest = parse(&body_bytes(body)?)?;
    let Some(checked_at) = parse_rfc3339(&request.checked_at) else {
        return Err(ApiError::bad_request("`checked_at` is not an RFC 3339 timestamp.").into());
    };
    let node = live_node(&app, &auth.node_id).await?;

    let now = (app.clock)();
    let skew_ms = checked_at.saturating_sub(now.0);
    if !(-CHECKED_AT_PAST_MS..=CHECKED_AT_FUTURE_MS).contains(&skew_ms) {
        if app.ledger.skew_event_due(&auth.node_id, now) {
            let event = Event {
                kind: EventKind::ClockSkew,
                at: now,
                node: Some(auth.node_id.clone()),
                owner: None,
                domain: None,
                detail: json!({
                    "checked_at": rfc3339(checked_at),
                    "server_now": rfc3339(now.0),
                    "skew_sec": skew_ms / 1000,
                }),
            };
            // Raised here and not by the state owner, so it reaches the panels from here.
            app.events.publish(&event);
            app.store.send(event);
        }
        return Err(problem(
            StatusCode::CONFLICT,
            "clock_skew",
            "Clock skew",
            "The report is dated outside the accepted window; check the clock of the node.",
        ));
    }

    // What a node says about its address is checked against the registry: a node must not
    // vouch for another address.
    if request.ip != node.ip {
        return Err(mismatch("address"));
    }
    if request.port != node.port {
        return Err(mismatch("port"));
    }

    let settled = Settled {
        node: auth.node_id.clone(),
        generation: node.generation,
        metrics_ok: request.metrics.ok,
        globalping: Vote::Abstain(AbstainReason::NoData),
        traffic: request.metrics.ok.then_some(TrafficCounters {
            from_client: request.metrics.from_client_total,
            to_client: request.metrics.to_client_total,
        }),
        clients: request.metrics.clients,
    };
    match request.gp {
        GpReport::NoData { reason } => {
            let abstain = gp_abstain(&reason);
            // `NoData` is the usual report between measurements. The agent's own word stays out
            // of the log: only what the registrar knows goes there.
            if abstain != AbstainReason::NoData {
                tracing::warn!(
                    node = %auth.node_id,
                    reason = ?abstain,
                    "no Globalping measurement from the node"
                );
            }
            let settled = Settled {
                globalping: Vote::Abstain(abstain),
                ..settled
            };
            let reply = app
                .actor
                .call(settled.command())
                .await
                .map_err(|_| actor_stopped())?;
            Ok(reply_response(reply))
        }
        GpReport::Measured { id } => {
            if request.sni != node.tls_domain {
                return Err(mismatch("SNI"));
            }
            // The slot first: a report refused for being too soon burns no measurement id.
            let Some(slot) = app.ledger.take_slot(&auth.node_id) else {
                return Err(ApiError::rate_limited(VERIFY_BUSY_RETRY_SEC).into());
            };
            if !claim_measurement(&app, id.clone(), auth.node_id, now).await? {
                return Err(problem(
                    StatusCode::CONFLICT,
                    "measurement_reused",
                    "Measurement already used",
                    "This measurement was reported before; make a new one.",
                ));
            }
            let expect = GpExpect {
                ip: node.ip,
                port: node.port,
                sni: node.tls_domain,
                reported_at: Ts(checked_at),
            };
            tokio::spawn(settle_measured(app, settled, id, expect, slot));
            Ok(empty_ok())
        }
    }
}

// ----- config ----------------------------------------------------------------------------------

/// What the node's proxy has to be set up with.
#[utoipa::path(
    get,
    path = "/api/v1/agent/config",
    tag = "agent",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The proxy users, SNI, port and intervals", body = AgentConfig),
        (status = 401, description = "No valid node token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The node is out of the pool: `{\"terminate\": true, ...}`", body = Terminate),
        (status = 410, description = "The registrar does not know the node; enroll again", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn config(State(app): State<AppState>, auth: NodeAuth) -> Result<Response, Failure> {
    let id = auth.node_id;
    let seen = app
        .actor
        .inspect(move |state| {
            let node = state.nodes.get(&id)?;
            if let Some(reason) = terminal(node.state) {
                return Some(Err(reason));
            }
            let s = &state.settings;
            Some(Ok(AgentConfig {
                users: s
                    .proxy_users
                    .iter()
                    .map(|u| AgentUser {
                        name: u.name.clone(),
                        secret_hex: u.secret_hex.clone(),
                    })
                    .collect(),
                sni: s.tls_domain.clone(),
                port: s.proxy_port,
                intervals: Intervals {
                    heartbeat_sec: s.heartbeat_interval_sec,
                    globalping_sec: s.globalping_interval_sec,
                    metrics_sec: s.metrics_interval_sec,
                    sync_sec: s.sync_interval_sec,
                },
                antiscan: true,
            }))
        })
        .await
        .map_err(|_| actor_stopped())?;
    match seen {
        None => Err(unknown_node()),
        Some(Err(reason)) => Err(Failure(Box::new(terminate(reason)))),
        Some(Ok(config)) => Ok(json_response(
            StatusCode::OK,
            serde_json::to_string(&config).expect("an agent config serializes"),
        )),
    }
}

// ----- update --------------------------------------------------------------------------------

/// A node of the pool that may talk about updates: not unknown, not out of the pool.
async fn in_pool(app: &AppState, id: NodeId) -> Result<(), Failure> {
    let seen = app
        .actor
        .inspect(move |state| state.nodes.get(&id).map(|n| terminal(n.state)))
        .await
        .map_err(|_| actor_stopped())?;
    match seen {
        None => Err(unknown_node()),
        Some(Some(reason)) => Err(Failure(Box::new(terminate(reason)))),
        Some(None) => Ok(()),
    }
}

/// The agent tells which version it runs and learns whether to update.
#[utoipa::path(
    post,
    path = "/api/v1/agent/update",
    tag = "agent",
    security(("bearer" = [])),
    request_body = UpdatePoll,
    responses(
        (status = 200, description = "`{\"version\": \"x.y.z\"}` when the node is to update to it, `{}` when not", body = UpdateOffer),
        (status = 400, description = "Bad body or a version that is not x.y.z", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid node token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The node is out of the pool: `{\"terminate\": true, ...}`", body = Terminate),
        (status = 410, description = "The registrar does not know the node; enroll again", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is larger than 256 KiB", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn update(
    State(app): State<AppState>,
    auth: NodeAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    let poll: UpdatePoll = parse(&body_bytes(body)?)?;
    if mtpool_selfupdate::manifest::Version::parse(&poll.version).is_none() {
        return Err(ApiError::bad_request("version must be x.y.z.").into());
    }
    in_pool(&app, auth.node_id.clone()).await?;
    let now = (app.clock)();
    let offer = app.rollouts.poll(&auth.node_id, &poll.version, now);
    let version = if offer {
        app.rollouts.offered_version()
    } else {
        None
    };
    Ok(json_response(
        StatusCode::OK,
        serde_json::to_string(&UpdateOffer { version }).expect("an update offer serializes"),
    ))
}

/// The agent reports a step of its update.
#[utoipa::path(
    post,
    path = "/api/v1/agent/update-status",
    tag = "agent",
    security(("bearer" = [])),
    request_body = UpdateStatus,
    responses(
        (status = 200, description = "Noted (also when no rollout wants it)", body = Empty),
        (status = 400, description = "Bad body, a version that is not x.y.z, or `failed` without a valid `code`", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid node token", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The node is out of the pool: `{\"terminate\": true, ...}`", body = Terminate),
        (status = 410, description = "The registrar does not know the node; enroll again", body = Problem, content_type = "application/problem+json"),
        (status = 413, description = "The body is larger than 256 KiB", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn update_status(
    State(app): State<AppState>,
    auth: NodeAuth,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Failure> {
    use mtpool_core::rollout::Status;
    let report: UpdateStatus = parse(&body_bytes(body)?)?;
    if mtpool_selfupdate::manifest::Version::parse(&report.version).is_none() {
        return Err(ApiError::bad_request("version must be x.y.z.").into());
    }
    let status = match report.outcome {
        UpdateOutcome::Started => Status::Started,
        UpdateOutcome::Updated => Status::Updated,
        UpdateOutcome::RolledBack => Status::RolledBack,
        UpdateOutcome::Failed => {
            let code = report.code.as_deref().unwrap_or("");
            let plain = (1..=40).contains(&code.len())
                && code
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if !plain {
                return Err(ApiError::bad_request(
                    "failed needs a code of 1 to 40 characters: a-z, 0-9, underscore.",
                )
                .into());
            }
            Status::Failed {
                code: code.to_owned(),
            }
        }
    };
    in_pool(&app, auth.node_id.clone()).await?;
    app.rollouts
        .status(&auth.node_id, &report.version, &status, (app.clock)());
    Ok(empty_ok())
}
