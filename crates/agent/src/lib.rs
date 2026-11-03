//! Node agent: enrolls with the registrar, keeps its credentials in private state files and
//! runs the heartbeat, report and config loops. The agent opens no listening socket.

pub mod client;
pub mod config;
pub mod engine;
pub mod host;
pub mod lifecycle;
pub mod state_files;
