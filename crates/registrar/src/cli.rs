//! Command line: `serve`, `openapi`, `admin reset-token`, `backup`, `restore`, `version`.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::auth::tokens;
use crate::backup;
use crate::config::{Config, DEFAULT_LOG, ENV_LOG};
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
    /// Copy the database to a backup file (and write its .sha256), while the registrar runs.
    Backup {
        /// Where to write it; by default /var/backups/mtpool/registrar-<unix time>.db.
        #[arg(long)]
        to: Option<PathBuf>,
    },
    /// Restore the database from a backup file. The registrar must be stopped.
    Restore {
        /// The backup; the file <from>.sha256 must lie beside it.
        #[arg(long)]
        from: PathBuf,
    },
    /// Print the version.
    Version,
    /// The root-side halves of the self-update (run by systemd units, not by hand).
    Update {
        #[command(subcommand)]
        command: UpdateCommand,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum UpdateCommand {
    /// Install the published release if it is newer and intact, and watch that it comes up.
    Apply,
    /// Put the previous build back when the last update did not come up.
    Rollback,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum AdminCommand {
    /// Issue a new admin token.
    ResetToken,
}

/// The filter for `directives` (the `MTPOOL_LOG` value). Directives that do not parse give
/// `info` and a warning text for the operator.
pub fn log_filter(directives: &str) -> (EnvFilter, Option<String>) {
    match EnvFilter::try_new(directives) {
        Ok(filter) => (filter, None),
        Err(_) => (
            EnvFilter::new(DEFAULT_LOG),
            Some(format!(
                "warning: {ENV_LOG} value {directives:?} is not a valid log filter and is ignored, using {DEFAULT_LOG:?}"
            )),
        ),
    }
}

pub fn init_logging(directives: &str) {
    let (filter, warning) = log_filter(directives);
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    // Standard output carries command results (`openapi`, `version`): logs go to standard error.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
}

pub async fn run(cli: Cli, config: Config) -> ExitCode {
    match cli.command {
        CliCommand::Version => {
            println!("{}", version_line());
            ExitCode::SUCCESS
        }
        CliCommand::Update { command } => {
            let result = match command {
                UpdateCommand::Apply => crate::update::run_apply(config.listen).await,
                UpdateCommand::Rollback => crate::update::run_rollback(config.listen).await,
            };
            match result {
                Ok(message) => {
                    println!("{message}");
                    ExitCode::SUCCESS
                }
                Err(message) => fail(&message),
            }
        }
        CliCommand::Openapi => {
            println!("{}", crate::api::openapi_pretty());
            ExitCode::SUCCESS
        }
        CliCommand::Serve => match serve(config).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(message) => fail(&message),
        },
        CliCommand::Admin {
            command: AdminCommand::ResetToken,
        } => match reset_token(config).await {
            // The token is shown here and nowhere else.
            Ok(token) => {
                println!("{token}");
                ExitCode::SUCCESS
            }
            Err(message) => fail(&message),
        },
        CliCommand::Backup { to } => {
            let to = to.unwrap_or_else(default_backup_path);
            match blocking(move || backup::backup(&config.db_path, &to).map(|()| to)).await {
                Ok(to) => {
                    println!("{}", to.display());
                    ExitCode::SUCCESS
                }
                Err(message) => fail(&message),
            }
        }
        CliCommand::Restore { from } => {
            match blocking(move || backup::restore(&config.db_path, &from)).await {
                Ok(()) => {
                    println!("restored; start the registrar again");
                    ExitCode::SUCCESS
                }
                Err(message) => fail(&message),
            }
        }
    }
}

/// `/var/backups/mtpool/registrar-<unix time>.db`.
fn default_backup_path() -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    PathBuf::from(format!("/var/backups/mtpool/registrar-{now}.db"))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, backup::BackupError> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

/// The version, and the id of the signing key built in when there is one.
fn version_line() -> String {
    match mtpool_protocol::signing::key_id(crate::release::MINISIGN_PUB) {
        Some(id) => format!("{} (signing key {id})", env!("CARGO_PKG_VERSION")),
        None => env!("CARGO_PKG_VERSION").to_owned(),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}

/// Issues a new admin token: the old ones stop working and every panel session ends. Works on
/// a stopped or a running registrar, which reads tokens and sessions from the database.
async fn reset_token(config: Config) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        let store = Store::open(&config.db_path).map_err(|e| e.to_string())?;
        let token = tokens::rotate_admin(&store).map_err(|e| e.to_string());
        let closed = store.close().map_err(|e| e.to_string());
        token.and_then(|t| closed.map(|()| t))
    })
    .await
    .map_err(|e| e.to_string())?
}

async fn serve(config: Config) -> Result<(), String> {
    let path = config.db_path.clone();
    // Held for as long as the registrar runs: a restore refuses while it is.
    let _serving = backup::lock_serving(&path).map_err(|e| match e {
        backup::BackupError::Running => format!(
            "another registrar already serves {}; stop it first",
            path.display()
        ),
        other => other.to_string(),
    })?;
    tracing::info!(db = %path.display(), version = env!("CARGO_PKG_VERSION"), "starting");
    let store = tokio::task::spawn_blocking(move || {
        if crate::store::undo_newer_schema(&path)? {
            tracing::warn!(
                db = %path.display(),
                "a newer build migrated this database and was rolled back: went back to the copy from before the migration"
            );
        }
        Store::open(&path)
    })
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
        assert_eq!(parse(&["backup"]).unwrap(), CliCommand::Backup { to: None });
        assert_eq!(
            parse(&["backup", "--to", "/tmp/b.db"]).unwrap(),
            CliCommand::Backup {
                to: Some("/tmp/b.db".into())
            }
        );
        assert_eq!(
            parse(&["restore", "--from", "/tmp/b.db"]).unwrap(),
            CliCommand::Restore {
                from: "/tmp/b.db".into()
            }
        );
        assert!(parse(&["restore"]).is_err(), "restore needs a file");
        assert_eq!(parse(&["version"]).unwrap(), CliCommand::Version);
        assert_eq!(
            parse(&["admin", "reset-token"]).unwrap(),
            CliCommand::Admin {
                command: AdminCommand::ResetToken
            }
        );
        assert_eq!(
            parse(&["update", "apply"]).unwrap(),
            CliCommand::Update {
                command: UpdateCommand::Apply
            }
        );
        assert_eq!(
            parse(&["update", "rollback"]).unwrap(),
            CliCommand::Update {
                command: UpdateCommand::Rollback
            }
        );
        assert!(parse(&["update"]).is_err());
        assert!(parse(&["nonsense"]).is_err());
    }

    #[test]
    fn invalid_log_filter_falls_back_to_info() {
        let (filter, warning) = log_filter("info");
        assert_eq!(filter.to_string(), "info");
        assert_eq!(warning, None);

        let (filter, warning) = log_filter("mtpool_registrar=nonsense_level");
        assert_eq!(filter.to_string(), "info");
        let warning = warning.expect("a bad directive is reported");
        assert!(warning.contains("MTPOOL_LOG"), "{warning}");
        assert!(
            warning.contains("mtpool_registrar=nonsense_level"),
            "{warning}"
        );
    }
}
