#!/usr/bin/env bash
# Run an OMEMO interop bot against the dev Prosody (dev/prosody/setup.sh first),
# or against a hub with XMPP_SERVER=hub (see dev/lib/server.sh).
#   dev/omemo-bot/run.sh NAME BACKENDS [GREET_JID]
#   BACKENDS: both | twomemo | oldmemo
# Creates the account NAME@localhost (password NAME-pass) if needed.
set -euo pipefail
cd "$(dirname "$0")"
root="$(cd ../.. && pwd)"
AC="${AC:-$root/target/debug/agent-chat}"
# shellcheck source=../lib/server.sh
source "$root/dev/lib/server.sh"
name="$1"; backends="$2"; greet="${3:-}"
docker build -q -t agent-chat-omemo-bot . >/dev/null
server_register "$name" "$name-pass"
docker rm -f "omemo-bot-$name" >/dev/null 2>&1 || true
# shellcheck disable=SC2046
docker run -d --name "omemo-bot-$name" $(server_docker_args) \
  -e BOT_JID="$name@localhost" -e BOT_PASSWORD="$name-pass" \
  -e OMEMO_BACKENDS="$backends" -e GREET_JID="$greet" \
  -v "omemo-bot-$name:/data" agent-chat-omemo-bot >/dev/null
echo "started omemo-bot-$name ($backends); logs: docker logs -f omemo-bot-$name"
