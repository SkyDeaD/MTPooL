//! `mtpool-agent`: the node agent. `run` enrolls the node and keeps it in step with the registrar.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use mtpool_agent::client::RegistrarClient;
use mtpool_agent::config::{DEFAULT_CONFIG_PATH, LocalConfig};
use mtpool_agent::lifecycle::{
    DetectedEngine, Hooks, Lifecycle, LogSink, NoReports, StaticEngine, system_jitter,
};
use mtpool_agent::state_files::RealFs;
use mtpool_protocol::public::Engine;
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
    // Until the engine adapter exists the agent assumes telemt on the configured port.
    let engine = config.port.map(|port| DetectedEngine {
        engine: Engine::Telemt,
        port,
    });
    let hooks = Hooks {
        engine: StaticEngine(engine),
        reports: NoReports,
        sink: LogSink,
    };
    let lifecycle = match Lifecycle::new(&config, client, Arc::new(RealFs), hooks, system_jitter())
    {
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
    match lifecycle.run(shutdown_signal()).await {
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
