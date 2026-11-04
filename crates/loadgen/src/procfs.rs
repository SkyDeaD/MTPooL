//! The registrar's memory and CPU from `/proc`, read through the shared pid namespace.

/// What one look at a process gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcSample {
    pub rss_kb: u64,
    pub hwm_kb: u64,
    /// utime + stime in clock ticks.
    pub cpu_ticks: u64,
}

/// `VmRSS` and `VmHWM` (kB) from the text of `/proc/<pid>/status`.
pub fn parse_status(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            line.strip_prefix(name)?
                .trim()
                .strip_suffix("kB")?
                .trim()
                .parse()
                .ok()
        })
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}

/// utime + stime (clock ticks) from the text of `/proc/<pid>/stat`.
pub fn parse_stat_ticks(text: &str) -> Option<u64> {
    // The name is in parentheses and may hold spaces: count from the last closing one. After it
    // come the state (field 3), then fields 4.. so utime (14) and stime (15) are at 11 and 12.
    let rest = text.get(text.rfind(')')? + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

pub fn read(pid: u32) -> Option<ProcSample> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (rss_kb, hwm_kb) = parse_status(&status)?;
    Some(ProcSample {
        rss_kb,
        hwm_kb,
        cpu_ticks: parse_stat_ticks(&stat)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "Name:\tmtpool-registrar\nVmPeak:\t  300000 kB\nVmHWM:\t   52340 kB\nVmRSS:\t   41200 kB\nThreads:\t12\n";

    #[test]
    fn status_gives_rss_and_peak() {
        assert_eq!(parse_status(STATUS), Some((41200, 52340)));
    }

    #[test]
    fn status_without_vmrss_is_none() {
        assert_eq!(parse_status("Name:\tx\n"), None);
    }

    #[test]
    fn stat_sums_user_and_system_ticks_even_with_spaces_in_the_name() {
        // fields after the closing parenthesis: state ppid ... utime(14) stime(15)
        let line =
            "1 (mtpool (reg)) S 0 1 1 0 -1 4194560 100 0 0 0 250 70 0 0 20 0 12 0 5 1000 2000";
        assert_eq!(parse_stat_ticks(line), Some(320));
    }

    #[test]
    fn broken_stat_is_none() {
        assert_eq!(parse_stat_ticks("1 (x) S 0"), None);
    }
}
