//! Statistics: ban and traffic figures over 24 hours, 7 days or 30 days, and the client
//! history over 10 minutes to 7 days.
//!
//! | Route                    | Answer                                            |
//! |--------------------------|---------------------------------------------------|
//! | `GET /api/v1/stats/summary` | totals, bans by reason, the latest bans        |
//! | `GET /api/v1/stats/series`  | the figures per bucket                         |
//! | `GET /api/v1/stats/clients` | clients over time, per domain, with event marks |
//!
//! Computed from the database, cached for ten seconds per route and range, each with its own
//! `ETag`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use mtpool_protocol::problem::Problem;
use mtpool_protocol::public::{
    ClientsMark, ClientsMarkKind, ClientsPoint, ClientsRange, ClientsSeries, RecentBan,
    StatsBucket, StatsPoint, StatsRange, StatsSeries, StatsSummary, Vote,
};
use mtpool_protocol::rfc3339;
use rusqlite::Connection;

use super::public::{mask_text, parse_votes, read};
use super::{ApiError, RawQuery, query_map, tagged};
use crate::server::AppState;
use crate::snapshot::etag_of;

/// How long a computed answer is served as it is.
pub const CACHE_MS: i64 = 10_000;
/// The longest list of `recent`.
const RECENT: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Route {
    Summary,
    Series,
}

#[derive(Debug, Clone)]
struct Entry {
    computed_at: i64,
    tag: String,
    body: String,
}

/// The computed answers. Clones share them.
#[derive(Debug, Clone, Default)]
pub struct StatsCache {
    entries: Arc<Mutex<HashMap<(Route, StatsRange), Entry>>>,
    clients: Arc<Mutex<HashMap<ClientsRange, Entry>>>,
}

impl StatsCache {
    pub fn new() -> StatsCache {
        StatsCache::default()
    }

    fn fresh(&self, route: Route, range: StatsRange, now: i64) -> Option<Entry> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(&(route, range))?;
        let age = now - entry.computed_at;
        (0..CACHE_MS).contains(&age).then(|| entry.clone())
    }

    fn put(&self, route: Route, range: StatsRange, entry: Entry) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((route, range), entry);
    }

    fn fresh_clients(&self, range: ClientsRange, now: i64) -> Option<Entry> {
        let entries = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(&range)?;
        let age = now - entry.computed_at;
        (0..CACHE_MS).contains(&age).then(|| entry.clone())
    }

    fn put_clients(&self, range: ClientsRange, entry: Entry) {
        self.clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(range, entry);
    }
}

fn parse_range(query: RawQuery) -> Result<StatsRange, ApiError> {
    let query = query_map(query)?;
    match query.get("range") {
        None => Ok(StatsRange::Day),
        Some(text) => StatsRange::parse(text)
            .ok_or_else(|| ApiError::bad_request("range must be 24h, 7d or 30d.")),
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Acc {
    bans: i64,
    lifetime_sum: i64,
    lifetime_n: i64,
    gap_sum: i64,
    gap_n: i64,
    ingress: i64,
    egress: i64,
}

impl Acc {
    fn ban(&mut self, lifetime: Option<i64>, gap: Option<i64>) {
        self.bans += 1;
        if let Some(v) = lifetime {
            self.lifetime_sum += v;
            self.lifetime_n += 1;
        }
        if let Some(v) = gap {
            self.gap_sum += v;
            self.gap_n += 1;
        }
    }

    fn point(&self) -> StatsPoint {
        StatsPoint {
            bans: self.bans,
            avg_lifetime_sec: mean(self.lifetime_sum, self.lifetime_n),
            avg_gap_sec: mean(self.gap_sum, self.gap_n),
            traffic_ingress_bytes: self.ingress,
            traffic_egress_bytes: self.egress,
            traffic_total_bytes: self.ingress + self.egress,
        }
    }
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn mean(sum: i64, n: i64) -> Option<i64> {
    (n > 0).then(|| (sum as f64 / n as f64).round() as i64)
}

/// Everything both routes show, for one range.
struct Computed {
    from_ms: i64,
    step_sec: i64,
    kpi: StatsPoint,
    buckets: Vec<StatsPoint>,
    by_reason: BTreeMap<String, i64>,
    recent: Vec<RecentBan>,
}

/// Bans that are not re-checks, with the seconds from the registration of the node and from
/// the previous ban. Both are `NULL` when unknown.
const BANS: &str = "WITH real AS (
        SELECT b.id AS id, b.node AS node, b.ip AS ip, b.at AS at, b.votes AS votes,
               CASE WHEN n.id IS NULL THEN NULL
                    ELSE (b.at - CAST(json_extract(n.record, '$.registered_at') AS INTEGER)) / 1000
               END AS lifetime,
               (b.at - LAG(b.at) OVER (ORDER BY b.at, b.id)) / 1000 AS gap
        FROM bans b LEFT JOIN nodes n ON n.id = b.node
        WHERE b.reverify = 0
    )
    SELECT node, ip, at, votes, lifetime, gap FROM real";

struct BanRow {
    node: String,
    ip: String,
    at: i64,
    votes: String,
    lifetime: Option<i64>,
    gap: Option<i64>,
}

fn ban_rows(
    conn: &Connection,
    tail: &str,
    params: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<Vec<BanRow>> {
    let mut statement = conn.prepare_cached(&format!("{BANS} {tail}"))?;
    let rows = statement.query_map(params, |r| {
        Ok(BanRow {
            node: r.get(0)?,
            ip: r.get(1)?,
            at: r.get(2)?,
            votes: r.get(3)?,
            lifetime: r.get(4)?,
            gap: r.get(5)?,
        })
    })?;
    rows.collect()
}

fn compute(conn: &Connection, range: StatsRange, now_ms: i64) -> rusqlite::Result<Computed> {
    let step = range.step_sec();
    let now_s = now_ms.div_euclid(1000);
    let from_s = (now_s - range.seconds()).div_euclid(step) * step;
    let from_ms = from_s * 1000;
    let count = usize::try_from((now_s - from_s) / step + 1).unwrap_or(1);
    let step_ms = step * 1000;
    let bucket_of =
        |at_ms: i64| usize::try_from((at_ms - from_ms) / step_ms).map_or(0, |i| i.min(count - 1));

    let mut accs = vec![Acc::default(); count];
    let mut kpi = Acc::default();
    let mut by_reason = BTreeMap::<String, i64>::new();
    for row in ban_rows(
        conn,
        "WHERE at >= ?1 AND at <= ?2 ORDER BY at, id",
        &[&from_ms, &now_ms],
    )? {
        accs[bucket_of(row.at)].ban(row.lifetime, row.gap);
        kpi.ban(row.lifetime, row.gap);
        let mut blocked: Vec<String> = parse_votes(&row.votes)
            .into_iter()
            .filter(|v| v.vote == Vote::Blocked)
            .map(|v| v.sensor)
            .collect();
        blocked.sort();
        let reason = if blocked.is_empty() {
            "unknown".to_owned()
        } else {
            blocked.join("+")
        };
        *by_reason.entry(reason).or_default() += 1;
    }

    let mut statement = conn.prepare_cached(
        "SELECT (at - ?1) / ?2, SUM(from_client), SUM(to_client)
         FROM traffic WHERE at >= ?1 AND at <= ?3 GROUP BY 1",
    )?;
    let traffic = statement.query_map([from_ms, step_ms, now_ms], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;
    for row in traffic {
        let (index, ingress, egress) = row?;
        let acc = &mut accs[bucket_of(from_ms + index * step_ms)];
        acc.ingress += ingress;
        acc.egress += egress;
        kpi.ingress += ingress;
        kpi.egress += egress;
    }

    let recent = ban_rows(conn, "ORDER BY at DESC, id DESC LIMIT ?1", &[&RECENT])?
        .into_iter()
        .map(|row| RecentBan {
            ts: rfc3339(row.at),
            node_id: row.node,
            ip_masked: mask_text(&row.ip),
            lifetime_sec: row.lifetime,
            gap_sec: row.gap,
        })
        .collect();

    Ok(Computed {
        from_ms,
        step_sec: step,
        kpi: kpi.point(),
        buckets: accs.iter().map(Acc::point).collect(),
        by_reason,
        recent,
    })
}

async fn answer(
    app: &AppState,
    headers: &HeaderMap,
    route: Route,
    range: StatsRange,
) -> Result<Response, ApiError> {
    let now = (app.clock)().0;
    if let Some(hit) = app.stats.fresh(route, range, now) {
        return Ok(tagged(headers, &hit.tag, "application/json", hit.body));
    }
    let computed = read(app, move |conn| compute(conn, range, now)).await?;
    let from = rfc3339(computed.from_ms);
    let body = match route {
        Route::Summary => serde_json::to_string(&StatsSummary {
            range,
            now: rfc3339(now),
            from,
            step_sec: computed.step_sec,
            kpi: computed.kpi,
            by_reason: computed.by_reason,
            recent: computed.recent,
            history_ok: true,
        }),
        Route::Series => serde_json::to_string(&StatsSeries {
            range,
            now: rfc3339(now),
            from,
            step_sec: computed.step_sec,
            buckets: computed
                .buckets
                .into_iter()
                .enumerate()
                .map(|(i, point)| StatsBucket {
                    start_ts: rfc3339(
                        computed.from_ms + i64::try_from(i).unwrap_or(0) * computed.step_sec * 1000,
                    ),
                    point,
                })
                .collect(),
            history_ok: true,
        }),
    }
    .expect("statistics serialize");
    let tag = etag_of(&[&body]);
    app.stats.put(
        route,
        range,
        Entry {
            computed_at: now,
            tag: tag.clone(),
            body: body.clone(),
        },
    );
    Ok(tagged(headers, &tag, "application/json", body))
}

#[utoipa::path(
    get,
    path = "/api/v1/stats/summary",
    tag = "public",
    params(("range" = Option<StatsRange>, Query, description = "The window: 24h (default), 7d or 30d")),
    responses(
        (status = 200, description = "Totals over the range, the bans by reason and the latest bans", body = StatsSummary,
            headers(("ETag" = String, description = "Changes with the figures; send it back in If-None-Match"))),
        (status = 304, description = "The client's copy is current"),
        (status = 400, description = "Unknown range", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn summary(
    State(app): State<AppState>,
    headers: HeaderMap,
    query: RawQuery,
) -> Result<Response, ApiError> {
    answer(&app, &headers, Route::Summary, parse_range(query)?).await
}

#[utoipa::path(
    get,
    path = "/api/v1/stats/series",
    tag = "public",
    params(("range" = Option<StatsRange>, Query, description = "The window: 24h (default), 7d or 30d")),
    responses(
        (status = 200, description = "The figures per bucket over the range", body = StatsSeries,
            headers(("ETag" = String, description = "Changes with the figures; send it back in If-None-Match"))),
        (status = 304, description = "The client's copy is current"),
        (status = 400, description = "Unknown range", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn series(
    State(app): State<AppState>,
    headers: HeaderMap,
    query: RawQuery,
) -> Result<Response, ApiError> {
    answer(&app, &headers, Route::Series, parse_range(query)?).await
}

fn parse_clients_range(query: RawQuery) -> Result<ClientsRange, ApiError> {
    let query = query_map(query)?;
    match query.get("range") {
        None => Ok(ClientsRange::Day),
        Some(text) => ClientsRange::parse(text)
            .ok_or_else(|| ApiError::bad_request("range must be 10m, 1h, 24h or 7d.")),
    }
}

/// The events that become marks on the chart, and what each stands for:
///
/// - `master_changed` is a change of masters (`master_change`);
/// - `node_banned` is a ban (`ban`);
/// - `srmd_action` whose detail says `fold` is a domain folded into another (`fold`).
///
/// Everything else in the log (registrations, quarantines, DNS, rounds, the other SRMD actions,
/// lifted bans, re-check failures) is not a mark.
const MARKS: &str = "SELECT at, CASE kind
            WHEN 'master_changed' THEN 'master_change'
            WHEN 'node_banned' THEN 'ban'
            ELSE 'fold' END
     FROM events
     WHERE at >= ?1 AND at <= ?2
       AND (kind IN ('master_changed', 'node_banned')
            OR (kind = 'srmd_action' AND json_extract(detail, '$.action') = 'fold'))
     ORDER BY at, id";

fn mark_kind(text: &str) -> Option<ClientsMarkKind> {
    match text {
        "master_change" => Some(ClientsMarkKind::MasterChange),
        "ban" => Some(ClientsMarkKind::Ban),
        "fold" => Some(ClientsMarkKind::Fold),
        _ => None,
    }
}

/// The mean of every step that has samples, oldest first. A step without samples is left out.
fn compute_clients(
    conn: &Connection,
    range: ClientsRange,
    now_ms: i64,
) -> rusqlite::Result<ClientsSeries> {
    let step = range.step_sec();
    let step_ms = step * 1000;
    let from_ms = (now_ms - range.seconds() * 1000).div_euclid(step_ms) * step_ms;

    // (sum, count) per step and domain; the pool total has the empty domain.
    let mut sums = BTreeMap::<i64, BTreeMap<String, (i64, i64)>>::new();
    let mut statement = conn.prepare_cached(
        "SELECT (at - ?1) / ?2, domain, SUM(clients), COUNT(*)
         FROM client_samples WHERE at >= ?1 AND at <= ?3 GROUP BY 1, 2",
    )?;
    let rows = statement.query_map([from_ms, step_ms, now_ms], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (index, domain, sum, n) = row?;
        sums.entry(index).or_default().insert(domain, (sum, n));
    }
    let average = |(sum, n): (i64, i64)| mean(sum, n).and_then(|v| u64::try_from(v).ok());
    let points = sums
        .into_iter()
        .filter_map(|(index, mut domains)| {
            // A step is a point only when the total was sampled in it.
            let total = average(domains.remove("")?)?;
            Some(ClientsPoint {
                at: rfc3339(from_ms + index * step_ms),
                total,
                domains: domains
                    .into_iter()
                    .filter_map(|(domain, acc)| Some((domain, average(acc)?)))
                    .collect(),
            })
        })
        .collect();

    let mut statement = conn.prepare_cached(MARKS)?;
    let marks = statement
        .query_map([from_ms, now_ms], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(at, kind)| {
            Some(ClientsMark {
                at: rfc3339(at),
                kind: mark_kind(&kind)?,
            })
        })
        .collect();

    Ok(ClientsSeries {
        range,
        step_sec: step,
        points,
        marks,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/stats/clients",
    tag = "public",
    params(("range" = Option<ClientsRange>, Query, description = "The window: 10m, 1h, 24h (default) or 7d")),
    responses(
        (status = 200, description = "Clients of the pool and of each domain over time, with the moments of master changes, folds and bans", body = ClientsSeries,
            headers(("ETag" = String, description = "Changes with the figures; send it back in If-None-Match"))),
        (status = 304, description = "The client's copy is current"),
        (status = 400, description = "Unknown range", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many requests", body = Problem, content_type = "application/problem+json",
            headers(("Retry-After" = u64, description = "Seconds until the limit window ends"))),
    )
)]
pub async fn clients(
    State(app): State<AppState>,
    headers: HeaderMap,
    query: RawQuery,
) -> Result<Response, ApiError> {
    let range = parse_clients_range(query)?;
    let now = (app.clock)().0;
    if let Some(hit) = app.stats.fresh_clients(range, now) {
        return Ok(tagged(&headers, &hit.tag, "application/json", hit.body));
    }
    let series = read(&app, move |conn| compute_clients(conn, range, now)).await?;
    let body = serde_json::to_string(&series).expect("client history serializes");
    let tag = etag_of(&[&body]);
    app.stats.put_clients(
        range,
        Entry {
            computed_at: now,
            tag: tag.clone(),
            body: body.clone(),
        },
    );
    Ok(tagged(&headers, &tag, "application/json", body))
}
