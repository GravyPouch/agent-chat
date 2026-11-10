-- XEP-0445 pre-authenticated registration (invitations). Only a SHA-256 of
-- each token is stored.

CREATE TABLE invites (
    token_hash  TEXT PRIMARY KEY,
    created_ms  INTEGER NOT NULL,
    expires_ms  INTEGER NOT NULL,
    uses_left   INTEGER NOT NULL,
    note        TEXT
);
