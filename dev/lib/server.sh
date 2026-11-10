# Which XMPP server the dev scripts talk to (docs/design/embedded-server.md §8.3).
#
#   XMPP_SERVER=prosody (default): the dev Prosody in Docker (dev/prosody/setup.sh).
#   XMPP_SERVER=hub: an agent-chat hub for the domain `localhost`, started by
#     server_start_hub in $AGENT_CHAT_HOME on a free loopback port ($HUB_PORT).
#     Containers reach it through host.docker.internal.
#
# Callers set AC to the agent-chat binary. Source this file; don't run it.

XMPP_SERVER="${XMPP_SERVER:-prosody}"

# Start a throwaway hub (only for XMPP_SERVER=hub). Exports HUB_PORT.
server_start_hub() {
  [ "$XMPP_SERVER" = hub ] || return 0
  HUB_PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
  export HUB_PORT
  "$AC" hub init --listen "127.0.0.1:$HUB_PORT" >/dev/null
  "$AC" hub start >/dev/null
}

server_stop_hub() {
  [ "$XMPP_SERVER" = hub ] || return 0
  "$AC" hub stop >/dev/null 2>&1 || true
}

# server_register NAME PASSWORD: create NAME@localhost (ignored if it exists).
server_register() {
  local name="$1" pass="$2"
  case "$XMPP_SERVER" in
    hub) printf '%s' "$pass" | "$AC" hub user add "$name" --password-stdin >/dev/null 2>&1 || true ;;
    *) docker exec agent-chat-prosody prosodyctl register "$name" localhost "$pass" >/dev/null 2>&1 || true ;;
  esac
}

# The client port on 127.0.0.1.
server_port() {
  case "$XMPP_SERVER" in
    hub) echo "$HUB_PORT" ;;
    *) echo 5222 ;;
  esac
}

# `docker run` arguments that let a container reach the server.
server_docker_args() {
  case "$XMPP_SERVER" in
    hub) echo "--add-host host.docker.internal:host-gateway -e XMPP_HOST=host.docker.internal -e XMPP_PORT=$HUB_PORT" ;;
    *) echo "--network prosody_default -e XMPP_HOST=prosody" ;;
  esac
}
