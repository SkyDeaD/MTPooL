//! The report of the node: what the checks found, in the shape the registrar takes. The node's
//! own checks live here too: [`NodeChecks`] is the [`ReportSource`] of the lifecycle.

use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mtpool_protocol::agent::{
    AgentConfig, Dc2Report, GpReport, HostReport, MetricsReport, ReportRequest,
};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use crate::dead_kill::verdict;
use crate::engine::http::HttpGet;
use crate::engine::telemt::REASON_USER_TELEMETRY_DISABLED;
use crate::engine::{Engine, EngineHealth, Result as EngineResult};
use crate::gp::{self, GpJob, GpLoop};
use crate::hoststats::HostStats;
use crate::ip_detect::{IpEvent, IpWatch};
use crate::lifecycle::{ConfigSink, ReportSource, Watch};
use crate::local_health::{LocalHealth, PortCheck, check_once};
use crate::state_files::Fs;

/// Everything one report is made of.
pub struct ReportInputs {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub sni: String,
    pub gp: GpReport,
    pub metrics: EngineResult<MetricsReport>,
    pub dc2: Option<Dc2Report>,
    pub host: Option<HostReport>,
    pub health: EngineHealth,
    pub now: SystemTime,
}

/// `now` as RFC 3339 in UTC, whole seconds.
fn rfc3339(now: SystemTime) -> String {
    use time::format_description::well_known::Rfc3339;
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0);
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

pub fn build_report(inputs: ReportInputs) -> ReportRequest {
    // An engine that does not answer is a proxy that does not serve: `ok: false`, not a gap.
    let metrics = inputs.metrics.unwrap_or(MetricsReport {
        ok: false,
        from_client_total: 0,
        to_client_total: 0,
        clients: None,
    });
    let error = (inputs.health.reason.as_deref() == Some(REASON_USER_TELEMETRY_DISABLED))
        .then(|| REASON_USER_TELEMETRY_DISABLED.to_owned());
    ReportRequest {
        ip: inputs.ip,
        port: inputs.port,
        sni: inputs.sni,
        gp: inputs.gp,
        metrics,
        dc2: inputs.dc2,
        host: inputs.host,
        checked_at: rfc3339(inputs.now),
        error,
    }
}

/// What the config last said, for the checks: the domain the proxy pretends to be and how often
/// to ask Globalping.
#[derive(Debug, Clone, Default)]
pub struct ConfigMemory(Arc<Mutex<Option<(String, u32)>>>);

impl ConfigMemory {
    pub fn new() -> ConfigMemory {
        ConfigMemory::default()
    }

    fn slot(&self) -> MutexGuard<'_, Option<(String, u32)>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn remember(&self, config: &AgentConfig) {
        *self.slot() = Some((config.sni.clone(), config.intervals.globalping_sec));
    }

    pub fn get(&self) -> Option<(String, u32)> {
        self.slot().clone()
    }
}

/// A sink that notes the config for [`ConfigMemory`] and hands it on.
pub struct Remembering<K> {
    pub memory: ConfigMemory,
    pub inner: K,
}

impl<K: ConfigSink> ConfigSink for Remembering<K> {
    async fn apply(&self, config: &AgentConfig) {
        self.memory.remember(config);
        self.inner.apply(config).await;
    }
}

pub struct ChecksSetup<H> {
    pub engine: Arc<dyn Engine>,
    /// The port from the local configuration, which wins over the engine's own.
    pub port: Option<u16>,
    pub ports: Arc<dyn PortCheck>,
    pub fs: Arc<dyn Fs>,
    pub ip: IpWatch<H>,
    pub gp: GpLoop,
    pub memory: ConfigMemory,
}

struct Shared<H> {
    engine: Arc<dyn Engine>,
    port: Option<u16>,
    ports: Arc<dyn PortCheck>,
    fs: Arc<dyn Fs>,
    ip: AsyncMutex<IpWatch<H>>,
    gp: GpLoop,
    memory: ConfigMemory,
    health: Mutex<LocalHealth>,
}

/// The checks of one node.
pub struct NodeChecks<H> {
    shared: Arc<Shared<H>>,
    clock: fn() -> SystemTime,
}

impl<H> Clone for NodeChecks<H> {
    fn clone(&self) -> Self {
        NodeChecks {
            shared: self.shared.clone(),
            clock: self.clock,
        }
    }
}

impl<H: HttpGet + 'static> NodeChecks<H> {
    pub fn new(setup: ChecksSetup<H>) -> NodeChecks<H> {
        NodeChecks {
            shared: Arc::new(Shared {
                engine: setup.engine,
                port: setup.port,
                ports: setup.ports,
                fs: setup.fs,
                ip: AsyncMutex::new(setup.ip),
                gp: setup.gp,
                memory: setup.memory,
                health: Mutex::new(LocalHealth::new()),
            }),
            clock: SystemTime::now,
        }
    }

    /// Replaces the clock the reports are stamped with.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> SystemTime) -> NodeChecks<H> {
        self.clock = clock;
        self
    }

    /// The port of the proxy: the local configuration's, else the engine's own.
    fn port(&self) -> Option<u16> {
        self.shared.port.or_else(|| self.shared.engine.port())
    }

    fn health(&self) -> MutexGuard<'_, LocalHealth> {
        self.shared
            .health
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<H: HttpGet + 'static> ReportSource for NodeChecks<H> {
    /// `None` until the address and the domain are known: a report without them would only be
    /// refused.
    async fn build(&self) -> Option<ReportRequest> {
        let ip = self.shared.ip.lock().await.current()?;
        let (sni, _) = self.shared.memory.get()?;
        let port = self.port()?;
        let engine = &self.shared.engine;
        let (metrics, dc2, health) = tokio::join!(engine.metrics(), engine.dc2(), engine.health());
        let host = HostStats::read(&*self.shared.fs).map(HostStats::into_report);
        let gp = self.shared.gp.take_result().unwrap_or(GpReport::NoData {
            reason: gp::NO_DATA.into(),
        });
        Some(build_report(ReportInputs {
            ip,
            port,
            sni,
            gp,
            metrics,
            dc2,
            host,
            health,
            now: (self.clock)(),
        }))
    }

    async fn watch(&self, now: Instant) -> Watch {
        let (event, ip) = {
            let mut watch = self.shared.ip.lock().await;
            let event = watch.check(now).await;
            (event, watch.current())
        };
        let port = self.port();

        let check = check_once(&*self.shared.engine, &*self.shared.ports, port, ip).await;
        let health = {
            let mut health = self.health();
            health.observe(now, check.ok);
            verdict(now, &health)
        };

        // What Globalping is to measure follows the address, the domain and the port.
        let job = match (ip, port, self.shared.memory.get()) {
            (Some(ip), Some(port), Some((sni, seconds))) => Some(GpJob {
                ip,
                port,
                sni,
                interval: Duration::from_secs(u64::from(seconds.max(1))),
            }),
            _ => None,
        };
        self.shared.gp.set_job(job);

        Watch {
            ip_changed: matches!(event, IpEvent::Changed { .. }),
            health,
        }
    }

    fn enrolled(&self) {
        *self.health() = LocalHealth::new();
        self.shared.gp.restart();
        // The lifecycle is one task: nothing else holds the lock. If something did, the stored
        // address would only stay as it was, which is as current as the last look.
        if let Ok(mut watch) = self.shared.ip.try_lock() {
            watch.enrolled();
        }
    }
}
