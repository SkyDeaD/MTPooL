-- Owners, API tokens and admin sessions. Tokens and sessions are stored as SHA-256 hashes
-- only: the secret itself is shown once, when it is issued.
-- Timestamps are Unix milliseconds.

CREATE TABLE owners (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    node_limit INTEGER NOT NULL,
    note       TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);

-- `kind` is admin, enroll, node, bot or metrics. `hash` is SHA-256 of the whole token;
-- `prefix` is its first 12 characters, to tell tokens apart in the panel. `owner_id` is set
-- for enroll and bot tokens, `node_id` for node tokens.
CREATE TABLE tokens (
    id           INTEGER PRIMARY KEY,
    kind         TEXT NOT NULL,
    hash         BLOB NOT NULL UNIQUE,
    prefix       TEXT NOT NULL,
    owner_id     INTEGER REFERENCES owners(id),
    node_id      TEXT,
    expires_at   INTEGER,
    revoked_at   INTEGER,
    last_used_at INTEGER,
    created_at   INTEGER NOT NULL
);
CREATE INDEX tokens_kind ON tokens(kind);

-- `hash` is SHA-256 of the cookie value; `csrf` is the token the page sends in a header.
CREATE TABLE sessions (
    hash       BLOB PRIMARY KEY,
    csrf       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
