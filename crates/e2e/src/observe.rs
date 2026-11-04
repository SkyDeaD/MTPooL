//! What a scenario looks at: the public and admin APIs of the registrar and a way to wait for
//! them to say something.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{POLL_EVERY, Result, Stand, StandError, fail};

/// Runs `check` until it gives a value, for at most `within`. A check that is not satisfied yet
/// says what it saw instead; the last of those is in the error when time runs out.
pub async fn eventually<T, F, Fut>(what: &str, within: Duration, mut check: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::result::Result<T, String>>,
{
    let deadline = Instant::now() + within;
    loop {
        let seen = match check().await {
            Ok(value) => return Ok(value),
            Err(seen) => seen,
        };
        if Instant::now() >= deadline {
            return fail(format!("{what}: not within {within:?}; last seen: {seen}"));
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

impl Stand {
    /// The status code of the registrar's `/readyz`.
    pub async fn readyz(&self) -> Result<u16> {
        let response = self
            .http
            .get(format!("{}/readyz", self.registrar_url.get()))
            .send()
            .await
            .map_err(|e| StandError(format!("GET /readyz: {}", e.without_url())))?;
        Ok(response.status().as_u16())
    }

    /// The public record of one node, `last_round` included.
    pub async fn node_info(&self, node_id: &str) -> Result<Value> {
        self.public(
            &self.registrar_url.get(),
            &format!("/api/v1/nodes/{node_id}"),
        )
        .await
    }

    /// The state of one node in the public list, or `None` when the list does not have it.
    pub async fn node_state(&self, node_id: &str) -> Result<Option<String>> {
        // The node's own page: the list leaves banned nodes out.
        let response = self
            .http
            .get(format!(
                "{}/api/v1/nodes/{node_id}",
                self.registrar_url.get()
            ))
            .send()
            .await
            .map_err(|e| StandError(format!("GET the node: {}", e.without_url())))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let page: Value = response
            .error_for_status()
            .map_err(|e| StandError(format!("GET the node: {}", e.without_url())))?
            .json()
            .await
            .map_err(|e| StandError(format!("GET the node: {}", e.without_url())))?;
        Ok(page["state"].as_str().map(str::to_owned))
    }

    /// The dashboard numbers (`/api/v1/stats/summary`).
    pub async fn stats_summary(&self) -> Result<Value> {
        self.public(&self.registrar_url.get(), "/api/v1/stats/summary")
            .await
    }

    /// The ban log.
    pub async fn bans(&self) -> Result<Vec<Value>> {
        match self
            .public(&self.registrar_url.get(), "/api/v1/bans")
            .await?
        {
            Value::Array(bans) => Ok(bans),
            other => fail(format!("the ban log is not an array: {other}")),
        }
    }

    /// The event log (admin), newest first; `kind` and `node` narrow it.
    pub async fn events(&self, kind: Option<&str>, node: Option<&str>) -> Result<Vec<Value>> {
        let mut path = "/api/v1/admin/events?limit=500".to_owned();
        if let Some(kind) = kind {
            path.push_str(&format!("&kind={kind}"));
        }
        if let Some(node) = node {
            path.push_str(&format!("&node={node}"));
        }
        match self.admin.get(&path).await? {
            Value::Array(events) => Ok(events),
            other => fail(format!("the event log is not an array: {other}")),
        }
    }

    /// Waits until the node is in one of `states` in the public list; returns the one it is in.
    pub async fn wait_state_in(
        &self,
        node_id: &str,
        states: &[&str],
        within: Duration,
    ) -> Result<String> {
        eventually(
            &format!("node {node_id} in one of {states:?}"),
            within,
            || async {
                match self.node_state(node_id).await {
                    Ok(Some(state)) if states.contains(&state.as_str()) => Ok(state),
                    Ok(other) => Err(format!("{other:?}")),
                    Err(error) => Err(error.to_string()),
                }
            },
        )
        .await
    }

    /// The calls the stand-ins have counted for one sensor (`globalping`, `cheburcheck`, ...).
    pub async fn sensor_calls(&self, sensor: &str) -> Result<u64> {
        let state = self.fakes_state().await?;
        Ok(state["calls"][sensor].as_u64().unwrap_or(0))
    }
}
