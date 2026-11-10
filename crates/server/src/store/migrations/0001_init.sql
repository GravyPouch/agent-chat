-- M1: hub metadata, hosted domains and accounts (docs/design/embedded-server.md §5.8).
-- Conventions: timestamps are INTEGER milliseconds, JIDs are normalized TEXT,
-- explicit ids, nothing relies on rowid. Never edit an applied migration.

CREATE TABLE meta (
    key    TEXT PRIMARY KEY,
    value  BLOB NOT NULL
);

CREATE TABLE hosts (
    domain       TEXT PRIMARY KEY,
    created_ms   INTEGER NOT NULL,
    config_json  TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE accounts (
    id                   TEXT PRIMARY KEY,
    domain               TEXT NOT NULL REFERENCES hosts(domain),
    localpart            TEXT NOT NULL,
    role                 TEXT NOT NULL,      -- admin | user | probation
    state                TEXT NOT NULL,      -- active | locked
    created_ms           INTEGER NOT NULL,
    created_via          TEXT NOT NULL,      -- admin | invite | pow | open
    last_login_ms        INTEGER,
    scram_salt           BLOB NOT NULL,
    scram_iterations     INTEGER NOT NULL,
    sha1_stored_key      BLOB NOT NULL,
    sha1_server_key      BLOB NOT NULL,
    sha256_stored_key    BLOB NOT NULL,
    sha256_server_key    BLOB NOT NULL,
    password_changed_ms  INTEGER NOT NULL,
    UNIQUE (domain, localpart)
);
