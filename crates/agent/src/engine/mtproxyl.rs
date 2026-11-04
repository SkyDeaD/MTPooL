//! The MTProxyL adapter. MTProxyL runs telemt too, but it owns the configuration: it rebuilds
//! the file from its own settings, so editing the file would be undone. Everything is therefore
//! set through its CLI (`mtproxyl ...`, run with `MTPROXYL_ASSUME_YES=1` so that it never asks),
//! and the service is restarted by `mtproxyl restart`, never by `systemctl`.
//!
//! Reading (metrics, DC2, health, the port) is the telemt adapter's, on the telemt configuration
//! the manager generates.
//!
//! A change is a list of steps. Each step that changes something has its inverse; when a step
//! or the restart fails, or the engine does not answer after it, the inverses run newest first
//! and the engine is restarted on the old settings.
//!
//! The runner has no environment parameter, so the CLI is run as
//! `env MTPROXYL_ASSUME_YES=1 mtproxyl <args>`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use mtpool_protocol::agent::{AgentConfig, Dc2Report, MetricsReport};

use super::http::HttpGet;
use super::telemt::{self, TelemtEngine, TelemtToml};
use super::{ApplyOutcome, DesiredConfig, Engine, EngineError, EngineHealth, EngineKind, Result};
use crate::host::CommandRunner;
use crate::lifecycle::{ConfigSink, DetectedEngine, EngineProbe};
use crate::state_files::Fs;

pub const SETTINGS_PATH: &str = "/opt/mtproxyl/settings.conf";
/// The places the telemt configuration of MTProxyL is looked for, the first one that exists
/// wins.
pub const CONFIG_CANDIDATES: [&str; 3] = [
    "/opt/mtproxyl/mtproxy/config.toml",
    "/opt/mtproxyl/mtproxy/telemt.toml",
    "/opt/mtproxyl/superexpert.toml",
];

const SUPEREXPERT_KEY: &str = "SUPEREXPERT_ENABLED";

/// One change made through the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    AddSecret {
        name: String,
        secret: String,
    },
    RemoveSecret {
        name: String,
    },
    SetDomains(Vec<String>),
    /// Pairs of domain and target.
    SetMask(Vec<(String, String)>),
}

impl Step {
    /// The arguments after `mtproxyl`.
    fn args(&self) -> Vec<String> {
        let own = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        match self {
            Step::AddSecret { name, secret } => {
                let mut args = own(&["secret", "add"]);
                args.push(name.clone());
                args.push(secret.clone());
                args
            }
            Step::RemoveSecret { name } => {
                let mut args = own(&["secret", "remove"]);
                args.push(name.clone());
                args
            }
            Step::SetDomains(domains) => {
                let mut args = own(&["expert", "set", "censorship", "tls_domains"]);
                args.extend(domains.iter().cloned());
                args.push("--no-apply".to_owned());
                args
            }
            Step::SetMask(pairs) => {
                let mut args = own(&["expert", "set", "--raw", "censorship.exclusive_mask"]);
                for (domain, target) in pairs {
                    args.push(domain.clone());
                    args.push(target.clone());
                }
                args.push("--no-apply".to_owned());
                args
            }
        }
    }

    /// What the step is called in a message: never the secret.
    fn label(&self) -> String {
        match self {
            Step::AddSecret { name, .. } => format!("secret add {name}"),
            Step::RemoveSecret { name } => format!("secret remove {name}"),
            Step::SetDomains(_) => "expert set censorship tls_domains".to_owned(),
            Step::SetMask(_) => "expert set censorship.exclusive_mask".to_owned(),
        }
    }
}

/// A step and the one that undoes it, if there is one.
struct Planned {
    step: Step,
    undo: Option<Step>,
}

pub struct MtProxyLEngine<R, H> {
    runner: Arc<R>,
    fs: Arc<dyn Fs>,
    http: Arc<H>,
    state_dir: PathBuf,
    settings_path: PathBuf,
    config_path: PathBuf,
    clock: fn() -> u64,
    /// The telemt adapter on the generated configuration; only ever read through.
    reader: TelemtEngine<R, H>,
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> MtProxyLEngine<R, H> {
    /// The configuration is the first of [`CONFIG_CANDIDATES`] that exists, the first one if
    /// none does yet.
    pub fn new(state_dir: PathBuf, runner: Arc<R>, fs: Arc<dyn Fs>, http: Arc<H>) -> Self {
        let config_path = CONFIG_CANDIDATES
            .iter()
            .map(Path::new)
            .find(|path| fs.exists(path).unwrap_or(false))
            .unwrap_or(Path::new(CONFIG_CANDIDATES[0]))
            .to_owned();
        let clock = telemt_clock();
        let reader = Self::reader_of(&config_path, &state_dir, &runner, &fs, &http, clock);
        MtProxyLEngine {
            runner,
            fs,
            http,
            state_dir,
            settings_path: PathBuf::from(SETTINGS_PATH),
            config_path,
            clock,
            reader,
        }
    }

    fn reader_of(
        config_path: &Path,
        state_dir: &Path,
        runner: &Arc<R>,
        fs: &Arc<dyn Fs>,
        http: &Arc<H>,
        clock: fn() -> u64,
    ) -> TelemtEngine<R, H> {
        // The unit is never used: this adapter only reads through it.
        TelemtEngine::new(
            config_path.to_owned(),
            telemt::DEFAULT_UNIT,
            state_dir.to_owned(),
            runner.clone(),
            fs.clone(),
            http.clone(),
        )
        .with_clock(clock)
    }

    fn rebuild_reader(&mut self) {
        self.reader = Self::reader_of(
            &self.config_path,
            &self.state_dir,
            &self.runner,
            &self.fs,
            &self.http,
            self.clock,
        );
    }

    #[must_use]
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = path;
        self.rebuild_reader();
        self
    }

    #[must_use]
    pub fn with_settings_path(mut self, path: PathBuf) -> Self {
        self.settings_path = path;
        self
    }

    /// Replaces the clock (seconds since the epoch) the DC2 check uses.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> u64) -> Self {
        self.clock = clock;
        self.rebuild_reader();
        self
    }

    /// Runs `mtproxyl <args>`. The error says which step, never what its arguments held.
    async fn cli(&self, args: &[String], label: &str) -> std::result::Result<(), String> {
        let mut full: Vec<&str> = vec!["MTPROXYL_ASSUME_YES=1", "mtproxyl"];
        full.extend(args.iter().map(String::as_str));
        let output = self
            .runner
            .run("env", &full, None)
            .await
            .map_err(|e| format!("mtproxyl {label} could not be run: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let code = output
                .status
                .code()
                .map_or_else(|| "a signal".to_owned(), |c| format!("code {c}"));
            Err(format!("mtproxyl {label} ended with {code}"))
        }
    }

    /// The steps that take the engine from what `config` has to what `desired` asks, and what
    /// undoes each. Only the users and the domain the pool wrote before (`pool`) are ever
    /// removed; the owner's extra domains and mask pairs are carried along as they are.
    fn plan(
        config: &TelemtToml,
        desired: &DesiredConfig,
        pool: &telemt::PoolState,
    ) -> Result<Vec<Planned>> {
        let managed = &pool.users;
        let have = config.users();
        let secret_of = |name: &str| have.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone());
        let mut steps = Vec::new();

        for name in managed {
            let revoked = !desired.users.iter().any(|(wanted, _)| wanted == name);
            if let (true, Some(old)) = (revoked, secret_of(name)) {
                steps.push(Planned {
                    step: Step::RemoveSecret { name: name.clone() },
                    undo: Some(Step::AddSecret {
                        name: name.clone(),
                        secret: old,
                    }),
                });
            }
        }
        for (name, secret) in &desired.users {
            let current = secret_of(name);
            if current.as_deref() == Some(secret.as_str()) {
                continue;
            }
            let add = Step::AddSecret {
                name: name.clone(),
                secret: secret.clone(),
            };
            match current {
                // A different secret is a rotation: the old one goes first.
                Some(old) => {
                    steps.push(Planned {
                        step: Step::RemoveSecret { name: name.clone() },
                        undo: Some(Step::AddSecret {
                            name: name.clone(),
                            secret: old,
                        }),
                    });
                    steps.push(Planned {
                        step: add,
                        undo: Some(Step::RemoveSecret { name: name.clone() }),
                    });
                }
                None => steps.push(Planned {
                    step: add,
                    undo: Some(Step::RemoveSecret { name: name.clone() }),
                }),
            }
        }

        let sni = desired.sni.as_str();
        let previous = pool.sni.as_deref().filter(|previous| *previous != sni);

        // The extras are what the CLI sets; the primary `tls_domain` is not among them.
        let extras = config.extra_tls_domains()?;
        let mut wanted: Vec<String> = extras
            .iter()
            .filter(|domain| Some(domain.as_str()) != previous)
            .cloned()
            .collect();
        let listed = config.tls_domain().as_deref() == Some(sni) || wanted.iter().any(|d| d == sni);
        if !listed {
            wanted.push(sni.to_owned());
        }
        if wanted != extras {
            steps.push(Planned {
                step: Step::SetDomains(wanted),
                // Nothing recorded says the CLI takes an empty list. An extra domain left behind
                // by a failed apply does no harm; a rollback that fails is reported as broken.
                undo: (!extras.is_empty()).then_some(Step::SetDomains(extras)),
            });
        }

        let pairs = config.exclusive_mask()?;
        let pool_target = |domain: &str| format!("{domain}:443");
        let mut wanted: Vec<(String, String)> = pairs
            .iter()
            .filter(|(domain, target)| {
                !(Some(domain.as_str()) == previous && *target == pool_target(domain))
            })
            .cloned()
            .collect();
        if !wanted.iter().any(|(domain, _)| domain == sni) {
            wanted.push((sni.to_owned(), pool_target(sni)));
        }
        if wanted != pairs {
            steps.push(Planned {
                step: Step::SetMask(wanted),
                undo: (!pairs.is_empty()).then_some(Step::SetMask(pairs)),
            });
        }
        Ok(steps)
    }

    /// Reads `SUPEREXPERT_ENABLED`; `Some(true)` if it is on in any of the forms the manager
    /// is known to write (`1`, `true`, `yes`, `on`, quoted or not).
    fn superexpert_state(&self) -> Result<Option<(String, bool)>> {
        let bytes = self
            .fs
            .read(&self.settings_path)
            .map_err(|source| EngineError::Io {
                what: format!("cannot read {}", self.settings_path.display()),
                source,
            })?;
        let Some(bytes) = bytes else { return Ok(None) };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let on = text.lines().any(|line| enabled_value(line).is_some());
        Ok(Some((text, on)))
    }

    /// Superexpert gives the manager's own file the last word over what the CLI sets, so it is
    /// switched off first. The CLI does it; if the file still says on afterwards, the line is
    /// rewritten in its own style.
    async fn disable_superexpert(&self) -> Result<()> {
        let Some((_, true)) = self.superexpert_state()? else {
            return Ok(());
        };
        self.cli(
            &["superexpert".to_owned(), "off".to_owned()],
            "superexpert off",
        )
        .await
        .map_err(EngineError::Host)?;
        let Some((text, true)) = self.superexpert_state()? else {
            return Ok(());
        };
        let rewritten: Vec<String> = text
            .split_inclusive('\n')
            .map(|line| match enabled_value(line) {
                Some(value) => line.replacen(value, off_form(value), 1),
                None => line.to_owned(),
            })
            .collect();
        self.fs
            .write_like(
                &self.settings_path,
                rewritten.concat().as_bytes(),
                &self.settings_path,
            )
            .map_err(|source| EngineError::Io {
                what: format!("cannot write {}", self.settings_path.display()),
                source,
            })
    }

    /// Runs the steps, then applies and restarts. The reason if anything fails; `undo` holds the
    /// inverses of the steps that were started, the failed one included.
    async fn forward(
        &self,
        steps: &[Planned],
        undo: &mut Vec<Step>,
    ) -> std::result::Result<(), String> {
        for planned in steps {
            // The inverse is queued before the step runs: a step that fails half way is undone.
            undo.extend(planned.undo.clone());
            self.cli(&planned.step.args(), &planned.step.label())
                .await?;
        }
        self.cli(&["expert".to_owned(), "apply".to_owned()], "expert apply")
            .await?;
        self.cli(&["restart".to_owned()], "restart").await?;
        self.reader.wait_ready().await
    }

    /// The inverses newest first (each best effort: the next one still runs), then apply and
    /// restart on what is left.
    async fn back(&self, undo: &[Step]) -> std::result::Result<(), String> {
        for step in undo.iter().rev() {
            if let Err(error) = self.cli(&step.args(), &step.label()).await {
                tracing::warn!("rolling back: {error}");
            }
        }
        self.cli(&["expert".to_owned(), "apply".to_owned()], "expert apply")
            .await?;
        self.cli(&["restart".to_owned()], "restart").await?;
        self.reader.wait_ready().await
    }
}

fn telemt_clock() -> fn() -> u64 {
    || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

/// The value of a `SUPEREXPERT_ENABLED=...` line if it says on, as it is written (with its
/// quotes).
fn enabled_value(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix(SUPEREXPERT_KEY)?;
    let value = rest.trim_start().strip_prefix('=')?;
    let value = value.trim_end();
    let bare = value.trim_matches(|c| c == '"' || c == '\'');
    ["1", "true", "yes", "on"]
        .iter()
        .any(|on| bare.eq_ignore_ascii_case(on))
        .then_some(value)
}

/// `value` as it says off: `"true"` becomes `"false"`, `1` becomes `0`, in the same quotes.
fn off_form(value: &str) -> &'static str {
    match (
        value.trim_matches(|c| c == '"' || c == '\''),
        value.starts_with('"'),
    ) {
        ("1", true) => "\"0\"",
        ("1", false) => "0",
        (word, true) if word.eq_ignore_ascii_case("true") => "\"false\"",
        (word, false) if word.eq_ignore_ascii_case("true") => "false",
        (word, true) if word.eq_ignore_ascii_case("yes") => "\"no\"",
        (word, false) if word.eq_ignore_ascii_case("yes") => "no",
        (_, true) => "\"off\"",
        (_, false) => "off",
    }
}

#[async_trait]
impl<R: CommandRunner + 'static, H: HttpGet + 'static> Engine for MtProxyLEngine<R, H> {
    fn kind(&self) -> EngineKind {
        EngineKind::MtProxyL
    }

    fn port(&self) -> Option<u16> {
        self.reader.port()
    }

    async fn metrics(&self) -> Result<MetricsReport> {
        self.reader.metrics().await
    }

    async fn dc2(&self) -> Option<Dc2Report> {
        self.reader.dc2().await
    }

    async fn health(&self) -> EngineHealth {
        self.reader.health().await
    }

    /// Neither the port nor the primary domain is applied: the pool never writes the first, and
    /// the CLI has no way to set the second.
    async fn apply_config(&self, desired: &DesiredConfig) -> Result<ApplyOutcome> {
        telemt::validate(desired)?;
        let (_, config) = self.reader.load()?;
        let before = self.reader.pool_state();
        let after = telemt::pool_state_of(desired);

        let steps = Self::plan(&config, desired, &before)?;
        // The users the engine is left with: its own, without the ones removed, plus the new.
        let removed = |name: &str| {
            steps
                .iter()
                .any(|p| matches!(&p.step, Step::RemoveSecret { name: n } if n == name))
        };
        let kept = config
            .users()
            .iter()
            .filter(|(name, _)| !removed(name))
            .count();
        let added = steps
            .iter()
            .filter(|p| matches!(p.step, Step::AddSecret { .. }))
            .count();
        if kept + added == 0 {
            return Err(EngineError::Config(
                "the configuration would have no user, and the engine does not start without one"
                    .to_owned(),
            ));
        }

        if steps.is_empty() {
            if before != after {
                self.reader.remember_pool_state(&after);
            }
            return Ok(ApplyOutcome::Unchanged);
        }

        self.disable_superexpert().await?;
        let mut undo = Vec::new();
        match self.forward(&steps, &mut undo).await {
            Ok(()) => {
                self.reader.remember_pool_state(&after);
                Ok(ApplyOutcome::Applied)
            }
            Err(reason) => match self.back(&undo).await {
                Ok(()) => Ok(ApplyOutcome::RolledBack(reason)),
                Err(cause) => Err(EngineError::RollbackFailed { reason, cause }),
            },
        }
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> EngineProbe for MtProxyLEngine<R, H> {
    async fn detect(&self) -> Option<DetectedEngine> {
        Some(DetectedEngine {
            engine: EngineKind::MtProxyL,
            port: self.reader.port()?,
        })
    }
}

impl<R: CommandRunner + 'static, H: HttpGet + 'static> ConfigSink for MtProxyLEngine<R, H> {
    async fn apply(&self, config: &AgentConfig) {
        match self.apply_config(&DesiredConfig::from(config)).await {
            Ok(ApplyOutcome::Unchanged) => tracing::debug!("MTProxyL already serves this config"),
            Ok(ApplyOutcome::Applied) => tracing::info!(
                users = config.users.len(),
                "MTProxyL restarted with the new config"
            ),
            Ok(ApplyOutcome::RolledBack(reason)) => {
                tracing::error!("the new MTProxyL config failed, the old one is back: {reason}");
            }
            Err(error) => tracing::error!("cannot apply the config to MTProxyL: {error}"),
        }
    }
}
