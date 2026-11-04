-- Agent rollouts: one row per release offered to the node agents. `state` is the JSON of the
-- rollout (phase, canary, the stage of every node), rewritten on every change so that a
-- registrar restart, a self-update included, carries on where it was. Timestamps are Unix
-- milliseconds.
CREATE TABLE rollouts (
    id         INTEGER PRIMARY KEY,
    version    TEXT NOT NULL,
    state      TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
