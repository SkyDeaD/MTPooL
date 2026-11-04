//! SRMD: automatic growing and folding of proxy domains.
//!
//! When the queue outgrows the domains, SRMD adds one (`shared.example.com` gives
//! `shared1.example.com`). When there are more domains than the queue needs, it folds the
//! surplus as a CNAME onto a surviving domain. It follows section 6 of the spec. `rebalance`
//! is meant to run on every 3 s tick and works from its arguments only.
//!
//! - The queue needs `max(1, ceil(queue / (max_nodes_per_domain * masters_per_domain)))`
//!   domains, at most `srmd.max_domains`.
//! - The difference between needed and active domains must hold for `srmd.diverge_ticks` ticks
//!   in a row. Then one step is taken and the counter restarts. A step settles the whole
//!   difference.
//! - Growing first unfolds folded domains, then creates new ones. `rebalance` does not add a
//!   created domain to the settings: the caller does, and `SrmdState::created` remembers it.
//! - Folding keeps `max(needed, domains that SRMD did not create)` active domains: the ones SRMD
//!   did not create first, then the rest in the order of `active`.
//!   The others fold, the biggest by clients first, each onto the least loaded survivor. A
//!   domain SRMD did not create (the base one, one added by hand) is never folded unless an
//!   admin hands it over with `take`.
//! - A CNAME always points at an active domain. Chains are repaired on every tick, also while
//!   SRMD is disabled or waiting, and a loop is cut by unfolding the domain.
//! - The clients of a domain are updated by the caller through `set_clients`. A missing metric
//!   keeps the last known value.
//! - `take` and `release` are the manual switch between "managed by the admin" and "managed by
//!   SRMD". `take` makes a domain SRMD's, so it may fold and unfold. `release` gives it back:
//!   SRMD never folds it again, and a folded one is unfolded. Neither touches `Settings`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::settings::Settings;

/// SRMD memory between ticks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SrmdState {
    /// Domains SRMD owns: the ones it created and the ones the admin handed over with `take`.
    pub created: BTreeSet<String>,
    /// Folded domain to the active domain it points at.
    pub cnames: BTreeMap<String, String>,
    /// Last known client count of a domain.
    pub domain_clients: BTreeMap<String, u64>,
    /// Ticks in a row with too few domains.
    pub expand_ticks: u32,
    /// Ticks in a row with too many domains.
    pub fold_ticks: u32,
}

/// What the caller has to do after a tick. The state is already updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrmdAction {
    /// The domain is back in rotation, its CNAME is gone.
    Unfold(String),
    /// A new domain: the caller adds it to the settings.
    Create(String),
    /// The domain is a CNAME onto `target`.
    Fold { domain: String, target: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrmdError {
    /// `take` on a domain SRMD already owns.
    AlreadyManaged(String),
    /// `release` on a domain SRMD does not own.
    NotManaged(String),
}

impl fmt::Display for SrmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SrmdError::AlreadyManaged(d) => write!(f, "{d} is already managed by SRMD"),
            SrmdError::NotManaged(d) => write!(f, "{d} is not managed by SRMD"),
        }
    }
}

impl std::error::Error for SrmdError {}

/// Domains the queue needs: `max(1, ceil(queue / (max_nodes * masters)))`.
pub fn required_domains(queue: usize, max_nodes: u32, masters: u32) -> usize {
    let capacity = (u64::from(max_nodes) * u64::from(masters)).max(1);
    let needed = (queue as u64).div_ceil(capacity).max(1);
    usize::try_from(needed).unwrap_or(usize::MAX)
}

/// How many domains the pool wants for `queue_len` waiting and serving nodes: the number
/// `rebalance` aims for, within `max_domains` and never below one.
pub fn needed_domains(queue_len: usize, settings: &Settings) -> usize {
    let cfg = &settings.srmd;
    required_domains(
        queue_len,
        cfg.max_nodes_per_domain,
        settings.masters_per_domain,
    )
    .min(cfg.max_domains as usize)
    .max(1)
}

/// Stores the clients of a domain. `None` (no metric) keeps the last known value.
pub fn set_clients(state: &mut SrmdState, domain: &str, clients: Option<u64>) {
    if let Some(n) = clients {
        state.domain_clients.insert(domain.to_string(), n);
    }
}

/// One tick. `active` are the domains that are not folded, in survival order.
pub fn rebalance(
    state: &mut SrmdState,
    active: &[String],
    queue_len: usize,
    settings: &Settings,
) -> Vec<SrmdAction> {
    let mut actions = normalize_chains(state);
    let cfg = &settings.srmd;
    let required = needed_domains(queue_len, settings);
    let have = active.len();

    if !cfg.enabled || required == have {
        state.expand_ticks = 0;
        state.fold_ticks = 0;
        return actions;
    }
    let step = if have < required {
        state.fold_ticks = 0;
        state.expand_ticks = state.expand_ticks.saturating_add(1);
        if state.expand_ticks < cfg.diverge_ticks {
            return actions;
        }
        state.expand_ticks = 0;
        expand(state, active, required, settings)
    } else {
        state.expand_ticks = 0;
        state.fold_ticks = state.fold_ticks.saturating_add(1);
        if state.fold_ticks < cfg.diverge_ticks {
            return actions;
        }
        state.fold_ticks = 0;
        fold(state, active, required)
    };
    actions.extend(step);
    // A domain folded just now may have been the target of another CNAME.
    actions.extend(normalize_chains(state));
    actions
}

/// Admin hands a domain over to SRMD.
pub fn take(state: &mut SrmdState, domain: &str) -> Result<(), SrmdError> {
    if state.created.insert(domain.to_string()) {
        Ok(())
    } else {
        Err(SrmdError::AlreadyManaged(domain.to_string()))
    }
}

/// Admin takes a domain back from SRMD. `Some(Unfold)` when it was folded.
pub fn release(state: &mut SrmdState, domain: &str) -> Result<Option<SrmdAction>, SrmdError> {
    if !state.created.remove(domain) {
        return Err(SrmdError::NotManaged(domain.to_string()));
    }
    Ok(state
        .cnames
        .remove(domain)
        .map(|_| SrmdAction::Unfold(domain.to_string())))
}

/// Points every CNAME straight at the domain at the end of its chain. A loop is cut by
/// unfolding the domain.
fn normalize_chains(state: &mut SrmdState) -> Vec<SrmdAction> {
    let mut actions = Vec::new();
    let folded: Vec<String> = state.cnames.keys().cloned().collect();
    for domain in folded {
        let target = state.cnames[&domain].clone();
        match chain_end(&domain, &state.cnames) {
            None => {
                state.cnames.remove(&domain);
                actions.push(SrmdAction::Unfold(domain));
            }
            Some(end) if end != target => {
                state.cnames.insert(domain.clone(), end.clone());
                actions.push(SrmdAction::Fold {
                    domain,
                    target: end,
                });
            }
            Some(_) => {}
        }
    }
    actions
}

/// The first domain that is not folded on the way from `start`, `None` for a loop.
fn chain_end(start: &str, cnames: &BTreeMap<String, String>) -> Option<String> {
    let mut seen = BTreeSet::from([start]);
    let mut current = start;
    while let Some(next) = cnames.get(current) {
        if !seen.insert(next.as_str()) {
            return None;
        }
        current = next;
    }
    Some(current.to_string())
}

/// Fills the gap up to `required`: folded domains first, then new names.
fn expand(
    state: &mut SrmdState,
    active: &[String],
    required: usize,
    settings: &Settings,
) -> Vec<SrmdAction> {
    let mut need = required - active.len();
    let mut actions = Vec::new();
    let mut managed: BTreeSet<String> = settings
        .domains
        .iter()
        .chain(active)
        .chain(state.cnames.keys())
        .map(|d| d.to_lowercase())
        .collect();

    // Settings order first, then folded domains the settings do not list.
    let mut folded: Vec<String> = settings
        .domains
        .iter()
        .filter(|d| state.cnames.contains_key(*d))
        .cloned()
        .collect();
    folded.extend(
        state
            .cnames
            .keys()
            .filter(|d| !settings.domains.contains(d))
            .cloned(),
    );
    for domain in folded.into_iter().take(need) {
        state.cnames.remove(&domain);
        actions.push(SrmdAction::Unfold(domain));
        need -= 1;
    }
    let Some((prefix, suffix)) = split_base(settings) else {
        return actions;
    };
    let mut taken = managed.clone();
    taken.extend(state.created.iter().map(|d| d.to_lowercase()));
    let mut names = (1..1000)
        .map(|n| format!("{prefix}{n}{suffix}"))
        .filter(|name| !taken.contains(&name.to_lowercase()));
    while need > 0 && managed.len() < settings.srmd.max_domains as usize {
        let Some(name) = names.next() else { break };
        managed.insert(name.to_lowercase());
        state.created.insert(name.clone());
        actions.push(SrmdAction::Create(name));
        need -= 1;
    }
    actions
}

/// `shared.example.com` to (`shared`, `.example.com`): the scheme new names grow by.
fn split_base(settings: &Settings) -> Option<(String, String)> {
    let usable = |d: &&str| !d.trim().is_empty();
    let explicit = settings.srmd.base_domain.as_deref().filter(usable);
    let base = explicit
        .or_else(|| settings.domains.iter().map(String::as_str).find(usable))?
        .trim();
    let dot = base.find('.')?;
    if dot == 0 || dot == base.len() - 1 {
        return None;
    }
    Some((base[..dot].to_string(), base[dot..].to_string()))
}

/// Folds the surplus domains. The first `max(required, domains SRMD does not own)` active
/// domains survive. The biggest of the others goes first, each onto the least loaded survivor.
fn fold(state: &mut SrmdState, active: &[String], required: usize) -> Vec<SrmdAction> {
    // Domains SRMD does not own come first wherever the caller listed them, so they survive.
    let (not_owned, owned): (Vec<&String>, Vec<&String>) =
        active.iter().partition(|d| !state.created.contains(*d));
    let keep = required.max(not_owned.len());
    if keep >= active.len() {
        return Vec::new();
    }
    let ordered: Vec<String> = not_owned.into_iter().chain(owned).cloned().collect();
    let clients = |state: &SrmdState, d: &str| state.domain_clients.get(d).copied().unwrap_or(0);
    let (survivors, surplus) = ordered.split_at(keep);
    let mut surplus: Vec<&String> = surplus.iter().collect();
    surplus.sort_by_key(|d| std::cmp::Reverse(clients(state, d)));
    let mut load: Vec<(&String, u64)> = survivors.iter().map(|d| (d, clients(state, d))).collect();

    let mut actions = Vec::new();
    for domain in surplus {
        let moved = clients(state, domain);
        let Some((target, total)) = load.iter_mut().min_by_key(|(_, l)| *l) else {
            break;
        };
        *total += moved;
        state.cnames.insert(domain.clone(), (*target).clone());
        state.domain_clients.insert((*target).clone(), *total);
        actions.push(SrmdAction::Fold {
            domain: domain.clone(),
            target: (*target).clone(),
        });
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SrmdSettings;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn settings(enabled: bool, domains: &[&str]) -> Settings {
        Settings {
            domains: names(domains),
            srmd: SrmdSettings {
                enabled,
                ..SrmdSettings::default()
            },
            ..Settings::default()
        }
    }

    /// Runs `n` ticks with the same input and returns the actions of each tick.
    fn ticks(
        state: &mut SrmdState,
        active: &[&str],
        queue: usize,
        s: &Settings,
        n: usize,
    ) -> Vec<Vec<SrmdAction>> {
        let active = names(active);
        (0..n)
            .map(|_| rebalance(state, &active, queue, s))
            .collect()
    }

    fn fold(domain: &str, target: &str) -> SrmdAction {
        SrmdAction::Fold {
            domain: domain.into(),
            target: target.into(),
        }
    }

    fn create(name: &str) -> SrmdAction {
        SrmdAction::Create(name.into())
    }

    fn unfold(name: &str) -> SrmdAction {
        SrmdAction::Unfold(name.into())
    }

    #[test]
    fn needed_domains_is_capped_by_max_domains_and_floored_at_one() {
        let mut settings = Settings::default();
        settings.srmd.max_nodes_per_domain = 5;
        settings.srmd.max_domains = 3;
        settings.masters_per_domain = 2;
        assert_eq!(needed_domains(0, &settings), 1);
        assert_eq!(needed_domains(11, &settings), 2);
        assert_eq!(needed_domains(1000, &settings), 3);
    }

    #[test]
    fn required_domains_rounds_up_and_has_a_floor() {
        assert_eq!(required_domains(0, 5, 2), 1);
        assert_eq!(required_domains(10, 5, 2), 1);
        assert_eq!(required_domains(11, 5, 2), 2);
        assert_eq!(required_domains(1, u32::MAX, u32::MAX), 1);
        assert_eq!(required_domains(usize::MAX, 1, 1), usize::MAX);
        assert_eq!(required_domains(3, 0, 0), 3, "zero capacity counts as 1");
    }

    #[test]
    fn disabled_no_action() {
        let s = settings(false, &["a.example.com"]);
        let mut state = SrmdState::default();
        let out = ticks(&mut state, &["a.example.com"], 1000, &s, 100);
        assert!(out.iter().all(Vec::is_empty));
        assert_eq!((state.expand_ticks, state.fold_ticks), (0, 0));
    }

    #[test]
    fn needs_20_stable_ticks() {
        let s = settings(true, &["a.example.com"]);
        let mut state = SrmdState::default();
        let out = ticks(&mut state, &["a.example.com"], 6, &s, 19);
        assert!(out.iter().all(Vec::is_empty));
        let out = ticks(&mut state, &["a.example.com"], 6, &s, 1);
        assert_eq!(out[0], vec![create("a1.example.com")]);
        assert_eq!(
            state.expand_ticks, 0,
            "the counter restarts after the action"
        );
        let out = ticks(&mut state, &["a.example.com", "a1.example.com"], 6, &s, 20);
        assert!(out.iter().all(Vec::is_empty), "queue 6 needs 2 domains");
    }

    #[test]
    fn divergence_must_hold_in_a_row() {
        let s = settings(true, &["a.example.com", "b.example.com"]);
        let mut state = SrmdState::default();
        ticks(&mut state, &["a.example.com"], 11, &s, 19);
        // Needed equals active for one tick: the counter resets.
        ticks(&mut state, &["a.example.com"], 5, &s, 1);
        assert_eq!(state.expand_ticks, 0);
        let out = ticks(&mut state, &["a.example.com"], 11, &s, 19);
        assert!(out.iter().all(Vec::is_empty));
        // The direction changes: both sides start over.
        ticks(&mut state, &["a.example.com", "b.example.com"], 1, &s, 5);
        assert_eq!((state.expand_ticks, state.fold_ticks), (0, 5));
        ticks(&mut state, &["a.example.com"], 11, &s, 1);
        assert_eq!((state.expand_ticks, state.fold_ticks), (1, 0));
    }

    #[test]
    fn expand_unfolds_before_create() {
        let s = settings(true, &["proxy.example.com", "proxy1.example.com"]);
        let mut state = SrmdState::default();
        state
            .cnames
            .insert("proxy1.example.com".into(), "proxy.example.com".into());
        let out = ticks(&mut state, &["proxy.example.com"], 11, &s, 20);
        assert_eq!(
            out[19],
            vec![unfold("proxy1.example.com"), create("proxy2.example.com")],
            "the existing proxy1 is not reused as a new name"
        );
        assert!(state.cnames.is_empty());
        assert!(state.created.contains("proxy2.example.com"));
    }

    #[test]
    fn create_name_base_plus_n() {
        let mut state = SrmdState::default();
        let s = settings(true, &["shared.example.com", "other.example.org"]);
        let out = ticks(&mut state, &["shared.example.com"], 16, &s, 20);
        assert_eq!(
            out[19],
            vec![
                create("shared1.example.com"),
                create("shared2.example.com"),
                create("shared3.example.com"),
            ],
            "queue 16 at 5 per domain needs 4 domains"
        );
    }

    #[test]
    fn create_name_uses_explicit_base_and_skips_taken_numbers() {
        let mut s = settings(true, &["shared.example.com", "shared1.example.com"]);
        s.srmd.base_domain = Some("base.example.org".into());
        let mut state = SrmdState::default();
        state.created.insert("BASE1.example.org".into());
        let out = ticks(&mut state, &["shared.example.com"], 6, &s, 20);
        assert_eq!(out[19], vec![create("base2.example.org")]);
        // Folded domains and the active ones count as taken too.
        let s = settings(true, &["shared.example.com"]);
        let mut state = SrmdState::default();
        state
            .cnames
            .insert("shared1.example.com".into(), "shared.example.com".into());
        let out = ticks(
            &mut state,
            &["shared.example.com", "shared2.example.com"],
            11,
            &s,
            20,
        );
        assert_eq!(out[19], vec![unfold("shared1.example.com")]);
    }

    #[test]
    fn one_written_domain_grows_by_number_as_nodes_come() {
        let s = settings(true, &["mt.example.com"]);
        let mut state = SrmdState::default();
        // Five nodes fit the one domain, nothing is created.
        let out = ticks(&mut state, &["mt.example.com"], 5, &s, 20);
        assert!(out.iter().all(Vec::is_empty));
        let out = ticks(&mut state, &["mt.example.com"], 6, &s, 20);
        assert_eq!(out[19], vec![create("mt1.example.com")]);
        let out = ticks(
            &mut state,
            &["mt.example.com", "mt1.example.com"],
            11,
            &s,
            20,
        );
        assert_eq!(out[19], vec![create("mt2.example.com")]);
    }

    #[test]
    fn no_written_domain_starts_from_the_base_name() {
        let mut s = settings(true, &[]);
        s.srmd.base_domain = Some("mt.example.com".into());
        let mut state = SrmdState::default();
        let out = ticks(&mut state, &[], 1, &s, 20);
        assert_eq!(out[19], vec![create("mt1.example.com")]);
        let out = ticks(&mut state, &["mt1.example.com"], 6, &s, 20);
        assert_eq!(out[19], vec![create("mt2.example.com")]);
        // The nodes leave: the first domain stays and the other folds onto it.
        let out = ticks(
            &mut state,
            &["mt1.example.com", "mt2.example.com"],
            1,
            &s,
            20,
        );
        assert_eq!(
            out[19],
            vec![SrmdAction::Fold {
                domain: "mt2.example.com".into(),
                target: "mt1.example.com".into(),
            }]
        );
    }

    #[test]
    fn no_usable_base_means_no_create() {
        for domains in [&[][..], &["localhost"], &[".example.com"], &["host."]] {
            let s = settings(true, domains);
            let mut state = SrmdState::default();
            let out = ticks(&mut state, &["a.example.com"], 100, &s, 40);
            assert!(out.iter().all(Vec::is_empty), "{domains:?}");
            assert!(state.created.is_empty());
        }
    }

    #[test]
    fn max_50_domains() {
        let all: Vec<String> = (0..50).map(|i| format!("d{i}.example.com")).collect();
        let refs: Vec<&str> = all.iter().map(String::as_str).collect();
        let s = settings(true, &refs);
        let mut state = SrmdState::default();
        let out = ticks(&mut state, &refs, 1_000_000, &s, 60);
        assert!(out.iter().all(Vec::is_empty));
        // Domains that are in the settings count towards the limit even when not active.
        let mut s = settings(true, &["a.example.com", "x.example.com", "y.example.com"]);
        s.srmd.max_domains = 3;
        let mut state = SrmdState::default();
        let out = ticks(&mut state, &["a.example.com"], 1000, &s, 40);
        assert!(out.iter().all(Vec::is_empty));
    }

    #[test]
    fn fold_keeps_non_created_domains() {
        let s = settings(true, &["a.example.com", "b.example.com", "c.example.com"]);
        let mut state = SrmdState::default();
        let out = ticks(
            &mut state,
            &["a.example.com", "b.example.com", "c.example.com"],
            1,
            &s,
            60,
        );
        assert!(out.iter().all(Vec::is_empty));
        assert!(state.cnames.is_empty());
    }

    #[test]
    fn fold_large_first_to_least_loaded() {
        let s = settings(true, &["a.example.com", "b.example.com"]);
        let mut state = SrmdState::default();
        state.created.insert("c.example.com".into());
        state.created.insert("d.example.com".into());
        for (d, n) in [("a", 10), ("b", 20), ("c", 5), ("d", 30)] {
            set_clients(&mut state, &format!("{d}.example.com"), Some(n));
        }
        let active = [
            "a.example.com",
            "b.example.com",
            "c.example.com",
            "d.example.com",
        ];
        let out = ticks(&mut state, &active, 1, &s, 20);
        assert_eq!(
            out[19],
            vec![
                fold("d.example.com", "a.example.com"),
                fold("c.example.com", "b.example.com"),
            ],
            "d (30) goes first to a (10), then c (5) to b (20 < 40)"
        );
        assert_eq!(state.fold_ticks, 0);
        assert_eq!(state.cnames["d.example.com"], "a.example.com");
    }

    #[test]
    fn fold_follows_active_order_for_survivors() {
        let s = settings(true, &[]);
        let mut state = SrmdState::default();
        for d in ["x.example.com", "y.example.com", "z.example.com"] {
            state.created.insert(d.into());
        }
        let out = ticks(
            &mut state,
            &["z.example.com", "x.example.com", "y.example.com"],
            1,
            &s,
            20,
        );
        assert_eq!(
            out[19],
            vec![
                fold("x.example.com", "z.example.com"),
                fold("y.example.com", "z.example.com"),
            ]
        );
    }

    #[test]
    fn fold_never_touches_non_created_domain_listed_late() {
        let s = settings(true, &[]);
        let mut state = SrmdState::default();
        state.created.insert("c1.example.com".into());
        state.created.insert("c2.example.com".into());
        let out = ticks(
            &mut state,
            &["c1.example.com", "base.example.com", "c2.example.com"],
            1,
            &s,
            20,
        );
        assert_eq!(
            out[19],
            vec![
                fold("c1.example.com", "base.example.com"),
                fold("c2.example.com", "base.example.com"),
            ],
            "base is not owned by SRMD and survives wherever it is listed"
        );
        assert!(!state.cnames.contains_key("base.example.com"));
    }

    #[test]
    fn cname_never_targets_cname() {
        let s = settings(true, &["a.example.com"]);
        let mut state = SrmdState::default();
        state.created.insert("c.example.com".into());
        state
            .cnames
            .insert("x.example.com".into(), "c.example.com".into());
        let out = ticks(&mut state, &["a.example.com", "c.example.com"], 1, &s, 20);
        assert_eq!(
            out[19],
            vec![
                fold("c.example.com", "a.example.com"),
                fold("x.example.com", "a.example.com"),
            ],
            "x pointed at c, which has just been folded"
        );
        assert_eq!(state.cnames["x.example.com"], "a.example.com");
        assert_eq!(state.cnames["c.example.com"], "a.example.com");
    }

    #[test]
    fn broken_chain_normalized_each_tick() {
        // SRMD is disabled: the chain is repaired anyway.
        let s = settings(false, &[]);
        let mut state = SrmdState::default();
        state
            .cnames
            .insert("b.example.com".into(), "c.example.com".into());
        state
            .cnames
            .insert("c.example.com".into(), "a.example.com".into());
        let out = ticks(&mut state, &["a.example.com"], 1, &s, 2);
        assert_eq!(out[0], vec![fold("b.example.com", "a.example.com")]);
        assert!(out[1].is_empty());
        assert_eq!(state.cnames["b.example.com"], "a.example.com");
    }

    #[test]
    fn loop_and_self_reference_are_unfolded() {
        let s = settings(false, &[]);
        let mut state = SrmdState::default();
        state
            .cnames
            .insert("x.example.com".into(), "y.example.com".into());
        state
            .cnames
            .insert("y.example.com".into(), "x.example.com".into());
        state
            .cnames
            .insert("z.example.com".into(), "z.example.com".into());
        let out = ticks(&mut state, &["a.example.com"], 1, &s, 2);
        assert_eq!(
            out[0],
            vec![unfold("x.example.com"), unfold("z.example.com")]
        );
        assert!(out[1].is_empty());
        assert_eq!(state.cnames.len(), 1, "y still points at the unfolded x");
    }

    #[test]
    fn missing_metric_uses_last_known_clients() {
        let s = settings(true, &["a.example.com", "b.example.com"]);
        let mut state = SrmdState::default();
        state.created.insert("c.example.com".into());
        set_clients(&mut state, "a.example.com", Some(10));
        set_clients(&mut state, "a.example.com", None);
        set_clients(&mut state, "b.example.com", Some(5));
        assert_eq!(state.domain_clients["a.example.com"], 10);
        let out = ticks(
            &mut state,
            &["a.example.com", "b.example.com", "c.example.com"],
            1,
            &s,
            20,
        );
        assert_eq!(out[19], vec![fold("c.example.com", "b.example.com")]);
        // A domain never measured stays unknown.
        set_clients(&mut state, "never.example.com", None);
        assert!(!state.domain_clients.contains_key("never.example.com"));
    }

    #[test]
    fn manual_take_release_keeps_settings() {
        let s = settings(true, &["a.example.com", "b.example.com"]);
        let before = s.clone();
        let mut state = SrmdState::default();
        let active = ["a.example.com", "b.example.com"];

        // Not owned by SRMD: never folded.
        let out = ticks(&mut state, &active, 1, &s, 40);
        assert!(out.iter().all(Vec::is_empty));

        assert_eq!(take(&mut state, "b.example.com"), Ok(()));
        assert_eq!(
            take(&mut state, "b.example.com"),
            Err(SrmdError::AlreadyManaged("b.example.com".into()))
        );
        let out = ticks(&mut state, &active, 1, &s, 20);
        assert_eq!(out[19], vec![fold("b.example.com", "a.example.com")]);

        // Release unfolds a folded domain and returns the action.
        assert_eq!(
            release(&mut state, "b.example.com"),
            Ok(Some(unfold("b.example.com")))
        );
        assert!(state.cnames.is_empty());
        assert!(!state.created.contains("b.example.com"));
        assert_eq!(
            release(&mut state, "b.example.com"),
            Err(SrmdError::NotManaged("b.example.com".into()))
        );

        // Release of an unfolded domain returns no action and protects it from folding.
        take(&mut state, "b.example.com").unwrap();
        assert_eq!(release(&mut state, "b.example.com"), Ok(None));
        let out = ticks(&mut state, &active, 1, &s, 40);
        assert!(out.iter().all(Vec::is_empty));
        assert_eq!(s, before);
    }
}
