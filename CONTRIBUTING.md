# Contributing to agent-chat

Thanks for your interest! agent-chat is early, so issues describing what you tried, and what
happened, are as valuable as code.

## Getting started

```sh
cargo build                     # CLI, daemon and libraries
cargo build -p agent-chat-gui   # desktop app (needs the GPUI platform dependencies)
cargo test                      # all unit tests
```

The build compiles the vendored libomemo-c and generates its bindings, so you need a C
compiler and `libclang`:
- **macOS:** the Xcode Command Line Tools.
- **Debian/Ubuntu:** `build-essential clang libclang-dev`.

## Testing against real servers

```sh
dev/prosody/setup.sh                 # Prosody 13 in Docker: alice, bob, agent @localhost
dev/omemo-bot/interop-test.sh        # OMEMO interop with python-omemo (both protocol versions)
docker exec agent-chat-prosody lua5.4 /etc/prosody/custom-modules/mod_register_pow/test_vector.lua
```

When testing locally, point `AGENT_CHAT_HOME` at a scratch directory so your real profiles
aren't touched:

```sh
export AGENT_CHAT_HOME=$(mktemp -d)
```

## Guidelines

- **Keep changes focused.** One feature or fix per pull request, with tests.
- **Protocol work:** cite the XEP and section you implement. Prefer interop tests against
  independent implementations (Prosody, python-omemo, stock clients) over tests of our code
  against itself.
- **Security-sensitive code** (OMEMO, TLS, authentication, registration, anything that parses
  input from the network): explain the threat model in the PR, and add tests for rejection
  paths as well as the happy path.
- **Agent-facing output** (CLI JSON, exit codes, MCP tools) is an API. Don't break it
  without a migration note in `docs/AGENTS.md`.
- `cargo build` must stay warning-free.

## Licensing

By contributing, you agree that your contributions are licensed under the project's license,
GPL-3.0-or-later.
