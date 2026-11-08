//! `mtpool-agent`: the node agent. `run` enrolls the node and keeps it in step with the registrar.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use mtpool_agent::antiscan::{Antiscan, AntiscanSink, IpsumHttp};
use mtpool_agent::check_host::{CheckHostLoop, IpSource};
use mtpool_agent::client::RegistrarClient;
use mtpool_agent::config::{DEFAULT_CONFIG_PATH, LocalConfig, Overrides};
use mtpool_agent::engine::http::ReqwestHttp;
use mtpool_agent::engine::select::{AnyEngine, select_engine};
use mtpool_agent::gp::GpLoop;
use mtpool_agent::host::TokioRunner;
use mtpool_agent::ip_detect::IpWatch;
use mtpool_agent::lifecycle::{
    ConfigSink, DetectedEngine, EngineProbe, Hooks, Lifecycle, system_jitter,
};
use mtpool_agent::local_health::LocalTcp;
use mtpool_agent::port_filter::PortFilterWatch;
use mtpool_agent::report::{ChecksSetup, ConfigMemory, NodeChecks, Remembering};
use mtpool_agent::state_files::RealFs;
use mtpool_protocol::agent::AgentConfig;
use tracing_subscriber::EnvFilter;

const ENV_LOG: &str = "MTPOOL_AGENT_LOG";
const DEFAULT_LOG: &str = "info";

#[derive(Debug, Parser)]
#[command(name = "mtpool-agent", about = "MTPooL node agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum Command {
    /// Run the agent.
    Run {
        /// The local configuration file.
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
    /// Print the version.
    Version,
    /// Manage the agent's own update.
    Update {
        #[command(subcommand)]
        action: UpdateAction,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum UpdateAction {
    /// Put the previous build back when a new one is on probation. Run by the rollback unit.
    Rollback {
        /// The local configuration file.
        #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
        config: PathBuf,
    },
}

fn init_logging() {
    let directives = std::env::var(ENV_LOG).unwrap_or_else(|_| DEFAULT_LOG.to_owned());
    let filter = EnvFilter::try_new(&directives).unwrap_or_else(|_| {
        eprintln!(
            "warning: {ENV_LOG} is not a valid log filter and is ignored, using {DEFAULT_LOG:?}"
        );
        EnvFilter::new(DEFAULT_LOG)
    });
    // Standard output carries command results: logs go to standard error.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

/// Resolves on SIGTERM or SIGINT.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        // Without signal handlers the agent cannot be stopped politely; never resolve.
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

type Chosen = AnyEngine<TokioRunner, ReqwestHttp>;

/// The engine the host runs, as the engine the lifecycle detects and as the sink it hands the
/// config to. A port in the local config wins over the one in the engine's own file.
#[derive(Clone)]
struct SharedEngine {
    engine: Arc<Chosen>,
    port: Option<u16>,
}

impl EngineProbe for SharedEngine {
    async fn detect(&self) -> Option<DetectedEngine> {
        match self.port {
            Some(port) => Some(DetectedEngine {
                engine: mtpool_agent::engine::Engine::kind(&*self.engine),
                port,
            }),
            None => self.engine.detect().await,
        }
    }
}

impl ConfigSink for SharedEngine {
    async fn apply(&self, config: &AgentConfig) {
        self.engine.apply(config).await;
    }
}

async fn run(config_path: PathBuf) -> ExitCode {
    let overrides = match Overrides::from_env() {
        Ok(overrides) => overrides,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if overrides.allow_private_ips {
        tracing::warn!(
            "MTPOOL_ALLOW_PRIVATE_IPS is on: private addresses count as the node's own and the \
             registrar may be plain http. This is for a test stand, never for a public node"
        );
    }
    let config = match LocalConfig::load_allowing(&config_path, overrides.allow_private_ips) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let client = match RegistrarClient::new(config.registrar.clone()) {
        Ok(client) => client,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let http = match ReqwestHttp::new() {
        Ok(http) => http,
        Err(error) => {
            tracing::error!("cannot build the HTTP client for the engine: {error}");
            return ExitCode::FAILURE;
        }
    };
    let http = Arc::new(http);
    let fs = Arc::new(RealFs);
    let engine = Arc::new(
        select_engine(
            config.state_dir.clone(),
            Arc::new(TokioRunner),
            fs.clone(),
            http.clone(),
        )
        .await,
    );
    tracing::info!(engine = ?mtpool_agent::engine::Engine::kind(&*engine), "proxy engine picked");
    let shared = SharedEngine {
        engine: engine.clone(),
        port: config.port,
    };

    let ipsum = match IpsumHttp::with_url(&overrides.ipsum_url) {
        Ok(ipsum) => ipsum,
        Err(error) => {
            tracing::error!("cannot build the HTTP client for antiscan: {error}");
            return ExitCode::FAILURE;
        }
    };
    let antiscan = Arc::new(Antiscan::new(Arc::new(TokioRunner), ipsum, config.antiscan));
    let antiscan_task = tokio::spawn(antiscan.clone().run());

    let ports = Arc::new(LocalTcp);
    let gp = match GpLoop::new(&overrides.globalping_url, ports.clone()) {
        Ok(gp) => gp,
        Err(error) => {
            tracing::error!("cannot build the HTTP client for Globalping: {error}");
            return ExitCode::FAILURE;
        }
    };
    let gp_task = tokio::spawn(gp.clone().run());
    let check_host = match CheckHostLoop::new(
        &overrides.check_host_url,
        config.check_host_port,
        IpSource::default(),
    ) {
        Ok(check_host) => check_host,
        Err(error) => {
            tracing::error!("cannot build the HTTP client for check-host: {error}");
            return ExitCode::FAILURE;
        }
    };
    let check_host_task = tokio::spawn(check_host.clone().run());
    let memory = ConfigMemory::new();
    let checks = NodeChecks::new(ChecksSetup {
        engine,
        port: config.port,
        ports,
        fs: fs.clone(),
        ip: IpWatch::new(http, fs.clone(), config.state_dir.clone())
            .with_sources(overrides.ip_sources.clone())
            .with_allow_private(overrides.allow_private_ips),
        gp,
        check_host,
        port_filter: PortFilterWatch::new(Arc::new(TokioRunner)),
        memory: memory.clone(),
    });
    let hooks = Hooks {
        engine: shared.clone(),
        reports: checks,
        sink: Remembering {
            memory,
            inner: AntiscanSink {
                antiscan,
                local_enabled: config.antiscan,
                inner: shared,
            },
        },
    };
    let lifecycle = match Lifecycle::new(&config, client, fs.clone(), hooks, system_jitter()) {
        Ok(lifecycle) => lifecycle,
        Err(error) => {
            tracing::error!(
                "cannot read the state in {}: {error}",
                config.state_dir.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let update_task = update_task(&config, fs.clone());
    tracing::info!(version = env!("CARGO_PKG_VERSION"), name = %config.name, "mtpool-agent starts");
    let outcome = lifecycle.run(shutdown_signal()).await;
    gp_task.abort();
    check_host_task.abort();
    antiscan_task.abort();
    if let Some(task) = update_task {
        task.abort();
    }
    match outcome {
        Ok(()) => {
            tracing::info!("mtpool-agent stops");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            tracing::error!("{reason}");
            ExitCode::from(reason.exit_code())
        }
    }
}

/// The self-update loop, when this build can verify releases.
fn update_task(config: &LocalConfig, fs: Arc<RealFs>) -> Option<tokio::task::JoinHandle<()>> {
    use mtpool_agent::state_files::StateFiles;
    use mtpool_agent::update::{self, AgentUpdater, RegistrarSource, SystemHost};
    let Some(cfg) = update::host_config(&config.state_dir) else {
        tracing::info!("no release key is built in: the agent does not update itself");
        return None;
    };
    if let Err(error) = std::fs::create_dir_all(&cfg.paths.releases) {
        tracing::warn!(%error, "cannot prepare the update directory: the agent does not update itself");
        return None;
    }
    let client = RegistrarClient::new(config.registrar.clone()).ok()?;
    let source = RegistrarSource {
        client,
        state: StateFiles::new(fs, config.state_dir.clone()),
    };
    let host = SystemHost {
        runner: TokioRunner,
        unit: update::UNIT.to_owned(),
    };
    let updater = AgentUpdater::new(cfg, source, host);
    Some(tokio::spawn(update::run_loop(
        updater,
        update::poll_every(),
    )))
}

/// `update rollback`: what the rollback unit runs.
async fn rollback(config_path: PathBuf) -> ExitCode {
    use mtpool_agent::state_files::StateFiles;
    use mtpool_agent::update::{self, AgentUpdater, RegistrarSource, SystemHost};
    let allow_private = match Overrides::from_env() {
        Ok(overrides) => overrides.allow_private_ips,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let config = match LocalConfig::load_allowing(&config_path, allow_private) {
        Ok(config) => config,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(cfg) = update::host_config(&config.state_dir) else {
        tracing::error!("no release key is built in: nothing to roll back");
        return ExitCode::FAILURE;
    };
    let Ok(client) = RegistrarClient::new(config.registrar.clone()) else {
        return ExitCode::FAILURE;
    };
    // The rollback only writes the report to the outbox; the restarted agent sends it.
    let source = RegistrarSource {
        client,
        state: StateFiles::new(Arc::new(RealFs), config.state_dir.clone()),
    };
    let host = SystemHost {
        runner: TokioRunner,
        unit: update::UNIT.to_owned(),
    };
    match AgentUpdater::new(cfg, source, host).rollback().await {
        Ok(Some(version)) => {
            println!("rolled back {version}");
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("no update is on probation; nothing changed");
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => {
            match mtpool_protocol::signing::key_id(mtpool_agent::MINISIGN_PUB) {
                Some(id) => println!("{} (signing key {id})", env!("CARGO_PKG_VERSION")),
                None => println!("{}", env!("CARGO_PKG_VERSION")),
            }
            ExitCode::SUCCESS
        }
        Command::Update {
            action: UpdateAction::Rollback { config },
        } => {
            init_logging();
            rollback(config).await
        }
        Command::Run { config } => {
            init_logging();
            run(config).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_defaults_to_the_system_config() {
        let cli = Cli::try_parse_from(["mtpool-agent", "run"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Run {
                config: PathBuf::from(DEFAULT_CONFIG_PATH)
            }
        );
    }

    #[test]
    fn run_takes_a_config_path() {
        let cli = Cli::try_parse_from(["mtpool-agent", "run", "--config", "/tmp/a.toml"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Run {
                config: PathBuf::from("/tmp/a.toml")
            }
        );
    }

    #[test]
    fn version_parses_and_unknown_commands_do_not() {
        assert_eq!(
            Cli::try_parse_from(["mtpool-agent", "version"])
                .unwrap()
                .command,
            Command::Version
        );
        assert!(Cli::try_parse_from(["mtpool-agent", "serve"]).is_err());
    }
}
