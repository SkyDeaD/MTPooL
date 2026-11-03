//! HTTP server and the service lifecycle: start, serve, stop.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use listenfd::ListenFd;
use tokio::net::TcpListener;
use tower_http::limit::RequestBodyLimitLayer;

use crate::actor::{Actor, ActorHandle, EffectDriver};
use crate::auth::{AuthState, session};
use crate::config::Config;
use crate::snapshot::SnapshotStore;
use crate::store::{Store, StoreError, WriteHealth};

/// Largest request body any route accepts.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub actor: ActorHandle,
    pub snapshots: SnapshotStore,
    pub auth: AuthState,
}

impl AppState {
    pub fn new(store: Store, actor: ActorHandle, snapshots: SnapshotStore) -> AppState {
        AppState {
            store,
            actor,
            snapshots,
            auth: AuthState::new(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .merge(session::routes())
        .with_state(state)
        // Outermost: the limit also covers routes that do not exist.
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
}

async fn healthz() -> &'static str {
    "ok"
}

/// 503 while the disk does not keep up with the state (sticky until restart) or the state
/// owner is gone.
async fn readyz(State(app): State<AppState>) -> Response {
    if let WriteHealth::Failed { error, .. } = app.store.health() {
        tracing::error!(%error, "not ready: store writes failed");
        return (StatusCode::SERVICE_UNAVAILABLE, "store write failed").into_response();
    }
    if app.actor.is_stopped() {
        return (StatusCode::SERVICE_UNAVAILABLE, "state owner stopped").into_response();
    }
    (StatusCode::OK, "ok").into_response()
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot listen: {0}")]
    Listen(io::Error),
    #[error("server error: {0}")]
    Serve(io::Error),
    #[error("a background task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// The listening socket: the one passed in through `listenfd` (socket activation) or else
/// `config.listen`.
pub async fn bind(config: &Config) -> io::Result<TcpListener> {
    if let Some(inherited) = ListenFd::from_env().take_tcp_listener(0)? {
        inherited.set_nonblocking(true)?;
        return TcpListener::from_std(inherited);
    }
    TcpListener::bind(config.listen).await
}

/// Stops the actor (after the commands already queued), then writes everything to disk and
/// stops the store writer.
pub async fn shutdown(actor: &ActorHandle, store: &Store) -> Result<(), StoreError> {
    actor.stop().await;
    let store = store.clone();
    tokio::task::spawn_blocking(move || {
        let flushed = store.flush();
        let closed = store.close();
        flushed.and(closed)
    })
    .await
    .unwrap_or(Err(StoreError::WriterStopped))
}

/// Runs the service on `listener` until `stop` completes: restore the state, start the actor,
/// serve, and on stop wait for requests in flight, stop the actor and flush the store.
pub async fn run(
    store: Store,
    listener: TcpListener,
    drivers: Vec<Arc<dyn EffectDriver>>,
    stop: impl Future<Output = ()> + Send + 'static,
) -> Result<(), RunError> {
    let loader = store.clone();
    let state = tokio::task::spawn_blocking(move || loader.load()).await??;
    let actor = Actor::spawn(state, store.clone(), drivers);
    let app = router(AppState::new(
        store.clone(),
        actor.clone(),
        actor.snapshots(),
    ));
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(%addr, "listening");
    }
    // The peer address is what tells whether `X-Forwarded-For` comes from the local proxy.
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stop)
    .await
    .map_err(RunError::Serve);
    tracing::info!("stopping");
    let stored = shutdown(&actor, &store).await;
    served?;
    stored?;
    Ok(())
}

/// Resolves on SIGTERM or SIGINT.
pub async fn termination_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::error!("cannot install signal handlers");
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
