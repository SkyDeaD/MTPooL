//! Command line: `serve`, `openapi`, `admin reset-token`, `backup`, `restore`, `version`.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::auth::tokens;
use crate::config::{Config, ENV_LOG};
use crate::server;
use crate::store::Store;

#[derive(Debug, Parser)]
#[command(name = "mtpool-registrar", about = "MTPooL pool registrar")]
pub struct Cli {
    #[command(subcommand)]
    pub command: CliCommand,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum CliCommand {
    /// Run the registrar.
    Serve,
    /// Print the OpenAPI document.
    Openapi,
    /// Administrative actions on a stopped or running registrar.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Copy the database to a backup file.
    Backup,
    /// Restore the database from a backup file.
    Restore,
    /// Print the version.
    Version,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum AdminCommand {
    /// Issue a new admin token.
    ResetToken,
}

pub fn init_logging() {
    let filter = EnvFilter::try_from_env(ENV_LOG).unwrap_or_else(|_| EnvFilter::new("info"));
    // Standard output carries command results (`openapi`, `version`): logs go to standard error.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

pub async fn run(cli: Cli) -> ExitCode {
    match cli.command {
        CliCommand::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        // Placeholder until the API types exist: the document is generated from them.
        CliCommand::Openapi => {
            println!("{{}}");
            ExitCode::SUCCESS
        }
        CliCommand::Serve => match serve().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => fail(&message),
        },
        CliCommand::Admin {
            command: AdminCommand::ResetToken,
        } => match reset_token().await {
            // The token is shown here and nowhere else.
            Ok(token) => {
                println!("{token}");
                ExitCode::SUCCESS
            }
            Err(message) => fail(&message),
        },
        CliCommand::Backup => fail("backup is not implemented yet"),
        CliCommand::Restore => fail("restore is not implemented yet"),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}

/// Issues a new admin token: the old ones stop working and every panel session ends. Works on
/// a stopped or a running registrar, which reads tokens and sessions from the database.
async fn reset_token() -> Result<String, String> {
    let config = Config::from_env().map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        let store = Store::open(&config.db_path).map_err(|e| e.to_string())?;
        let token = tokens::rotate_admin(&store).map_err(|e| e.to_string());
        let closed = store.close().map_err(|e| e.to_string());
        token.and_then(|t| closed.map(|()| t))
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn serve() -> Result<(), String> {
    let config = Config::from_env().map_err(|e| e.to_string())?;
    let path = config.db_path.clone();
    tracing::info!(db = %path.display(), version = env!("CARGO_PKG_VERSION"), "starting");
    let store = tokio::task::spawn_blocking(move || Store::open(&path))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let listener = server::bind(&config)
        .await
        .map_err(|e| format!("cannot listen on {}: {e}", config.listen))?;
    server::run(store, listener, Vec::new(), server::termination_signal())
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliCommand, clap::Error> {
        Cli::try_parse_from(std::iter::once("mtpool-registrar").chain(args.iter().copied()))
            .map(|c| c.command)
    }

    #[test]
    fn all_subcommands_parse() {
        assert_eq!(parse(&["serve"]).unwrap(), CliCommand::Serve);
        assert_eq!(parse(&["openapi"]).unwrap(), CliCommand::Openapi);
        assert_eq!(parse(&["backup"]).unwrap(), CliCommand::Backup);
        assert_eq!(parse(&["restore"]).unwrap(), CliCommand::Restore);
        assert_eq!(parse(&["version"]).unwrap(), CliCommand::Version);
        assert_eq!(
            parse(&["admin", "reset-token"]).unwrap(),
            CliCommand::Admin {
                command: AdminCommand::ResetToken
            }
        );
        assert!(parse(&["nonsense"]).is_err());
    }
}
