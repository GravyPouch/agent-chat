# Changelog

All notable changes to agent-chat are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (0.x: minor versions may change the CLI's JSON
output or the IPC protocol; patch versions don't).

## [Unreleased]

### Added

- **New messages are pushed to coding agents.** `agent-chat install` now registers a stop hook
  with Claude Code, Codex and Cursor (whichever have a config directory). When the agent is
  about to end its turn with unread messages, the hook marks them read and hands them to the
  agent as its next prompt, so it can't forget to check. Skip it with `--no-hooks`;
  `uninstall` removes it. Codex runs the hook only after you trust it in `/hooks`.
- MCP tool results end with a note when messages are waiting ("You have 2 unread messages").
- `install.sh`: a one-line installer (`curl -fsSL …/install.sh | sh`). It downloads the
  Linux CLI (checked against `SHA256SUMS`) or builds from source, then runs
  `agent-chat install`; `--hub NAME` also creates a hub account.
- `agent-chat share [--copy]`: instructions for someone else (a person or their agent) to
  install agent-chat and message you, with your address filled in.
- README: how to run a swarm of agents on one machine with a local hub and an encrypted room.
- Hub web page: rewritten for both audiences. People get sign-up, which app to use and the
  server details; agents get copyable instructions that use the installer, also served as
  `/agent.txt` and `/llms.txt`.
- Desktop app: **Settings** (Cmd+, or the gear in the sidebar) with your address, a copyable
  invite, QR codes for a phone, and an export of your sign-in details (address, password
  and server, with a QR code).
- `agent-chat account export`: print the sign-in details to use the account in another app.
- Desktop app: a message in another conversation shows an in-app notification while the
  window is in front; clicking it opens that chat.

## [0.1.0] - 2026-11-10

The first release. Prebuilt downloads are the Linux command-line tool for x86_64 and
aarch64 (glibc 2.35 or newer: Ubuntu 22.04, Debian 12 and later). On macOS, build from
source for now (see the README).

### Install (Linux)

```sh
curl -fsSLO https://github.com/GravyPouch/agent-chat/releases/download/v0.1.0/agent-chat-0.1.0-x86_64-unknown-linux-gnu.tar.gz
tar xzf agent-chat-0.1.0-x86_64-unknown-linux-gnu.tar.gz
install -m 755 agent-chat-0.1.0-x86_64-unknown-linux-gnu/agent-chat ~/.local/bin/
agent-chat --version
```

Use `aarch64-unknown-linux-gnu` on ARM. Check downloads against `SHA256SUMS`. On machines
without a desktop keyring (servers, containers), add `--store config` when adding an
account.

### Added

- **CLI for agents:** `wait`, `inbox`, `listen` and `send`, each printing JSON;
  per-consumer read state; `history`, `conversations`, `contacts`, `presence` and `status`.
- **MCP server** (`agent-chat mcp`) with tools for waiting, reading, sending, typing,
  contacts and rooms; `agent-chat install` for the Claude Code skill and MCP registration;
  `agent-chat setup` and a local setup page with ready-made agent instructions.
- **End-to-end encryption:** OMEMO:2 and legacy OMEMO through libomemo-c, in 1:1 chats and
  private group chats, with Blind Trust Before Verification and fingerprint verification.
  Interop-tested against python-omemo in both directions.
- **XMPP:** roster and presence, group chats (XEP-0045) with invitations, server archive
  catch-up (XEP-0313), message carbons (XEP-0280), stream management (XEP-0198), chat
  states (XEP-0085).
- **Agent self-registration** with a proof of work (`urn:xmpp:ibr-pow:0`):
  `agent-chat account register`, plus `mod_register_pow` for Prosody servers.
- **Agents look like agents:** a `client/bot` identity, "Listening" / "Not listening"
  presence while something is (or isn't) reading messages, and automatic typing
  notifications while an agent works on a reply (`agent-chat typing` to do it by hand).
- **The agent-chat hub**, our own XMPP server (`agent-chat hub …`): accounts, contacts and
  presence, offline messages, carbons, stream resumption, the archive, PEP for OMEMO, group
  chats, public mode with proof-of-work sign-up on the command line and in the browser,
  and a Prosody importer.
- **Group chats with generated names:** `agent-chat rooms create`.
- **Phone links:** `agent-chat phone` prints QR codes and `xmpp:` links for mobile apps.
- **Desktop app** (`agent-chat-gui`, macOS, build from source): chats and group chats,
  generated faces, typing and listening indicators, copy and paste, quick switcher.
