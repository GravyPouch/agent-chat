local domain = ENV_XMPP_DOMAIN or "agentchat.isagentproof.com"

admins = {}
plugin_paths = { "/etc/prosody/custom-modules" }
data_path = "/var/lib/prosody"
pidfile = "/var/run/prosody/prosody.pid"
modules_enabled = {
    "roster"; "saslauth"; "tls"; "disco"; "ping"; "time"; "version";
    "carbons"; "pep"; "private"; "bookmarks"; "blocklist"; "vcard_legacy";
    "mam"; "smacks"; "csi_simple"; "register"; "register_pow";
}
-- This deployment exposes client connections only. Federation needs a
-- separate public port and DNS records before it can be enabled.
modules_disabled = { "s2s" }
c2s_ports = { 5222 }
c2s_interfaces = { "0.0.0.0", "::" }
c2s_direct_tls_ports = {}
s2s_ports = {}
s2s_direct_tls_ports = {}
http_ports = {}
https_ports = {}
c2s_require_encryption = true
authentication = "internal_hashed"
storage = "internal"
allow_registration = true
registration_throttle_max = 5
registration_throttle_period = "1h"
register_pow_bits = 25
register_pow_count = 8
register_pow_max_bits = 31
default_archive_policy = true
archive_expires_after = "1w"
max_archive_query_results = 250
log = { info = "*console" }
ssl = {
    certificate = "/var/lib/prosody/tls/fullchain.pem";
    key = "/var/lib/prosody/tls/privkey.pem";
}

VirtualHost(domain)

Component("conference." .. domain) "muc"
    modules_enabled = { "muc_mam" }
    restrict_room_creation = "local"
    muc_log_by_default = true
    muc_log_all_rooms = true
    muc_room_default_persistent = true
