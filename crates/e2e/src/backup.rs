//! The registrar's own commands on the stand: running one in its container and restoring a
//! backup while it is stopped.

use crate::{CommandRun, Result, Stand, published, run, run_raw, wait_ready};

impl Stand {
    /// Runs `command` (`sh -c`) in the registrar's container. A failure is an answer: what the
    /// command printed, stdout and stderr together, and whether it succeeded.
    pub async fn registrar_exec(&self, command: &str) -> Result<CommandRun> {
        let raw = run_raw(
            self.guard
                .compose()
                .args(["exec", "-T", "registrar", "sh", "-c", command]),
            "exec in the registrar",
            None,
        )
        .await?;
        Ok(CommandRun {
            success: raw.success,
            output: format!("{}{}", raw.stdout, raw.stderr),
        })
    }

    /// Stops the registrar, restores `from` (a path on the registrar's volume) into its database
    /// in a second container that shares the volume, and starts it again. Returns what
    /// `restore` printed.
    pub async fn restore_registrar(&self, from: &str) -> Result<CommandRun> {
        if self.small_db {
            return crate::fail("this stand's database is on a tmpfs and cannot be restored");
        }
        run(
            self.guard
                .compose()
                .args(["stop", "--timeout", "10", "registrar"]),
            "docker compose stop registrar",
            None,
        )
        .await?;
        let raw = run_raw(
            self.guard.compose().args([
                "run",
                "--rm",
                "--no-deps",
                "-T",
                "registrar",
                "mtpool-registrar",
                "restore",
                "--from",
                from,
            ]),
            "restore in a second registrar container",
            None,
        )
        .await?;
        run(
            self.guard.compose().args(["start", "registrar"]),
            "docker compose start registrar",
            None,
        )
        .await?;
        let url = published(&self.guard, "registrar", "8080").await?;
        self.registrar_url.set(url.clone());
        wait_ready(&self.http, &url).await?;
        Ok(CommandRun {
            success: raw.success,
            output: format!("{}{}", raw.stdout, raw.stderr),
        })
    }

    /// Starts the bare Ubuntu of the registrar installer (the compose service `registrar-fresh`,
    /// which is not part of the stand until asked for) and puts the registrar build of the stand
    /// at `/opt/mtpool-registrar` on it (not in `/tmp`, a tmpfs that `docker cp` cannot write to).
    pub async fn start_fresh_ubuntu(&self) -> Result<()> {
        run(
            self.guard.compose().args([
                "--profile",
                "fresh",
                "up",
                "-d",
                "--build",
                "--wait",
                "--no-deps",
                "registrar-fresh",
            ]),
            "docker compose up registrar-fresh",
            None,
        )
        .await?;
        let on_host = crate::release::scratch_dir("registrar-bin")?.join("mtpool-registrar");
        run(
            self.guard
                .compose()
                .arg("cp")
                .arg("registrar:/usr/local/bin/mtpool-registrar")
                .arg(&on_host),
            "docker compose cp of the registrar build",
            None,
        )
        .await?;
        run(
            self.guard
                .compose()
                .arg("cp")
                .arg(&on_host)
                .arg("registrar-fresh:/opt/mtpool-registrar"),
            "docker compose cp of the registrar build to the fresh machine",
            None,
        )
        .await?;
        let _ = std::fs::remove_file(on_host);
        Ok(())
    }

    /// Runs `command` (`sh -c`) on the fresh Ubuntu; a failure is an answer, as for
    /// [`Stand::registrar_exec`].
    pub async fn fresh_exec(&self, command: &str) -> Result<CommandRun> {
        self.fresh_exec_env(command, &[]).await
    }

    /// Like [`Stand::fresh_exec`], with `env` handed over as environment (not in the command
    /// line, so a secret in it shows in no log).
    pub async fn fresh_exec_env(&self, command: &str, env: &[(&str, &str)]) -> Result<CommandRun> {
        let mut compose = self.guard.compose();
        compose.args(["exec", "-T"]);
        for (name, _) in env {
            compose.args(["-e", name]);
        }
        compose.envs(env.iter().copied());
        let raw = run_raw(
            compose.args(["registrar-fresh", "sh", "-c", command]),
            "exec in the fresh machine",
            None,
        )
        .await?;
        Ok(CommandRun {
            success: raw.success,
            output: format!("{}{}", raw.stdout, raw.stderr),
        })
    }
}
