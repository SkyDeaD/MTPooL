//! Antiscan: a drop list of scanner addresses on the node, kept in an ipset behind an iptables
//! chain. The list is downloaded every 30 minutes and put in place with one `ipset restore` that
//! fills a temporary set and swaps it with the live one, so the live set is never half filled.

use std::collections::HashSet;
use std::future::Future;
use std::net::Ipv4Addr;
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mtpool_protocol::agent::AgentConfig;
use tokio::sync::{Mutex, Notify};
use tokio::time::Instant;

use crate::host::CommandRunner;
use crate::lifecycle::ConfigSink;

/// Level 1 of the IPsum list: every address that is on at least one blocklist.
pub const IPSUM_URL: &str = "https://raw.githubusercontent.com/stamparm/ipsum/master/levels/1.txt";
/// The name of the chain and of the live set.
pub const NAME: &str = "ANTISCAN_MTPOOL";
/// The set the new list is built in before it is swapped in.
pub const TMP_NAME: &str = "ANTISCAN_MTPOOL_tmp";
/// How many addresses a set holds (`maxelem`); a longer list is cut to it.
pub const MAX_ELEMENTS: usize = 262_144;
/// How often the list is downloaded again.
pub const REFRESH_EVERY: Duration = Duration::from_secs(30 * 60);
/// How soon a failed refresh is tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
/// How often `run` looks whether there is work.
pub const TICK: Duration = Duration::from_secs(30);
/// The download gives up after this long.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);
/// The download gives up when the list is longer than this.
pub const MAX_DOWNLOAD_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AntiscanError {
    #[error("cannot download the list: {0}")]
    Download(String),
    #[error("the list holds no usable address")]
    Empty,
    #[error("host command failed: {0}")]
    Host(String),
}

/// Where the list comes from.
pub trait ListSource: Send + Sync {
    fn fetch(&self) -> impl Future<Output = Result<String, String>> + Send;
}

/// The list over HTTPS: no redirects, no proxy, 20 s, at most 8 MiB.
#[derive(Debug, Clone)]
pub struct IpsumHttp {
    client: reqwest::Client,
    url: String,
}

impl IpsumHttp {
    pub fn new() -> Result<IpsumHttp, reqwest::Error> {
        IpsumHttp::with_url(IPSUM_URL)
    }
    pub fn with_url(url: &str) -> Result<IpsumHttp, reqwest::Error> {
        let client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(IpsumHttp {
            client,
            url: url.to_owned(),
        })
    }
}

impl ListSource for IpsumHttp {
    async fn fetch(&self) -> Result<String, String> {
        let mut response = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the server answered HTTP {}", status.as_u16()));
        }
        let too_long = || format!("the list is too long (over {MAX_DOWNLOAD_BYTES} bytes)");
        if response
            .content_length()
            .is_some_and(|n| n > MAX_DOWNLOAD_BYTES as u64)
        {
            return Err(too_long());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| e.without_url().to_string())?
        {
            if body.len() + chunk.len() > MAX_DOWNLOAD_BYTES {
                return Err(too_long());
            }
            body.extend_from_slice(&chunk);
        }
        String::from_utf8(body).map_err(|_| "the list is not text".to_owned())
    }
}

/// Whether the standard error of an `ipset` or `iptables` call only says that the thing is not
/// there (or already is), which is what a removal or a repeated setup meets and is no problem.
pub fn is_absent_error(stderr: &str) -> bool {
    const PHRASES: [&str; 5] = [
        "does not exist",
        "already exists",
        "bad rule",
        "no chain/target/match by that name",
        "no such file or directory",
    ];
    let lower = stderr.to_lowercase();
    PHRASES.iter().any(|phrase| lower.contains(phrase))
}

/// The addresses of an IPsum list: one per line, the first column; `#` comments, blank lines,
/// scores, anything that is not an IPv4 address and addresses that are not on the public
/// internet are left out. Each address once, in the order of the list.
pub fn parse_ipsum(text: &str) -> Vec<Ipv4Addr> {
    let mut seen = HashSet::new();
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            line.split_whitespace().next()?.parse::<Ipv4Addr>().ok()
        })
        .filter(|ip| is_blockable(*ip))
        .filter(|ip| seen.insert(*ip))
        .collect()
}

/// A list that names the loopback or a private network is wrong or hostile: dropping those
/// would cut the node off from its own host or network.
fn is_blockable(ip: Ipv4Addr) -> bool {
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast())
}

/// The script `ipset restore` reads: it fills `<name>_tmp` and swaps it with `name`.
pub fn render_ipset_restore(name: &str, ips: &[Ipv4Addr]) -> String {
    let tmp = format!("{name}_tmp");
    let mut script = format!(
        "create {name} hash:ip family inet maxelem {MAX_ELEMENTS} -exist\n\
         create {tmp} hash:ip family inet maxelem {MAX_ELEMENTS} -exist\n\
         flush {tmp}\n"
    );
    for ip in ips {
        script.push_str(&format!("add {tmp} {ip}\n"));
    }
    script.push_str(&format!("swap {tmp} {name}\ndestroy {tmp}\n"));
    script
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rules {
    /// Nothing done yet in this run of the agent.
    Unknown,
    Installed,
    Removed,
}

#[derive(Debug)]
struct State {
    rules: Rules,
    /// Not before this: the next download (`None`: now).
    refresh_at: Option<Instant>,
    /// Not before this: the next try to remove the rules after one that left something behind.
    removal_at: Option<Instant>,
}

fn due(at: Option<Instant>, now: Instant) -> bool {
    at.is_none_or(|at| now >= at)
}

/// How often `iptables` waits for the lock of the xtables before it gives up.
const LOCK_WAIT: &str = "5";

/// `iptables` with the lock wait in front.
fn iptables(rest: &[&str]) -> Vec<String> {
    let mut args = vec!["-w".to_owned(), LOCK_WAIT.to_owned()];
    args.extend(rest.iter().map(|a| (*a).to_owned()));
    args
}

/// The rule that drops what the set holds.
const DROP_RULE: [&str; 8] = [NAME, "-m", "set", "--match-set", NAME, "src", "-j", "DROP"];
const JUMP: [&str; 3] = ["INPUT", "-j", NAME];

pub struct Antiscan<R, S> {
    runner: Arc<R>,
    source: S,
    wanted: AtomicBool,
    wake: Notify,
    state: Mutex<State>,
}

impl<R: CommandRunner, S: ListSource> Antiscan<R, S> {
    pub fn new(runner: Arc<R>, source: S, enabled: bool) -> Antiscan<R, S> {
        Antiscan {
            runner,
            source,
            wanted: AtomicBool::new(enabled),
            wake: Notify::new(),
            state: Mutex::new(State {
                rules: Rules::Unknown,
                refresh_at: None,
                removal_at: None,
            }),
        }
    }

    /// Switches the rules on or off; `run` (or the next `tick`) does the work.
    pub fn set_enabled(&self, enabled: bool) {
        self.wanted.store(enabled, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// Downloads the list and puts it and the chain in place now. Does nothing when disabled.
    pub async fn refresh(&self) -> Result<usize, AntiscanError> {
        let mut state = self.state.lock().await;
        self.refresh_locked(&mut state, Instant::now()).await
    }

    /// One look at the work: installs or refreshes the rules when due, removes them when switched
    /// off. A failure is logged and the old list stays.
    pub async fn tick(&self, now: Instant) {
        let mut state = self.state.lock().await;
        if self.wanted.load(Ordering::SeqCst) {
            if due(state.refresh_at, now) {
                match self.refresh_locked(&mut state, now).await {
                    Ok(count) => tracing::info!(addresses = count, "antiscan list in place"),
                    Err(error) => tracing::warn!(%error, "antiscan: the old list stays"),
                }
            }
        } else if state.rules != Rules::Removed && due(state.removal_at, now) {
            self.remove_locked(&mut state, now).await;
        }
    }

    /// Calls `tick` for as long as the agent runs.
    pub async fn run(self: Arc<Self>) {
        loop {
            self.tick(Instant::now()).await;
            tokio::select! {
                () = tokio::time::sleep(TICK) => {}
                () = self.wake.notified() => {}
            }
        }
    }
}

impl<R: CommandRunner, S: ListSource> Antiscan<R, S> {
    async fn refresh_locked(
        &self,
        state: &mut State,
        now: Instant,
    ) -> Result<usize, AntiscanError> {
        if !self.wanted.load(Ordering::SeqCst) {
            return Ok(0);
        }
        let result = self.install().await;
        match result {
            Ok(_) => {
                state.rules = Rules::Installed;
                state.refresh_at = Some(now + REFRESH_EVERY);
                state.removal_at = None;
            }
            Err(_) => state.refresh_at = Some(now + RETRY_AFTER),
        }
        result
    }

    /// Downloads, loads the set, makes sure the chain is there. The set is loaded before the
    /// chain refers to it, and a list that did not come or does not parse changes nothing.
    async fn install(&self) -> Result<usize, AntiscanError> {
        let text = self.source.fetch().await.map_err(AntiscanError::Download)?;
        let mut ips = parse_ipsum(&text);
        if ips.is_empty() {
            return Err(AntiscanError::Empty);
        }
        if ips.len() > MAX_ELEMENTS {
            tracing::warn!(
                addresses = ips.len(),
                kept = MAX_ELEMENTS,
                "antiscan: the list is longer than a set, the rest is dropped"
            );
            ips.truncate(MAX_ELEMENTS);
        }
        let script = render_ipset_restore(NAME, &ips);
        self.checked("ipset", &["restore"], Some(script.as_bytes()))
            .await?;

        // A chain that exists already is the usual case, not an error.
        let made = self.attempt("iptables", &iptables(&["-N", NAME])).await?;
        if !made.status.success() {
            self.note_unless_absent("iptables -N", &made);
        }
        self.ensure_rule(&DROP_RULE, "-A").await?;
        self.ensure_rule(&JUMP, "-I").await?;
        Ok(ips.len())
    }

    /// `-C rule` and, when it is not there, `add rule`.
    async fn ensure_rule(&self, rule: &[&str], add: &str) -> Result<(), AntiscanError> {
        let mut check = vec!["-C"];
        check.extend_from_slice(rule);
        if self
            .attempt("iptables", &iptables(&check))
            .await?
            .status
            .success()
        {
            return Ok(());
        }
        let mut insert = vec![add];
        insert.extend_from_slice(rule);
        let args = iptables(&insert);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.checked("iptables", &args, None).await
    }

    /// Removes the jump, then the chain, then the sets (a set in use cannot be destroyed).
    /// What is not there is not a problem; anything else is logged and tried again later.
    async fn remove_locked(&self, state: &mut State, now: Instant) {
        let mut clean = true;
        let mut jump = vec!["-D"];
        jump.extend_from_slice(&JUMP);
        // More than one jump may be there if somebody added it by hand.
        for _ in 0..MAX_JUMPS {
            match self.attempt("iptables", &iptables(&jump)).await {
                Ok(done) if done.status.success() => {}
                Ok(_) => break,
                Err(error) => {
                    tracing::warn!(%error, "antiscan: cannot remove the jump");
                    clean = false;
                    break;
                }
            }
        }
        let steps: [(&str, Vec<String>); 4] = [
            ("iptables", iptables(&["-F", NAME])),
            ("iptables", iptables(&["-X", NAME])),
            ("ipset", vec!["destroy".into(), NAME.into()]),
            ("ipset", vec!["destroy".into(), TMP_NAME.into()]),
        ];
        for (program, args) in &steps {
            match self.attempt(program, args).await {
                Ok(done) if done.status.success() => {}
                Ok(done) => {
                    if !self.note_unless_absent(program, &done) {
                        clean = false;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "antiscan: cannot remove the rules");
                    clean = false;
                }
            }
        }
        state.refresh_at = None;
        if clean {
            state.rules = Rules::Removed;
            state.removal_at = None;
            tracing::info!("antiscan rules removed");
        } else {
            state.rules = Rules::Unknown;
            state.removal_at = Some(now + RETRY_AFTER);
        }
    }

    /// Runs a command; `Err` only when it could not be started.
    async fn attempt(&self, program: &str, args: &[String]) -> Result<Output, AntiscanError> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner
            .run(program, &args, None)
            .await
            .map_err(|e| AntiscanError::Host(format!("{program}: {e}")))
    }

    /// Runs a command that has to succeed.
    async fn checked(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<(), AntiscanError> {
        let output = self
            .runner
            .run(program, args, stdin)
            .await
            .map_err(|e| AntiscanError::Host(format!("{program}: {e}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(AntiscanError::Host(format!(
            "{program} {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }

    /// Logs a failed command unless its error only says that the thing is (not) there. Returns
    /// whether it was only that.
    fn note_unless_absent(&self, what: &str, output: &Output) -> bool {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_absent_error(&stderr) {
            return true;
        }
        tracing::warn!(command = what, stderr = %stderr.trim(), "antiscan: host command failed");
        false
    }
}

/// How many times the jump is removed at most.
const MAX_JUMPS: usize = 8;

/// A sink that hands the `antiscan` flag of the config on, and the config to the sink inside.
pub struct AntiscanSink<K, R, S> {
    pub antiscan: Arc<Antiscan<R, S>>,
    /// The switch of the local config: `false` turns antiscan off whatever the registrar says.
    pub local_enabled: bool,
    pub inner: K,
}

impl<K, R, S> ConfigSink for AntiscanSink<K, R, S>
where
    K: ConfigSink,
    R: CommandRunner + 'static,
    S: ListSource + 'static,
{
    async fn apply(&self, config: &AgentConfig) {
        self.antiscan
            .set_enabled(self.local_enabled && config.antiscan);
        self.inner.apply(config).await;
    }
}

/// Fake list sources for tests.
pub mod testing {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::ListSource;

    /// Answers from a queue; counts the calls. With the queue empty it fails.
    #[derive(Debug, Default)]
    pub struct ScriptedList {
        answers: Mutex<VecDeque<Result<String, String>>>,
        fetched: Mutex<u32>,
    }

    impl ScriptedList {
        pub fn new() -> ScriptedList {
            ScriptedList::default()
        }
        pub fn push(&self, answer: Result<String, String>) {
            lock(&self.answers).push_back(answer);
        }
        pub fn fetched(&self) -> u32 {
            *lock(&self.fetched)
        }
    }

    impl ListSource for ScriptedList {
        async fn fetch(&self) -> Result<String, String> {
            *lock(&self.fetched) += 1;
            lock(&self.answers)
                .pop_front()
                .unwrap_or_else(|| Err("no scripted list left".into()))
        }
    }

    impl<T: ListSource> ListSource for std::sync::Arc<T> {
        async fn fetch(&self) -> Result<String, String> {
            (**self).fetch().await
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
