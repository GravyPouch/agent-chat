#!/bin/bash
# Entrypoint of the agent-chat hub image (server/deploy-hub/Dockerfile).
# Runs as root to set up the data volume and copy certificates, then runs the
# hub as the unprivileged `hub` user. Mirrors server/deploy/start-prosody.sh.
set -euo pipefail

home="${AGENT_CHAT_HOME:-/var/lib/agent-chat}"
hub="$home/hub"
domain="${XMPP_DOMAIN:-agentchat.isagentproof.com}"
mkdir -p "$hub/tls"

# First start: the configuration template with this deployment's domain.
if [[ ! -f "$hub/hub.toml" ]]; then
    sed "s/^domain = .*/domain = \"$domain\"/" /usr/local/share/agent-chat-hub/hub.toml >"$hub/hub.toml"
fi

# The certificate service writes /certs/xmpp/{fullchain,privkey}.pem (root,
# 0600); the hub reads its own copies and reloads them within 30 s.
install_certificate() {
    install -o hub -g hub -m 644 /certs/xmpp/fullchain.pem "$hub/tls/fullchain.pem"
    install -o hub -g hub -m 600 /certs/xmpp/privkey.pem "$hub/tls/privkey.pem"
}
if [[ -s /certs/xmpp/fullchain.pem && -s /certs/xmpp/privkey.pem ]]; then
    install_certificate
fi

# One-time migration: PROSODY_IMPORT names a copy of Prosody's data_path
# (README.md, "Cutover"). It runs as root because Prosody's files belong to
# the prosody user; the marker file keeps it from running twice.
if [[ -n "${PROSODY_IMPORT:-}" && ! -f "$hub/prosody-import.json" ]]; then
    echo "Importing Prosody data from $PROSODY_IMPORT"
    /usr/local/bin/agent-chat-hub import-prosody "$PROSODY_IMPORT" >"$hub/prosody-import.json.tmp"
    mv "$hub/prosody-import.json.tmp" "$hub/prosody-import.json"
    cat "$hub/prosody-import.json"
fi
chown -R hub:hub "$home"
chmod 700 "$home" "$hub"

as_hub() { setpriv --reuid=hub --regid=hub --init-groups "$@"; }

as_hub /usr/local/bin/agent-chat-hub run &
hub_pid=$!
trap 'kill -TERM "$hub_pid" 2>/dev/null || true; wait "$hub_pid" || true; exit 0' TERM INT

(
    previous=""
    [[ -s "$hub/tls/fullchain.pem" ]] && previous="$(sha256sum "$hub/tls/fullchain.pem" | cut -d' ' -f1)"
    while kill -0 "$hub_pid" 2>/dev/null; do
        if [[ -s /certs/xmpp/fullchain.pem && -s /certs/xmpp/privkey.pem ]]; then
            current="$(sha256sum /certs/xmpp/fullchain.pem | cut -d' ' -f1)"
            if [[ "$current" != "$previous" ]]; then
                install_certificate
                previous="$current"
                echo "Installed renewed XMPP certificate"
            fi
        fi
        sleep 60
    done
) &
wait "$hub_pid"
