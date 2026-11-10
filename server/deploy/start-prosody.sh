#!/bin/bash
set -euo pipefail

domain="${XMPP_DOMAIN:-agentchat.isagentproof.com}"
mkdir -p /var/lib/prosody/tls /var/run/prosody
chown prosody:prosody /var/lib/prosody /var/lib/prosody/tls /var/run/prosody
chmod 700 /var/lib/prosody/tls

install_certificate() {
    install -o prosody -g prosody -m 644 /certs/xmpp/fullchain.pem /var/lib/prosody/tls/fullchain.pem
    install -o prosody -g prosody -m 600 /certs/xmpp/privkey.pem /var/lib/prosody/tls/privkey.pem
}

if [[ -s /certs/xmpp/fullchain.pem && -s /certs/xmpp/privkey.pem ]]; then
    install_certificate
elif [[ ! -s /var/lib/prosody/tls/privkey.pem ]]; then
    # Temporary bootstrap certificate. Clients verify certificates and will
    # connect only after the certificate service obtains a trusted one.
    openssl req -x509 -newkey rsa:2048 -nodes -days 1 \
        -subj "/CN=$domain" -addext "subjectAltName=DNS:$domain" \
        -keyout /var/lib/prosody/tls/privkey.pem \
        -out /var/lib/prosody/tls/fullchain.pem 2>/dev/null
    chown prosody:prosody /var/lib/prosody/tls/*
    chmod 600 /var/lib/prosody/tls/privkey.pem
fi

runuser -u prosody -- prosody -F &
prosody_pid=$!
trap 'kill -TERM "$prosody_pid" 2>/dev/null || true; wait "$prosody_pid" || true; exit 0' TERM INT

(
    previous="$(sha256sum /var/lib/prosody/tls/fullchain.pem | cut -d' ' -f1)"
    while kill -0 "$prosody_pid" 2>/dev/null; do
        if [[ -s /certs/xmpp/fullchain.pem && -s /certs/xmpp/privkey.pem ]]; then
            current="$(sha256sum /certs/xmpp/fullchain.pem | cut -d' ' -f1)"
            if [[ "$current" != "$previous" ]]; then
                install_certificate
                # The image ships systemd detection but runs without systemd.
                # Signal Prosody itself, using its own PID rather than runuser's.
                if kill -HUP "$(cat /var/run/prosody/prosody.pid)"; then
                    previous="$current"
                    echo "Installed renewed XMPP certificate"
                fi
            fi
        fi
        sleep 60
    done
) &
wait "$prosody_pid"
