//! Things that go wrong on the stand, on purpose: a network that loses packets, an engine that
//! stops, a registrar whose probes are dropped, a registrar that restarts, a database that is
//! full.

use std::time::Duration;

use crate::{Node, Result, Stand, fail, published, run, run_raw, wait_ready};

/// The port the engines of the stand listen on and the registrar probes.
const PROXY_PORT: u16 = 443;

impl Node<'_> {
    /// Makes the node's network drop `loss_pct` percent of its packets (`tc netem`); 0 removes
    /// it again.
    pub async fn netem(&self, loss_pct: u32) -> Result<()> {
        let dev = r#"dev=$(ip route show default | awk '{print $5; exit}')"#;
        let command = if loss_pct == 0 {
            format!("{dev}; tc qdisc del dev \"$dev\" root 2>/dev/null || true")
        } else {
            format!("{dev}; tc qdisc replace dev \"$dev\" root netem loss {loss_pct}%")
        };
        self.exec(&command).await.map(|_| ())
    }

    /// Stops the proxy engine; the agent stays up.
    pub async fn stop_engine(&self) -> Result<()> {
        self.exec("systemctl stop telemt").await.map(|_| ())
    }

    /// What the agent has logged since it was last started.
    pub async fn agent_log(&self) -> Result<String> {
        self.exec("journalctl -u mtpool-agent --no-pager -n 80 2>&1 || true")
            .await
    }

    pub async fn start_engine(&self) -> Result<()> {
        self.exec("systemctl start telemt").await.map(|_| ())
    }

    /// Drops the registrar's TCP connections to the engine's port, as a firewall that the
    /// registrar's probes cannot pass would. Everything else (the agent's heartbeats too) goes
    /// on as before.
    pub async fn block_registrar_probe(&self) -> Result<()> {
        self.exec(&format!(
            "reg=$(getent hosts registrar | awk '{{print $1; exit}}') && test -n \"$reg\" \
             && iptables -I INPUT -s \"$reg\" -p tcp --dport {PROXY_PORT} -j DROP"
        ))
        .await
        .map(|_| ())
    }

    pub async fn unblock_registrar_probe(&self) -> Result<()> {
        self.exec(&format!(
            "reg=$(getent hosts registrar | awk '{{print $1; exit}}') && test -n \"$reg\" \
             && iptables -D INPUT -s \"$reg\" -p tcp --dport {PROXY_PORT} -j DROP"
        ))
        .await
        .map(|_| ())
    }
}

impl Stand {
    /// Restarts the registrar's container (its database stays) and waits until it answers again.
    /// Its published port is a new one afterwards; the stand follows it.
    pub async fn restart_registrar(&self) -> Result<()> {
        if self.small_db {
            return fail(
                "the database of this stand is on a tmpfs and would be lost with a restart",
            );
        }
        run(
            self.guard
                .compose()
                .args(["restart", "--timeout", "10", "registrar"]),
            "docker compose restart registrar",
            None,
        )
        .await?;
        let url = published(&self.guard, "registrar", "8080").await?;
        self.registrar_url.set(url.clone());
        wait_ready(&self.http, &url).await
    }

    /// Fills the registrar's database volume, so that its next write fails. Needs a stand made
    /// with [`crate::StandOptions::small_db`].
    pub async fn break_db_writes(&self) -> Result<()> {
        if !self.small_db {
            return fail("break_db_writes needs a stand made with StandOptions { small_db: true }");
        }
        // `dd` ends with "No space left on device": that is the point.
        let filled = run_raw(
            self.guard.compose().args([
                "exec",
                "-T",
                "registrar",
                "sh",
                "-c",
                "dd if=/dev/zero of=/var/lib/mtpool/filler bs=64k 2>&1; df -k /var/lib/mtpool",
            ]),
            "fill the registrar's volume",
            None,
        )
        .await?;
        if !filled.stdout.contains("No space left") {
            return fail(format!(
                "the registrar's volume did not fill up:\n{}",
                filled.stdout
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }
}
