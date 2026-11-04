-- Admin and bot API: the audit log, webhook subscriptions with their delivery log, secrets
-- that can be changed without a restart, token scopes and soft-deleted owners.
-- Timestamps are Unix milliseconds.

-- One row per mutating admin action. `actor` is `token:<id>` or `session`. `detail` is JSON
-- and never holds a secret.
CREATE TABLE audit (
    id     INTEGER PRIMARY KEY,
    at     INTEGER NOT NULL,
    actor  TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT,
    detail TEXT NOT NULL
);
CREATE INDEX audit_at ON audit(at);

-- `owner_id` is the owner who registered it; `scope` is `owner` (events of that owner's
-- nodes) or `pool` (every event). `secret` signs the deliveries. `events` is a JSON array of
-- webhook event names.
CREATE TABLE webhooks (
    id          INTEGER PRIMARY KEY,
    owner_id    INTEGER NOT NULL REFERENCES owners(id),
    scope       TEXT NOT NULL DEFAULT 'owner',
    url         TEXT NOT NULL,
    secret      TEXT NOT NULL,
    events      TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX webhooks_owner ON webhooks(owner_id);

-- `status` is pending, delivered or failed.
CREATE TABLE webhook_deliveries (
    id            INTEGER PRIMARY KEY,
    webhook_id    INTEGER NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    event_id      TEXT NOT NULL,
    event         TEXT NOT NULL,
    payload       TEXT NOT NULL,
    attempt       INTEGER NOT NULL DEFAULT 0,
    status        TEXT NOT NULL,
    response_code INTEGER,
    last_error    TEXT,
    at            INTEGER NOT NULL,
    next_at       INTEGER
);
CREATE INDEX webhook_deliveries_hook ON webhook_deliveries(webhook_id, id);

-- Values set from the panel. They win over the environment variable of the same name.
CREATE TABLE secrets (
    name       TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- A bot token with the scope `pool:read` may register pool-wide webhooks.
ALTER TABLE tokens ADD COLUMN scope TEXT NOT NULL DEFAULT '';

-- A deleted owner stays as a row so its id is never handed to a new owner; its name is
-- renamed to free the unique name.
ALTER TABLE owners ADD COLUMN deleted_at INTEGER;
