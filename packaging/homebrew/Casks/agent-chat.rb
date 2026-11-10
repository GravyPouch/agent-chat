# DRAFT for GravyPouch/homebrew-tap (Casks/agent-chat.rb). Do not publish as is:
#   * version/sha256 are placeholders; the release workflow rewrites both lines.
#   * The bundle id must match BUNDLE_ID in packaging/macos/bundle.sh and
#     .github/workflows/release.yml (see the open questions in docs/design/homebrew.md).
#
# This cask bundles the CLI instead of using `depends_on formula:`. The app runs
# Contents/MacOS/agent-chat as its daemon (DaemonLauncher::locate looks next to the GUI
# first), so the GUI and the daemon are always the same, Developer ID-signed version, and
# a Finder-launched app does not depend on Homebrew's bin being on launchd's PATH.
# Reasons and trade-offs: docs/design/homebrew.md, section 4.
#
# The `binary` link conflicts with the agent-chat formula's bin/agent-chat. Casks can
# only declare `conflicts_with cask:`, so the caveats say it instead.
cask "agent-chat" do
  version "0.1.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/GravyPouch/agent-chat/releases/download/v#{version}/agent-chat-#{version}-macos-arm64.zip"
  name "agent-chat"
  desc "End-to-end encrypted XMPP messenger for people and AI agents"
  homepage "https://github.com/GravyPouch/agent-chat"

  livecheck do
    url :url
    strategy :github_latest
  end

  depends_on arch: :arm64
  depends_on macos: :sequoia

  app "agent-chat.app"
  binary "#{appdir}/agent-chat.app/Contents/MacOS/agent-chat"
  # Once the CLI supports clap_complete's dynamic completions (P2-3 in the plan):
  # generate_completions_from_executable "agent-chat.app/Contents/MacOS/agent-chat",
  #                                      shell_parameter_format: :clap

  # Stop the default profile's daemon, so an upgrade does not leave the old binary
  # running; the next CLI or app call starts the new one. Exit code 3 (not running) is fine.
  uninstall quit:   "io.github.gravypouch.agent-chat",
            script: {
              executable:   "#{appdir}/agent-chat.app/Contents/MacOS/agent-chat",
              args:         ["daemon", "stop"],
              must_succeed: false,
              print_stderr: false,
            }

  # Message history, OMEMO identity keys and account settings live in Application
  # Support and are shared with the formula. Keychain items are not removed.
  zap trash: [
    "~/.claude/skills/agent-chat",
    "~/Library/Application Support/agent-chat",
    "~/Library/Caches/io.github.gravypouch.agent-chat",
    "~/Library/HTTPStorages/io.github.gravypouch.agent-chat",
    "~/Library/Preferences/io.github.gravypouch.agent-chat.plist",
    "~/Library/Saved Application State/io.github.gravypouch.agent-chat.savedState",
  ]

  caveats <<~EOS
    The agent-chat CLI is included and linked into #{HOMEBREW_PREFIX}/bin.
    To install the agent-chat skill for Claude Code and register the MCP server:
      agent-chat install --skill-only --mcp

    This cask cannot be installed alongside the agent-chat formula; uninstall the
    formula first with `brew uninstall agent-chat`.
  EOS
end
