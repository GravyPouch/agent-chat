## What and why

## How it was tested
- [ ] `cargo test`
- [ ] `cargo build` (and `-p agent-chat-gui` for GUI changes) with no warnings
- [ ] Interop / live checks, if protocol code changed (`dev/omemo-bot/interop-test.sh`, a stock client, …)

## Notes for reviewers
Security-sensitive changes (crypto, TLS, auth, registration, network parsing): describe the threat model.
