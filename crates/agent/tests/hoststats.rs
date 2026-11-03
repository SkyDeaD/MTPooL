//! The host's load, memory and disk, from samples in the shape `/proc` has them.

use std::path::Path;

use mtpool_agent::hoststats::{
    HostStats, LOADAVG_PATH, MEMINFO_PATH, parse_loadavg, parse_meminfo,
};
use mtpool_agent::state_files::{DiskUsage, Fs, MemFs, RealFs};

const LOADAVG: &str = "0.52 0.58 0.59 1/467 12345\n";
const MEMINFO: &str = "MemTotal:       16384256 kB\n\
MemFree:         1200000 kB\n\
MemAvailable:   10000000 kB\n\
Buffers:          300000 kB\n\
Cached:          5000000 kB\n\
SwapTotal:       2097148 kB\n";

fn host(loadavg: Option<&str>, meminfo: Option<&str>, disk: Option<DiskUsage>) -> MemFs {
    let fs = MemFs::new();
    if let Some(text) = loadavg {
        fs.write_private(Path::new(LOADAVG_PATH), text.as_bytes())
            .unwrap();
    }
    if let Some(text) = meminfo {
        fs.write_private(Path::new(MEMINFO_PATH), text.as_bytes())
            .unwrap();
    }
    if let Some(usage) = disk {
        fs.set_disk_usage(usage);
    }
    fs
}

const DISK: DiskUsage = DiskUsage {
    total_bytes: 50_000_000_000,
    used_bytes: 12_000_000_000,
};

#[test]
fn hoststats_parse_proc_samples() {
    assert_eq!(parse_loadavg(LOADAVG), Some(0.52));
    assert_eq!(
        parse_meminfo(MEMINFO),
        Some((16_384_256 * 1024, 10_000_000 * 1024))
    );

    let stats = HostStats::read(&host(Some(LOADAVG), Some(MEMINFO), Some(DISK))).unwrap();
    assert_eq!(stats.load1, 0.52);
    assert_eq!(stats.mem_total_bytes, 16_384_256 * 1024);
    assert_eq!(stats.mem_used_bytes, (16_384_256 - 10_000_000) * 1024);
    assert_eq!(stats.disk_total_bytes, 50_000_000_000);
    assert_eq!(stats.disk_used_bytes, 12_000_000_000);

    let report = stats.into_report();
    assert_eq!(report.load1, 0.52);
    assert_eq!(report.mem_used_bytes, stats.mem_used_bytes);
}

#[test]
fn unreadable_samples_say_nothing() {
    for text in ["", "abc 1 2", "NaN 0 0", "-1.0 0 0", "inf 0 0"] {
        assert_eq!(parse_loadavg(text), None, "{text:?}");
    }
    for text in [
        "",
        "MemTotal: 100 kB\n",
        "MemAvailable: 100 kB\n",
        "MemTotal: lots kB\nMemAvailable: 1 kB\n",
        // Available more than the total cannot be: not a sample to trust.
        "MemTotal: 100 kB\nMemAvailable: 200 kB\n",
    ] {
        assert_eq!(parse_meminfo(text), None, "{text:?}");
    }
    // A missing part is no report: not a Linux host, or a /proc that is closed.
    assert_eq!(
        HostStats::read(&host(None, Some(MEMINFO), Some(DISK))),
        None
    );
    assert_eq!(
        HostStats::read(&host(Some(LOADAVG), None, Some(DISK))),
        None
    );
    assert_eq!(
        HostStats::read(&host(Some(LOADAVG), Some(MEMINFO), None)),
        None
    );
}

#[test]
fn the_real_disk_has_a_size() {
    let usage = RealFs.disk_usage(Path::new("/")).unwrap();
    assert!(usage.total_bytes > 0);
    assert!(usage.used_bytes <= usage.total_bytes);
}
