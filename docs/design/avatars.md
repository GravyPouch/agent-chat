# Avatars: BoopFace and the alternatives

- **Status:** research note. Nothing here is implemented yet.
- **Date:** 2026-11-10
- **Scope:**
  - the default picture for a JID that has no published avatar;
  - real XMPP avatars (XEP-0084, XEP-0153);
  - what GPUI and GPUI Kit can display;
  - license fit with GPL-3.0-or-later.
- **Request during the research:** the user asked for "Grok Bot style characters". §6 covers that case.

**How claims are marked**

- **[V]** I verified it against a cited source: a spec, upstream source code, a license file or crate metadata. The sources are listed in §9.
- **[M]** I measured it myself: `cargo tree` on a throwaway manifest outside the repo, and a Python check of the XEP-0392 test vectors.
- **[I]** It's my inference, estimate or design choice, and is not verified.

I wrote a throwaway Rust program to render sample sheets of each style. Building it was blocked in this session, because compiling third-party crates runs their build scripts. **No style was rendered for this note.** Each style's look is described from its definition files, its README and its source code.

---

## 1. Summary

**BoopFace is not an avatar generator.**
- It is one animated robot character: a dark rounded-square head, two big white eyes and an antenna. It ships as a browser Web Component [V].
- It has no seed. Every instance is the same robot unless the page sets `color`, `skin` or head roundness [V].
- GPUI has no DOM, so we can't run it. Its idle face is simple enough to redraw in about 30 lines of SVG [V/I].

**Recommended default: two tiers, with XEP-0392 colour as the common thread.**

1. **Real avatar first.** Use XEP-0084 (PEP) when the contact publishes one, and XEP-0153 (vCard) as a fallback, mainly for MUC occupants. Cache avatars by SHA-1 [V spec].
2. **Otherwise, a generated default, seeded by the bare JID and coloured with the XEP-0392 hue.** Dino and Gajim colour a contact with the same hue [V].
   - **Minimal:** an initial on an XEP-0392 colour, as Dino and Gajim draw it [V]. It needs about 60 lines of our own code and no art licenses. We need a small custom element, because GPUI Kit's `Avatar` picks its colour from the *initials*. It can't take an XEP-0392 colour [V].
   - **Characterful ("Grok Bot style"):** DiceBear's **Voxel Bot** style. It is CC0, robot heads with a dark screen and glowing eyes, varied by top accessory, eyes, mouth and chest [V]. It runs through the native `dicebear-core` crate and feeds GPUI as SVG bytes [V]. Pass the XEP-0392 colour as the robot's body colour so it matches other clients [I].
   - **Fallbacks for the characterful tier:** **Pixelbot**, also CC0 and flatter [V], or **gsichtl `robot12`**, which is MIT, pixel art and has a single dependency [V].
   - Use robots for agents, meaning entities whose disco identity is `client/bot`, and initials for people [I].

**License red flags.**
- **Multiavatar and its Rust port:** the license adds restrictions that GPL-3 does not allow.
- **DiceBear's Bottts and Avataaars styles:** licensed only as "free for personal and commercial use", with no explicit right to modify or redistribute.
- **RoboHash sets 2 and 3:** CC BY 3.0 only, which is not on the FSF list of GPL-compatible licenses.
- **BoopFace's character clause:** an extra note on top of MIT.

Details are in §5.

---

## 2. What the desktop app can display

All of this was read from `gpui-pre-0.3.12`, `gpui-component-0.9.1` and `gpui-base-0.9.1` in the local cargo registry [V].

**`img(source)` takes an `ImageSource`.**
- `Resource::{Uri, Path, Embedded}`, from a `&str`, `String`, `PathBuf` or `SharedUri`.
- `Image(Arc<Image>)`: raw bytes plus an `ImageFormat`.
- `Render(Arc<RenderImage>)`: already-decoded BGRA frames.
- `Custom(loader)`.

**Formats.** `Image::to_image_data` decodes PNG, JPEG, WebP, GIF (animated), BMP, TIFF, ICO, PNM and **SVG**.
- SVG goes through `resvg`/`usvg` 0.46, with system fonts loaded.
- So `Arc::new(Image::from_bytes(ImageFormat::Svg, svg_bytes))` displays a multi-colour generated avatar with no rasterising code of our own.
- `Image::from_bytes` sets `id = hash(bytes)`, so GPUI's asset cache dedupes identical avatars.

**SVG resolution.**
- SVG bytes go through `render_single_frame(bytes, 1.0)`, scaled by `SMOOTH_SVG_SCALE_FACTOR = 2`. An SVG declared at 80×80 is rasterised at 160×160 px.
- Generate SVG at the largest display size (80 px).

**Keep remote URLs away from `img()`.**
- A `&str` that parses as a URL becomes `Resource::Uri`, which GPUI fetches through its HTTP client.
- Any other string is looked up as an embedded asset.
- Pass `PathBuf` or `Arc<Image>`, never a string. Otherwise a stray `https://…` (for example an XEP-0084 `url` pointer) leaks the user's IP to a third party.

**`svg()` is monochrome.**
- `svg().path(..)`, `.external_path(..)` and `.data(&[u8])` render an alpha mask tinted with `text_color`.
- That suits single-colour icons such as minidenticons, but not multi-colour avatars.

**GPUI Kit `Avatar`.**
- **Sizes:** `Avatar::new().name(..).src(..)` comes in XSmall 16, Small 24, Medium 48 and Large 80 px, and is clipped to a circle.
- **Image instead of fallback:** with `.src(..)` the image is drawn *instead of* the fallback. The fallback is not shown while the image loads or if it fails to load (`gpui-base` `Avatar::render`). Validate files before handing them over.
- **Fallback colour:** the name-only fallback hashes the **initials** (`gpui::hash`, FxHash) into one of 12 OkLCH hues.
  - "Alice Adams" and "Andy Anderson" share a colour.
  - The colour does not match what Dino or Gajim show for the same JID.
  - No API overrides the colour, so XEP-0392 needs our own small element.

**Call sites to change** in `crates/gui/src/app.rs` (lines 750, 875, 1004 and 1105).
- Today they pass display titles or sender names.
- They need the JID, or the room nick for MUC occupants, as well [V].

---

## 3. XMPP standards

**XEP-0084 User Avatar** (1.1.5, Stable) [V].
- **Nodes:** two PEP nodes, `urn:xmpp:avatar:data` (base64 image) and `urn:xmpp:avatar:metadata` (`<info bytes id type width height url?>`).
- **Id:** the item id is the SHA-1 of the raw image bytes.
- **Formats:** PNG support is required; GIF and JPEG are recommended.
- **Notifications:** subscribers learn about changes through `urn:xmpp:avatar:metadata+notify` in caps. They fetch data only for unknown ids, and should not upscale.
- **Disabling:** an empty `<metadata/>` disables the avatar.
- **Our side:** the daemon already advertises `…+notify` features for OMEMO in `crates/daemon/src/xmpp/disco.rs`, and has PEP publish and items helpers in `omemo/xml.rs`. `xmpp-parsers` 0.23 already ships `avatar`, `vcard` and `vcard_update` modules [V].

**XEP-0153 vCard-Based Avatars** (1.1.1, Historical, Active) [V].
- **How it works:** a vCard `PHOTO/BINVAL` holds the image, and every presence carries `<x xmlns='vcard-temp:x:update'><photo>sha1-hex</photo></x>`.
- **When to fetch:** fetch the vCard when the hash is unknown, and never poll.
- **Image guidance:** under 8 KB and 64×64 is recommended.

**XEP-0398 PEP-vCard conversion** (1.0.0, Stable) [V].
- If the server advertises `urn:xmpp:pep-vcard-conversion:0`, the client publishes and reads its own avatar through XEP-0084 only. It uses XEP-0153 only for other users, such as MUC occupants.

**XEP-0392 Consistent Color Generation** (1.0.1, Stable, 2025-10-03) [V].
- **Hue:** `angle = (sha1(utf8(id))[0] + sha1(..)[1]*256) / 65536 * 360`. The hue goes through **HSLuv**.
- **Saturation and lightness:** left to the implementation. The test vectors use S=100 and L=50, and the spec says to adapt lightness to the background.
- **Rules:** the user SHOULD be able to turn colouring off, and clients MUST NOT rely on colours being unique.
- **Test vectors:** I checked the §13.1 angles with a Python one-liner (Romeo → 327.255249°, juliet@capulet.lit → 209.410400°, 😺 → 331.199341°) [M].
- **Dino:** colours the bare JID with `hsluv(angle, 100, 50)`, and uses the nickname for MUC occupants. Its fallback avatar is the uppercase first character on that colour [V].
- **Gajim:** `nbxmpp.util.text_to_color` is the XEP-0392 algorithm, and `generate_default_avatar` draws one letter on that colour [V].
- **Conversations:** I could not confirm its code through GitHub search [I].

**Where the generated picture fits.** A generated avatar is *client-side presentation*, not protocol.
- It is not published, so other clients won't see it, unless we choose to publish it (§7.3, step 8).
- Seeding it from the bare JID and colouring it with the XEP-0392 hue keeps it deterministic on every device. It also keeps the colour consistent with what other XMPP clients show for the same contact [I].

---

## 4. BoopFace (CX-ArtLab/BoopFace)

| Question | Answer |
|---|---|
| What it is | "A lightweight, expressive robot avatar component for AI bots, agents and interactive apps", formerly *Agent Robot Avatar*. The README names "always-on agents like Grok Bot" as a target [V]. |
| How it draws the face | One fixed SVG in Shadow DOM, with a 240×240 viewBox [V]. It has a 160-point rounded-square head path, two white ellipses for eyes (rx 27, ry 29), eyelid rects that animate for blinks and expressions, and an antenna dot. Motion includes pointer-following eyes, jelly drag, a spring antenna, and 18 actions such as `waiting-spin`, `success`, `failure`, `love` and `sleep`. |
| Variation | `color`, `skin` (188 numbered skins in 17 series, painted by about 2.6k lines of JS with gradients and `feGaussianBlur` filters) and head roundness [V]. |
| Deterministic from a seed? | **No.** No seed or identity input exists. Idle motion uses `Math.random()`. The only seeded PRNG is inside skin texture patterns [V]. We would have to map JID → colour or skin ourselves [I]. |
| Output | A live, animated SVG Web Component (`<boop-face>`). It needs ES modules, Custom Elements, Web Animations, `ResizeObserver` and so on [V]. BoopFace Embedded is a C++11 engine (about 2.8k lines) that renders **only the eyes** into a 1-bit framebuffer for MCU displays [V]. |
| Language and dependencies | Vanilla JS with zero runtime dependencies. Lite build about 45 KB gzip, full build about 113 KB with skins. About 6.9k lines of JS in `src/` and the entry files [V]. |
| License | **MIT** for code. The art *is* the code: SVG built in JS [V]. The README adds a "Character design and visual identity" note. MIT "allows the avatar to be used, modified, and distributed as part of applications", but grants no ownership of the BoopFace name or character, nor the right "to present them as another party's original character or standalone brand" [V]. MIT is GPL-compatible. Treat that note as a branding restriction: don't make the BoopFace robot *our* mascot [I]. |
| Maintenance | Very active, single maintainer (CX-Lab). Created 2026-09-02, 110 commits, last push 2026-11-10, v0.8.0, 83 stars and 7 forks. Still pre-1.0, and renamed in 0.8 [V]. |
| Usable from Rust? | **Not directly.** No webview or DOM exists in GPUI, and a JS engine without a DOM can't run it [I]. Options: **(a)** redraw the static idle face as an SVG template (head, eyes and antenna, recoloured per JID), about 30 lines [I]; **(b)** use BoopFace Embedded through `cc` and FFI to animate status eyes for the open agent conversation. It draws eyes only, needs a C++ build step, and is "experimental", untested on hardware [V/I]. The skins are not worth porting [I]. |

**Verdict.**
- **As a contact-list default it is a poor fit.** Every contact would be the same robot in a different colour.
- **The idea is a good fit for a status face.** One expressive robot in the header of an *agent* conversation could show thinking, waiting or done. That's a later, optional feature [I].

---

## 5. Alternatives compared

O/D means offline and deterministic. Dependency counts are for the Rust route.

| Option | Code license | Asset license | O/D | Output | Rust | Look | Maintenance | Size or deps |
|---|---|---|---|---|---|---|---|---|
| **XEP-0392 + initial** | ours | none | yes/yes | GPUI text on a colour (or SVG) | ~60 lines + `sha1` (already locked) + `hsluv` 0.3 (MIT, no deps) | Dino/Gajim-style letter on a colour | spec Stable 1.0.1 | trivial [V/I] |
| **DiceBear** (`dicebear-core` + `dicebear-styles`) | MIT | per style: 44 CC0, 14 CC BY 4.0, 1 MIT, **4 "free for personal and commercial use"** | yes/yes (FNV-1a + Mulberry32; same output in all 7 languages) | SVG | **native crate**, 11.0.0-rc.2 / rc.3; styles are feature-gated | 63 styles, from geometric to people and robots (Voxel Bot, Pixelbot, Bottts) | very active (v10.7.0 2026-08-26; v11 RC 2026-09; ~10k stars) | core pulls `jsonschema` with default features (`reqwest`): 141 crates, **~22 new** for agent-chat [M]; voxel-bot.json is 48 KB [V] |
| **Boring Avatars** | MIT | none (shapes in code) | yes/yes (32-bit string hash over a 5-colour palette) | SVG (React) | none; each variant is 60–130 lines of TSX, easy to port | soft abstract marble, beam (a minimal face), pixel, sunset, ring, bauhaus | maintained, 2.0.x, last push 2026-06 | zero deps [V] |
| **Jdenticon** | MIT | none | yes/yes (SHA-1) | SVG or PNG | `rdenticon` 0.1.4 (MIT, raster via `ril`, 2023); `sdenticon` 1.0.0 (MIT, SVG, 2026-09, 21 downloads, no repo) | symmetric geometric glyph | slow; JS 3.3.0 from 2024-05 | ~2k lines of JS [V] |
| **Multiavatar** | Multiavatar License v1.0 | same, **restrictive** | yes/yes (SHA-256) | SVG | `multiavatar` 1.0.13 port, which *labels itself MIT* | colourful multicultural cartoon heads | original stale since 2022-03 | port ~92 KB with tiny-skia and codecs [V] |
| **minidenticons** | MIT | none | yes/yes (custom 32-bit hash, 9 hues) | one-colour SVG, 5×5 symmetric grid | none; ~20 lines to port, and draws with GPUI `svg().data()` tinted with the XEP-0392 colour | GitHub-like pixel blocks | finished (4.2.1, 2024-03) | 2.7 KB [V] |
| **GitHub-style identicons** | `identicon-rs` 7.2.1: MIT OR Apache-2.0; `identicon` 0.2.1: MIT | none | yes/yes | raster (via `image`) | native | 5×5 symmetric block grid | identicon-rs active (2026-06); identicon stale (2021) | `image` is already in the tree through GPUI, plus `sha3` or `md-5` [V] |
| **gsichtl** | MIT | original, in code (MIT) | yes/yes (BLAKE3 derive-key with versioned contexts) | cell grid → RGBA8 or PNG; trivial to turn into SVG rects | native | 8×8 and 12×12 pixel-art **robots** (antenna, siren, propeller, glowing eyes), monsters, nerds, crests | brand new (created 2026-10-02, v0.5.0 2026-11-02, 1 star) | `blake3` only [V] |
| **RoboHash** | MIT (Python); Rust port `robohash` 0.2.3, MIT in repo, "non-standard" on crates.io | set1 robots CC BY 3.0/4.0; set2 and set3 **CC BY 3.0**; set4 CC BY 4.0; set5 "free for personal and commercial use"; set6 CC0 | yes/yes if the PNG layers are bundled | layered PNG | port stale since 2022-12 | the classic cartoon robots | Python repo active (2026-09) | `image`, `imageproc`, `ring` + MBs of PNG [V] |
| **Libravatar / Gravatar** | services | n/a | **no**/yes | HTTP image | n/a | server-side defaults: identicon, monsterid, wavatar, retro, robohash, (pagan) | live services | every lookup leaks a contact hash and the user's IP [V/I] |

**Also found** [V]:
- **`hashavatar` 1.3.0** (MIT OR Apache-2.0): 31 families with expressions and accessories, WebP or SVG. Young, 0 stars.
- **`svg_avatars`**, **`ferrav`**, **`ikonym`**, **`holey-shapes`** and **`animatar`**: small geometric or animal SVG generators.
- None of them changes the recommendation [I].

**Notes on individual options**

**DiceBear.**
- **Bot styles:**
  - **Voxel Bot** (CC0): 7 tops (antenna, dish, fin, lightbar, siren, studs, twin), 7 eyes (cyclops, happy, plus, round, sleepy, square, visor), 5 mouths and 6 chests. Palettes cover background, body, glow, screen and accent [V].
  - **Pixelbot** (CC0): glowing pixel eyes (7 variants) and mouth (5 variants) on dark backgrounds [V].
- **Options** [V from `options.rs`]:
  - `{component}Variant` pins a part, which lets the same bot show a *state*: `eyesVariant: "sleepy"` for offline, `"happy"` for done [I].
  - `{color}Color` overrides a palette entry, for example `bodyColor`.
  - `animation: false` turns off the CSS keyframes. resvg would draw only the resting frame anyway [I].
- **Caveats:**
  - v11 is a **release candidate**. A style update can change existing pictures, so pin exact versions [I].
  - The `jsonschema` dependency brings in an HTTP client we would never use.
  - The DiceBear **HTTP API** is a network service, so don't use it.

**Libravatar and Gravatar.**
- They hash an e-mail address (MD5 or SHA-256). Libravatar federates over DNS SRV and redirects to Gravatar when it has no image [V].
- A JID is not necessarily an e-mail address [I].
- Every lookup sends the contact's hash, which can be reversed by dictionary attack, plus the user's IP and timing, to a third party [I].
- Fails requirement 1. At most an explicit opt-in for importing *your own* picture [I].

### License red flags

1. **Multiavatar.**
   - The license bars re-packaging the designs, building a "similar or competing product", and using it other than as "an addition to your product".
   - These are further restrictions on top of GPL-3 (§7, §10), so the art cannot be bundled in a GPL-3 binary [V license text; I legal reading].
   - The Rust crate `multiavatar` presents itself as MIT while embedding that art. **Do not use it.**
2. **DiceBear `bottts`, `bottts-neutral`, `avataaars`, `avataaars-neutral`** (Pablo Stanley).
   - Licensed only as "Free for personal and commercial use" [V]. bottts.com could not be loaded to check further: its TLS certificate had expired.
   - There is no explicit grant to modify or redistribute, so the terms are unclear. Avoid them, even though Bottts is the best-known robot style [I].
3. **RoboHash sets 2 and 3 (CC BY 3.0).**
   - The FSF lists CC BY **4.0** as "compatible with all versions of the GNU GPL" and CC0 as compatible [V]. It does not list CC BY 3.0 there [V].
   - Set1 is offered under CC BY 4.0 too. Set5 has the same unclear terms as Bottts.
4. **DiceBear CC BY 4.0 styles** (Adventurer, Big Smile, Micah, Fun Emoji and others).
   - Fine for GPL [V FSF], but they need attribution and a "modified" notice in About or Credits [V DiceBear's `license.ts` comment on CC BY §3(a)(1)(B)].
   - Prefer the CC0 styles to avoid this.
5. **BoopFace.** MIT plus the character and visual-identity note in §4. Fine to use inside the app; don't present it as our own character [I].
6. **Gravatar and Libravatar.** A privacy problem, not a license problem (above).

---

## 6. "Grok Bot style" characters

**What Grok Bot looks like.**
- xAI's design post describes Grok Bot avatars as one consistent construction built from "simple shapes and expressive eyes".
- Individual bots are told apart through "controlled variations and accessories".
- A motion system covers idle, thinking, working, waiting, blocked and done [V, x.ai post].
- We should copy the *principle*, not xAI's art or name [I].

| Candidate | Consistent base | Expressive eyes | Variations and accessories | Fit |
|---|---|---|---|---|
| DiceBear **Voxel Bot** (CC0) | yes: body, dark screen, glow | 7 eye types | 7 tops, 6 chests, 5 mouths, palettes | **best off-the-shelf match** [I] |
| DiceBear **Pixelbot** (CC0) | yes: dark "face screen" | 7 eye types | mouth only, plus colours | good, more minimal [I] |
| **gsichtl `robot12`** (MIT) | yes | glowing eyes | antenna, siren, propeller, rabbit ears, bolts | good if pixel art is welcome [I] |
| **BoopFace** static redraw (MIT) | yes | the best eyes, but only when animated | colour only, unless we add accessories ourselves | best for a *live status face*, weak as a contact default [I] |
| DiceBear Bottts | yes | yes | many | license unclear: **avoid** |

**Choice for characterful faces: DiceBear Voxel Bot, with Pixelbot as the alternative.**
- Seed: the bare JID.
- Body colour: the XEP-0392 hex.
- Later, BoopFace-style animated eyes can show state in the open agent's header.

**Choice for minimal: XEP-0392 colour plus initial.**

---

## 7. Recommendation

### 7.1 Default for JIDs without a published picture

- **Agents** get a **Voxel Bot**. An agent is a JID whose caps or disco identity is `client/bot`, or a contact the user marked as an agent.
  - `client/bot` is a registered identity: "An automated client that is not controlled by a human user" [V].
  - agent-chat advertises `client/pc` today [V]. Agent profiles could advertise `client/bot` [I].
- **People and rooms** get the **XEP-0392 colour plus initial**.
- **Rooms:** use the room JID as the seed and colour input. For MUC occupants, use the nick, as Dino does [V].
- **Setting:** "Robot avatars for everyone / agents only / off". With "off", it's initials only, and colours can be turned off as XEP-0392 asks.

### 7.2 Real avatars

Use them in this order:

1. The XEP-0084 avatar.
2. The XEP-0153 vCard photo, mainly for MUC occupants and old servers.
3. The generated default.

Rules:
- **Verify:** check that the SHA-1 of the bytes equals the advertised id.
- **Limits:** cap the size (for example 1 MiB) [I].
- **Types:** accept only PNG, JPEG, WebP or GIF from the network. Don't accept remote SVG [I].
- **Never follow** the metadata `url` pointer automatically, for the IP-leak reasons in §2 [I].
- **Cache:** content-addressed, `<cache>/avatars/<sha1>.<ext>`, so the same picture is shared across contacts and devices [I].

### 7.3 Implementation sketch

This note adds no dependencies. Crates in brackets are proposals.

1. **`crates/proto`: `consistent_color.rs`.**
   - Compute the XEP-0392 angle and an HSLuv → sRGB conversion, with lightness chosen per theme.
   - [`sha1` 0.10, already in `Cargo.lock` through `xmpp-parsers`; `hsluv` 0.3, MIT, no deps, or vendor its ~100 lines.]
   - Unit-test against the §13.1 vectors.
2. **`crates/gui`: a `ContactAvatar` element.**
   - Inputs: JID, name, size, theme and an optional image.
   - With a real picture: GPUI Kit `Avatar::new().name(name).src(PathBuf)`.
   - Otherwise: a rounded div with the XEP-0392 background and an initial, or the bot (step 6).
   - Replace the four call sites (§2).
3. **Daemon: subscribe to avatars.**
   - Add `urn:xmpp:avatar:metadata+notify` to `FEATURES` in `xmpp/disco.rs`.
   - Handle PEP metadata events with `xmpp_parsers::avatar::Metadata`.
   - Fetch `urn:xmpp:avatar:data` by id only when it isn't cached, then verify and store it.
   - An empty `<metadata/>` clears the avatar.
4. **Daemon: vCard fallback.**
   - Parse `vcard-temp:x:update` in presence (`xmpp_parsers::vcard_update`).
   - Fetch the vCard (`xmpp_parsers::vcard`) only for unknown hashes. Never poll.
5. **Store and IPC.**
   - Add an SQLite table `avatar(jid, sha1, mime, updated_at)`.
   - Add `avatar: Option<AvatarRef { sha1, mime }>` on `Contact` and on room occupants, plus `Event::AvatarChanged { jid }`.
   - CLI and agents get the same data: `agent-chat contacts --json` shows the hash.
6. **GUI: characterful default.**
   - [`dicebear-core = "=11.0.0-rc.2"`, `dicebear-styles = { version = "=11.0.0-rc.3", features = ["voxel-bot"] }`.]
   - Call `Avatar::new(&style, json!({ "seed": bare_jid, "size": 80, "animation": false, "bodyColor": [xep0392_hex] }))`.
   - Then `Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()))`.
   - Memoise it in a `HashMap<(Jid, Theme), Arc<Image>>`.
   - CC0 needs no attribution. Keep DiceBear's MIT notice in the licenses screen.
   - If the RC status or the `jsonschema`/`reqwest` weight is unwelcome, swap in [`gsichtl` 0.5] (`robot12(jid)` → SVG rects → the same `Image` path) without touching anything else.
7. **Publish our own avatar.**
   - Commands: `agent-chat avatar set <png>` and `avatar clear`.
   - Publish data, then metadata.
   - Also update the vCard unless the server advertises `urn:xmpp:pep-vcard-conversion:0`.
8. **Optional: let agents show their robot everywhere.**
   - `agent-chat avatar set --generated` rasterises the agent's own Voxel Bot to PNG and publishes it through XEP-0084.
   - Conversations, Dino and Gajim then show the same robot.
   - [`resvg` in the CLI crate. It's already in the tree through GPUI, but the CLI doesn't depend on GPUI.] [I]
9. **Later: a live status face.**
   - A BoopFace-style animated pair of eyes in the open agent's header, driven by agent state.
   - Build it with GPUI animations, or with BoopFace Embedded through FFI.

### 7.4 Open questions

- **Robots for whom?** Robots only for `client/bot` entities, or for everyone? This is a product choice [I].
- **Stability across upgrades:** do we accept that a DiceBear style update can change existing robots? If not, vendor the style JSON and pin it [I].
- **Lightness per theme:** the XEP-0392 lightness for light and dark themes still needs a WCAG contrast check, like GPUI Kit's existing tests [I].

---

## 8. Not chosen, and why

- **BoopFace as a dependency:** it is a web-only Web Component with no seed (§4).
- **Multiavatar and its Rust port:** the license is GPL-incompatible.
- **Bottts and Avataaars:** the terms are unclear.
- **Gravatar and Libravatar:** they need the network and leak contacts.
- **RoboHash:** CC BY 3.0 art, a stale Rust port and a heavy raster pipeline.
- **Jdenticon, minidenticons, GitHub identicons and Boring Avatars:** fine licenses, but abstract. They don't give the character the user asked for. minidenticons is the cheapest non-letter fallback if wanted.

## 9. Sources

- BoopFace: https://github.com/CX-ArtLab/BoopFace (README, LICENSE, `src/boopface-core.js`, `embedded/lib/BoopFace/src/BoopFace.h`, `package.json`, git log), https://boopface.com
- Grok Bot design: https://x.ai/news/designing-grok-bot
- DiceBear: https://github.com/dicebear/dicebear (README, LICENSE, `src/rust/core`), https://github.com/dicebear/styles (LICENSE.md, `src/*.json`, Cargo.toml), https://crates.io/crates/dicebear-core, https://www.dicebear.com/licenses/
- Boring Avatars: https://github.com/boringdesigners/boring-avatars
- Jdenticon: https://github.com/dmester/jdenticon, https://crates.io/crates/rdenticon, https://crates.io/crates/sdenticon
- Multiavatar: https://github.com/multiavatar/Multiavatar (LICENSE), https://crates.io/crates/multiavatar
- minidenticons: https://github.com/laurentpayot/minidenticons
- Identicons: https://crates.io/crates/identicon-rs, https://crates.io/crates/identicon
- gsichtl: https://github.com/cebor/gsichtl
- RoboHash: https://github.com/e1ven/Robohash (README license table), https://github.com/kyco/robohash
- hashavatar: https://github.com/valkyoth/hashavatar
- Libravatar: https://wiki.libravatar.org/api/; Gravatar: https://docs.gravatar.com/sdk/images/
- XEPs: https://xmpp.org/extensions/xep-0084.html, https://xmpp.org/extensions/xep-0153.html, https://xmpp.org/extensions/xep-0398.html, https://xmpp.org/extensions/xep-0392.html; disco registry: https://xmpp.org/registrar/disco-categories.xml
- Dino: https://github.com/dino/dino (`xmpp-vala/src/module/xep/0392_consistent_color/`, `main/src/ui/util/helper.vala`, `main/src/ui/widgets/avatar_picture.vala`)
- Gajim: https://github.com/gajim/gajim (`src/gajim/gtk/avatar.py`), https://github.com/gajim/python-nbxmpp (`nbxmpp/util.py`)
- FSF license list: https://www.gnu.org/licenses/license-list.html (#ccby, #CC0, #CC-BY-NC)
- Local sources: `gpui-pre-0.3.12` (`src/elements/img.rs`, `src/elements/svg.rs`, `src/platform.rs`, `src/svg_renderer.rs`, `src/asset_cache.rs`), `gpui-component-0.9.1/src/avatar/`, `gpui-base-0.9.1/src/avatar.rs`, `xmpp-parsers-0.23.0/src/{avatar,vcard,vcard_update}.rs`
