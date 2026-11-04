-- Client numbers over time: one row per moment and domain, written every 30 seconds. The pool
-- total is stored under the empty domain. `at` is Unix milliseconds. Rows older than 8 days
-- are deleted by the retention pass.
CREATE TABLE client_samples (
    at      INTEGER NOT NULL,
    domain  TEXT NOT NULL,
    clients INTEGER NOT NULL,
    PRIMARY KEY (at, domain)
);
