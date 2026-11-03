//! `mtpool-agent`: the node agent. `run` enrolls the node and keeps it in step with the registrar.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use mtpool_agent::antiscan::{Antiscan, AntiscanSink, IpsumHttp};
use mtpool_agent::client::RegistrarClient;
use mtpool_agent::config::{DEFAULT_CONFIG_PATH, LocalConfig};
use mtpool_agent::engine::http::ReqwestHttp;
use mtpool_agent::engine::select::{AnyEngine, select_engine};
use mtpool_agent::gp::{DEFAULT_BASE, GpLoop};
use mtpool_agent::host::TokioRunner;
use mtpool_agent::ip_detect::IpWatch;
use mtpool_agent::lifecycle::{
    ConfigSink, DetectedEngine, EngineProbe, Hooks, Lifecycle, system_jitter,
};
use mtpool_agent::local_health::LocalTcp;
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
    let config = match LocalConfig::load(&config_path) {
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

    let ipsum = match IpsumHttp::new() {
        Ok(ipsum) => ipsum,
        Err(error) => {
            tracing::error!("cannot build the HTTP client for antiscan: {error}");
            return ExitCode::FAILURE;
        }
    };
    let antiscan = Arc::new(Antiscan::new(Arc::new(TokioRunner), ipsum, config.antiscan));
    let antiscan_task = tokio::spawn(antiscan.clone().run());

    let ports = Arc::new(LocalTcp);
    let gp = match GpLoop::new(DEFAULT_BASE, ports.clone()) {
        Ok(gp) => gp,
        Err(error) => {
            tracing::error!("cannot build the HTTP client for Globalping: {error}");
            return ExitCode::FAILURE;
        }
    };
    let gp_task = tokio::spawn(gp.clone().run());
    let memory = ConfigMemory::new();
    let checks = NodeChecks::new(ChecksSetup {
        engine,
        port: config.port,
        ports,
        fs: fs.clone(),
        ip: IpWatch::new(http, fs.clone(), config.state_dir.clone()),
        gp,
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
    let lifecycle = match Lifecycle::new(&config, client, fs, hooks, system_jitter()) {
        Ok(lifecycle) => lifecycle,
        Err(error) => {
            tracing::error!(
                "cannot read the state in {}: {error}",
                config.state_dir.display()
            );
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(version = env!("CARGO_PKG_VERSION"), name = %config.name, "mtpool-agent starts");
    let outcome = lifecycle.run(shutdown_signal()).await;
    gp_task.abort();
    antiscan_task.abort();
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

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
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
