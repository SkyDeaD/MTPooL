//! The check of the second Telegram data center from telemt's control API (`/v1/stats/dcs`).

use mtpool_protocol::agent::Dc2Report;
use serde_json::Value;

/// A snapshot older than this is not trusted.
pub const MAX_AGE_SECS: u64 = 300;
/// A snapshot dated further ahead than this is not trusted either.
pub const MAX_AHEAD_SECS: u64 = 120;

pub const API_ERROR: &str = "dc2_api_error";
pub const ME_DISABLED: &str = "dc2_me_disabled";
pub const STALE: &str = "dc2_stale";
pub const ABSENT: &str = "dc2_absent";
pub const NO_ENDPOINTS: &str = "dc2_no_endpoints";
pub const NO_FRESH: &str = "dc2_no_fresh";

/// Judges what the API answered. `answer` is the body of a successful response, or a failure to
/// get one (`Err` holds no reason: it is always `dc2_api_error`). `now` is the clock of the node,
/// in seconds since the epoch.
///
/// The checks run in this order, and the first one that fails is the reason: the answer itself
/// (`dc2_api_error`), the middle proxy being on (`dc2_me_disabled`), the age of the snapshot
/// (`dc2_stale`), DC 2 being in it (`dc2_absent`), its endpoints (`dc2_no_endpoints`) and a
/// writer that is alive and fresh (`dc2_no_fresh`).
pub fn evaluate(answer: Result<&str, ()>, now: u64) -> Dc2Report {
    match check(answer, now) {
        Ok(()) => Dc2Report {
            ok: true,
            reason: None,
        },
        Err(reason) => Dc2Report {
            ok: false,
            reason: Some(reason.to_owned()),
        },
    }
}

fn check(answer: Result<&str, ()>, now: u64) -> Result<(), &'static str> {
    let body = answer.map_err(|()| API_ERROR)?;
    let value: Value = serde_json::from_str(body).map_err(|_| API_ERROR)?;
    if value.get("ok") != Some(&Value::Bool(true)) {
        return Err(API_ERROR);
    }
    let data = value
        .get("data")
        .and_then(Value::as_object)
        .ok_or(API_ERROR)?;
    if data.get("middle_proxy_enabled") == Some(&Value::Bool(false)) {
        return Err(ME_DISABLED);
    }
    let generated = data
        .get("generated_at_epoch_secs")
        .and_then(Value::as_u64)
        .ok_or(STALE)?;
    if now.saturating_sub(generated) > MAX_AGE_SECS
        || generated.saturating_sub(now) > MAX_AHEAD_SECS
    {
        return Err(STALE);
    }
    let dcs = data.get("dcs").and_then(Value::as_array).ok_or(API_ERROR)?;
    let dc2 = dcs
        .iter()
        .find(|dc| dc.get("dc").and_then(Value::as_i64) == Some(2))
        .ok_or(ABSENT)?;
    let has_endpoints = dc2
        .get("endpoints")
        .and_then(Value::as_array)
        .is_some_and(|endpoints| !endpoints.is_empty());
    if !has_endpoints {
        return Err(NO_ENDPOINTS);
    }
    let fresh = dc2
        .get("fresh_alive_writers")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if fresh == 0 {
        return Err(NO_FRESH);
    }
    Ok(())
}
