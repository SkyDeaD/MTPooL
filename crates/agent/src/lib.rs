//! Node agent: enrolls with the registrar, keeps its credentials in private state files and
//! runs the heartbeat, report and config loops. The agent opens no listening socket.

pub mod antiscan;
pub mod check_host;
pub mod client;
pub mod config;
pub mod dead_kill;
pub mod engine;
pub mod gp;
pub mod host;
pub mod hoststats;
pub mod ip_detect;
pub mod lifecycle;
pub mod local_health;
pub mod port_filter;
pub mod report;
pub mod state_files;
pub mod update;

/// The public key releases are verified with, as built in (empty in a debug build made without
/// the key file).
pub const MINISIGN_PUB: &str = env!("MTPOOL_MINISIGN_PUB");
