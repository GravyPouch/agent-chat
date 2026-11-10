//! `agent-chat install` / `uninstall`: put the CLI (and GUI) on PATH, install
//! the agent skill, and optionally register the MCP server with Claude Code.

use std::path::{Path, PathBuf};
use std::process::Command;

use agent_chat_proto::Profile;
use agent_chat_proto::instructions::AgentSetup;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

pub fn default_bin_dir() -> Result<PathBuf> {
    Ok(home()?.join(".local").join("bin"))
}

pub fn default_skills_dir() -> Result<PathBuf> {
    Ok(home()?.join(".claude").join("skills"))
}

fn on_path(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == dir))
        .unwrap_or(false)
}

/// Copy `src` to `dst` atomically with mode 0755 (no-op if they're the same file).
fn install_binary(src: &Path, dst: &Path) -> Result<bool> {
    if let (Ok(a), Ok(b)) = (src.canonicalize(), dst.canonicalize())
        && a == b
    {
        return Ok(false);
    }
    let tmp = dst.with_extension("installing");
    std::fs::copy(src, &tmp).with_context(|| format!("copying {} to {}", src.display(), tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, dst)?;
    Ok(true)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(name))
            .find(|c| c.is_file())
    })
}

/// The account JID of a profile, read from its config (no daemon needed).
pub fn profile_jid(profile: &Profile) -> Option<String> {
    agent_chat_daemon::config::Config::load(&profile.config_path())
        .ok()
        .and_then(|c| c.account.map(|a| a.jid))
}

pub struct InstallOptions {
    pub bin_dir: Option<PathBuf>,
    pub skills_dir: Option<PathBuf>,
    pub skill: bool,
    pub mcp: bool,
}

pub fn install(profile: &Profile, opts: InstallOptions) -> Result<Value> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let bin_dir = match opts.bin_dir {
        Some(d) => d,
        None => default_bin_dir()?,
    };
    std::fs::create_dir_all(&bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;
    let cli = bin_dir.join("agent-chat");
    let cli_copied = install_binary(&exe, &cli)?;

    // The GUI, if it was built next to the CLI.
    let gui_src = exe.with_file_name("agent-chat-gui");
    let gui = if gui_src.is_file() {
        let dst = bin_dir.join("agent-chat-gui");
        install_binary(&gui_src, &dst)?;
        Some(dst)
    } else {
        None
    };

    let setup = AgentSetup {
        profile: profile.name.clone(),
        jid: profile_jid(profile),
        cli: cli.to_string_lossy().into_owned(),
        web_url: None,
    };

    let skill = if opts.skill {
        let dir = match opts.skills_dir {
            Some(d) => d,
            None => default_skills_dir()?,
        }
        .join("agent-chat");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        std::fs::write(&path, setup.skill_md())?;
        Some(path)
    } else {
        None
    };

    let mcp = if opts.mcp {
        match find_on_path("claude") {
            Some(claude) => {
                // Re-registering replaces an older entry.
                let name = if setup.profile == agent_chat_proto::paths::DEFAULT_PROFILE {
                    "agent-chat".to_owned()
                } else {
                    format!("agent-chat-{}", setup.profile)
                };
                let _ = Command::new(&claude).args(["mcp", "remove", "--scope", "user", &name]).output();
                let mut args = vec!["mcp".to_owned(), "add".into(), "--scope".into(), "user".into(), name, "--".into()];
                args.push(setup.cli.clone());
                if setup.profile != agent_chat_proto::paths::DEFAULT_PROFILE {
                    args.push("--profile".into());
                    args.push(setup.profile.clone());
                }
                args.push("mcp".into());
                let out = Command::new(&claude).args(&args).output()?;
                if out.status.success() {
                    json!({"registered": true, "client": "claude-code"})
                } else {
                    json!({"registered": false, "error": String::from_utf8_lossy(&out.stderr).trim(), "command": setup.claude_mcp_add()})
                }
            }
            None => json!({"registered": false, "error": "`claude` not found on PATH", "command": setup.claude_mcp_add()}),
        }
    } else {
        Value::Null
    };

    let path_hint = (!on_path(&bin_dir)).then(|| {
        format!(
            "{} is not on your PATH; add this to your shell profile: export PATH=\"{}:$PATH\"",
            bin_dir.display(),
            bin_dir.display()
        )
    });
    Ok(json!({
        "ok": true,
        "cli": cli,
        "cli_updated": cli_copied,
        "gui": gui,
        "skill": skill,
        "mcp": mcp,
        "path_hint": path_hint,
        "next": format!("Paste the output of `{} setup` into your agent, or open the setup page.", setup.cmd()),
    }))
}

pub fn uninstall(bin_dir: Option<PathBuf>, skills_dir: Option<PathBuf>) -> Result<Value> {
    let bin_dir = match bin_dir {
        Some(d) => d,
        None => default_bin_dir()?,
    };
    let skills_dir = match skills_dir {
        Some(d) => d,
        None => default_skills_dir()?,
    };
    let mut removed = Vec::new();
    for name in ["agent-chat", "agent-chat-gui"] {
        let p = bin_dir.join(name);
        if p.is_file() {
            std::fs::remove_file(&p)?;
            removed.push(p);
        }
    }
    let skill = skills_dir.join("agent-chat");
    if skill.join("SKILL.md").is_file() {
        std::fs::remove_dir_all(&skill)?;
        removed.push(skill);
    }
    Ok(json!({"ok": true, "removed": removed}))
}

/// Copy text to the system clipboard (pbcopy / wl-copy / xclip).
pub fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    let candidates: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (cmd, args) in candidates {
        if find_on_path(cmd).is_some() {
            let mut child = Command::new(cmd).args(*args).stdin(std::process::Stdio::piped()).spawn()?;
            child.stdin.take().expect("piped").write_all(text.as_bytes())?;
            child.wait()?;
            return Ok(());
        }
    }
    Err(anyhow!("no clipboard tool found (pbcopy, wl-copy, xclip or xsel)"))
}

pub fn open_url(url: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    Command::new(opener).arg(url).spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_into_temp_dirs_and_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile {
            name: "bot1".into(),
            dir: dir.path().join("profile"),
        };
        let bin = dir.path().join("bin");
        let skills = dir.path().join("skills");
        let out = install(
            &profile,
            InstallOptions {
                bin_dir: Some(bin.clone()),
                skills_dir: Some(skills.clone()),
                skill: true,
                mcp: false,
            },
        )
        .unwrap();
        assert!(bin.join("agent-chat").is_file());
        let skill = std::fs::read_to_string(skills.join("agent-chat/SKILL.md")).unwrap();
        assert!(skill.starts_with("---\nname: agent-chat"));
        assert!(skill.contains(&format!("{} --profile bot1 wait", bin.join("agent-chat").display())));
        assert!(out["path_hint"].is_string());
        let removed = uninstall(Some(bin.clone()), Some(skills.clone())).unwrap();
        assert_eq!(removed["removed"].as_array().unwrap().len(), 2);
        assert!(!bin.join("agent-chat").exists());
    }
}

// ------------------------------------------------------------------ GUI

/// The desktop app: next to this binary, else on PATH.
pub fn find_gui() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.canonicalize().ok())
        .map(|e| e.with_file_name("agent-chat-gui"))
        .filter(|p| p.is_file());
    beside.or_else(|| find_on_path("agent-chat-gui"))
}

/// The source checkout this binary was built from, if it still exists.
pub fn source_root() -> Option<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?.to_path_buf();
    (root.join("Cargo.toml").is_file() && root.join("crates/gui/Cargo.toml").is_file()).then_some(root)
}

/// Build the desktop app from source (release) and put it next to this
/// binary, unless this binary runs from the checkout's target directory.
pub fn build_and_install_gui() -> Result<PathBuf> {
    let root = source_root().ok_or_else(|| {
        anyhow!(
            "no source checkout found to build the desktop app from; install it with \
             `cargo install --git https://github.com/GravyPouch/agent-chat agent-chat-gui`"
        )
    })?;
    eprintln!("Building the desktop app in {} (this can take a few minutes the first time)…", root.display());
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "agent-chat-gui"])
        .current_dir(&root)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .context("running cargo (is the Rust toolchain installed?)")?;
    if !status.success() {
        return Err(anyhow!("building the desktop app failed (see the output above)"));
    }
    let built = root.join("target/release/agent-chat-gui");
    let exe_dir = std::env::current_exe()?.canonicalize()?.parent().map(Path::to_path_buf);
    match exe_dir {
        // Running from the checkout: use the build output in place.
        Some(dir) if dir.starts_with(root.join("target")) => Ok(built),
        Some(dir) => {
            let dst = dir.join("agent-chat-gui");
            install_binary(&built, &dst)?;
            Ok(dst)
        }
        None => Ok(built),
    }
}

/// Start the desktop app detached from this terminal.
pub fn launch_gui(gui: &Path, profile: &Profile, open: Option<&str>) -> Result<u32> {
    let mut cmd = Command::new(gui);
    cmd.args(["--profile", &profile.name]);
    if let Some(jid) = open {
        cmd.args(["--open", jid]);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn().with_context(|| format!("starting {}", gui.display()))?;
    Ok(child.id())
}
