//! A filter of another pool on the proxy port. Such a pool drops the addresses of an ipset in
//! `iptables`, and with it on Globalping sees the port closed for reasons that are no block. The
//! agent reads `iptables-save` every 30 minutes and reports the name of the set; the name goes to
//! the registrar and the panel only.

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tokio::time::Instant;

use crate::antiscan;
use crate::host::CommandRunner;

/// How often the rules are read again.
pub const READ_EVERY: Duration = Duration::from_secs(30 * 60);
/// `iptables-save` gives up after this long.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How many jumps from `INPUT` are followed.
const MAX_DEPTH: usize = 4;
/// The longest name that is reported; a longer one is cut.
const MAX_NAME: usize = 64;

/// One rule of the `filter` table, reduced to what the search needs.
struct Rule<'a> {
    chain: &'a str,
    /// Whether the rule applies to TCP traffic to the port.
    applies: bool,
    /// The set the rule matches the source address against, if it does so without negation and it
    /// is not the pool's own.
    set: Option<String>,
    target: Option<&'a str>,
}

/// Splits a line of `iptables-save` the way the tool wrote it: double quotes group a word.
fn tokens(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut open = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                open = true;
            }
            c if c.is_whitespace() && !quoted => {
                if open {
                    words.push(std::mem::take(&mut word));
                    open = false;
                }
            }
            c => {
                word.push(c);
                open = true;
            }
        }
    }
    if open {
        words.push(word);
    }
    words
}

/// Whether a port list (`443`, `400:500`, `80,443`) holds `port`.
fn ports_hold(list: &str, port: u16) -> bool {
    list.split(',').any(|item| match item.split_once(':') {
        Some((from, to)) => match (from.parse::<u16>(), to.parse::<u16>()) {
            (Ok(from), Ok(to)) => (from..=to).contains(&port),
            _ => false,
        },
        None => item.parse::<u16>() == Ok(port),
    })
}

/// A name made only of what ipset allows, at most [`MAX_NAME`] long. The value comes from the
/// host, so nothing else gets through.
fn clean_name(raw: &str) -> Option<String> {
    let name: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(MAX_NAME)
        .collect();
    (!name.is_empty()).then_some(name)
}

fn parse_rule(tokens: &[String], port: u16) -> Option<Rule<'_>> {
    if tokens.first().map(String::as_str) != Some("-A") {
        return None;
    }
    let mut rule = Rule {
        chain: tokens.get(1)?,
        applies: true,
        set: None,
        target: None,
    };
    let mut negated = false;
    let mut i = 2;
    while i < tokens.len() {
        let flag = tokens[i].as_str();
        let value = tokens.get(i + 1).map(String::as_str).unwrap_or_default();
        match flag {
            "!" => {
                negated = true;
                i += 1;
                continue;
            }
            "-p" | "--protocol" => {
                if negated || !matches!(value, "tcp" | "6" | "all" | "0") {
                    rule.applies = false;
                }
            }
            "--dport" | "--destination-port" | "--dports" | "--destination-ports" => {
                if ports_hold(value, port) == negated {
                    rule.applies = false;
                }
            }
            "--match-set" => {
                let flags = tokens.get(i + 2).map(String::as_str).unwrap_or_default();
                let own = value == antiscan::NAME || value == antiscan::TMP_NAME;
                if !negated && !own && rule.set.is_none() && flags.split(',').next() == Some("src")
                {
                    rule.set = clean_name(value);
                }
                i += 1;
            }
            "-j" | "-g" | "--jump" | "--goto" => rule.target = Some(value),
            _ => {
                negated = false;
                i += 1;
                continue;
            }
        }
        negated = false;
        i += 2;
    }
    Some(rule)
}

/// The name of the set another pool drops the addresses of on `port`, if `save` (the output of
/// `iptables-save`) shows such a rule: `-m set --match-set NAME src -j DROP|REJECT`, in `INPUT` or
/// in a chain `INPUT` jumps to (up to four jumps deep). A negated set is an allowlist and does not
/// count; neither does the chain and the set of the pool's own antiscan.
pub fn find_port_filter(save: &str, port: u16) -> Option<String> {
    let table: Vec<&str> = save
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "*filter")
        .skip(1)
        .take_while(|line| *line != "COMMIT")
        .collect();
    let declared: HashSet<&str> = table
        .iter()
        .filter_map(|line| line.strip_prefix(':')?.split_whitespace().next())
        .collect();
    let words: Vec<Vec<String>> = table.iter().map(|line| tokens(line)).collect();
    let rules: Vec<Rule> = words
        .iter()
        .filter_map(|line| parse_rule(line, port))
        .collect();
    walk("INPUT", &rules, &declared, &mut vec!["INPUT"])
}

fn walk<'a>(
    chain: &'a str,
    rules: &[Rule<'a>],
    declared: &HashSet<&str>,
    path: &mut Vec<&'a str>,
) -> Option<String> {
    for rule in rules.iter().filter(|r| r.chain == chain && r.applies) {
        let Some(target) = rule.target else {
            continue;
        };
        if matches!(target, "DROP" | "REJECT") {
            if let Some(name) = &rule.set {
                return Some(name.clone());
            }
        } else if declared.contains(target)
            && target != antiscan::NAME
            && path.len() <= MAX_DEPTH
            && !path.contains(&target)
        {
            path.push(target);
            let found = walk(target, rules, declared, path);
            path.pop();
            if found.is_some() {
                return found;
            }
        }
    }
    None
}

type SaveFuture = Pin<Box<dyn Future<Output = io::Result<Output>> + Send>>;

#[derive(Default)]
struct Read {
    port: Option<u16>,
    at: Option<Instant>,
}

/// Reads the rules of the host every [`READ_EVERY`] and keeps what [`find_port_filter`] made of
/// them.
pub struct PortFilterWatch {
    save: Box<dyn Fn() -> SaveFuture + Send + Sync>,
    last: AsyncMutex<Read>,
    found: Mutex<Option<String>>,
}

impl PortFilterWatch {
    pub fn new<R: CommandRunner + 'static>(runner: Arc<R>) -> PortFilterWatch {
        PortFilterWatch {
            save: Box::new(move || {
                let runner = runner.clone();
                Box::pin(async move { runner.run("iptables-save", &["-t", "filter"], None).await })
            }),
            last: AsyncMutex::new(Read::default()),
            found: Mutex::new(None),
        }
    }

    fn set(&self, name: Option<String>) {
        *self.found.lock().unwrap_or_else(|p| p.into_inner()) = name;
    }

    /// The name of the set last found, if any.
    pub fn current(&self) -> Option<String> {
        self.found.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Reads the rules if [`READ_EVERY`] has passed since the last read or the pool's port is not
    /// the one that was read. Without a port there is nothing to look for.
    pub async fn tick(&self, now: Instant, pool_port: Option<u16>) {
        let mut last = self.last.lock().await;
        let Some(port) = pool_port else {
            *last = Read::default();
            self.set(None);
            return;
        };
        let due = last.port != Some(port)
            || last
                .at
                .is_none_or(|at| now.saturating_duration_since(at) >= READ_EVERY);
        if !due {
            return;
        }
        *last = Read {
            port: Some(port),
            at: Some(now),
        };
        let name = self
            .read()
            .await
            .and_then(|save| find_port_filter(&save, port));
        self.set(name);
    }

    /// The output of `iptables-save`; what goes wrong is logged without the output.
    async fn read(&self) -> Option<String> {
        match tokio::time::timeout(READ_TIMEOUT, (self.save)()).await {
            Err(_) => tracing::warn!("iptables-save did not answer in time"),
            Ok(Err(error)) => tracing::warn!(%error, "cannot run iptables-save"),
            Ok(Ok(output)) if !output.status.success() => {
                tracing::warn!(status = ?output.status.code(), "iptables-save failed");
            }
            Ok(Ok(output)) => match String::from_utf8(output.stdout) {
                Ok(text) => return Some(text),
                Err(_) => tracing::warn!("the output of iptables-save is not text"),
            },
        }
        None
    }
}
