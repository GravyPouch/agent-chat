# DRAFT for GravyPouch/homebrew-tap (Formula/agent-chat.rb). Do not publish as is:
#   * url/sha256 are placeholders until v0.1.0 is tagged; the release workflow
#     (.github/workflows/release.yml) rewrites both lines on every release.
#   * It relies on code changes listed in docs/design/homebrew.md, section 5:
#     the `system-libomemo` feature (P0-1), `install --skill-only` and Cellar
#     detection (P0-2), and `daemon run --supervised` (P1-1).
#   * The `bottle do` block is added by `brew pr-pull` in the tap; never write it by hand.
class AgentChat < Formula
  desc "End-to-end encrypted XMPP messenger and MCP server for AI agents"
  homepage "https://github.com/GravyPouch/agent-chat"
  url "https://github.com/GravyPouch/agent-chat/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "GPL-3.0-or-later"
  head "https://github.com/GravyPouch/agent-chat.git", branch: "master"

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on "pkgconf" => :build
  depends_on "rust" => :build
  # Homebrew's libomemo-c 0.5.1 is the same release as third_party/libomemo-c
  # (commit 1f0b37f = tag v0.5.1), so the vendored copy is not built.
  depends_on "libomemo-c"

  # bindgen in crates/omemo-sys/build.rs needs libclang: the Command Line Tools
  # provide it on macOS, Homebrew's llvm on Linux.
  uses_from_macos "llvm" => :build

  deny_network_access!

  # `cargo fetch` runs with network access; `install` then builds offline
  # (std_cargo_args adds --offline because this method exists).
  def fetch
    system "cargo", "fetch", *std_cargo_fetch_args
  end

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/cli", features: "system-libomemo")
  end

  def caveats
    <<~EOS
      To install the agent-chat skill for Claude Code and register the MCP server:
        agent-chat install --skill-only --mcp

      The desktop app is the agent-chat cask. It bundles its own copy of this CLI,
      so uninstall this formula before installing it:
        brew uninstall agent-chat
        brew install --cask gravypouch/tap/agent-chat

      Account passwords are kept in the macOS keychain. After an upgrade, macOS may
      ask again whether agent-chat may read them; choose "Always Allow".
    EOS
  end

  # Runs the daemon of the default profile (set AGENT_CHAT_PROFILE to change it).
  # Without the service, the CLI starts the daemon on demand, as before.
  service do
    run [opt_bin/"agent-chat", "daemon", "run", "--supervised"]
    # `agent-chat daemon stop` exits 0 and stays stopped; crashes and
    # version-upgrade restarts (exit code 75) are restarted by launchd/systemd.
    keep_alive successful_exit: false
    environment_variables PATH: std_service_path_env
    log_path var/"log/agent-chat.log"
    error_log_path var/"log/agent-chat.log"
    stop_timeout 15
  end

  test do
    # A short base directory keeps the daemon's Unix socket inside testpath (the
    # path limit is 100 bytes); the test sandbox only allows sockets there.
    ENV["AGENT_CHAT_HOME"] = testpath/"d"
    (testpath/"d/profiles/default").mkpath
    # No setup page: the daemon must not listen on TCP during the test.
    (testpath/"d/profiles/default/config.toml").write <<~TOML
      [settings]
      http_port = 0
    TOML

    assert_equal "agent-chat #{version}", shell_output("#{bin}/agent-chat --version").strip

    # `daemon status` never starts a daemon: exit code 3 when none is running.
    assert_match "daemon is not running", shell_output("#{bin}/agent-chat daemon status 2>&1", 3)

    # `status` starts the per-profile daemon on demand (SQLite store, IPC over
    # the Unix socket) and reports that no account is configured.
    begin
      status = JSON.parse(shell_output("#{bin}/agent-chat status"))
      assert_equal "default", status["profile"]
      assert_equal "no_account", status["state"]
    ensure
      system bin/"agent-chat", "daemon", "stop"
    end

    # Homebrew-aware install: the skill points at a path that survives upgrades,
    # and the binary is not copied to ~/.local/bin (HOME is testpath here).
    system bin/"agent-chat", "install", "--skill-only", "--skills-dir", testpath/"skills"
    skill = (testpath/"skills/agent-chat/SKILL.md").read
    refute_match "/Cellar/", skill
    assert_match %r{#{Regexp.escape(HOMEBREW_PREFIX.to_s)}/(?:bin|opt/agent-chat/bin)/agent-chat}, skill
    refute_path_exists testpath/".local/bin/agent-chat"
  end
end
