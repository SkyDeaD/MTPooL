//! The public read-only API under `/api/v1`, its error shape and the OpenAPI document.
//!
//! Lists come from the published snapshot (JSON rendered once per change, `ETag` and 304);
//! bans, node rounds and statistics are read from the database.

use std::collections::HashMap;
use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::Query;
use axum::extract::rejection::QueryRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mtpool_protocol::problem::{CONTENT_TYPE as PROBLEM_CONTENT_TYPE, Problem};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::server::AppState;

pub mod admin;
pub mod agent;
pub mod bot;
pub mod public;
pub mod qr;
pub mod ratelimit;
pub mod rollout;
pub mod stats;
pub mod stream;
pub mod util;
pub mod webhook_url;

/// An error answer: `application/problem+json` with a stable `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    problem: Box<Problem>,
    retry_after: Option<u64>,
    www_authenticate: bool,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, title: &str, detail: &str) -> ApiError {
        ApiError {
            problem: Box::new(Problem::new(status.as_u16(), code, title, detail)),
            retry_after: None,
            www_authenticate: false,
        }
    }

    /// Adds `Retry-After: <seconds>`.
    pub fn retry_after(mut self, seconds: u64) -> ApiError {
        self.retry_after = Some(seconds);
        self
    }

    /// Adds `WWW-Authenticate: Bearer`.
    pub fn www_authenticate(mut self) -> ApiError {
        self.www_authenticate = true;
        self
    }

    pub fn bad_request(detail: &str) -> ApiError {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Bad request",
            detail,
        )
    }

    pub fn not_found(detail: &str) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "Not found", detail)
    }

    pub fn method_not_allowed() -> ApiError {
        ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Method not allowed",
            "This path does not accept that method.",
        )
    }

    pub fn rate_limited(retry_after_seconds: u64) -> ApiError {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many requests",
            "Too many requests from this address; try again later.",
        )
        .retry_after(retry_after_seconds)
    }

    pub fn internal() -> ApiError {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal error",
            "The request could not be processed.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_string(&*self.problem).expect("a problem serializes");
        let mut response = (status, body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(PROBLEM_CONTENT_TYPE),
        );
        if let Some(seconds) = self.retry_after {
            headers.insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if self.www_authenticate {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// Answer for a path no route has.
pub async fn not_found() -> ApiError {
    ApiError::not_found("There is nothing at this path.")
}

/// Answer for a known path asked with a method it does not have.
pub async fn method_not_allowed() -> ApiError {
    ApiError::method_not_allowed()
}

/// The query as a map. Unknown parameters are the handler's to ignore; a query that cannot be
/// decoded at all is a bad request.
pub(crate) type RawQuery = Result<Query<HashMap<String, String>>, QueryRejection>;

pub(crate) fn query_map(query: RawQuery) -> Result<HashMap<String, String>, ApiError> {
    query
        .map(|Query(map)| map)
        .map_err(|_| ApiError::bad_request("The query string cannot be decoded."))
}

/// Whether `If-None-Match` names the entity tag `etag` (quoted), or is `*`.
fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

/// A JSON answer with `ETag` (`tag` is the bare value) and 304 when the client has it.
pub(crate) fn tagged(
    headers: &HeaderMap,
    tag: &str,
    content_type: &'static str,
    body: String,
) -> Response {
    let etag = format!("\"{tag}\"");
    let value = HeaderValue::from_str(&etag).expect("a hex tag is a valid header value");
    let mut response = if matches_etag(headers, &etag) {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        r
    } else {
        let mut r = Response::new(Body::from(body));
        r.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        r
    };
    let out = response.headers_mut();
    out.insert(header::ETAG, value);
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// An untagged JSON answer.
pub(crate) fn json(body: String) -> Response {
    let mut response = Response::new(Body::from(body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "MTPooL API",
        version = "1",
        description = "The public read-only view of the proxy pool: nodes, domains, Telegram links, bans and statistics. No credentials needed; node addresses are masked and measurement ids are never shown. The agent routes under `/api/v1/agent`, the bot routes under `/api/v1/me` and the admin routes under `/api/v1/admin` take a bearer token. Errors are application/problem+json with a stable `code`."
    ),
    tags((name = "public", description = "Public read-only routes"))
)]
struct ApiDoc;

/// The routes of the public API, with their documentation.
pub fn public_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(public::list_nodes))
        .routes(routes!(public::get_node))
        .routes(routes!(public::get_node_history))
        .routes(routes!(public::list_domains))
        .routes(routes!(public::list_links))
        .routes(routes!(public::link_qr))
        .routes(routes!(public::list_bans))
        .routes(routes!(stats::summary))
        .routes(routes!(stats::series))
        .routes(routes!(stats::clients))
        .routes(routes!(public::openapi_document))
        .routes(routes!(crate::release::install_script))
        .routes(routes!(crate::release::release_file))
}

/// Every documented route: the public, agent, bot and admin APIs.
pub fn routes() -> OpenApiRouter<AppState> {
    public_routes()
        .merge(agent::routes())
        .merge(bot::routes())
        .merge(admin::routes())
        .merge(rollout::routes())
        .merge(crate::metrics::routes())
}

/// The OpenAPI document, pretty-printed: what `mtpool-registrar openapi` prints (plus a
/// newline) and `GET /api/v1/openapi.json` serves.
pub fn openapi_pretty() -> &'static str {
    static DOC: OnceLock<String> = OnceLock::new();
    DOC.get_or_init(|| {
        let (_, doc) = routes().split_for_parts();
        doc.to_pretty_json()
            .expect("the OpenAPI document serializes")
    })
}
