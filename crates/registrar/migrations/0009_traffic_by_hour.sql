-- A ban made before `bans.node_registered_at` whose record was already replaced found nothing in
-- the nodes: the journal still says when that node registered. The registration that counts is
-- the last one before the ban.
UPDATE bans
   SET node_registered_at = (
       SELECT MAX(e.at)
         FROM events e
        WHERE e.node = bans.node AND e.kind = 'node_registered' AND e.at <= bans.at)
 WHERE reverify = 0 AND node_registered_at IS NULL;

-- Traffic is kept for the life of the pool: one row per node and hour, with the users of the
-- node's owner beside the pool's. What was stored so far is the pool's.
CREATE TABLE traffic_by_hour (
    node              TEXT NOT NULL,
    at                INTEGER NOT NULL,
    from_client       INTEGER NOT NULL,
    to_client         INTEGER NOT NULL,
    other_from_client INTEGER NOT NULL DEFAULT 0,
    other_to_client   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (node, at)
);
INSERT INTO traffic_by_hour(node, at, from_client, to_client)
SELECT node, at / 3600000 * 3600000, SUM(from_client), SUM(to_client)
  FROM traffic
 GROUP BY 1, 2;
DROP TABLE traffic;
ALTER TABLE traffic_by_hour RENAME TO traffic;
CREATE INDEX traffic_at ON traffic(at);

-- The owners' traffic was only summed per node. What the nodes have counted goes in as this hour.
INSERT INTO traffic(node, at, from_client, to_client, other_from_client, other_to_client)
SELECT node,
       CAST(strftime('%s', 'now') AS INTEGER) / 3600 * 3600000,
       0,
       0,
       COALESCE(json_extract(stats, '$.other_from_client_total'), 0),
       COALESCE(json_extract(stats, '$.other_to_client_total'), 0)
  FROM node_stats
 WHERE COALESCE(json_extract(stats, '$.other_from_client_total'), 0) > 0
    OR COALESCE(json_extract(stats, '$.other_to_client_total'), 0) > 0
ON CONFLICT(node, at) DO UPDATE SET
    other_from_client = excluded.other_from_client,
    other_to_client = excluded.other_to_client;
