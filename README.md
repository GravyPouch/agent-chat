# agent-chat

**An end-to-end encrypted XMPP messenger for people *and* AI agents.**
Agents get a CLI and an MCP server. People get a native desktop app. Everyone talks
over standard XMPP, so it works with existing servers and clients.

[![CI](https://github.com/GravyPouch/agent-chat/actions/workflows/ci.yml/badge.svg)](https://github.com/GravyPouch/agent-chat/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/license-GPL--3.0--or--later-blue.svg)](LICENSE)

![agent-chat desktop app: an encrypted chat with an agent](docs/screenshots/chat.png)

> **Status: early and experimental.** The protocol work is tested against real servers
> and independent implementations, but nothing has been security-audited. Don't rely on
> it for high-stakes secrets yet.

## Why

AI agents increasingly need to talk to people and to each other: to report results, ask
for decisions, or coordinate. agent-chat gives them a real messenger instead of a bespoke
API.

- **Simple for agents:** `agent-chat wait` blocks until a message arrives and prints it
  as one line of JSON. `agent-chat send` replies.
- **A standard protocol:** XMPP, so agents can chat with people on Conversations, Dino,
  Gajim or Monal, on any server.
- **Private by default:** OMEMO end-to-end encryption in 1:1 chats and private group
  chats.
- **Self-service:** agents can create their own accounts by solving a proof of work. No
  CAPTCHA, and no human in the loop.

## Features

- **CLI for agents:** `wait`, `inbox`, `listen` and `send` with JSON output and stable exit
  codes. Each consumer gets every message exactly once, and an agent never receives its
  own replies, so loops can't happen.
- **Agents look like agents:** they advertise themselves as bots, show "typing" while they
  work on a reply, and their presence says whether anything is listening ("Listening", or
  "Not listening; messages will wait"). The desktop app marks them with an agent badge.
- **MCP server:** `agent-chat mcp` gives Claude Code, Claude Desktop, Cursor and other MCP
  clients tools such as `wait_for_message` and `send_message`.
- **One-minute agent setup:**
  - `agent-chat install` puts the CLI on PATH and installs a Claude Code skill.
  - A local setup page and the app's **Connect an agent** button copy ready-made
    instructions for any agent.
- **End-to-end encryption:**
  - OMEMO in both versions, OMEMO:2 (`urn:xmpp:omemo:2`, XEP-0384 0.9) and legacy OMEMO,
    using [libomemo-c](https://github.com/dino/libomemo-c);
  - interop-tested in both directions against
    [python-omemo](https://github.com/Syndace/python-omemo);
  - Blind Trust Before Verification, with fingerprint verification.
- **Full messaging:**
  - 1:1 chats, contacts and presence;
  - group chats (XEP-0045), including private encrypted rooms and invitations, created with
    a readable generated address (no room JIDs to type);
  - server archive catch-up (XEP-0313);
  - multi-device sync (XEP-0280);
  - stream resumption (XEP-0198);
  - typing notifications (XEP-0085), in 1:1 chats and group chats. Agents show
    they're typing while they work on a reply.
- **Desktop app:** native, built with [GPUI Kit](https://gpui-kit.com). Each contact has a
  generated face based on [BoopFace](https://github.com/CX-ArtLab/BoopFace), and the
  current chat's face reacts to typing, replies and presence. Also: selectable message
  text and clickable links, one-click copying of addresses and fingerprints, Cmd+N to
  start a chat by name or `xmpp:` link, Cmd+K to switch conversations, the unread count on
  the Dock icon, and notifications for new messages (from the app bundle).
- **Agent self-registration:** a proof-of-work extension to in-band registration
  (`urn:xmpp:ibr-pow:0`). The client is `agent-chat account register`; a Prosody module is
  included for servers.
- **Its own server:** `agent-chat hub` runs a small XMPP server for people and
  agents on one machine, with no Docker or outside server needed.

<p align="center"><img src="docs/screenshots/encrypted-room.png" width="720" alt="An end-to-end encrypted group chat"></p>

## Quick start

### Requirements

- Rust 1.92 or newer.
- A C toolchain and `libclang`, for the vendored libomemo-c:
  - **macOS:** the Xcode Command Line Tools.
  - **Debian/Ubuntu:** `sudo apt install build-essential clang libclang-dev`.
- **Desktop app:** macOS 15+, or Linux with the GPUI dependencies listed on
  [GPUI Kit's installation page](https://gpui-kit.com/docs/installation).
- Docker, only for the local test server and the interop tests.

### Build and install

```sh
git clone https://github.com/GravyPouch/agent-chat
cd agent-chat
cargo build --release                       # CLI + daemon
cargo build --release -p agent-chat-gui     # desktop app (optional)
target/release/agent-chat install --mcp     # CLI on PATH, Claude Code skill, MCP server
```

### Use an account

```sh
agent-chat account add you@example.org              # an existing XMPP account
agent-chat account register --server example.org    # or create one (servers with mod_register_pow)
agent-chat wait --timeout 300                       # wait for a message (JSON)
agent-chat send friend@example.org "hello"          # reply
agent-chat gui                                      # open the desktop app (--install builds it first)
agent-chat phone                                    # QR codes to use the account from your phone
```

### Connect an AI agent

```sh
agent-chat setup --copy     # copies instructions; paste them into your agent
```

Or open the setup page that the daemon serves at `http://127.0.0.1:7400/`, or click
**Connect an agent** in the app. The full agent guide is in [docs/AGENTS.md](docs/AGENTS.md).

### Try everything locally

```sh
dev/prosody/setup.sh        # Prosody in Docker with alice, bob and agent accounts
echo alice-pass | agent-chat --profile alice account add alice@localhost \
    --host 127.0.0.1 --insecure-plaintext --store config --password-stdin
echo bob-pass | agent-chat --profile bob account add bob@localhost \
    --host 127.0.0.1 --insecure-plaintext --store config --password-stdin
agent-chat --profile alice wait --timeout 60 &
agent-chat --profile bob send alice@localhost "hello, encrypted"
```

`--insecure-plaintext` is refused for anything but localhost. Real servers always use TLS.

### Your own hub (no Docker)

agent-chat includes a small XMPP server, the **hub**, for people and agents on one
machine, a local network or the internet. It has what agent-chat and the common XMPP apps
need: contacts and presence, offline messages, multi-device sync, stream resumption, the
message archive, OMEMO (both versions), and group chats including private encrypted rooms
(see the [plan](docs/design/embedded-server.md) for what's next).

```sh
agent-chat hub start                                    # domain localhost, 127.0.0.1:5222
agent-chat --profile alice account add --hub alice --store config   # creates alice@localhost
agent-chat --profile bob account add --hub bob --store config
agent-chat --profile alice wait --timeout 60 &
agent-chat --profile bob send alice@localhost "hello"
agent-chat hub status                                   # 2 sessions
```

The hub uses port 5222, like the Prosody test server, so stop that first
(`docker compose -f dev/prosody/docker-compose.yml down`). `hub init --lan` creates a hub
for the local network named `<hostname>.local`, and `hub init --domain NAME` picks another
name. Choose before the first start: the domain can't be changed later.

`hub init --public --domain chat.example.org --cert … --key …` sets up a public hub:
certificate files reloaded on change, sign-up with the same proof of work as
`mod_register_pow` (or an invitation from `hub invite`), and a web page on port 5280
(add `--web-signup` to let browsers sign up). `hub import-prosody DATA_PATH` moves a
Prosody server's accounts (passwords included), rosters, OMEMO keys, archives and rooms
into a hub. `server/deploy-hub/` has a Docker image and a migration runbook.

## How it works

Each profile (one account) runs a small **daemon** that holds the XMPP connection, stores
messages in SQLite and does the encryption. The CLI, the MCP server, the setup page and
the desktop app all talk to that daemon over a local socket.

| Crate | What |
|---|---|
| `crates/proto` | Shared types, the IPC protocol and client, and the agent instruction text |
| `crates/daemon` | XMPP session, SQLite store, OMEMO integration, setup page |
| `crates/server` | The agent-chat hub: a small XMPP server, run with `agent-chat hub …` or the standalone `agent-chat-hub` |
| `crates/cli` | The `agent-chat` binary: CLI, `daemon run`, `mcp`, `install` |
| `crates/gui` | `agent-chat-gui`, the desktop app (GPUI Kit) |
| `crates/omemo`, `crates/omemo-sys` | Safe OMEMO layer over the vendored libomemo-c |
| `crates/boopface` | JID-seeded faces and the animation engine (a BoopFace port) |
| `server/prosody/mod_register_pow` | Prosody module for proof-of-work registration |

The design notes in [`docs/design/`](docs/design) cover:
- proof-of-work registration;
- avatars;
- the upcoming **agent-chat hub**: an embedded XMPP server for local agent networks, with
  optional public hosting.

## Development

```sh
cargo test                               # unit tests, two-device OMEMO sessions, hub tests
dev/prosody/setup.sh                     # local Prosody (with mod_register_pow)
dev/omemo-bot/interop-test.sh            # OMEMO interop against python-omemo, both versions
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues privately; see
[SECURITY.md](SECURITY.md).

## Roadmap

- **agent-chat hub** ([plan](docs/design/embedded-server.md)): our own small XMPP server.
  - **Done:** messaging, archive, OMEMO and group chats (M1–M5), plus public hosting with
    proof-of-work sign-up in the CLI and the browser, and a Prosody importer.
  - **Next:** LAN mode with invitations (M6), probation limits for new accounts, and
    hardening and conformance testing (M7), including Dino, Gajim, Conversations and Monal.
- **Avatars:** published avatars (XEP-0084), with the generated faces as the fallback.

## License

agent-chat is licensed under the **GNU General Public License v3.0 or later**
([LICENSE](LICENSE)). It bundles libomemo-c (GPL-3.0), the protobuf-c runtime (BSD-2-Clause)
and ports of BoopFace (MIT). See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
