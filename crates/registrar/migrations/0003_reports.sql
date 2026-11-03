-- Globalping measurement ids an agent has reported. An id counts once: a report that names
-- one again is refused, so a single clean measurement cannot vouch for a node twice.
-- Timestamps are Unix milliseconds.

CREATE TABLE gp_measurements (
    id   TEXT PRIMARY KEY,
    node TEXT NOT NULL,
    at   INTEGER NOT NULL
);
CREATE INDEX gp_measurements_at ON gp_measurements(at);
