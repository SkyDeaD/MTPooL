//! The Docker stand: the registrar, the stand-ins for the services outside the pool and three
//! systemd nodes, started with `docker compose` under a project name of their own. A [`Stand`]
//! is one running copy of it; dropping it takes the copy down again (set `MTPOOL_E2E_KEEP=1` to
//! keep it for a look, and `MTPOOL_E2E_LOG_DIR=<dir>` to save the logs of a stand that failed).
//! Everything here is for tests: nothing checks more than a test needs.

mod backup;
mod faults;
mod install;
mod observe;
mod release;
mod signing;
mod traffic;
mod update;

pub use install::{InstallOptions, Installed, Token};
pub use observe::eventually;
pub use release::{Release, ReleaseSpec};
pub use signing::{TestKey, ensure_test_key};
pub use update::{RegistrarRelease, pool_release, registrar_release};

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
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

/// An address that can change under the holders of it: the registrar's published port is a new
/// one after the container restarts.
#[derive(Clone)]
struct Url(Arc<RwLock<String>>);

impl Url {
    fn new(value: String) -> Url {
        Url(Arc::new(RwLock::new(value)))
    }

    fn get(&self) -> String {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn set(&self, value: String) {
        *self
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = value;
    }
}

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(StandError(message.into()))
}

/// How often the public API is asked while waiting (its limit is 120 requests a minute).
const POLL_EVERY: Duration = Duration::from_secs(1);
/// The pool's name for the domain the stand serves; the fake Cloudflare knows `example.com`.
pub const DOMAIN: &str = "pool.example.com";
/// The TLS domain the proxies of the stand impersonate.
pub const TLS_DOMAIN: &str = "www.example.com";
/// The secret of the stand's proxy user `e2e`: a value for tests, not a secret.
pub const PROXY_USER_SECRET: &str = "0123456789abcdef0123456789abcdef";
/// The token the stand-ins accept for every secret.
const FAKE_SECRET: &str = "e2e-fake-token";
/// The state directory of the agent on a node.
const STATE_DIR: &str = "/var/lib/mtpool";

fn compose_file() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../e2e/compose.yml");
    path.canonicalize().unwrap_or(path)
}

/// What `docker compose up` does about the images: builds them, unless the variable
/// `MTPOOL_E2E_PREBUILT=1` says they are loaded already. Then a missing image is an error.
fn build_flag() -> &'static str {
    build_flag_for(prebuilt())
}

fn prebuilt() -> bool {
    std::env::var("MTPOOL_E2E_PREBUILT").as_deref() == Ok("1")
}

fn build_flag_for(prebuilt: bool) -> &'static str {
    if prebuilt { "--no-build" } else { "--build" }
}

/// What the compose file is told about the images. Built here, they go by the id of the key they
/// embed: another working copy has another key, so its builds neither replace these images nor
/// share their cargo target. Loaded images keep the compose file's default tag.
fn image_env_for(prebuilt: bool, key: &TestKey) -> Vec<(String, String)> {
    if prebuilt {
        Vec::new()
    } else {
        vec![("MTPOOL_E2E_TAG".to_owned(), key.id())]
    }
}

/// The file for the logs of a failed stand: the test it served and its project.
fn log_path(dir: &Path, test: Option<&str>, project: &str) -> PathBuf {
    let test = test.unwrap_or("stand").replace("::", "-");
    dir.join(format!("{test}-{project}.log"))
}

/// A project name nothing else shares: six hex digits from the clock and the process tell runs
/// apart, a counter tells apart the stands of one run, which start within the same clock tick.
fn project_name() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mixed = nanos.rotate_left(13) ^ std::process::id().wrapping_mul(2_654_435_761);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("mtpool-e2e-{:06x}-{serial}", mixed & 0x00ff_ffff)
}

/// The output of a finished command, failing with it unless it succeeded.
struct Output {
    stdout: String,
}

/// What a command printed and whether it succeeded; [`run_raw`] does not call a failure an error.
struct Raw {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn run(command: &mut Command, what: &str, stdin: Option<&str>) -> Result<Output> {
    let raw = run_raw(command, what, stdin).await?;
    if !raw.success {
        return fail(format!(
            "{what} failed\n--- stdout\n{}\n--- stderr\n{}",
            raw.stdout, raw.stderr
        ));
    }
    Ok(Output { stdout: raw.stdout })
}

async fn run_raw(command: &mut Command, what: &str, stdin: Option<&str>) -> Result<Raw> {
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
    Ok(Raw {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The admin API of the registrar.
pub struct Admin {
    http: reqwest::Client,
    base: Url,
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
            .request(method.clone(), format!("{}{path}", self.base.get()))
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

/// What a command did: whether it succeeded and everything it printed.
#[derive(Debug, Clone)]
pub struct CommandRun {
    pub success: bool,
    pub output: String,
}

/// What a stand is brought up with.
#[derive(Debug, Clone, Default)]
pub struct StandOptions {
    /// Put the registrar's database on a 2 MB tmpfs that [`Stand::break_db_writes`] can fill.
    /// Such a database goes with the container, so the registrar of this stand is not restarted.
    pub small_db: bool,
    /// Have the registrar poll the GitHub stand-in for releases, as an installed one polls GitHub.
    pub self_update: bool,
}

/// One running copy of the stand.
pub struct Stand {
    /// Takes the project down when the stand is dropped.
    guard: Guard,
    registrar_url: Url,
    fakes_url: String,
    http: reqwest::Client,
    pub admin: Admin,
    enroll_token: String,
    /// Whether the registrar's database is on the small tmpfs.
    small_db: bool,
}

impl Stand {
    /// Brings the stand up, waits for the registrar and sets the pool up: settings that make the
    /// pool quick, an owner and an enroll token.
    pub async fn up() -> Result<Stand> {
        Stand::up_with(StandOptions::default()).await
    }

    /// [`Stand::up`] with the options given.
    pub async fn up_with(options: StandOptions) -> Result<Stand> {
        // The images embed the public half of the stand's key; it has to exist before they build.
        let key = ensure_test_key()?;
        let project = project_name();
        let mut files = vec![compose_file()];
        if options.small_db {
            files.push(compose_file().with_file_name("compose.small-db.yml"));
        }
        if options.self_update {
            files.push(compose_file().with_file_name("compose.self-update.yml"));
        }
        let mut guard = Guard {
            project: project.clone(),
            files,
            env: image_env_for(prebuilt(), &key),
            up: false,
        };
        eprintln!("e2e: project {project}");
        run(
            guard
                .compose()
                .args(["up", "-d", build_flag(), "--wait", "--wait-timeout", "240"]),
            "docker compose up",
            None,
        )
        .await?;

        let registrar_url = Url::new(published(&guard, "registrar", "8080").await?);
        let fakes_url = published(&guard, "fakes", "9000").await?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .no_proxy()
            .build()
            .map_err(|e| StandError(format!("cannot build the HTTP client: {e}")))?;
        wait_ready(&http, &registrar_url.get()).await?;

        let token = run(
            guard.compose().args([
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

        guard.up = true;
        Ok(Stand {
            guard,
            registrar_url,
            fakes_url,
            http,
            admin,
            enroll_token,
            small_db: options.small_db,
        })
    }

    pub fn registrar_url(&self) -> String {
        self.registrar_url.get()
    }

    /// The enroll token of the stand's owner. A secret: it goes to a node by stdin or a file.
    pub fn enroll_token(&self) -> &str {
        &self.enroll_token
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
        match self
            .public(&self.registrar_url.get(), "/api/v1/nodes")
            .await?
        {
            Value::Array(nodes) => Ok(nodes),
            other => fail(format!("the node list is not an array: {other}")),
        }
    }

    /// The public domain list of the registrar.
    pub async fn public_domains(&self) -> Result<Value> {
        self.public(&self.registrar_url.get(), "/api/v1/domains")
            .await
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

    /// Waits until the public API says the node is in `state`.
    pub async fn wait_state(&self, node_id: &str, state: &str, within: Duration) -> Result<()> {
        let deadline = Instant::now() + within;
        let mut last;
        loop {
            match self.node_state(node_id).await {
                Ok(seen) => {
                    if seen.as_deref() == Some(state) {
                        return Ok(());
                    }
                    last = format!("{seen:?}");
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
    /// The compose file, then the files that override it.
    files: Vec<PathBuf>,
    /// Variables of every `docker compose` command (the compose file reads some).
    env: Vec<(String, String)>,
    /// Whether the stand came up. One that did not has no test left to panic for it.
    up: bool,
}

impl Guard {
    /// `-f <file>` for each compose file.
    fn file_args(&self) -> Vec<std::ffi::OsString> {
        let mut args = Vec::new();
        for file in &self.files {
            args.push("-f".into());
            args.push(file.into());
        }
        args
    }

    /// `docker compose` for this project.
    fn compose(&self) -> Command {
        let mut command = Command::new("docker");
        command
            .args(["compose", "-p", &self.project])
            .args(self.file_args())
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .kill_on_drop(true);
        command
    }

    /// Saves everything the services said under `MTPOOL_E2E_LOG_DIR`, if it is set. It runs while
    /// a test unwinds, so a failure here is only reported.
    fn save_logs(&self) {
        let Some(dir) = std::env::var_os("MTPOOL_E2E_LOG_DIR") else {
            return;
        };
        let dir = PathBuf::from(dir);
        let path = log_path(&dir, std::thread::current().name(), &self.project);
        let file = match std::fs::create_dir_all(&dir).and_then(|()| std::fs::File::create(&path)) {
            Ok(file) => file,
            Err(e) => {
                eprintln!("e2e: cannot write {}: {e}", path.display());
                return;
            }
        };
        let saved = StdCommand::new("docker")
            .args(["compose", "-p", &self.project, "--profile", "*"])
            .args(self.file_args())
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .args(["logs", "--no-color"])
            .stdout(file)
            .stderr(Stdio::null())
            .status();
        if let Err(e) = saved {
            eprintln!("e2e: cannot save the logs of {}: {e}", self.project);
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if std::thread::panicking() || !self.up {
            self.save_logs();
            // The test failed: what the services said is the first thing to look at.
            let _ = StdCommand::new("docker")
                .args(["compose", "-p", &self.project, "--profile", "*"])
                .args(self.file_args())
                .envs(self.env.iter().map(|(k, v)| (k, v)))
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
            .args(["compose", "-p", &self.project, "--profile", "*"])
            .args(self.file_args())
            .envs(self.env.iter().map(|(k, v)| (k, v)))
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
            &mut self.exec_command(command, &[]),
            &format!("exec in {}: {command}", self.service),
            stdin,
        )
        .await?;
        Ok(output.stdout)
    }

    /// `docker compose exec` of `sh -c command`; `env` is handed to the command as environment
    /// (not as an argument, so it does not show in the process list).
    fn exec_command(&self, command: &str, env: &[(&str, &str)]) -> Command {
        let mut c = self.stand.guard.compose();
        c.args(["exec", "-T"]);
        for (name, _) in env {
            // `-e NAME` takes the value from this process's environment.
            c.args(["-e", name]);
        }
        c.args([&self.service, "sh", "-c", command]);
        c.envs(env.iter().copied());
        c
    }

    /// Like [`Node::exec`], but a failure is an answer, not an error: what the command printed
    /// (stdout and stderr together) and whether it succeeded.
    pub async fn try_exec(&self, command: &str, env: &[(&str, &str)]) -> Result<CommandRun> {
        self.try_exec_with(command, env, None).await
    }

    async fn try_exec_with(
        &self,
        command: &str,
        env: &[(&str, &str)],
        stdin: Option<&str>,
    ) -> Result<CommandRun> {
        let raw = run_raw(
            &mut self.exec_command(command, env),
            &format!("exec in {}", self.service),
            stdin,
        )
        .await?;
        Ok(CommandRun {
            success: raw.success,
            output: format!("{}{}", raw.stdout, raw.stderr),
        })
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
        self.wait_node_id(Duration::from_secs(60)).await
    }

    /// The id the registrar gave this node, once the agent has enrolled; `None` before.
    pub async fn node_id(&self) -> Result<Option<String>> {
        let id = self
            .exec(&format!("cat {STATE_DIR}/node_id 2>/dev/null || true"))
            .await?;
        let id = id.trim();
        Ok((!id.is_empty()).then(|| id.to_owned()))
    }

    /// Waits for the agent on this node to enroll, and returns the node's id.
    pub async fn wait_node_id(&self, within: Duration) -> Result<String> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(id) = self.node_id().await? {
                return Ok(id);
            }
            if Instant::now() >= deadline {
                let journal = self
                    .exec("journalctl -u mtpool-agent --no-pager -n 40 2>&1 || true")
                    .await
                    .unwrap_or_default();
                return fail(format!(
                    "{} did not enroll within {within:?}; the agent said:\n{journal}",
                    self.service
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// The address on this machine of a port the compose file publishes.
async fn published(guard: &Guard, service: &str, port: &str) -> Result<String> {
    let out = run(
        guard.compose().args(["port", service, port]),
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
                "tls_domain": TLS_DOMAIN,
                "dns_ttl": 60,
                "proxy_users": [{"name": "e2e", "secret_hex": PROXY_USER_SECRET}],
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

#[cfg(test)]
mod build_flag_tests {
    use super::{TestKey, build_flag_for, image_env_for};

    #[test]
    fn images_are_built_unless_they_came_prebuilt() {
        assert_eq!(build_flag_for(false), "--build");
        assert_eq!(build_flag_for(true), "--no-build");
    }

    #[test]
    fn built_images_go_by_the_key_and_loaded_ones_keep_their_tag() {
        let key = TestKey::generate().expect("a key");
        assert_eq!(
            image_env_for(false, &key),
            [("MTPOOL_E2E_TAG".to_owned(), key.id())]
        );
        assert!(image_env_for(true, &key).is_empty());
        assert_ne!(key.id(), TestKey::generate().expect("a key").id());
    }
}

#[cfg(test)]
mod log_path_tests {
    use super::log_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn the_file_says_which_test_and_which_stand() {
        assert_eq!(
            log_path(Path::new("/logs"), Some("ops::restore"), "mtpool-e2e-ab-0"),
            PathBuf::from("/logs/ops-restore-mtpool-e2e-ab-0.log")
        );
        assert_eq!(
            log_path(Path::new("/logs"), None, "mtpool-e2e-ab-0"),
            PathBuf::from("/logs/stand-mtpool-e2e-ab-0.log")
        );
    }
}

#[cfg(test)]
mod project_name_tests {
    use super::project_name;
    use std::collections::HashSet;

    #[test]
    fn project_names_never_repeat_inside_one_run() {
        let names: HashSet<String> = (0..4096).map(|_| project_name()).collect();
        assert_eq!(names.len(), 4096);
    }
}
