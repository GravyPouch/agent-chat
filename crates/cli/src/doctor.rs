//! `agent-chat doctor`: check that the agents on this machine each have
//! their own profile, and that the hook and MCP server are set up the
//! current way (see docs/design/agent-isolation.md). Read-only: it never
//! starts a daemon or changes a file.

use std::path::Path;

use agent_chat_proto::ipc::Request;
use agent_chat_proto::model::AccountKind;
use agent_chat_proto::paths::{DEFAULT_PROFILE, PROFILE_ENV};
use agent_chat_proto::{IpcClient, Profile};
use anyhow::Result;
use serde_json::{Value, json};

#[derive(Default)]
struct Report {
    checks: Vec<Value>,
}

impl Report {
    fn ok(&mut self, check: &str, message: String) {
        self.checks
            .push(json!({"check": check, "level": "ok", "message": message}));
    }

    fn info(&mut self, check: &str, message: String) {
        self.checks
            .push(json!({"check": check, "level": "info", "message": message}));
    }

    fn warn(&mut self, check: &str, message: String, fix: String) {
        self.checks
            .push(json!({"check": check, "level": "warn", "message": message, "fix": fix}));
    }
}

/// Run every check. `home`: the user's home directory; `dir`: the project.
pub async fn run(home: &Path, dir: &Path) -> Result<Value> {
    let mut r = Report::default();
    hooks(&mut r, home);
    mcp(&mut r, home);
    let profiles = profiles(&mut r).await?;
    session(&mut r, dir, &profiles);
    let problems = r.checks.iter().filter(|c| c["level"] == "warn").count();
    Ok(json!({
        "ok": problems == 0,
        "problems": problems,
        "checks": r.checks,
        "profiles": profiles,
    }))
}

fn hooks(r: &mut Report, home: &Path) {
    let installed = crate::hooks::installed(home);
    if installed.is_empty() {
        r.info(
            "hook",
            "no Claude Code, Codex or Cursor configuration in your home directory".into(),
        );
    }
    for (agent, path, commands) in installed {
        let path = path.display();
        match commands {
            Err(e) => r.warn(
                "hook",
                format!("{agent}: can't read {path}: {e:#}"),
                format!("fix {path}"),
            ),
            Ok(c) if c.is_empty() => r.info(
                "hook",
                format!("{agent}: no stop hook, so new messages don't reach it at the end of a turn (agent-chat install adds it)"),
            ),
            Ok(c) if c.iter().any(|c| c.contains(" --profile ")) => r.warn(
                "hook",
                format!("{agent}: an older stop hook names a profile, so every session reads that profile's messages"),
                "agent-chat install".into(),
            ),
            Ok(c) if c.len() > 1 => r.warn(
                "hook",
                format!("{agent}: {} agent-chat stop hooks in {path}", c.len()),
                "agent-chat install".into(),
            ),
            Ok(_) => r.ok("hook", format!("{agent}: stop hook installed")),
        }
    }
}

/// Claude Code's user-scope MCP servers (`~/.claude.json`).
fn mcp(r: &mut Report, home: &Path) {
    let Ok(text) = std::fs::read_to_string(home.join(".claude.json")) else {
        return;
    };
    let Ok(doc) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(servers) = doc["mcpServers"].as_object() else {
        return;
    };
    let ours: Vec<(&String, &Value)> = servers
        .iter()
        .filter(|(_, s)| s["command"].as_str().is_some_and(|c| c.contains("agent-chat")))
        .collect();
    if ours.is_empty() {
        r.info("mcp", "the MCP server isn't registered with Claude Code".into());
    }
    for (name, server) in ours {
        let bound = server["args"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == "--profile"));
        if bound {
            r.warn(
                "mcp",
                format!("Claude Code's MCP server {name:?} names a profile, so every session uses that account"),
                format!("claude mcp remove --scope user {name} && agent-chat install --mcp"),
            );
        } else {
            r.ok(
                "mcp",
                format!("Claude Code's MCP server {name:?} follows {PROFILE_ENV}"),
            );
        }
    }
}

async fn profiles(r: &mut Report) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for profile in Profile::list()? {
        let config = agent_chat_daemon::config::Config::load(&profile.config_path()).ok();
        let jid = config.as_ref().and_then(|c| c.account.as_ref()).map(|a| a.jid.clone());
        let kind = config.as_ref().and_then(|c| c.settings.account_kind);
        let (running, claim) = match IpcClient::connect(&profile.socket_path()).await {
            Ok(client) => (
                true,
                client.call_value(Request::ClaimStatus).await.unwrap_or(Value::Null),
            ),
            Err(_) => (false, Value::Null),
        };
        if profile.name == DEFAULT_PROFILE && kind == Some(AccountKind::Agent) {
            r.warn(
                "profiles",
                "the default profile is marked as an agent's; if it's your own account, agents' hooks and \
                 reads can take your messages"
                    .into(),
                "agent-chat account kind person (if it's yours), and give each agent its own profile: \
                 agent-chat agent init NAME"
                    .into(),
            );
        }
        if let Some(session) = claim["session"].as_str() {
            r.info(
                "profiles",
                format!(
                    "{} is claimed by session {session} ({})",
                    profile.name,
                    if claim["reading"] == true {
                        "reading now".to_owned()
                    } else {
                        format!("last read {} s ago", claim["idle_secs"])
                    }
                ),
            );
        }
        out.push(json!({
            "name": profile.name,
            "jid": jid,
            "kind": kind.map(|k| k.as_str()),
            "daemon": running,
            "claim": claim,
        }));
    }
    if out.is_empty() {
        r.info("profiles", "no profiles yet".into());
    }
    Ok(out)
}

/// The profile this session and project use.
fn session(r: &mut Report, dir: &Path, profiles: &[Value]) {
    let account_of = |name: &str| {
        profiles
            .iter()
            .find(|p| p["name"] == name)
            .and_then(|p| p["jid"].as_str())
            .map(str::to_owned)
    };
    let bound = crate::hooks::bound_profile(dir);
    let env = std::env::var(PROFILE_ENV).ok().filter(|p| !p.is_empty());
    match &bound {
        Some(p) => match account_of(p) {
            Some(jid) => r.ok("project", format!("{} is bound to profile {p} ({jid})", dir.display())),
            None => r.warn(
                "project",
                format!("{} is bound to profile {p}, which has no account", dir.display()),
                format!("agent-chat agent init {p} --server DOMAIN (or --hub)"),
            ),
        },
        None => r.info("project", format!("{} isn't bound to an agent profile", dir.display())),
    }
    match (&env, &bound) {
        (Some(e), Some(b)) if e != b => r.warn(
            "session",
            format!("this session uses {PROFILE_ENV}={e}, but the project is bound to {b}"),
            "restart the agent session to pick up the project's binding".into(),
        ),
        (Some(e), _) if account_of(e).is_none() => r.warn(
            "session",
            format!("this session uses {PROFILE_ENV}={e}, which has no account"),
            format!("agent-chat agent init {e} --server DOMAIN (or --hub)"),
        ),
        (Some(e), _) => r.ok("session", format!("this session uses profile {e}")),
        (None, Some(b)) => r.warn(
            "session",
            format!("the project is bound to {b}, but this session doesn't have {PROFILE_ENV} set"),
            "restart the agent session (Claude Code reads the binding when it starts)".into(),
        ),
        (None, None) => r.info("session", format!("this session has no {PROFILE_ENV}")),
    }
    if let (Some(me), Some(p)) = (
        agent_chat_proto::paths::session_from_env(),
        env.as_ref().or(bound.as_ref()),
    ) {
        let holder = profiles
            .iter()
            .find(|x| x["name"] == p.as_str())
            .and_then(|x| x["claim"]["session"].as_str());
        if let Some(other) = holder.filter(|h| *h != me) {
            r.warn(
                "session",
                format!("profile {p} is claimed by another session ({other}); this session's reads are refused"),
                format!("use your own profile (agent-chat agent init NAME), or if that session ended: agent-chat --profile {p} account release"),
            );
        }
    }
}

/// Text output: one line per check.
pub fn print_text(report: &Value) {
    for c in report["checks"].as_array().into_iter().flatten() {
        let mark = match c["level"].as_str() {
            Some("ok") => "ok  ",
            Some("warn") => "WARN",
            _ => "info",
        };
        println!(
            "{mark} {}: {}",
            c["check"].as_str().unwrap_or_default(),
            c["message"].as_str().unwrap_or_default()
        );
        if let Some(fix) = c["fix"].as_str() {
            println!("     fix: {fix}");
        }
    }
    match report["problems"].as_u64() {
        Some(0) => println!("No problems found."),
        Some(n) => println!("{n} problem{} found.", if n == 1 { "" } else { "s" }),
        None => {}
    }
}
