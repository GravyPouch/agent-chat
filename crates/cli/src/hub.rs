//! `agent-chat hub …`: run and manage the embedded XMPP server
//! (docs/design/embedded-server.md §5.2). Every command prints one JSON line.

use std::io::IsTerminal;
use std::process::Stdio;
use std::time::Duration;

use agent_chat_server::admin::{
    AdminCallError, AdminClient, AdminErrorCode, AdminRequest, HubStatus, UserCreated, UserInfo,
};
use agent_chat_server::config::TlsMode;
use agent_chat_server::{Config, HubPaths, InitOptions, RuntimeInfo};
use anyhow::{Context, Result, anyhow};
use clap::{Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::{Exit, Format, print_json};

#[derive(Subcommand)]
pub enum HubCmd {
    /// Create the hub: configuration, TLS certificate and database.
    Init {
        /// The hub's XMPP domain. Permanent: accounts can never move.
        /// Default: localhost, or <this machine's hostname>.local with --lan.
        #[arg(long)]
        domain: Option<String>,
        /// Serve the local network too (default address 0.0.0.0:5222).
        #[arg(long, value_name = "ADDR:PORT", num_args = 0..=1, default_missing_value = "0.0.0.0:5222")]
        lan: Option<String>,
        /// Listen on exactly these addresses (repeatable; port 0 picks one).
        #[arg(long, value_name = "ADDR:PORT")]
        listen: Vec<String>,
        /// Certificate source.
        #[arg(long, value_enum, default_value_t = TlsArg::SelfSigned)]
        tls: TlsArg,
    },
    /// Run the hub in the foreground (logs to stderr).
    Run,
    /// Start the hub in the background (initializing it with defaults the first time).
    Start,
    /// Stop the hub.
    Stop,
    /// Show sessions, accounts, listeners and counters.
    Status,
    /// Manage accounts.
    #[command(subcommand)]
    User(UserCmd),
}

#[derive(Copy, Clone, ValueEnum)]
pub enum TlsArg {
    /// Generated certificate; our client pins it (hub.json).
    SelfSigned,
    /// A local CA for stock clients (not implemented yet).
    LocalCa,
    /// PEM files you put at tls/cert.pem and tls/key.pem.
    Files,
}

#[derive(Subcommand)]
pub enum UserCmd {
    /// Create an account. Without --password-stdin a password is generated
    /// and printed once.
    Add {
        name: String,
        #[arg(long)]
        password_stdin: bool,
        #[arg(long)]
        admin: bool,
    },
    /// Delete an account (its sessions are closed).
    Del { name: String },
    /// Set a new password (generated unless --password-stdin).
    Passwd {
        name: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// List accounts.
    List,
}

pub async fn run(cmd: HubCmd, fmt: Format) -> Result<()> {
    let paths = HubPaths::resolve()?;
    match cmd {
        HubCmd::Init {
            domain,
            lan,
            listen,
            tls,
        } => {
            let report = agent_chat_server::init(
                &paths,
                InitOptions {
                    domain,
                    lan,
                    listen,
                    tls_mode: Some(match tls {
                        TlsArg::SelfSigned => TlsMode::SelfSigned,
                        TlsArg::LocalCa => TlsMode::LocalCa,
                        TlsArg::Files => TlsMode::Files,
                    }),
                },
            )
            .await?;
            print_json(&report);
        }
        HubCmd::Run => {
            let log = Config::load(paths.config()).map(|c| c.log).unwrap_or_default();
            agent_chat_server::init_logging(&log);
            agent_chat_server::run(paths).await?;
        }
        HubCmd::Start => {
            let (status, started, initialized) = ensure_running(&paths).await?;
            let mut v = serde_json::to_value(&status)?;
            v["started"] = json!(started);
            v["initialized"] = json!(initialized);
            match fmt {
                Format::Json => print_json(&v),
                Format::Text => print_status_text(&status),
            }
        }
        HubCmd::Stop => {
            let mut client = connect(&paths).await?;
            client.call_value(AdminRequest::Shutdown).await.map_err(admin_err)?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while AdminClient::connect(&paths.socket()).await.is_ok() || paths.runtime().exists() {
                if tokio::time::Instant::now() >= deadline {
                    return Err(anyhow!("the hub did not stop within 15 s"));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            print_json(&json!({"ok": true, "stopped": true}));
        }
        HubCmd::Status => {
            let status: HubStatus = connect(&paths)
                .await?
                .call(AdminRequest::Status)
                .await
                .map_err(admin_err)?;
            match fmt {
                Format::Json => print_json(&status),
                Format::Text => print_status_text(&status),
            }
        }
        HubCmd::User(cmd) => user(&paths, cmd, fmt).await?,
    }
    Ok(())
}

async fn user(paths: &HubPaths, cmd: UserCmd, fmt: Format) -> Result<()> {
    let mut client = connect(paths).await?;
    match cmd {
        UserCmd::Add {
            name,
            password_stdin,
            admin,
        } => {
            let password = password_stdin.then(read_password_line).transpose()?;
            let created: UserCreated = client
                .call(AdminRequest::UserAdd {
                    username: name,
                    password,
                    admin,
                })
                .await
                .map_err(admin_err)?;
            let connect = connect_info(paths);
            match fmt {
                Format::Json => {
                    let mut v = serde_json::to_value(&created)?;
                    v["connect"] = connect;
                    print_json(&v);
                }
                Format::Text => print_account_text(&created.jid, created.password.as_deref(), &connect),
            }
        }
        UserCmd::Del { name } => {
            client
                .call_value(AdminRequest::UserDel { username: name })
                .await
                .map_err(admin_err)?;
            print_json(&json!({"ok": true}));
        }
        UserCmd::Passwd { name, password_stdin } => {
            let password = password_stdin.then(read_password_line).transpose()?;
            let v = client
                .call_value(AdminRequest::UserPasswd {
                    username: name,
                    password,
                })
                .await
                .map_err(admin_err)?;
            print_json(&json!({"ok": true, "password": v.get("password").cloned().unwrap_or(Value::Null)}));
        }
        UserCmd::List => {
            let users: Vec<UserInfo> = client.call(AdminRequest::UserList).await.map_err(admin_err)?;
            match fmt {
                Format::Json => print_json(&users),
                Format::Text => {
                    for u in users {
                        println!("{:<40} {:<9} {}", u.jid, u.role.as_str(), u.state.as_str());
                    }
                }
            }
        }
    }
    Ok(())
}

/// How clients on this machine reach the hub, from `hub.json`.
fn connect_info(paths: &HubPaths) -> Value {
    match RuntimeInfo::read(paths).ok().flatten() {
        Some(info) => {
            let addr = info.loopback_c2s().or_else(|| info.c2s.first().copied());
            json!({
                "domain": info.domain,
                "host": addr.map(|a| a.ip().to_string()),
                "port": addr.map(|a| a.port()),
                "tls": "starttls",
                "certificate": info.tls_mode.as_str(),
                "pin": info.pin,
            })
        }
        None => Value::Null,
    }
}

fn print_account_text(jid: &str, password: Option<&str>, connect: &Value) {
    println!("account:  {jid}");
    if let Some(p) = password {
        println!("password: {p}   (shown once)");
    }
    if let (Some(host), Some(port)) = (connect["host"].as_str(), connect["port"].as_u64()) {
        println!("server:   {host}, port {port} (STARTTLS)");
    }
    if let Some(pin) = connect["pin"].as_str() {
        println!("certificate: self-signed; its key pin is {pin}");
    }
}

fn print_status_text(s: &HubStatus) {
    println!("domain:    {}", s.domain);
    println!("listening: {}", s.c2s.join(", "));
    println!("sessions:  {}", s.sessions);
    println!("accounts:  {}", s.accounts);
    println!("uptime:    {} s", s.uptime_secs);
    println!("version:   {} (pid {})", s.version, s.pid);
}

fn admin_err(e: AdminCallError) -> anyhow::Error {
    match e {
        AdminCallError::Remote(r) if r.code == AdminErrorCode::Exists => Exit(6, r.message).into(),
        AdminCallError::Remote(r) => anyhow!(r.message),
        other => anyhow!(other),
    }
}

async fn connect(paths: &HubPaths) -> Result<AdminClient> {
    AdminClient::connect(&paths.socket())
        .await
        .map_err(|_| Exit(3, "the hub is not running; start it with `agent-chat hub start`".into()).into())
}

fn read_password_line() -> Result<String> {
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password("New password: ").context("reading password");
    }
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim_end_matches(['\r', '\n']).to_owned())
}

/// Start the hub if needed (running `init` with defaults the first time).
/// Returns its status, whether we started it, and whether we initialized it.
pub async fn ensure_running(paths: &HubPaths) -> Result<(HubStatus, bool, bool)> {
    if let Ok(mut c) = AdminClient::connect(&paths.socket()).await {
        let status = c.call(AdminRequest::Status).await.map_err(admin_err)?;
        return Ok((status, false, false));
    }
    let initialized = if paths.config().exists() {
        false
    } else {
        agent_chat_server::init(paths, InitOptions::default()).await?;
        true
    };
    spawn_detached(paths)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(mut c) = AdminClient::connect(&paths.socket()).await {
            let status = c.call(AdminRequest::Status).await.map_err(admin_err)?;
            return Ok((status, true, initialized));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("the hub did not start; see {}", paths.log().display()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `agent-chat hub run`, detached, logging to `hub.log`.
fn spawn_detached(paths: &HubPaths) -> Result<()> {
    paths.ensure_dir()?;
    let log_path = log_file(paths);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(["hub", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: outlives the shell or agent that started it.
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().context("starting the hub")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn log_file(paths: &HubPaths) -> std::path::PathBuf {
    Config::load(paths.config())
        .map(|c| paths.resolve_file(&c.log.file))
        .unwrap_or_else(|_| paths.log())
}

/// For `account add --hub NAME`: create NAME on the local hub (starting
/// it if needed) and return (jid, password, host, port).
pub async fn create_hub_account(name: &str, password: Option<String>) -> Result<(String, String, String, u16)> {
    let paths = HubPaths::resolve()?;
    ensure_running(&paths).await?;
    let info = RuntimeInfo::read(&paths)?.context("the hub is running but hub.json is missing")?;
    let addr = info
        .loopback_c2s()
        .context("the hub has no loopback listener; use `account add JID --host …` instead")?;
    let username = match name.split_once('@') {
        Some((user, domain)) if domain.eq_ignore_ascii_case(&info.domain) => user.to_owned(),
        Some((_, domain)) => {
            return Err(anyhow!("this hub serves {}, not {domain}", info.domain));
        }
        None => name.to_owned(),
    };
    let mut client = connect(&paths).await?;
    let created: UserCreated = client
        .call(AdminRequest::UserAdd {
            username,
            password: password.clone(),
            admin: false,
        })
        .await
        .map_err(admin_err)?;
    let password = created
        .password
        .or(password)
        .ok_or_else(|| anyhow!("the hub returned no password"))?;
    Ok((created.jid, password, addr.ip().to_string(), addr.port()))
}
