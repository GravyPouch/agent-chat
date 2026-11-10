# Third-party notices

agent-chat is licensed under the GNU General Public License v3.0 or later (see
[LICENSE](LICENSE)). It includes, or is derived from, the following third-party work.

## Vendored source code

| Component | Location | License | Copyright |
|---|---|---|---|
| libomemo-c (fork of libsignal-protocol-c) | `third_party/libomemo-c` | GPL-3.0 ([LICENSE](third_party/libomemo-c/LICENSE)) | 2015–2016 Open Whisper Systems; 2020–2025 Dino Team |
| curve25519-donna, ed25519 ref10 and additions (inside libomemo-c) | `third_party/libomemo-c/src/curve25519` | BSD-style / public domain, as noted in each file | 2008 Google Inc., and others |
| protobuf-c runtime 1.5.1 | `third_party/protobuf-c` | BSD-2-Clause ([LICENSE](third_party/protobuf-c/LICENSE)) | 2008–2025 Dave Benson and the protobuf-c authors |

## Ported code

| Component | Location | License | Copyright |
|---|---|---|---|
| BoopFace (geometry, SVG face, embedded motion engine) | `crates/boopface` | MIT ([LICENSE](third_party/boopface/LICENSE)) | 2026 CX ART Lab |

The BoopFace character, name and visual identity are CX ART Lab's original design;
agent-chat uses a port of it for avatars. See [third_party/boopface/README.md](third_party/boopface/README.md)
for which files were ported from where.

## Assets compiled into the desktop app

| Component | Source | License | Copyright |
|---|---|---|---|
| Lucide icons | via the `gpui-kit-assets` crate | ISC | Lucide Icons and Contributors |

## Rust crates

agent-chat depends on several hundred crates from crates.io. They're listed in `Cargo.lock`, and
their licenses are permissive and GPL-3.0-compatible: MIT, Apache-2.0, BSD-2/3-Clause, ISC,
Zlib, MPL-2.0, Unicode-3.0, Unlicense, CC0-1.0 and BSL-1.0. Notable ones include GPUI and
GPUI Kit (Apache-2.0), tokio-xmpp and xmpp-parsers (MPL-2.0), rusqlite (MIT) and the RustCrypto
crates (MIT/Apache-2.0).

To produce a full per-crate license report, use for example
[`cargo-about`](https://github.com/EmbarkStudios/cargo-about) or `cargo deny check licenses`.

## Test tooling (not distributed)

`dev/omemo-bot` installs [slixmpp-omemo](https://github.com/Syndace/slixmpp-omemo) (AGPL-3.0) and
python-omemo inside a Docker image, used only for interoperability tests. It isn't linked into
or shipped with agent-chat.
