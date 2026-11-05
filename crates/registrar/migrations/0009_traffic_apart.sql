-- A ban made before `bans.node_registered_at` whose record was already replaced found nothing in
-- the nodes: the journal still says when that node registered. The registration that counts is
-- the last one before the ban.
UPDATE bans
   SET node_registered_at = (
       SELECT MAX(e.at)
         FROM events e
        WHERE e.node = bans.node AND e.kind = 'node_registered' AND e.at <= bans.at)
 WHERE reverify = 0 AND node_registered_at IS NULL;

-- The users of a node's owner get their own series beside the pool's. They were not stored over
-- time so far: what is here stays the pool's, with nothing known about the others.
ALTER TABLE traffic ADD COLUMN other_from_client INTEGER NOT NULL DEFAULT 0;
ALTER TABLE traffic ADD COLUMN other_to_client INTEGER NOT NULL DEFAULT 0;
