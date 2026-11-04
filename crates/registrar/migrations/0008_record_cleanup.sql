-- A banned node has no record any more: its block remembers the owner, for the webhook of the
-- lifted ban. A block older than this column has none.
ALTER TABLE ip_blocks ADD COLUMN owner INTEGER;

-- The token of a node goes with its record. Records removed before this rule left theirs behind.
DELETE FROM tokens
 WHERE kind = 'node' AND node_id IS NOT NULL AND node_id NOT IN (SELECT id FROM nodes);
CREATE INDEX tokens_node_id ON tokens(node_id);
