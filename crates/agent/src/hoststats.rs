//! What the host looks like: load, memory and disk, read from `/proc` and `statvfs` through
//! [`Fs`] so tests feed samples.

use std::path::Path;

use mtpool_protocol::agent::HostReport;

use crate::state_files::Fs;

pub const LOADAVG_PATH: &str = "/proc/loadavg";
pub const MEMINFO_PATH: &str = "/proc/meminfo";
/// The file system whose use is reported.
pub const DISK_PATH: &str = "/";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostStats {
    pub load1: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub disk_total_bytes: u64,
}

/// The one-minute load average from the text of `/proc/loadavg`.
pub fn parse_loadavg(text: &str) -> Option<f64> {
    let load: f64 = text.split_whitespace().next()?.parse().ok()?;
    (load.is_finite() && load >= 0.0).then_some(load)
}

/// `(MemTotal, MemAvailable)` in bytes from the text of `/proc/meminfo`.
pub fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let slot = match key {
            "MemTotal" => &mut total,
            "MemAvailable" => &mut available,
            _ => continue,
        };
        // The kernel writes kibibytes, `kB`.
        let mut words = rest.split_whitespace();
        let kib: u64 = words.next()?.parse().ok()?;
        if words.next() != Some("kB") {
            return None;
        }
        *slot = Some(kib.checked_mul(1024)?);
    }
    let (total, available) = (total?, available?);
    (available <= total).then_some((total, available))
}

impl HostStats {
    /// `None` if any of the three samples is missing or unreadable (not a Linux host, a
    /// restricted `/proc`): the report then says nothing about the host.
    pub fn read(fs: &dyn Fs) -> Option<HostStats> {
        let text = |path: &str| {
            let bytes = fs.read(Path::new(path)).ok()??;
            String::from_utf8(bytes).ok()
        };
        let load1 = parse_loadavg(&text(LOADAVG_PATH)?)?;
        let (mem_total_bytes, available) = parse_meminfo(&text(MEMINFO_PATH)?)?;
        let disk = fs.disk_usage(Path::new(DISK_PATH)).ok()?;
        Some(HostStats {
            load1,
            mem_used_bytes: mem_total_bytes - available,
            mem_total_bytes,
            disk_used_bytes: disk.used_bytes,
            disk_total_bytes: disk.total_bytes,
        })
    }

    pub fn into_report(self) -> HostReport {
        HostReport {
            load1: self.load1,
            mem_used_bytes: self.mem_used_bytes,
            mem_total_bytes: self.mem_total_bytes,
            disk_used_bytes: self.disk_used_bytes,
            disk_total_bytes: self.disk_total_bytes,
        }
    }
}
