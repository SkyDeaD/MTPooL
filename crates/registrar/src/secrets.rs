//! Secrets of the external services that can be changed from the panel without a restart.
//!
//! A value set in the panel lives in the `secrets` table and wins; without one the environment
//! variable of the same service is used. Values are never logged, listed or returned.

use rusqlite::OptionalExtension;

use crate::store::Store;

/// A secret the panel can set, and the environment variable that is its fallback.
pub const SECRETS: &[(&str, &str)] = &[
    ("globalping_token", "GLOBALPING_TOKEN"),
    ("redflag_api_key", "REDFLAG_API_KEY"),
    ("cloudflare_api_token", "CLOUDFLARE_API_TOKEN"),
];

/// Names of the secrets, in the order the settings document lists them.
pub fn names() -> impl Iterator<Item = &'static str> {
    SECRETS.iter().map(|(name, _)| *name)
}

pub fn is_known(name: &str) -> bool {
    SECRETS.iter().any(|(known, _)| *known == name)
}

fn env_var(name: &str) -> Option<&'static str> {
    SECRETS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, var)| *var)
}

fn from_env(name: &str) -> Option<String> {
    let value = std::env::var(env_var(name)?).ok()?;
    (!value.is_empty()).then_some(value)
}

fn stored(store: &Store, name: &str) -> Option<String> {
    store
        .read()
        .query_row("SELECT value FROM secrets WHERE name = ?1", [name], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .unwrap_or_else(|e| {
            tracing::error!(secret = name, error = %e, "cannot read a stored secret");
            None
        })
        .filter(|v| !v.is_empty())
}

/// The value of a secret: the one set in the panel, else the environment variable. `None` for
/// an unknown name or when neither is set. Blocks briefly on a read connection.
pub fn secret(store: &Store, name: &str) -> Option<String> {
    if !is_known(name) {
        return None;
    }
    stored(store, name).or_else(|| from_env(name))
}

/// Whether a value is available, from the panel or the environment.
pub fn is_set(store: &Store, name: &str) -> bool {
    secret(store, name).is_some()
}
