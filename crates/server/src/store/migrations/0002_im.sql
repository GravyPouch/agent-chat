-- M2–M5: roster, offline storage, the message archive, PEP, entity caps,
-- MUC rooms, blocking and private storage (docs/design/embedded-server.md §5.8).
-- Accounts and rooms are keyed by their bare JID (normalized TEXT), so rows
-- of a deleted account are removed explicitly in the same transaction.

CREATE TABLE roster_items (
    account       TEXT NOT NULL,
    contact       TEXT NOT NULL,
    name          TEXT,
    subscription  TEXT NOT NULL,              -- none | to | from | both
    ask_out       INTEGER NOT NULL DEFAULT 0, -- we sent subscribe, no answer yet
    pending_in    INTEGER NOT NULL DEFAULT 0, -- they sent subscribe, not answered
    approved      INTEGER NOT NULL DEFAULT 0, -- pre-approved (RFC 6121 §3.4)
    hidden        INTEGER NOT NULL DEFAULT 0, -- only holds a pending request; not in the roster
    groups_json   TEXT NOT NULL DEFAULT '[]',
    updated_ms    INTEGER NOT NULL,
    PRIMARY KEY (account, contact)
);

-- User and room archives (XEP-0313). `id` doubles as the XEP-0359 stanza-id;
-- `seq` orders the archive (ids are random).
CREATE TABLE archive (
    id           TEXT PRIMARY KEY,
    seq          INTEGER NOT NULL UNIQUE,
    archive_jid  TEXT NOT NULL,
    ts_ms        INTEGER NOT NULL,
    with_jid     TEXT NOT NULL,      -- the peer's bare JID; for rooms the sender's real bare JID
    direction    TEXT NOT NULL,      -- in | out
    origin_id    TEXT,
    from_jid     TEXT NOT NULL,
    stanza       TEXT NOT NULL
);
CREATE INDEX archive_by_owner ON archive (archive_jid, seq);
CREATE INDEX archive_by_with ON archive (archive_jid, with_jid, seq);
CREATE INDEX archive_by_time ON archive (ts_ms);

-- Messages waiting for an account to come online (RFC 6121 §8.5.2).
CREATE TABLE offline (
    account     TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    archive_id  TEXT,
    stanza      TEXT NOT NULL,
    added_ms    INTEGER NOT NULL,
    PRIMARY KEY (account, seq)
);

CREATE TABLE pep_nodes (
    owner        TEXT NOT NULL,
    node         TEXT NOT NULL,
    config_json  TEXT NOT NULL,
    created_ms   INTEGER NOT NULL,
    PRIMARY KEY (owner, node)
);

CREATE TABLE pep_items (
    owner         TEXT NOT NULL,
    node          TEXT NOT NULL,
    item_id       TEXT NOT NULL,
    seq           INTEGER NOT NULL,
    publisher     TEXT NOT NULL,
    payload       TEXT NOT NULL,
    published_ms  INTEGER NOT NULL,
    PRIMARY KEY (owner, node, item_id)
);
CREATE INDEX pep_items_by_seq ON pep_items (owner, node, seq);

-- XEP-0115 verification string → advertised features (global cache).
CREATE TABLE caps (
    ver            TEXT PRIMARY KEY,
    features_json  TEXT NOT NULL,
    verified_ms    INTEGER NOT NULL
);

CREATE TABLE muc_rooms (
    jid             TEXT PRIMARY KEY,
    config_json     TEXT NOT NULL,
    subject         TEXT,
    subject_from    TEXT,
    created_ms      INTEGER NOT NULL,
    last_active_ms  INTEGER NOT NULL
);

CREATE TABLE muc_affiliations (
    room         TEXT NOT NULL,
    jid          TEXT NOT NULL,
    affiliation  TEXT NOT NULL,          -- owner | admin | member | outcast
    nick         TEXT,
    reason       TEXT,
    PRIMARY KEY (room, jid)
);

CREATE TABLE blocklist (
    account  TEXT NOT NULL,
    jid      TEXT NOT NULL,
    PRIMARY KEY (account, jid)
);

-- vcard-temp (key `vcard`) and XEP-0049 private XML (key `private:{ns}name`).
CREATE TABLE private_xml (
    account  TEXT NOT NULL,
    key      TEXT NOT NULL,
    xml      TEXT NOT NULL,
    PRIMARY KEY (account, key)
);
