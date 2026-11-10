# agent-chat hub: the embedded XMPP server, as a container that can stand in
# for server/deploy (Prosody). Build from the repository root:
#
#   docker build -f server/deploy-hub/Dockerfile -t agent-chat-hub .
#
# Volumes: /var/lib/agent-chat (data), /certs (read-only, the shared
# certificate volume). Ports: 5222 (XMPP clients, STARTTLS), 5280 (web page).
# See server/deploy-hub/README.md.

FROM rust:1-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends cmake clang \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# Only what Cargo needs, so editing deployment files doesn't rebuild Rust.
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --locked --release -p agent-chat-server --bin agent-chat-hub \
 && strip target/release/agent-chat-hub

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --home-dir /var/lib/agent-chat --shell /usr/sbin/nologin hub
COPY --from=build /src/target/release/agent-chat-hub /usr/local/bin/agent-chat-hub
COPY server/deploy-hub/hub.toml /usr/local/share/agent-chat-hub/hub.toml
COPY server/deploy-hub/start-hub.sh /usr/local/bin/start-hub
RUN chmod 0755 /usr/local/bin/start-hub

ENV AGENT_CHAT_HOME=/var/lib/agent-chat
EXPOSE 5222 5280
HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
  CMD ["/usr/local/bin/agent-chat-hub", "health"]
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/start-hub"]
