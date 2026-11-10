//! `agent-chat-hub`: the hub without the client, for Docker and server
//! installs. `agent-chat hub …` offers the same commands with more options.
//!
//! Usage: `agent-chat-hub [init [DOMAIN] | run]` (default: `run`).
//! The hub directory is `$AGENT_CHAT_HOME/hub` or the platform data dir.

use std::process::ExitCode;

use agent_chat_server::{Config, HubPaths, InitOptions};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let result = runtime.block_on(async {
        let paths = HubPaths::resolve()?;
        match args.first().map(String::as_str) {
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
                Ok(())
            }
            None | Some("run") => {
                let log = Config::load(paths.config()).map(|c| c.log).unwrap_or_default();
                agent_chat_server::init_logging(&log);
                agent_chat_server::run(paths).await
            }
            Some(other) => anyhow::bail!("unknown command {other:?}; use `init [DOMAIN]` or `run`"),
        }
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
