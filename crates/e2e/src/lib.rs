//! The Docker stand: the registrar, the stand-ins for the services outside the pool and three
//! systemd nodes, started with `docker compose` under a project name of their own. A [`Stand`]
//! is one running copy of it; dropping it takes the copy down again (set `MTPOOL_E2E_KEEP=1` to
//! keep it for a look). Everything here is for tests: nothing checks more than a test needs.

use std::fmt;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// A failure of the stand: what was tried and what came of it.
#[derive(Debug)]
pub struct StandError(String);

impl fmt::Display for StandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StandError {}

pub type Result<T> = std::result::Result<T, StandError>;

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(StandError(message.into()))
}

/// How often the public API is asked while waiting (its limit is 120 requests a minute).
const POLL_EVERY: Duration = Duration::from_secs(1);
/// The pool's name for the domain the stand serves; the fake Cloudflare knows `example.com`.
pub const DOMAIN: &str = "pool.example.com";
/// The token the stand-ins accept for every secret.
const FAKE_SECRET: &str = "e2e-fake-token";
/// The state directory of the agent on a node.
const STATE_DIR: &str = "/var/lib/mtpool";

fn compose_file() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../e2e/compose.yml");
    path.canonicalize().unwrap_or(path)
}

/// A project name no other run shares: six hex digits from the clock and the process.
fn project_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mixed = nanos.rotate_left(13) ^ std::process::id().wrapping_mul(2_654_435_761);
    format!("mtpool-e2e-{:06x}", mixed & 0x00ff_ffff)
}

/// The output of a finished command, failing with it unless it succeeded.
struct Output {
    stdout: String,
}

async fn run(command: &mut Command, what: &str, stdin: Option<&str>) -> Result<Output> {
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| StandError(format!("{what}: cannot start: {e}")))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes())
            .await
            .map_err(|e| StandError(format!("{what}: cannot write the input: {e}")))?;
        drop(pipe);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| StandError(format!("{what}: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return fail(format!(
            "{what} ended with {}\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
            output.status
        ));
    }
    Ok(Output { stdout })
}

/// The admin API of the registrar.
pub struct Admin {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Admin {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| StandError(format!("{method} {path}: {}", e.without_url())))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| StandError(format!("{method} {path}: {}", e.without_url())))?;
        if !status.is_success() {
            return fail(format!("{method} {path} answered {status}: {text}"));
        }
        serde_json::from_str(&text)
            .map_err(|e| StandError(format!("{method} {path}: not JSON ({e}): {text}")))
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }

    pub async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        self.call(reqwest::Method::PATCH, path, Some(body)).await
    }
}

/// One running copy of the stand.
pub struct Stand {
    /// Takes the project down when the stand is dropped.
    guard: Guard,
    registrar_url: String,
    fakes_url: String,
    http: reqwest::Client,
    pub admin: Admin,
    enroll_token: String,
}

impl Stand {
    /// Brings the stand up, waits for the registrar and sets the pool up: settings that make the
    /// pool quick, an owner and an enroll token.
    pub async fn up() -> Result<Stand> {
        let project = project_name();
        let file = compose_file();
        let guard = Guard {
            project: project.clone(),
            file: file.clone(),
        };
        eprintln!("e2e: project {project}");
        run(
            compose(&project, &file).args([
                "up",
                "-d",
                "--build",
                "--wait",
                "--wait-timeout",
                "240",
            ]),
            "docker compose up",
            None,
        )
        .await?;

        let registrar_url = published(&project, &file, "registrar", "8080").await?;
        let fakes_url = published(&project, &file, "fakes", "9000").await?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .no_proxy()
            .build()
            .map_err(|e| StandError(format!("cannot build the HTTP client: {e}")))?;
        wait_ready(&http, &registrar_url).await?;

        let token = run(
            compose(&project, &file).args([
                "exec",
                "-T",
                "registrar",
                "mtpool-registrar",
                "admin",
                "reset-token",
            ]),
            "admin reset-token",
            None,
        )
        .await?
        .stdout
        .trim()
        .to_owned();
        if token.is_empty() {
            return fail("admin reset-token printed no token");
        }
        let admin = Admin {
            http: http.clone(),
            base: registrar_url.clone(),
            token,
        };
        configure(&admin).await?;
        let owner = admin
            .post(
                "/api/v1/admin/owners",
                json!({"name": "e2e", "node_limit": 10}),
            )
            .await?;
        let enroll = admin
            .post(
                "/api/v1/admin/tokens",
                json!({"kind": "enroll", "owner_id": owner["id"]}),
            )
            .await?;
        let enroll_token = enroll["token"]
            .as_str()
            .ok_or_else(|| StandError(format!("no token in the answer: {enroll}")))?
            .to_owned();

        Ok(Stand {
            guard,
            registrar_url,
            fakes_url,
            http,
            admin,
            enroll_token,
        })
    }

    pub fn registrar_url(&self) -> &str {
        &self.registrar_url
    }

    pub fn fakes_url(&self) -> &str {
        &self.fakes_url
    }

    /// The node served by the compose service `node<n>`.
    pub fn node(&self, n: u32) -> Node<'_> {
        Node {
            stand: self,
            service: format!("node{n}"),
        }
    }

    /// Switches what a stand-in sensor answers (`globalping`, `cheburcheck`, `redflag` or
    /// `cloudflare`; the modes are those of `mtpool-fakes`).
    pub async fn set_sensor(&self, sensor: &str, mode: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/__control", self.fakes_url))
            .json(&json!({"sensor": sensor, "mode": mode}))
            .send()
            .await
            .map_err(|e| StandError(format!("set_sensor: {}", e.without_url())))?;
        if !response.status().is_success() {
            return fail(format!(
                "set_sensor {sensor} {mode}: {}: {}",
                response.status(),
                response.text().await.unwrap_or_default()
            ));
        }
        Ok(())
    }

    /// What the stand-ins have seen: the DNS records and the number of calls.
    pub async fn fakes_state(&self) -> Result<Value> {
        self.public(&self.fakes_url, "/__state").await
    }

    /// The public node list of the registrar.
    pub async fn nodes(&self) -> Result<Vec<Value>> {
        match self.public(&self.registrar_url, "/api/v1/nodes").await? {
            Value::Array(nodes) => Ok(nodes),
            other => fail(format!("the node list is not an array: {other}")),
        }
    }

    /// The public domain list of the registrar.
    pub async fn public_domains(&self) -> Result<Value> {
        self.public(&self.registrar_url, "/api/v1/domains").await
    }

    async fn public(&self, base: &str, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(format!("{base}{path}"))
            .send()
            .await
            .map_err(|e| StandError(format!("GET {path}: {}", e.without_url())))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return fail(format!("GET {path} answered {status}: {text}"));
        }
        serde_json::from_str(&text)
            .map_err(|e| StandError(format!("GET {path}: not JSON ({e}): {text}")))
    }

    /// Waits until the node is in `state` in the public list.
    pub async fn wait_state(&self, node_id: &str, state: &str, within: Duration) -> Result<()> {
        let deadline = Instant::now() + within;
        let mut last;
        loop {
            match self.nodes().await {
                Ok(nodes) => {
                    let ours = nodes.iter().find(|n| n["id"] == node_id);
                    if ours.is_some_and(|n| n["state"] == state) {
                        return Ok(());
                    }
                    last = format!("{ours:#?}");
                }
                Err(error) => last = error.to_string(),
            }
            if Instant::now() >= deadline {
                return fail(format!(
                    "node {node_id} did not reach {state} within {within:?}; last seen: {last}"
                ));
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }

    /// Waits until the node is among the masters of `domain`.
    pub async fn wait_master(&self, domain: &str, node_id: &str, within: Duration) -> Result<()> {
        let deadline = Instant::now() + within;
        let mut last;
        loop {
            match self.public_domains().await {
                Ok(domains) => {
                    let ours = domains
                        .as_array()
                        .and_then(|all| all.iter().find(|d| d["domain"] == domain));
                    if ours.is_some_and(|d| {
                        d["masters"]
                            .as_array()
                            .is_some_and(|m| m.iter().any(|id| id == node_id))
                    }) {
                        return Ok(());
                    }
                    last = format!("{ours:#?}");
                }
                Err(error) => last = error.to_string(),
            }
            if Instant::now() >= deadline {
                return fail(format!(
                    "node {node_id} did not become the master of {domain} within {within:?}; \
                     last seen: {last}"
                ));
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }

    /// Waits until the fake DNS holds an `A` record `domain -> ip`, and returns it. The record
    /// follows the master by a round, so it is awaited, not read once.
    pub async fn wait_dns_record(
        &self,
        domain: &str,
        ip: &str,
        within: Duration,
    ) -> Result<serde_json::Value> {
        let deadline = Instant::now() + within;
        let mut last;
        loop {
            match self.fakes_state().await {
                Ok(state) => {
                    let found = state["dns"].as_array().and_then(|records| {
                        records
                            .iter()
                            .find(|r| r["type"] == "A" && r["name"] == domain && r["content"] == ip)
                            .cloned()
                    });
                    if let Some(record) = found {
                        return Ok(record);
                    }
                    last = state.to_string();
                }
                Err(error) => last = error.to_string(),
            }
            if Instant::now() >= deadline {
                return fail(format!(
                    "no A record {domain} -> {ip} in the fake DNS within {within:?}; \
                     last seen: {last}"
                ));
            }
            tokio::time::sleep(POLL_EVERY).await;
        }
    }
}

/// Takes the project down when it goes out of scope, unless it is to be kept.
struct Guard {
    project: String,
    file: PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // The test failed: what the services said is the first thing to look at.
            let _ = StdCommand::new("docker")
                .args(["compose", "-p", &self.project, "-f"])
                .arg(&self.file)
                .args(["logs", "--no-color", "--tail", "80"])
                .status();
        }
        if std::env::var("MTPOOL_E2E_KEEP").as_deref() == Ok("1") {
            eprintln!(
                "e2e: keeping project {}; remove it with: docker compose -p {} down -v",
                self.project, self.project
            );
            return;
        }
        let _ = StdCommand::new("docker")
            .args(["compose", "-p", &self.project, "-f"])
            .arg(&self.file)
            .args(["down", "-v", "--remove-orphans", "--timeout", "5"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A node of the stand: one systemd container with telemt and the agent.
pub struct Node<'a> {
    stand: &'a Stand,
    service: String,
}

impl Node<'_> {
    /// Runs `command` in the node (`sh -c`), returning what it printed.
    pub async fn exec(&self, command: &str) -> Result<String> {
        self.exec_with(command, None).await
    }

    async fn exec_with(&self, command: &str, stdin: Option<&str>) -> Result<String> {
        let output = run(
            compose(&self.stand.guard.project, &self.stand.guard.file).args([
                "exec",
                "-T",
                &self.service,
                "sh",
                "-c",
                command,
            ]),
            &format!("exec in {}: {command}", self.service),
            stdin,
        )
        .await?;
        Ok(output.stdout)
    }

    /// The address of the node on the stand's network.
    pub async fn ip(&self) -> Result<String> {
        let out = self.exec("hostname -I").await?;
        out.split_whitespace()
            .next()
            .map(str::to_owned)
            .ok_or_else(|| StandError(format!("{} has no address", self.service)))
    }

    /// Gives the agent its configuration and enroll token, starts it, and returns the id the
    /// registrar gave the node.
    pub async fn enroll(&self, name: &str) -> Result<String> {
        let config = format!(
            "registrar = \"http://registrar:8080\"\nname = \"{name}\"\nantiscan = false\n\n\
             [intervals]\nheartbeat_sec = 1\nglobalping_sec = 5\nmetrics_sec = 1\nsync_sec = 1\n"
        );
        self.exec_with(
            "mkdir -p /etc/mtpool && cat > /etc/mtpool/agent.toml",
            Some(&config),
        )
        .await?;
        self.exec_with(
            &format!(
                "mkdir -p {STATE_DIR} && umask 077 && cat > {STATE_DIR}/enroll_token \
                 && chmod 600 {STATE_DIR}/enroll_token"
            ),
            Some(&self.stand.enroll_token),
        )
        .await?;
        self.exec("systemctl start mtpool-agent").await?;

        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let id = self
                .exec(&format!("cat {STATE_DIR}/node_id 2>/dev/null || true"))
                .await?;
            let id = id.trim();
            if !id.is_empty() {
                return Ok(id.to_owned());
            }
            if Instant::now() >= deadline {
                let journal = self
                    .exec("journalctl -u mtpool-agent --no-pager -n 40 2>&1 || true")
                    .await
                    .unwrap_or_default();
                return fail(format!(
                    "{} did not enroll within 60 seconds; the agent said:\n{journal}",
                    self.service
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

fn compose(project: &str, file: &PathBuf) -> Command {
    let mut command = Command::new("docker");
    command
        .args(["compose", "-p", project, "-f"])
        .arg(file)
        .kill_on_drop(true);
    command
}

/// The address on this machine of a port the compose file publishes.
async fn published(project: &str, file: &PathBuf, service: &str, port: &str) -> Result<String> {
    let out = run(
        compose(project, file).args(["port", service, port]),
        &format!("docker compose port {service} {port}"),
        None,
    )
    .await?
    .stdout;
    let line = out.lines().next().unwrap_or("").trim();
    let Some((_, number)) = line.rsplit_once(':') else {
        return fail(format!(
            "cannot read the published port of {service}: {out:?}"
        ));
    };
    if number.parse::<u16>().is_err() {
        return fail(format!(
            "cannot read the published port of {service}: {out:?}"
        ));
    }
    Ok(format!("http://127.0.0.1:{number}"))
}

async fn wait_ready(http: &reqwest::Client, base: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let answer = http.get(format!("{base}/readyz")).send().await;
        if answer.as_ref().is_ok_and(|r| r.status().is_success()) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return fail(format!("the registrar is not ready: {answer:?}"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The settings of the stand: the pool reacts in seconds, not minutes; the domain is one the
/// fake Cloudflare serves; every secret is the token the stand-ins accept.
async fn configure(admin: &Admin) -> Result<()> {
    admin
        .patch(
            "/api/v1/admin/settings",
            json!({
                "heartbeat_interval_sec": 1,
                "globalping_interval_sec": 5,
                "metrics_interval_sec": 2,
                "sync_interval_sec": 2,
                "heartbeat_ttl_sec": 5,
                "tcp_probe_interval_sec": 1,
                "round_interval_sec": 2,
                "round_interval_slow_sec": 5,
                "dead_after_sec": 30,
                "domains": [DOMAIN],
                "tls_domain": "www.example.com",
                "dns_ttl": 60,
                "proxy_users": [{"name": "e2e", "secret_hex": "0123456789abcdef0123456789abcdef"}],
                "secrets": {
                    "globalping_token": FAKE_SECRET,
                    "redflag_api_key": FAKE_SECRET,
                    "cloudflare_api_token": FAKE_SECRET,
                },
            }),
        )
        .await
        .map(|_| ())
}
