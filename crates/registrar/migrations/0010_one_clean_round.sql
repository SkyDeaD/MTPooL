-- One clean round is enough to leave quarantine now: the confirmation from Russia stands behind
-- it. The value the old default stored becomes the new one; a value the owner chose stays.
UPDATE settings SET json = json_set(json, '$.clean_rounds_to_exit', 1)
 WHERE json_extract(json, '$.clean_rounds_to_exit') = 2;
