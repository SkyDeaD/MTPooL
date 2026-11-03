//! Which engine the host runs, and which systemd unit it runs in.
//!
//! The managers leave a directory under `/opt`: MEKO (`/opt/mtpr-simple`) and MTProxyL
//! (`/opt/mtproxyl`). A host with neither runs a plain telemt. The units are the names the
//! managers are known to give the telemt service.

use std::path::Path;

use super::EngineKind;
use crate::host::CommandRunner;
use crate::state_files::Fs;

pub const MEKO_DIR: &str = "/opt/mtpr-simple";
pub const MTPROXYL_DIR: &str = "/opt/mtproxyl";

/// The unit names to look for, in the order they are tried.
pub const SERVICE_UNIT_CANDIDATES: [&str; 3] = [
    "telemt.service",
    "mtproxy.service",
    "mtproxyl-telemt.service",
];

/// MEKO's directory wins over MTProxyL's, and either wins over a plain telemt. A directory that
/// cannot be asked about is taken as not there.
pub fn detect_engine(fs: &dyn Fs) -> EngineKind {
    let there = |dir: &str| fs.exists(Path::new(dir)).unwrap_or(false);
    if there(MEKO_DIR) {
        EngineKind::Meko
    } else if there(MTPROXYL_DIR) {
        EngineKind::MtProxyL
    } else {
        EngineKind::Telemt
    }
}

/// The first candidate unit that `systemctl cat` knows. `None` if none is, or if systemd cannot
/// be asked at all.
pub async fn find_service_unit<R: CommandRunner>(runner: &R) -> Option<String> {
    for unit in SERVICE_UNIT_CANDIDATES {
        match runner.run("systemctl", &["cat", unit], None).await {
            Ok(output) if output.status.success() => return Some(unit.to_owned()),
            Ok(_) => {}
            Err(error) => tracing::debug!("cannot ask systemd about {unit}: {error}"),
        }
    }
    None
}
