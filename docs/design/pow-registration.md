# Proof-of-work self-registration for agents

- **Status:** design proposal. Nothing here is implemented yet.
- **Date:** 2026-11-10
- **Scope:**
  - the `agent-chat account register` command;
  - a Prosody module, `mod_register_pow`;
  - the wire protocol between them.

**How claims are marked**

- **[V]** I verified it against a cited source: a spec, Prosody or crate source code, or project docs. The sources are listed in §13.
- **[M]** I measured it on the development machine: an Apple M2 (4 performance and 4 efficiency cores), macOS, Rust 1.99 `--release`, with a throwaway benchmark outside the repo.
- **[I]** It's my inference, estimate or design choice, and is not verified.

---

## 1. Summary

**Protocol.** Agents register through standard XEP-0077 in-band registration.
- The server's registration form carries a signed, stateless proof-of-work (PoW) challenge in namespaced data-form fields.
- The client solves the challenge and returns the solution in the same form.
- There is no new IQ namespace and no web endpoint.

**v1 algorithm: SHA-256 partial preimage (hashcash-style).**
- A solution is 8 sub-puzzles of 25 leading zero bits each, so the expected work is 2^28 hashes.
- On an M2 that takes about 9 s with 8 threads and about 48 s on one thread [M].
- Verification costs 8 hashes and 1 HMAC. It needs nothing beyond Prosody's built-in `util.hashes` [V].

**Binding and replay.**
- The challenge is an HMAC-signed token with a 1-hour expiry. It is bound to the server domain and to the client's IP prefix.
- The hash input also binds the challenge to the requested username.
- Replay is blocked by a cache of spent tokens and by the uniqueness of usernames.

**Difficulty.** Difficulty is a base value plus two escalations, capped at 2^34 expected hashes (about 9.5 minutes on an M2 with all cores) [M-derived]:
- a per-prefix escalation: each recent registration from the same IP prefix doubles the work, up to 64×;
- a global-load escalation.

**PoW slows attackers down but does not stop them.**
- An RTX 4090 computes SHA-256 about 2,600–3,900× faster than one M2 core [V+M]. That is about 12 ms per default challenge.
- So the design pairs PoW with other limits:
  - per-IP throttles;
  - a probation role with send limits for new accounts;
  - account aging;
  - optionally, a separate VirtualHost for agent accounts.
- The protocol lets a challenge name its algorithm. A GPU-resistant function (Equi-X, or its successor HashWX) is the planned upgrade path.

**Server.**
- `mod_register_pow` hooks `stanza/iq/jabber:iq:register:query` at a higher priority than `mod_register_ibr` [V].
- It serves the form, verifies the solution, then hands the stanza on. This means `mod_register_ibr`, `mod_register_limits` and `mod_invites_register` work unchanged.
- Holders of an XEP-0445 invite skip the PoW.

**Client.**
- Registration is a library function in `agent-chat-daemon`, called by the CLI.
- It uses tokio-xmpp's low-level connector and `XmlStream` before SASL, plus the xmpp-parsers `ibr` and `data_forms` types [V].
- The solver is multi-threaded on a blocking thread. Progress goes to stderr as JSON lines, and one JSON result goes to stdout.

**Standardization.** Write this up as a ProtoXEP once it has run in the dev environment. The SHA-256 challenge that XEP-0158 already defines is underspecified: it has no expiry, no username binding, no byte encoding and no replay rule [V].

---

## 2. Goals and non-goals

**Goals**

1. An agent with only the CLI and network access can create an account on a participating server, with no human, CAPTCHA, email or invite.
2. Each additional account costs the registrant measurable compute. The operator can change that cost at runtime.
3. It works with stock Prosody 13 plus one drop-in Lua module, with no C dependencies.
4. It uses only standard XMPP mechanisms (XEP-0077 with XEP-0004 data forms), so other servers and clients can implement it and it can be standardized.

**Non-goals**

- Proving that the registrant is an AI agent, or a human. Anyone who runs the CLI can register. [I: no PoW scheme can establish this.]
- Stopping a well-funded attacker on its own (see §4).
- Password recovery or reset, which belong to XEP-0389.
- Changing servers we don't run. The client must still work with plain open registration (a form with no PoW fields) and with invites.

---

## 3. Research findings

### 3.1 XMPP specifications

| Spec | Status | What matters here |
|---|---|---|
| **XEP-0077** In-Band Registration | Final, v2.4 (2012) [V] | See below the table. |
| **XEP-0004** Data Forms | Final | A hidden field "is not shown to the form-submitting entity, but instead is returned with the form". A processor MUST ignore fields it does not understand, and a submission MAY omit fields that aren't required [V]. |
| **XEP-0068** Field Standardization | Active, v1.3.0 [V] | A field defined by anyone other than the owner of the FORM_TYPE MUST be named in Clark notation, `{namespace-uri}name`. The `x-` prefix is no longer required [V]. So our fields are named `{urn:xmpp:ibr-pow:0}challenge`, and so on. |
| **XEP-0158** CAPTCHA Forms | Stable, v1.0.1 [V] | See below the table. |
| **XEP-0389** Extensible In-Band Registration | Experimental, v0.6.0, last updated 2020-11-17 [V] | Registration is a "flow" of challenges. Proof-of-work is a stated goal, but the spec defines only data-form, out-of-band and SASL-reset challenges [V]. Prosody 13.0 has no code for `urn:xmpp:register:0` [V]. It is not usable today. |
| **XEP-0401** Ad-hoc Account Invitation Generation | Experimental, v0.6.0 (2026-03-08) [V] | Invite URIs of the form `xmpp:example.org?register;preauth=TOKEN` [V]. |
| **XEP-0445** Pre-Authenticated In-Band Registration | Proposed, v0.2.0 [V] | See below the table. |
| **XEP-0518** Payment Required | Experimental, v0.1.0 (2026-08-25) [V] | Covers paid registration. It cites XEP-0158's hashcash but defines no PoW mechanics [V]. |

**XEP-0077 details**
- The client sends an empty `<query/>` as an IQ get. The server answers with the fields it needs, optionally as an `x:data` form whose FORM_TYPE is `jabber:iq:register`.
- The client then submits **only** the form; it must not also send the legacy fields. An empty result means success. `conflict` means the username is taken, and `not-acceptable` means information is missing [V].
- A host "MUST NOT add new fields to the 'jabber:iq:register' namespace". Extra data goes in data forms [V].
- Registration happens on an unauthenticated client stream, advertised by `<register xmlns='http://jabber.org/features/iq-register'/>` [V].
- The security section says in-band registration is insecure without channel encryption. It says nothing about CAPTCHAs; its only abuse control is that a server may return `not-acceptable` after too many attempts [V].

**XEP-0158 details**
- It already has a computational challenge. In the §6.2 `SHA-256` field, the label is a random hex value of a configured bit length. The answer is a string that starts with the JID the stanza was sent to, and whose SHA-256 has least-significant bits equal to the label [V].
- In registration, the form sits directly in `<query/>` with FORM_TYPE `jabber:iq:register`, alongside hidden `challenge`, `sid` and `answers` fields [V].
- New challenge types may be registered with the XMPP Registrar [V].
- The spec says administrators "SHOULD increment the configured number of bits from time to time", and that hashcash alone "will not completely stop abusive stanzas" [V].
- It does not define:
  - an expiry;
  - a binding to the username;
  - the byte encoding or bit order of the hash input;
  - any replay handling [V: absent from the spec].

**XEP-0445 details**
- The stream feature is `<register xmlns='urn:xmpp:ibr-token:0'/>`.
- Before XEP-0077 registration, the client sends the IQ `<preauth xmlns='urn:xmpp:pars:0' token='…'/>` [V].
- Prosody 13 implements this in `mod_invites_register` [V].

**Existing proposals.** I found no XEP, ProtoXEP or archived standards-list thread that proposes proof-of-work for registration. The closest are XEP-0158 §6.2 and the stated goals of XEP-0389. Absence from a search is weaker evidence than a positive finding.

### 3.2 Prosody 13 registration machinery

I read all of the following in the Prosody `13.0` branch source [V]. `mod_register_ibr`, `mod_register_limits` and `mod_invites_register` differ from trunk only cosmetically.

**Module chain**
- `mod_register` loads `mod_register_ibr` when `allow_registration = true`. It also loads `mod_user_account_management`, which handles password changes for logged-in users.
- `mod_register_ibr` tries to load `mod_register_limits` with `module:depends` inside a `pcall`.
- `mod_invites_register`, which the official Docker config enables by default, loads `mod_register_ibr` itself when `registration_invite_only` is true. **`registration_invite_only` defaults to `true`.**

**How `mod_register_ibr` works**
- It handles `stanza/iq/jabber:iq:register:query` at priority 0.
  - It refuses sessions that aren't `c2s_unauthed`.
  - It refuses sessions without encryption when `c2s_require_encryption` is set, which is the default (true).
- The GET reply is a **static** `<query/>` built once at module load. It contains `<instructions/>`, the legacy `<username/>` and `<password/>`, and a data form with FORM_TYPE, `username`, `password` and the fields from `additional_registration_fields`. **There is no hook that lets another module add per-request fields to this form.**
- On SET, it parses the form with `util.dataforms` `form:data()`. That function iterates **only the fields declared in the layout**, so unknown fields are dropped.
- It then fires `user-registering` with `{ username, password, host, additional, ip, session, role, allowed = true }`. **The event does not carry the stanza.**
  - A handler vetoes registration by setting `allowed = false` and `error` (a `util.error`), or the older `reason`, `error_type` and `error_condition`.
  - A handler can also set `role`, which is passed to `create_user_with_role` (default role `prosody:registered`).
- After the account is created, it fires `user-registered` with `{ username, host, source, session }`.
- Prosody's event system runs handlers in **descending priority** and stops at the first handler that returns a non-nil value. Unauthenticated IQs are dispatched as `stanza/iq/<xmlns>:<name>` on the target host (`core/stanza_router.lua`).

**`mod_register_limits`**
- It runs on `user-registering` and checks, in order: the blocklist, then the allowlist-only mode, then a per-IP `util.throttle`.
- The throttle is keyed on the **exact IP string**, with an LRU cache of 100 IPs by default. A throttled attempt gets `wait`/`policy-violation` with the text "Too many registrations from this IP address recently".
- The default allowlist is `{ "127.0.0.1", "::1" }`, and allowlisted IPs skip the throttle. There is no throttle at all unless `registration_throttle_max` or `min_seconds_between_registrations` is set.
- Because the key is the exact IP, a per-address throttle on IPv6 is easy to evade [I].

**`mod_invites_register`**
- On `stanza/iq/urn:xmpp:pars:0:preauth`, it sets `session.validated_invite`.
- On `user-registering`, it denies registration when invite-only mode is on and there's no invite. When invite-only mode is off, it lets registrations without an invite through.

**Roles**
- `prosody:member` inherits from `prosody:registered`, which inherits from `prosody:guest`.
- Self-registered accounts get `prosody:registered`.
- Permissions that `prosody:registered` has by default:
  - `:upload` (file sharing);
  - `:create-room`;
  - `:invite-users`, when `allow_user_invites` is set.
- Admins can define their own roles with `custom_roles`, and adjust permissions with `add_permissions` and `remove_permissions`.

**Other modules that already hook registration in 13.0:**
- `mod_tombstones` blocks re-registration of deleted usernames;
- `mod_mimicking`;
- `mod_watchregistrations`;
- `mod_welcome`;
- `mod_account_activity` records the last login time and has a `user list_inactive` shell command.

**Crypto available to Lua: `prosody.util.hashes`** (C, OpenSSL EVP)
- Hashes: `sha1`, `sha224`, `sha256`, `sha384`, `sha512`, `md5`, `sha3_256`, `sha3_512`, `blake2s256`, `blake2b512`.
- HMAC: `hmac_*` for each of the above. The signature is `(key, msg, hex)`.
- Key derivation: `pbkdf2_hmac_sha1`, `pbkdf2_hmac_sha256`, `hkdf_hmac_sha256` and `hkdf_hmac_sha384`.
- Constant-time comparison: `equals`.
- **There is no Argon2, scrypt or Equi-X.** A memory-hard verifier would need a C binding. One option is `lua-argon2`, a LuaRocks package that links the reference libargon2 [V]. No Lua binding for Equi-X exists [I: none found].

**Helpers available to Lua**
- `prosody.util.random.bytes` (CSPRNG);
- `util.cache` (LRU);
- `util.throttle`;
- `util.ip`, whose `truncate(ip, bits)` accepts multiples of 8 only;
- `module:daily()` and `module:hourly()` cron helpers;
- `usermanager.set_user_role`.

**c2s limits**
- `c2s_timeout` defaults to "5 minutes". It closes connections that are still unauthenticated after that time.
- `c2s_unauthed_stanza_size_limit` defaults to 10000 bytes.

**Official Docker image (`hg.prosody.im/docker`)** [V]
- It is built on Debian trixie with Lua 5.4. `luarocks` and the Lua headers are installed.
- `/etc/prosody/modules` is on the plugin path. `PROSODY_ENABLE_MODULES` and `PROSODY_EXTRA_CONFIG` (`/etc/prosody/conf.d/*.cfg.lua`) are supported.
- The default 13.0 config enables `register`, `invites`, `invites_adhoc`, `invites_register` and `account_activity`.

**Community modules** (modules.prosody.im) [V]
- Registration-related:
  - `mod_captcha_registration` (beta) is a modified copy of `mod_register` that serves image CAPTCHAs through libgd;
  - `mod_register_dnsbl`;
  - `mod_dnsbl`;
  - `mod_block_registrations`;
  - `mod_register_web` (reCAPTCHA);
  - `mod_invites_*`.
- Abuse-related:
  - `mod_firewall`: rule-based stanza filtering with `LIMIT`/`%RATE`, but marks apply only to sessions;
  - `mod_anti_spam` (alpha): filters messages from strangers and supports RTBLs such as xmppbl.org;
  - `mod_throttle_unsolicited`: limits unsolicited messages per minute from local users, and needs `mod_track_muc_joins`;
  - `mod_spam_reporting` (XEP-0377).
- **No module does PoW or hashcash.** So `mod_register_pow` doesn't clash with an existing name.

### 3.3 Prior art

**Hashcash (Back, 1997/2002)**
- Format and checks:
  - The stamp is `ver:bits:date:resource:ext:rand:counter`, hashed with SHA-1.
  - 20 bits takes about 1 s on a desktop of that era. Verification is a single hash.
  - The recipient checks that the date is within 2 days and the resource matches, and keeps a **double-spend database** [V, Wikipedia].
- Known weaknesses:
  - work levels are hard to tune across hardware;
  - Moore's law forces difficulty up over time;
  - botnets get the work for free;
  - ASICs could be about three orders of magnitude faster [V].
- Laurie & Clayton, "Proof-of-Work Proves Not to Work" (WEIS 2004), argue that per-email PoW cannot price out spammers without burdening legitimate senders [V, abstract/conclusion]. Registration happens once per account, not once per message, so it can tolerate a much larger cost per event [I].

**Tor onion-service PoW (Proposal 327, hspow-spec v1 "Equi-X")**
- The algorithm:
  - The client solves Equi-X over `P || ID || seed || nonce || effort`. It then checks a Blake2b-32 result `R` with `R * effort <= UINT32_MAX`.
  - The service runs the cheap Blake2b check before `equix_verify` [V].
  - Equi-X is built on Equihash(60,3) with HashX. It is designed to "minimize the CPU-to-GPU asymmetry", needs about 1.8 MiB of RAM, and has about 50 µs verification [V].
- Replay and seeds:
  - The service MUST reject a repeated `(seed, nonce)`. A hash table or Bloom filter can track them for the seed's lifetime.
  - Seeds rotate every 105–120 minutes [V].
- Effort:
  - The suggested effort is retuned every 300 s from the priority queue: it rises under overload and decays by 2/3.
  - Clients retry with doubled effort (×1.5 above 1000), capped at 10000 [V].
- Upstream status:
  - tevador's README now says "**Equi-X has been deprecated. New projects should use HashWX**" [V].
  - HashWX (C11, LGPL-3.0, no releases yet) claims verification under 10 µs. Its benchmark has an RTX 5060 Ti at 5.6 MH/s against a Ryzen 3700X at 4.4 MH/s, i.e. GPU close to parity with a CPU [V].

**Anubis (TecharoHQ, current main branch)**
- The classic `fast` challenge:
  - It uses SHA-256 with "leading zeroes" counted in **hex nibbles**, defaulting to 5.
  - The client solves in a Web Worker. Success sets a signed JWT cookie with a 1-week expiry.
  - The ed25519 signing key is regenerated at every start [V].
- The newer WASM challenges offer `argon2id` (RustCrypto `Argon2::default()`, i.e. m = 19 MiB, t = 2, p = 1), `hashx` and `sha256`, with difficulty counted in **bits**.
- The docs warn about plain SHA-256: "There are known tools that solve this particular challenge. Prefer argon2id or hashx" [V].
- The author notes that counting difficulty in nibbles made each step 16× harder, and moved to bits [V].

**mCaptcha**
- It uses a SHA-256 PoW (`pow_sha256` crate) whose difficulty is set per traffic **level**, using visitor thresholds within a cool-down window. Its documentation calls a difficulty factor of 5000 the lowest advisable value [V].
- The documentation I read doesn't describe its replay or GPU handling.

**Friendly Captcha (`friendly-pow`)**
- It uses BLAKE2b-256 with puzzles signed by the server. A puzzle buffer holds a timestamp, account and app IDs, an expiry (in 300 s units), the number of solutions and the difficulty.
- The client finds **n solutions at a lower threshold to reduce variance and to drive a progress bar**.
- The verifier checks expiry and **whether the puzzle has been submitted before** [V].
- The client also reports its solve time as diagnostic data.

**hackint/hashcash (IRC)**
- It is a SHA-256 PoW front end for anonymous account creation on Atheme IRC services. It is direct precedent for PoW-gated chat accounts [V, repo description].

### 3.4 Local measurements [M]

These were taken with a throwaway local benchmark: `sha2 0.10.9`, `argon2 0.5.3`, `equix 0.8.1`, and Python's `hashlib.scrypt`.

| Primitive | M2, 1 thread | Notes |
|---|---|---|
| SHA-256, `sha2` crate, cloned midstate | **5.6 MH/s** | 8 threads: **30.6 MH/s** (measured on the solver). OpenSSL `speed` on 64-byte inputs gives 8.4 MH/s per core. |
| Argon2id, m=4 MiB, t=1 | 1.6 ms/hash | |
| Argon2id, m=8 MiB, t=1 | 3.2 ms/hash | |
| Argon2id, m=19 MiB, t=2 (`Argon2::default`, Anubis) | 15.5 ms/hash | |
| Argon2id, m=64 MiB, t=1 / t=3 | 31.4 / 96.4 ms/hash | |
| scrypt N=16384, r=8 (16 MiB) | 28.3 ms/hash (35 H/s) | Used for the GPU comparison below. |
| Equi-X (`equix` crate) | 7.2 ms per solve call; **47 µs per verify** | About 2 solutions per call, per the upstream README. |

**GPU comparison** (hashcat 6.2.6 on an RTX 4090 [V])
- **SHA2-256: 21,975.5 MH/s.** That is about 2,600–3,900× one M2 core, and about 720× a whole 8-core M2.
- **scrypt (16 MiB): 7,126 H/s.** That is about 200× one M2 core, and about 25–35× the whole machine [I: the 8-core rate is extrapolated].
- So a 16 MiB memory-hard function cuts the GPU advantage by roughly 20× compared with SHA-256, but does not remove it. Equi-X and HashWX target close to parity [V: design claims].

---

## 4. Threat model

### 4.1 Actors

| Actor | Wants | Volume |
|---|---|---|
| Legitimate agent | One account, created unattended, in seconds to a minute. | 1, or a few per operator. |
| Fleet operator | Tens to hundreds of agent accounts from one host. | Medium, from one IP. |
| Spam bot operator | Many accounts to send spam or phishing across the federation. | Thousands or more, from many IPs (residential proxies, botnets, cloud). |
| Multi-account abuser | Sock puppets, ban evasion in group chats, harassment, squatting usernames, command-and-control channels. | Tens to hundreds. |
| Resource attacker | To fill storage or stall registration. | Request floods. |

**Assets at stake:**
- the server's federation reputation (other servers and RTBLs can block the whole domain);
- other users' inboxes;
- server CPU, RAM and storage;
- the username namespace.

### 4.2 What PoW does and doesn't do [I]

**PoW does:**
- set a floor on the per-account cost, turning free mass registration into a compute bill;
- rate-limit a single machine (one M2 at the default difficulty manages about 400 accounts per hour);
- give the operator a difficulty knob that can be turned **during** an attack without deploying CAPTCHAs or changing clients;
- make pre-computation useless, because every challenge contains server randomness and is bound to a username and domain.

**PoW does not:**
- show that the registrant is an agent (or a human);
- stop **GPU attackers** when the algorithm is SHA-256. One RTX 4090 solves a default challenge in about 12 ms, which is about 80 accounts per second, or about 300,000 per hour, if nothing else limits it. Even at the cap (2^34) it is about 0.8 s per account;
- stop **botnets**, whose work is done on victims' machines;
- stop **low-volume, targeted abuse**, where a single account is cheap at any reasonable difficulty;
- prevent abuse **after** registration. The account itself is the asset.

**Rough cost to an attacker** (default difficulty, 2^28 hashes) [I, derived from §3.4]:

| Hardware | Time per account | Accounts per hour |
|---|---|---|
| 1 M2-class core | ~48 s | ~75 |
| 8-core laptop | ~9 s | ~400 |
| RTX 4090 | ~12 ms | ~300,000 |

With Equi-X at a matched CPU cost, the GPU row would drop to roughly CPU level [I, from Tor's and HashWX's design claims].

**Conclusion.** What actually limits a capable attacker is the number of IP addresses they control plus the restrictions placed on new accounts. PoW adds cost on top, makes naive scripts fail, and provides the difficulty knob. The design must not depend on PoW alone.

### 4.3 Complementary limits (recommended, in priority order)

1. **Throttle by IP and prefix.**
   - Keep `mod_register_limits` with a real throttle and a larger cache, for example `registration_throttle_max = 5`, `registration_throttle_period = "1h"`, `registration_throttle_cache_size = 10000`.
   - `mod_register_pow` adds escalation per **prefix** (IPv4 /32, IPv6 /56), because exact-IP keys are trivially evaded on IPv6 (§5.6).
2. **A probation role for new PoW accounts** (`register_pow_role`, for example `pow:probation`, defined with `custom_roles`), with:
   - no room creation, uploads or invite creation, by not granting `:create-room`, `:upload` and `:invite-users`;
   - **a send limit to strangers**: at most 5 messages per minute and at most 50 distinct new recipients per day to JIDs that aren't in the roster;
   - at most 20 outbound subscription requests per day.

   These limits are enforced by hooks on `pre-message/*` and `pre-presence/*`; see §6.6. **Caveat:** room creation and upload are handled by **component** hosts, which assign roles to users of the parent host through `host_user_role`. A per-account probation role may therefore not reach them. Verify this during implementation [I].
3. **Account aging.** Promote an account to `prosody:registered` after 7 days if it has logged in at least once (`mod_account_activity`) and has no spam reports against it. Delete accounts that never logged in after 7 days. Enable `mod_tombstones` so deleted names can't be registered again.
4. **A separate VirtualHost for self-registered agents**, for example `agents.example.org`. If federation partners or RTBLs block the domain, human users on the main domain aren't affected. Stricter limits can also be set per host [I].
5. **Detection and reporting:**
   - `mod_watchregistrations`, which alerts admins;
   - `mod_spam_reporting` with `mod_watch_spam_reports` (XEP-0377);
   - the `register_pow_*` metrics (§6.7);
   - optionally `mod_register_dnsbl`, to check the registering IP against blocklists.
6. **Invites for fleets.** Operators who need many accounts should use XEP-0401/0445 invites (`mod_invites`). Invites skip PoW, so honest bulk use never has to pay escalated difficulty.

---

## 5. Algorithm, parameters and challenge protocol

### 5.1 Algorithm choice

| | SHA-256 hashcash | Argon2id puzzle (Anubis-style) | Equi-X (Tor) |
|---|---|---|---|
| Cost per attempt, 1 M2 thread [M] | 0.18 µs | 1.6 ms (4 MiB) to 15.5 ms (19 MiB) | 7.2 ms per call (about 2 candidate solutions) |
| Server verification cost | ~µs for 8 hashes + HMAC [I, from rate] | **One full Argon2id, 1.6–15.5 ms plus 4–19 MiB, blocking Prosody's single Lua thread** [M + I] | 47 µs [M] |
| GPU advantage | Very large: about 2,600–3,900× per core [V+M] | Reduced. Memory-bandwidth-bound; compare scrypt-16MiB at about 200× per core [V+M+I] | Designed to be minimal [V]. HashWX: GPU close to parity [V] |
| Prosody dependency | **None**: `util.hashes.sha256` [V] | C binding: `lua-argon2` plus libargon2 [V] | **None exists.** A Lua C binding to tevador's or Tor's equix would have to be written [I] |
| Rust client crate | `sha2` (MIT/Apache-2.0, already in `Cargo.lock`) [V] | `argon2` (MIT/Apache-2.0) [V] | `equix` 0.8.1 (**LGPL-3.0-only**, Tor/Arti) [V] |
| Ease of specifying and interoperating | Trivial; XEP-0158 is precedent | Parameters must be fixed in the spec | Must reference an external algorithm spec; upstream has deprecated it in favor of HashWX [V] |

**Recommendation** [I]

1. **v1: `sha256` only, and mandatory to implement.**
   - It needs no native dependencies on either side.
   - Verification takes microseconds inside Prosody's single-threaded event loop.
   - It is trivial to specify and to write test vectors for.
   - It gets the whole flow (protocol, module, CLI, limits) into production quickly.
   - Its weakness against GPUs is accepted and offset by §4.3.
2. **The protocol names the algorithm** (`algorithm` field, carried inside the signed token). That leaves room for a later `equix` or `hashwx` algorithm, issued only by servers that have the native binding. The client side of Equi-X is ready today (`equix` crate). The server side needs a small Lua C module, roughly 150 lines plus packaging [I].
3. **Don't use Argon2id for in-process verification in Prosody.** Each verification blocks the event loop for milliseconds and allocates megabytes. Junk submissions would turn that into an amplification attack unless every token were consumed before verification. Revisit only if verification moves out of process (see §10.3).

### 5.2 Parameters (defaults)

| Parameter | Default | Rationale |
|---|---|---|
| `count` (k sub-puzzles) | 8 | See below the table. |
| `bits` (leading zero bits per sub-puzzle) | 25 | See below the table. |
| Escalation per prefix | +1 bit for each successful PoW registration from the same prefix in the last 24 h, capped at +6 (64×) | Fleet operators pay ×2, ×4, …; mass registration from one prefix grows exponentially. Combined with the hard throttle. |
| Global load escalation | 0 below T successful registrations per hour (default T = 30). Above it, +⌊log2(R/T)⌋+1, capped at +4 | Like mCaptcha's levels and Tor's suggested effort. It is fair across clients, but legitimate users pay too during attacks. |
| `max_bits` | 31 (8 × 2^31 = 2^34 expected hashes) | About 9.4 min on an M2 with 8 threads, about 51 min on 1 thread [M-derived]. |
| Token TTL | 1 h | It must exceed the worst-case solve time at `max_bits`. It also bounds how long the replay cache keeps entries. |
| IP binding | on; IPv4 /32, IPv6 /56 | Needed so that per-prefix difficulty can't be dodged by fetching from a cold IP and submitting from a hot one. /56 is a common ISP delegation size for IPv6 [I]. |

**Why 8 sub-puzzles.** The expected total work stays k × 2^bits, but the variance falls. Measured by simulation, with time expressed as a multiple of the mean [I]:

| k | p95 | p99 |
|---|---|---|
| 1 | 3.0× | 4.6× |
| 8 | 1.64× | 2.0× |

It also gives a natural "3 of 8" progress signal, as in Friendly Captcha.

**Why 25 bits.** 8 × 2^25 = **2^28 ≈ 2.7e8** expected hashes. On an M2 that's about **8.8 s with 8 threads** (30.6 MH/s) and about **48 s single-threaded** [M]. A real 8-thread solve of the test vector in §5.7 took 5.8 s, a lucky run at 178M hashes [M]. Server verification is 8 SHA-256 plus 1 HMAC.

**Target and budget.**
- The target is about 10 s on a typical 2020s laptop using all cores, and under a minute on a single core.
- Agents on small VMs (1–2 vCPU, no SHA extensions) may take a few minutes [I: not measured].
- The CLI estimates the solve time before starting and refuses up front if the estimate exceeds its timeout (§7.4).

**Effective bits** = `min(max_bits, bits + prefix_extra + load_extra)`. The value is computed when the challenge is issued and is signed into the token.

**At submission**, the server also checks that `token.bits ≥ bits + prefix_extra(now)`. This blocks the trick of fetching many cheap tokens before registering in bulk from one prefix. The check deliberately ignores `load_extra(now)`, so a legitimate client whose token was issued before a load spike isn't failed [I].

### 5.3 Namespace and fields

- **Namespace:** `urn:xmpp:ibr-pow:0`. This is a **provisional** name in the XSF style. The `:0` suffix marks it as experimental, and the final name is assigned if a ProtoXEP is accepted. If the project doesn't intend to submit one, use a URI under a domain the project controls instead. See §11.
- **Fields.** All fields live in the existing `jabber:iq:register` form and are named in Clark notation, per XEP-0068 [V].

| Field var | Type | Direction | Meaning |
|---|---|---|---|
| `{urn:xmpp:ibr-pow:0}challenge` | hidden | server → client → server | Opaque signed token (§5.4). The client MUST echo it unchanged. |
| `{urn:xmpp:ibr-pow:0}algorithm` | hidden | server → client | `sha256` in v1. A client MUST abort if it doesn't recognize the value. |
| `{urn:xmpp:ibr-pow:0}bits` | hidden | server → client | Leading zero bits per sub-puzzle. |
| `{urn:xmpp:ibr-pow:0}count` | hidden | server → client | Number of sub-puzzles, k. |
| `{urn:xmpp:ibr-pow:0}expires` | hidden | server → client | XEP-0082 timestamp; informational. |
| `{urn:xmpp:ibr-pow:0}solution` | text-single, required | client → server | k nonces, each 16 lowercase hex characters, comma-separated, in index order. |

Some notes on these fields:
- The client reads the parameters from the separate fields, but the server trusts **only** the token. If the two ever disagreed, the solution would simply fail.
- Hidden fields that the client returns, other than `challenge`, are ignored.
- A human using a generic client (Gajim, for example) sees an unfillable required "solution" field, plus `<instructions/>` explaining what to do. That is the intended failure mode.
- A server MAY also advertise the stream feature `<register xmlns='urn:xmpp:ibr-pow:0'/>` so clients can learn about PoW support before requesting the form. Clients MUST NOT rely on it, because the form is authoritative.

### 5.4 Challenge token (stateless, HMAC-signed)

```
payload = "1" ":" alg ":" bits ":" count ":" iat ":" exp ":" rnd
          ; iat, exp are Unix seconds; rnd is 16 random bytes as 32 lowercase hex characters
ipbind  = "" if IP binding is off,
          else the truncated prefix in text form plus "/" plus the length, e.g. "203.0.113.7/32", "2001:db8:1200::/56"
tag     = lowercase-hex( HMAC-SHA256( K, "urn:xmpp:ibr-pow:0" 0x00 domain 0x00 ipbind 0x00 payload ) )[0..32]
token   = payload ":" tag
```

**Server key `K`**
- `K` comes from `register_pow_secret`. If that option is unset, `K` is 32 random bytes generated at module load.
- A random key has a useful side effect: a restart invalidates every outstanding token, so losing the in-memory replay cache on restart is harmless. Anubis does the same with its signing key [V].
- A fixed secret is needed if challenges must survive restarts, or if several servers share one domain.

**Interpreting the token**
- The token is **opaque** to clients. Only the issuing server interprets it.
- `ipbind` is **not** carried in the token. The server recomputes it from the submitting connection's address. A client whose prefix changed simply gets `challenge-invalid` and fetches a new challenge.
- An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) MUST be treated as IPv4 [I].

### 5.5 The puzzle

```
P      = "urn:xmpp:ibr-pow:0"                         ; personalization string
D      = server domain (A-label, lowercase), i.e. the registering VirtualHost
U      = requested username after nodeprep (UTF-8)
prefix = P 0x00 token 0x00 D 0x00 U 0x00
for i in 0 .. k-1:
    N_i = 8 bytes chosen by the client (sent as 16 hex characters; the big-endian bytes are hashed)
    H_i = SHA-256( prefix || BYTE(i) || N_i )
    valid_i  <=>  H_i begins with at least `bits` zero bits (most significant bit of byte 0 first)
solution valid <=> valid_0 and ... and valid_{k-1}
```

- **What the inputs bind.**
  - The token, `D` and `U` bind a solution to one challenge, one server and one username.
  - Putting the index `i` in the hash makes the k sub-puzzles independent.
  - The client can absorb `prefix || i` once as a SHA-256 midstate, so each attempt costs one compression of the final block [I].
- **Normalizing the username.**
  - The server hashes `nodeprep(submitted username)`.
  - By default the CLI only generates or accepts usernames matching `^[a-z0-9][a-z0-9._-]{0,63}$`, which nodeprep leaves unchanged.
  - Otherwise it runs nodeprep itself, using the `stringprep` crate, which is already in `Cargo.lock` [V].

### 5.6 Server verification order (cheapest checks first)

1. If the session isn't `c2s_unauthed`, or encryption is required and the session isn't secure, **fall through** (return nil). `mod_register_ibr` then sends its usual error.
2. Parse the `jabber:x:data` submit form. Extract `username`, `challenge` and `solution`.
3. If `session.validated_invite` is set and `register_pow_exempt_invites` is true, fall through. The invite path (XEP-0445) skips PoW.
4. If `challenge` or `solution` is missing, return `pow-required`.
5. Parse the token and check that its version and algorithm are supported.
   - If `now > exp`, or `iat > now + 60`, return `challenge-expired`.
6. Recompute `tag` for (D, current ipbind, payload) and compare with `util.hashes.equals` in constant time. On a mismatch, return `challenge-invalid`.
7. If the tag is in the spent cache, return `challenge-used`.
8. If `bits` is less than the per-prefix requirement now, return `challenge-outdated` (§5.2).
9. Apply nodeprep to the username. If that fails, return `not-acceptable` ("invalid username").
10. Check that there are exactly k nonces, each 16 hex characters, and that every `H_i` passes. Otherwise return `solution-invalid`.
11. Record `session.ibr_pow = { username = U, tag = tag, bits = bits }` and **return nil**. `mod_register_ibr` continues with the same stanza: `user-registering` (limits, invites, tombstones, our own check), then account creation, then `user-registered`.
12. On `user-registered`, add `tag` to the spent cache, which is an LRU with entries implicitly valid until `exp`. Then bump the prefix and global counters and record audit data.

**Replay protection** has three layers:
- the spent-token cache;
- the binding of each solution to a username (a replayed solution can only target the same username, which now exists, so it gets `conflict`);
- short token lifetimes, with key rotation on restart.

Tokens are marked spent **on success, not on attempt**. A client that hits `wait`/`policy-violation` from the throttle can retry the same solved token later, within its TTL. That is safe because verification is cheap. An expensive algorithm would need to be consumed on attempt instead.

**Error conditions.** All of these are sent by `mod_register_pow` itself, with an application-specific condition element. `util.error` objects with `extra = { namespace, condition }` render that element [V: `stanza_mt.add_error`]. Errors raised later by `user-registering` lose the element, because `mod_register_ibr` rebuilds them from type, condition and text only [V].

| App condition (`urn:xmpp:ibr-pow:0`) | Stanza error | Client action |
|---|---|---|
| `pow-required` | modify / not-acceptable | Fetch the form again. The client probably skipped the fields. |
| `unsupported-algorithm` | modify / not-acceptable | Abort (exit 5). |
| `challenge-expired`, `challenge-invalid`, `challenge-outdated` | modify / not-acceptable | Fetch a new form and solve again. Retry automatically once. |
| `challenge-used` | modify / not-acceptable | Fetch a new form. |
| `solution-invalid` | modify / not-acceptable | This is a client bug. Abort with a diagnostic. |
| *(from `mod_register_limits`)* | wait / policy-violation, "Too many registrations…" | Abort with exit 5. The token stays valid until `exp`. |
| *(from `mod_register_ibr`)* | cancel / conflict | Username taken. With a generated username, generate a new one and solve again once. Otherwise exit 4. |

### 5.7 Stanza examples

These are complete, real values.
- Test-vector inputs:
  - `K = "example-secret-do-not-use"`;
  - domain `example.org`;
  - client IP `203.0.113.7`, so ipbind is `203.0.113.7/32`;
  - username `agent-7k2m9qx4td`;
  - `iat` = 2026-11-10T16:00:00Z, `exp` = 17:00:00Z.
- I computed the tag with Python's `hmac`, solved with a Rust `sha2` solver, and re-verified with an independent Python verifier [M].
- The implementation should ship these values as shared test vectors for both the Lua and the Rust test suites.

**1. Stream features after STARTTLS** (abridged):

```xml
<stream:features>
  <mechanisms xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>
    <mechanism>SCRAM-SHA-1</mechanism>
    <!-- … -->
  </mechanisms>
  <register xmlns='http://jabber.org/features/iq-register'/>
  <register xmlns='urn:xmpp:ibr-token:0'/>   <!-- only if mod_invites_register is loaded -->
  <register xmlns='urn:xmpp:ibr-pow:0'/>     <!-- optional hint, see §5.3 -->
</stream:features>
```

**2. The client requests the registration form:**

```xml
<iq type='get' id='reg1' to='example.org'>
  <query xmlns='jabber:iq:register'/>
</iq>
```

**3. The server returns the form with a challenge.** The legacy `<username/>` and `<password/>` elements are deliberately left out, so clients that only support legacy registration see only the instructions:

```xml
<iq type='result' id='reg1' from='example.org'>
  <query xmlns='jabber:iq:register'>
    <instructions>This server requires a proof of work to register. Use a client that supports urn:xmpp:ibr-pow:0, or ask the administrator for an invitation.</instructions>
    <x xmlns='jabber:x:data' type='form'>
      <title>Creating a new account</title>
      <instructions>Choose a username and password. Your client must also solve the proof-of-work challenge.</instructions>
      <field var='FORM_TYPE' type='hidden'><value>jabber:iq:register</value></field>
      <field var='username' type='text-single' label='Username'><required/></field>
      <field var='password' type='text-private' label='Password'><required/></field>
      <field var='{urn:xmpp:ibr-pow:0}challenge' type='hidden'>
        <value>1:sha256:25:8:1794326400:1794330000:9f2c4e7a1b3d5f60718293a4b5c6d7e8:6b8ae478ff3592e715741262396837c1</value>
      </field>
      <field var='{urn:xmpp:ibr-pow:0}algorithm' type='hidden'><value>sha256</value></field>
      <field var='{urn:xmpp:ibr-pow:0}bits' type='hidden'><value>25</value></field>
      <field var='{urn:xmpp:ibr-pow:0}count' type='hidden'><value>8</value></field>
      <field var='{urn:xmpp:ibr-pow:0}expires' type='hidden'><value>2026-11-10T17:00:00Z</value></field>
      <field var='{urn:xmpp:ibr-pow:0}solution' type='text-single'
             label='Proof of work (computed automatically by a supporting client)'><required/></field>
    </x>
  </query>
</iq>
```

**4. The client submits the solution.** The hex nonces below are real solutions for this token, username and domain:

```xml
<iq type='set' id='reg2' to='example.org'>
  <query xmlns='jabber:iq:register'>
    <x xmlns='jabber:x:data' type='submit'>
      <field var='FORM_TYPE'><value>jabber:iq:register</value></field>
      <field var='username'><value>agent-7k2m9qx4td</value></field>
      <field var='password'><value>(32 random bytes, base64url, generated by the CLI)</value></field>
      <field var='{urn:xmpp:ibr-pow:0}challenge'>
        <value>1:sha256:25:8:1794326400:1794330000:9f2c4e7a1b3d5f60718293a4b5c6d7e8:6b8ae478ff3592e715741262396837c1</value>
      </field>
      <field var='{urn:xmpp:ibr-pow:0}solution'>
        <value>0000000001dee49f,000000000212fd53,00000000006093ad,000000000016321f,00000000002aa9f1,0000000003f1a352,00000000022ca4ce,000000000014a0b1</value>
      </field>
    </x>
  </query>
</iq>
```

For reference, `H_0 = SHA-256(prefix || 0x00 || 0x0000000001dee49f)` starts with `0000005a6294cc6c…`, i.e. 25 leading zero bits [M]. Changing one character of the username makes the same nonce hash to `1f81d249…`, which fails.

**5. Success** (the account now exists, and the client continues with normal SASL login or closes the stream):

```xml
<iq type='result' id='reg2' from='example.org'/>
```

**6. Example errors:**

```xml
<!-- challenge expired: fetch a new form -->
<iq type='error' id='reg2' from='example.org'>
  <error type='modify'>
    <not-acceptable xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/>
    <text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'>Proof-of-work challenge expired; request a new registration form.</text>
    <challenge-expired xmlns='urn:xmpp:ibr-pow:0'/>
  </error>
</iq>

<!-- per-IP throttle (mod_register_limits, unchanged) -->
<iq type='error' id='reg2' from='example.org'>
  <error type='wait'>
    <policy-violation xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'/>
    <text xmlns='urn:ietf:params:xml:ns:xmpp-stanzas'>Too many registrations from this IP address recently</text>
  </error>
</iq>
```

**7. Using an invite instead (XEP-0445, no PoW).** The client sends `<iq type='set'><preauth xmlns='urn:xmpp:pars:0' token='…'/></iq>` first, then makes the same form request. The server MAY still include the PoW fields in the form, but it doesn't require them, because `session.validated_invite` is set.

### 5.8 Should this be a ProtoXEP?

**Yes, once v1 has run end to end against Prosody** [I]. Reasons:
- XEP-0158 §6.2 shows that the XSF already accepts hashcash in registration forms, but it is too underspecified to implement interoperably (§3.1).
- XEP-0389 lists PoW as a goal and never defines it.
- Agent self-registration is a use case that other server implementations (ejabberd, Openfire, and others) will want too.

**Proposed contents:**
1. The data-form binding (this section).
2. The exact byte layout and test vectors.
3. A registry of algorithms (`sha256` mandatory; `equix` and `hashwx` optional).
4. Error conditions.
5. Security considerations: GPU asymmetry, IP binding, replay, privacy of IP prefixes.
6. A mapping onto XEP-0389 as a challenge type, for whenever XEP-0389 revives.

Before submitting, ask on the standards list whether to frame it as a new XEP-0158 challenge type registered with the Registrar or as a standalone XEP. The standalone route is cleaner, because XEP-0158 challenges are tied to its CAPTCHA wrapper and `answers` semantics.

---

## 6. Server side: `mod_register_pow` (Prosody 13, Lua)

### 6.1 Placement and distribution

- The module lives in this repo at `server/prosody/mod_register_pow/` (`mod_register_pow.lua` plus a `README.md` in the prosody-modules format).
- Prosody's plugin loader finds `mod_<name>/mod_<name>.lua` inside any directory on `plugin_paths` [V].
- Once it is stable, submit it to the prosody-modules community repository so operators can run `prosodyctl install` [I].

### 6.2 Events hooked

| Event | Priority | Purpose |
|---|---|---|
| `stanza/iq/jabber:iq:register:query` | **10** (runs before `mod_register_ibr` at 0) | On GET, answer with the full form plus a fresh challenge and return `true`. On SET, verify (§5.6), then either send an error and return `true`, or return `nil` so that `mod_register_ibr` proceeds. |
| `user-registering` | 10 | A second line of defense. If `event.session` is `c2s_unauthed`, has no `validated_invite`, and `session.ibr_pow.username ~= event.username`, set `allowed = false`. Optionally set `event.role = register_pow_role`. Registrations that don't come through a c2s session (web, admin, API) are ignored. |
| `user-registered` | default | Mark the token spent, update the prefix and load counters, write an audit record, add the user to the probation list. |
| `stream-features` | default | Optional `<register xmlns='urn:xmpp:ibr-pow:0'/>` hint, offered only to unauthenticated sessions with encryption, as `mod_register_ibr` does. |
| `module:daily()` | n/a | Promotion and aging (§6.6). |

**Coupling with `mod_register_ibr`.**
- Because the GET form in `mod_register_ibr` is a private static value with no extension hook, `mod_register_pow` **builds the form itself**. It re-reads `additional_registration_fields`, `registration_title` and `registration_instructions` with the same option names.
- This duplicates logic and ties us to `mod_register_ibr`'s internals [V that no hook exists; I that this is the best option].
- The cleaner fix is a small upstream patch to `mod_register_ibr` with two parts:
  - fire an event such as `register-form` carrying `{ session, form_stanza }` before replying, so modules can add per-request fields;
  - pass the raw `stanza`, or the parsed form, inside `user-registering`.
- Raise this with the Prosody developers. It would also help `mod_captcha_registration`, which currently replaces `mod_register` outright [V].

### 6.3 Coexistence with other modules

- **`mod_register` and `mod_register_ibr`:** require `allow_registration = true`. They are unchanged: `mod_register_ibr` still creates the account, applies the role, stores `account_details`, and fires `user-registered`.
- **`mod_register_limits`:** unchanged. It runs on `user-registering` after our stanza hook, so throttling applies only to submissions that already carry a valid PoW.
- **`mod_invites_register`:** set `registration_invite_only = false` (its default is `true` [V]), otherwise every registration without an invite is refused. Invite holders skip PoW when `register_pow_exempt_invites = true`.
- **`mod_tombstones`, `mod_mimicking`, `mod_block_registrations`:** unchanged, all on `user-registering`.
- **`mod_captcha_registration`:** incompatible, because it replaces `mod_register`. Don't load both.

### 6.4 Configuration (proposed)

```lua
-- Per VirtualHost
modules_enabled = { "register", "register_pow" }
allow_registration = true
registration_invite_only = false          -- only if mod_invites_register is loaded

register_pow_algorithm = "sha256"
register_pow_bits = 25                    -- per sub-puzzle
register_pow_count = 8
register_pow_max_bits = 31
register_pow_ttl = "1h"
register_pow_secret = nil                 -- nil: random per start (restart invalidates challenges)
register_pow_bind_ip = true
register_pow_ipv4_prefix = 32             -- multiples of 8 (util.ip.truncate)
register_pow_ipv6_prefix = 56
register_pow_prefix_window = "24h"
register_pow_prefix_escalation_cap = 6
register_pow_load_threshold = 30          -- successful registrations per hour before global escalation
register_pow_load_escalation_cap = 4
register_pow_replay_cache_size = 10000
register_pow_exempt_invites = true
register_pow_instructions = "This server requires a proof of work to register…"

-- Probation (optional; see §6.6)
register_pow_role = "pow:probation"       -- nil = keep register_ibr_default_role
register_pow_promote_after = "7d"
register_pow_promote_to = "prosody:registered"
register_pow_delete_unused_after = "7d"
register_pow_probation_unsolicited_per_minute = 5
register_pow_probation_new_contacts_per_day = 50
register_pow_probation_subscriptions_per_day = 20
custom_roles = { { name = "pow:probation"; inherits = { "prosody:guest" }; priority = 20 } }

-- Recommended alongside
registration_throttle_max = 5
registration_throttle_period = "1h"
registration_throttle_cache_size = 10000
```

### 6.5 Sketch of the core

This is illustrative Lua. It has not been run.

```lua
local hashes   = require "prosody.util.hashes";
local random   = require "prosody.util.random";
local errors   = require "prosody.util.error";
local ip_util  = require "prosody.util.ip";
local nodeprep = require "prosody.util.encodings".stringprep.nodeprep;

local NS = "urn:xmpp:ibr-pow:0";
local secret = module:get_option_string("register_pow_secret") or random.bytes(32);
local spent = require "prosody.util.cache".new(module:get_option_integer("register_pow_replay_cache_size", 10000, 100));

local function ipbind(session)            -- "" when binding is off
	if not bind_ip or not session.ip then return "" end
	local ip = ip_util.new_ip(session.ip);  -- (map ::ffff:a.b.c.d to IPv4 first)
	local len = ip.proto == "IPv6" and v6_len or v4_len;
	return ip_util.truncate(ip, len).normal .. "/" .. len;
end

local function sign(payload, session)
	return hashes.hmac_sha256(secret, NS.."\0"..module.host.."\0"..ipbind(session).."\0"..payload, true):sub(1, 32);
end

local function leading_zero_bits_ok(h, bits)  -- h: raw 32-byte digest
	local full, rem = math.floor(bits / 8), bits % 8;
	for i = 1, full do if h:byte(i) ~= 0 then return false end end
	return rem == 0 or h:byte(full + 1) < 2^(8 - rem);
end

local function pow_error(condition, text)
	return errors.new({ type = "modify", condition = "not-acceptable", text = text,
		extra = { namespace = NS, condition = condition } });
end

module:hook("stanza/iq/jabber:iq:register:query", function (event)
	local session, stanza = event.origin, event.stanza;
	if session.type ~= "c2s_unauthed" or (require_encryption and not session.secure) then
		return; -- let mod_register_ibr produce its usual error
	end
	if stanza.attr.type == "get" then
		session.send(st.reply(stanza):add_child(build_query(session))); -- form + new token
		return true;
	end
	-- … parse the form, apply the invite exemption, run steps 4–10 of §5.6, using
	--   hashes.equals(tag, sign(payload, session)) and
	--   hashes.sha256(prefix .. string.char(i) .. nonce_bytes)
	-- on failure: session.send(st.error_reply(stanza, pow_error("challenge-expired", "…"))); return true;
	session.ibr_pow = { username = username, tag = tag, bits = bits };
	-- return nil: mod_register_ibr (priority 0) handles the same stanza next
end, 10);
```

### 6.6 Probation, aging and limits on new accounts

- **On `user-registered`, when the registration came through PoW:**
  - The account gets `register_pow_role`, set through `event.role` in `user-registering` [V: `mod_register_ibr` passes `user.role` to `create_user_with_role`].
  - It is added to a `register_pow` map store with `{ registered_at, bits, ip_prefix }`.
  - The store is kept by username, so the daily job doesn't have to walk every user.
- **Daily job (`module:daily`):**
  - Promote an account (`usermanager.set_user_role`) when it is older than `promote_after` and has a recorded login in `mod_account_activity`.
  - Delete an account (and tombstone it, if `mod_tombstones` is loaded) when it is older than `delete_unused_after` and has never logged in.
- **Send limits.**
  - Hook `pre-message/bare`, `pre-message/full` and `pre-presence/bare` (outbound `subscribe`) at high priority.
  - When `origin.role.name == register_pow_role` and the recipient isn't in the sender's roster and isn't a joined MUC, apply a per-account `util.throttle`.
  - Over the limit, bounce `wait`/`policy-violation` with the text "New accounts are rate-limited".
  - Hooks of this kind are how `mod_blocklist` and `mod_carbons` handle outbound c2s messages [V].
  - This could be a separate small module, `mod_probation_limits`, so it also covers accounts created by other routes [I].

### 6.7 Observability and privacy

- **Metrics:**
  - `register_pow_challenges_issued`;
  - `register_pow_verify_result{result=…}`;
  - a histogram of bits issued;
  - `register_pow_registrations`;
  - the current load level.
- **Logs:** at `info`, record the username, bits and IP prefix. Never log passwords or solutions.
- **Stored IP prefixes count as personal data.** Keep them for at most 30 days, configurable. See the open questions in §11.

---

## 7. Client side: Rust daemon and CLI

### 7.1 Where the code lives

- Registration needs **no existing account**, so it doesn't go through the running daemon.
- The core is a library function in `crates/daemon/src/register.rs`, called directly by the CLI. The `cli` crate already depends on `agent-chat-daemon` [V: `crates/cli/Cargo.toml`].
- After success, the CLI writes the profile's `config.toml` and the Keychain entry, then starts the daemon as usual. `--no-start` skips that step.
- The GUI depends only on `proto`, so later it would need an IPC request `RegisterAccount` with progress events. That is out of scope for v1.

### 7.2 CLI surface (proposed)

```
agent-chat account register --server DOMAIN
    [--username NAME]           # default: agent-<10 random base32 chars>
    [--password-stdin]          # default: generate 32 random bytes, base64url
    [--invite URI]              # xmpp:DOMAIN?register;preauth=TOKEN → XEP-0445; no PoW
    [--threads N]               # default: available_parallelism()
    [--timeout SECS]            # overall budget, default 900
    [--no-start]                # don't start the daemon afterwards
    [--insecure-plaintext]      # allowed only for localhost/127.0.0.1 (same rule as the plan)
```

**Exit codes.** These are the plan's codes plus two new ones:

| Code | Meaning |
|---|---|
| 0 | registered |
| 1 | error (network, protocol, unsupported form) |
| 2 | timeout (estimated or actual solve time exceeds `--timeout`) |
| **4** | username taken (when `--username` was given) |
| **5** | refused by server policy (throttled, blocklisted, invite-only, registration disabled, unsupported algorithm) |
| 130 | interrupted |

### 7.3 Flow

1. **Connect.**
   - Use `StartTlsServerConnector(DnsConfig::srv_default_client(domain))`, or `TcpServerConnector(DnsConfig::addr("127.0.0.1:5222"))` with `--insecure-plaintext` for localhost.
   - Call `ServerConnector::connect(&domain_jid, ns::JABBER_CLIENT, Timeouts::default())`. It returns a stream that is "ready to login", i.e. TLS is up and SASL has not run [V].
2. **Read the stream features.**
   - Call `recv_features::<XmppStreamElement>()` [V].
   - If `features.in_band_registration` is false and no invite was given, stop with exit 5 and the error `registration_not_offered`.
3. **Use the invite first, if one was given.** With `--invite`, send an IQ carrying a `preauth` element and expect an empty result (XEP-0445).
4. **Request the form.**
   - Send an IQ get with `xmpp_parsers::ibr::FieldsQuery` and parse the result as `ibr::FormQuery` [V].
   - xso discards unknown children by default (unless the `pedantic` feature is on), so the legacy `<username/>` elements Prosody sends don't break parsing [V].
5. **Classify the form.**
   - With PoW fields, read the parameters.
   - Without PoW fields, fall back to plain registration.
   - If there are required fields we don't recognize (a CAPTCHA, for example), stop with exit 1, error `unsupported_form`, and the list of field names.
   - With an unrecognized `algorithm`, stop with exit 5.
6. **Estimate the time.**
   - Run about 300 ms of real solving to measure the hash rate, then compute `expected = count × 2^bits / rate`.
   - If `expected × 1.6` (the p95) exceeds the remaining budget, or exceeds the time left before `expires`, stop with exit 2 and report `estimated_seconds`.
7. **Solve.**
   - The solver runs in `tokio::task::spawn_blocking`, which drives `std::thread::scope` workers. Workers take strided nonces, share a stop `AtomicBool` and a hash-count `AtomicU64`, and work on the sub-puzzles one after another.
   - Meanwhile a `tokio::select!` watches:
     - solver completion;
     - Ctrl-C/SIGTERM, which sets the stop flag and exits 130;
     - the overall deadline, which sets the stop flag and exits 2;
     - a progress ticker every 5 s;
     - the XMPP stream. If the stream closes (Prosody's 5-minute `c2s_timeout` for unauthenticated connections [V], or a NAT drop), note that a reconnect is needed but **keep solving**. The token is stateless.
8. **Submit.**
   - Reconnect first if needed. Send an IQ set with `FormQuery { form: DataForm(submit) }`.
   - Map errors as in the table in §5.6:
     - `challenge-expired`, `challenge-invalid` or `challenge-outdated`: fetch a new form and solve again, at most once.
     - `conflict` with a generated username: generate a new name and solve again, at most once.
9. **Finish.**
   - On success, close the stream, store the password in the Keychain, and write the profile config.
   - Print the result JSON and exit 0.

**Sketch of the solver core** (illustrative):

```rust
fn solve_one(base: &Sha256, index: u8, bits: u32, threads: u64,
             stop: &AtomicBool, hashes: &AtomicU64) -> Option<u64> {
    let mut b = base.clone();            // midstate already holds `prefix`
    b.update([index]);
    let found = AtomicU64::new(u64::MAX);
    std::thread::scope(|s| {
        for t in 0..threads {
            let (b, found) = (b.clone(), &found);
            s.spawn(move || {
                let mut n = t;
                while found.load(Relaxed) == u64::MAX && !stop.load(Relaxed) {
                    for _ in 0..4096 {        // check flags about every 1 ms per thread
                        let mut h = b.clone();
                        h.update(n.to_be_bytes());
                        if leading_zero_bits(&h.finalize()) >= bits {
                            let _ = found.compare_exchange(u64::MAX, n, SeqCst, SeqCst);
                            break;
                        }
                        n += threads;
                    }
                    hashes.fetch_add(4096, Relaxed);
                }
            });
        }
    });
    let n = found.into_inner();
    (n != u64::MAX).then_some(n)
}
```

### 7.4 Progress, output and timeouts

- **stdout:** exactly one JSON object, as other commands do, so `agent-chat account register … | jq` works.
- **stderr:** JSON Lines with progress events. Agents that capture both streams (Claude Code's Bash tool, for example) can see these.

```json
{"event":"pow_challenge","server":"example.org","username":"agent-7k2m9qx4td","algorithm":"sha256","bits":25,"count":8,"expected_hashes":268435456,"threads":8,"estimated_seconds":9,"expires_at":"2026-11-10T17:00:00Z"}
{"event":"pow_progress","solved":3,"count":8,"hashes":101234567,"hash_rate":30600000,"elapsed_ms":3400,"eta_seconds":6}
```

```json
{"ok":true,"jid":"agent-7k2m9qx4td@example.org","profile":"default","password_stored":"keychain",
 "pow":{"algorithm":"sha256","bits":25,"count":8,"hashes":178644820,"elapsed_ms":5840,"threads":8},"invite_used":false}
```

```json
{"ok":false,"error":"throttled","message":"Too many registrations from this IP address recently","retryable":true,"exit_code":5}
```

**Rules:**
- The password is never printed. If no Keychain is available, the command fails unless `--password-stdin` was given, or a password file option is added (open question in §11).
- `--timeout` covers the whole operation: connecting, solving and submitting.

### 7.5 Crates

| Need | Crate | Notes |
|---|---|---|
| Hashing | `sha2` 0.10 | Already in `Cargo.lock` transitively, MIT/Apache-2.0 [V]. |
| nodeprep | `stringprep` 0.1 | Already in `Cargo.lock` [V]. |
| Randomness for usernames and passwords | `getrandom` (or `rand`) | Base32 and base64url encoding by hand, or with `data-encoding`, which is already in the lock [V]. |
| Stream and IQ handling | `tokio-xmpp` 6 (`connect`, `xmlstream`), `xmpp-parsers` 0.23 (`ibr`, `data_forms`, `iq`, `stanza_error`, `stream_features`) | Already dependencies of the daemon [V]. Check exact signatures when implementing. |
| Threads and cancellation | `std::thread::scope`, `std::sync::atomic`, `tokio::signal` | No `rayon` or `tokio-util` needed. |
| Later: Equi-X | `equix` 0.8 | LGPL-3.0-only; see §11. |

### 7.6 Tests

- **Unit tests:**
  - token parsing;
  - puzzle prefix construction;
  - the leading-zero check;
  - **the §5.7 test vector**, verified by Rust and by Lua (busted) from the same JSON fixture;
  - the solver at `bits = 8`;
  - mapping of form classification and errors to exit codes.
- **Integration tests** (feature `integration`, against the Docker Prosody):
  - successful registration followed by a normal login;
  - a username mismatch gives `solution-invalid`;
  - with `register_pow_ttl = "2s"`, `challenge-expired` triggers one automatic refetch;
  - registering, deleting the account with `prosodyctl`, then replaying the old token gives `challenge-used`;
  - with `registration_throttle_max = 1`, the second registration exits 5;
  - registering via `--invite` skips PoW;
  - registering against a server without the module uses plain registration.

---

## 8. Development setup (Docker Prosody)

**Files to add.** Proposed; none of these are created by this document.

```
server/prosody/mod_register_pow/mod_register_pow.lua
server/prosody/mod_register_pow/README.md
server/prosody/mod_register_pow/spec/test_vectors.json   # shared with Rust tests
dev/prosody/docker-compose.yml
dev/prosody/prosody.cfg.lua
dev/prosody/setup.sh
```

**`docker-compose.yml`** (excerpt):

```yaml
services:
  prosody:
    image: prosodyim/prosody:13.0        # confirm the tag; the image builds the prosody-13.0 package
    ports: ["127.0.0.1:5222:5222"]
    environment:
      AGENT_CHAT_POW_BITS: "10"          # set to 25 to feel the real cost
    volumes:
      - ./prosody.cfg.lua:/etc/prosody/prosody.cfg.lua:ro
      - ../../server/prosody/mod_register_pow:/etc/prosody/modules/mod_register_pow:ro
      - prosody-data:/var/lib/prosody
volumes:
  prosody-data:
```

**`prosody.cfg.lua`** (excerpt, on top of the modules the plan already lists):

```lua
plugin_paths = { "/etc/prosody/modules" }
c2s_require_encryption = false            -- dev only; the CLI permits plaintext only to localhost

VirtualHost "localhost"
  modules_enabled = { "register", "register_pow", "account_activity" }
  allow_registration = true
  registration_invite_only = false
  register_pow_bits = Lua.tonumber(ENV_AGENT_CHAT_POW_BITS) or 10   -- Lua.* and ENV_* as used by the official config
  register_pow_count = 4
  register_pow_secret = "dev-only-not-secret"   -- stable across restarts for repeatable tests
  register_pow_ttl = "10m"
  registration_throttle_max = 1000              -- every test connects from the same Docker gateway IP
  registration_throttle_period = "1m"
```

**Notes:**
- The Prosody 13 Docker config reads `ENV_*` variables and uses `Lua.tonumber` [V].
- With Docker Desktop on macOS, connections forwarded from the host reach Prosody from a Docker gateway address, not from `127.0.0.1`. So the default `registration_allowlist` (`127.0.0.1`, `::1`) does **not** exempt them, and every test counts as one IP [I: verify with a log line].
- Keep `setup.sh` registering alice, bob and agent with `prosodyctl`, so existing tests don't depend on PoW.
- Add one smoke test: `agent-chat --profile pow account register --server localhost --insecure-plaintext --no-start`, then `agent-chat --profile pow send alice@localhost hi`.
- A second "realistic" configuration with `AGENT_CHAT_POW_BITS=25` and `register_pow_count = 8` reproduces the production timings in §5.2.

---

## 9. Riskiest assumptions

1. **SHA-256 PoW gives almost no protection against GPU-equipped attackers** (§4.2). v1 relies on IP throttles and probation limits to hold the line, and on the algorithm field for a later upgrade. If abuse appears early, Equi-X or HashWX may be needed before limits can be tuned.
2. **Agent hardware.** The timing targets come from one Apple M2 [M]. Small cloud VMs, ARM boards without SHA extensions, and CPU-throttled containers have not been measured. The default difficulty may be too high for them, or too low for the people abusing it.
3. **Prosody internals.** The module overrides the GET form and runs ahead of `mod_register_ibr`'s SET handler. Both were verified against the 13.0 source, but neither is a documented extension point. Upgrades could break them silently. This is why the upstream hook in §6.2 matters.
4. **Probation roles may not reach components.** It's unverified whether a per-account role restricts actions on MUC and upload components (`host_user_role`).
5. **IP binding and network changes.** A client that reconnects from a different prefix (IPv6 privacy addresses, Happy Eyeballs switching address family, mobile networks) loses its solved token. The CLI should reconnect to the address it already resolved (`DnsConfig::addr`) and stay on the first connection when it can, which reduces this but doesn't remove it [I].
6. **Namespace.** `urn:xmpp:ibr-pow:0` is provisional. Shipping a module that uses it before the XSF accepts it could force a migration later.

---

## 10. Alternatives considered

1. **Reuse XEP-0158's `SHA-256` field as is.**
   - Advantage: it's an existing standard.
   - Rejected because:
     - it has no expiry or username binding;
     - its bit order and encoding are ambiguous;
     - it has no replay rule;
     - I know of no client that implements it in registration [I].
2. **XEP-0389 challenge flows.** Not implemented in Prosody 13 [V], and the spec has been inactive since 2020 [V]. Map onto it later (§5.8).
3. **A PoW-gated invite service over HTTPS.**
   - How it works: the agent solves PoW at an HTTPS endpoint, receives an XEP-0401 invite, and registers through standard XEP-0445.
   - Advantages:
     - only existing XMPP standards are used on the wire;
     - verification can run out of process and multi-threaded, in any language (Argon2id or Equi-X would be easy);
     - it works for any server that supports invites.
   - Disadvantages:
     - it needs an extra service plus endpoint discovery;
     - the client needs a custom HTTP flow anyway.
   - Worth keeping as plan B for servers other than Prosody, or if the Prosody coupling proves fragile [I].
4. **Stateful challenges stored on the session.**
   - Advantage: simpler, with no HMAC.
   - Disadvantages:
     - the solve must finish within the 5-minute unauthenticated timeout on a single connection;
     - unauthenticated connections hold state;
     - multi-server deployments and reconnects don't work.
   - Rejected [I].
5. **Argon2id inside Prosody.** Rejected for v1: the verification cost blocks the event loop, and it needs a C binding (§5.1).

---

## 11. Open questions for the project owner

1. **Algorithm for v1:** SHA-256 now, with Equi-X/HashWX later (recommended)? Or go straight to Equi-X? That means writing and packaging a Lua C binding and accepting an **LGPL-3.0** Rust dependency in an MIT/Apache-2.0 project.
2. **Difficulty target:** is about 10 s on all cores of a laptop (about 50 s on one core) right for agents? What should the cap be (proposed 2^34, about 10 minutes)? And what hardware do our agents typically run on (laptops, CI runners, small VMs)?
3. **Standardization:** submit a ProtoXEP? If not, which project-owned URI should replace `urn:xmpp:ibr-pow:0`?
4. **A dedicated agent domain:** should operators be told to run PoW registration only on a separate VirtualHost such as `agents.<domain>`?
5. **Probation policy:** which limits apply to new accounts, when does promotion happen, and should unused accounts be deleted automatically?
6. **IP binding and privacy:**
   - Should IP binding be on by default?
   - Which prefix lengths?
   - May the server store IP prefixes for abuse handling, and for how long?
7. **Invites bypass PoW** (proposed: yes). Should an invite exempt the account from probation too?
8. **Usernames:** random by default (proposed), or require a name? Should `mod_block_registrations` reserve names such as `admin` and `abuse`?
9. **Headless password custody:** with no Keychain (Linux CI, containers), should the CLI write a `0600` file in the config directory, require `--password-stdin`, or fail?
10. **Upstream:** propose the `register-form` hook and passing the stanza in `user-registering` to Prosody before we ship? Or ship the self-contained override first?
11. **Exit codes:** are 4 (username taken) and 5 (refused by policy) acceptable additions to the plan's set?

---

## 12. Implementation order (suggested)

1. Write the test vectors and spec text (§5.4–5.5). Build the Rust solver and verifier with unit tests.
2. Write `mod_register_pow`: issuing and verifying challenges, and the error paths. Run it in Docker at low difficulty.
3. Add `register.rs` and `account register` to the CLI, with JSON progress, timeouts and cancellation.
4. Add the integration tests from §7.6.
5. Add escalation (per prefix and global), probation, aging and metrics.
6. Raise the upstream Prosody hook, then the ProtoXEP draft.
7. Optionally, the `equix` algorithm.

---

## 13. Sources

**XMPP specifications**
- XEP-0077 In-Band Registration: https://xmpp.org/extensions/xep-0077.html
- XEP-0004 Data Forms: https://xmpp.org/extensions/xep-0004.html
- XEP-0068 Field Standardization for Data Forms: https://xmpp.org/extensions/xep-0068.html
- XEP-0158 CAPTCHA Forms: https://xmpp.org/extensions/xep-0158.html
- XEP-0389 Extensible In-Band Registration: https://xmpp.org/extensions/xep-0389.html
- XEP-0401 Ad-hoc Account Invitation Generation: https://xmpp.org/extensions/xep-0401.html
- XEP-0445 Pre-Authenticated In-Band Registration: https://xmpp.org/extensions/xep-0445.html
- XEP-0518 Payment Required: https://xmpp.org/extensions/xep-0518.html

**Prosody 13.0 source** (branch `13.0` of the trunk repository)
- https://hg.prosody.im/trunk/raw-file/13.0/plugins/mod_register_ibr.lua
- https://hg.prosody.im/trunk/raw-file/13.0/plugins/mod_register_limits.lua
- https://hg.prosody.im/trunk/raw-file/13.0/plugins/mod_invites_register.lua
- https://hg.prosody.im/trunk/raw-file/13.0/util-src/hashes.c
- The full branch archive, used for `util/dataforms.lua`, `util/events.lua`, `util/ip.lua`, `util/stanza.lua`, `util/pluginloader.lua`, `core/stanza_router.lua`, `plugins/mod_c2s.lua`, `mod_authz_internal.lua`, `mod_account_activity.lua` and others: https://hg.prosody.im/trunk/archive/13.0.tar.gz

**Prosody documentation**
- Roles: https://prosody.im/doc/roles
- Creating accounts: https://prosody.im/doc/creating_accounts
- Public servers: https://prosody.im/doc/public_servers
- mod_register_limits: https://prosody.im/doc/modules/mod_register_limits

**Prosody Docker image**
- https://hg.prosody.im/docker/ (`README.md`, `Dockerfile`, `configs/prosody-13.0.cfg.lua`)

**Prosody community modules**
- Index: https://modules.prosody.im/
- https://modules.prosody.im/mod_captcha_registration
- https://modules.prosody.im/mod_anti_spam
- https://modules.prosody.im/mod_firewall
- https://modules.prosody.im/mod_throttle_unsolicited

**Tor proof-of-work**
- Spec: https://spec.torproject.org/hspow-spec/index.html
- v1 (Equi-X): https://spec.torproject.org/hspow-spec/v1-equix.html
- Common protocol: https://spec.torproject.org/hspow-spec/common-protocol.html
- Equi-X: https://github.com/tevador/equix
- HashWX: https://github.com/tevador/hashwx
- Rust `equix` crate: https://docs.rs/equix/latest/equix/

**Anubis**
- Docs (WASM challenges): https://github.com/TecharoHQ/anubis/blob/main/docs/docs/admin/configuration/challenges/wasm.mdx
- How Anubis works: https://github.com/TecharoHQ/anubis/blob/main/docs/docs/design/how-anubis-works.mdx
- Argon2id challenge source: https://github.com/TecharoHQ/anubis/blob/main/wasm/pow/argon2id/src/lib.rs
- SHA-256 challenge source: https://github.com/TecharoHQ/anubis/blob/main/lib/challenge/proofofwork/proofofwork.go
- Blog post on the WASM challenges: https://github.com/TecharoHQ/anubis/blob/main/docs/blog/2026-09-06-anubis-wasm/index.mdx

**mCaptcha**
- https://mcaptcha.org/docs/webmasters/configuring-difficulty-factor/
- https://mcaptcha.org/docs/api/pow-sha256/

**Friendly Captcha**
- https://github.com/FriendlyCaptcha/friendly-pow
- https://friendlycaptcha.com/insights/proof-of-work/

**Hashcash**
- https://en.wikipedia.org/wiki/Hashcash
- Laurie & Clayton, "Proof-of-Work Proves Not to Work": https://www.cl.cam.ac.uk/~rnc1/proofwork.pdf
- hackint hashcash: https://github.com/hackint/hashcash

**GPU benchmark**
- hashcat 6.2.6 on an RTX 4090: https://gist.github.com/Chick3nman/32e662a5bb63bc4f51b847bb422222fd

**Crate sources** read from the local Cargo registry
- `tokio-xmpp` 6.0.0: `connect`, `xmlstream`, `client/login.rs`
- `xmpp-parsers` 0.23.0: `ibr.rs`, `stream_features.rs`, `data_forms.rs`
- `xso` 0.4.0: `UnknownChildPolicy`
- `argon2` 0.5.3: `params.rs`
- crate licences of `sha2`, `argon2` and `equix`
