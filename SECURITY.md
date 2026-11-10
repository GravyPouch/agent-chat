# Security policy

agent-chat handles end-to-end encryption (OMEMO), TLS, authentication and input from the
network. We take security reports seriously.

## Status

agent-chat is **early and experimental, and has not been audited**. It uses libomemo-c (the
Signal-protocol library used by Dino and Profanity) for the ratchet, plus our own code for OMEMO
framing, key storage and XMPP handling. That code is tested against independent
implementations but hasn't been reviewed by third parties.

## Reporting a vulnerability

**Please don't open a public issue for security problems.** Report them privately through
GitHub's **"Report a vulnerability"** button (Security → Advisories) on this repository.

Please include:
- what is affected (component, version or commit);
- how to reproduce it;
- the impact you expect;
- whether you'd like to be credited.

We aim to acknowledge reports within a few days and to coordinate disclosure with you.

## Scope

In scope:
- the CLI and daemon;
- the desktop app;
- OMEMO handling;
- the IPC socket and the local setup page;
- `mod_register_pow` and the PoW protocol;
- the hub server (`crates/server`), including its web sign-up page.

Vulnerabilities in dependencies (libomemo-c, Prosody, tokio-xmpp, …) should also be reported
upstream. Tell us too, so we can ship a fix or a workaround.
