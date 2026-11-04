//! The command line.

use clap::Parser;

/// Simulated nodes and public viewers against a registrar. The admin token comes from the
/// environment (`MTPOOL_ADMIN_TOKEN`), never from the command line.
#[derive(Debug, Clone, Parser)]
#[command(name = "mtpool-loadgen", version)]
pub struct Args {
    /// Simulated nodes.
    #[arg(long, default_value_t = 1000)]
    pub nodes: u32,
    /// Seconds of the measured steady state.
    #[arg(long, default_value_t = 300)]
    pub duration: u64,
    /// Seconds over which the nodes enroll.
    #[arg(long, default_value_t = 60)]
    pub ramp: u64,
    /// Seconds after the last enrollment before the measuring starts (the nodes become healthy
    /// in this time).
    #[arg(long, default_value_t = 90)]
    pub warmup: u64,
    /// Public viewers, each polling the pool pages as the web UI does.
    #[arg(long, default_value_t = 300)]
    pub viewers: u32,
    /// Registrar base URL.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    pub registrar: String,
    /// Base URL of the fake Globalping (the part before `/v1/measurements`).
    #[arg(long, default_value = "http://fakes:9000/globalping")]
    pub globalping: String,
    /// The proxy port the simulated nodes enroll with; the loadgen listens on it so the
    /// registrar's TCP probes succeed.
    #[arg(long, default_value_t = 443)]
    pub node_port: u16,
    /// Pid of the registrar in this pid namespace, for its memory and CPU.
    #[arg(long)]
    pub registrar_pid: Option<u32>,
    /// Admin token, from the environment.
    #[arg(long, env = "MTPOOL_ADMIN_TOKEN", hide_env_values = true, hide = true)]
    pub admin_token: String,
    /// Where to write the JSON report (stdout when absent).
    #[arg(long)]
    pub report: Option<std::path::PathBuf>,
    /// Do not create the owner, the enroll token and the settings (the registrar is ready).
    #[arg(long, default_value_t = false)]
    pub skip_setup: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<Args, clap::Error> {
        let mut argv = vec!["mtpool-loadgen", "--admin-token", "x"];
        argv.extend_from_slice(list);
        Args::try_parse_from(argv)
    }

    #[test]
    fn defaults_are_the_thousand_nodes_and_five_minutes() {
        let a = parse(&[]).unwrap();
        assert_eq!((a.nodes, a.duration), (1000, 300));
        assert_eq!(a.node_port, 443);
        assert_eq!(a.registrar_pid, None);
    }

    #[test]
    fn flags_override() {
        let a = parse(&["--nodes", "50", "--duration", "20", "--registrar-pid", "1"]).unwrap();
        assert_eq!((a.nodes, a.duration, a.registrar_pid), (50, 20, Some(1)));
    }

    #[test]
    fn rubbish_is_refused() {
        assert!(parse(&["--nodes", "many"]).is_err());
    }
}
