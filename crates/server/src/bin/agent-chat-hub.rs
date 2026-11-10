//! `agent-chat-hub`: the hub without the client, for Docker and server
//! installs. `agent-chat hub …` offers the same commands with more options.
//!
//! ```text
//! agent-chat-hub [run]                       run in the foreground (default)
//! agent-chat-hub init [DOMAIN]               create hub.toml and the database
//! agent-chat-hub health                      exit 0 if the running hub answers
//! agent-chat-hub status                      JSON status
//! agent-chat-hub user add NAME [--admin]     password from stdin (or generated)
//! agent-chat-hub user list | del NAME | passwd NAME
//! agent-chat-hub invite [USES] [EXPIRES]     e.g. `invite 1 7d`
//! agent-chat-hub import-prosody DATA_PATH [--no-archives]   (hub stopped)
//! ```
//!
//! The hub directory is `$AGENT_CHAT_HOME/hub` or the platform data dir.

use std::io::Read;
use std::process::ExitCode;

use agent_chat_server::admin::{AdminClient, AdminRequest};
use agent_chat_server::{Config, HubPaths, InitOptions};
use anyhow::{Context, Result, bail};

fn stdin_password() -> Result<Option<String>> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    let s = s.trim_end_matches(['\n', '\r']).to_owned();
    Ok((!s.is_empty()).then_some(s))
}

async fn admin(paths: &HubPaths, req: AdminRequest) -> Result<serde_json::Value> {
    let mut c = AdminClient::connect(&paths.socket())
        .await
        .context("the hub is not running (no admin socket)")?;
    c.call_value(req).await.map_err(|e| anyhow::anyhow!("{e}"))
}

async fn run(args: Vec<String>) -> Result<()> {
    let paths = HubPaths::resolve()?;
    let arg = |i: usize| args.get(i).map(String::as_str);
    let print = |v: &serde_json::Value| println!("{v}");
    match arg(0) {
        Some("init") => {
            let report = agent_chat_server::init(
                &paths,
                InitOptions {
                    domain: args.get(1).cloned(),
                    ..Default::default()
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&report)?);
        }
        None | Some("run") => {
            let log = Config::load(paths.config()).map(|c| c.log).unwrap_or_default();
            agent_chat_server::init_logging(&log);
            agent_chat_server::run(paths).await?;
        }
        Some("health") => {
            let v = tokio::time::timeout(std::time::Duration::from_secs(5), admin(&paths, AdminRequest::Status))
                .await
                .context("the hub did not answer within 5 s")??;
            if v["domain"].is_null() {
                bail!("unexpected status answer");
            }
        }
        Some("status") => print(&admin(&paths, AdminRequest::Status).await?),
        Some("user") => {
            let name = || arg(2).map(str::to_owned).context("give a user name");
            let req = match arg(1) {
                Some("add") => AdminRequest::UserAdd {
                    username: name()?,
                    password: stdin_password()?,
                    admin: args.iter().any(|a| a == "--admin"),
                },
                Some("list") => AdminRequest::UserList,
                Some("del") => AdminRequest::UserDel { username: name()? },
                Some("passwd") => AdminRequest::UserPasswd {
                    username: name()?,
                    password: stdin_password()?,
                },
                _ => bail!("use `user add|list|del|passwd`"),
            };
            print(&admin(&paths, req).await?);
        }
        Some("invite") => {
            let uses = arg(1)
                .map(str::parse)
                .transpose()
                .context("USES is a number")?
                .unwrap_or(1);
            let expires = agent_chat_server::config::parse_duration(arg(2).unwrap_or("7d"))?;
            let v = admin(
                &paths,
                AdminRequest::InviteCreate {
                    uses,
                    expires_secs: expires.as_secs(),
                    note: None,
                },
            )
            .await?;
            print(&v);
        }
        Some("import-prosody") => {
            let data_path = arg(1).context("give Prosody's data_path")?.into();
            let imported = agent_chat_server::import::import_prosody(
                &paths,
                agent_chat_server::import::ImportOptions {
                    data_path,
                    host: None,
                    archives: !args.iter().any(|a| a == "--no-archives"),
                },
            )
            .await?;
            println!("{}", serde_json::to_string(&imported)?);
        }
        Some(other) => bail!("unknown command {other:?}; see the top of crates/server/src/bin/agent-chat-hub.rs"),
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
