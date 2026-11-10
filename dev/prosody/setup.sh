#!/usr/bin/env bash
# Start the local Prosody server and create test accounts.
#   alice@localhost / alice-pass
#   bob@localhost   / bob-pass
#   agent@localhost / agent-pass
# Rooms: any name@conference.localhost is created on first join.
set -euo pipefail
cd "$(dirname "$0")"

docker compose up -d

echo -n "waiting for Prosody on 127.0.0.1:5222 "
for _ in $(seq 1 60); do
  if nc -z 127.0.0.1 5222 2>/dev/null; then echo " up"; break; fi
  echo -n "."; sleep 1
done

for user in alice bob agent; do
  # prosodyctl exits non-zero if the user exists; that's fine.
  docker exec agent-chat-prosody prosodyctl register "$user" localhost "$user-pass" >/dev/null 2>&1 || true
done

cat <<'MSG'
Prosody is running. Configure a profile with, for example:

  echo alice-pass | agent-chat --profile alice account add alice@localhost \
      --host 127.0.0.1 --insecure-plaintext --store config --password-stdin

Stop it with:  docker compose -f dev/prosody/docker-compose.yml down
MSG
