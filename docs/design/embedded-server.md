# Embedded XMPP server ("agent-chat hub"): implementation plan

- **Status:** plan. Nothing here is implemented yet.
- **Date:** 2026-11-10
- **Decision already made by the user:**
  - We build a small XMPP server of our own that `agent-chat` can run itself: a local hub for people and AI agents on one machine or a LAN.
  - v1 has **no federation**.
  - Over time it grows into a **hosted service "for anyone"**.
  - Prosody stays the dev/test reference server and the option for operators who want it. This document does not revisit that choice.
- **Decisions on the open questions (2026-11-10):**
  - **Domain:** `localhost` by default, listening on loopback only.
    - For LAN use, the option is a `.local` name: `agent-chat hub init --domain NAME.local`, or `--lan`, which uses `<hostname>.local`. That's the machine's mDNS name, so Dino and Gajim can resolve it without DNS or SRV records.
    - Other domains are accepted for the hosted path later.
    - The domain is fixed at init. Accounts never move between domains, so `localhost` accounts stay local. The user accepted this trade-off; this replaces an earlier "real domain at setup" decision.
  - **Process:** a separate hub process (`agent-chat hub start`, the same binary) that serves every local profile, auto-started by the CLI when needed. This was recommended, and the user didn't object after an explanation.
  - **Stock clients that must work in v1:** Dino and Gajim, in addition to agent-chat. Conversations and Monal are best-effort.
  - **License:** GPL-3.0-or-later, the same as the rest of the project (not AGPL).
  - **Registration by mode (2026-11-10, the user decided):** proof of work is **only for public hosting**.
    - **`localhost`:** plain XEP-0077 registration with no PoW. Only processes on this machine can reach the hub.
    - **`.local` LAN:** no PoW either. Registration is by invite (XEP-0445) by default, because anyone on the network can reach the hub; open registration is opt-in.
    - **Public (`--public`):** registration requires the `urn:xmpp:ibr-pow:0` proof of work, or an invite.
    - `agent-chat account register` already falls back to plain registration when the form has no PoW fields, so the same command works in every mode.
  - **HTTP port and public hosting (2026-11-10).**
    - The hub serves a web page on an HTTP port: onboarding instructions for people and agents, MCP setup, and a "copy instructions for your agent" button. It serves the same content as the daemon's local setup page, but for joining this hub.
    - **Public hosting is an option:** `hub init --public --domain example.org`.
      - It listens on all interfaces and requires real TLS (certificate files now, ACME later).
      - It can turn on **public registration with proof of work** from the web page. The browser solves the same `urn:xmpp:ibr-pow:0` puzzle in Web Workers (or WASM built from the Rust solver), and the hub verifies it with the same code as XMPP in-band registration: same token, IP binding, escalation and replay cache.
      - Off by default. Invites and admin-created accounts keep working.
    - This is a new milestone after M1, ahead of the LAN and registration work in M6.
  - Everything else in §12 uses this document's proposed defaults until the user says otherwise.
- **Scope of this document:**
  - the v1 feature set and its non-goals;
  - the architecture;
  - security for a hub exposed on a LAN;
  - how the client finds and trusts a hub;
  - testing;
  - milestones;
  - the path to a hosted service.

**How claims are marked** (the same scheme as `pow-registration.md`)

- **[V]** I verified it in this session against a cited source: crate source code in the local Cargo registry, this repository, crates.io metadata, a spec, or a web page I opened. Sources are listed in §13.
- **[V: search]** It comes from a search-engine summary of a page I did not open myself. Treat it as weaker than [V].
- **[I]** It's my inference, estimate, design choice, or something I recall from a spec without re-reading it in this session.

---

## 1. Summary

**What we build**
- A server library, `crates/server` (package `agent-chat-server`), that the `agent-chat` binary runs as `agent-chat hub run|start|stop|status`. The hub runs as its own process, separate from the per-profile daemons, so one hub serves every local profile. There is also a standalone `agent-chat-hub` binary for Docker and server installs.
- The hub has one XMPP domain plus its group-chat subdomain (`conference.<domain>`). It stores everything in one SQLite file. By default it listens on **127.0.0.1:5222 and [::1]:5222 only**.

**v1 scope.** Everything our client uses today, plus what Gajim, Dino and Conversations need to work on a LAN:
- STARTTLS, SCRAM (including `-PLUS` with `tls-exporter`), resource binding;
- XEP-0198 stream management;
- RFC 6121 roster and presence, with offline storage;
- XEP-0280 carbons;
- XEP-0313 MAM with RSM paging, and XEP-0359 stanza-ids, for accounts and rooms;
- XEP-0045 MUC, including private (members-only, non-anonymous) rooms;
- PEP with publish-options and `+notify`, enough for both OMEMO versions;
- disco and caps;
- ping;
- XEP-0077 in-band registration with **native `urn:xmpp:ibr-pow:0`**, plus XEP-0445 invites.

**Explicit non-goals for v1:**
- server-to-server federation;
- push notifications (they need federation; see §3.6);
- HTTP upload;
- SASL2, Bind2 and FAST;
- clustering;
- Postgres.

**Building blocks** [V]
- tokio-xmpp 6 already has a race-free *responder* API for XML streams: `accept_stream`, then `send_header`, then `send_features`, plus `accept_reset` for the SASL reset.
- xmpp-parsers 0.23 has most of the stanza and nonza types we need.
- We write the rest:
  - the c2s negotiation state machine;
  - a SASL server (our own SCRAM; we don't use the `sasl` crate's server, see §3.2);
  - stanza size and rate limits (tokio-xmpp's reader has none);
  - routing;
  - every service.

**Architecture.**
- One task per connection.
- One actor per active account: it owns presence, roster, carbons, PEP and caps interest.
- One actor per active room.
- A storage thread.
- No shared mutable state except two registries.
- Everything addressed to an account passes through that account's actor, which keeps ordering simple. Actors are addressed by JID, so the design can later be distributed.

**Security.**
- Loopback-only by default.
- LAN mode requires TLS. The certificate is either self-signed and pinned by our client, or issued by a local CA, which stock clients need. ACME comes later.
- Credentials are stored as SCRAM StoredKey/ServerKey, never as passwords or salted passwords.
- Uniform authentication failures and fake salts prevent account enumeration.
- Per-IP and per-session limits.
- PoW and invites gate registration.

**A protocol finding that affects M1** [V]. Over TLS 1.3, tokio-xmpp 6 always offers the `tls-exporter` channel binding. Its SCRAM then asks for `SCRAM-SHA-256-PLUS`. If the server lists only non-PLUS SCRAM, **our client silently falls back to PLAIN**. So the hub must implement SCRAM-PLUS from M1.

**Milestones.** Seven milestones, about **15–17 weeks of calendar time** for one developer with an AI coding agent, or about 4 months with buffer (§9). Each milestone ends in a demo:
- M1: two agents chat over the hub.
- M3: messages survive restarts and network drops.
- M4: OMEMO interop with python-omemo against the hub.
- M5: encrypted private rooms.
- M6: a second machine joins over pinned TLS with an invite.
- M7: conformance runs and stock clients.

**Biggest risks** (§11):
- XMPP's long tail of interop details, especially MUC and PEP;
- running a self-written server on a LAN;
- the TLS trust experience for stock clients;
- no push without federation, which hurts mobile clients;
- the permanence of the domain chosen for v1 JIDs.

---

## 2. Context

What exists today [V]:
- The client (`crates/daemon`) talks to servers through tokio-xmpp's `StanzaStream`, with `StartTlsServerConnector` for TLS or `TcpServerConnector` for plaintext. Plaintext is only allowed to loopback hosts (`config.rs::is_loopback_host`).
- `dev/prosody/` runs Prosody 13 with:
  - `carbons`, `pep`, `mam`, `smacks`, `register` and `register_pow`;
  - MUC with `muc_mam`.
- `dev/omemo-bot/interop-test.sh` creates its accounts with `docker exec agent-chat-prosody prosodyctl register`. The bots join the Docker network `prosody_default` and connect to host `prosody`. The bot disables certificate verification (`bot.py:132–133`), so it will accept a hub's self-signed certificate over STARTTLS.
- `server/prosody/mod_register_pow/` is the reference PoW implementation:
  - `pow.lib.lua` holds the pure functions;
  - `test_vector.lua` checks the §5.7 vector from `pow-registration.md`.
- The client already has the solver and checker in `crates/daemon/src/register.rs` (`puzzle_prefix`, `leading_zero_bits`, `check_nonce`), with the same test vector.
- **There are no automated end-to-end tests yet.** `crates/daemon/Cargo.toml` declares an `integration` feature, but no code uses it. The scenarios in the original plan's "Verification" section were never written down as tests. This plan writes them so they run against both servers (§8.3).

---

## 3. Research findings

### 3.1 tokio-xmpp 6.0.0: what we can reuse on the server side

I read `~/.cargo/registry/src/index.crates.io-*/tokio-xmpp-6.0.0/src` [V]. The license is MPL-2.0.

| Piece | What it gives a server | Reuse? |
|---|---|---|
| `xmlstream::accept_stream(io, stream_ns, timeouts)` → `AcceptedStream` | Reads the initiator's stream header. `header()`/`take_header()` expose `from`, `to` and `id`. | **Yes.** It is the entry point for every c2s connection and every stream restart. |
| `AcceptedStream::send_header(StreamHeader)` → `PendingFeaturesSend` | Sends our header with `version='1.0'`. `StreamHeader` has only `from`, `to` and `id`, so there is no way to send `xml:lang` [V `common.rs`]. | **Yes.** Missing `xml:lang` is a minor deviation from RFC 6120 §4.7.4 [I]. |
| `PendingFeaturesSend::send_features::<T>(&StreamFeatures)` → `XmlStream<Io, T>` | Sends the features and fixes the item type `T` for the next stage. `send_error(&StreamError)` refuses a stream before the features, for example with `host-unknown`. | **Yes.** `T` can differ between stages: typed `XmppStreamElement` before authentication, `minidom::Element` after. |
| `XmlStream::accept_reset(&barrier)` | Sends the barrier element (SASL `<success/>`), resets the parser, and awaits the new header in one step. The module documentation explains that this avoids the reset race [V `xmlstream/mod.rs`]. | **Yes**, for the reset after SASL. |
| `XmlStream::into_inner()` | Returns the raw I/O object, for the STARTTLS upgrade. It panics if a read is in progress. | **Yes** (see the STARTTLS notes in §5.4). |
| `XmlStream` as `Stream` + `Sink` | `Stream<Item = Result<T, ReadError>>`, where `ReadError` is `SoftTimeout`, `HardError`, `ParseError` or `StreamFooterReceived`. `Timeouts { read_timeout, response_timeout }` drives keepalives. | **Yes.** A soft timeout means "send a whitespace ping"; a hard timeout means the peer is dead. |
| `XmppStreamElement` / `FallibleStreamElement` | One enum covering stanzas, SASL nonzas, STARTTLS nonzas, component handshake, stream errors and SM nonzas [V `xmlstream/xmpp.rs`]. If a stanza fails to parse, the error still carries its `from`, `to`, `type` and `id`, so we can bounce `bad-request` instead of killing the stream. | **Yes**, for the pre-authentication stages. |
| `xmpp_parsers::stream_features::StreamFeatures` | `starttls`, `bind`, `sasl_mechanisms`, `limits` (XEP-0478), `sasl2`, `sasl_cb` (XEP-0440), `stream_management`, `in_band_registration`, and `others: Vec<Element>` for anything else (`ibr-pow`, `ibr-token`, `csi`, `rosterver`, …) [V]. | **Yes.** |
| `examples/echo_server.rs` | TCP accept → `accept_stream` → `send_header(default)` → `send_features::<Element>(default)` → echo loop [V]. | It shows the responder path works and that `minidom::Element` works as a stream item type. |
| `stanzastream` | The client side only: `new_c2s`, reconnect, the client's SM negotiation. `mod stream_management` is private [V]. | **No** for the server. **Yes** as the test client in our harness. |
| `component` | The *initiator* side of XEP-0114 (`component_login` connects to a server) [V]. | **No** for accepting components. Useful later to test our own XEP-0114 listener. |
| `connect::ServerConnector` | A public trait that returns `(PendingFeaturesRecv<Stream>, ChannelBinding)` [V `connect/mod.rs`]. | **Client side:** we can write a pinned-certificate STARTTLS connector without forking tokio-xmpp (§7.3). |

**What we have to write ourselves:**
1. The c2s negotiation state machine:
   - which features to offer at each stage;
   - STARTTLS;
   - SASL;
   - bind and the legacy session IQ;
   - SM enable and resume;
   - pre-authentication registration.
2. A TLS acceptor (`tokio-rustls`) and the server-side channel-binding export. rustls has `ConnectionCommon::export_keying_material`, which works on server connections too [V `rustls-0.23.46/src/conn.rs:484`]. rustls has no `tls-unique` [V: no match in the source], so **-PLUS is offered only on TLS 1.3**.
3. SASL mechanisms (§3.2).
4. **Stanza size and rate limits.**
   - `RawXmlStream::new` always uses `rxml::Parser::default()` and gives us no hook to pass options [V `xmlstream/common.rs`].
   - rxml's only limit is `max_token_length` (default 8192 bytes). It limits names and attribute values; long text is split into chunks [V `rxml-0.14.0/src/parser/common.rs`].
   - Nothing bounds the size or depth of an element. A client can stream an unbounded stanza into memory.
   - So we wrap the I/O in a byte-budget reader (§5.4).
5. Helpers for stream errors and stanza-error bounces, `from` stamping, and every service.

**When tokio-xmpp's client falls back to PLAIN** [V]
- Over TLS 1.3, `connect/tls_common.rs` exports keying material with the label `EXPORTER-Channel-Binding` (32 bytes) and returns `ChannelBinding::TlsExporter`.
- In `sasl-0.5.2/src/client/mechanisms/scram.rs`, `Scram::name()` returns the `-PLUS` name whenever channel binding is set.
- `client/login.rs` tries SCRAM-SHA-256, then SCRAM-SHA-1, then PLAIN, then ANONYMOUS, and skips any mechanism whose `name()` is not in the server's list.
- Result: against a server that offers only `SCRAM-SHA-256`, the client sends PLAIN over TLS.
- Over plaintext loopback (`TcpServerConnector`), channel binding is `None`, so plain SCRAM is used.

### 3.2 SASL: why we write our own SCRAM server

`sasl` 0.5.2 (MPL-2.0) does ship server-side SCRAM (`src/server/mechanisms/scram.rs`). I read it [V] and it doesn't fit:

| Finding [V] | Why it matters |
|---|---|
| The provider returns `Pbkdf2Secret { salt, iterations, digest }`, where `digest` is the **SaltedPassword**. The mechanism derives ClientKey and ServerKey from it on every login. | We would have to store SaltedPassword, which is password-equivalent for SCRAM: whoever holds it can log in. RFC 5802 §3 has the server keep only "a salt, StoredKey, ServerKey, and the iteration count" [V]. |
| The client proof is compared with `!=`. | That comparison is not constant-time. |
| An unknown user makes the provider fail before the server-first message. | The failure is distinguishable, so accounts can be enumerated. |
| Channel binding: there is a `TODO: grab the data`. It accepts only `supports("tls-unique")`, and gs2 parsing is ad hoc. | There is no `tls-exporter` and no authzid handling. RFC 9266 says implementations supporting channel binding over TLS 1.3 "MUST implement tls-exporter" [V]. |

**Plan:**
- Write `c2s/sasl/scram.rs` on `hmac`, `sha1`, `sha2` and `pbkdf2`, which are already in `Cargo.lock` [V], plus `subtle` for constant-time comparison. Estimate: about 400 lines plus known-answer tests [I].
- Keep the `sasl` crate only as the client in tests, since tokio-xmpp uses it.
- Lonewolf has already done SCRAM with channel binding in Rust under Apache-2.0 (§3.4). Read it while implementing.

### 3.3 Crates and license compatibility

Versions and licenses come from the crates.io API [V].

| Crate | Version | License | Role |
|---|---|---|---|
| tokio-xmpp | 6.0.0 | MPL-2.0 | XML stream responder; test clients. |
| xmpp-parsers | 0.23.0 | MPL-2.0 | Stanza and payload types. **No typed `muc#owner` or `muc#admin` queries**: the `muc/` module has only `muc`, `user` and `mav` [V]. We handle those with `minidom` and `data_forms`. `pubsub/` has `pubsub`, `owner` and `event` [V]. |
| jid | 0.12.3 | MPL-2.0 | JIDs. Normalizes with stringprep (`nodeprep`, `nameprep`, `resourceprep`) [V `jid/src/lib.rs`], the same as the client. |
| rxml / minidom / xso | 0.14.0 / 0.19.0 / 0.4.0 | MIT / MPL-2.0 / MPL-2.0 | XML underneath. |
| tokio-rustls / rustls | 0.26.6 / 0.23.46 (locked) | MIT OR Apache-2.0 | TLS acceptor. |
| rcgen | 0.14.10 | MIT OR Apache-2.0 | Self-signed certificates and a local CA. |
| rusqlite | 0.41.1 (locked; `libsqlite3-sys` 0.39.1) | MIT | SQLite, the same crate the daemon uses. |
| sqlx-sqlite | 0.9.0 | MIT OR Apache-2.0 | **Not usable next to rusqlite 0.41.** It requires `libsqlite3-sys >=0.30.1, <0.38.0` [V crates.io]. Cargo allows only one package per `links = "sqlite3"` [I], so the two can't share a build. |
| deadpool-sqlite | 0.14.0 | MIT OR Apache-2.0 | Depends on `rusqlite ^0.40` [V], so it doesn't line up with our 0.41. Use our own small pool (§5.8). |
| tokio-postgres / sqlx-postgres | 0.7.18 / 0.9.0 | MIT OR Apache-2.0 | Later, for the Postgres backend. Use `sqlx` with the `postgres` feature only. |
| instant-acme / rustls-acme | 0.8.5 / 0.15.4 | Apache-2.0 / Apache-2.0 OR MIT | Later, for ACME. |
| prometheus-client | 0.25.2 | Apache-2.0 OR MIT | Metrics (should). |
| governor | 0.10.4 | MIT | Token-bucket rate limits. |
| mdns-sd | 0.21.7 | Apache-2.0 OR MIT | LAN advertisement (should). |
| stringprep | 0.1.5 | MIT/Apache-2.0 | SASLprep. |
| libfuzzer-sys / arbitrary | 0.4.13 / 1.5.0 | (MIT OR Apache-2.0) AND NCSA / MIT OR Apache-2.0 | Fuzzing. |

**Licenses**
- MIT, Apache-2.0 and MPL-2.0 code can all be used in a GPL-3.0-or-later program [I: these are the usual compatibility conclusions; I didn't re-read the license texts in this session].
- AGPL-3.0 code (xmpp-proxy) can be combined with GPL-3.0 code, but the combination then carries the AGPL network clause. We keep it out of our process.
- Add `cargo deny` with a license allowlist to CI.

### 3.4 Existing XMPP server code

| Project | Language and stack | License | Can we use it in GPL-3.0? | State | What we take |
|---|---|---|---|---|---|
| **Lonewolf** ([github.com/lonewolf-im/lonewolf](https://github.com/lonewolf-im/lonewolf)) | Rust. `compio` (not tokio), `quick-xml`, `redb`, `rustls`, `instant-acme`, `prometheus-client`, `rcgen` [V Cargo.toml]. | Apache-2.0 [V README] | Yes: Apache-2.0 code can go into GPLv3, in one direction only [I]. | Pre-1.0 [V]. Has c2s, roster, presence, offline storage, carbons, disco, vCard, ping, STARTTLS, "SCRAM login with channel binding", ACME, `lonewolfctl`, Prometheus. s2s is in progress. **No MUC, MAM or SM yet.** The README says most of its code was written with AI coding agents [V]. | **Reference reading**, not a dependency, because its runtime and XML stack differ from ours. Read its SCRAM and channel binding, ACME, ctl design and `docs/STANDARDS.md` [V]. Its STANDARDS.md is a good template for our conformance checklist. |
| **xmpp-proxy** ([moparisthebest/xmpp-proxy](https://github.com/moparisthebest/xmpp-proxy)) | Rust | AGPL-3.0-or-later [V crates.io; README] | Only as a separate process. | Terminates STARTTLS, Direct TLS, QUIC and WebSocket in front of a plaintext server. Limits stanza sizes "without an XML parser". Supports PROXY protocol v1 [V README]. | Optional front end at the hosted stage, and its stanza-size limiting technique. |
| **prose-pod-server** | Lua (a Prosody fork) | MIT [V: search] | n/a | Prosody-based. The Rust part is the separate admin REST API, prose-pod-api [V: search]. | Ideas for an admin API. |
| `xmpp-server` crate | Rust 2015 | MPL-2.0 | Yes | Released 2017, marked "not recommended", "Under construction" [V lib.rs] | Nothing. |
| Freyskeyd/xmpp-rs (server) | Rust | — | — | Last push 2021 [V: search] | Nothing. |
| Zerta, jackal (Go) | Go | not checked | — | Listed on xmpp.org [V] | Not investigated. |
| Prosody 13 | Lua | MIT | — | Our reference | Behaviour reference, the differential-testing target (§8.4), and `mod_register_pow` as the PoW reference. |

**Conclusion:** there is no mature Rust server to embed. We build our own on tokio-xmpp's responder API and xmpp-parsers, and borrow ideas from Lonewolf.

### 3.5 What our client needs from a server

I derived this "client contract" from the daemon source [V]. It is the acceptance bar for M1–M5.

| Area | What the client does (file) | So the hub must… |
|---|---|---|
| Connect and authenticate | `xmpp/session.rs`: `StanzaStream::new_c2s` over STARTTLS or plaintext to loopback. Mechanisms in preference order: SCRAM-SHA-256(-PLUS), SCRAM-SHA-1(-PLUS), PLAIN (§3.1). | Offer STARTTLS. Offer SCRAM-SHA-256-PLUS and SCRAM-SHA-1-PLUS on TLS 1.3, plain SCRAM everywhere, and PLAIN only over TLS or loopback. |
| Bind and SM | tokio-xmpp enables SM with resumption and reports `Reset`, `Suspended` and `Resumed` events. A fixed resource is configured (`AccountConfig.resource`). | Implement XEP-0198 `enable`, `resume`, `r` and `a`. On a resource conflict, replace the old session. |
| After login | `actions.rs::after_login`, in order: roster get with `ver: None`; initial presence with caps (`disco.rs`, node `urn:x-agent-chat:client`); carbons enable; OMEMO PEP publish for both versions; MAM catch-up; rejoin autojoin rooms. | Support all of those in that order, with no versioning required. |
| MAM catch-up | `sync_archive`: on the first sync, RSM `before=''` with `max = mam_backfill` (default 50), i.e. the **last page**. After that, `after=<last id>`, `max=100`, up to 50 pages, until `fin@complete`. Results are matched by `queryid` and must arrive **before** the `<fin/>` IQ result. | Support an empty `<before/>`, `after`, `max`, `complete`, `first` and `last`, with strict ordering of results before `fin`. |
| Deduplication | `inbound.rs`: XEP-0359 stanza-id with `by` = the archive JID, then origin-id, then (from, id). It ignores stanza-ids with a foreign `by` (test `EVIL`). | Stamp `stanza-id by=<account bare JID>` on delivery and `by=<room>` in rooms. **Strip** any client-supplied stanza-id that claims our `by`. Give offline-delivered and archived copies the same id. |
| Carbons | It unwraps `received` and `sent`. It **rejects carbons whose outer `from` isn't the own bare JID**. | Send carbons from the bare JID, per XEP-0280. |
| Roster | Accepts roster pushes from no `from` or from the own bare JID. Uses subscribe, subscribed and unsubscribed. | Implement the full RFC 6121 subscription state machine. |
| MUC join | It queries the room's disco#info first and reads `urn:xmpp:mam:2`, `muc_nonanonymous` and `muc_membersonly`. If the room has MAM, it joins with `<history maxstanzas='0'/>`. It recognizes itself by status 110, and treats 201 as "created". | Advertise those features. Implement locked rooms with status codes 110, 201 and 100/170. |
| MUC config | `configure_new_room`: either an instant room (empty submit form) or a private room (`membersonly=1`, `whois=anyone`, `persistentroom=1`, `publicroom=0`). | Implement the `muc#owner` form get and set. |
| MUC members | `refresh_room_info`: `muc#admin` get for owner, admin and member. It tolerates being refused. `invite_to_room`: sets affiliation `member`, then sends a mediated invite in a message of type `normal`. Invites are parsed both mediated and direct (XEP-0249). | Implement affiliation list get and set, and mediated invites. Route direct invites as ordinary messages. |
| OMEMO in rooms | It reads occupants' real JIDs from `muc#user` `item@jid` in presence. **It also reads the real JID from `<x xmlns='…muc#user'><item jid/></x>` inside archived room messages** (`omemo/xmpp.rs:597–603`). | In non-anonymous rooms, put real JIDs in occupant presence **and in room MAM results**. |
| PEP for OMEMO | `omemo/xmpp.rs`: publish with publish-options `pubsub#access_model=open`, plus `pubsub#max_items=max` for OMEMO:2 bundles. On `precondition-not-met` or `conflict` it republishes without options. It gets items by node, and OMEMO:2 bundles by item id. It treats `item-not-found` as "no devices". | Auto-create nodes. Match publish-options, reject mismatches with `precondition-not-met`, and accept `max_items=max`. Allow open access for non-contacts. Return `item-not-found` for missing nodes. |
| Notifications | The client advertises `urn:xmpp:omemo:2:devices+notify` and `eu.siacs.conversations.axolotl.devicelist+notify` in caps. | Resolve caps (disco#info to the client, verify the hash, cache it). Send PEP notifications and last items on presence, filtered by `+notify`. |
| IQs to clients | The client answers ping and disco#info itself. | Route IQs to full JIDs. Bounce `service-unavailable` for offline resources. |
| Registration | `register.rs`: an unauthenticated stream; `features.in_band_registration`; optional XEP-0445 `preauth`; jabber:iq:register get and set with the `{urn:xmpp:ibr-pow:0}` fields; maps the PoW app error conditions to exit codes. | Implement `ibr-pow:0` natively, with the same token format and errors as `mod_register_pow` (§5.7). |
| Messages | `<store xmlns='urn:xmpp:hints'/>`, origin-id, EME, OMEMO `<encrypted>` with a fallback body. Inbound chat states without a body are ignored. Uncommitted work in the tree (seen in `git diff` on 2026-11-10) also *sends* XEP-0085 chat states with `<no-store xmlns='urn:xmpp:hints'/>`. | Archive bodiless OMEMO messages that carry `<encrypted>` or a `store` hint. Route bodiless chat-state messages, but never archive them or store them offline. They are also what CSI holds back (§4.3 item 26). |

### 3.6 What mainstream clients need

| Client | Needs from the server | Notes for v1 |
|---|---|---|
| **Monal** (iOS, macOS) | **Required:** XEP-0357 push, XEP-0198 (session timeout ≥ 24 h recommended), XEP-0313. **Strongly recommended:** SASL2, FAST and Bind2; direct TLS (XEP-0368) preferred over STARTTLS; TLS 1.3; valid signed certificates; STUN/TURN. **Push needs bidirectional s2s with `eu.prod.push.monal-im.org`** [V Monal wiki]. | Without federation there is no push, so iOS gets messages only while the app is in the foreground. macOS Monal is less affected [I]. iOS needs a certificate the device trusts (install a local CA profile) [I]. |
| **Conversations** (Android) | XEP-0198, XEP-0357 through the project's app server ("Conversations proxy server") [V: search, README mirror]. Also CSI, MAM, carbons, PEP publish-options for OMEMO, HTTP upload for media, and blocking [I]. | Android keeps a long-lived connection, so it works without push if battery settings allow [I]. Has a dialog to accept unknown certificates (MemorizingTrustManager) [V: search, older version]. Supports `xmpp:` invite URIs [I]. |
| **Dino** (Linux desktop) | Carbons, MAM (including MUC MAM), OMEMO via PEP, MUC [I]. | **No documented way to accept a self-signed certificate.** User reports show failures [V: search]. Needs an OS-trusted certificate, i.e. a local CA or ACME. |
| **Gajim** (desktop) | Carbons, MAM, OMEMO (plugin), MUC, bookmarks via PEP (XEP-0402), vCard avatars [I]. | Prompts to accept an untrusted certificate. The certificate must match the XMPP domain [V: search]. |

**Takeaways:**
- The desktop clients (Gajim, Dino) and Conversations can work in v1.
- iOS Monal needs push, which needs federation. That is the strongest argument for adding, early on, a *limited* outbound s2s to push app servers (§10.7).
- For LAN TLS, a local CA is the only option that works across all four clients.

### 3.7 Compliance suites and test tools

**XEP-0479 "XMPP Compliance Suites 2023"**
- Experimental 0.1.0, dated 2023-05-04. xmpp.org still lists it as the current suite, and no newer edition is listed [V].
- Server requirements [V]:

| Suite | Core level | Advanced level adds |
|---|---|---|
| Core | RFC 6120, RFC 7590, XEP-0030, XEP-0114 | XEP-0368 (accept direct TLS), XEP-0115, XEP-0163 |
| IM | RFC 6121, XEP-0054, XEP-0280, XEP-0045 + XEP-0249, XEP-0363 | XEP-0398 + XEP-0153, XEP-0191, XEP-0198, XEP-0313 (including MUC archives), XEP-0048/0402/0049, XEP-0223, XEP-0410 |
| Mobile | XEP-0198, XEP-0352 | XEP-0357 |
| Web | RFC 7395 and/or XEP-0206 | (same) |
| A/V | XEP-0215 | (same) |

**XMPP Interop Testing** ([xmpp-interop-testing.github.io](https://xmpp-interop-testing.github.io/))
- Smack-based, client-to-server only, CI-oriented. It tests only MUST-level requirements [V].
- It covers RFC 6120, RFC 6121, XEP-0030, 0045, 0060, 0115, 0198, 0199, 0313 and more [V].
- The GitHub action is Apache-2.0 [V]. Its inputs are:
  - `host`, `domain`, `timeout`;
  - `adminAccountUsername` and `adminAccountPassword` (XEP-0133);
  - `accountOneUsername` … `accountThreePassword`;
  - `enabledSpecifications`, `disabledSpecifications`, `enabledTests`, `disabledTests`;
  - `failOnImpossibleTest`, `logDir`.
- It provisions accounts with XEP-0133, explicit accounts or IBR, in that order [V].
- **Smack can't solve PoW**, so we pass three pre-created accounts.

**caas / Compliance Tester** ([compliance.conversations.im](https://compliance.conversations.im/about/))
- It checks servers against the XEPs Conversations cares about. It is BSD-3 and has a command-line tool to check a server locally [V: search]. It runs with real account credentials [I].

**XEP statuses relevant to this plan** [V xmpp.org/extensions]

| Status | XEPs |
|---|---|
| Final | 0030, 0077, 0199 |
| Stable | 0045 (2026-05-03), 0060, 0115, 0163, 0191, 0198, 0215, 0249, 0280, 0297, 0313, 0352, 0363, 0368, 0386, 0388, 0398, 0402, 0410, 0421, 0440, 0485 |
| Active | 0054, 0114, 0133, 0153, 0157, 0223 |
| Experimental | 0359, 0369, 0401, 0441, 0474, 0478, 0479 |
| Proposed | 0377, 0445, 0484 |
| Deferred | 0357, 0390 |

### 3.8 Security references

- **RFC 5802 (SCRAM)** [V]:
  - The server stores the salt, StoredKey, ServerKey and iteration count (§3).
  - "In order to prevent information disclosure, the server may substitute the real reason with 'other-error'" (§7).
  - Servers SHOULD announce at least 4096 iterations (§5.1).
- **RFC 9266 (`tls-exporter`)** [V]:
  - Label `EXPORTER-Channel-Binding`, empty context, 32 bytes.
  - The default binding for SCRAM over TLS 1.3.
  - "MUST implement" for anyone doing channel binding over TLS 1.3.
- **Prosody's limits** [V via pow-registration.md §3.2]: unauthenticated stanzas are capped at 10,000 bytes, and unauthenticated connections time out after 5 minutes.
- **The PoW design** (`pow-registration.md` §4): SHA-256 PoW barely slows down GPUs. IP throttles and probation limits do the real work.

---

## 4. Scope of v1

### 4.1 Goals

1. `agent-chat hub start` gives a working, secure-by-default local server in one command. Agents get accounts on it with one more command.
2. Everything `agent-chat` does against Prosody today works the same against the hub. That includes OMEMO (both versions) in 1:1 chats and private rooms, MAM catch-up and stream resumption.
3. Opt-in LAN mode lets other machines join over TLS, either with our client (pinned TLS, invite) or with stock desktop and Android clients (local CA).
4. Agents can self-register with the PoW protocol natively, and humans can join with invites.
5. The design doesn't block multi-domain hosting, federation, Postgres or clustering later (§10.10).

### 4.2 Non-goals for v1

- **Federation**: no s2s, no dialback, no SRV lookups. Stanzas to remote domains bounce with `remote-server-not-found`.
- **Push notifications** (XEP-0357), because they need s2s (§3.6).
- **HTTP File Upload** (XEP-0363), STUN/TURN discovery (XEP-0215), Jingle relays.
- **BOSH and WebSocket** (XEP-0206, RFC 7395).
- **SASL2, Bind2, FAST** (XEP-0388, 0386, 0484), SCRAM downgrade protection (XEP-0474).
- **MIX** (XEP-0369), a generic pubsub service on the domain, ad-hoc commands (XEP-0050) and service administration (XEP-0133).
- **Components** (XEP-0114).
- **Clustering, Postgres, multiple domains**, a web admin UI.
- **Windows.** The admin socket is a Unix socket, like the daemon's.
- **Legacy auth** (XEP-0078) and pre-1.0 streams.
- Importing Prosody data (XEP-0227) comes later.

### 4.3 Features by tier

- **Must:** v1 doesn't ship without it.
- **Should:** in v1 if the schedule holds, otherwise v1.x.
- **Later:** after v1.

| # | Feature or spec | Tier | Milestone | Why |
|---|---|---|---|---|
| 1 | RFC 6120 streams, STARTTLS, stream errors and stanza errors | Must | M1 | Core |
| 2 | SASL SCRAM-SHA-256, SCRAM-SHA-1, their `-PLUS` variants with `tls-exporter`, PLAIN (TLS or loopback only) | Must | M1 | Our client and stock clients; §3.1 |
| 3 | XEP-0440 channel-binding advertisement | Must | M1 | Cheap, and clients use it to pick `-PLUS` |
| 4 | Resource binding; reply to the legacy `session` IQ | Must | M1 | Core; old clients |
| 5 | RFC 7622 addressing with the `jid` crate (stringprep) | Must | M1 | Same normalization as the client |
| 6 | 1:1 routing to full and bare JIDs, `from` stamping | Must | M1 | Core |
| 7 | XEP-0199 ping, XEP-0030 disco on the server | Must | M1 | Core |
| 8 | RFC 6121 roster, subscriptions, presence broadcast and probes, directed presence, priority | Must | M2 | Our client, all clients |
| 9 | Offline message storage (RFC 6121 §8.5.2) | Must | M2 | Clients without MAM; consistency with MAM |
| 10 | XEP-0280 carbons | Must | M2 | Our client, all clients |
| 11 | XEP-0115 caps processing (server-side disco, hash check, cache) | Must | M2 | Needed for PEP `+notify` |
| 12 | XEP-0198 SM: ack, resume, hibernation | Must | M3 | Our client; Conversations, Monal |
| 13 | XEP-0313 MAM with XEP-0059 RSM for accounts; XEP-0359 stanza-id; XEP-0203 delay; XEP-0297 forwarding | Must | M3 | Our client |
| 14 | XEP-0163 PEP over a subset of XEP-0060: publish, publish-options, auto-create, items, retract, owner configure and delete; `open` and `presence` access models | Must | M4 | OMEMO, avatars |
| 15 | XEP-0045 MUC: create (locked), instant and custom config, members-only, non-anonymous, persistent, affiliations, mediated invites, subject, history, PMs | Must | M5 | Our client, all clients |
| 16 | MUC MAM with real JIDs in non-anonymous rooms | Must | M5 | Our client (OMEMO in rooms) |
| 17 | XEP-0249 direct invites (just routed) | Must | M5 | Our client parses them |
| 18 | XEP-0077 IBR with native `urn:xmpp:ibr-pow:0`; password change; account removal | Must | M6 | Agent self-registration |
| 19 | XEP-0445 pre-authenticated IBR (invite tokens); `hub invite` | Must | M6 | LAN onboarding |
| 20 | Probation limits for self-registered accounts | Must for LAN mode | M6 | `pow-registration.md` §4.3 |
| 21 | TLS modes: self-signed + pin, local CA, operator-provided files | Must | M1 (self-signed), M6 (CA, files) | LAN |
| 22 | Stanza size, rate, connection and auth-failure limits | Must | M1, tuned in M7 | DoS |
| 23 | XEP-0191 blocking | Should | M7 | IM Advanced; Conversations |
| 24 | XEP-0054 vcard-temp; XEP-0153 hashes in presence | Should | M7 | Gajim and Dino; MUC avatars |
| 25 | XEP-0049 private XML; XEP-0402 bookmarks (PEP `whitelist` access model, `max_items=max`) | Should | M7 | Gajim, Dino and Conversations autojoin |
| 26 | XEP-0352 CSI (hold presence and chat states while inactive) | Should | M7 | Mobile battery |
| 27 | XEP-0368 direct TLS listener | Should | M7 | Monal prefers it |
| 28 | XEP-0410 MUC self-ping, XEP-0421 occupant-id | Should | M5/M7 | Stock clients |
| 29 | XEP-0478 stream limits advertisement | Should | M7 | Lets clients know our limits |
| 30 | XEP-0092 version, XEP-0157 contact addresses | Should | M7 | Cheap; abuse contact |
| 31 | Roster versioning (RFC 6121 §2.6) | Should | M7 | Mobile reconnects |
| 32 | mDNS advertisement on the LAN | Should | M6 | Discovery |
| 33 | Prometheus endpoint | Should | M7 | Operations |
| 34 | XEP-0357 push (needs outbound s2s) | Later | v1.x | Monal, Conversations |
| 35 | XEP-0363 HTTP upload | Later | v1.x | IM Core compliance; media |
| 36 | XEP-0388, 0386, 0484 (SASL2, Bind2, FAST); XEP-0474 | Later | v1.x | Monal; security |
| 37 | XEP-0133/0050 admin commands; XEP-0377 spam reporting | Later | hosted | Operations, abuse |
| 38 | S2S federation (RFC 6120, XEP-0220, XEP-0178, XEP-0288) | Later | hosted | §10.3 |
| 39 | WebSocket (RFC 7395); XEP-0215; XEP-0114 | Later | hosted | Web suite, A/V, gateways |
| 40 | XEP-0227 import/export | Later | v1.x | Portability to and from Prosody |

---

## 5. Architecture

### 5.1 Crates and how the CLI embeds the server

```
crates/ibr-pow/      NEW  lib agent_chat_ibr_pow
                     Pure urn:xmpp:ibr-pow:0 code: token format, HMAC signing, puzzle prefix,
                     leading-zero check, verifier, multi-threaded solver. Moved out of
                     crates/daemon/src/register.rs. Ships tests/vectors.json (the §5.7 vector),
                     which the Rust tests and server/prosody/.../test_vector.lua both read.
crates/server/       NEW  package agent-chat-server, lib agent_chat_server
  src/lib.rs           Hub builder, HubHandle (status, shutdown), run(config, cancel)
  src/config.rs        TOML schema, defaults, validation (§5.9)
  src/net/             listener.rs (accept loop, per-IP admission), budget.rs (byte budget and
                       rate limiting around the I/O), tls.rs (rustls acceptor, cert modes, SPKI pin)
  src/c2s/             negotiate.rs (stages A–C), sasl/{scram.rs, plain.rs}, session.rs (post-bind
                       loop), sm.rs (XEP-0198 queue and counters), errors.rs
  src/router.rs        address classification, registries (accounts, rooms)
  src/user/            actor.rs, roster.rs (pure subscription state machine), presence.rs,
                       carbons.rs, offline.rs, caps.rs, blocking.rs, csi.rs
  src/pep/             node config, publish-options matching, access checks, notifications
  src/mam/             archiving policy, query and RSM paging
  src/muc/             service.rs, room.rs (actor), config form, affiliations and roles, history
  src/register/        ibr.rs (XEP-0077 + ibr-pow + XEP-0445), invites.rs, probation.rs
  src/disco.rs, vcard.rs, private_xml.rs
  src/store/           mod.rs (Store trait), sqlite/ (implementation, migrations/NNNN_*.sql)
  src/admin.rs         admin socket: JSON lines, the same framing as the daemon IPC
  src/metrics.rs
  src/bin/agent-chat-hub.rs   standalone binary (Docker, systemd)
  fuzz/                cargo-fuzz targets (§8.6)
crates/cli/          `hub …` subcommands, behind the cargo feature `hub` (on by default)
crates/daemon/       pinned-TLS connector, `tls_pin` in AccountConfig, local-hub detection;
                     register.rs uses crates/ibr-pow
dev/hub/             Dockerfile, docker-compose.yml, hub.dev.toml, setup.sh (mirrors dev/prosody)
```

**Dependency rules**
- `agent-chat-server` doesn't depend on the daemon, `omemo` or `omemo-sys`. The server has no use for libomemo-c, and this keeps the server's licensing decision open (§12, Q6).
- The CLI depends on the server crate only through the `hub` feature, so a slim client build is still possible.
- The admin request and response types live in `agent-chat-server::admin`. The GUI can later reach them through `proto` if it needs to.

**Why a separate process, not inside the daemon:**
- Daemons are per profile. One hub serves many profiles (alice, bob, several agents).
- The two have different lifecycles: the hub should outlive any one profile's daemon.
- A crash in one doesn't take down the other.
- A LAN hub is often a headless service (launchd or systemd).

**Why the same binary:**
- One install gives agents both client and server.
- `agent-chat hub start` uses the same detached-spawn mechanism as `daemon start` (`proto/src/client.rs::DaemonLauncher` [V]).

### 5.2 Process model, files and commands

Files live under `<base>/hub/`. `<base>` is `$AGENT_CHAT_HOME` or the platform data directory, as for profiles (`proto/src/paths.rs` [V]). The directory is mode 0700.

| File | Purpose |
|---|---|
| `hub.toml` | Configuration (§5.9). Written by `hub init`. |
| `hub.db` (+ `-wal`, `-shm`) | All persistent state. Mode 0600. |
| `tls/key.pem`, `tls/cert.pem`, `tls/ca.pem`, `tls/ca-key.pem` | Key and certificate; CA files only in local-CA mode. Mode 0600. |
| `hub.sock` | Admin socket. Mode 0600. |
| `hub.lock` | One hub per base directory (`File::try_lock`, as `daemon.rs` does [V]). |
| `hub.json` | Runtime info written at start: pid, version, domain, listeners, SPKI pin, start time. Clients read it to auto-configure (§7.1). |
| `hub.log` | Log file when detached (§5.10). |

**Commands.** Each prints one JSON line, like the rest of the CLI.

| Command | What it does |
|---|---|
| `agent-chat hub init [--domain D] [--lan ADDR:PORT] [--tls self-signed\|local-ca\|files]` | Writes `hub.toml`, creates `hub.db`, generates TLS material. Refuses if already initialized. |
| `agent-chat hub run` | Foreground. Logs go to stderr. Used by `start`, Docker and launchd. |
| `agent-chat hub start` / `stop` / `status` | Detached start (it runs `init` with defaults the first time), graceful stop, and a status snapshot (sessions, accounts, rooms, listeners, uptime, version). |
| `agent-chat hub user add NAME [--password-stdin] [--admin]` / `del` / `passwd` / `list` | Account management over the admin socket, with no PoW. With no password given, it generates one and prints it once. |
| `agent-chat hub invite [--user NAME] [--uses N] [--expires 7d]` | Creates an XEP-0445 token. Prints our join URI and a standard `xmpp:` invite URI (§7.2). |
| `agent-chat hub cert show\|rotate [--keep-key]\|export-ca` | Shows the TLS pin; rotates; exports the CA for installation on devices. |
| `agent-chat hub backup PATH` | Online SQLite backup (§10.8). |
| `agent-chat hub logs [-f]` | Tails `hub.log`. |

### 5.3 Task and actor model

```
                    ┌──────────────── agent-chat hub run ─────────────────────────────────┐
 TCP 5222 ──► Listener ──spawn──► Connection task (one per socket)                       │
                    │              stages A–C: features, STARTTLS, SASL, bind / resume    │
                    │              then the session loop: socket ⇄ outbox mpsc           │
                    │                    │ stanzas (Element)            ▲ deliveries     │
                    │                    ▼                              │                │
                    │   Router: classify the `to` address ──► Registry (JID → mailbox)   │
                    │          │                     │                      │           │
                    │          ▼                     ▼                      ▼           │
                    │   UserActor(alice)      UserActor(bob)        RoomActor(team@conf)│
                    │   presence, roster,     …                     occupants, config, │
                    │   carbons, PEP, caps,                         affiliations       │
                    │   offline, SM hibernation                     (MucService: create/│
                    │          │                                     directory)        │
                    │          ▼                                                        │
                    │   Store: one writer thread (batched transactions) + reader pool  │
                    │   Admin socket server · timers (expiry, SM timeouts) · metrics    │
                    └─────────────────────────────────────────────────────────────────────┘
```

**Actors and tasks**

| Unit | Count | Owns | Lifetime |
|---|---|---|---|
| Supervisor | 1 | Config, listeners, shutdown token (`tokio_util::sync::CancellationToken`), registries | Process |
| Listener | 1 per address | Accept loop, per-IP admission counters | Process |
| Connection task | 1 per TCP connection | Socket, negotiation state, SM counters, the outbound queue for this resource | Socket; under SM, the hibernation period |
| UserActor | 1 per account with sessions or pending work | Available resources (priority, last presence, caps ver, carbons flag, CSI state); directed-presence set; roster cache; PEP node cache; pending server-originated IQs (caps disco) | Started on demand. Stops after 5 minutes idle with no sessions [I]. |
| RoomActor | 1 per room with occupants or pending work | Occupants (nick → full JID, role), config, affiliations cache, subject, history cursor | Loaded from the database on demand. Unloaded when empty. Non-persistent rooms are destroyed when empty. |
| MucService | 1 | Room creation (quotas), disco#items of the component | Process |
| Store writer | 1 OS thread | The SQLite write connection | Process |
| Store readers | N connections, used through `spawn_blocking` | Read-only connections (WAL) | Process |
| Admin server | 1 | `hub.sock` | Process |

**Rules**
1. **Exclusive ownership.** Actors own their state and talk only through bounded `tokio::sync::mpsc` mailboxes. The only shared maps are the account and room registries (`JID → mailbox sender`), with get-or-spawn under one short lock. No lock is held across an `.await`.
2. **No synchronous cycles.**
   - An actor never awaits a reply from another actor inside its message loop.
   - Request/response, for example a probe answer or a server-originated disco IQ, is asynchronous: a later message correlated by id, with a timeout.
   - Actor-to-actor sends use `try_send`. When a mailbox is full:
     - for a connection outbox, the session is congested. If it can't drain within 30 s, it is disconnected. With SM, the stanzas stay queued for resumption [I].
     - for a UserActor or RoomActor mailbox, the sender gets `wait`/`resource-constraint`.
3. **Ordering.** Every stanza addressed to a local account passes through that account's UserActor, whether it's addressed to the bare or a full JID. Every stanza to a room or occupant passes through its RoomActor. Each hop is FIFO, so stanzas from one session to one entity stay in order. That is how I read RFC 6120 §10.1's in-order processing requirement [I].
4. **Address is the key.** Actors are found by JID, never by holding a direct handle across operations. Clustering can then replace the registry with a cluster-aware one (§10.4).
5. **Durability.**
   - A message to an offline account is committed to the archive and the offline table **before** it is acknowledged to the sender's SM counter.
   - Messages to online recipients are delivered immediately and archived in a ≤10 ms batch window [I].
   - A crash in that window can lose the archive copy of a message that was delivered. That trade-off is acceptable for v1. Making it configurable is an open question (§12).
6. **Shutdown.**
   - Cancel the listeners.
   - Send `<stream:error><system-shutdown/>` to every session.
   - Flush the store writer.
   - Remove `hub.json` and the socket.
   - SM sessions can't be resumed across a restart in v1.

**Mailbox sizes** (defaults, configurable) [I]
- Connection outbox: 512 stanzas.
- UserActor: 1024.
- RoomActor: 2048.
- Store writer: 4096 commands.

### 5.4 Connection pipeline (c2s)

1. **Accept.**
   - Per-IP admission: concurrent connections, and a token bucket on new connections.
   - Set `TCP_NODELAY`.
   - Wrap the socket in `Budgeted<BufStream<TcpStream>>`. The byte-budget reader:
     - counts bytes consumed since the last complete top-level element. The session resets the counter each time it yields an element. Past the limit, the read fails with `InvalidData` and the session sends `<policy-violation/>` and closes;
     - applies a token bucket to inbound bytes. Over the bucket, reads are **delayed** rather than failed, which throttles a sustained flood without disconnecting [I: the same idea as Prosody's rate limiting].
2. **Stream header.**
   - `accept_stream(io, "jabber:client", timeouts)`.
   - If `to` isn't the hosted domain, send `host-unknown` with `PendingFeaturesSend::send_error`.
   - Otherwise send a header with `from = domain` and `id` = 128 random bits.
3. **Stage A: before TLS.**
   - Features: `<starttls>`, with `<required/>` unless the peer is on loopback.
   - On loopback, if `allow_plaintext_loopback` is set (the default), also offer SCRAM mechanisms (never PLAIN) and `register`.
   - Any other stanza gets a `not-authorized` stream error.
4. **STARTTLS.**
   - Send `<proceed/>` and flush, then `into_inner()`.
   - **Check that the `BufStream` read buffer is empty.** If the client pipelined bytes after `<starttls/>`, close the connection: that is the STARTTLS command-injection class [I].
   - Run the TLS handshake with a 10 s timeout. ALPN is not required on STARTTLS.
   - Export `tls-exporter` on TLS 1.3.
   - Wrap a new `Budgeted<BufStream<TlsStream>>` and go back to step 2.
5. **Stage B: TLS, not yet authenticated.** Features:
   - mechanisms, in this order: `SCRAM-SHA-256-PLUS` and `SCRAM-SHA-1-PLUS` (TLS 1.3 only), `SCRAM-SHA-256`, `SCRAM-SHA-1`, `PLAIN`;
   - `<sasl-channel-binding><channel-binding type='tls-exporter'/>` (XEP-0440);
   - `<register xmlns='http://jabber.org/features/iq-register'/>` when registration is allowed on this channel;
   - `<register xmlns='urn:xmpp:ibr-pow:0'/>` (hint) and `<register xmlns='urn:xmpp:ibr-token:0'/>`;
   - `<limits>` (XEP-0478, should).

   In this stage the stream accepts only:
   - SASL nonzas;
   - IQs in `jabber:iq:register` and `urn:xmpp:pars:0`, sent to the domain or with no `to`.

   Everything else gets `not-authorized`.
6. **SASL.**
   - `<auth mechanism>` drives the mechanism's state machine through `<challenge>`/`<response>`. `<abort/>` cancels.
   - Failures send `not-authorized`, `invalid-mechanism`, `encryption-required`, `malformed-request` or `temporary-auth-failure` (during backoff). After 3 failures on one connection, it closes.
   - On success, `accept_reset(&Success { additional data = v=… })` (§3.1), then back to step 2.
7. **Stage C: authenticated.**
   - Features: `<bind/>`, `<sm xmlns='urn:xmpp:sm:3'/>`, `<session><optional/></session>`, `<csi/>` (should), `<ver xmlns='urn:xmpp:features:rosterver'/>` (should).
   - Switch the stream item type to `minidom::Element`.
8. **Bind or resume.**
   - **Bind:** apply resourceprep to the requested resource, or generate one. If the resource is already in use, the old session gets `<conflict/>` and is replaced; our client keeps a fixed resource. Cap resources per account (default 10).
   - Register the session with the account's UserActor.
   - **Resume:** `<resume previd h>` hands the socket to the hibernating session (§5.4.1).
9. **Session loop.** `select!` over:
   - inbound elements;
   - the outbox;
   - timers: SM `<r/>` every 5 stanzas or 30 s [I], and a whitespace keepalive on soft timeout;
   - shutdown.

   On every inbound stanza:
   - **stamp `from`** with the full JID. If a client-supplied `from` doesn't match, send an `invalid-from` stream error;
   - check `to` syntax (`jid-malformed`);
   - strip any `stanza-id` whose `by` is ours;
   - apply the per-session stanza rate limit;
   - hand the stanza to the router.

**Stanzas are routed as `minidom::Element`.** Typed xmpp-parsers structs are used only inside the handlers that understand a payload.
- This keeps unknown or unusual payloads, which are often OMEMO or new XEPs, flowing untouched.
- It keeps a strict typed parser from rejecting valid stanzas it doesn't model.
- A handler parse error becomes `bad-request` to the sender, with the original id. It is never a panic and never a stream error.

**Timeouts and limits** (defaults; LAN mode, with the local-mode value where it differs) [I]

| Item | Default |
|---|---|
| Total time from accept to authenticated | 60 s (the PoW client reconnects; tokens are stateless [V pow doc §7.3]) |
| TLS handshake | 10 s |
| Stanza budget before authentication | 10,000 bytes (Prosody's value [V via pow doc]) |
| Stanza budget after authentication | 256 KiB. An OMEMO bundle with 100 prekeys is on the order of 10 KB [I]. |
| Inbound byte rate per session | 20 KB/s sustained, 200 KB burst (loopback: unlimited) |
| Soft read timeout → whitespace ping; hard timeout | 120 s; +60 s |
| SASL failures | 3 per connection. Per account and per IP, exponential backoff after 5 failures in 10 minutes. |

#### 5.4.1 Stream management (XEP-0198)

- `<enable resume='true'/>` gets back `<enabled id=… resume='true' max=…/>`.
  - The id is 128 random bits and also encodes a node id, for later clustering.
  - `max` defaults to 600 s, configurable up to 86,400 s. Monal recommends 24 h or more [V].
- Inbound counter `h`. Outbound queue of unacked stanzas, capped at 1000 by default. When the queue is full, the session is closed and the unacked stanzas are handled as below.
- On disconnect with resumption enabled, the session **hibernates**. It stays registered as available (presence unchanged), and deliveries are queued.
- On resume, the server replays everything after the client's `h`. Otherwise, at timeout:
  - the server broadcasts unavailable presence;
  - unacked messages of type `chat` or `normal` with a body or `<encrypted>` are put in the offline queue, **unless** they were carbons or another resource of the account already received them. The archive has them in any case [I].
- `<r/>`/`<a/>` handling. An `h` higher than what was sent gets `<handled-count-too-high>` (xmpp-parsers has the type [V `sm.rs`]).

### 5.5 Routing

**Address classes:**
- a local account (bare or full);
- the domain itself;
- the MUC component (`conference.<domain>`), a room, or an occupant;
- any other domain, which is remote and bounced in v1.

**Rules** [I: summarized from RFC 6121 §8.5, to be checked against the RFC in M2]

| Stanza | To a local bare JID | To a full JID, resource online | To a full JID, resource offline |
|---|---|---|---|
| message `chat`/`normal` | Deliver to the available resources with the highest non-negative priority (all of them on a tie). Carbon copies go to other carbons-enabled resources. If none is available and the message is archivable: store offline (§5.7 MAM). | Deliver. Copy as `received` carbons to the other carbons-enabled resources. | Treat as addressed to the bare JID. |
| message `groupchat` | `service-unavailable` | Deliver (it comes from a RoomActor) | Drop silently |
| message `headline` | All non-negative available resources, otherwise drop | Deliver | Drop |
| message `error` | Deliver as for chat, never stored offline, never bounced | Deliver | Drop |
| presence (available/unavailable, directed) | Handled by the UserActor (subscription rules, §5.6) | Deliver, and record the directed presence | Drop |
| presence subscription types | UserActor state machine | Treat as bare | Treat as bare |
| iq get/set | The server answers for the account: roster, PEP (pubsub to the bare JID), MAM, vCard, disco#info/items on the bare JID, blocking. Otherwise `service-unavailable`. | Deliver | `service-unavailable` |
| iq result/error | Matched against server-originated requests, otherwise dropped | Deliver | Drop |

- **To the domain:** disco, ping, version, register, server-side handlers.
- **To a remote domain:** `remote-server-not-found` (`cancel`) in v1.
- **Never** bounce an error in reply to an error.

**What the server stamps:**
- `stanza-id` (`by` = the archiving entity) on every archived message it delivers;
- `<delay from=domain stamp>` on offline delivery;
- carbons wrapped as `<received|sent><forwarded>…`, sent **from the bare JID** to the full JID;
- in MUC, `stanza-id by=room` and the room as `from`.

**Carbons** (XEP-0280)
- Eligible messages: type `chat`; type `normal` with a body; MUC private messages (those carrying `<x xmlns='…muc#user'/>`) [I].
- Not eligible: messages with `<private xmlns='urn:xmpp:carbons:2'/>` or `<no-copy xmlns='urn:xmpp:hints'/>`.
- Sent carbons go to the sender's other carbons-enabled resources.

### 5.6 Presence and roster

**Subscription state machine**
- `user/roster.rs` is a pure function `(state, event) → (new_state, side_effects)`.
- The states are RFC 6121 Appendix A's: None, None+PendingOut, None+PendingIn, None+PendingOut+PendingIn, To, To+PendingIn, From, From+PendingOut, Both.
- The events are the inbound and outbound subscribe, subscribed, unsubscribe and unsubscribed, plus roster remove.
- The Appendix A tables become table-driven unit tests [I].
- Since both parties are local in v1, a subscription is two UserActor messages. The **same code paths** will later serve remote contacts through s2s.

**Roster operations**
- `get`, returning the whole roster (versioning is should, item 31).
- `set`: add or update; `subscription='remove'` cancels both directions.
- A roster push goes to every *interested* resource, meaning one that has fetched the roster.
- Pending inbound subscription requests are stored with their original stanza and redelivered on the next initial presence.

**Initial presence** (the first available presence from a resource) [I: RFC 6121 §4.2]:
1. Broadcast to contacts with subscription `from` or `both`, and to the account's other available resources.
2. Probe contacts with `to` or `both`. For local contacts this is a `Probe` message to their UserActor, which replies with its available presences, or the last unavailable one.
3. Deliver pending subscription requests.
4. Deliver offline messages (with delay and stanza-id) if the priority is ≥ 0. Delete them from the offline queue once delivered (with SM, once acked).
5. Once the resource's caps are known, send the last PEP items of contacts whose nodes it `+notify`s (§5.7).

**Other presence rules:**
- Later available presences are broadcast to the same audience.
- Unavailable presence also goes to everyone who got directed presence.
- Directed presence is tracked per resource.
- A negative priority means the resource receives no bare-JID messages.

**Caps** (XEP-0115)
1. On a presence with `<c node ver hash>`, look up `(hash, ver)` in the `caps` table.
2. If it's unknown, the UserActor sends `disco#info` to that full JID (`node#ver`), verifies the hash with `xmpp_parsers::caps::{compute_disco, hash_caps}` (the client uses the same functions [V `disco.rs`]), and stores the result.
3. A mismatched hash is never cached; the server then uses that resource's own disco reply without caching it.
4. From the caps, the server derives the resource's `+notify` set.

### 5.7 Services

#### PEP (XEP-0163 over a subset of XEP-0060)

Nodes are keyed by (account, node).

- **Publish**
  - Auto-create the node. If it doesn't exist, it is created with the publish-options. If it exists and an option differs from its configuration, the publish fails with `precondition-not-met` [I: XEP-0060 §7.1.5]. Our client then retries without options [V].
  - Supported options: `pubsub#access_model` (`open`, `presence`; `whitelist` is should), `pubsub#max_items` (an integer or `max`), `pubsub#persist_items`, `pubsub#send_last_published_item`, `pubsub#notify_retract`. An unsupported option gets `precondition-not-met`.
  - Default access model: `presence`.
  - The item id is generated if absent. An existing id is replaced.
  - After each publish, trim the node to `max_items`.
- **Items:** all items (honouring `max_items`), or by item ids.
  - Access: the owner always.
  - `presence` access: contacts with subscription `from` or `both`.
  - `open` access: anyone.
  - A missing node or item gets `item-not-found`.
- **Retract** (notify if configured), **delete node**, **owner configure** get and set (`pubsub#owner`), **purge** (should).
- **Notifications** (`<message type='headline' from=owner-bare><event xmlns='…pubsub#event'>`) go to [I: XEP-0163 §4]:
  - the owner's available resources whose caps include `node+notify`;
  - contacts with subscription `from` or `both` whose available resources include `node+notify`.
- **Last item on presence and caps:** when a contact's resource becomes available, or its caps are learned, send it the last item of each node it `+notify`s, if it may access the node.
- **disco#info on a bare account JID:**
  - identities `account/registered` and `pubsub/pep`;
  - pubsub features, including `#publish`, `#publish-options`, `#auto-create`, `#access-open`, `#access-presence`, `#retrieve-items`, `#item-ids`, `#persistent-items`, `#last-published`, `#config-node`, `#delete-nodes`, `#retract-items`, `#multi-items`.
  - **disco#items** lists the nodes the requester may access.
- **Limits:** 64 KiB per item payload, 100 nodes per account, `max` = 1000 items [I].

#### MAM (XEP-0313 v2, `urn:xmpp:mam:2`)

**What is archived** (the user archive):
- type `chat` or `normal` messages that have a body, `<encrypted>` (either OMEMO namespace) or `<store/>`;
- **excluding** `<no-store/>`/`<no-permanent-store/>`, error and headline messages, and chat states without a body;
- messages a local user sends are archived in the sender's archive (outgoing) and in the recipient's archive (incoming), with **separate ids per archive**;
- `groupchat` messages are archived only in the room's archive.

**Archive ids**
- Ids are UUIDv7 strings, time-ordered and unguessable. The `uuid` crate is already a workspace dependency; add its `v7` feature.
- The id doubles as the XEP-0359 `stanza-id`.
- Offline delivery points at the same archive row, so the client's deduplication keys line up [V client contract].

**Queries**
- Form fields: `with`, `start`, `end`. Should: the `#extended` fields `before-id`, `after-id` and `ids`.
- RSM: `max` capped at 250 [I: the dev Prosody's `max_archive_query_results` is 250 [V `dev/prosody/prosody.cfg.lua`]]; `after`; `before`, where an empty `before` means the last page; `flip-page`.
- Results are sent as `<message><result queryid id><forwarded><delay/>original</forwarded></result></message>`, strictly before the `<iq type='result'><fin complete first last/></iq>`.
- `index` gets `feature-not-implemented`.
- `count` is included when cheap (indexed `COUNT`).

**Room archives**
- Same table, with `archive_jid = room`.
- In non-anonymous rooms the archived stanza keeps `<x xmlns='…muc#user'><item jid='real@bare'/></x>`, because our client needs it [V].
- Room queries are allowed to members, or to anyone if the room is public.

**Preferences and retention**
- Default policy `always`. `prefs` (XEP-0313 §6) get and set is should.
- Retention is `archive.expire_after` (§5.9). A daily task deletes expired rows in batches.

#### MUC (XEP-0045) on `conference.<domain>`

- **Service**
  - disco#info: identity `conference/text`, feature `http://jabber.org/protocol/muc`.
  - disco#items: public rooms.
  - Room creation: any local account (configurable to admins only), rate-limited. Accounts on probation can't create rooms.
- **Creation**
  - The first join creates a **locked** room. The creator gets status 110 + 201 and is owner. Everyone else gets `item-not-found` until the room is configured [I: XEP-0045 §10.1].
  - An empty `<x type='submit'/>` makes an instant room. Owner form get and set implements the `muc#roomconfig` fields:
    - `roomname`, `roomdesc`;
    - `persistentroom`, `publicroom`, `membersonly`, `moderatedroom`;
    - `whois` (`moderators` or `anyone`);
    - `passwordprotectedroom` + `roomsecret`;
    - `allowinvites`, `changesubject`, `maxusers`;
    - `getmemberlist` (who may fetch the member list);
    - `enablelogging`.
  - Should: room destroy.
- **Join**, with these errors:
  - nick conflict → `conflict`;
  - members-only and not a member → `registration-required`;
  - wrong password → `not-authorized`;
  - outcast → `forbidden`;
  - room full → `service-unavailable`.

  The joiner gets:
  - existing occupants' presence first, then their own with status 110. Status 100 if the room is non-anonymous, 170 if logging is on;
  - for whois=anyone, or for moderators, occupant presence includes `item@jid`;
  - history from the room archive, honouring `maxstanzas`, `maxchars`, `seconds` and `since` (default 20 stanzas), then the subject.
- **Messages**
  - `groupchat` from occupants with voice is reflected to every occupant, the sender included, with the original `origin-id` kept and `stanza-id by=room` added.
  - Subject changes follow `changesubject`.
  - Private messages (`chat` to `room/nick`) go to that occupant's full JID with `<x xmlns='…muc#user'/>`.
- **Affiliations and roles** (`muc#admin`)
  - Get and set for owner, admin, member and outcast lists.
  - Removing a member from a members-only room removes the occupant.
  - Role changes (kick, voice) are should.
  - The permission matrix follows XEP-0045 §5 and becomes a table-driven test [I].
- **Invites**
  - A mediated invite goes to the invitee as `<message from=room><x muc#user><invite from=real-inviter><reason/></invite><password/></x></message>`.
  - In members-only rooms, the invitee is **added as a member** when the inviter is an owner or admin, or when `allowinvites` is on.
  - Decline is relayed.
  - Direct invites (XEP-0249) are ordinary messages.
- **Persistence**
  - Rooms, config, affiliations and subject are stored in the database. Occupants are in memory only; after a hub restart clients rejoin, and XEP-0410 helps them notice.
- **Should:**
  - XEP-0410: the room answers IQ ping to the occupant's own JID.
  - XEP-0421: add `occupant-id`.

#### Registration and accounts

- **XEP-0077 on the unauthenticated stream.**
  - The GET reply carries:
    - `<instructions>`;
    - a `jabber:iq:register` data form (username, password);
    - the `{urn:xmpp:ibr-pow:0}` fields when the policy needs PoW;
    - no legacy `<username/>` or `<password/>` elements, the same as `mod_register_pow` [V].
  - SET runs the verification steps in `pow-registration.md` §5.6, steps 2–12, with `crates/ibr-pow`. The hub owns the form, so the Prosody coupling problems in that document's §6.2 don't arise.
  - The token format, HMAC binding (domain, IP prefix), puzzle, error conditions, per-prefix and global escalation, and the spent-token cache all match the design doc. Tokens are marked spent on success.
- **Policies** (`registration.mode`):
  - `closed` (admin CLI only);
  - `invite` (XEP-0445 token required);
  - `pow` (PoW, or an invite instead);
  - `open` (no PoW; dev only).
  - Loopback clients are exempt from PoW by default (`pow_exempt_loopback = true`; open question Q3).
- **XEP-0445:** `<preauth xmlns='urn:xmpp:pars:0' token/>` on the unauthenticated stream validates the token against `invites` (hash, expiry, uses). It can reserve a username. It skips PoW.
- **Authenticated stream:**
  - a password change (`jabber:iq:register` with username and password) recomputes the SCRAM keys;
  - `<remove/>` deletes the account's data and writes a tombstone.
- **Reserved names:** `admin`, `abuse`, `postmaster`, `root`, `hub`, `support`, `security`, `webmaster`.
- **Probation** (`pow-registration.md` §4.3, §6.6), for accounts created by PoW in LAN mode:
  - at most 5 messages a minute and 50 new recipients a day to JIDs outside the roster;
  - at most 20 outgoing subscription requests a day;
  - no room creation.
  - A daily task promotes accounts after 7 days with at least one login, and deletes never-used accounts.

#### Other services

| Service | Behaviour |
|---|---|
| disco on the domain | Identity `server/im`. Features built from the enabled modules, e.g. `urn:xmpp:ping`, `jabber:iq:register`, `urn:xmpp:carbons:2`, `urn:xmpp:blocking`, `msgoffline`, `urn:xmpp:sid:0`, `vcard-temp`, and XEP-0157 contact info (should). disco#items lists `conference.<domain>`. |
| Should tier | vcard-temp (get and set for your own account, get for others); private XML storage; blocking (XEP-0191: stanzas from blocked JIDs are dropped and IQs answered with `service-unavailable`; on block, unavailable presence is sent to them); CSI (while inactive, hold presence updates and bodiless messages, and flush on active or on an important message); `jabber:iq:version`. |

### 5.8 Storage

**`Store` trait**
- Async methods grouped by area: accounts, roster, archive and offline, PEP, MUC, caps, invites and registration, misc.
- Handlers use only the trait. A shared test suite runs against every implementation, which is SQLite in v1 and Postgres later.

**SQLite implementation**
- `rusqlite` 0.41, the same as the daemon. This is forced by the `links = "sqlite3"` constraint (§3.3).
- Pragmas: WAL, `synchronous=NORMAL`, `foreign_keys=ON`, `busy_timeout=5000`.
- **One writer thread** takes commands from an mpsc channel and batches them into one transaction per ≤10 ms or per 256 commands [I].
- **N reader connections** (default 4) are used through `spawn_blocking`.
- Migrations live in `store/sqlite/migrations/NNNN_name.sql` and are applied in order, tracked with `PRAGMA user_version`. An applied migration is never edited.

**Conventions that keep Postgres easy:**
- timestamps are `INTEGER` milliseconds;
- JIDs are normalized `TEXT`;
- stanzas are `TEXT` (serialized XML);
- JSON is stored as `TEXT`;
- upserts use `ON CONFLICT … DO UPDATE`, which both engines support;
- nothing relies on `rowid`;
- explicit ids everywhere.

**Schema**

| Table | Columns (key in **bold**) | Indexes and notes |
|---|---|---|
| `meta` | **key**, value | Schema info; secrets: `pow_secret` (persisted so tokens survive restarts), `scram_mock_key`, `sm_token_key` |
| `hosts` | **domain**, created_ms, config_json | One row in v1. Multi-domain becomes a data change, not a schema change. |
| `accounts` | **id**, domain, localpart, role (`admin`/`user`/`probation`), state (`active`/`locked`), created_ms, created_via (`admin`/`invite`/`pow`/`open`), last_login_ms, scram_salt, scram_iterations, sha1_stored_key, sha1_server_key, sha256_stored_key, sha256_server_key, password_changed_ms | UNIQUE(domain, localpart) |
| `tombstones` | **domain, localpart**, deleted_ms | Prevents reuse of deleted names |
| `roster_items` | **account_id, contact**, name, subscription (`none`/`to`/`from`/`both`), ask_out, pending_in, pending_in_stanza, approved, groups_json, updated_ms | |
| `roster_versions` | **account_id**, version | Should (versioning) |
| `archive` | **id** (UUIDv7), archive_jid, ts_ms, with_jid, direction (`in`/`out`), origin_id, from_jid, stanza, flags | (archive_jid, ts_ms, id); (archive_jid, with_jid, ts_ms, id). For rooms, `with_jid` is the sender's real bare JID. |
| `offline` | **account_id, archive_id**, added_ms | FK to `archive`. Capped at 1000 per account (oldest dropped and logged). |
| `pep_nodes` | **account_id, node**, config_json, created_ms | |
| `pep_items` | **account_id, node, item_id**, seq, publisher, payload, published_ms | (account_id, node, seq) |
| `pep_affiliations` | **account_id, node, jid**, affiliation | Should (`whitelist` access model) |
| `caps` | **algo, ver**, features_json, disco_xml, verified_ms | Global cache |
| `muc_rooms` | **jid**, config_json, subject, subject_from, subject_ms, created_ms, last_active_ms, locked | |
| `muc_affiliations` | **room_jid, jid**, affiliation, reserved_nick, reason | |
| `vcards` | **account_id**, xml, hash | Should |
| `private_xml` | **account_id, ns**, xml | Should |
| `blocklist` | **account_id, jid** | Should |
| `invites` | **token_hash**, domain, created_by, created_ms, expires_ms, max_uses, uses, role, reserved_username, note | Only a SHA-256 of the token is stored |
| `registrations` | **account_id**, ts_ms, method, ip_prefix, pow_bits | Audit trail. IP prefixes kept for 30 days (`pow-registration.md` §6.7). |
| `pow_events` | **prefix, ts_ms** | Per-prefix escalation; pruned past the window |
| `pow_spent` | **tag**, exp_ms | Replay cache; pruned at expiry |

**Backups.** `hub backup PATH` uses SQLite's online backup, through rusqlite's `backup` feature or `VACUUM INTO` [I], so it doesn't block the server.

### 5.9 Configuration

`<base>/hub/hub.toml` holds the defaults. Every key is optional except `domain`. The path can be overridden with `AGENT_CHAT_HUB_CONFIG`.

```toml
domain = "localhost"                 # permanent: stored in `hosts`; the hub refuses to start if it changes

[listen]
c2s = ["127.0.0.1:5222", "[::1]:5222"]
c2s_direct_tls = []                  # should (XEP-0368), e.g. ["0.0.0.0:5223"]
allow_plaintext_loopback = true      # SCRAM without TLS, from loopback peers only

[tls]
mode = "self-signed"                 # "self-signed" | "local-ca" | "files" | (later) "acme"
cert = "tls/cert.pem"
key  = "tls/key.pem"
extra_names = []                     # extra SANs, e.g. ["hub.lan", "192.168.1.20"]

[auth]
mechanisms = ["SCRAM-SHA-256-PLUS", "SCRAM-SHA-1-PLUS", "SCRAM-SHA-256", "SCRAM-SHA-1", "PLAIN"]
scram_iterations = 10000
max_failures_per_10min = 5           # per account and per IP, then exponential backoff

[registration]
mode = "pow"                         # "closed" | "invite" | "pow" | "open"
pow_exempt_loopback = true
pow_bits = 25
pow_count = 8
pow_max_bits = 31
pow_ttl = "1h"
pow_bind_ip = true
probation = true                     # applies to PoW-created accounts on non-loopback connections

[limits]
max_stanza_bytes = 262144
unauth_stanza_bytes = 10000
session_rate = "20KB/s"
session_burst = "200KB"
max_connections_per_ip = 20          # loopback is exempt
max_new_connections_per_ip_per_min = 30
max_resources_per_account = 10
negotiation_timeout = "60s"

[sm]
resume_timeout = "10m"
max_queue = 1000

[archive]
enabled = true
expire_after = "1y"                  # or "never"; open question Q7
max_page = 250

[muc]
enabled = true
host = "conference"                  # → conference.<domain>
room_creation = "local-users"        # or "admins"
default_history = 20

[log]
level = "info"
format = "text"                      # or "json"
file = "hub.log"                     # used when detached

[metrics]
prometheus = ""                      # should: e.g. "127.0.0.1:9655"; empty disables it
```

**Validation, enforced at startup:**
- A listener on a non-loopback address requires TLS material, and makes STARTTLS required for non-loopback peers.
- `allow_plaintext_loopback` applies only to peers whose address is loopback.
- `registration.mode = "open"` on a non-loopback listener needs `--i-know-this-is-open`.
- `domain` must pass nameprep and match the `hosts` row.
- On SIGHUP, the hub reloads log level, limits, registration policy and the certificate. Listeners and the domain need a restart.

### 5.10 Logging and metrics

**Logging (`tracing`)**
- Under `hub run`, logs go to stderr. Detached, they go to `hub.log`, rotated daily with `tracing-appender` and 7 files kept.
- Each connection gets a span `conn{id, peer_prefix, jid}`, where `jid` is set after bind.
- Events at `info`:
  - listener up;
  - connection accepted or refused (with reason);
  - auth success or failure (mechanism; never passwords or SASL payloads);
  - bind;
  - resume;
  - disconnect (reason);
  - registration (method, bits, IP prefix);
  - limits hit;
  - room create and destroy.
- **Privacy:**
  - message bodies and stanza payloads never appear at `info` or `debug`;
  - IPs are logged as prefixes (/24 for IPv4, /56 for IPv6) at `info`;
  - full XML tracing exists only at `trace`, for one account, with `--trace-xml JID`. It redacts `<body>`, `<password>`, SASL data and `{urn:xmpp:ibr-pow:0}solution`.

**Metrics.** `hub status` returns a JSON snapshot from M1. The Prometheus endpoint (should, M7) is loopback-only by default.
- Counters:
  - `connections_total{result}`;
  - `auth_total{mechanism,result}`;
  - `stanzas_in_total{kind}`, `stanzas_out_total{kind}`;
  - `routed_total{target_class}`;
  - `offline_stored_total`;
  - `archive_writes_total`;
  - `sm_resumes_total{result}`;
  - `pow_challenges_total`, `pow_verify_total{result}`;
  - `registrations_total{method}`;
  - `limit_hits_total{limit}`.
- Gauges: `sessions`, `hibernating_sessions`, `user_actors`, `room_actors`, `occupants`, `mailbox_depth_max{actor}`, `store_queue_depth`.
- Histograms: `stanza_route_seconds`, `store_txn_seconds`, `mam_query_seconds`.

---

## 6. Security design for a LAN-exposed hub

### 6.1 Exposure modes

| Mode | Listeners | TLS | Who can register | Typical use |
|---|---|---|---|---|
| **Local** (default) | 127.0.0.1 and ::1 | Offered (self-signed, pinned by our client). Not required for loopback. | Admin CLI. IBR from loopback without PoW. | One machine: people plus agents |
| **LAN** (`hub init --lan 0.0.0.0:5222`) | Chosen address(es) | **Required** for non-loopback peers | Invites (default), plus PoW IBR if enabled | A household, office or lab |
| **Hosted** (later) | Public | Required; ACME | PoW, invites, probation, abuse controls | §10 |

### 6.2 TLS certificates

| Mode | How | Good for | Notes |
|---|---|---|---|
| **Self-signed + pin** (default) | rcgen creates an ECDSA P-256 key and a certificate with SANs `domain`, `conference.domain` and `extra_names`. `hub.json` and invites carry the pin, `sha256/` + base64 of the SPKI SHA-256. | Our client (pinned connector, §7.3) | `hub cert rotate --keep-key` keeps the pin valid. Plain rotation changes it, so clients need a new invite. |
| **Local CA** | rcgen creates a CA and issues the leaf. `hub cert export-ca` writes `ca.pem`, plus a `.mobileconfig` later. **Name-constrain the CA** to the hub's names, so installing it can't be abused to impersonate other sites [I: verify rcgen's name-constraint support in M6]. | Stock clients: Dino, Monal, Gajim, Conversations (§3.6) | The user installs the CA once per device. The CA key is kept 0600, and could be kept offline later. |
| **Operator files** | `tls.mode = "files"`. PEM files from any PKI, including ACME run outside the hub. Reloaded on SIGHUP. | People with their own domain | |
| **ACME** (later) | instant-acme. DNS-01 for LAN hubs with public names; TLS-ALPN-01 or HTTP-01 when hosted. | Hosted; LAN with a real domain | §10.8 |

**TLS settings**
- rustls defaults: TLS 1.2 and 1.3, AEAD suites only.
- `-PLUS` is offered only on TLS 1.3.
- On direct TLS (should), ALPN is `xmpp-client`.
- SNI is ignored in v1, since there is one certificate. Multi-domain later uses rustls `ResolvesServerCert`.

### 6.3 Credentials

- **Storage.** Per account:
  - a random 16-byte salt and the iteration count (default 10,000; RFC 5802 recommends at least 4096 [V]);
  - **StoredKey and ServerKey** for both SHA-1 and SHA-256.
  - Nothing password-equivalent is stored.
  - Passwords are SASLprep'd (RFC 4013, via `stringprep::saslprep`) before key derivation.
  - Buffers holding passwords are zeroized.
- **Mechanisms.**
  - SCRAM verification is constant-time (`subtle`).
  - Channel binding:
    - `-PLUS` requires `p=tls-exporter` with the exported value;
    - a client that sends gs2 `y` when the server offered `-PLUS` is refused, which detects downgrades [I: RFC 5802 §6];
    - `n` is fine.
  - PLAIN is offered only over TLS or loopback. It is verified by deriving the StoredKey from the supplied password and comparing.
- **No enumeration.**
  - For an unknown user, the server-first message carries a **fake salt** = HMAC-SHA-256(`scram_mock_key`, domain ‖ 0x00 ‖ username)[..16] with the normal iteration count.
  - The final failure is the same `not-authorized` as a wrong password (RFC 5802 §7 allows `other-error` [V]).
  - IBR `conflict` still reveals whether a username exists; that is inherent to XEP-0077.
- **Throttling.**
  - Failures are counted per account and per IP prefix. After 5 failures in 10 minutes, attempts get `temporary-auth-failure` with exponential backoff (cap 15 minutes).
  - Loopback is not exempt from per-account throttling.
- **Admin-created accounts** get a generated 24-character password unless one is given (minimum 12 characters).
- **Later:** XEP-0474 downgrade protection, FAST tokens, and Argon2-based storage for PLAIN-only clients if any appear.

### 6.4 Limits

| Limit | Local mode | LAN mode | Why |
|---|---|---|---|
| Concurrent connections per IP | unlimited for loopback | 20 | Socket exhaustion |
| New connections per IP per minute | — | 30 | Floods |
| Unauthenticated stanza size | 10,000 B | 10,000 B | Pre-auth memory |
| Authenticated stanza size | 256 KiB | 256 KiB | Memory; XEP-0478 advertises it |
| Session inbound rate | unlimited for loopback | 20 KB/s, burst 200 KB | Flooding the router |
| Negotiation deadline | 60 s | 60 s | Slowloris |
| SASL attempts per connection | 3 | 3 | Brute force |
| Resources per account | 10 | 10 | Resource churn |
| Roster size | 2000 | 2000 | Storage |
| PEP | 100 nodes, 64 KiB per item, 1000 items | same | Storage |
| Offline queue per account | 1000 | 1000 | Storage |
| Rooms created per account per day | 20 | 20 (0 on probation) | Spam |
| MUC occupants per room | 200 | 200 | Fan-out |
| Registrations per IP prefix | n/a | 5/h plus PoW escalation | `pow-registration.md` §4.3 |

### 6.5 Admin access and the local attack surface

- The admin socket is the **only** admin interface in v1. It is mode 0600 inside a 0700 directory, so only the same OS user can create accounts. There is no network admin in v1.
- `hub.db` and the TLS keys are 0600. Secrets in `meta` never leave the machine.
- The hub runs as an ordinary user. Port 5222 needs no root.
- Under launchd or systemd, sandboxing such as `ProtectSystem` comes later.

### 6.6 Threats and mitigations

| Threat | Mitigation |
|---|---|
| A rogue device on the LAN intercepts traffic | TLS is required off-loopback. The pin or local CA is distributed out of band (invite). No TOFU by default. |
| A STARTTLS stripping or injection attack | STARTTLS is required on LAN listeners. The pre-TLS buffer must be empty. Our client refuses plaintext to non-loopback hosts (`config.rs` [V]). |
| Credential stuffing or brute force | Per-account and per-IP backoff; 3 attempts per connection; uniform failures. |
| Memory or CPU exhaustion | Byte budgets, rate buckets, bounded mailboxes, connection caps, negotiation deadline. |
| XML attacks (billion laughs, DTDs) | rxml implements XMPP's restricted XML, with no DTD processing [I: from rxml's design; add fuzz cases]. Token length is capped at 8192 [V]. |
| Spoofed `from`, stanza-ids or carbons | The server stamps `from`. It strips stanza-ids that claim our `by`. Carbons come only from the bare JID (the client checks this too [V]). MAM results go only to the archive owner. |
| Registration spam | PoW with escalation, IP-prefix throttle, invites, probation (§5.7). |
| Room abuse | Room-creation quotas; members-only private rooms; outcast lists. |
| Data at rest | File permissions; the OS disk encryption (FileVault); OMEMO keeps message content opaque to the server. |
| Logs leaking content | §5.10 privacy rules. |

---

## 7. How the client discovers and uses a hub

### 7.1 On the same machine

```sh
agent-chat hub start                              # first run: init with domain "localhost", then start
agent-chat --profile alice account add --hub alice   # creates alice@localhost on the hub and configures the profile
agent-chat --profile bob   account add --hub bob
agent-chat --profile alice wait --timeout 60 &
agent-chat --profile bob send alice@localhost "hello"
```

**`account add --hub NAME`** (new):
1. Reads `<base>/hub/hub.json`, starting the hub first if it isn't running.
2. Creates the account through the admin socket, with a generated password and no PoW.
3. Stores the password as usual (Keychain or config).
4. Writes `AccountConfig { jid, host: "127.0.0.1", port, tls_pin }`.
5. Starts the daemon.

**Auto-detection.** `account add alice@localhost` with no `--host`: if `hub.json` exists and its domain equals the JID's domain, host, port and pin come from it. Otherwise the client uses DNS SRV as today.

**Transport on loopback**
- The client uses STARTTLS with the pin from `hub.json`. That gives SCRAM-PLUS for free.
- `--insecure-plaintext` keeps working, under the existing loopback-only rule [V README, `config.rs`].

**Optional auto-start.** A profile setting `start_local_hub = true` makes the daemon start the hub when it's down, the same way the CLI auto-starts the daemon (should).

### 7.2 On a LAN

On the hub machine:

```sh
agent-chat hub init --domain hub.lan --lan 0.0.0.0:5222 --tls self-signed
agent-chat hub start
agent-chat hub invite --user bob --expires 7d
# → {"join_uri":"agent-chat+hub://192.168.1.20:5222/join?domain=hub.lan&pin=sha256/…&preauth=…&user=bob",
#    "xmpp_uri":"xmpp:hub.lan?register;preauth=…", "pin":"sha256/…"}
```

On the other machine (our client):

```sh
agent-chat --profile bob account join 'agent-chat+hub://192.168.1.20:5222/join?…'
```

`account join` does the following:
1. Parses the URI.
2. Connects over STARTTLS **with the pin**.
3. Sends the XEP-0445 `preauth`.
4. Registers through XEP-0077. The invite skips PoW, so this is the existing `register.rs` flow.
5. Writes the profile with `host`, `port` and `tls_pin`.
6. Starts the daemon.

The URI format is ours [I]. The `xmpp:` part follows the XEP-0401 style already quoted in `pow-registration.md` §3.1 [V].

**Agents without an invite.** If the hub allows PoW registration on the LAN:
- `agent-chat account register --server hub.lan --host 192.168.1.20 --tls-pin sha256/…` (new flag) solves the challenge as today.
- The pin still has to come from somewhere trusted, such as the operator or a config management system.

**mDNS (should)**
- The hub advertises `_xmpp-client._tcp.local.` with TXT records `domain=hub.lan` and `pin=sha256/…`.
- `agent-chat hub discover` lists the hubs it finds.
- An mDNS pin is spoofable. `account join --discover` therefore needs either an invite or an explicit `--trust-on-first-use`, with the fingerprint printed for comparison.

**With a real domain.**
- Point `hub.example.com` at the LAN IP and use operator files, or ACME DNS-01 later.
- Stock clients then work with no CA install, and our client needs no pin.

### 7.3 Client changes (crates/daemon, crates/cli)

| Change | Where | Notes |
|---|---|---|
| `AccountConfig.tls_pin: Option<String>` | `config.rs` | Validated format `sha256/<base64>` |
| `PinnedStartTlsConnector` | new `xmpp/connector.rs` | Implements tokio-xmpp's public `ServerConnector` [V]. It follows `StartTlsServerConnector`'s sequence (initiate, recv features, starttls, into_inner, TLS, initiate again) but uses a rustls `ServerCertVerifier` that checks the SPKI hash, still verifies the handshake signature, and **returns `ChannelBinding::TlsExporter`** so SCRAM-PLUS works. |
| Choosing a connector | `xmpp/session.rs::start` | Pin present → pinned connector. Otherwise the existing rules. |
| `register.rs` | | Uses `crates/ibr-pow`. Accepts the pin. Supports the invite path from a join URI. |
| `account add --hub`, `account join URI`, `account register --tls-pin` | `crates/cli` | Same JSON output conventions and exit codes |
| `hub …` command group | `crates/cli` | §5.2 |
| `docs/AGENTS.md` | | A short "Run your own hub" section |

### 7.4 Stock clients on a LAN hub

| Client | Trust path | v1 expectations |
|---|---|---|
| Gajim | Accept the certificate prompt, or install the local CA [V: search] | Full IM, MUC, OMEMO, MAM |
| Dino | Install the local CA in the OS trust store (no prompt [V: search]) | Full IM, MUC, OMEMO, MAM |
| Conversations | Trust prompt or local CA. Invite link through the `xmpp:` URI [I]. | Works while the app keeps its connection. No push. |
| Monal (iOS) | Install the CA as a configuration profile and enable full trust [I] | Foreground only, because there is no push in v1 |

---

## 8. Testing strategy

### 8.1 Unit tests (`cargo test -p agent-chat-server`)

- **SCRAM known-answer tests:**
  - the SCRAM-SHA-1 example in RFC 5802 §5;
  - the SCRAM-SHA-256 example in RFC 7677 §3 [I: use the RFCs' own vectors];
  - `-PLUS` with a fixed exporter value;
  - fake-salt determinism;
  - constant-time comparison is exercised.
- **PoW:**
  - `crates/ibr-pow/tests/vectors.json` (the `pow-registration.md` §5.7 vector) is verified by the server verifier, the client solver tests and `test_vector.lua`;
  - token parsing;
  - escalation arithmetic.
- **Negotiation:** stage transitions; which features each stage offers; refusals (plaintext PLAIN, a stanza before auth, `host-unknown`).
- **Roster:** subscription state machine tables from RFC 6121 Appendix A, as data-driven cases.
- **Routing:** a pure `route(kind, type, target_state)` decision function, tested against the §5.5 table.
- **MUC:** the permission matrix (affiliation × role × action), config form parsing, history selection.
- **PEP:** publish-options matching, access decisions, `max_items` trimming, `+notify` audience computation.
- **MAM:** RSM paging edge cases (empty `before`, `after` the last id, `max=0`, `flip-page`), the archiving-policy filter.
- **Caps:** the XEP-0115 example hashes, plus the client's own caps from `disco.rs`.
- **Config and limits:** validation rules; the budget reader (exactly at the limit, one byte over); the rate bucket.
- **Store:** the contract test suite, run against SQLite in v1 and Postgres later.

### 8.2 In-process protocol tests

- `tests/harness.rs` starts a hub on `127.0.0.1:0` with a temporary directory and returns its address, domain and the pin.
- Test clients are tokio-xmpp's `StanzaStream`, or the raw `xmlstream` initiator for negotiation tests.
- A small "send X, expect stanza matching Y" DSL ignores ids and timestamps.
- Scenarios: one per acceptance test in §9, for example:
  - SCRAM-PLUS over TLS 1.3;
  - PLAIN refused before TLS;
  - a resource conflict replaces the old session;
  - offline then delivery with the same stanza-id as MAM;
  - MAM last page;
  - SM resume replays unacked stanzas;
  - publish-options mismatch;
  - a locked room gives `item-not-found` to non-owners;
  - an invite to a members-only room grants membership.

### 8.3 Our client's flows against the hub

1. **The `integration` feature.** Use the daemon's unused `integration` feature for a test suite that drives the real `agent-chat` binary through scenarios. The server is chosen by `AGENT_CHAT_TEST_SERVER=hub|prosody`:
   - `hub`: start the hub in-process in a temporary `AGENT_CHAT_HOME`.
   - `prosody`: use `dev/prosody`.

   **Scenarios.** The first four come from the original plan's "Verification" section:
   1. bob → alice: `wait` returns the message as JSON.
   2. A message sent while alice's daemon is stopped is recovered through MAM after restart.
   3. A message from alice's second resource arrives as a carbon, marked `own`, and isn't returned by `wait`.
   4. bob's message in `test@conference.<domain>` is returned by `wait --room`.
   5. OMEMO:2 and legacy 1:1 between two agent-chat profiles, with `--require-encryption`.
   6. A private room with `--private` + `rooms invite`: encrypted message delivered to the invitee.
   7. `account register` with PoW (low bits) and with an invite.
   8. SM: the daemon reconnects through a fault-injecting TCP proxy (`dev/e2e/flaky-proxy`, a ~100-line Rust test helper) and reports `Resumed`.
2. **The interop script** (`dev/omemo-bot/interop-test.sh`).
   - Factor the Prosody-specific lines into `dev/lib/server.sh`, with `server_register NAME PASS`, `server_network` and `server_host`. Select with `XMPP_SERVER=prosody|hub`.
   - For the hub, `dev/hub/` runs the standalone `agent-chat-hub` in a container named `agent-chat-hub` on the network `hub_default`. Accounts are created with `docker exec agent-chat-hub agent-chat-hub user add NAME --password-stdin`.
   - The bot uses STARTTLS and already ignores certificate errors (`bot.py:132–133` [V]), so the hub needs no plaintext exception.
   - The agent-chat side connects to `127.0.0.1:5222` (port published on loopback) with the pin from the container's `hub.json`, or with plaintext on loopback.
   - **Acceptance:** `XMPP_SERVER=hub dev/omemo-bot/interop-test.sh` prints the same four `PASS` lines it prints against Prosody.
3. **The README quick start** gets a hub variant, which is the M1 demo.

### 8.4 Differential tests against Prosody

The harness can record a **normalized transcript** of the stanzas a scenario produces:
- ids, timestamps and stanza-ids are replaced by placeholders;
- feature lists are sorted;
- server-specific identities are masked.

The same scenario runs against the hub and against `dev/prosody`, and the transcripts are diffed. A small allowlist covers intended differences. This catches subtle MUC and PEP behaviour (status codes, presence order, error conditions) that unit tests miss. It runs nightly, not on every PR.

### 8.5 Manual checks with stock clients

Run before closing M5, M6 and M7. Record the results in a compatibility table, a new doc written then.

| Check | Gajim | Dino | Conversations | Monal (macOS/iOS) |
|---|---|---|---|---|
| Log in over TLS (local CA; Gajim and Conversations also through the prompt) | | | | |
| Add a contact, approve the subscription, see presence | | | | |
| 1:1 chat with an `agent-chat` agent, both directions | | | | |
| OMEMO 1:1 with agent-chat (both versions where the client supports them) | | | | |
| Two devices of one account: carbons and history | | | | |
| History after being offline (MAM) | | | | |
| Create a public room; join; subject; history | | | | |
| Private OMEMO room with agent-chat members | | | | |
| Mediated invite into a members-only room | | | | |
| Avatar (XEP-0084 through PEP) | | | | |
| Bookmarks and autojoin (XEP-0402 or 0049) | | | | |
| Network switch: SM resume | | | | |
| Registration through an invite link | | | | |

### 8.6 Fuzzing

`crates/server/fuzz/` uses cargo-fuzz (nightly toolchain, CI only).

| Target | Input | Asserts |
|---|---|---|
| `stream_bytes` | Arbitrary bytes, fed through a `tokio::io::duplex` into a full connection pipeline, with a fixed test account | No panic. Memory stays bounded by the budget. The connection always ends. |
| `stanza_dispatch` | Arbitrary `minidom::Element` trees (generated with `arbitrary`) into the router and handlers, with an in-memory store | No panic. Every IQ get or set gets exactly one reply. |
| `sasl_scram` | Arbitrary client-first and client-final messages | No panic. Never succeeds without the right proof. |
| `pow_token`, `data_form`, `rsm`, `jid` | Arbitrary strings | No panic. Parse and serialize round-trips. |

- **Seeds:** transcripts captured from §8.2 and §8.3, plus valid stanzas exported from Prosody test runs.
- **CI:** 10 minutes per target nightly. Crashes become regression unit tests.
- **Property tests** (`proptest`):
  - the roster state machine (subscription states reachable only through valid transitions);
  - RSM paging (paging forward and backward visits every archived item exactly once).

### 8.7 Conformance

- **XMPP Interop Testing** (§3.7)
  - Runs in CI from M2 against the hub in Docker.
  - Pass three pre-created accounts.
  - `enabledSpecifications` grows with the milestones: RFC6120, RFC6121, XEP-0030, XEP-0199 (M2); XEP-0115, XEP-0198, XEP-0313 (M3); XEP-0060/XEP-0163 (M4); XEP-0045 (M5).
  - `failOnImpossibleTest` stays off until M7.
- **caas command-line tool** against a LAN-mode hub with a real certificate (M7).
- **Conformance checklist.** A table maintained from M2 in a new doc written then, with rows of:
  - specification and section;
  - normative requirement (MUST, SHOULD);
  - test id (unit, harness, interop, manual);
  - status;
  - notes.

  Lonewolf's `docs/STANDARDS.md` is a good model [V].
- **v1 target:**
  - XEP-0479 Core: everything except XEP-0114 (components) and XEP-0368 (should);
  - XEP-0479 IM Core: everything except XEP-0363 (upload), with XEP-0054 as should;
  - the gaps documented.

---

## 9. Milestones

The estimates are calendar time for one developer working with an AI coding agent [I]. They assume:
- the agent writes most handler code and tests;
- the developer designs the state machines, reviews security-relevant code and does interop debugging, which dominates the later milestones.

Lonewolf, also written largely with AI agents, reached c2s, roster, carbons and SCRAM-PLUS after hundreds of commits. MUC, MAM and SM are still open there [V]. That supports keeping a buffer.

| Milestone | Content | Estimate | Demo |
|---|---|---|---|
| **M1** | Streams, TLS, auth, bind, 1:1 routing, admin CLI | 2–2.5 weeks | Two agent profiles chat through `agent-chat hub` |
| **M2** | Roster, presence, offline storage, carbons, caps, disco | 2 weeks | Contacts and presence in the GUI; two devices with carbons |
| **M3** | XEP-0198 and MAM (accounts) | 2 weeks | Restart and network drop lose nothing |
| **M4** | PEP; OMEMO interop; `dev/hub` Docker | 1.5 weeks | `XMPP_SERVER=hub interop-test.sh` all PASS |
| **M5** | MUC with room MAM and private rooms | 2.5–3 weeks | Encrypted private room with agents and Gajim |
| **M6** | Registration (ibr-pow, invites, probation); LAN TLS; client pin and join | 2 weeks | A second machine joins with an invite over pinned TLS |
| **M7** | Hardening, should-tier features, conformance, stock clients, packaging | 2.5–3 weeks | Interop suite green; stock-client matrix filled |
| **Total** | | **14.5–17 weeks**, about 4 months with a 15–20% buffer | |

### M1: streams, TLS, authentication, 1:1 routing (2–2.5 weeks)

**Scope**
- `crates/server` skeleton (§5.1), config loading and validation, `meta`/`hosts`/`accounts` tables and the migration runner.
- Listener with per-IP admission; `Budgeted` I/O.
- Negotiation stages A–C; STARTTLS with a self-signed certificate from rcgen; `hub.json` with the pin.
- SCRAM-SHA-256 and SCRAM-SHA-1 with `-PLUS` (`tls-exporter`); PLAIN under TLS or loopback; XEP-0440; fake salts; backoff.
- Bind (conflict replaces the old session), legacy session IQ, stream and stanza errors.
- Router plus a minimal UserActor: the resource list, priority 0 assumed until M2. Message routing to full and bare JIDs, IQ routing to full JIDs, ping, disco#info on the domain.
- Admin socket; CLI `hub init|run|start|stop|status|user add|del|passwd|list`.
- `hub status` JSON; logging per §5.10.

**Suggested order** (working days)
1. Days 1–2: crate skeleton, config, store with accounts, admin socket, CLI.
2. Days 3–4: listener, `accept_stream`, features, STARTTLS, budget reader, timeouts.
3. Days 5–6: SCRAM (+PLUS) and PLAIN, known-answer tests, failure handling.
4. Day 7: bind, session, errors.
5. Days 8–9: router, UserActor stub, message and IQ routing, ping, disco.
6. Day 10: harness tests, the demo script, README section.

**Demo**
- `agent-chat hub start`.
- `agent-chat --profile alice account add alice@localhost --host 127.0.0.1 --insecure-plaintext` (the existing flag). `--hub` arrives in M6, or earlier if cheap.
- The same for bob.
- `wait` and `send` work in both directions.

**Acceptance tests**
1. Harness: the tokio-xmpp client authenticates over TLS 1.3 with `SCRAM-SHA-256-PLUS`, and over loopback plaintext with `SCRAM-SHA-256`. A client that offers only PLAIN is refused before TLS.
2. Harness: a wrong password and an unknown user produce byte-identical failure sequences, apart from nonces.
3. Harness: a 300 KiB stanza closes the stream with `policy-violation`. A 10,001-byte stanza before auth does the same.
4. Harness: a second bind with the same resource sends `conflict` to the first session.
5. E2E (`AGENT_CHAT_TEST_SERVER=hub`): scenario 1 of §8.3.
6. `hub status` reports 2 sessions while two daemons are connected. `hub stop` makes both daemons report `Suspended`, then `Reset`, when the hub restarts.

### M2: roster, presence, offline storage, carbons, caps (2 weeks)

**Scope**
- The roster state machine and roster operations and pushes.
- Presence: initial, broadcast, probes, directed, unavailable on disconnect, priority routing.
- Pending subscription requests.
- Offline storage: an interim table until M3 links it to the archive.
- Carbons.
- Caps resolution and cache.
- disco#info and disco#items on accounts.
- Start the XMPP Interop Testing CI job.

**Demo**
- In the GUI, alice adds bob, bob approves (`contacts approve`), and presence dots update.
- alice on two resources (two profiles, same JID, different resources) sees her own sent messages as carbons.

**Acceptance tests**
1. RFC 6121 Appendix A tables pass as unit tests.
2. Harness: subscription round-trip; a presence probe answered from a local contact; directed presence then unavailable.
3. Harness: a message to the bare JID with two resources at priorities 5 and 0 goes only to the priority-5 resource, and as a carbon to the other.
4. Harness: a message while offline is delivered on initial presence with `<delay/>`.
5. E2E scenario 3. Interop suite: RFC6120, RFC6121, XEP-0030 and XEP-0199 tests pass, or each failure is documented with a reason.

### M3: stream management and MAM (2 weeks)

**Scope**
- XEP-0198: enable, resume, hibernation, queue limits, the offline handling of unacked stanzas.
- The archive table and policy.
- XEP-0313 queries with RSM, XEP-0359 stamping, XEP-0203.
- Offline storage now references archive rows.
- Retention task.

**Demo**
- `daemon stop` alice; bob sends 120 messages; `daemon start` alice: all of them show up exactly once in `inbox`.
- Kill alice's TCP connection: the daemon logs `Resumed` and nothing is lost.

**Acceptance tests**
1. E2E scenario 2, with 120 messages so RSM paging is exercised, plus scenario 8.
2. Harness: `before=''` with `max=50` returns the last 50 in order with `complete` set correctly. `after=<last>` returns an empty, complete page.
3. Harness: the offline-delivered copy and the MAM copy carry the same `stanza-id`.
4. Harness: a client-supplied `<stanza-id by='alice@localhost'>` is stripped.
5. Interop suite: XEP-0198 and XEP-0313.

### M4: PEP and OMEMO (1.5 weeks)

**Scope**
- PEP per §5.7 with `open` and `presence` access.
- `+notify` notifications; last item on presence and on caps.
- disco on bare JIDs.
- `dev/hub/` (Dockerfile, compose, setup script) and the factored `dev/lib/server.sh`.

**Demo**
- `agent-chat omemo devices bob@localhost` lists bob's devices.
- `XMPP_SERVER=hub dev/omemo-bot/interop-test.sh` prints four `PASS` lines.

**Acceptance tests**
1. E2E scenario 5, with both OMEMO versions in both directions.
2. Interop script all PASS against the hub.
3. Harness: publish-options mismatch gives `precondition-not-met`; `max_items=max` is accepted; a non-contact can read an `open` node but not a `presence` node.
4. Harness: a contact advertising `urn:xmpp:omemo:2:devices+notify` receives a notification on publish, and the last item when it comes online.
5. Interop suite: XEP-0060 and XEP-0163 tests.

### M5: group chat (2.5–3 weeks)

**Scope**
- MucService and RoomActor per §5.7: locked creation, owner forms, members-only and non-anonymous rooms, affiliations, mediated invites with membership, subject, history, private messages.
- Room MAM with real JIDs; occupant persistence rules.
- XEP-0410 and XEP-0421 (should).

**Demo**
- `agent-chat rooms join team@conference.localhost --private` as alice, then `rooms invite team@… bob@localhost`. bob accepts.
- An encrypted message appears for bob and in Gajim (joined as carol, invited).

**Acceptance tests**
1. E2E scenarios 4 and 6.
2. Harness: a locked room gives `item-not-found` to others until configured; status codes 110/201/100/170 are present as specified.
3. Harness: room MAM results for a non-anonymous room carry `muc#user item@jid`.
4. Differential transcript against Prosody for create → configure → join → message → invite → leave, with only allowlisted differences.
5. Interop suite: XEP-0045 tests. Manual: the Gajim and Dino rows for rooms.

### M6: registration and LAN mode (2 weeks)

**Scope**
- Extract `crates/ibr-pow` and switch `register.rs` to it.
- Native XEP-0077 with `ibr-pow:0`, policies, escalation; XEP-0445 invites and `hub invite`; probation, promotion and cleanup.
- TLS modes `local-ca` and `files`; `extra_names`.
- Client: `tls_pin`, `PinnedStartTlsConnector`, `account add --hub`, `account join`, `account register --tls-pin`.
- mDNS (should).

**Demo**
- On machine A: `hub init --lan` and `hub invite`.
- On machine B: `account join URI`, and B's agent chats with A's agent.
- `account register --server hub.lan …` solves a 16-bit challenge.

**Acceptance tests**
1. `crates/ibr-pow` vectors pass in Rust and in `test_vector.lua`.
2. Harness: the error conditions `pow-required`, `challenge-expired`, `challenge-invalid` (wrong IP prefix), `challenge-used`, `challenge-outdated` and `solution-invalid` are returned exactly as `mod_register_pow` returns them. The same client test suite passes against both servers.
3. Harness: a preauth token skips PoW, honours `uses`, and expires.
4. E2E scenario 7 against both servers.
5. Pinned connector: a wrong pin fails before any SASL is sent; the right pin succeeds with SCRAM-PLUS.
6. A LAN listener refuses PLAIN and IBR before STARTTLS.

### M7: hardening, conformance and should-tier features (2.5–3 weeks)

**Scope**
- Fuzzing targets in nightly CI.
- Tune the limits under a load test: 200 sessions and 50 messages/s on a laptop [I target].
- Should-tier features: blocking, vCard, private XML and bookmarks (`whitelist` access model), CSI, direct TLS, XEP-0478, version, contact addresses, roster versioning, Prometheus.
- caas run.
- The full stock-client matrix.
- launchd plist and systemd unit examples; Docker image; `cargo deny`.
- docs/AGENTS.md section; conformance checklist complete.

**Demo**
- The interop suite is green except documented gaps.
- The stock-client matrix is filled, with Monal iOS's limits documented.
- `hub status` and the Prometheus endpoint show the load test.

**Acceptance tests**
1. Each fuzz target runs for 1 hour with no crash.
2. The full interop suite with `failOnImpossibleTest=true` for the enabled specifications.
3. Load test: p99 routing latency < 50 ms and RSS < 300 MB for the target load [I target].
4. Every Must row in §4.3 has at least one automated test referenced in the checklist.

---

## 10. From local hub to hosted for anyone

### 10.1 Stages

| Stage | Who uses it | What gets added |
|---|---|---|
| 0 (v1) | One machine or one LAN | This plan |
| 1 (v1.x) | Small teams, home labs | XEP-0363 upload (HTTP listener plus file store), SASL2/Bind2/FAST, XEP-0227 import and export, **outbound-only s2s to push app servers** with XEP-0357 |
| 2 | One public server, one domain | Full S2S federation, ACME, abuse controls, Prometheus and alerting, Postgres optional |
| 3 | Hosted, many tenants | Multi-domain, per-tenant quotas, provisioning API, admin roles, billing hooks |
| 4 | Scale-out | Clustering, Postgres required, a separate TLS edge (xmpp-proxy-style), regional deployment |

### 10.2 Multiple domains and tenants

- v1 already keys accounts by `(domain, localpart)` and has a `hosts` table. Adding a domain is a row plus a certificate.
- Needed later:
  - per-host config overrides;
  - SNI certificate selection (`ResolvesServerCert`);
  - per-host MUC components;
  - per-host registration policy (for example a separate `agents.<domain>` host for self-registered agents, as `pow-registration.md` §4.3 recommends);
  - per-tenant quotas (accounts, storage, rooms);
  - a provisioning API (HTTP + JSON, authenticated) for creating tenants and domains;
  - tenant isolation in metrics and logs.

### 10.3 Server-to-server federation

- **Protocol:**
  - RFC 6120 s2s streams (`jabber:server`);
  - TLS with PKIX validation;
  - SASL EXTERNAL with certificates (XEP-0178), plus **Dialback (XEP-0220)** for interop with servers that lack usable certificates;
  - XEP-0288 bidirectional streams;
  - DNS SRV (`_xmpp-server._tcp`) and XEP-0368 `_xmpps-server`;
  - POSH and DANE later.
- **Architecture:**
  - an `S2sManager` with one actor per remote domain (outbound queue, connection state, backoff);
  - inbound s2s connections authenticate the remote domain and are allowed to send only `from` that domain;
  - the router's "remote" class, which bounces in v1, now enqueues to the S2sManager;
  - roster, presence and PEP already work through UserActor messages, so remote contacts reuse the same state machine with an s2s hop in place of an actor message [I].
- **Abuse at the federation edge:**
  - spam from strangers: RTBLs such as xmppbl.org, through XEP-0377 reports; a mod_anti_spam-style filter for first-contact messages;
  - per-remote-domain rate limits.
- **First step: allowlisted outbound s2s.** Allow federation only to the push app servers (`eu.prod.push.monal-im.org` [V Monal wiki]; Conversations' app server [I]). That unlocks push (§10.7) without opening the hub to the federated network.

### 10.4 Scaling out

- **Vertical first.** One node with Postgres can probably serve tens of thousands of concurrent sessions [I]. Most hosted deployments will never need more.
- **Cluster design** that the v1 actor model allows:
  - **Placement:** UserActors and RoomActors are placed on nodes by consistent hashing of the bare JID. Connection tasks stay wherever the TCP connection lands.
  - **Inter-node bus:** an authenticated internal transport (QUIC or TCP with length-prefixed frames carrying serialized stanzas plus actor messages). The `Router` sends to a local mailbox or a remote node based on the hash ring.
  - **Session registry:** the UserActor already knows its resources. A remote resource is just a handle naming another node.
  - **SM resumption across nodes:** the resume id encodes the node, and the edge forwards the resume to it. This is why §5.4.1 reserves bits in the id.
  - **Edge:** a separate TLS-termination tier (our own, or xmpp-proxy as a separate AGPL process) with PROXY protocol, so the hub sees real client IPs.
- **What v1 must avoid** so this stays possible:
  - global in-memory state other than the registries;
  - synchronous cross-actor calls;
  - relying on SQLite's single-writer serialization for correctness, such as read-modify-write outside a transaction.

### 10.5 Postgres

- A second implementation of the `Store` trait, using `sqlx` with only the `postgres` feature, or `tokio-postgres`. This avoids the `libsqlite3-sys` conflict (§3.3).
- Migrations per backend in `store/postgres/migrations/`, with the same logical schema (§5.8 conventions).
- The `archive` table partitioned by month. MAM queries stay index-only on `(archive_jid, ts_ms, id)`.
- `hub migrate --from sqlite://… --to postgres://…` for upgrades.
- XEP-0227 export and import for moving to or from Prosody and ejabberd.
- The Store contract test suite (§8.1) runs against both backends in CI.

### 10.6 Abuse controls

- **Registration:**
  - native PoW with per-prefix and global escalation;
  - an algorithm upgrade path (Equi-X or HashWX; the `equix` crate is LGPL-3.0-only, which is GPL-compatible [I]);
  - invites;
  - a separate agent domain;
  - tombstones;
  - reserved names.
- **After registration:**
  - probation limits (stranger messages, subscriptions, room creation);
  - account aging and promotion;
  - per-account stanza and storage quotas;
  - room-creation quotas.
- **Reporting and moderation:**
  - XEP-0377 reports with a moderation queue;
  - XEP-0191 blocking;
  - admin tools: CLI, then XEP-0133 commands and a web view;
  - an audit log of admin actions.
- **Network:**
  - per-IP and per-prefix connection limits;
  - optional DNSBL or RTBL checks at registration;
  - IPv6 prefix handling as in the PoW design.
- **Policy and legal:**
  - abuse contact (XEP-0157), terms of service, a data-retention policy;
  - a process for law-enforcement requests and illegal content.

  These are operational work, not code, but they gate "for anyone".

### 10.7 Push notifications

- XEP-0357 on the server:
  - clients send enable and disable IQs naming an app server JID and node;
  - the server publishes a summary notification (an XEP-0004 form with message count and last sender; no content by default) to the app server **over s2s** when a message arrives for a hibernating or offline session.
- Monal requires push, plus SM timeouts of 24 h or more [V]. Prosody's `mod_cloud_notify` is the behavioural reference [V Monal wiki names it].
- It depends on §10.3's allowlisted s2s. **It is the most valuable post-v1 feature for mobile users.**

### 10.8 Operations

- **Backups:**
  - SQLite online backups (`hub backup`) on a schedule;
  - optional streaming replication with Litestream [I];
  - Postgres point-in-time recovery when hosted.
  - Restore drills belong in the runbook.
- **Upgrades:**
  - versioned migrations, with `hub migrate --dry-run` and a pre-upgrade backup;
  - a graceful shutdown with `system-shutdown`; clients reconnect, and SM state is lost across restarts in v1;
  - later: persist SM queues, or drain one node at a time in a cluster.
- **Monitoring:**
  - Prometheus metrics (§5.10);
  - health endpoint;
  - alerts on auth-failure spikes, registration spikes, mailbox depth, store latency, certificate expiry and disk space.
- **Certificates:** ACME renewal at 2/3 of lifetime, hot-reloaded.
- **Packaging:**
  - a Docker image of `agent-chat-hub`;
  - systemd and launchd units;
  - signed release binaries;
  - `cargo audit` and `cargo deny` in CI.
- **Privacy:**
  - account export (XEP-0227) and full deletion (archive, PEP, rooms owned) on `<remove/>`;
  - a retention configuration per tenant.

### 10.9 Compliance roadmap

| When | Target |
|---|---|
| v1 | XEP-0479 Core (except 0114/0368) and IM Core (except 0363) |
| v1.x | Core Advanced (0368, 0115, 0163 complete), IM Advanced (0191, 0198, 0313 + MUC, 0402/0048/0049, 0223, 0410, 0398 + 0153), IM Core 0363 |
| Hosted | Mobile Advanced (0357), Web (RFC 7395), A/V (XEP-0215 with TURN) |
| Every release | Interop suite in CI; publish the results; caas badge for the public instance |

### 10.10 v1 choices that make the hosted path easier or harder

| v1 choice | Effect | Notes and mitigation |
|---|---|---|
| Actors addressed by JID; no synchronous cross-actor calls | **Easier**: clustering can swap the registry | Keep it a rule in code review |
| `Store` trait, portable SQL conventions, explicit ids, UUIDv7 archive ids | **Easier**: Postgres is a second implementation | The contract tests enforce it |
| `hosts` table and `(domain, localpart)` keys from day one | **Easier**: multi-domain becomes a data change | |
| Routing raw `Element`s, typed parsing only at the edges | **Easier**: s2s forwarding and new XEPs pass through | The namespace changes for s2s (`jabber:client` → `jabber:server`); serialize per stream |
| Roster and presence logic as pure state machines, local contacts via actor messages | **Easier**: remote contacts reuse it with an s2s hop | |
| Native PoW and invites in the core | **Easier**: abuse controls exist before public exposure | |
| SM id reserves node bits | **Easier**: cross-node resumption | |
| One SQLite file, single writer | **Harder** past one node; fine for v1 | Postgres stage |
| In-memory occupants, presence and SM state | **Harder**: restarts drop SM sessions and room occupancy | Clients rejoin. Persisting SM queues is later. |
| Default domain `localhost` | **Harder**: those JIDs can never federate or move to a hosted domain | `hub init --domain` with a real name for anything long-lived; XEP-0227 export; open question Q1 |
| Self-signed certificate + pin trust model | **Harder** for public use, where stock clients need PKIX | ACME and operator files are already modes |
| CLI-only admin over a Unix socket | **Harder** for hosted operations | Add an HTTP provisioning API and XEP-0133 later; reuse the admin request types |
| Server crate free of libomemo-c | **Easier**: its license can be chosen independently (Q6) | |
| No federation, so no push | **Harder** for mobile adoption | Allowlisted outbound s2s first (§10.3) |

---

## 11. Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| **The long tail of interop details**, especially MUC status codes and presence order, PEP notification semantics, and caps edge cases | High | Stock clients misbehave in subtle ways | Differential tests against Prosody (§8.4); interop suite; stock-client matrix per milestone; MUC has the largest buffer (M5) |
| **A self-written server exposed on a LAN** has security bugs | Medium | Account takeover, DoS | Loopback by default; TLS required on LAN; own SCRAM with known-answer tests; fuzzing; limits from M1; security review before M6 sign-off |
| **TLS trust for stock clients** (Dino and Monal need an OS-trusted certificate) | High | Friction for LAN users | Local-CA mode with a name-constrained CA; document a real-domain + ACME path |
| **No push without federation** | Certain | Monal on iOS barely usable; Conversations relies on a background connection | Say so clearly; allowlisted outbound s2s as the first post-v1 item |
| **The domain is permanent** | Medium | `localhost` users can't move later | Ask for a real domain at `hub init` when LAN mode is chosen; XEP-0227 export |
| **The tokio-xmpp responder API changes** in 7.x | Medium | Rework of the negotiation code | Pin `=6.0`. Keep our use behind `c2s/negotiate.rs`. The fallback is our own thin stream layer on `rxml` (MIT) |
| **xmpp-parsers strictness** rejects valid stanzas | Medium | Bounced messages | Route `Element`s; typed parsing only in handlers; fuzzing |
| **Unbounded XML elements** (no limit in tokio-xmpp's reader) | High if left unhandled | Memory DoS | Byte-budget reader in M1 (§5.4) |
| **Our client falls back to PLAIN** if `-PLUS` is missing | Certain if `-PLUS` is skipped | Weaker auth than intended | `-PLUS` in M1; a harness test asserts the mechanism used |
| **SQLite limits** (single writer, archive growth) | Low for LAN | Latency under bursts | Batched writer; retention; the Postgres path |
| **Two PoW implementations drift** (Lua module and hub) | Medium | Clients break on one server | Shared `crates/ibr-pow` vectors used by the Lua tests; the same client test suite runs against both (M6) |
| **Estimate overrun** | Medium | Later v1 | The should tier is cuttable; M7 scope can move to v1.x |
| **Hosted "for anyone" is more than code**: abuse handling, legal, on-call | Certain | Launch blocked | Plan the operational work separately before Stage 2 |

---

## 12. Open questions for the user

> Questions 1 (domain), 2 (process model), 4 (stock clients) and 6 (license) were decided on 2026-11-10; see the decisions at the top of this document. The rest still use the recommendations below.

1. **The hub's domain.** For local mode, is `localhost` acceptable, given that those JIDs can never move? For LAN mode, should `hub init --lan` require a name such as `hub.lan`, `<hostname>.local` or a real domain you own? *Recommendation:* `localhost` for local-only; require an explicit `--domain` for LAN; recommend a real domain.
2. **Process model.** Is a separate hub process (`agent-chat hub run`), in the same binary, acceptable? Or do you want the hub to run inside a daemon? *Recommendation:* separate process, optional auto-start from the daemon.
3. **Default registration policy.** Should loopback connections skip PoW (proposed)? In LAN mode, should the default be invites only, or invites plus PoW?
4. **Which stock clients must work in v1?** Is "iOS Monal in the foreground only" acceptable until push arrives?
5. **HTTP File Upload (XEP-0363).** Stock clients use it for images and files, and IM Core compliance needs it. Should it be in v1 (about +1.5 weeks [I]), or v1.x?
6. **License of the server crate.** Should it stay GPL-3.0-or-later like the workspace, or should a hosted future use **AGPL-3.0-or-later** so that people who run modified versions as a service must share their changes? The server doesn't link libomemo-c, so either is possible.
7. **Retention and privacy defaults.** MAM retention (proposed: 1 year, configurable, or forever?). Registration IP prefixes (proposed: 30 days). Archive-before-delivery durability for online recipients (proposed: ≤10 ms batch; is a crash window acceptable?).
8. **Early limited federation.** Is outbound-only s2s to allowlisted push servers acceptable as the first step after v1, before full federation?
9. **Admin interface.** CLI and Unix socket only in v1 (proposed)? Or also XEP-0133 commands, so admins can use Gajim?
10. **The hosted target.** What timeline and scale (accounts, concurrent sessions, number of tenants) should the hosted service plan for? That decides how early Postgres and clustering matter.
11. **Packaging.** Is a Docker image and a standalone `agent-chat-hub` binary wanted in v1 (proposed: yes, they're needed for interop tests anyway)? Should launchd and systemd units ship as examples?

---

## 13. Sources

**Local crate source** (`~/.cargo/registry/src/index.crates.io-*/…`) [V]
- `tokio-xmpp-6.0.0`:
  - `src/xmlstream/{mod.rs, responder.rs, common.rs, xmpp.rs}`
  - `examples/echo_server.rs`
  - `src/connect/{mod.rs, starttls.rs, tls_common.rs}`
  - `src/client/login.rs`
  - `src/stanzastream/mod.rs`
  - `src/component/login.rs`
  - `Cargo.toml` (license MPL-2.0)
- `sasl-0.5.2`:
  - `src/server/{mod.rs, mechanisms/scram.rs}`
  - `src/secret.rs`
  - `src/client/mechanisms/scram.rs`
  - `src/common/mod.rs`
- `rxml-0.14.0/src/parser/common.rs` (`max_token_length`, default 8192)
- `xmpp-parsers-0.23.0`:
  - `src/stream_features.rs`
  - `src/sm.rs`
  - the `src/muc/` and `src/pubsub/` module lists
- `jid-0.12.3/src/lib.rs` (stringprep)
- `rustls-0.23.46/src/conn.rs` (`export_keying_material` on `ConnectionCommon`)

**This repository** [V]
- `README.md`, `docs/AGENTS.md`
- `docs/design/pow-registration.md`, `docs/design/avatars.md`
- `crates/daemon/src/`:
  - `xmpp/session.rs`, `xmpp/actions.rs`, `xmpp/inbound.rs`, `xmpp/disco.rs`
  - `omemo/xmpp.rs`, `omemo/xml.rs`
  - `config.rs`, `daemon.rs`, `register.rs`
- `crates/proto/src/paths.rs`, `crates/cli/src/main.rs`
- `dev/prosody/{docker-compose.yml, prosody.cfg.lua, setup.sh}`
- `dev/omemo-bot/{interop-test.sh, run.sh, bot.py}`
- `server/prosody/mod_register_pow/{mod_register_pow.lua, pow.lib.lua, test_vector.lua, README.md}`
- `Cargo.toml` and `Cargo.lock` (locked versions)

**crates.io API** [V]: versions, licenses and dependency requirements for:
- tokio-xmpp, xmpp-parsers, jid
- sqlx-sqlite 0.9.0, deadpool-sqlite 0.14.0, rusqlite 0.41.1
- rcgen, instant-acme, rustls-acme, tokio-rustls
- prometheus-client, metrics-exporter-prometheus, governor
- mdns-sd, libfuzzer-sys, arbitrary
- precis-profiles, stringprep, quick-xml
- tokio-postgres, sqlx-postgres
- xmpp-proxy, scram
- https://crates.io/api/v1/crates/sqlx-sqlite/0.9.0/dependencies

**Specifications**
- RFC 5802 SCRAM [V]: https://www.rfc-editor.org/rfc/rfc5802.html
- RFC 9266 Channel Bindings for TLS 1.3 [V]: https://www.rfc-editor.org/rfc/rfc9266.html
- RFC 6120, RFC 6121, RFC 7622, RFC 7677, RFC 7590 [I: cited from knowledge, re-read during M1–M2]:
  - https://www.rfc-editor.org/rfc/rfc6120.html
  - https://www.rfc-editor.org/rfc/rfc6121.html
  - https://www.rfc-editor.org/rfc/rfc7622.html
  - https://www.rfc-editor.org/rfc/rfc7677.html
- XEP list and statuses [V]: https://xmpp.org/extensions/
- XEP-0479 Compliance Suites 2023 [V]: https://xmpp.org/extensions/xep-0479.html
- Compliance suites page [V]: https://xmpp.org/about/compliance-suites/
- XEPs cited for behaviour [I: to be re-read in the milestone that implements them]:
  - XEP-0045: https://xmpp.org/extensions/xep-0045.html
  - XEP-0060: https://xmpp.org/extensions/xep-0060.html
  - XEP-0115: https://xmpp.org/extensions/xep-0115.html
  - XEP-0163: https://xmpp.org/extensions/xep-0163.html
  - XEP-0198: https://xmpp.org/extensions/xep-0198.html
  - XEP-0280: https://xmpp.org/extensions/xep-0280.html
  - XEP-0313: https://xmpp.org/extensions/xep-0313.html
  - XEP-0357: https://xmpp.org/extensions/xep-0357.html
  - XEP-0359: https://xmpp.org/extensions/xep-0359.html
  - XEP-0440: https://xmpp.org/extensions/xep-0440.html
  - XEP-0445: https://xmpp.org/extensions/xep-0445.html
  - XEP-0478: https://xmpp.org/extensions/xep-0478.html

**Existing servers and tools**
- Lonewolf [V]:
  - https://github.com/lonewolf-im/lonewolf
  - https://raw.githubusercontent.com/lonewolf-im/lonewolf/main/Cargo.toml
  - https://raw.githubusercontent.com/lonewolf-im/lonewolf/main/docs/STANDARDS.md
  - https://xmpp.org/software/lonewolf/
- XMPP server list [V]: https://xmpp.org/software/?category=servers
- xmpp-proxy [V]: https://github.com/moparisthebest/xmpp-proxy
- prose-pod-server [V: search]: https://github.com/prose-im/prose-pod-server
- `xmpp-server` crate [V]: https://lib.rs/crates/xmpp-server
- XMPP Interop Testing [V]:
  - https://xmpp-interop-testing.github.io/
  - https://github.com/XMPP-Interop-Testing/xmpp-interop-tests-action
- Compliance Tester / caas [V: search]: https://compliance.conversations.im/about/
- Monal server-admin considerations [V]: https://github.com/monal-im/Monal/wiki/Considerations-for-XMPP-server-admins
- Conversations README mirror (push, SM) [V: search]: https://git.secluded.site/cheogram-android/blob/25856992d199a76a661c246e9621a21c5cf81775/README.md
- Gajim self-signed certificate reports [V: search]:
  - https://dev.gajim.org/gajim/gajim/-/issues/11366
  - https://discourse.igniterealtime.org/t/gajim-has-problem-with-self-signed-certificate/91574
