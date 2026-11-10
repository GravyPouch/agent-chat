#!/usr/bin/env bash
# Run an OMEMO interop bot against the dev Prosody (dev/prosody/setup.sh first).
#   dev/omemo-bot/run.sh NAME BACKENDS [GREET_JID]
#   BACKENDS: both | twomemo | oldmemo
# Creates the account NAME@localhost (password NAME-pass) if needed.
set -euo pipefail
cd "$(dirname "$0")"
name="$1"; backends="$2"; greet="${3:-}"
docker build -q -t agent-chat-omemo-bot . >/dev/null
docker exec agent-chat-prosody prosodyctl register "$name" localhost "$name-pass" >/dev/null 2>&1 || true
docker rm -f "omemo-bot-$name" >/dev/null 2>&1 || true
docker run -d --name "omemo-bot-$name" --network prosody_default \
  -e BOT_JID="$name@localhost" -e BOT_PASSWORD="$name-pass" -e XMPP_HOST=prosody \
  -e OMEMO_BACKENDS="$backends" -e GREET_JID="$greet" \
  -v "omemo-bot-$name:/data" agent-chat-omemo-bot >/dev/null
echo "started omemo-bot-$name ($backends); logs: docker logs -f omemo-bot-$name"
