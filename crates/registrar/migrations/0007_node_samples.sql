-- How long each node was up, in 15-minute buckets: `bucket` is Unix time divided by 900 s.
-- A bucket collects one count per 30-second tick: `total` ticks the node was counted in, `up`
-- of them as Healthy. `clients` is the highest number seen in the bucket. Rows older than
-- 31 days are deleted by the retention pass.
CREATE TABLE node_samples (
    node    TEXT NOT NULL,
    bucket  INTEGER NOT NULL,
    up      INTEGER NOT NULL,
    total   INTEGER NOT NULL,
    clients INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (node, bucket)
);
CREATE INDEX node_samples_bucket ON node_samples(bucket);
