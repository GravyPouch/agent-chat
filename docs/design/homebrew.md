# Homebrew distribution: plan

- **Status:** plan and drafts. Nothing is published. There is no tap repository, no tag and no Apple certificate yet.
- **Date:** 2026-11-10
- **Drafts that come with this plan:**
  - `packaging/homebrew/Formula/agent-chat.rb`: the CLI, built from source.
  - `packaging/homebrew/Casks/agent-chat.rb`: the signed desktop app, with the CLI inside.
  - `.github/workflows/release.yml`: the release pipeline. It is inert until a `v*` tag is pushed.
  - `packaging/macos/bundle.sh`, `Info.plist.in` and `agent-chat.entitlements`: assemble, sign and notarize `agent-chat.app`.
- **Scope:**
  - Homebrew rules as of Homebrew 7.0.13;
  - the formula and cask split;
  - the code changes needed before packaging;
  - signing and notarization;
  - release automation;
  - versioning, milestones, risks and open questions.

**How claims are marked** (the scheme used by the other design docs)

- **[V]** Verified in this session against a cited source. That means Homebrew's documentation and code shipped with the installed Homebrew 7.0.13 (`/opt/homebrew/docs`, `/opt/homebrew/Library/Homebrew`), a web page I opened, or this repository. Sources are in §14.
- **[V: search]** Taken from a search-engine summary of pages I did not open myself. Weaker than [V].
- **[M]** Measured or checked on this machine in this session.
- **[I]** My inference, design choice, or recollection, not re-checked in this session.

---

## 1. Summary

**Recommended path**

1. **Own tap first: `GravyPouch/homebrew-tap`**, which users know as `gravypouch/tap`. It holds two packages:
   - **Formula `agent-chat`:** the CLI, which is also the daemon and the MCP server. It builds from the release source tarball against Homebrew's `libomemo-c`. The tap's CI builds bottles, so users don't compile Rust. It works on macOS and Linux.
   - **Cask `agent-chat`:** `agent-chat.app`, signed with a Developer ID and notarized. It bundles the CLI at `Contents/MacOS/agent-chat` and links it into `$(brew --prefix)/bin`. It is Apple Silicon only and needs macOS 15 or later.
   - The formula and the cask are alternatives: a user installs one or the other (§4).
2. **homebrew-core and homebrew/cask later**, once the repository passes Homebrew's notability bar.
   - That bar is 75 stars, 30 forks or 30 watchers when someone else submits the package.
   - For a self-submission it is 225 stars, 90 forks or 90 watchers.
   - Either way, the repository must be at least 30 days old [V].
   - The tap formula is written to core's rules already, so moving it is mostly a copy.
3. **One release pipeline:** a `vX.Y.Z` tag builds and notarizes the app, publishes a GitHub Release, and opens a pull request on the tap. The tap's `brew test-bot` builds bottles for that PR, and `brew pr-pull` merges it.

**What has to happen first**

- **Code changes:** six P0 changes (§5). They make the CLI aware of Homebrew and app-bundle installs:
  - never copy itself out of the Cellar;
  - always point agents at `$(brew --prefix)/bin/agent-chat`;
  - add an `install --skill-only` flag;
  - add a `system-libomemo` feature.
- **Also before the first release:**
  - a daemon version handshake and `daemon run --supervised`, both P1;
  - `CHANGELOG.md`;
  - an app icon.
- **The user must provide:**
  - an Apple Developer Program membership;
  - a Developer ID Application certificate and an App Store Connect API key;
  - seven GitHub secrets (§8);
  - the `GravyPouch/homebrew-tap` repository;
  - the agent-chat repository made public.

## 2. Local checks [M]

| Check | Result |
|---|---|
| `brew --version` | Homebrew 7.0.13. |
| `brew info agent-chat` | "No available formula with the name "agent-chat"". The name is free in homebrew-core. |
| `brew info --cask agent-chat` | "No Cask with this name exists". The name is free in homebrew/cask. |
| `brew search agent-chat` | **Failed.** It refused to load `penso/tap/agent-launcher` from an untrusted third-party tap tapped on this machine. That is tap trust at work (§3.3). The two `info` checks above cover core and cask. |
| `brew info libomemo-c` | Stable 0.5.1, bottled, `GPL-3.0-only`. It depends on `protobuf-c` and builds with `cmake`. Not installed here. |
| Vendored libomemo-c | `third_party/libomemo-c/VENDORED_COMMIT` is `1f0b37fe…`, which is exactly tag **v0.5.1** [V]. Homebrew's library is the same release. |
| `agent-chat --version` | Prints `agent-chat 0.1.0`. clap's `#[command(version)]` in `crates/cli/src/main.rs` already provides it. |
| Formula test steps, run against today's debug binary with a scratch `AGENT_CHAT_HOME` | All pass: `daemon status` exits 3 with "daemon is not running", `status` auto-starts the daemon and prints `{"profile":"default","state":"no_account",…}`, `daemon stop` exits 0. Only `install --skill-only` remains, because that flag doesn't exist yet. |
| `packaging/macos/bundle.sh --skip-build --dmg`, ad-hoc signed, with existing local binaries | Builds `agent-chat.app` with the CLI identifier `io.github.gravypouch.agent-chat.cli`. `codesign --verify --strict --deep` passes, and the ZIP and DMG are written. |
| Toolchain | Homebrew's `rust` is 1.99.1; the workspace needs 1.92. |
| Native dependencies of the CLI on Linux (`cargo tree`) | `aws-lc-sys`, `libsqlite3-sys` (bundled), `bindgen`, and `zbus-secret-service-keyring-store`, which is pure Rust: no libdbus. |
| GPUI shaders | `gpui-pre-platform` is built with `runtime_shaders`, so building the app needs no Metal compiler. |
| Repository state | Default branch `master`; no tags; no `CHANGELOG.md`; no app icon. `.gitignore` doesn't list `dist/`. |
| Working tree during this session | A merge that adds `crates/server` (the hub) was in progress, with conflicts in `Cargo.toml`, `README.md`, `crates/cli/src/main.rs` and `docs/AGENTS.md`. Most of this plan was checked against the tree before the merge. It names files and functions, not line numbers, so it still applies. |

`brew cat` and `brew audit --help` switch on Homebrew's developer mode as a side effect. I switched it off again (`brew developer off`) both times.

## 3. Research findings

### 3.1 homebrew-core acceptance

- **Notability:** see §1 for the thresholds. Repositories younger than 30 days are normally not eligible. [V]
- **Livecheck:** new packages need a working `livecheck`. [V]
- **Platforms:** a formula must build and pass its tests on core's CI for the supported macOS and Linux configurations. An explicit platform restriction is possible only when upstream doesn't support a platform. [V]
- **Stable releases:** upstream must call the version stable and provide an immutable tag or release. Release archives are preferred to Git checkouts. The formula must build without downstream patches. [V]
- **Build from source:** a formula must build from source or install portable output. Platform-specific binaries belong in a cask. [V]
- **Native apps:** a formula whose primary output is a macOS `.app` is not eligible. When upstream has both a CLI and a GUI, the CLI should stay the formula's primary purpose. [V]
- **Vendored dependencies:**
  - "A separate project should not be bundled when a maintained Homebrew formula can provide the same dependency." [V]
  - For Rust: "Declare native libraries used by crates as formula dependencies rather than allowing their build scripts to download them." [V]
  - So in core, libomemo-c must come from Homebrew. That needs the `system` feature, which exists, plus a way to turn it on from the CLI crate: P0-1.
  - Bundled SQLite (rusqlite `bundled`) may draw a review comment (P2-2). [I]
- **Licences:** DFSG-compatible licences only. GNU licences need the `-only` or `-or-later` suffix. `GPL-3.0-or-later` qualifies. [V]
- **Self-updating software:** must have self-update disabled. agent-chat doesn't self-update. [V]

### 3.2 Rust formula conventions (Homebrew 7)

- Declare `depends_on "rust" => :build` and build with `--locked` against upstream's `Cargo.lock`. [V]
- **Fetch phase:**
  - A formula defines `def fetch; system "cargo", "fetch", *std_cargo_fetch_args; end`. That method runs *with* network access.
  - `install` then runs with networking off and the caches read-only.
  - `std_cargo_fetch_args` is `--locked --target host-tuple`. [V]
  - Homebrew 7.0.0 started moving formulae to this model. [V]
- **`std_cargo_args`:**
  - It is `--jobs N --locked --root=<prefix> --path=<path>`, plus `--features=…` when given, plus `--offline` when the formula defines `fetch`.
  - Use `path:` for a workspace member and `features:` for extra features. [V]
- **`deny_network_access!`** with no argument denies network in the build, test and postinstall phases. The documented Rust example uses it together with `fetch`. [V]
- **bindgen and libclang:**
  - The idiom is `uses_from_macos "llvm" => :build`. `sequoia-sq` and `sequoia-chameleon-gnupg` use it for bindgen through `nettle-sys`, and neither sets `LIBCLANG_PATH`. [V]
  - `uses_from_macos` acts as `depends_on` on Linux and is ignored on macOS, where the Command Line Tools provide libclang. [V]
- **Tests:**
  - `HOME` is `testpath`.
  - On macOS the test sandbox allows Unix sockets only inside `testpath`.
  - "`foo --version` and `foo --help` are bad tests", but better than none. Preferred: real functionality, or failing as expected without credentials. [V]
- **Sandbox:** Homebrew 7 blocks sandboxed reads of the home directory by default. [V] So a formula cannot install the Claude Code skill into `~/.claude` during `post_install`. That is why the formula prints a caveat instead. [I]

### 3.3 Taps and tap trust

- **Repository name:** a GitHub tap repository should start with `homebrew-`. `GravyPouch/homebrew-tap` is used as `gravypouch/tap`. [V]
- **`brew tap-new`:**
  - `brew tap-new GravyPouch/homebrew-tap` creates the layout and bottle-building GitHub Actions workflows. [V]
  - Bottles are published with `brew pr-pull` (optionally `--head-sha`), or with the generated `brew pr-pull` workflow. [V]
  - In Homebrew 7, `tap-new` generates *attesting* bottle workflows by default, and Homebrew verifies those attestations. [V]
- **Tap trust (since Homebrew 6.0.0):** third-party taps must be trusted explicitly before Homebrew evaluates their code. [V]
  - `brew install user/repo/formula` and `brew install --cask user/repo/cask`, with the fully qualified name, trust only that item. That is the recommended install command.
  - Short names work only after `brew trust --formula`, `brew trust --cask`, or `brew trust user/repo`.
  - **The README must use fully qualified names.** [V]
- **Cask names** must be globally unique across taps. A formula and a cask may share a token. [V]
  - With a shared token, a bare `brew install gravypouch/tap/agent-chat` resolves to the formula, so the app needs `--cask`. [I]
- **Upstream taps vs core:** Homebrew won't remove a core formula in favour of an upstream tap. [V]

### 3.4 Casks in 2026

- **homebrew/cask:**
  - Apps must pass Homebrew's Gatekeeper checks, which means Developer ID signing and notarization. [V]
  - Homebrew 5.0.0 deprecated unsigned casks and announced that official casks failing Gatekeeper would be disabled in September 2026. [V]
  - `zap` is required. [V]
- **Third-party taps:**
  - Homebrew's signing audit is skipped for non-official taps unless you pass `brew audit --signing`. Its check is `codesign --verify -R=notarized --check-notarization`. [V: code]
  - But Homebrew always quarantines cask downloads, and `--no-quarantine` is gone: Homebrew 5.0 deprecated it [V], and 7.0.13's `brew install --help` no longer lists it [M].
  - macOS Sequoia removed the Control-click bypass. Users must go to *System Settings → Privacy & Security → Open Anyway*. [V: search]
  - **So an unsigned app in our own tap is technically allowed but practically unusable. Sign and notarize from day one.**
- **Stanzas:**
  - `depends_on macos: :sequoia` sets a minimum version. Arrays mean an exact set. [V]
  - `brew audit --online` compares it with `LSMinimumSystemVersion` and the main executable's load commands, and fails on a mismatch. [V] So `Info.plist.in` says `15.0`, and `bundle.sh` sets `MACOSX_DEPLOYMENT_TARGET=15.0`.
  - **`conflicts_with` accepts only `cask:`** in 7.0.13 (`VALID_KEYS = [:cask]`). A cask cannot declare a conflict with a formula. [V: code]
  - `binary "#{appdir}/X.app/Contents/MacOS/tool"` links a tool from inside the app. [V]
  - `generate_completions_from_executable … shell_parameter_format: :clap` uses clap's `COMPLETE=<shell>` dynamic completions. [V]
  - `uninstall` directives run in a fixed order: …, `quit`, `signal`, …, `script`, …. `signal` is skipped on upgrade unless `on_upgrade:` says otherwise. [V] I infer that `quit` and `script` do run on upgrade. [I]
  - Stanza order and the alphabetical order of `depends_on` keys follow the Cask Cookbook. [V]
- **Linux:** casks may support Linux too. Homebrew 6 added AppImage. [V] Out of scope here.

### 3.5 Audits and tests

The documented local checks [V]:
- for a formula: `HOMEBREW_NO_INSTALL_FROM_API=1 brew install --build-from-source F`, `brew test F`, `brew audit --strict --new --online F`, `brew style --fix --formula F`;
- for a cask: `brew install --cask C`, `brew uninstall --cask C`, `brew audit --new --cask C`, `brew style --fix --cask C`;
- then `brew lgtm --online` before opening a pull request on core or cask.

For our own tap, drop `--new`, because it applies the notability rules. Add `--signing` for the cask.

### 3.6 `service do`

- **Fields:** `run`, `keep_alive` (`true`, `successful_exit:`, `crashed:`, `path:`), `environment_variables` (with `std_service_path_env`), `log_path`, `error_log_path`, `working_dir`, `run_at_load`, `throttle_interval`, and `stop_timeout`, which is new in 7.0. [V]
- **Labels:** new services use the label `sh.brew.<formula>`. [V]
- **`keep_alive successful_exit: false`:** launchd restarts the job only after a non-zero exit. [I: launchd `SuccessfulExit` semantics]
- **Upgrades:** I found no service-restart logic in the formula upgrade path of 7.0.13; the only restart code is in `brew bundle`. [I from a code search] A running daemon therefore keeps running the old binary after `brew upgrade`. P1-2 handles that.

### 3.7 Intel and platform tiers

- Intel macOS has been Tier 3 since September 2026: no CI, no new bottles. Homebrew stops running on Intel in or after September 2027. [V]
- macOS 27 "Golden Gate" dropped Intel. GitHub will retire Intel macOS runners in autumn 2027. [V]
- Tier 1 is Apple Silicon on macOS 27, 26 and 15. Homebrew itself requires macOS 15 or later. [V]
- **Recommendation: an arm64-only app.** `bundle.sh --universal` exists if Intel turns out to matter. Intel users can still build the formula from source.

### 3.8 Apple signing and notarization

- **Requirements:** a Developer ID certificate, the hardened runtime ("for your app and command line targets"), a secure timestamp, no `get-task-allow`, and the macOS 10.9 SDK or later. [V]
- **What can be submitted:** you can't upload a `.app` directly, so create a ZIP with `ditto -c -k --keepParent`. [V]
- **Stapling:**
  - "While you can notarize a ZIP archive, you can't staple to it directly." Staple each item, then re-zip. [V]
  - Standalone binaries get tickets but cannot be stapled. [V] That is one more reason to ship the CLI inside the app rather than as a separate notarized tarball.
- **notarytool flags:**
  - API key: `--key <p8> --key-id <id> --issuer <uuid>`. The issuer is "Required for Team API Keys. Do not provide for Individual API Keys." [M: `xcrun notarytool submit --help`]
  - Apple ID: `--apple-id`, `--team-id`, `--password` with an app-specific password. [M]
- **Helper tools:** `Contents/MacOS/` is the documented location for "help app, helper tool". Scripts count as resources. [V]
- **Who signs:** Apple's notarization page says the Account Holder must sign with the Developer ID. [V] Whether team Admins can also create Developer ID certificates is not settled here. [I]

### 3.9 App-bundle tooling

| Option | Facts | Fit |
|---|---|---|
| `cargo-packager` 0.11.8 (docs dated 2025-11-27) | Makes `.app` and `.dmg`. It handles the signing identity, entitlements and notarization credentials, and supports `external_binaries` named `<name>-<target-triple>`. [V] | Workable, but the CLI would be an "external binary" with triple-suffixed names. Signing order and identifiers are less explicit. Another tool to pin. |
| `cargo-bundle` | Self-described "very early alpha". Signs in-process with `apple-codesign`, but "does not notarize artifacts". Repeating `--target` produces a universal binary via `lipo`. [V] | No notarization; we would script that anyway. |
| Zed's approach | A fork of cargo-bundle plus a shell script. [V: search] | Confirms that GPUI apps end up with a script. |
| **Hand-rolled `bundle.sh` (chosen)** | About 200 lines of bash with no dependencies beyond Xcode's tools. Tested ad-hoc here [M]. | Full control over layout, signing order, the CLI's code-signing identifier, notarization and verification. Runs the same locally and in CI. |

### 3.10 Release automation

- **`mislav/bump-homebrew-formula-action@v4`:**
  - Bumps `url` and `sha256` of a formula in a tap. By default it uses the GitHub tag archive URL.
  - It needs a `COMMITTER_TOKEN`. It has "limited support for bumping Homebrew casks". [V]
- **`brew bump-formula-pr` and `brew bump-cask-pr`** are official alternatives. They need Homebrew on the runner and a trusted tap. [I]
- **The draft workflow** uses neither. It runs two `sed` edits on the tap checkout and opens one pull request with two commits, formula and cask. The edits were tested on copies of the drafts with BSD sed. [M]
- **GitHub runners:**
  - `macos-26` is arm64 and `macos-26-intel` is x64; macOS 26 runners are generally available since 2026-02-26 [V].
  - `macos-latest` moved to macOS 26 in June 2026 [V: search].
  - `ubuntu-24.04-arm` is the arm64 Linux runner [I].

## 4. Distribution strategy and the formula/cask split

### 4.1 Phases

| Phase | Where | What |
|---|---|---|
| 1 (now) | `GravyPouch/homebrew-tap` | Formula `agent-chat`: the CLI from source, with bottles from the tap's CI. Cask `agent-chat`: the notarized app with the CLI inside. |
| 2 (when notable) | `Homebrew/homebrew-core` | The same formula without its `bottle do` block. Submit with `brew audit --strict --new --online`. Afterwards, `tap_migrations.json` in the tap points users to core. [I] |
| 3 (when notable) | `Homebrew/homebrew-cask` | The same cask. CI checks Gatekeeper and `zap`. A cask in the tap with the same name must then be removed or migrated, because cask names must be globally unique. |

### 4.2 Who installs what

| User | Command | Gets |
|---|---|---|
| Agent hosts, servers, Linux, people who only want the CLI | `brew install gravypouch/tap/agent-chat` | `agent-chat` in `$(brew --prefix)/bin`, plus `brew services` support. |
| People who want the desktop app (macOS 15+, Apple Silicon) | `brew install --cask gravypouch/tap/agent-chat` | `/Applications/agent-chat.app`, with its CLI linked as `$(brew --prefix)/bin/agent-chat`. |
| Either, afterwards | `agent-chat install --skill-only --mcp` | `~/.claude/skills/agent-chat/SKILL.md` and `claude mcp add --scope user agent-chat -- $(brew --prefix)/bin/agent-chat mcp`. |

### 4.3 Decision: the cask bundles the CLI; it does not use `depends_on formula:`

Why bundle:

1. **The daemon matches the app.**
   - `DaemonLauncher::locate()` (`crates/proto/src/client.rs`) looks next to the running executable first. So the GUI starts `Contents/MacOS/agent-chat`, which is always the same version as the GUI.
   - With a formula dependency, the GUI and the CLI would upgrade independently and could disagree on the IPC protocol.
2. **Finder-launched apps don't see Homebrew's PATH.**
   - Apps started from the Dock get launchd's minimal PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), so the formula's CLI wouldn't be found on PATH. [I]
   - A bundled CLI sidesteps this. P0-6 still adds fallbacks for non-bundled builds.
3. **Keychain access survives upgrades.**
   - Legacy keychain ACLs identify the accessing program by its code-signing designated requirement. A Developer ID-signed CLI with a fixed identifier (`io.github.gravypouch.agent-chat.cli`) keeps the same requirement across releases.
   - A Homebrew-built CLI is ad-hoc signed, so its requirement is its cdhash, which changes every build. Every upgrade is likely to trigger a new keychain prompt. [I] See §12, risk R1.
4. **Tap trust.**
   - A fully qualified cask install trusts only that cask. [V]
   - A formula dependency from the same untrusted tap would probably need separate trust. [I]
5. **No Rust toolchain or compile time for app users.** The ZIP or DMG also works for people who don't use Homebrew.

The price is that **the formula and the cask are mutually exclusive**:
- Both provide `$(brew --prefix)/bin/agent-chat`.
- A cask cannot declare `conflicts_with formula:` (§3.4).
- In practice, installing the cask over a linked formula fails at the `binary` step, and installing the formula over the cask leaves the formula unlinked. [I: verify in M2]
- Both packages' caveats say so. The README will too.

Rejected alternative: a cask with only the GUI and `depends_on formula: "gravypouch/tap/agent-chat"`. It has none of the five benefits above, and it would need P0-6 just to work.

## 5. Code changes before packaging

Priorities:
- **P0:** blocks the first tap release.
- **P1:** do before or with the first release.
- **P2:** later.

All paths are relative to the repository root.

### P0-1. A Cargo feature that builds against Homebrew's libomemo-c

- `crates/omemo-sys/Cargo.toml` already has `system = []`. `build.rs` then probes `pkg-config` for `libomemo-c >= 0.5` and only runs bindgen.
- Nothing above it can turn the feature on, because `cargo install --path crates/cli --features omemo-sys/system` only accepts features of direct dependencies. [I]
- Add a forwarding chain:
  - `crates/omemo/Cargo.toml`: `[features] system = ["omemo-sys/system"]`
  - `crates/daemon/Cargo.toml`: `system-libomemo = ["omemo/system"]` (next to `integration`)
  - `crates/cli/Cargo.toml`: `system-libomemo = ["agent-chat-daemon/system-libomemo"]`, added to the `[features]` table. The hub work that was being merged while this plan was written already adds that table, with `default = ["hub"]`.
- `--features=system-libomemo` keeps the default features. So the formula and the app both include `agent-chat hub` (the embedded server from `embedded-server.md`) in the same binary.
- The standalone `agent-chat-hub` binary in `crates/server` is not packaged. Add a second `bin.install` or a hub `service` later, if that plan wants one.
- `crates/omemo-sys/build.rs`: add `println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH")`.
- **Verify on macOS and Linux:**

  ```sh
  brew install libomemo-c pkgconf
  cargo build --locked -p agent-chat-cli --features system-libomemo
  otool -L target/debug/agent-chat | grep omemo     # Linux: ldd
  cargo test --locked -p omemo --features system
  ```

  - The concern is whether Homebrew's shared `libomemo-c` exports every symbol the `omemo` crate uses. Examples: `curve_decode_point_ed`, `ec_public_key_get_ed`, `session_cipher_set_version` and the `*_omemo` serializers.
  - The release is the same, but symbol visibility in the shared build is untested. [I]
- **Add a CI step:** a macOS job that installs `libomemo-c` and builds with `--features system-libomemo` (P1-4).

### P0-2. Detect how the binary was installed, and give a stable path

New, shared code in `crates/proto/src/paths.rs`, because `cli`, `daemon` and `gui` all depend on `agent-chat-proto`:

```rust
/// How an `agent-chat` binary was installed, judged from its *canonical* path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// `<prefix>/Cellar/agent-chat/<version>/bin/agent-chat` (Homebrew formula).
    Homebrew { prefix: PathBuf },
    /// `<X>.app/Contents/MacOS/agent-chat` (the desktop app, e.g. from the cask).
    AppBundle { app: PathBuf },
    /// Anything else: target/, ~/.local/bin, cargo install.
    Unmanaged,
}

pub fn install_kind(cli: &Path) -> InstallKind;

/// A path to `cli` that survives upgrades, for SKILL.md, MCP configs and the setup page.
pub fn stable_cli_path(cli: &Path) -> PathBuf;

/// What to show users and agents: the stable path for managed installs, else the first
/// `agent-chat` on PATH, else `cli` itself.
pub fn preferred_cli_path(cli: &Path) -> PathBuf;
```

Rules for `stable_cli_path`:
- **`Homebrew { prefix }`:**
  - Use `prefix/bin/agent-chat` if it canonicalizes to `cli`.
  - Else use `prefix/opt/agent-chat/bin/agent-chat` if it exists. That path survives `brew unlink`.
  - Else use `cli`.
  - Detection works on path components (`…/Cellar/agent-chat/<ver>/bin/agent-chat`), so it also works for `/usr/local`, `/home/linuxbrew/.linuxbrew` and custom prefixes.
- **`AppBundle { app }`:**
  - Use the first of `$HOMEBREW_PREFIX/bin`, `/opt/homebrew/bin` or `/usr/local/bin` whose `agent-chat` canonicalizes to `cli`. That is the cask's `binary` link.
  - Else use `cli` itself. `/Applications/agent-chat.app/Contents/MacOS/agent-chat` is stable, because the cask moves the app to the same place on every upgrade.
- **`Unmanaged`:** `cli`.

Notes:
- Callers must pass a canonical path. On macOS, `std::env::current_exe()` returns the path used to exec the program and doesn't resolve symlinks. [I]
- **Tests:**
  - pure unit tests of `install_kind` on synthetic paths;
  - a tempdir test that builds `p/Cellar/agent-chat/1.0/bin/agent-chat` with a `p/bin/agent-chat` symlink and checks both rules;
  - the same for a fake `X.app/Contents/MacOS/agent-chat`.

### P0-3. `agent-chat install` that knows about Homebrew

**Flag:** `crates/cli/src/main.rs`, in the `Cmd::Install` variant:

```rust
/// Only install the agent skill (and, with --mcp, the MCP server); never copy binaries.
/// Implied when this binary is managed by Homebrew or lives in an app bundle.
#[arg(long, conflicts_with_all = ["bin_dir", "no_skill"])]
skill_only: bool,
```

- Pass it on in `install::InstallOptions`, as a new `skill_only: bool` field.
- **Text output** (the `Format::Text` branch of `Cmd::Install`): for managed installs, print `Using agent-chat from Homebrew: <path>` instead of `Installed <path>`.

**Behaviour:** `crates/cli/src/install.rs`, in `install()`:
1. Compute `let kind = paths::install_kind(&exe)`, where `exe` is already canonical. Set `let copy = !opts.skill_only && kind == InstallKind::Unmanaged`.
2. When `!copy`:
   - Don't call `install_binary` for the CLI or the GUI.
   - Set `cli = paths::stable_cli_path(&exe)`.
   - If the install is managed **and** `--bin-dir` was given, fail with: *"agent-chat is managed by Homebrew (or by agent-chat.app) and updates with `brew upgrade`; it is not copied. Run `agent-chat install --skill-only`."*
3. **Shadowing copies:**
   - If the install is managed and `default_bin_dir()/agent-chat` is a regular file (a copy from an earlier `install`), add a warning. For example: *"~/.local/bin/agent-chat is an old copy and may shadow Homebrew's; remove it with `rm ~/.local/bin/agent-chat ~/.local/bin/agent-chat-gui`"*.
   - `~/.local/bin` often comes before Homebrew in PATH. [I]
4. **JSON output:**
   - Add `"managed_by": "homebrew" | "app" | null` and `"warnings": [...]`. `"cli_updated"` is `false` when nothing was copied.
   - Compute `path_hint` for `cli.parent()`, not for `bin_dir`.
5. **What doesn't change:** the skill (`setup.skill_md()`) and the MCP registration both use `setup.cli`. They now get the stable path:

   ```sh
   claude mcp add --scope user agent-chat -- /opt/homebrew/bin/agent-chat mcp
   ```

6. **The GUI's install button:** `crates/gui/src/app.rs`, `App::run_install`, runs `<cli> --format text install`. The bundled CLI detects `AppBundle` by itself. Pass `--skill-only` explicitly anyway, for clarity.

**Tests:** extend `install_into_temp_dirs_and_uninstall`.
- Add a variant with `skill_only: true`: it creates no `bin/`, and the skill references the exe path.
- Add a test for a fake Cellar layout. That needs `install()` to take the exe path as a parameter internally (`install_with_exe(exe, profile, opts)`).

### P0-4. Every "CLI path" shown to users goes through `preferred_cli_path`

- **`crates/daemon/src/web/mod.rs`, `preferred_cli_path()`:**
  - Becomes `paths::preferred_cli_path(&current_exe()?.canonicalize()?)`, keeping its current fallback.
  - Today it returns the first `agent-chat` on PATH and then the canonical current exe, which is a **Cellar path** under Homebrew.
  - A daemon started by the GUI inherits launchd's PATH, so it always falls back. [I]
- **`crates/cli/src/main.rs`, `Cmd::Setup`:** already calls `agent_chat_daemon::web::preferred_cli_path()`. No change.
- **`crates/gui/src/app.rs`, `App::agent_setup`:** replace its own PATH search with this:

  ```rust
  DaemonLauncher::locate().ok().and_then(|l| l.program.canonicalize().ok())
      .map(|p| paths::preferred_cli_path(&p))
  ```

  The "Connect an agent" dialog then shows `/opt/homebrew/bin/agent-chat`, not the bundle-internal or Cellar path.

### P0-5. GUI discovery and launch must not write into the Cellar or the bundle

All in `crates/cli/src/install.rs`:
- **`find_gui()`:** after "next to this binary" and PATH, also try `/Applications/agent-chat.app/Contents/MacOS/agent-chat-gui` and `~/Applications/agent-chat.app/...`. That lets `agent-chat gui` from the formula, or a terminal, open the cask's app.
- **`launch_gui()`:**
  - If the GUI path is inside `<X>.app/Contents/MacOS/`, run `open -a <X>.app --args --profile <name> [--open <jid>]`. LaunchServices then treats it as an app: activation, Dock icon, a single instance. [I]
  - Otherwise, spawn it as today.
- **`build_and_install_gui()`:**
  - Refuse when `install_kind(current_exe)` is `Homebrew` or `AppBundle`. Today it would copy `agent-chat-gui` into the Cellar's `bin/`, or into a signed bundle and break its signature.
  - The error should read: *"The desktop app is the agent-chat cask, which includes the CLI: `brew uninstall agent-chat && brew install --cask gravypouch/tap/agent-chat`."*
  - `source_root()` already returns `None` under Homebrew, because the build directory is gone.
- **`crates/cli/src/main.rs`, `Cmd::Gui`:** make the `(None, false)` error text depend on the install kind: the same cask hint for Homebrew, and today's text otherwise.

### P0-6. `DaemonLauncher::locate()` fallbacks for Finder-launched GUIs

- **`crates/proto/src/client.rs`, `DaemonLauncher::locate()`:** after PATH, try in order:
  - `/opt/homebrew/bin/agent-chat`
  - `/usr/local/bin/agent-chat`
  - `/home/linuxbrew/.linuxbrew/bin/agent-chat`
  - `$HOME/.local/bin/agent-chat`
- Update the error message to list where it looked.
- The bundled app doesn't need this, because the sibling check comes first. A GUI built from source and started from Finder does.

### P1-1. `agent-chat daemon run --supervised` for `brew services`

- **Flag:** `crates/cli/src/main.rs`: `DaemonCmd::Run { #[arg(long)] supervised: bool }`.
- **Signature:** `crates/daemon/src/daemon.rs`: change `run(profile)` to `run(profile, RunOptions { supervised })`.
- **Behaviour when supervised:**
  - If `lock.try_lock()` fails because a daemon started on demand is running, log "waiting for the running daemon to exit". Block in `tokio::task::spawn_blocking(move || lock.lock())`, then continue.
  - Treat "a daemon is already listening" the same way.
  - Without this, launchd restarts a failing service every 10 seconds (the default `throttle_interval`). [I]
- **Exit codes:**
  - 0 after `Request::Shutdown` (`agent-chat daemon stop`). With `keep_alive successful_exit: false`, the service then stays stopped.
  - **75** after a restart request (P1-2), so launchd starts the new binary at `opt_bin`.
  - Map this in `main()` through the existing `Exit(u8, String)` error type.
- **Logs:** under the service, stderr goes to `var/log/agent-chat.log`. Optionally add `--log-file`, so messages like "see daemon.log" stay accurate.
- **One profile per service:**
  - The `service` block runs the default profile. `brew services` supports one service per formula, and `AGENT_CHAT_PROFILE` in the plist environment selects another profile.
  - Say so in the docs. Multi-profile users keep the on-demand daemons.

### P1-2. A daemon version handshake, so upgrades take effect

- **`crates/proto/src/model.rs`, `StatusInfo`:** add `#[serde(default, skip_serializing_if = "Option::is_none")] pub version: Option<String>`.
  - Set it in `crates/daemon/src/core.rs`, `Core::new`, to `Some(env!("CARGO_PKG_VERSION").into())`.
  - All crates inherit the version from `[workspace.package]`.
- **`crates/proto/src/ipc.rs`:** add `Request::Restart`. It shuts down like `Shutdown`, but makes `run()` return a `Restart` outcome: exit 75 when supervised, 0 otherwise.
- **`crates/proto/src/client.rs`, `IpcClient::connect_or_spawn`:**
  - After connecting, call `Status`.
  - If the daemon's version is **older** than the client's (semver compare, and never downgrade), send `Restart`.
  - Wait until the socket is gone. Move the wait loop from `DaemonCmd::Stop` in `main.rs` into a proto helper, `wait_for_exit(&Profile)`.
  - Give a supervised daemon up to 5 seconds to come back, else `spawn`.
- **Why:**
  - `brew upgrade` replaces the formula binary while the old daemon keeps running from the old Cellar directory (§3.6).
  - A cask upgrade does the same, softened by the cask's `uninstall script: daemon stop`.
  - Without the handshake, a new CLI talks to an old daemon until the next reboot.

### P1-3. Versions everywhere

- **`crates/gui/src/main.rs`, `main()`:** handle `--version` before `Profile::resolve`, printing `agent-chat-gui X.Y.Z`. `bundle.sh` and CI can then smoke-test both binaries.
- **`agent-chat status`:** prints the daemon version (P1-2). The CLI's own `--version` already exists [M].

### P1-4. CI coverage for packaging

In `.github/workflows/ci.yml`:
- a macOS step: `brew install libomemo-c pkgconf && cargo build --locked -p agent-chat-cli --features system-libomemo`;
- a macOS step that runs `packaging/macos/bundle.sh --version "$(cargo pkgid -p agent-chat-cli | cut -d'#' -f2)"` with ad-hoc signing, to catch bundle regressions. Use `--skip-build` after a `--target aarch64-apple-darwin` build.

### P1-5. Files that aren't Rust

- `CHANGELOG.md` (§10).
- `packaging/macos/AppIcon.png`: 1024×1024. `bundle.sh` turns it into `.icns` with `sips` and `iconutil`.
- Add `dist/` to `.gitignore`.
- A README install section with the fully qualified `brew` commands.

### P2 (later)

1. **Pregenerated bindings.**
   - Commit `crates/omemo-sys/src/bindings.rs`, generated for LP64 targets.
   - Make `bindgen` an optional build-dependency: `bindgen = { version = "0.73", optional = true }`, `[features] bindgen = ["dep:bindgen"]`.
   - `build.rs` includes the committed file unless the feature is on.
   - Add a CI job that regenerates the file and runs `git diff --exit-code`.
   - Benefit: `cargo install` and Linux users no longer need libclang. Homebrew doesn't need this, because `uses_from_macos "llvm"` is cheap.
   - Keep running bindgen in `system` mode, unless `build.rs` also checks that pkg-config reports exactly the libomemo-c version the bindings were made from.
2. **System SQLite.** Add a default-on `bundled-sqlite` feature in `crates/daemon`, forwarding to `rusqlite/bundled`, so a core formula could use `uses_from_macos "sqlite"`. Only if core reviewers ask.
3. **Shell completions.**
   - Use `clap_complete`'s dynamic `CompleteEnv` in `crates/cli/src/main.rs`, at the top of `main()`. [I: API name]
   - Then add `generate_completions_from_executable(bin/"agent-chat", shell_parameter_format: :clap)` to the formula. The cask already has the matching stanza, commented out.
4. **`agent-chat uninstall`** (`install::uninstall`) also runs `claude mcp remove --scope user <name>`.
5. **Keychain help.**
   - When the daemon cannot read the password, because of a denied keychain prompt or a headless session, the error should name the fix: re-run `agent-chat account add` in a GUI session, or use `--store config`.
   - Optionally add `agent-chat account reauth`, which deletes and re-saves the item so the current binary is on its ACL. [I]
6. **Man page** with `clap_mangen`, installed by the formula with `man1.install`.

## 6. Build requirements, per artifact

| Requirement | Formula (tap and core) | App (cask, ZIP, DMG) | Linux tarballs |
|---|---|---|---|
| libomemo-c | Homebrew `libomemo-c` 0.5.1 via `--features system-libomemo` (needs P0-1). | Vendored and statically linked: no Homebrew dependency, self-contained. | Vendored. |
| bindgen / libclang | CLT on macOS; `uses_from_macos "llvm" => :build` on Linux. | Xcode on the runner. | `clang libclang-dev` from apt. |
| SQLite | Bundled (rusqlite `bundled`). | Bundled. | Bundled. |
| TLS crypto | `aws-lc-sys`, built with `cc`. [I: no cmake needed on these targets] | Same. | Same. |
| Network during the build | Only in `fetch` (`cargo fetch --locked --target host-tuple`); `install` builds `--offline`. | CI has network. | CI has network. |
| Rust | `depends_on "rust" => :build` (1.99.1 today); the workspace needs ≥ 1.92. | `dtolnay/rust-toolchain@stable`. | Same. |

`cargo fetch` downloads the whole workspace's lock file (917 packages, GPUI included), not only the CLI's roughly 300. [M] Builds are slower, but correct. Splitting the GUI into its own workspace would fix it, and isn't worth doing now.

## 7. The macOS app bundle

**Layout** (made by `packaging/macos/bundle.sh`):

```
agent-chat.app/Contents/
  Info.plist           CFBundleExecutable = agent-chat-gui, LSMinimumSystemVersion = 15.0
  PkgInfo
  MacOS/agent-chat-gui the app
  MacOS/agent-chat     CLI, daemon and MCP server (Apple's place for helper tools)
  Resources/AppIcon.icns, LICENSE, THIRD_PARTY_NOTICES.md
```

**Info.plist** (`packaging/macos/Info.plist.in`):
- Bundle id: `io.github.gravypouch.agent-chat`. This is a placeholder (open question 2), and the same id is set in the cask and in `release.yml`.
- `CFBundleShortVersionString` is `X.Y.Z` with any pre-release suffix removed, because it must be three integers.
- `CFBundleVersion` is the CI run number.
- `NSHighResolutionCapable`.
- `LSApplicationCategoryType`: social networking.
- `NSLocalNetworkUsageDescription`, for LAN servers and hubs. [I: macOS's Local Network privacy applies to the app as the daemon's responsible process]

**Signing order** (no `--deep` when signing):
1. `codesign --force --options runtime --timestamp --identifier io.github.gravypouch.agent-chat.cli --sign "$ID" Contents/MacOS/agent-chat`. The fixed identifier keeps the keychain designated requirement stable.
2. `codesign --force --options runtime --timestamp --entitlements agent-chat.entitlements --sign "$ID" agent-chat.app`.
3. `codesign --verify --strict --deep --verbose=2`. Then check that the bundled CLI's `--version` matches the tag.

**Entitlements:** none. The app is not sandboxed, has no JIT, loads no third-party dylibs, and uses runtime-compiled shaders. The file exists so that exceptions are explicit and reviewed.

**Notarization:**
1. `ditto -c -k --keepParent agent-chat.app submit.zip`
2. `xcrun notarytool submit … --wait --timeout 1h --output-format json`. On anything other than `Accepted`, print `notarytool log` and fail.
3. `xcrun stapler staple agent-chat.app` and `stapler validate`.
4. `spctl --assess --type execute -vv` and `codesign --verify -R=notarized --check-notarization`, the same check as `brew audit --signing`.
5. Re-zip.

**DMG:** optional (`--dmg`). The DMG contains the stapled app and an `/Applications` link. It is signed, notarized and stapled itself.

**ZIP vs DMG:**
- The cask uses the **ZIP**: one notarization, simpler extraction, and `ditto` preserves signatures.
- The DMG is for people downloading from the Releases page.
- Both are published.

**Universal vs arm64:** arm64 only (§3.7). `--universal` builds `aarch64` and `x86_64` and runs `lipo`. Cross-compiling `x86_64` on an arm64 runner should work, because Apple's clang targets both and `cc` and bindgen honour `--target`. [I]

**GPL compliance:**
- Releases are binary distributions of GPL-3.0 code: agent-chat itself and libomemo-c.
- The bundle carries `LICENSE` and `THIRD_PARTY_NOTICES.md`.
- Each GitHub Release has the matching source archive at the same tag, which is "equivalent access … from the same place" under GPLv3 §6(d). [I]

## 8. Signing and notarization prerequisites (the user provides these)

**Apple:**
1. **Apple Developer Program membership** ($99 per year). [I: price]
   - As an *individual*, your legal name appears as the developer.
   - As an *organization*, you need a D-U-N-S number and the organization's name appears. See open question 1.
2. **A Developer ID Application certificate**, created by the Account Holder in *Certificates, Identifiers & Profiles*.
   - Export it together with its private key as a `.p12` with a strong password.
   - A *Developer ID Installer* certificate is not needed, because there is no `.pkg`.
   - No provisioning profile is needed, because there are no restricted entitlements. [I]
3. **An App Store Connect API key for notarization:** a Team key under *Users and Access → Integrations*, with the *Developer* role. [I: role name] Download the `.p8` once, and note its Key ID and Issuer ID.
   - The alternative is your Apple ID with an app-specific password. `bundle.sh` supports both; the workflow uses the API key.

**GitHub** (repository settings of `GravyPouch/agent-chat`):

| Secret | Content | Used by |
|---|---|---|
| `MACOS_CERTIFICATE_P12_BASE64` | `base64 -i DeveloperID.p12` | `macos-app`: imported into a temporary keychain, which is deleted at the end. |
| `MACOS_CERTIFICATE_PASSWORD` | The `.p12` export password. | Same. |
| `MACOS_SIGNING_IDENTITY` | `Developer ID Application: NAME (TEAMID)`, exactly as `security find-identity -v -p codesigning` shows it. | `bundle.sh`. |
| `APPLE_API_KEY_P8_BASE64` | `base64 -i AuthKey_XXXX.p8` | `notarytool --key`. |
| `APPLE_API_KEY_ID` | The key's ID. | `notarytool --key-id`. |
| `APPLE_API_ISSUER_ID` | The issuer UUID (team keys). | `notarytool --issuer`. |
| `HOMEBREW_TAP_TOKEN` | A fine-grained personal access token limited to `GravyPouch/homebrew-tap`, with Contents and Pull requests read/write. A GitHub App token also works. | `bump-tap`: pushes a branch and opens the PR. A PAT or App token, unlike `GITHUB_TOKEN`, triggers the tap's CI. [I] |

- **Release environment:** put the six Apple secrets in a GitHub **environment** named `release`, with yourself as a required reviewer. The job already declares `environment: release`. Every release then pauses for one approval before secrets are exposed.
- **Tag protection:** add a ruleset that lets only maintainers create `v*` tags.

## 9. Release automation

**`.github/workflows/release.yml`** runs only on `push: tags: ["v*"]`. Its jobs:

| Job | Runner | Does |
|---|---|---|
| `meta` | ubuntu-latest | Checks the tag against `cargo metadata` (the `agent-chat-cli` version), marks `-rc`/`-beta` tags as prereleases, and extracts `## [X.Y.Z]` from `CHANGELOG.md` as the release notes. Fails if the section is missing or empty. |
| `macos-app` | `macos-26` (arm64), environment `release` | Builds with `MACOSX_DEPLOYMENT_TARGET=15.0`, imports the certificate into a throwaway keychain, writes the `.p8`, and runs `bundle.sh --target aarch64-apple-darwin --dmg`. Refuses to continue without signing secrets. Uploads the ZIP and the DMG, then deletes the keychain and the key in an `if: always()` step. |
| `linux-cli` | `ubuntu-24.04`, `ubuntu-24.04-arm` | Builds `agent-chat` for each target, checks `--version`, and makes `agent-chat-X.Y.Z-<triple>.tar.gz` with LICENSE, README and notices. |
| `release` | ubuntu-latest, `contents: write` | Collects the assets, writes `SHA256SUMS`, and runs `gh release create --verify-tag` (with `--prerelease` for prerelease tags). |
| `bump-tap` | ubuntu-latest | Skipped for prereleases. Downloads the tag's source archive and the app ZIP to compute their SHA-256. Checks out the tap with `HOMEBREW_TAP_TOKEN`. Rewrites only the top-level `url` and `sha256` of `Formula/agent-chat.rb`, and `version` and `sha256` of `Casks/agent-chat.rb`; bottle lines are indented deeper and untouched [M]. Makes one commit per file, then pushes `agent-chat-X.Y.Z` and opens a PR. |

**Tap side** (one-time setup, M0):

```sh
brew tap-new GravyPouch/homebrew-tap      # developer command: turns on developer mode
cp packaging/homebrew/Formula/agent-chat.rb "$(brew --repository gravypouch/tap)/Formula/"
cp packaging/homebrew/Casks/agent-chat.rb  "$(brew --repository gravypouch/tap)/Casks/"
gh repo create GravyPouch/homebrew-tap --public --push --source "$(brew --repository gravypouch/tap)"
brew developer off
```

`tap-new` generates the `brew test-bot` workflow, which builds bottles on PRs, and the `brew pr-pull` workflow. Bottles are uploaded to the tap's GitHub Releases with attestations. [V]

**Each release:**
1. The bump PR builds bottles on macOS arm64 and Linux.
2. You review it.
3. You run `brew pr-pull --tap=gravypouch/tap --head-sha=<sha> <PR>`, or trigger its workflow.
4. That adds the `bottle do` block and merges.

**Formula URL:** the formula uses GitHub's tag archive, `…/archive/refs/tags/vX.Y.Z.tar.gz`. That is the convention in core and what `bump-homebrew-formula-action` assumes. If checksum stability ever becomes a problem, upload a `git archive` tarball as a release asset and point the formula at it (§12, R6).

**Re-runs:**
- Every job can be re-run on its own.
- If `release` already published, delete the release and re-run, or tag a patch version.
- If the repository turns on GitHub's immutable releases, only the second option remains.

## 10. Versioning and CHANGELOG

- **One version.** `[workspace.package] version` in the root `Cargo.toml` is the only place it is set; every crate inherits it [V]. Tags are `vX.Y.Z`, and `meta` enforces that they match.
- **SemVer:** 0.x for now. A minor bump may break the CLI's JSON output or the IPC protocol; a patch bump never does.
- **Pre-releases:** `vX.Y.Z-rc.N`. They become GitHub prereleases with no tap bump. The app gets `CFBundleShortVersionString X.Y.Z`.
- **`CHANGELOG.md`:** use the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) format, with `## [Unreleased]` at the top and `## [X.Y.Z] - YYYY-MM-DD` sections underneath. Pull requests add lines under *Unreleased*.
- **Release checklist** (`cargo-release` can automate steps 1–4):
  1. Move *Unreleased* into `## [X.Y.Z] - date`.
  2. Bump `version` in `Cargo.toml`, then run `cargo update --workspace`. **Required:** `--locked` builds fail on a stale `Cargo.lock`.
  3. Commit `Release X.Y.Z`.
  4. `git tag -s vX.Y.Z && git push origin master vX.Y.Z`
  5. Approve the `release` environment.
  6. Review and `pr-pull` the tap PR.
- **First version:** the workspace says `0.1.0`. Dry-run as `0.1.0-rc.1` first (M4).

## 11. Milestones

| # | Milestone | Steps | Done when |
|---|---|---|---|
| M0 | Decisions and accounts | Answer §13. Enrol in the Apple Developer Program, create the certificate and the API key. Create the tap repository (§9). Make `agent-chat` public. | Accounts exist; the tap repository exists. |
| M1 | P0 code changes | Implement P0-1 to P0-6 with tests. Verify `system-libomemo` against Homebrew's libomemo-c on macOS and in an Ubuntu container with Linuxbrew. | `cargo test` passes; `agent-chat install --skill-only` behaves as in §5 for a fake Cellar layout. |
| M2 | Local formula and cask dry run | `git archive --prefix=agent-chat-0.1.0/ -o /tmp/agent-chat-0.1.0.tar.gz HEAD`. In a local tap, point `url` at `file:///tmp/…` with its `sha256`. Run `brew trust gravypouch/tap`, `HOMEBREW_NO_INSTALL_FROM_API=1 brew install --build-from-source gravypouch/tap/agent-chat`, `brew test`, `brew audit --strict --online`, `brew style`, and `brew services start` then `brew services stop`. Check the formula/cask conflict behaviour (§4.3). | All green. Agents see `/opt/homebrew/bin/agent-chat` in SKILL.md and in `claude mcp get agent-chat`. |
| M3 | Signing | Run `bundle.sh` locally with the real identity and API key. Install the ZIP by hand on a second Mac or user account, and open it from Finder: no Gatekeeper dialog other than the first-launch confirmation. Test on macOS 15, 26 and 27. Run `brew audit --signing --cask` against a local copy of the cask. | `spctl` says "Notarized Developer ID". |
| M4 | P1 changes and pipeline dry run | Implement P1-1 to P1-5. Set the version to `0.1.0-rc.1`, tag and push. | A GitHub prerelease with a notarized ZIP, DMG, Linux tarballs and SHA256SUMS. No tap PR, as intended. |
| M5 | First release | Set the version to `0.1.0`, tag it, approve the environment. The tap PR builds bottles; run `pr-pull`. On a clean Mac, test `brew install gravypouch/tap/agent-chat`, then the cask instead, `agent-chat install --skill-only --mcp`, and the GUI. | Both install paths work from bottles and the ZIP. |
| M6 | Upgrade path | Release `0.1.1`. `brew upgrade` both ways. Check that the daemon restarts on the new version (P1-2), that `brew services` restarts it (exit 75), and the keychain behaviour (R1). | No stale daemon; keychain prompts documented or absent (cask). |
| M7 | Official repositories | When §3.1's thresholds are met, open a homebrew-core PR with the formula (no bottle block, `brew audit --new`, `brew lgtm --online`, AI-assistance disclosed in the PR template). Then a homebrew/cask PR. Add `tap_migrations.json` to the tap. | Merged upstream. |

## 12. Risks

| # | Risk | Likelihood / impact | Mitigation |
|---|---|---|---|
| R1 | **Keychain prompts after each formula upgrade.** The bottle is ad-hoc signed, so its designated requirement (the cdhash) changes every build. A daemon running headless (SSH, or a service before login) can't answer the prompt. | High / medium | The cask's Developer ID-signed CLI has a stable identifier. Formula caveat ("Always Allow"). P2-5's error hint. `--store config` for headless hosts. Verify in M6. [I] |
| R2 | Formula and cask conflict on `bin/agent-chat` and confuse users. | Medium / low | Caveats in both, a README table (§4.2), and a clear `brew` error. Casks cannot declare formula conflicts [V]. |
| R3 | Version skew: an old daemon serves a new CLI or GUI after an upgrade. | High without P1-2 / medium | P1-2 handshake; the cask's `uninstall script: daemon stop`; `--supervised` with exit 75. |
| R4 | Notarization slow or failing; Apple service outages. | Low / medium | `--wait --timeout 1h`, the log printed on failure, jobs re-runnable on their own. |
| R5 | Leaked signing secrets. | Low / high | The `release` environment with a reviewer, a throwaway keychain, the `.p8` deleted with `if: always()`, actions pinned to SHAs, tag rulesets. Revoke the certificate and the API key if exposed. |
| R6 | The checksum of GitHub's tag archive changes. | Low / medium | Upload our own `git archive` tarball as a release asset and switch `url`. |
| R7 | homebrew-core reviewers object to bundled SQLite or to the size of the fetch. | Medium / low | P2-2. Splitting the workspace if needed. |
| R8 | The app misbehaves on macOS 27, the latest version, which homebrew/cask requires. | Medium / medium | Test on 27 in M3. CI moves to a macOS 27 runner image when GitHub offers one. |
| R9 | The bundle id or display name changes after the first signed release. That resets keychain ACLs, preferences, `zap` paths and Local Network permission. | Low / high | Decide in M0 (questions 2 and 3) and never change them. |
| R10 | Tap trust adds friction: short names fail until trusted. | Certain / low | Document the fully qualified commands only. |
| R11 | Intel users get no app. | Certain / low | Intel is Tier 3 and is being retired. The formula still builds from source. `--universal` exists. |
| R12 | Homebrew's shared libomemo-c hides symbols the vendored build exposes, so `system-libomemo` fails to link. | Low / medium | M1 checks it. Fallback: the formula builds the vendored copy, with a visible-exception comment in the formula as core's vendoring rule asks. |
| R13 | `brew audit --online` disagrees with the minimum macOS version derived from the binaries. | Low / low | `MACOSX_DEPLOYMENT_TARGET=15.0` in `bundle.sh`; `LSMinimumSystemVersion` 15.0; `depends_on macos: :sequoia`. |

## 13. Open questions for the user

1. **Apple account:** individual (your legal name shown as developer) or organization (D-U-N-S, the company name shown)? Who is the Account Holder?
2. **Bundle id:** `io.github.gravypouch.agent-chat` is a placeholder. Do you own a domain to use instead (`com.example.agentchat`)? It can never change (R9).
3. **App name:** `agent-chat.app`, as drafted, or `Agent Chat.app`? The cask token stays `agent-chat` either way.
4. **Cask design:** confirm "the cask bundles the CLI and conflicts with the formula" (§4.3), rather than a GUI-only cask with `depends_on formula:`.
5. **Architectures:** arm64 only (recommended), or a universal app for Intel Macs on macOS 15 and 26?
6. **Artifacts:** ZIP and DMG (drafted), or ZIP only? Keep the Linux CLI tarballs in releases?
7. **`brew services`:** offer it (one profile, the default), or keep only on-demand daemons and drop the `service` block and P1-1?
8. **First release:** do an `0.1.0-rc.1` dry run first, as planned? When will the repository go public? Remember the 30-day minimum age for core.
9. **Tap automation:** a fine-grained PAT or a GitHub App for `HOMEBREW_TAP_TOKEN`? Who reviews the `release` environment?
10. **App icon:** who provides the 1024×1024 PNG?
11. **`zap`:** it removes `~/Library/Application Support/agent-chat`, which holds message history, OMEMO keys and settings, plus `~/.claude/skills/agent-chat`. This is standard for apps, but the formula shares that data. Keep it?
12. **Core submission:** wait for a third party to submit (75 stars), or submit yourself at 225?

## 14. Sources

**Homebrew** (local copies in `/opt/homebrew/docs` for 7.0.13; the online versions are at docs.brew.sh):
- Package acceptance policy: <https://docs.brew.sh/Package-Acceptance-Policy>
- Acceptable formulae: <https://docs.brew.sh/Acceptable-Formulae>
- Acceptable casks: <https://docs.brew.sh/Acceptable-Casks>
- Language-specific formulae, Rust: <https://docs.brew.sh/Language-Specific-Formulae#rust>
- Formula Cookbook (`fetch`, standard arguments, service files, tests): <https://docs.brew.sh/Formula-Cookbook>
- Cask Cookbook (stanza order, `binary`, `conflicts_with`, `depends_on`, `uninstall`, `zap`, `generate_completions_from_executable`): <https://docs.brew.sh/Cask-Cookbook>
- Tap trust: <https://docs.brew.sh/Tap-Trust>
- How to create and maintain a tap: <https://docs.brew.sh/How-to-Create-and-Maintain-a-Tap>
- Adding software to Homebrew: <https://docs.brew.sh/Adding-Software-to-Homebrew>
- Security and supply chain (the cask trust model): <https://docs.brew.sh/Homebrew-Security-and-Supply-Chain>
- Deprecating, disabling and removing: <https://docs.brew.sh/Deprecating-Disabling-and-Removing>
- Licence guidelines: <https://docs.brew.sh/Licence-Guidelines>
- Support tiers: <https://docs.brew.sh/Support-Tiers>
- Autobump: <https://docs.brew.sh/Autobump>

**Homebrew code and formulae:**
- Homebrew 7.0.13 source: `Library/Homebrew/formula.rb` (`std_cargo_args`, `std_cargo_fetch_args`, `deny_network_access!`), `cask/dsl/conflicts_with.rb` (`VALID_KEYS = [:cask]`), `cask/audit.rb` (`audit_signing`, `notarization_check`).
- Release notes: <https://brew.sh/2025/11/12/homebrew-5.0.0/>, <https://brew.sh/2026/06/11/homebrew-6.0.0/> and <https://brew.sh/2026/09/13/homebrew-7.0.0/>
- The `sequoia-sq` formula (`uses_from_macos "llvm" => :build`, `fetch`): <https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/s/sequoia-sq.rb>
- The `libomemo-c` formula: <https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/lib/libomemo-c.rb>
- libomemo-c tags (v0.5.1 = `1f0b37fe…`): <https://api.github.com/repos/dino/libomemo-c/tags>

**Apple:**
- Notarizing macOS software before distribution: <https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution>
- Customizing the notarization workflow: <https://developer.apple.com/documentation/security/customizing-the-notarization-workflow>
- Placing content in a bundle: <https://developer.apple.com/documentation/bundleresources/placing-content-in-a-bundle>
- Sequoia's Gatekeeper change [V: search]: <https://www.macrumors.com/2024/08/06/macos-sequoia-gatekeeper-security-change/> and <https://appleinsider.com/articles/24/08/06/apple-removes-control-click-option-for-skipping-gatekeeper-in-macos-sequoia>

**Tools and runners:**
- cargo-packager: <https://github.com/crabnebula-dev/cargo-packager>, <https://docs.rs/cargo-packager/latest/cargo_packager/config/struct.MacOsConfig.html> and <https://docs.rs/cargo-packager/latest/cargo_packager/config/struct.Config.html>
- cargo-bundle: <https://github.com/burtonageo/cargo-bundle>
- bump-homebrew-formula-action: <https://github.com/mislav/bump-homebrew-formula-action>
- GitHub macOS 26 runners: <https://github.blog/changelog/2026-02-26-macos-26-is-now-generally-available-for-github-hosted-runners/>
- Keep a Changelog: <https://keepachangelog.com/en/1.1.0/>
