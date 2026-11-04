//! The public address of the node. Plain-text services (four by default, two to eight when the
//! environment names them) are asked at once; the address that at least two of them agree on is
//! the answer. A change makes the node enroll again.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use tokio::time::Instant;

use crate::engine::http::HttpGet;
use crate::state_files::Fs;

pub const IP_SOURCES: [&str; 4] = [
    "https://api.ipify.org",
    "https://ifconfig.me/ip",
    "https://icanhazip.com",
    "https://ipinfo.io/ip",
];
/// How often the address is looked up.
pub const IP_CHECK_EVERY: Duration = Duration::from_secs(45);
/// How long one service may take.
pub const SOURCE_TIMEOUT: Duration = Duration::from_secs(5);
pub const PUBLIC_IP_FILE: &str = "public_ip";

/// A public IPv4 address as a service says it: the address alone, spaces and the newline around
/// it aside. Private, loopback, link-local, shared (100.64/10), multicast and reserved ranges are
/// no node's public address; the documentation ranges are let through (the recorded answers in
/// the tests are in them). With `allow_private` (a test network, `MTPOOL_ALLOW_PRIVATE_IPS`) the
/// private, loopback, link-local and shared ranges count; unspecified, broadcast, multicast and
/// reserved addresses never do.
fn parse_public(text: &str, allow_private: bool) -> Option<Ipv4Addr> {
    let ip: Ipv4Addr = text.trim().parse().ok()?;
    let [first, second, ..] = ip.octets();
    let shared = first == 100 && (64..128).contains(&second);
    let reserved = first >= 240;
    let never = ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || reserved;
    let private = ip.is_private() || ip.is_loopback() || ip.is_link_local() || shared;
    (!never && (allow_private || !private)).then_some(ip)
}

/// Polls every future until all are done; the answers come back in the order of `futures`.
async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    type Slot<F> = (Pin<Box<F>>, Option<<F as Future>::Output>);
    let mut slots: Vec<Slot<F>> = futures
        .into_iter()
        .map(|future| (Box::pin(future), None))
        .collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (future, output) in &mut slots {
            if output.is_none() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(value) => *output = Some(value),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    slots.into_iter().filter_map(|(_, output)| output).collect()
}

async fn ask<H: HttpGet>(http: &H, url: &str, allow_private: bool) -> Option<Ipv4Addr> {
    let answer = tokio::time::timeout(SOURCE_TIMEOUT, http.get(url, None))
        .await
        .ok()?
        .ok()?;
    if answer.status != 200 {
        return None;
    }
    parse_public(&answer.body, allow_private)
}

/// The address at least two of the `sources` agree on. `None` if no two agree (a 2-2 split
/// too), or the answers are not public IPv4 addresses.
pub async fn detect_public_ip<H: HttpGet>(http: &H, sources: &[&str]) -> Option<Ipv4Addr> {
    detect_public_ip_allowing(http, sources, false).await
}

/// [`detect_public_ip`], with private addresses accepted when `allow_private` is set.
pub async fn detect_public_ip_allowing<H: HttpGet>(
    http: &H,
    sources: &[&str],
    allow_private: bool,
) -> Option<Ipv4Addr> {
    let answers = join_all(
        sources
            .iter()
            .map(|url| ask(http, url, allow_private))
            .collect(),
    )
    .await;
    let mut votes: BTreeMap<Ipv4Addr, usize> = BTreeMap::new();
    for ip in answers.into_iter().flatten() {
        *votes.entry(ip).or_default() += 1;
    }
    let mut agreed = votes.into_iter().filter(|(_, count)| *count >= 2);
    let (winner, _) = agreed.next()?;
    // Two different pairs are a split, not a majority.
    agreed.next().is_none().then_some(winner)
}

/// What a look at the address found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpEvent {
    /// Not due yet, nothing found, or the same address as before.
    Quiet,
    /// The first address this node knows of.
    Found(Ipv4Addr),
    /// A different address than before: the node has to enroll again.
    Changed { from: Ipv4Addr, to: Ipv4Addr },
}

pub struct IpWatch<H> {
    http: Arc<H>,
    fs: Arc<dyn Fs>,
    file: PathBuf,
    sources: Vec<String>,
    allow_private: bool,
    /// What the file held, kept to read it again under a different rule.
    stored: Option<String>,
    known: Option<Ipv4Addr>,
    last_look: Option<Instant>,
}

impl<H: HttpGet> IpWatch<H> {
    /// The address the node had when it last ran is read from `<state_dir>/public_ip`, so a
    /// change while the agent was down is seen too.
    pub fn new(http: Arc<H>, fs: Arc<dyn Fs>, state_dir: PathBuf) -> IpWatch<H> {
        let file = state_dir.join(PUBLIC_IP_FILE);
        let stored = match fs.read(&file) {
            Ok(Some(bytes)) => String::from_utf8(bytes).ok(),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "cannot read the stored public address");
                None
            }
        };
        let known = stored.as_deref().and_then(|t| parse_public(t, false));
        IpWatch {
            http,
            fs,
            file,
            sources: IP_SOURCES.map(String::from).to_vec(),
            allow_private: false,
            stored,
            known,
            last_look: None,
        }
    }

    /// The services to ask instead of the default four (two to eight of them).
    #[must_use]
    pub fn with_sources(mut self, sources: Vec<String>) -> IpWatch<H> {
        self.sources = sources;
        self
    }

    /// Accept private addresses (a test network), also the one stored by an earlier run.
    #[must_use]
    pub fn with_allow_private(mut self, allow: bool) -> IpWatch<H> {
        self.allow_private = allow;
        self.known = self.stored.as_deref().and_then(|t| parse_public(t, allow));
        self
    }

    /// The address the node has now, as far as it knows.
    pub fn current(&self) -> Option<Ipv4Addr> {
        self.known
    }

    /// Looks the address up if [`IP_CHECK_EVERY`] has passed since the last look.
    pub async fn check(&mut self, now: Instant) -> IpEvent {
        if let Some(last) = self.last_look
            && now.saturating_duration_since(last) < IP_CHECK_EVERY
        {
            return IpEvent::Quiet;
        }
        self.last_look = Some(now);
        let sources: Vec<&str> = self.sources.iter().map(String::as_str).collect();
        let Some(found) =
            detect_public_ip_allowing(&*self.http, &sources, self.allow_private).await
        else {
            return IpEvent::Quiet;
        };
        let event = match self.known {
            None => IpEvent::Found(found),
            Some(from) if from != found => IpEvent::Changed { from, to: found },
            Some(_) => return IpEvent::Quiet,
        };
        self.known = Some(found);
        self.remember(found);
        event
    }

    /// The node has enrolled: the address now is the one it is registered under.
    pub fn enrolled(&mut self) {
        // The address is the one last looked up; nothing is pending.
        if let Some(ip) = self.known {
            self.remember(ip);
        }
    }

    fn remember(&self, ip: Ipv4Addr) {
        if let Err(error) = self
            .fs
            .write_private(&self.file, format!("{ip}\n").as_bytes())
        {
            tracing::warn!(%error, "cannot store the public address");
        }
    }
}
