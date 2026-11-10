#!/usr/bin/env bash
# OMEMO interop regression test against python-omemo (slixmpp-omemo), both
# protocol versions, both directions. Needs the dev Prosody running
# (dev/prosody/setup.sh) and a built CLI (cargo build).
#
#   dev/omemo-bot/interop-test.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
AC="$root/target/debug/agent-chat"
export AGENT_CHAT_HOME="$(mktemp -d)"
user="interop$RANDOM"
fail=0

cleanup() {
  "$AC" --profile "$user" daemon stop >/dev/null 2>&1 || true
  docker rm -f "omemo-bot-it2-$user" "omemo-bot-itl-$user" >/dev/null 2>&1 || true
  rm -rf "$AGENT_CHAT_HOME"
}
trap cleanup EXIT

docker exec agent-chat-prosody prosodyctl register "$user" localhost "$user-pass" >/dev/null
echo "$user-pass" | "$AC" --profile "$user" account add "$user@localhost" \
  --host 127.0.0.1 --insecure-plaintext --store config --password-stdin >/dev/null
"$AC" --profile "$user" inbox --consumer it >/dev/null

# Bots greet us first: they initiate the key exchange.
"$root/dev/omemo-bot/run.sh" "it2-$user" twomemo "$user@localhost" >/dev/null
"$root/dev/omemo-bot/run.sh" "itl-$user" oldmemo "$user@localhost" >/dev/null

expect() { # consumer-inbox must contain a message from $1 with protocol $2 and text $3
  local from="$1" proto="$2" text="$3"
  for _ in $(seq 1 30); do
    if "$AC" --profile "$user" inbox --consumer "chk-$RANDOM" --from-start --peek \
        | python3 -c "
import json,sys
msgs=json.load(sys.stdin)
ok=any(m['from']=='$from' and (m.get('encryption') or {}).get('protocol')=='$proto'
       and not (m.get('encryption') or {}).get('error') and '$text' in m['body'] for m in msgs)
sys.exit(0 if ok else 1)"; then
      echo "PASS  $proto  $from: $text"; return 0
    fi
    sleep 1
  done
  echo "FAIL  $proto  $from: expected '$text'"; fail=1
}

expect "it2-$user@localhost" omemo2 "hello from it2-$user@localhost"
expect "itl-$user@localhost" omemo_legacy "hello from itl-$user@localhost"

# We send on the sessions they started; they echo back.
"$AC" --profile "$user" send "it2-$user@localhost" "ping-2" --require-encryption >/dev/null
"$AC" --profile "$user" send "itl-$user@localhost" "ping-legacy" --require-encryption >/dev/null
expect "it2-$user@localhost" omemo2 "echo[urn:xmpp:omemo:2]: ping-2"
expect "itl-$user@localhost" omemo_legacy "echo[eu.siacs.conversations.axolotl]: ping-legacy"

exit $fail
