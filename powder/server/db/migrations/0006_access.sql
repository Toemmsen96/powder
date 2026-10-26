-- Who may play on the server (services/access.rs): bans of players and of addresses, and the whitelist.
-- kind: 'ban_player' (value: profile id), 'ban_ip' (value: an address or a range "a.b.c.d/n", "x::/n"),
-- 'allow_player' (value: profile id; used while the whitelist is on: kv access/whitelist).
-- expires: unix seconds, NULL = until removed. by: who set it (an admin's name, "cli").
CREATE TABLE access_rules (
    kind       TEXT NOT NULL CHECK (kind IN ('ban_player', 'ban_ip', 'allow_player')),
    value      TEXT NOT NULL,
    reason     TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    expires    INTEGER,
    by         TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (kind, value)
);

-- Kicks asked for by another process (the admin CLI); the running server carries them out and deletes them.
CREATE TABLE kick_requests (
    profile_id   TEXT PRIMARY KEY,
    reason       TEXT NOT NULL DEFAULT '',
    requested_at TEXT NOT NULL
);
