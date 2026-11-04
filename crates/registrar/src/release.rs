//! The node installer and the release files it downloads.
//!
//! `GET /install.sh` is `deploy/install.sh` with the pool's address, signing key and settings
//! written over its markers, so that one command (`curl ... | sh`) needs nothing else from the
//! operator. `GET /releases/{name}` serves what the installer fetches: the signed manifest and
//! the agent binaries, from the releases directory.

use std::path::{Path as FsPath, PathBuf};

use axum::body::Body;
use axum::extract::rejection::PathRejection;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mtpool_protocol::problem::Problem;
use std::net::SocketAddr;

use crate::api::ApiError;
use crate::server::AppState;

/// The public key releases are verified with, as built in (empty in a debug build made
/// without the key file).
pub const MINISIGN_PUB: &str = env!("MTPOOL_MINISIGN_PUB");

/// Where release files are looked for unless `MTPOOL_RELEASES_DIR` says otherwise.
pub const DEFAULT_RELEASES_DIR: &str = "/var/lib/mtpool/releases";

/// The longest release file name the route accepts.
/// The largest release file the route serves.
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;

const INSTALL_SCRIPT: &str = include_str!("../../../deploy/install.sh");
const AGENT_UNIT: &str = include_str!("../../../deploy/systemd/mtpool-agent.service");
const AGENT_ROLLBACK_UNIT: &str =
    include_str!("../../../deploy/systemd/mtpool-agent-rollback.service");
const TELEMT_UNIT: &str = include_str!("../../../deploy/systemd/telemt.service");
const TELEMT_TOML: &str = include_str!("../../../deploy/systemd/telemt.toml");

/// A host name, IPv4 address or bracketed IPv6 address, with an optional port: what may go
/// between the quotes of the installer without being able to leave them.
fn plain_host(host: &str) -> bool {
    let (name, port) = if let Some(rest) = host.strip_prefix('[') {
        let Some((inside, after)) = rest.split_once(']') else {
            return false;
        };
        if inside.is_empty() || !inside.bytes().all(|b| b.is_ascii_hexdigit() || b == b':') {
            return false;
        }
        (inside, after.strip_prefix(':'))
    } else {
        match host.split_once(':') {
            Some((name, port)) => (name, Some(port)),
            None => (host, None),
        }
    };
    let name_ok = !name.is_empty()
        && name.len() <= 253
        && (host.starts_with('[')
            || name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'));
    let port_ok =
        port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()));
    name_ok && port_ok
}

/// The TLS domain of the pool as it may be written into the installer: a host name, or nothing
/// yet.
fn plain_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

/// The scheme of the pool's address: the proxy's word when the peer is the local proxy, else
/// the request's own.
fn scheme(request: &Request) -> Result<&'static str, ApiError> {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let forwarded = peer.is_some_and(|ip| ip.is_loopback());
    let given = if forwarded {
        request
            .headers()
            .get_all("x-forwarded-proto")
            .iter()
            .next_back()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .map(|v| v.trim().to_ascii_lowercase())
    } else {
        request.uri().scheme_str().map(str::to_ascii_lowercase)
    };
    match given.as_deref() {
        None | Some("http") => Ok("http"),
        Some("https") => Ok("https"),
        Some(_) => Err(ApiError::bad_request(
            "The request scheme is not http or https.",
        )),
    }
}

fn host(request: &Request) -> Result<String, ApiError> {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_owned()))
        .unwrap_or_default();
    if plain_host(&host) {
        Ok(host)
    } else {
        Err(ApiError::bad_request(
            "The Host header is missing or is not a plain host name.",
        ))
    }
}

/// The installer for `url` with the key and settings written in.
fn render(url: &str, key: &str, port: u16, tls_domain: &str) -> String {
    let file = |text: &str| text.trim_end_matches('\n').to_owned();
    INSTALL_SCRIPT
        .replace("__MTPOOL_URL__", url)
        .replace("__MTPOOL_PUBKEY__", key)
        .replace("__MTPOOL_PROXY_PORT__", &port.to_string())
        .replace("__MTPOOL_TLS_DOMAIN__", tls_domain)
        .replace("__MTPOOL_TELEMT_TOML__", &file(TELEMT_TOML))
        .replace("__MTPOOL_TELEMT_UNIT__", &file(TELEMT_UNIT))
        .replace("__MTPOOL_AGENT_UNIT__", &file(AGENT_UNIT))
        .replace("__MTPOOL_AGENT_ROLLBACK_UNIT__", &file(AGENT_ROLLBACK_UNIT))
}

#[utoipa::path(
    get,
    path = "/install.sh",
    tag = "public",
    responses(
        (status = 200, description = "The node installer, a POSIX shell script with this pool's address, signing key and proxy port written in. Run it as `curl -fsSL <pool>/install.sh | MTPOOL_TOKEN=<token> sh -s -- --name <name>`.",
            content_type = "text/plain; charset=utf-8", body = String),
        (status = 400, description = "The Host header or the forwarded scheme is not usable", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
        (status = 503, description = "This build has no signing key, so it cannot hand out an installer", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn install_script(
    State(app): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    if app.minisign_pub.is_empty() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_configured",
            "No signing key",
            "This registrar was built without a release signing key, so it cannot hand out an installer.",
        ));
    }
    let url = format!("{}://{}", scheme(&request)?, host(&request)?);
    let (port, tls_domain) = app
        .actor
        .inspect(|state| (state.settings.proxy_port, state.settings.tls_domain.clone()))
        .await
        .map_err(|_| ApiError::internal())?;
    if !plain_domain(&tls_domain) {
        tracing::error!(
            "the settings hold a TLS domain that is not a host name; the installer is not served"
        );
        return Err(ApiError::internal());
    }
    let script = render(&url, app.minisign_pub, port, &tls_domain);
    let mut response = script.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(response)
}

/// A release file name: letters, digits, dot, dash and underscore, not made of dots alone.
pub(crate) fn valid_name(name: &str) -> bool {
    mtpool_selfupdate::valid_file_name(name)
}

fn content_type(name: &str) -> &'static str {
    if name.ends_with(".json") {
        "application/json"
    } else if name.ends_with(".minisig") {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

/// The file `name` of `dir`, when it is a plain file (not a link, not a directory).
fn read_release(dir: &FsPath, name: &str) -> Option<Vec<u8>> {
    let path: PathBuf = dir.join(name);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

#[utoipa::path(
    get,
    path = "/releases/{name}",
    tag = "public",
    params(("name" = String, Path, description = "File name: `manifest.json`, `manifest.json.minisig` or an agent binary", example = "manifest.json")),
    responses(
        (status = 200, description = "The release file", content_type = "application/octet-stream", body = Vec<u8>,
            headers(("Cache-Control" = String, description = "Always `no-cache`: a new release replaces the file under the same name"))),
        (status = 404, description = "No such release file", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn release_file(
    State(app): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let unknown = || ApiError::not_found("No such release file.");
    let Ok(Path(name)) = name else {
        return Err(unknown());
    };
    if !valid_name(&name) {
        return Err(unknown());
    }
    let dir = app.releases_dir.clone();
    let wanted = name.clone();
    let bytes = tokio::task::spawn_blocking(move || read_release(&dir, &wanted))
        .await
        .map_err(|_| ApiError::internal())?
        .ok_or_else(unknown)?;
    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&name)),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_that_are_plain_pass_and_the_rest_do_not() {
        for ok in [
            "pool.example.test",
            "pool.example.test:8080",
            "10.0.0.1",
            "[::1]:8080",
            "[2001:db8::1]",
        ] {
            assert!(plain_host(ok), "{ok}");
        }
        for bad in [
            "", "a b", "a'b", "a/b", "a:b", "a:", "a:123456", "[::1", "[]", "[zz]", "$(x)", "a`b`",
            "a;b", "a\\b", "a\"b",
        ] {
            assert!(!plain_host(bad), "{bad}");
        }
    }

    #[test]
    fn names_of_dots_alone_are_not_release_names() {
        for bad in ["", ".", "..", "...", "a/b", "a b", "ä"] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        for ok in ["manifest.json", "mtpool-agent-x86_64", "a..b", ".hidden"] {
            assert!(valid_name(ok), "{ok}");
        }
    }

    #[test]
    fn the_embedded_installer_has_every_marker_the_renderer_fills() {
        for marker in [
            "__MTPOOL_URL__",
            "__MTPOOL_PUBKEY__",
            "__MTPOOL_PROXY_PORT__",
            "__MTPOOL_TLS_DOMAIN__",
            "__MTPOOL_TELEMT_TOML__",
            "__MTPOOL_TELEMT_UNIT__",
            "__MTPOOL_AGENT_UNIT__",
            "__MTPOOL_AGENT_ROLLBACK_UNIT__",
        ] {
            assert!(INSTALL_SCRIPT.contains(marker), "{marker}");
        }
        let out = render("http://x", "K", 1, "d");
        assert!(!out.contains("__MTPOOL_"));
    }
}
