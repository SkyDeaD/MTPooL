-- Tables the store writes from core's PersistOp and from events. Tables for tokens, owners,
-- sessions, webhooks, audit and rollouts come with their own migrations.
-- Timestamps are Unix milliseconds. JSON columns hold serde output of the core types.

CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL
);

-- One row: the whole Settings as JSON.
CREATE TABLE settings (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);

-- Scalar columns are for queries; `record` is the whole NodeRecord and is what load reads.
-- `health` is written by the batched NodeHealth op and wins over record.health on load.
CREATE TABLE nodes (
    id     TEXT PRIMARY KEY,
    owner  INTEGER NOT NULL,
    ip     TEXT NOT NULL,
    port   INTEGER NOT NULL,
    engine TEXT NOT NULL,
    state  TEXT NOT NULL,
    record TEXT NOT NULL,
    health TEXT
);
CREATE INDEX nodes_ip ON nodes(ip);

CREATE TABLE node_stats (
    node  TEXT PRIMARY KEY,
    stats TEXT NOT NULL
);

CREATE TABLE sensor_rounds (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    node       TEXT NOT NULL,
    generation INTEGER NOT NULL,
    at         INTEGER NOT NULL,
    verdict    TEXT NOT NULL
);
CREATE INDEX sensor_rounds_node_at ON sensor_rounds(node, at);

-- `data` is the whole SensorVote (vote and evidence); `vote` is blocked, clear or abstain.
CREATE TABLE sensor_votes (
    round_id INTEGER NOT NULL REFERENCES sensor_rounds(id) ON DELETE CASCADE,
    sensor   TEXT NOT NULL,
    vote     TEXT NOT NULL,
    data     TEXT NOT NULL,
    PRIMARY KEY (round_id, sensor)
);

-- `votes` is the JSON array of [sensor, vote] pairs that confirmed the ban (or, with
-- reverify = 1, that failed to lift it). lifted_at stays NULL while the row is open.
CREATE TABLE bans (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    ip               TEXT NOT NULL,
    node             TEXT NOT NULL,
    at               INTEGER NOT NULL,
    votes            TEXT NOT NULL,
    reverify         INTEGER NOT NULL,
    lifted_at        INTEGER,
    lifted_by_admin  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX bans_ip ON bans(ip);

CREATE TABLE ip_blocks (
    ip                TEXT PRIMARY KEY,
    node_id           TEXT NOT NULL,
    blocked_at        INTEGER NOT NULL,
    next_reverify_at  INTEGER
);

CREATE TABLE tombstones (
    node_id    TEXT NOT NULL,
    ip         TEXT NOT NULL,
    strikes    INTEGER NOT NULL,
    until      INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (node_id, ip)
);
CREATE INDEX tombstones_updated_at ON tombstones(updated_at);

CREATE TABLE assignments (
    domain TEXT PRIMARY KEY,
    slots  TEXT NOT NULL
);

CREATE TABLE dns_records (
    domain TEXT PRIMARY KEY,
    dns    TEXT NOT NULL
);

-- One row: created domains and CNAMEs. Client counts and tick counters are not stored.
CREATE TABLE srmd_state (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);

CREATE TABLE events (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    at     INTEGER NOT NULL,
    kind   TEXT NOT NULL,
    node   TEXT,
    domain TEXT,
    detail TEXT NOT NULL
);
CREATE INDEX events_at ON events(at);

CREATE TABLE traffic (
    node        TEXT NOT NULL,
    at          INTEGER NOT NULL,
    from_client INTEGER NOT NULL,
    to_client   INTEGER NOT NULL
);
CREATE INDEX traffic_at ON traffic(at);
CREATE INDEX traffic_node_at ON traffic(node, at);
