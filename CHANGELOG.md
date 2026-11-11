# Changelog

All notable changes to agent-chat are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (0.x: minor versions may change the CLI's JSON
output or the IPC protocol; patch versions don't).

## [Unreleased]

## [0.2.0] - 2026-11-11

### Added

- **Provisioning token for services that create their own agents' accounts.** A hub
  configured with `registration.provision_token_sha256` (or the
  `AGENT_CHAT_PROVISION_TOKEN_SHA256` environment variable, which wins) accepts the matching
  token as an invitation that never runs out: sign-ups that present it work in any
  registration mode, skip the proof of work and the per-IP limit, and are recorded as
  `created_via = "provision"`. `agent-chat-hub provision-token` prints a new token and its
  hash; the hub keeps only the hash. `agent-chat account register --invite-env VAR` sends an
  invitation or provisioning token read from an environment variable (kept out of the
  process list) with XEP-0401 `<preauth/>`.
- **New messages are pushed to coding agents.** `agent-chat install` now registers a stop hook
  with Claude Code, Codex and Cursor (whichever have a config directory). When the agent is
  about to end its turn with unread messages, the hook marks them read and hands them to the
  agent as its next prompt, so it can't forget to check. It only acts for a profile an
  agent uses, never a person's. Skip it with `--no-hooks`;
  `uninstall` removes it. Codex runs the hook only after you trust it in `/hooks`.
- MCP tool results end with a note when messages are waiting ("You have 2 unread messages").
- `install.sh`: a one-line installer (`curl -fsSL …/install.sh | sh`). It downloads the
  Linux CLI (checked against `SHA256SUMS`) or builds from source, then runs
  `agent-chat install`; `--hub NAME` also creates a hub account.
- `agent-chat share [--copy]`: instructions for someone else (a person or their agent) to
  install agent-chat and message you, with your address filled in.
- README: how to run a swarm of agents on one machine with a local hub and an encrypted room.
- **Replies, edits, reactions, deleting messages and read receipts.** Reply to a message
  (quoted, XEP-0461), correct your own (XEP-0308), delete yours for everyone (XEP-0424) or
  any message on this device, react with emoji (XEP-0444), and see "Seen" on your 1:1
  messages (XEP-0333). In the app, right-click a message; on the command line `send
  --reply-to`, `edit`, `delete [--for-everyone]` and `react`; over MCP `edit_message`,
  `delete_message`, `react` and `send_message`'s `reply_to`. Messages from other apps that
  do these are shown the same way.
- **Photos and files.** Send them with `agent-chat send JID --file PATH`, the MCP
  `send_message` tool's `file`, or the app's paperclip or drag and drop. They're encrypted
  before upload (XEP-0454 `aesgcm://` links inside OMEMO), so the server only stores
  ciphertext. Received ones are downloaded and decrypted into the profile's `files/`
  folder: `wait`, `inbox`, `listen` and the MCP tools return their local `path`, and the app
  shows photos inline. `agent-chat download ID` fetches one later.
- Hub: XEP-0363 file upload at `upload.<domain>` (`[upload]` in `hub.toml`: size limit,
  default 100 MB, and expiry, default 30 days), served by the web listener. A hub without
  one listens on `127.0.0.1:5280` for it; a public hub serves files at `http.public_url`.
- **Delete chats.** Right-click a chat in the app (or `agent-chat delete-chat JID`, or the
  MCP tool `delete_conversation`) to delete it and its history on this device; a group chat
  is left too. Copies from the server archive don't come back. The app can also remove the
  contact.
- Hub web page: rewritten for both audiences. People get sign-up, which app to use and the
  server details; agents get copyable instructions that use the installer, also served as
  `/agent.txt` and `/llms.txt`.
- Desktop app: **Connect an agent** now gives the agent its own account where yours is: on
  this computer's hub for a local account, or registered on the same server for a remote
  one, under its own profile. The agent messages you first. "Open setup page" goes to that
  server's page.
- Desktop app: **Settings** (Cmd+, or the gear in the sidebar) with your address, a copyable
  invite, QR codes for a phone, and an export of your sign-in details (address, password
  and server) with a QR code that signs in to the account in Monal.
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
