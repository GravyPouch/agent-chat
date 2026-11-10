# agent-chat

An XMPP client written in Rust for people and AI agents:

- **CLI for agents:** `agent-chat wait` blocks until a message arrives and prints it as JSON. See [docs/AGENTS.md](docs/AGENTS.md).
- **End-to-end encryption by default:** in 1:1 chats and private group chats. OMEMO, both OMEMO:2 (`urn:xmpp:omemo:2`, XEP-0384 0.9) and legacy OMEMO, via [libomemo-c](https://github.com/dino/libomemo-c). Tested against python-omemo in both directions.
- **Protocol coverage:**
  - 1:1 chat, roster and presence (RFC 6120/6121)
  - group chat (XEP-0045)
  - server archive catch-up (XEP-0313)
  - multi-device sync (XEP-0280)
  - stream resumption (XEP-0198)
- **Desktop GUI:** `agent-chat-gui`, built with GPUI Kit. Avatars are robot faces from a Rust
  port of [BoopFace](https://github.com/CX-ArtLab/BoopFace) (MIT, CX ART Lab), seeded by the JID
  and coloured per XEP-0392; the open chat's face reacts to typing, replies and presence.

## Layout

| Path | What |
|---|---|
| `crates/proto` | Shared types, IPC protocol and client |
| `crates/daemon` | XMPP session, SQLite store, IPC server, OMEMO integration |
| `crates/cli` | The `agent-chat` binary (also hosts `daemon run`) |
| `crates/omemo`, `crates/omemo-sys` | Safe OMEMO layer over vendored libomemo-c |
| `crates/gui` | `agent-chat-gui` (GPUI Kit) |
| `crates/boopface` | Rust port of BoopFace: JID-seeded faces, motion engine (see `third_party/boopface`) |
| `dev/prosody` | Local Prosody for development and tests |
| `dev/omemo-bot` | python-omemo interop bot and test |
| `docs/design` | Design notes (PoW self-registration) |

## Quick start (local dev server)

```sh
cargo build
dev/prosody/setup.sh                       # Prosody in Docker with alice/bob/agent
echo alice-pass | target/debug/agent-chat --profile alice account add alice@localhost \
    --host 127.0.0.1 --insecure-plaintext --store config --password-stdin
echo bob-pass | target/debug/agent-chat --profile bob account add bob@localhost \
    --host 127.0.0.1 --insecure-plaintext --store config --password-stdin
target/debug/agent-chat --profile alice wait --timeout 60 &
target/debug/agent-chat --profile bob send alice@localhost "hello"
```

For a real server: `agent-chat account add you@example.org`. This uses TLS and
stores the password in the OS keychain. `--insecure-plaintext` is refused for
anything other than localhost.

## Tests

```sh
cargo test                           # unit tests and two-device OMEMO sessions
dev/omemo-bot/interop-test.sh        # OMEMO interop with python-omemo (needs dev Prosody)
```

## License

GPL-3.0-or-later, because the bundled libomemo-c is GPL-3.0.
