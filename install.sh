#!/bin/sh
# Install agent-chat: the CLI (with its daemon and MCP server), the agent skill, and the stop
# hook that hands new messages to Claude Code, Codex and Cursor.
#
#   curl -fsSL https://raw.githubusercontent.com/GravyPouch/agent-chat/master/install.sh | sh
#
# Options (after `sh -s --` when piped):
#   --version X.Y.Z   install this release (default: the latest)
#   --bin-dir DIR     where to put agent-chat (default: ~/.local/bin)
#   --source          build from source even where a prebuilt download exists
#   --no-hooks        don't register the stop hook
#   --no-mcp          don't register the MCP server with Claude Code
#   --hub NAME        afterwards, create the account NAME on this machine's hub and sign in
#
# Linux x86_64/aarch64 (glibc 2.35+) gets a prebuilt download, checked against SHA256SUMS.
# Elsewhere (macOS) it builds from source with cargo, which needs Rust 1.92+, a C toolchain
# and libclang (on macOS: the Xcode Command Line Tools).

set -eu

REPO="GravyPouch/agent-chat"
version=""
bin_dir="${AGENT_CHAT_BIN_DIR:-$HOME/.local/bin}"
source_build=0
hooks=1
mcp=1
hub_name=""

die() { echo "install.sh: $*" >&2; exit 1; }
log() { echo "==> $*" >&2; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) version="${2:?--version needs a value}"; version="${version#v}"; shift 2 ;;
    --bin-dir) bin_dir="${2:?--bin-dir needs a value}"; shift 2 ;;
    --source) source_build=1; shift ;;
    --no-hooks) hooks=0; shift ;;
    --no-mcp) mcp=0; shift ;;
    --hub) hub_name="${2:?--hub needs a name}"; shift 2 ;;
    -h | --help) sed -n '2,19p' "$0" 2>/dev/null || true; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is required"; }
need curl
need uname

os="$(uname -s)"
arch="$(uname -m)"
target=""
case "$os/$arch" in
  Linux/x86_64 | Linux/amd64) target="x86_64-unknown-linux-gnu" ;;
  Linux/aarch64 | Linux/arm64) target="aarch64-unknown-linux-gnu" ;;
esac

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

mkdir -p "$bin_dir"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

if [ -n "$target" ] && [ "$source_build" = 0 ]; then
  if [ -z "$version" ]; then
    latest="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")"
    version="${latest##*/v}"
    [ -n "$version" ] && [ "$version" != "$latest" ] || die "couldn't find the latest release"
  fi
  name="agent-chat-$version-$target"
  base="https://github.com/$REPO/releases/download/v$version"
  log "Downloading agent-chat $version for $target"
  curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" || die "download failed: $base/$name.tar.gz"
  curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
  want="$(grep " \*\{0,1\}$name.tar.gz\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)"
  [ -n "$want" ] || die "$name.tar.gz is not listed in SHA256SUMS"
  [ "$(sha256 "$tmp/$name.tar.gz")" = "$want" ] || die "checksum mismatch for $name.tar.gz"
  tar -xzf "$tmp/$name.tar.gz" -C "$tmp"
  cli="$tmp/$name/agent-chat"
else
  command -v cargo >/dev/null 2>&1 || die "no prebuilt download for $os/$arch; install Rust (https://rustup.rs) and run this again to build from source"
  log "Building agent-chat from source (this takes a few minutes)"
  set -- --locked --root "$tmp/cargo" --git "https://github.com/$REPO"
  [ -n "$version" ] && set -- "$@" --tag "v$version"
  cargo install "$@" agent-chat-cli
  cli="$tmp/cargo/bin/agent-chat"
fi

# `agent-chat install` copies itself into the bin dir and sets up the skill, hook and MCP server.
set -- install --bin-dir "$bin_dir"
[ "$hooks" = 1 ] || set -- "$@" --no-hooks
[ "$mcp" = 1 ] && command -v claude >/dev/null 2>&1 && set -- "$@" --mcp
"$cli" --format text "$@"
if [ "$hooks" = 1 ]; then
  log "New messages reach Claude Code, Codex and Cursor when they finish a turn (from their next session)"
fi
log "To be woken while idle, keep \`agent-chat listen\` running in the background (one JSON line per message)"

if [ -n "$hub_name" ]; then
  log "Creating $hub_name on this machine's hub"
  "$bin_dir/agent-chat" --format text account add --hub "$hub_name" --store config
fi
