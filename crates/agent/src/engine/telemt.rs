//! The telemt adapter. telemt keeps its settings in `/etc/telemt/telemt.toml`, serves Prometheus
//! text on `/metrics` and (if the file says so) a JSON control API on the loopback. The pool's
//! users, the TLS domain and the port are written into that file; telemt reads them when it
//! starts, so a change is a restart that is rolled back if telemt does not come up.
//!
//! Everything the adapter does on the host goes through [`CommandRunner`] (`systemctl`), [`Fs`]
//! (the files) and [`HttpGet`] (the loopback), so tests script all three.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mtpool_protocol::agent::{AgentConfig, Dc2Report, MetricsReport};
use tokio::time::Instant;
use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use super::dc2;
use super::http::HttpGet;
use super::prom::{self, Sample, parse_prometheus};
use super::{ApplyOutcome, DesiredConfig, Engine, EngineError, EngineHealth, EngineKind, Result};
use crate::host::CommandRunner;
use crate::lifecycle::{ConfigSink, DetectedEngine, EngineProbe};
use crate::state_files::Fs;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/telemt/telemt.toml";
pub const DEFAULT_UNIT: &str = "telemt.service";
/// The file in the state directory with the names of the users the pool wrote into the engine's
/// configuration. Only those are ever removed from it.
pub const POOL_USERS_FILE: &str = "pool_users.json";

/// How long a restarted telemt has to answer its metrics.
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const READY_POLL: Duration = Duration::from_secs(2);
/// telemt's own default for `[server] port`.
const DEFAULT_PORT: u16 = 443;
/// telemt has no default metrics port: without `metrics_listen` or `metrics_port` it starts no
/// metrics listener at all. The agent therefore writes `metrics_listen` itself (on the loopback
/// of this port) when the file has neither, and reads from the same address.
const DEFAULT_METRICS_PORT: u16 = 9090;
const DEFAULT_API_LISTEN: &str = "0.0.0.0:9091";

pub const REASON_USER_TELEMETRY_DISABLED: &str = "user_telemetry_disabled";
pub const REASON_METRICS_UNREACHABLE: &str = "metrics_unreachable";
pub const REASON_NO_ME_WRITERS: &str = "no_me_writers";
pub const REASON_CONFIG_UNREADABLE: &str = "config_unreadable";

/// The control API of telemt, as the configuration describes it.
#[derive(Clone, PartialEq, Eq)]
pub struct ControlApi {
    /// `http://127.0.0.1:9091`, no trailing slash.
    pub base: String,
    /// The exact `Authorization` header value telemt wants, if it wants one.
    pub authorization: Option<String>,
}

// The header is a secret: it is never printed.
impl std::fmt::Debug for ControlApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlApi")
            .field("base", &self.base)
            .field(
                "authorization",
                &self.authorization.as_ref().map(|_| "<set>"),
            )
            .finish()
    }
}

/// `telemt.toml`, read for what the agent needs from it.
#[derive(Clone)]
pub struct TelemtToml {
    doc: DocumentMut,
}

// The users' secrets are in the file: nothing of it is printed.
impl std::fmt::Debug for TelemtToml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TelemtToml { .. }")
    }
}

impl TelemtToml {
    /// A syntax error is reported with its place, never with the text around it, which may hold
    /// secrets.
    pub fn parse(text: &str) -> Result<TelemtToml> {
        let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
            EngineError::Config(format!(
                "telemt.toml is not valid TOML: {} (at bytes {:?})",
                e.message(),
                e.span()
            ))
        })?;
        Ok(TelemtToml { doc })
    }

    fn table(&self, path: &[&str]) -> Option<&dyn TableLike> {
        let mut table: &dyn TableLike = self.doc.as_table();
        for key in path {
            table = table.get(key)?.as_table_like()?;
        }
        Some(table)
    }

    fn value_of(&self, path: &[&str], key: &str) -> Option<&Item> {
        self.table(path)?.get(key)
    }

    /// `[server] port`, telemt's 443 if absent.
    pub fn port(&self) -> u16 {
        self.value_of(&["server"], "port")
            .and_then(Item::as_integer)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .unwrap_or(DEFAULT_PORT)
    }

    /// `[censorship] tls_domain`.
    pub fn tls_domain(&self) -> Option<String> {
        self.value_of(&["censorship"], "tls_domain")
            .and_then(Item::as_str)
            .map(str::to_owned)
    }

    /// The TLS domains: `[censorship] tls_domain` first, then the entries of `tls_domains`, each
    /// once.
    pub fn tls_domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = self.tls_domain().into_iter().collect();
        let listed = self
            .value_of(&["censorship"], "tls_domains")
            .and_then(Item::as_array)
            .into_iter()
            .flat_map(|array| array.iter().filter_map(Value::as_str));
        for domain in listed {
            if !domains.iter().any(|known| known == domain) {
                domains.push(domain.to_owned());
            }
        }
        domains
    }

    /// `[access.users]`: name and secret, in the order of the file.
    pub fn users(&self) -> Vec<(String, String)> {
        self.table(&["access", "users"])
            .map(|users| {
                users
                    .iter()
                    .filter_map(|(name, item)| Some((name.to_owned(), item.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The control API. telemt serves it by default (on `0.0.0.0:9091`, no header check), also
    /// when the file has no `[server.api]` (or its older name `[server.admin_api]`) at all; only
    /// `enabled = false` switches it off.
    pub fn control_api(&self) -> Option<ControlApi> {
        let table = self
            .table(&["server", "api"])
            .or_else(|| self.table(&["server", "admin_api"]));
        if table.and_then(|t| t.get("enabled")).and_then(Item::as_bool) == Some(false) {
            return None;
        }
        let listen = table
            .and_then(|t| t.get("listen"))
            .and_then(Item::as_str)
            .and_then(|listen| listen.parse::<SocketAddr>().ok())
            .unwrap_or_else(|| {
                DEFAULT_API_LISTEN
                    .parse()
                    .expect("the default is an address")
            });
        let authorization = table
            .and_then(|t| t.get("auth_header"))
            .and_then(Item::as_str)
            .filter(|header| !header.is_empty())
            .map(str::to_owned);
        Some(ControlApi {
            base: format!("http://{}", reachable(listen)),
            authorization,
        })
    }

    /// `[general.telemetry] user_enabled`, on if absent.
    pub fn user_telemetry_enabled(&self) -> bool {
        self.value_of(&["general", "telemetry"], "user_enabled")
            .and_then(Item::as_bool)
            .unwrap_or(true)
    }

    /// `[general] use_middle_proxy`. Absent is taken as on.
    pub fn uses_middle_proxy(&self) -> bool {
        self.value_of(&["general"], "use_middle_proxy")
            .and_then(Item::as_bool)
            .unwrap_or(true)
    }
}

/// The address to reach a listener on this host: a wildcard bind is reached on the loopback.
fn reachable(addr: SocketAddr) -> SocketAddr {
    if !addr.ip().is_unspecified() {
        return addr;
    }
    let ip = match addr.ip() {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
    };
    SocketAddr::new(ip, addr.port())
}

/// Where telemt serves `/metrics`: `[server] metrics_listen`, else `metrics_port` on the
/// loopback, else port 9090 on the loopback, which is the listener `apply_config` writes into a
/// file that names none.
pub fn metrics_url(config: &TelemtToml) -> String {
    let listen = config
        .value_of(&["server"], "metrics_listen")
        .and_then(Item::as_str)
        .and_then(|listen| listen.parse::<SocketAddr>().ok());
    let addr = listen.map(reachable).unwrap_or_else(|| {
        let port = config
            .value_of(&["server"], "metrics_port")
            .and_then(Item::as_integer)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .unwrap_or(DEFAULT_METRICS_PORT);
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
    });
    format!("http://{addr}/metrics")
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// What one reading of `/metrics` says.
struct Snapshot {
    report: MetricsReport,
    user_telemetry_off: bool,
}

pub struct TelemtEngine<R, H> {
    runner: Arc<R>,
    http: Arc<H>,
    fs: Arc<dyn Fs>,
    config_path: PathBuf,
    unit: String,
    state_dir: PathBuf,
    clock: fn() -> u64,
}

impl<R: CommandRunner, H: HttpGet> TelemtEngine<R, H> {
    pub fn new(
        config_path: PathBuf,
        unit: impl Into<String>,
        state_dir: PathBuf,
        runner: Arc<R>,
        fs: Arc<dyn Fs>,
        http: Arc<H>,
    ) -> Self {
        TelemtEngine {
            runner,
            http,
            fs,
            config_path,
            unit: unit.into(),
            state_dir,
            clock: now_secs,
        }
    }

    /// Replaces the clock (seconds since the epoch) the DC2 check measures the age of the
    /// engine's snapshot with.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> u64) -> Self {
        self.clock = clock;
        self
    }

    fn read_file(&self, path: &Path) -> Result<Option<String>> {
        let bytes = self.fs.read(path).map_err(|source| EngineError::Io {
            what: format!("cannot read {}", path.display()),
            source,
        })?;
        bytes
            .map(|bytes| {
                String::from_utf8(bytes).map_err(|_| {
                    EngineError::Config(format!("{} is not UTF-8 text", path.display()))
                })
            })
            .transpose()
    }

    /// The configuration text and what the agent reads from it.
    pub(crate) fn load(&self) -> Result<(String, TelemtToml)> {
        let text = self.read_file(&self.config_path)?.ok_or_else(|| {
            EngineError::Config(format!("{} does not exist", self.config_path.display()))
        })?;
        let parsed = TelemtToml::parse(&text)?;
        Ok((text, parsed))
    }

    fn pool_users_path(&self) -> PathBuf {
        self.state_dir.join(POOL_USERS_FILE)
    }

    /// The names the pool wrote before. A list that cannot be read is an empty one: the pool
    /// then removes nothing, it does not guess.
    pub(crate) fn pool_users(&self) -> Vec<String> {
        let path = self.pool_users_path();
        match self.fs.read(&path) {
            Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                tracing::warn!("{} is not a list of names; ignoring it", path.display());
                Vec::new()
            }),
            Ok(None) => Vec::new(),
            Err(error) => {
                tracing::warn!("cannot read {}: {error}", path.display());
                Vec::new()
            }
        }
    }

    pub(crate) fn remember_pool_users(&self, names: &[String]) {
        let path = self.pool_users_path();
        let bytes = serde_json::to_vec(names).expect("a list of strings is json");
        if let Err(error) = self.fs.write_private(&path, &bytes) {
            tracing::warn!("cannot write {}: {error}", path.display());
        }
    }

    async fn get(&self, url: &str, authorization: Option<&str>) -> Result<String> {
        let answer = self
            .http
            .get(url, authorization)
            .await
            .map_err(|e| EngineError::Http(format!("{url}: {e}")))?;
        if !(200..300).contains(&answer.status) {
            return Err(EngineError::Http(format!(
                "{url} answered {}",
                answer.status
            )));
        }
        Ok(answer.body)
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        let (_, config) = self.load()?;
        let body = self.get(&metrics_url(&config), None).await?;
        let samples = parse_prometheus(&body);
        if !samples.iter().any(|s| s.name.starts_with("telemt_")) {
            return Err(EngineError::Http(
                "the answer is not telemt's metrics".to_owned(),
            ));
        }
        Ok(snapshot_of(&config, &samples))
    }

    /// telemt is up on the file it was started with: its metrics answer. Whether its middle
    /// proxy has writers is not asked here (its servers may be slow or unreachable for longer
    /// than the wait, and that is no fault of the new file); `health` reports it.
    async fn is_ready(&self) -> bool {
        self.snapshot().await.is_ok()
    }

    /// Polls until the engine is ready; the reason if it is not within the time.
    pub(crate) async fn wait_ready(&self) -> std::result::Result<(), String> {
        let started = Instant::now();
        loop {
            if self.is_ready().await {
                return Ok(());
            }
            if started.elapsed() >= READY_TIMEOUT {
                return Err(format!(
                    "telemt did not answer its metrics within {} seconds of the restart",
                    READY_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    async fn systemctl(&self, verb: &str) -> Result<()> {
        let output = self
            .runner
            .run("systemctl", &[verb, &self.unit], None)
            .await
            .map_err(|e| EngineError::Host(format!("systemctl {verb} {}: {e}", self.unit)))?;
        if output.status.success() {
            Ok(())
        } else {
            let code = output
                .status
                .code()
                .map_or_else(|| "a signal".to_owned(), |c| format!("code {c}"));
            Err(EngineError::Host(format!(
                "systemctl {verb} {} ended with {code}",
                self.unit
            )))
        }
    }

    /// Puts `old` back and brings the engine up on it.
    async fn roll_back(&self, old: &[u8]) -> std::result::Result<(), String> {
        self.fs
            .write_like(&self.config_path, old, &self.config_path)
            .map_err(|e| format!("cannot restore {}: {e}", self.config_path.display()))?;
        // Best effort: the unit may be stopped already.
        let _ = self.systemctl("stop").await;
        self.systemctl("start").await.map_err(|e| e.to_string())?;
        self.wait_ready().await
    }
}

fn snapshot_of(config: &TelemtToml, samples: &[Sample]) -> Snapshot {
    let user_telemetry_off = match prom::first(samples, "telemt_telemetry_user_enabled") {
        Some(gauge) => gauge == 0.0,
        None => !config.user_telemetry_enabled(),
    };
    let octets = |direction: &str| -> u64 {
        if user_telemetry_off {
            return 0;
        }
        // The series is `..._total` in telemt 3.5.16; one without the suffix is accepted too,
        // and an engine that exports both is counted once.
        let total = format!("telemt_user_octets_{direction}_total");
        let bare = format!("telemt_user_octets_{direction}");
        prom::sum(samples, &total)
            .or_else(|| prom::sum(samples, &bare))
            .map_or(0, |v| v as u64)
    };
    let clients = if user_telemetry_off {
        None
    } else {
        prom::sum(samples, "telemt_user_unique_ips_current").map(|v| v as u64)
    };
    let writers = prom::first(samples, "telemt_me_writers_active_current").unwrap_or(0.0);
    // With the control API the DC2 check says how the middle proxy is; without it a telemt that
    // uses the middle proxy is serving once it has a writer.
    let ok = config.control_api().is_some() || !config.uses_middle_proxy() || writers > 0.0;
    Snapshot {
        report: MetricsReport {
            ok,
            from_client_total: octets("from_client"),
            to_client_total: octets("to_client"),
            clients,
        },
        user_telemetry_off,
    }
}

#[async_trait]
impl<R: CommandRunner + 'static, H: HttpGet + 'static> Engine for TelemtEngine<R, H> {
    fn kind(&self) -> EngineKind {
        EngineKind::Telemt
    }

    fn port(&self) -> Option<u16> {
        self.load().ok().map(|(_, config)| config.port())
    }

    async fn metrics(&self) -> Result<MetricsReport> {
        Ok(self.snapshot().await?.report)
    }

    async fn dc2(&self) -> Option<Dc2Report> {
        let (_, config) = self.load().ok()?;
        let api = config.control_api()?;
        let url = format!("{}/v1/stats/dcs", api.base);
        let answer = self.get(&url, api.authorization.as_deref()).await;
        let now = (self.clock)();
        Some(dc2::evaluate(answer.as_deref().map_err(|_| ()), now))
    }

    async fn health(&self) -> EngineHealth {
        if self.load().is_err() {
            return EngineHealth {
                ok: false,
                reason: Some(REASON_CONFIG_UNREADABLE.to_owned()),
            };
        }
        let (ok, reason) = match self.snapshot().await {
            Err(_) => (false, REASON_METRICS_UNREACHABLE),
            Ok(s) if !s.report.ok => (false, REASON_NO_ME_WRITERS),
            Ok(s) if s.user_telemetry_off => (true, REASON_USER_TELEMETRY_DISABLED),
            Ok(_) => {
                return EngineHealth {
                    ok: true,
                    reason: None,
                };
            }
        };
        EngineHealth {
            ok,
            reason: Some(reason.to_owned()),
        }
    }

    async fn apply_config(&self, desired: &DesiredConfig) -> Result<ApplyOutcome> {
        validate(desired)?;
        let (old_text, _) = self.load()?;
        let managed = self.pool_users();
        let new_text = edit(&old_text, desired, &managed)?;
        let names: Vec<String> = desired.users.iter().map(|(name, _)| name.clone()).collect();

        if new_text == old_text {
            if managed != names {
                self.remember_pool_users(&names);
            }
            return Ok(ApplyOutcome::Unchanged);
        }

        // Nothing is written until telemt is stopped: if it cannot be, the file stays as it was.
        self.systemctl("stop").await?;
        let old = old_text.as_bytes();
        let failure = self.write_and_start(old, &new_text).await.err();
        match failure {
            None => {
                self.remember_pool_users(&names);
                Ok(ApplyOutcome::Applied)
            }
            Some(reason) => match self.roll_back(old).await {
                Ok(()) => Ok(ApplyOutcome::RolledBack(reason)),
                Err(cause) => Err(EngineError::RollbackFailed { reason, cause }),
            },
        }
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> TelemtEngine<R, H> {
    /// Keeps the old file next to the new one, writes the new, starts telemt and waits for it.
    async fn write_and_start(&self, old: &[u8], new: &str) -> std::result::Result<(), String> {
        let mut backup = self.config_path.as_os_str().to_owned();
        backup.push(".bak");
        let backup = PathBuf::from(backup);
        self.fs
            .write_like(&backup, old, &self.config_path)
            .map_err(|e| format!("cannot write {}: {e}", backup.display()))?;
        self.fs
            .write_like(&self.config_path, new.as_bytes(), &self.config_path)
            .map_err(|e| format!("cannot write {}: {e}", self.config_path.display()))?;
        self.systemctl("start").await.map_err(|e| e.to_string())?;
        self.wait_ready().await
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> EngineProbe for TelemtEngine<R, H> {
    async fn detect(&self) -> Option<DetectedEngine> {
        Some(DetectedEngine {
            engine: EngineKind::Telemt,
            port: Engine::port(self)?,
        })
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> ConfigSink for TelemtEngine<R, H> {
    async fn apply(&self, config: &AgentConfig) {
        match self.apply_config(&DesiredConfig::from(config)).await {
            Ok(ApplyOutcome::Unchanged) => tracing::debug!("telemt already serves this config"),
            Ok(ApplyOutcome::Applied) => tracing::info!(
                users = config.users.len(),
                port = config.port,
                "telemt restarted with the new config"
            ),
            Ok(ApplyOutcome::RolledBack(reason)) => {
                tracing::error!("the new telemt config failed, the old one is back: {reason}");
            }
            Err(error) => tracing::error!("cannot apply the config to telemt: {error}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Editing telemt.toml
// ---------------------------------------------------------------------------------------------

/// What the pool sends is checked before a byte of the engine's file changes.
pub(crate) fn validate(desired: &DesiredConfig) -> Result<()> {
    let bad = |what: String| Err(EngineError::Config(what));
    if desired.port == 0 {
        return bad("the port is 0".to_owned());
    }
    let domain_ok = !desired.sni.is_empty()
        && desired.sni.len() <= 253
        && desired
            .sni
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if !domain_ok {
        return bad("the TLS domain is empty or has characters a domain cannot have".to_owned());
    }
    for (name, secret) in &desired.users {
        let name_ok = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !name_ok {
            return bad("a user name is empty or has characters a key cannot have".to_owned());
        }
        if secret.len() != 32 || !secret.chars().all(|c| c.is_ascii_hexdigit()) {
            return bad(format!(
                "the secret of user `{name}` is not 32 hexadecimal digits"
            ));
        }
    }
    Ok(())
}

/// The table `key` of `parent`, made if it is not there. `implicit` leaves out its own header
/// when it only holds tables.
fn table_of<'a>(
    parent: &'a mut dyn TableLike,
    key: &str,
    implicit: bool,
) -> Result<&'a mut dyn TableLike> {
    if !parent.contains_key(key) {
        let mut table = Table::new();
        table.set_implicit(implicit);
        parent.insert(key, Item::Table(table));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| EngineError::Config(format!("`{key}` in telemt.toml is not a table")))
}

/// Sets `key` to `value`, keeping the comments around an existing one.
fn set_value(table: &mut dyn TableLike, key: &str, value: Value) {
    if let Some(Item::Value(current)) = table.get_mut(key) {
        let decor = current.decor().clone();
        *current = value;
        *current.decor_mut() = decor;
    } else {
        table.insert(key, Item::Value(value));
    }
}

/// The new text of the file: the pool's users, the TLS domain and the port set, the pool's
/// revoked users removed. Everything else, comments and users of the owner included, is kept as
/// it was. A value that is already right is not touched, so a file that needs nothing comes out
/// identical.
fn edit(old: &str, desired: &DesiredConfig, managed: &[String]) -> Result<String> {
    let mut doc: DocumentMut = old.parse().map_err(|e: toml_edit::TomlError| {
        EngineError::Config(format!(
            "telemt.toml is not valid TOML: {} (at bytes {:?})",
            e.message(),
            e.span()
        ))
    })?;
    let root: &mut dyn TableLike = doc.as_table_mut();

    let server = table_of(root, "server", false)?;
    let port = i64::from(desired.port);
    let current = server.get("port").and_then(Item::as_integer);
    if current != Some(port) && !(current.is_none() && desired.port == DEFAULT_PORT) {
        set_value(server, "port", Value::from(port));
    }
    // A listener with a port of its own keeps it in step; one without follows `[server] port`.
    if let Some(listeners) = server
        .get_mut("listeners")
        .and_then(Item::as_array_of_tables_mut)
    {
        for listener in listeners.iter_mut() {
            if listener
                .get("port")
                .is_some_and(|p| p.as_integer() != Some(port))
            {
                set_value(listener, "port", Value::from(port));
            }
        }
    }

    // telemt starts no metrics listener unless the file names one, and the agent needs it.
    if !server.contains_key("metrics_listen") && !server.contains_key("metrics_port") {
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_METRICS_PORT);
        set_value(server, "metrics_listen", Value::from(listen.to_string()));
    }

    let censorship = table_of(root, "censorship", false)?;
    if censorship.get("tls_domain").and_then(Item::as_str) != Some(desired.sni.as_str()) {
        set_value(censorship, "tls_domain", Value::from(desired.sni.as_str()));
    }

    let access = table_of(root, "access", true)?;
    let users = table_of(access, "users", false)?;
    for name in managed {
        if !desired.users.iter().any(|(wanted, _)| wanted == name) {
            users.remove(name);
        }
    }
    for (name, secret) in &desired.users {
        if users.get(name).and_then(Item::as_str) != Some(secret.as_str()) {
            set_value(users, name, Value::from(secret.as_str()));
        }
    }
    if users.is_empty() {
        return Err(EngineError::Config(
            "the configuration would have no user, and telemt does not start without one"
                .to_owned(),
        ));
    }
    Ok(doc.to_string())
}
