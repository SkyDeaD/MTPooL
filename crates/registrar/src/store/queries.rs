//! SQL for restoring the state on start and for retention.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use mtpool_core::{
    Assignments, DnsState, IpBlock, NodeHealth, NodeId, NodeRecord, NodeStats, OwnerId, PoolState,
    Settings, Slot, SrmdState, Tombstone, Ts,
};
use rusqlite::{Connection, OptionalExtension};
use serde::de::DeserializeOwned;

use super::StoreError;

const DAY_MS: i64 = 24 * 3600 * 1000;
const EVENTS_TTL_MS: i64 = 30 * DAY_MS;
const TRAFFIC_TTL_MS: i64 = 35 * DAY_MS;
const ROUNDS_TTL_MS: i64 = 35 * DAY_MS;
const TOMBSTONE_TTL_MS: i64 = 7 * DAY_MS;
const SAMPLES_TTL_MS: i64 = 8 * DAY_MS;
const NODE_SAMPLES_TTL_MS: i64 = 31 * DAY_MS;

fn invalid(what: &str, detail: impl std::fmt::Display) -> StoreError {
    StoreError::InvalidData(format!("{what}: {detail}"))
}

fn parse_json<T: DeserializeOwned>(what: &str, text: &str) -> Result<T, StoreError> {
    serde_json::from_str(text).map_err(|e| invalid(what, e))
}

fn parse_ip(what: &str, text: &str) -> Result<Ipv4Addr, StoreError> {
    text.parse().map_err(|e| invalid(what, e))
}

fn parse_id(what: &str, text: &str) -> Result<NodeId, StoreError> {
    NodeId::parse(text).map_err(|e| invalid(what, format!("{text:?}: {e}")))
}

/// Reads the whole state in one read transaction, so the tables belong to one moment.
pub(super) fn load_state(conn: &Connection, now: Ts) -> Result<PoolState, StoreError> {
    let tx = conn.unchecked_transaction()?;

    let settings = load_settings(&tx)?;
    let nodes = load_nodes(&tx)?;
    let stats = load_stats(&tx, &nodes)?;
    let blocks = load_blocks(&tx)?;
    let tombstones = load_tombstones(&tx, now)?;
    let assignments = load_assignments(&tx)?;
    let dns = load_dns(&tx, &settings)?;
    let srmd = load_srmd(&tx)?;

    tx.commit()?;
    Ok(PoolState {
        settings,
        nodes,
        stats,
        blocks,
        tombstones,
        assignments,
        dns,
        srmd,
    })
}

fn load_settings(conn: &Connection) -> Result<Settings, StoreError> {
    let json: Option<String> = conn
        .query_row("SELECT json FROM settings WHERE id = 1", [], |r| r.get(0))
        .optional()?;
    let Some(json) = json else {
        return Ok(Settings::default());
    };
    let settings: Settings = parse_json("settings", &json)?;
    settings.validate().map_err(|e| invalid("settings", e))?;
    Ok(settings)
}

fn load_nodes(conn: &Connection) -> Result<BTreeMap<NodeId, NodeRecord>, StoreError> {
    let mut stmt = conn.prepare("SELECT record, health FROM nodes ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut nodes = BTreeMap::new();
    for row in rows {
        let (record, health) = row?;
        let mut record: NodeRecord = parse_json("node record", &record)?;
        // The health column is newer than the one inside the record when a batched write
        // reached the disk after the last upsert.
        if let Some(health) = health {
            record.health = parse_json::<NodeHealth>("node health", &health)?;
        }
        nodes.insert(record.id.clone(), record);
    }
    Ok(nodes)
}

/// Every node has stats: a node whose batched stats row was lost gets the default.
fn load_stats(
    conn: &Connection,
    nodes: &BTreeMap<NodeId, NodeRecord>,
) -> Result<BTreeMap<NodeId, NodeStats>, StoreError> {
    let mut stmt = conn.prepare("SELECT node, stats FROM node_stats")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut stats = BTreeMap::new();
    for row in rows {
        let (node, json) = row?;
        let node = parse_id("node_stats.node", &node)?;
        if nodes.contains_key(&node) {
            stats.insert(node, parse_json("node stats", &json)?);
        }
    }
    for id in nodes.keys() {
        stats.entry(id.clone()).or_default();
    }
    Ok(stats)
}

fn load_blocks(conn: &Connection) -> Result<Vec<IpBlock>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT ip, node_id, blocked_at, next_reverify_at, owner FROM ip_blocks \
         ORDER BY blocked_at, ip",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut blocks = Vec::new();
    for row in rows {
        let (ip, node_id, blocked_at, next, owner) = row?;
        blocks.push(IpBlock {
            ip: parse_ip("ip_blocks.ip", &ip)?,
            node_id: parse_id("ip_blocks.node_id", &node_id)?,
            blocked_at: Ts(blocked_at),
            next_reverify_at: next.map(Ts),
            owner: owner.map(OwnerId),
        });
    }
    Ok(blocks)
}

/// Tombstones past their 7 days are not loaded.
fn load_tombstones(conn: &Connection, now: Ts) -> Result<Vec<Tombstone>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT node_id, ip, strikes, until, updated_at FROM tombstones \
         ORDER BY updated_at, node_id, ip",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, u32>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut tombstones = Vec::new();
    for row in rows {
        let (node_id, ip, strikes, until, updated_at) = row?;
        let tombstone = Tombstone {
            node_id: parse_id("tombstones.node_id", &node_id)?,
            ip: parse_ip("tombstones.ip", &ip)?,
            strikes,
            until: Ts(until),
            updated_at: Ts(updated_at),
        };
        if !tombstone.expired(now) {
            tombstones.push(tombstone);
        }
    }
    Ok(tombstones)
}

fn load_assignments(conn: &Connection) -> Result<Assignments, StoreError> {
    let mut stmt = conn.prepare("SELECT domain, slots FROM assignments")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (domain, slots) = row?;
        map.insert(domain, parse_json::<Vec<Slot>>("master slots", &slots)?);
    }
    Ok(Assignments(map))
}

/// DNS rows of domains that are no longer in the settings are dropped (the rows stay on disk).
fn load_dns(
    conn: &Connection,
    settings: &Settings,
) -> Result<BTreeMap<String, DnsState>, StoreError> {
    let mut stmt = conn.prepare("SELECT domain, dns FROM dns_records")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (domain, dns) = row?;
        if settings.domains.contains(&domain) {
            map.insert(domain, parse_json::<DnsState>("dns state", &dns)?);
        }
    }
    Ok(map)
}

fn load_srmd(conn: &Connection) -> Result<SrmdState, StoreError> {
    let json: Option<String> = conn
        .query_row("SELECT json FROM srmd_state WHERE id = 1", [], |r| r.get(0))
        .optional()?;
    match json {
        Some(json) => parse_json("srmd state", &json),
        None => Ok(SrmdState::default()),
    }
}

/// Events older than 30 days, traffic and sensor rounds older than 35 days, client samples
/// older than 8 days, node samples older than 31 days, tombstones past their 7 days. Bans are kept: they are the audit of bans.
pub(super) fn retain(conn: &Connection, now: Ts) -> Result<(), StoreError> {
    let cutoff = |ttl: i64| now.0.saturating_sub(ttl);
    conn.execute("DELETE FROM events WHERE at < ?1", [cutoff(EVENTS_TTL_MS)])?;
    conn.execute(
        "DELETE FROM traffic WHERE at < ?1",
        [cutoff(TRAFFIC_TTL_MS)],
    )?;
    // The votes of a round go with it (`ON DELETE CASCADE`, foreign keys are on).
    conn.execute(
        "DELETE FROM sensor_rounds WHERE at < ?1",
        [cutoff(ROUNDS_TTL_MS)],
    )?;
    conn.execute(
        "DELETE FROM client_samples WHERE at < ?1",
        [cutoff(SAMPLES_TTL_MS)],
    )?;
    // Buckets are Unix seconds divided by their length: compare in the same unit.
    conn.execute(
        "DELETE FROM node_samples WHERE bucket < ?1",
        [cutoff(NODE_SAMPLES_TTL_MS).div_euclid(1000) / super::history::BUCKET_SEC],
    )?;
    // `Tombstone::expired` counts the 7th day as expired.
    conn.execute(
        "DELETE FROM tombstones WHERE updated_at <= ?1",
        [cutoff(TOMBSTONE_TTL_MS)],
    )?;
    Ok(())
}
