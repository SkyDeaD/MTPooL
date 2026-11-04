//! HTTP server and the service lifecycle: start, serve, stop.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use listenfd::ListenFd;
use tokio::net::TcpListener;
use tokio::sync::{Mutex as AsyncMutex, oneshot};
use tower_http::limit::RequestBodyLimitLayer;

use crate::actor::{Actor, ActorHandle, Clock, EffectDriver, system_clock};
use crate::api;
use crate::api::agent::Ledger;
use crate::api::live::LiveFan;
use crate::api::ratelimit::{self as request_limit, RequestLimiter};
use crate::api::stats::StatsCache;
use crate::api::stream::EventFan;
use crate::api::webhook_url::{Resolver, SystemResolver};
use crate::auth::{AuthState, session};
use crate::config::{self, Config, ConfigError, Endpoints};
use crate::dns::cloudflare::CloudflareDns;
use crate::dns::worker::DnsWorker;
use crate::prober::Prober;
use crate::release;
use crate::secrets;
use crate::sensors::Sensor;
use crate::sensors::cheburcheck::CheburcheckSensor;
use crate::sensors::globalping::{
    GlobalpingClient, GlobalpingSensor, GlobalpingVerifier, SharedSettings, TokenSource,
};
use crate::sensors::redflag::RedFlagSensor;
use crate::sensors::round::RoundRunner;
use crate::snapshot::SnapshotStore;
use crate::store::{Store, StoreError, WriteHealth};
use crate::verifier::{MeasurementVerifier, NoVerifier};
use crate::webhooks::{WebhookFanOut, WebhookSender};
use arc_swap::ArcSwap;
use mtpool_core::{GlobalpingSettings, NodeId, Ts};

/// Largest request body any route accepts.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub actor: ActorHandle,
    pub snapshots: SnapshotStore,
    pub auth: AuthState,
    pub clock: Clock,
    pub limiter: Arc<RequestLimiter>,
    pub stats: StatsCache,
    /// Checks the measurements agents name in their reports.
    pub verifier: Arc<dyn MeasurementVerifier>,
    /// The Globalping part of the settings, which the verifier and the sensor read on every
    /// call. The settings route stores a change here once the state owner has it.
    pub gp_settings: SharedSettings,
    /// The measurement ids claimed lately, before they reach the database.
    pub ledger: Arc<Ledger>,
    /// The live event stream of the panel.
    pub events: EventFan,
    pub live: LiveFan,
    /// Resolves the host of a webhook URL.
    pub resolver: Arc<dyn Resolver>,
    /// Held while the settings are read, changed and handed to the state owner, so that two
    /// edits do not overwrite each other.
    pub settings_lock: Arc<AsyncMutex<()>>,
    /// When a re-check of a node was last asked for through the bot API (kept in memory).
    pub rechecks: Arc<Mutex<HashMap<NodeId, Ts>>>,
    /// Nodes may enroll from private, loopback and shared addresses. Set once at startup from
    /// `MTPOOL_ALLOW_PRIVATE_IPS`; only a test stand wants it.
    pub allow_private_ips: bool,
    /// Where `/releases/{name}` reads from.
    pub releases_dir: Arc<PathBuf>,
    /// The key releases are signed with, as served in `/install.sh`; empty when this build has
    /// none (then `/install.sh` says so).
    pub minisign_pub: &'static str,
    /// The rollout of agent releases.
    pub rollouts: crate::rollout::RolloutService,
}

impl AppState {
    pub fn new(store: Store, actor: ActorHandle, snapshots: SnapshotStore) -> AppState {
        AppState::with_clock(store, actor, snapshots, system_clock())
    }

    pub fn with_clock(
        store: Store,
        actor: ActorHandle,
        snapshots: SnapshotStore,
        clock: Clock,
    ) -> AppState {
        let rollouts = crate::rollout::RolloutService::load(store.clone(), clock.clone())
            .unwrap_or_else(|e| {
                tracing::error!(error = %e, "cannot read the saved rollouts, starting without");
                crate::rollout::RolloutService::empty(store.clone(), clock.clone())
            });
        AppState {
            rollouts,
            store,
            actor,
            snapshots,
            auth: AuthState::new(),
            clock,
            limiter: Arc::new(RequestLimiter::new()),
            stats: StatsCache::new(),
            verifier: Arc::new(NoVerifier),
            gp_settings: Arc::new(ArcSwap::from_pointee(GlobalpingSettings::default())),
            ledger: Arc::new(Ledger::new()),
            events: EventFan::new(),
            live: LiveFan::new(),
            resolver: Arc::new(SystemResolver),
            settings_lock: Arc::new(AsyncMutex::new(())),
            rechecks: Arc::new(Mutex::new(HashMap::new())),
            allow_private_ips: false,
            releases_dir: Arc::new(PathBuf::from(release::DEFAULT_RELEASES_DIR)),
            minisign_pub: release::MINISIGN_PUB,
        }
    }

    /// The same state with `events` as the fan the state owner feeds.
    pub fn with_events(mut self, events: EventFan) -> AppState {
        self.events = events;
        self
    }

    /// The same state with `live` as the fan of the public pages.
    pub fn with_live(mut self, live: LiveFan) -> AppState {
        self.live = live;
        self
    }

    /// The same state with `resolver` looking up the hosts of webhook URLs.
    pub fn with_resolver(mut self, resolver: Arc<dyn Resolver>) -> AppState {
        self.resolver = resolver;
        self
    }

    /// The same state with `gp_settings` as the Globalping settings the settings route updates.
    pub fn with_gp_settings(mut self, gp_settings: SharedSettings) -> AppState {
        self.gp_settings = gp_settings;
        self
    }

    /// The same state with private node addresses allowed or not.
    pub fn with_allow_private_ips(mut self, allow: bool) -> AppState {
        self.allow_private_ips = allow;
        self
    }

    /// The same state with release files read from `dir`.
    pub fn with_releases_dir(mut self, dir: PathBuf) -> AppState {
        self.releases_dir = Arc::new(dir);
        self
    }

    /// The same state serving `key` as the release signing key.
    pub fn with_minisign_pub(mut self, key: &'static str) -> AppState {
        self.minisign_pub = key;
        self
    }

    /// The same state with `verifier` checking the agents' measurements.
    pub fn with_verifier(mut self, verifier: Arc<dyn MeasurementVerifier>) -> AppState {
        self.verifier = verifier;
        self
    }
}

pub fn router(state: AppState) -> Router {
    // The request limit covers the public routes only, not the probes or the admin session.
    let (public, _) = api::public_routes()
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_limit::limit,
        ))
        .split_for_parts();
    // The agent routes are not behind it: agents are held to the failed-attempt limit.
    let (agent, _) = api::agent::routes().split_for_parts();
    // The bot and admin routes take a token of their own kind and are not behind it either.
    let (bot, _) = api::bot::routes().split_for_parts();
    let (admin, _) = api::admin::routes().split_for_parts();
    let (rollouts, _) = api::rollout::routes().split_for_parts();
    let (session, _) = session::routes().split_for_parts();
    // /metrics takes a metrics token and sits outside the API prefix.
    let (metrics, _) = crate::metrics::routes().split_for_parts();
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .merge(session)
        .merge(public)
        .merge(agent)
        .merge(bot)
        .merge(admin)
        .merge(rollouts)
        .merge(metrics)
        .fallback(crate::assets::fallback)
        .method_not_allowed_fallback(api::method_not_allowed)
        .with_state(state)
        // Outermost: the limit also covers routes that do not exist.
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(middleware::map_response(too_large_as_problem))
}

/// The body limit answers with a bare 413; every error of this API is a problem.
async fn too_large_as_problem(response: Response) -> Response {
    let plain = response.status() == StatusCode::PAYLOAD_TOO_LARGE
        && !response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with(mtpool_protocol::problem::CONTENT_TYPE));
    if plain {
        api::agent::payload_too_large().into_response()
    } else {
        response
    }
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
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("cannot listen: {0}")]
    Listen(io::Error),
    #[error("server error: {0}")]
    Serve(io::Error),
    #[error("cannot build the HTTP client: {0}")]
    HttpClient(reqwest::Error),
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

/// How long requests in flight may take after the stop signal. Past it the service stops
/// anyway: flushing the store never waits on an open connection.
pub const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// `run_with` and the standard drain deadline.
pub async fn run(
    store: Store,
    listener: TcpListener,
    drivers: Vec<Arc<dyn EffectDriver>>,
    stop: impl Future<Output = ()> + Send + 'static,
) -> Result<(), RunError> {
    run_with(store, listener, drivers, stop, DRAIN_DEADLINE).await
}

/// Runs the service on `listener` until `stop` completes: restore the state, start the actor,
/// serve, and on stop wait up to `drain_deadline` for requests in flight, stop the actor and
/// flush the store.
pub async fn run_with(
    store: Store,
    listener: TcpListener,
    drivers: Vec<Arc<dyn EffectDriver>>,
    stop: impl Future<Output = ()> + Send + 'static,
    drain_deadline: Duration,
) -> Result<(), RunError> {
    let endpoints = Endpoints::from_env()?;
    let allow_private_ips = config::allow_private_ips(|name| std::env::var(name).ok());
    if allow_private_ips {
        tracing::warn!(
            "MTPOOL_ALLOW_PRIVATE_IPS is on: nodes on private addresses may enroll. \
             This is for a test stand, never for a public registrar"
        );
    }
    let releases_dir = config::releases_dir(|name| std::env::var(name).ok());
    let loader = store.clone();
    let state = tokio::task::spawn_blocking(move || loader.load()).await??;
    // The fan is a driver of the state owner: every emitted event reaches the open streams.
    let fan = EventFan::new();
    let mut drivers = drivers;
    drivers.push(Arc::new(fan.clone()));
    let live = LiveFan::new();
    drivers.push(Arc::new(live.clone()));
    // The token is asked for on every request: a change in the panel applies at once.
    let gp_settings: SharedSettings =
        Arc::new(ArcSwap::from_pointee(state.settings.globalping.clone()));
    let token_store = store.clone();
    let token: TokenSource = Arc::new(move || secrets::secret(&token_store, "globalping_token"));
    let globalping =
        GlobalpingClient::new(endpoints.globalping, token).map_err(RunError::HttpClient)?;
    let verifier = GlobalpingVerifier::new(globalping.clone(), gp_settings.clone());
    // The three sensors of a round. RedFlag's key is asked for on every call too.
    let redflag_store = store.clone();
    let redflag_key: TokenSource =
        Arc::new(move || secrets::secret(&redflag_store, "redflag_api_key"));
    let sensors: Vec<Arc<dyn Sensor>> = vec![
        Arc::new(GlobalpingSensor::new(globalping, gp_settings.clone())),
        Arc::new(CheburcheckSensor::new(endpoints.cheburcheck).map_err(RunError::HttpClient)?),
        Arc::new(RedFlagSensor::new(endpoints.redflag, redflag_key).map_err(RunError::HttpClient)?),
    ];
    drivers.push(Arc::new(RoundRunner::new(sensors)));
    // DNS: the Cloudflare token is asked for on every request, like the others.
    let dns_store = store.clone();
    let dns_token: TokenSource =
        Arc::new(move || secrets::secret(&dns_store, "cloudflare_api_token"));
    let dns = CloudflareDns::new(endpoints.cloudflare, dns_token).map_err(RunError::HttpClient)?;
    drivers.push(Arc::new(DnsWorker::new(Arc::new(dns))));
    // Webhooks: every event becomes delivery rows; the sender posts what is due.
    drivers.push(Arc::new(WebhookFanOut::new(store.clone())));
    let actor = Actor::spawn(state, store.clone(), drivers);
    // The prober and the sender end on their own when the state owner is gone; the handles let
    // the shutdown cut a round that is still running.
    let mut workers = vec![
        tokio::spawn(Prober::new().run(actor.clone())),
        tokio::spawn(
            WebhookSender::new(store.clone(), Arc::new(SystemResolver), system_clock())
                .run(actor.clone()),
        ),
        tokio::spawn(live.clone().run(actor.clone(), system_clock())),
        tokio::spawn(crate::clients::run(
            actor.clone(),
            store.clone(),
            system_clock(),
        )),
        tokio::spawn(crate::clients::run_availability(
            actor.clone(),
            store.clone(),
            system_clock(),
        )),
        tokio::spawn(crate::clients::run_retention(
            actor.clone(),
            store.clone(),
            system_clock(),
        )),
    ];
    // The poller only fetches and checks files; it has no say over the state owner.
    workers.extend(crate::update::spawn_poller(store.clone()));
    let state = AppState::new(store.clone(), actor.clone(), actor.snapshots())
        .with_events(fan.clone())
        .with_live(live.clone())
        .with_gp_settings(gp_settings)
        .with_allow_private_ips(allow_private_ips)
        .with_releases_dir(releases_dir)
        .with_verifier(Arc::new(verifier));
    // The rollout worker only reads the state owner; the agents' polls reach the rollout
    // through the handlers.
    workers.push(tokio::spawn(crate::rollout::run(
        state.rollouts.clone(),
        actor.clone(),
        state.clock.clone(),
        state.releases_dir.clone(),
        state.minisign_pub,
    )));
    let app = router(state);
    if let Ok(addr) = listener.local_addr() {
        tracing::info!(%addr, "listening");
    }
    // The peer address is what tells whether `X-Forwarded-For` comes from the local proxy.
    let (stop_fired, stop_seen) = oneshot::channel::<()>();
    let closing = fan.clone();
    let stop = async move {
        stop.await;
        // Open event streams would hold the drain until its deadline.
        closing.close();
        live.close();
        let _ = stop_fired.send(());
    };
    let serving = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(stop)
    .into_future();
    let drain_expired = async move {
        if stop_seen.await.is_ok() {
            tokio::time::sleep(drain_deadline).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let served = tokio::select! {
        result = serving => result.map_err(RunError::Serve),
        () = drain_expired => {
            tracing::warn!(
                deadline_secs = drain_deadline.as_secs_f32(),
                "requests still open at the drain deadline, stopping anyway"
            );
            Ok(())
        }
    };
    tracing::info!("stopping");
    let stored = shutdown(&actor, &store).await;
    for worker in &workers {
        worker.abort();
    }
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
