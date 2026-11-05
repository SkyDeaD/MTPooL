-- A banned node has no record any more: its block remembers the owner, for the webhook of the
-- lifted ban. A block older than this column has none.
ALTER TABLE ip_blocks ADD COLUMN owner INTEGER;

-- How long a banned node lived cannot be asked of its record any more: the ban remembers when the
-- node registered. Records of the bans so far are still here.
ALTER TABLE bans ADD COLUMN node_registered_at INTEGER;
UPDATE bans
   SET node_registered_at = (
       SELECT CAST(json_extract(n.record, '$.registered_at') AS INTEGER)
         FROM nodes n WHERE n.id = bans.node)
 WHERE reverify = 0;

-- The token of a node goes with its record. Records removed before this rule left theirs behind.
DELETE FROM tokens
 WHERE kind = 'node' AND node_id IS NOT NULL AND node_id NOT IN (SELECT id FROM nodes);
CREATE INDEX tokens_node_id ON tokens(node_id);
