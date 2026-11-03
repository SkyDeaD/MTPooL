//! The engine the host runs, picked once at start by [`detect_engine`]: one value that is the
//! adapter for whichever it is.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use mtpool_protocol::agent::{AgentConfig, Dc2Report, MetricsReport};

use super::detect::{detect_engine, find_service_unit};
use super::http::HttpGet;
use super::meko::{self, MekoEngine};
use super::mtproxyl::MtProxyLEngine;
use super::telemt::{self, TelemtEngine};
use super::{ApplyOutcome, DesiredConfig, Engine, EngineHealth, EngineKind, Result};
use crate::host::CommandRunner;
use crate::lifecycle::{ConfigSink, DetectedEngine, EngineProbe};
use crate::state_files::Fs;

pub enum AnyEngine<R, H> {
    Telemt(TelemtEngine<R, H>),
    Meko(MekoEngine<R, H>),
    MtProxyL(MtProxyLEngine<R, H>),
}

/// The adapter for the engine [`detect_engine`] finds on the host.
pub async fn select_engine<R: CommandRunner + 'static, H: HttpGet + 'static>(
    state_dir: PathBuf,
    runner: Arc<R>,
    fs: Arc<dyn Fs>,
    http: Arc<H>,
) -> AnyEngine<R, H> {
    match detect_engine(&*fs) {
        EngineKind::Meko => AnyEngine::Meko(
            MekoEngine::detect_unit(
                PathBuf::from(meko::DEFAULT_CONFIG_PATH),
                state_dir,
                runner,
                fs,
                http,
            )
            .await,
        ),
        EngineKind::MtProxyL => {
            AnyEngine::MtProxyL(MtProxyLEngine::new(state_dir, runner, fs, http))
        }
        EngineKind::Telemt => {
            let unit = find_service_unit(&*runner)
                .await
                .unwrap_or_else(|| telemt::DEFAULT_UNIT.to_owned());
            AnyEngine::Telemt(TelemtEngine::new(
                PathBuf::from(telemt::DEFAULT_CONFIG_PATH),
                unit,
                state_dir,
                runner,
                fs,
                http,
            ))
        }
    }
}

macro_rules! each {
    ($self:expr, $engine:ident => $call:expr) => {
        match $self {
            AnyEngine::Telemt($engine) => $call,
            AnyEngine::Meko($engine) => $call,
            AnyEngine::MtProxyL($engine) => $call,
        }
    };
}

#[async_trait]
impl<R: CommandRunner + 'static, H: HttpGet + 'static> Engine for AnyEngine<R, H> {
    fn kind(&self) -> EngineKind {
        each!(self, e => e.kind())
    }
    fn port(&self) -> Option<u16> {
        each!(self, e => e.port())
    }
    async fn metrics(&self) -> Result<MetricsReport> {
        each!(self, e => e.metrics().await)
    }
    async fn dc2(&self) -> Option<Dc2Report> {
        each!(self, e => e.dc2().await)
    }
    async fn health(&self) -> EngineHealth {
        each!(self, e => e.health().await)
    }
    async fn apply_config(&self, desired: &DesiredConfig) -> Result<ApplyOutcome> {
        each!(self, e => e.apply_config(desired).await)
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> EngineProbe for AnyEngine<R, H> {
    async fn detect(&self) -> Option<DetectedEngine> {
        each!(self, e => EngineProbe::detect(e).await)
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> ConfigSink for AnyEngine<R, H> {
    async fn apply(&self, config: &AgentConfig) {
        each!(self, e => ConfigSink::apply(e, config).await)
    }
}
