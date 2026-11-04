//! The MEKO adapter. MEKO (`/opt/mtpr-simple`) runs telemt, so this is the telemt adapter with
//! MEKO's name, and the service unit found on the host instead of assumed.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mtpool_protocol::agent::{AgentConfig, Dc2Report, MetricsReport};

use super::detect::find_service_unit;
use super::http::HttpGet;
use super::telemt::{DEFAULT_UNIT, TelemtEngine};
use super::{ApplyOutcome, DesiredConfig, Engine, EngineHealth, EngineKind, Result};
use crate::host::CommandRunner;
use crate::lifecycle::{ConfigSink, DetectedEngine, EngineProbe};
use crate::state_files::Fs;

/// Where MEKO's telemt reads its settings. NOT verified against a MEKO host: it is the classic
/// telemt path, which is what the references give for it.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/telemt/telemt.toml";

pub struct MekoEngine<R, H> {
    inner: TelemtEngine<R, H>,
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> MekoEngine<R, H> {
    pub fn new(
        config_path: PathBuf,
        unit: impl Into<String>,
        state_dir: PathBuf,
        runner: Arc<R>,
        fs: Arc<dyn Fs>,
        http: Arc<H>,
    ) -> Self {
        MekoEngine {
            inner: TelemtEngine::new(config_path, unit, state_dir, runner, fs, http),
        }
    }

    /// The unit is the first of the candidates that `systemctl cat` knows, `telemt.service` (the
    /// name MEKO installs) if none is.
    pub async fn detect_unit(
        config_path: PathBuf,
        state_dir: PathBuf,
        runner: Arc<R>,
        fs: Arc<dyn Fs>,
        http: Arc<H>,
    ) -> Self {
        let unit = find_service_unit(&*runner)
            .await
            .unwrap_or_else(|| DEFAULT_UNIT.to_owned());
        MekoEngine::new(config_path, unit, state_dir, runner, fs, http)
    }

    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> u64) -> Self {
        self.inner = self.inner.with_clock(clock);
        self
    }
}

#[async_trait]
impl<R: CommandRunner + 'static, H: HttpGet + 'static> Engine for MekoEngine<R, H> {
    fn kind(&self) -> EngineKind {
        EngineKind::Meko
    }
    fn port(&self) -> Option<u16> {
        self.inner.port()
    }
    async fn metrics(&self) -> Result<MetricsReport> {
        self.inner.metrics().await
    }
    async fn dc2(&self) -> Option<Dc2Report> {
        self.inner.dc2().await
    }
    async fn health(&self) -> EngineHealth {
        self.inner.health().await
    }
    async fn apply_config(&self, desired: &DesiredConfig) -> Result<ApplyOutcome> {
        self.inner.apply_config(desired).await
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> EngineProbe for MekoEngine<R, H> {
    async fn detect(&self) -> Option<DetectedEngine> {
        Some(DetectedEngine {
            engine: EngineKind::Meko,
            port: self.inner.port()?,
        })
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> ConfigSink for MekoEngine<R, H> {
    async fn apply(&self, config: &AgentConfig) {
        self.inner.apply(config).await;
    }
}
