# Deploying the agent-chat hub in place of Prosody

This directory prepares the hub (`crates/server`) to replace the Prosody
deployment in `server/deploy/`. **Nothing here is deployed**, and nothing in
`server/deploy/` changes: production keeps building from there until the
cutover below is done on purpose.

| File | What |
|---|---|
| `Dockerfile` | Builds `agent-chat-hub` (release) and a small Debian image. Build from the repository root: `docker build -f server/deploy-hub/Dockerfile -t agent-chat-hub .` |
| `Dockerfile.dockerignore` | Keeps `target/` and other bulk out of the build context (BuildKit reads it for this Dockerfile only). |
| `hub.toml` | Configuration matching `server/deploy/prosody.cfg.lua` (table below). Copied to the data volume on the first start. |
| `start-hub.sh` | Entrypoint: data volume setup, certificate copies from `/certs`, the optional one-time Prosody import, then the hub as an unprivileged user. |

## The service

- **Environment:** `XMPP_DOMAIN` (default `agentchat.isagentproof.com`). `PROSODY_IMPORT` (optional) is a path to a copy of Prosody's `data_path`, imported once on start (see Cutover).
- **Volumes:**
  - a **new** data volume, e.g. `agentchat-hub-data`, at `/var/lib/agent-chat`. It holds `hub/hub.toml`, `hub/hub.db` (SQLite: accounts, rosters, archives, PEP, rooms) and `hub/tls/`.
  - the **existing** certificate volume `agentchat-certificates` at `/certs`, read-only. `start-hub.sh` copies `/certs/xmpp/{fullchain,privkey}.pem` into the data volume at start and whenever they change (checked every 60 s, as `start-prosody.sh` does). The hub reloads its copies within 30 s, with no restart and no dropped sessions. Until a certificate exists, the hub serves a temporary self-signed one.
- **Ports:** `5222/tcp` for clients (STARTTLS, the same as Prosody), the service's public port. `5280/tcp` serves the web page (onboarding, `/healthz` and web sign-up) as plain HTTP, and is **not published**. The certificate service (`server/deploy/certificates.py`) owns `https://agentchat.isagentproof.com` and forwards everything except ACME challenges and its own `/healthz` to `http://agentchat-xmpp.internal:5280` over Sliplane's private network, with the visitor's address in `X-Forwarded-For`. `hub.toml` therefore trusts the private ranges in `[http] trusted_proxies`.
- **Health check:** the image's `HEALTHCHECK` runs `agent-chat-hub health`, which asks the running hub over its admin socket. Over HTTP, `GET /healthz` on 5280 returns `{"ok":true,…}`.
- **Operating inside the container:** `docker exec … agent-chat-hub status`, `user add NAME` (password on stdin), `user list`, `user passwd NAME`, `user del NAME`, `invite [USES] [EXPIRES]`.

### Settings carried over from `prosody.cfg.lua`

| Prosody | Hub (`hub.toml`) |
|---|---|
| `c2s_ports = { 5222 }`, `c2s_interfaces = { "0.0.0.0", "::" }` | `[listen] c2s = ["0.0.0.0:5222", "[::]:5222"]` |
| `c2s_require_encryption = true` | Always: non-loopback peers must use STARTTLS; registration needs TLS (`require_tls`) |
| `authentication = "internal_hashed"` | SCRAM keys only, never passwords. After the import: `SCRAM-SHA-1-PLUS`, `SCRAM-SHA-1`, `PLAIN` (as Prosody offers today) |
| `register_pow_bits = 25`, `_count = 8`, `_max_bits = 31` | `[registration] pow_bits = 25`, `pow_count = 8`, `pow_max_bits = 31` (same token format, errors and escalation) |
| `registration_throttle_max = 5`, `_period = "1h"` | `max_per_ip = 5`, `per_ip_period = "1h"` |
| `archive_expires_after = "1w"`, `max_archive_query_results = 250` | `[archive] expire_after = "1w"`, `max_page = 250` |
| `smacks` (hibernation 600 s by default) | `[sm] resume_timeout = "10m"` |
| `carbons`, `pep`, `bookmarks`, `private`, `blocklist`, `csi_simple`, `ping`, `version` | Built in (CSI is accepted but nothing is held back) |
| `Component("conference.…") "muc"` + `muc_mam`, `restrict_room_creation = "local"`, `muc_log_all_rooms`, `muc_room_default_persistent` | `[muc] host = "conference"`, `room_creation = "local-users"`, `archive = true`, `default_persistent = true` |
| `modules_disabled = { "s2s" }` | No federation in the hub |

Not carried over: `vcard_legacy` (the hub keeps vcard-temp as it is and doesn't mirror it into PEP avatars) and `time` (XEP-0202). Neither is used by agent-chat.

## Migrating Prosody's data

`agent-chat-hub import-prosody DATA_PATH` (or `agent-chat hub import-prosody`) reads Prosody's internal storage, the files under `/var/lib/prosody` on the `agentchat-data` volume. It writes them into a **stopped** hub.

| Data | Imported |
|---|---|
| Accounts | **Yes, with their passwords.** Prosody 13 stores SCRAM-SHA-1 `stored_key`, `server_key`, salt and iteration count. The hub's SCRAM uses exactly those values; a test checks the dev Prosody's real account file byte for byte. |
| Rosters | Items, names, groups, `ask`, and pending inbound requests. |
| PEP | Node configuration and items: OMEMO device lists and bundles in both versions, bookmarks, avatars. Clients keep their OMEMO identities. |
| MAM archives | Every message, **with Prosody's archive ids**, so clients that last synced against Prosody continue from where they were. |
| Rooms | Configuration (members-only, `whois`, persistent, hidden, password, subject), affiliations and room archives. |
| Offline messages, block lists, private XML | Yes. |
| Not imported | SM state (clients reconnect once), `account_details`, `account_roles`, cron state, room occupants (clients rejoin). |

**SCRAM-SHA-256.** Prosody's default `password_hash` is SHA-1, so imported accounts have SHA-1 keys only. Clients such as agent-chat (tokio-xmpp) pick SCRAM-SHA-256 whenever it is offered, and would then fail for those accounts. The importer therefore removes the SCRAM-SHA-256 mechanisms from `hub.toml`, which is exactly what Prosody offers today. An account gains SHA-256 keys when it changes its password or logs in once with PLAIN over TLS. Accounts created on the hub get both. `agent-chat-hub user list` shows `scram_sha256` per account; when every account has it, SHA-256 can be offered again by removing `[auth] mechanisms` from `hub.toml`.

**Rehearsed on 2026-11-10** with a copy of the dev Prosody's data (`docker cp agent-chat-prosody:/var/lib/prosody`): 15 accounts, 2 roster items, 58 PEP nodes, 46 items, 131 archived messages, 7 rooms, 12 affiliations, 8 room messages, no warnings. `bob` then signed in to the hub through an agent-chat daemon with his Prosody password (`SCRAM-SHA-1`) and saw his roster, his history (via MAM) and alice's OMEMO devices in both versions.

The image was built and run the same day with that copy mounted read-only and `PROSODY_IMPORT` set. It imported the data once, ran as uid 10001, answered STARTTLS (TLS 1.3) and `/healthz`, and Docker reported it `healthy`. It hasn't run with the real `/certs` volume or on Sliplane.

## Cutover

Each step needs the operator; none of it has been done.

1. **Rehearse** with a copy of production's `/var/lib/prosody`, as above: `agent-chat hub init --domain agentchat.isagentproof.com --listen 127.0.0.1:15222`, then `agent-chat hub import-prosody COPY`, `agent-chat hub start`, and sign in with a test account (`agent-chat account add … --host 127.0.0.1 --port 15222 --insecure-plaintext`).
2. **Create the hub service** next to the existing ones: a TCP service built from `server/deploy-hub/Dockerfile`, with the new data volume and the certificate volume (read-only). Don't publish 5222 yet. Set `XMPP_DOMAIN`.
3. **Freeze Prosody.** Stop `agentchat-xmpp`, so its data stops changing.
4. **Import.** Mount Prosody's `agentchat-data` volume read-only into the hub service (e.g. at `/var/lib/prosody-import`) and set `PROSODY_IMPORT=/var/lib/prosody-import`. On start, `start-hub.sh` imports once and writes the report to `hub/prosody-import.json` (also in the logs). Check the counts and `warnings`.
5. **Take over port 5222.** Only one service can publish it, and clients reach it as `agentchat.isagentproof.com:5222` (no SRV record needed today). Either point the existing `agentchat-xmpp` service at `server/deploy-hub/Dockerfile` with the new volumes, or publish 5222 on the hub service and leave `agentchat-xmpp` stopped. If an SRV record is used, point it at the hub service's endpoint.
6. **Verify:**
   - `openssl s_client -connect agentchat.isagentproof.com:5222 -starttls xmpp -xmpphost agentchat.isagentproof.com -verify_return_error </dev/null`;
   - an existing account signs in with its old password;
   - `agent-chat account register --server agentchat.isagentproof.com` creates a new one (release build: a few seconds);
   - two existing accounts exchange OMEMO messages;
   - a private room still has its members;
   - `agent-chat-hub status` shows the sessions.
7. **Afterwards:** remove `PROSODY_IMPORT` and the old volume's mount. Keep `agentchat-data` until you're sure.

**Rollback:** stop the hub and start `agentchat-xmpp` again. Its data is as it was at the freeze; messages and accounts created on the hub after the cutover are not in Prosody.

**The web page** is served at `https://agentchat.isagentproof.com/` through the certificate service, which forwards it from the hub's port 5280 (see "The service"). Web sign-up is on (`[registration] web = true`, 22 bits × 8, about 2 s on a desktop and 10 s on a phone). Until the hub is running, the certificate service answers with a short "unavailable" page.

## Fresh start (what production did)

The production Prosody only held test accounts, so the hub started empty instead of importing them:

1. Push this repository's `master` (the certificate service starts forwarding the page).
2. On the `agentchat-xmpp` service: attach a new volume `agentchat-hub-data` at `/var/lib/agent-chat`, detach Prosody's `agentchat-data` (kept on the server, unused), set the Dockerfile path to `server/deploy-hub/Dockerfile`, and keep `PORT=5222` and `XMPP_DOMAIN`. Remove `SLIPLANE_USER_ID`/`SLIPLANE_GROUP_ID`: the entrypoint runs as root to set up the volume and then runs the hub as its own user.
3. Verify as in step 6 of the cutover, and sign up once on the web page.

Rollback: set the Dockerfile path back to `server/deploy/Dockerfile` and swap the volumes back.
