-- Accounts imported from Prosody have SCRAM-SHA-1 keys only (Prosody's
-- `password_hash` defaults to SHA-1), so the SHA-256 keys become optional.
-- SQLite can't drop NOT NULL in place: rebuild the table.

CREATE TABLE accounts_new (
    id                   TEXT PRIMARY KEY,
    domain               TEXT NOT NULL REFERENCES hosts(domain),
    localpart            TEXT NOT NULL,
    role                 TEXT NOT NULL,
    state                TEXT NOT NULL,
    created_ms           INTEGER NOT NULL,
    created_via          TEXT NOT NULL,      -- admin | invite | pow | open | import
    last_login_ms        INTEGER,
    scram_salt           BLOB NOT NULL,
    scram_iterations     INTEGER NOT NULL,
    sha1_stored_key      BLOB NOT NULL,
    sha1_server_key      BLOB NOT NULL,
    sha256_stored_key    BLOB,
    sha256_server_key    BLOB,
    password_changed_ms  INTEGER NOT NULL,
    UNIQUE (domain, localpart)
);
INSERT INTO accounts_new SELECT id, domain, localpart, role, state, created_ms, created_via, last_login_ms,
    scram_salt, scram_iterations, sha1_stored_key, sha1_server_key, sha256_stored_key, sha256_server_key,
    password_changed_ms FROM accounts;
DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;
