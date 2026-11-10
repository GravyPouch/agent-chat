#!/usr/bin/env bash
# Build, assemble, sign, notarize and package agent-chat.app, with the CLI inside.
#
# Usage:
#   packaging/macos/bundle.sh --version X.Y.Z [--target TRIPLE | --universal]
#                             [--out DIR] [--skip-build] [--dmg]
#
#   --target      Rust target to build (default: aarch64-apple-darwin).
#   --universal   Build aarch64 and x86_64 and merge them with lipo.
#   --out         Output directory (default: dist/ in the repository).
#   --skip-build  Use binaries already in target/<triple>/release.
#   --dmg         Also produce a signed, notarized and stapled .dmg.
#
# Bundle layout:
#   agent-chat.app/Contents/Info.plist               from Info.plist.in
#   agent-chat.app/Contents/PkgInfo
#   agent-chat.app/Contents/MacOS/agent-chat-gui     CFBundleExecutable
#   agent-chat.app/Contents/MacOS/agent-chat         CLI, daemon and MCP server; the cask
#                                                    links it into $(brew --prefix)/bin
#   agent-chat.app/Contents/Resources/AppIcon.icns   from AppIcon.icns or AppIcon.png, if present
#   agent-chat.app/Contents/Resources/LICENSE, THIRD_PARTY_NOTICES.md
#
# Outputs in --out: agent-chat-X.Y.Z-macos-ARCH.zip (the cask downloads this), the .dmg with
# --dmg, a .sha256 file for each, and the unpacked agent-chat.app.
#
# Environment. Everything is optional: without SIGNING_IDENTITY the app is ad-hoc signed
# and not notarized, which is enough for local testing only.
#   SIGNING_IDENTITY  "Developer ID Application: NAME (TEAMID)"
#   KEYCHAIN_PATH     keychain that holds the identity (CI uses a temporary one)
#   BUNDLE_ID         default: io.github.gravypouch.agent-chat
#   BUILD_NUMBER      CFBundleVersion; default: the numeric part of --version
#   Notarization with an App Store Connect API key (preferred):
#     NOTARY_KEY_PATH, NOTARY_KEY_ID, NOTARY_ISSUER (leave NOTARY_ISSUER unset for an
#     individual key)
#   Notarization with an Apple ID (alternative):
#     NOTARY_APPLE_ID, NOTARY_PASSWORD (an app-specific password), NOTARY_TEAM_ID
#
# Written for the bash 3.2 that ships with macOS: optional arrays are expanded as
# ${a[@]+"${a[@]}"} because "${a[@]}" of an empty array fails under `set -u` there.

set -euo pipefail

die() { echo "bundle.sh: $*" >&2; exit 1; }
log() { echo "==> $*" >&2; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HERE="$ROOT/packaging/macos"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

version=""
targets=()
out="$ROOT/dist"
build=1
dmg=0
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="${2:?--version needs a value}"; shift 2 ;;
    --target) targets=("${2:?--target needs a value}"); shift 2 ;;
    --universal) targets=(aarch64-apple-darwin x86_64-apple-darwin); shift ;;
    --out) out="${2:?--out needs a value}"; shift 2 ;;
    --skip-build) build=0; shift ;;
    --dmg) dmg=1; shift ;;
    -h | --help) sed -n '2,38p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ "$(uname -s)" = Darwin ] || die "this script only runs on macOS"
[ -n "$version" ] || die "--version is required"
[ ${#targets[@]} -gt 0 ] || targets=(aarch64-apple-darwin)

# CFBundleShortVersionString must be three integers, so drop a pre-release suffix.
short_version="${version%%-*}"
[[ "$short_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must look like 1.2.3 or 1.2.3-rc.1, got $version"
build_version="${BUILD_NUMBER:-$short_version}"
bundle_id="${BUNDLE_ID:-io.github.gravypouch.agent-chat}"

if [ ${#targets[@]} -gt 1 ]; then
  arch=universal
else
  case "${targets[0]}" in
    aarch64-apple-darwin) arch=arm64 ;;
    x86_64-apple-darwin) arch=x86_64 ;;
    *) die "unsupported target ${targets[0]}" ;;
  esac
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# ------------------------------------------------------------------ build

# Matches LSMinimumSystemVersion, so the binaries' LC_BUILD_VERSION agrees with Info.plist.
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-15.0}"

if [ "$build" = 1 ]; then
  for t in "${targets[@]}"; do
    log "cargo build --release --target $t"
    cargo build --release --locked --manifest-path "$ROOT/Cargo.toml" --target "$t" \
      -p agent-chat-cli -p agent-chat-gui
  done
fi

for bin in agent-chat agent-chat-gui; do
  inputs=()
  for t in "${targets[@]}"; do
    f="$TARGET_DIR/$t/release/$bin"
    [ -x "$f" ] || die "missing $f (build it first, or drop --skip-build)"
    inputs+=("$f")
  done
  if [ ${#inputs[@]} -gt 1 ]; then
    lipo -create -output "$work/$bin" "${inputs[@]}"
  else
    cp "${inputs[0]}" "$work/$bin"
  fi
done

# ------------------------------------------------------------------ assemble

mkdir -p "$out"
app="$out/agent-chat.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$work/agent-chat-gui" "$work/agent-chat" "$app/Contents/MacOS/"
chmod 755 "$app/Contents/MacOS/agent-chat-gui" "$app/Contents/MacOS/agent-chat"

sed -e "s|@BUNDLE_ID@|$bundle_id|g" \
  -e "s|@SHORT_VERSION@|$short_version|g" \
  -e "s|@BUILD_VERSION@|$build_version|g" \
  -e "s|@YEAR@|$(date +%Y)|g" \
  "$HERE/Info.plist.in" >"$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"
cp "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES.md" "$app/Contents/Resources/"

if [ -f "$HERE/AppIcon.icns" ]; then
  cp "$HERE/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
elif [ -f "$HERE/AppIcon.png" ]; then
  # A 1024x1024 PNG; build the .icns from it.
  iconset="$work/AppIcon.iconset"
  mkdir "$iconset"
  for s in 16 32 128 256 512; do
    sips -z "$s" "$s" "$HERE/AppIcon.png" --out "$iconset/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) "$HERE/AppIcon.png" --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
  done
  iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
else
  log "no packaging/macos/AppIcon.png: the app gets the generic icon"
  plutil -remove CFBundleIconFile "$app/Contents/Info.plist"
fi
plutil -lint "$app/Contents/Info.plist" >/dev/null

# ------------------------------------------------------------------ sign

identity="${SIGNING_IDENTITY:-}"
keychain_args=()
if [ -n "${KEYCHAIN_PATH:-}" ]; then
  keychain_args=(--keychain "$KEYCHAIN_PATH")
fi
sign_args=(--force --options runtime)
if [ -n "$identity" ]; then
  sign_args+=(--timestamp --sign "$identity")
else
  log "SIGNING_IDENTITY is not set: ad-hoc signing, no notarization (local testing only)"
  sign_args+=(--sign -)
fi

# Nested code first, then the bundle. No --deep: Apple advises against it for signing.
# The CLI gets its own stable identifier, so its keychain access survives upgrades.
log "signing with ${identity:-an ad-hoc signature}"
codesign "${sign_args[@]}" ${keychain_args[@]+"${keychain_args[@]}"} \
  --identifier "$bundle_id.cli" "$app/Contents/MacOS/agent-chat"
codesign "${sign_args[@]}" ${keychain_args[@]+"${keychain_args[@]}"} \
  --entitlements "$HERE/agent-chat.entitlements" "$app"
codesign --verify --strict --deep --verbose=2 "$app"

got="$("$app/Contents/MacOS/agent-chat" --version)"
[ "$got" = "agent-chat $version" ] || die "the bundled CLI reports '$got', expected 'agent-chat $version'"

# ------------------------------------------------------------------ notarize

notary_args=()
if [ -n "${NOTARY_KEY_PATH:-}" ]; then
  notary_args=(--key "$NOTARY_KEY_PATH" --key-id "${NOTARY_KEY_ID:?NOTARY_KEY_ID is required with NOTARY_KEY_PATH}")
  if [ -n "${NOTARY_ISSUER:-}" ]; then
    notary_args+=(--issuer "$NOTARY_ISSUER")
  fi
elif [ -n "${NOTARY_APPLE_ID:-}" ]; then
  notary_args=(--apple-id "$NOTARY_APPLE_ID"
    --password "${NOTARY_PASSWORD:?NOTARY_PASSWORD is required with NOTARY_APPLE_ID}"
    --team-id "${NOTARY_TEAM_ID:?NOTARY_TEAM_ID is required with NOTARY_APPLE_ID}")
fi

# Submit a .zip or .dmg and wait; print Apple's log and fail unless it is Accepted.
notarize() {
  local file="$1" result status id
  result="$work/notary-$(basename "$file").json"
  log "notarizing $(basename "$file") (usually a few minutes)"
  xcrun notarytool submit "$file" ${notary_args[@]+"${notary_args[@]}"} \
    --wait --timeout 1h --output-format json >"$result" || true
  status="$(plutil -extract status raw -o - "$result" 2>/dev/null || echo unknown)"
  if [ "$status" != Accepted ]; then
    id="$(plutil -extract id raw -o - "$result" 2>/dev/null || true)"
    if [ -n "$id" ]; then
      xcrun notarytool log "$id" ${notary_args[@]+"${notary_args[@]}"} >&2 || true
    fi
    die "notarization of $(basename "$file") failed: status=$status"
  fi
}

notarized=0
if [ -n "$identity" ] && [ ${#notary_args[@]} -gt 0 ]; then
  # A ZIP can be notarized but not stapled: staple the app, then zip it again.
  ditto -c -k --keepParent "$app" "$work/submit.zip"
  notarize "$work/submit.zip"
  xcrun stapler staple "$app"
  xcrun stapler validate "$app"
  spctl --assess --type execute --verbose=2 "$app"
  # The same check `brew audit --signing` runs for casks.
  codesign --verify -R=notarized --check-notarization "$app"
  notarized=1
elif [ -n "$identity" ]; then
  log "no notarization credentials: the app is signed but not notarized"
fi

# ------------------------------------------------------------------ package

zip="$out/agent-chat-$version-macos-$arch.zip"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
artifacts=("$zip")

if [ "$dmg" = 1 ]; then
  dmg_path="$out/agent-chat-$version-macos-$arch.dmg"
  stage="$work/dmg"
  mkdir "$stage"
  ditto "$app" "$stage/agent-chat.app"
  ln -s /Applications "$stage/Applications"
  rm -f "$dmg_path"
  hdiutil create -volname "agent-chat $version" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg_path" >/dev/null
  if [ -n "$identity" ]; then
    codesign --force --timestamp --sign "$identity" ${keychain_args[@]+"${keychain_args[@]}"} "$dmg_path"
  fi
  if [ "$notarized" = 1 ]; then
    notarize "$dmg_path"
    xcrun stapler staple "$dmg_path"
    xcrun stapler validate "$dmg_path"
  fi
  artifacts+=("$dmg_path")
fi

for f in "${artifacts[@]}"; do
  (cd "$out" && shasum -a 256 "$(basename "$f")" >"$(basename "$f").sha256")
  log "wrote $f"
done
