-- Prosody configuration for LOCAL DEVELOPMENT AND TESTS ONLY.
-- Plaintext client connections are allowed because the server is only
-- published on 127.0.0.1. Never use this config on a reachable host.

admins = { }
plugin_paths = { "/etc/prosody/custom-modules" }

modules_enabled = {
    "roster"; "saslauth"; "tls"; "disco"; "ping"; "time"; "version"; "uptime";
    "carbons";      -- XEP-0280
    "pep";          -- XEP-0163 (OMEMO device lists and bundles)
    "private"; "bookmarks"; "blocklist"; "vcard_legacy";
    "mam";          -- XEP-0313
    "smacks";       -- XEP-0198
    "csi_simple";
    "register";     -- XEP-0077 (used by the registration tests)
    "admin_shell";
}

-- Dev only: let clients connect without TLS (localhost-published port).
c2s_require_encryption = false
s2s_require_encryption = false
allow_registration = true
authentication = "internal_hashed"
storage = "internal"

default_archive_policy = true
archive_expires_after = "1w"
max_archive_query_results = 250

log = { info = "*console" }

VirtualHost "localhost"

Component "conference.localhost" "muc"
    modules_enabled = { "muc_mam" }
    restrict_room_creation = false
    muc_log_by_default = true
    muc_log_all_rooms = true
    muc_room_default_persistent = true
