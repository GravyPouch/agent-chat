//! `agent-chat`: command-line XMPP client for humans and AI agents.
//!
//! Agent-facing commands print JSON to stdout (one compact document per
//! line). Exit codes: 0 ok, 1 error, 2 timeout, 3 not connected / no
//! account, 4 encryption unavailable.

mod install;
mod mcp;

use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use agent_chat_proto::ipc::{AccountParams, ContactList, EncryptionMode, ErrorCode, PasswordStorage, Request};
use agent_chat_proto::model::{
    ConversationSummary, Delivery, DeviceInfo, Filter, Message, MessageKind, Room, RoomInvite, Show, StatusInfo,
};
use agent_chat_proto::{CallError, DaemonLauncher, IpcClient, Profile};
use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

#[cfg(feature = "hub")]
mod hub;

#[derive(Parser)]
#[command(name = "agent-chat", version, about = "XMPP chat for people and AI agents")]
struct Cli {
    /// Profile (one account per profile).
    #[arg(long, global = true, env = "AGENT_CHAT_PROFILE")]
    profile: Option<String>,
    /// Output format.
    #[arg(long, global = true, value_enum, default_value_t = Format::Json)]
    format: Format,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
    Json,
    Text,
}

#[derive(Subcommand)]
enum Cmd {
    /// Block until the next unread message arrives and print it.
    Wait {
        #[command(flatten)]
        read: ReadArgs,
        /// Give up after this many seconds (exit code 2).
        #[arg(long)]
        timeout: Option<u64>,
    },
    /// Print unread messages and mark them read for this consumer.
    Inbox {
        #[command(flatten)]
        read: ReadArgs,
        /// Don't mark the messages as read.
        #[arg(long)]
        peek: bool,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Stream unread and new messages forever, one JSON object per line.
    Listen {
        #[command(flatten)]
        read: ReadArgs,
    },
    /// Send a message. TEXT defaults to stdin.
    Send {
        /// Contact JID, room JID, or room/nick for a private message.
        to: String,
        /// Message text; omit or use `-` to read stdin.
        text: Vec<String>,
        /// Send to a room (default: auto-detect joined rooms).
        #[arg(long, conflicts_with = "chat")]
        room: bool,
        /// Send as a 1:1 chat even if TO is a room.
        #[arg(long)]
        chat: bool,
        /// Never encrypt.
        #[arg(long, conflicts_with = "require_encryption")]
        plaintext: bool,
        /// Fail instead of sending plaintext when OMEMO isn't available.
        #[arg(long)]
        require_encryption: bool,
    },
    /// Show stored history of a conversation (oldest first).
    History {
        conversation: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Only messages older than this local id.
        #[arg(long)]
        before: Option<i64>,
    },
    /// List conversations with their latest message.
    Conversations,
    /// Show connection status.
    Status,
    /// Configure the XMPP account.
    #[command(subcommand)]
    Account(AccountCmd),
    /// Manage contacts (roster).
    Contacts {
        #[command(subcommand)]
        cmd: Option<ContactsCmd>,
    },
    /// Manage group chats.
    Rooms {
        #[command(subcommand)]
        cmd: Option<RoomsCmd>,
    },
    /// Set presence: available, chat, away, xa, dnd or unavailable.
    Presence {
        show: String,
        /// Optional status text.
        status: Vec<String>,
    },
    /// OMEMO end-to-end encryption devices and trust.
    #[command(subcommand)]
    Omemo(OmemoCmd),
    /// Control the background daemon.
    #[command(subcommand)]
    Daemon(DaemonCmd),
    /// Run an MCP server on stdio exposing agent-chat as tools (for Claude
    /// Code, Claude Desktop, Cursor and other MCP clients).
    Mcp {
        /// Consumer name for wait/unread (separate from CLI agents).
        #[arg(long, env = "AGENT_CHAT_MCP_CONSUMER", default_value = "mcp")]
        consumer: String,
    },
    /// Instructions for connecting an agent: prints the prompt to paste into
    /// an agent (or the skill / MCP config), and can copy it or open the page.
    Setup {
        /// Copy the agent prompt to the clipboard.
        #[arg(long)]
        copy: bool,
        /// Open the local setup page in the browser.
        #[arg(long)]
        open: bool,
        /// Print the agent skill (SKILL.md) instead.
        #[arg(long, conflicts_with = "mcp_json")]
        skill: bool,
        /// Print the MCP client config (JSON) instead.
        #[arg(long)]
        mcp_json: bool,
    },
    /// QR codes and xmpp: links to use this account from a phone or another
    /// XMPP app (Monal, Siskin, Conversations, Dino, Gajim…).
    Phone {
        /// Only the sign-up code for this server.
        #[arg(long, conflicts_with = "contact")]
        register: bool,
        /// Only the "chat with this account" code.
        #[arg(long)]
        contact: bool,
    },
    /// Open the desktop app (building and installing it first with --install).
    Gui {
        /// Start with this conversation open.
        #[arg(long)]
        open: Option<String>,
        /// Build the app from source and install it next to this binary.
        #[arg(long)]
        install: bool,
        /// With --install: don't open it afterwards.
        #[arg(long, requires = "install")]
        no_launch: bool,
    },
    /// Install the CLI (and GUI) on PATH and the agent skill for Claude Code.
    Install {
        /// Where to put the binaries (default: ~/.local/bin).
        #[arg(long)]
        bin_dir: Option<std::path::PathBuf>,
        /// Skills directory (default: ~/.claude/skills).
        #[arg(long)]
        skills_dir: Option<std::path::PathBuf>,
        /// Don't install the agent skill.
        #[arg(long)]
        no_skill: bool,
        /// Also register the MCP server with Claude Code (`claude mcp add`).
        #[arg(long)]
        mcp: bool,
    },
    /// Remove what `install` put on this machine.
    Uninstall {
        #[arg(long)]
        bin_dir: Option<std::path::PathBuf>,
        #[arg(long)]
        skills_dir: Option<std::path::PathBuf>,
    },
    /// Run your own XMPP server: the agent-chat hub.
    #[cfg(feature = "hub")]
    #[command(subcommand)]
    Hub(hub::HubCmd),
}

#[derive(Args)]
struct ReadArgs {
    /// Consumer name; each consumer receives every message once.
    #[arg(long, env = "AGENT_CHAT_CONSUMER", default_value = "default")]
    consumer: String,
    /// Only messages from this contact (1:1 chats).
    #[arg(long)]
    from: Option<String>,
    /// Only messages in this room.
    #[arg(long)]
    room: Option<String>,
    /// Also include messages this account sent.
    #[arg(long)]
    include_own: bool,
    /// For a new consumer, start at the beginning of history instead of now.
    #[arg(long)]
    from_start: bool,
    /// Skip messages from untrusted OMEMO devices.
    #[arg(long)]
    require_trusted: bool,
}

impl ReadArgs {
    fn delivery(&self) -> Delivery {
        Delivery {
            consumer: self.consumer.clone(),
            include_own: self.include_own,
            from_start: self.from_start,
        }
    }

    fn filter(&self) -> Filter {
        Filter {
            from: self.from.clone(),
            room: self.room.clone(),
        }
    }
}

#[derive(Subcommand)]
enum AccountCmd {
    /// Set the account for this profile and connect.
    Add {
        /// The account's JID (with --hub, just a name is enough).
        jid: String,
        /// Read the password from stdin instead of prompting.
        #[arg(long)]
        password_stdin: bool,
        /// Create the account on the local agent-chat hub (starting the hub
        /// if needed) with a generated password, unless --password-stdin.
        #[arg(long, conflicts_with_all = ["host", "port", "insecure_plaintext"])]
        hub: bool,
        /// Server host (skips DNS SRV lookup).
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Unencrypted connection; only allowed to localhost (development).
        #[arg(long)]
        insecure_plaintext: bool,
        /// Where to keep the password.
        #[arg(long, value_enum, default_value_t = Storage::Keychain)]
        store: Storage,
    },
    /// Show the configured account and connection state.
    Show,
    /// Create a new account on a server (solving its proof-of-work challenge
    /// if it has one) and use it for this profile.
    ///
    /// Exit codes: 0 registered, 1 error, 2 timeout, 5 refused by the server,
    /// 6 username taken.
    Register {
        /// Server domain, e.g. example.org.
        #[arg(long)]
        server: String,
        /// Username (default: generated, like agent-k2m9qx4td7).
        #[arg(long)]
        username: Option<String>,
        /// Read the password from stdin (default: generate a strong one).
        #[arg(long)]
        password_stdin: bool,
        /// Server host (skips DNS SRV lookup).
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Unencrypted connection; only allowed to localhost (development).
        #[arg(long)]
        insecure_plaintext: bool,
        /// Solver threads (default: all cores).
        #[arg(long)]
        threads: Option<usize>,
        /// Give up after this many seconds (whole operation).
        #[arg(long, default_value_t = 900)]
        timeout: u64,
        /// Where to keep the password.
        #[arg(long, value_enum, default_value_t = Storage::Keychain)]
        store: Storage,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum Storage {
    Keychain,
    Config,
}

#[derive(Subcommand)]
enum ContactsCmd {
    List,
    Add {
        jid: String,
        #[arg(long)]
        name: Option<String>,
    },
    Remove {
        jid: String,
    },
    /// Approve a contact's request to see your presence.
    Approve {
        jid: String,
    },
    Deny {
        jid: String,
    },
}

#[derive(Subcommand)]
enum RoomsCmd {
    List,
    Join {
        jid: String,
        #[arg(long)]
        nick: Option<String>,
        #[arg(long)]
        password: Option<String>,
        /// If the room is new, make it private (members-only, real JIDs
        /// visible) so messages are end-to-end encrypted.
        #[arg(long)]
        private: bool,
    },
    Leave {
        jid: String,
    },
    /// Invite someone to a room (makes them a member when you're an owner/admin).
    Invite {
        room: String,
        jid: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// List pending invitations.
    Invites,
    /// Ignore a pending invitation.
    Decline {
        room: String,
    },
}

#[derive(Subcommand)]
enum OmemoCmd {
    /// List OMEMO devices and fingerprints (yours by default).
    Devices { jid: Option<String> },
    /// Mark a device as verified after comparing fingerprints out of band.
    Trust { jid: String, fingerprint: String },
    /// Mark a device as untrusted.
    Distrust { jid: String, fingerprint: String },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground.
    Run,
    /// Start the daemon in the background if it isn't running.
    Start,
    /// Stop the daemon.
    Stop,
    /// Show whether the daemon is running.
    Status,
}

#[derive(Debug)]
struct Exit(u8, String);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Exit {}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let Some(exit) = e.downcast_ref::<Exit>() {
                if !exit.1.is_empty() {
                    eprintln!("{}", exit.1);
                }
                return ExitCode::from(exit.0);
            }
            let code = match e.downcast_ref::<CallError>() {
                Some(CallError::Remote(r)) => match r.code {
                    ErrorCode::NotConnected | ErrorCode::NoAccount => 3,
                    ErrorCode::EncryptionUnavailable => 4,
                    _ => 1,
                },
                Some(CallError::Disconnected) => 3,
                _ => 1,
            };
            eprintln!("error: {e:#}");
            ExitCode::from(code)
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    let fmt = cli.format;
    // Hub commands don't use a profile or the daemon.
    #[cfg(feature = "hub")]
    if let Cmd::Hub(_) = &cli.cmd {
        let Cmd::Hub(cmd) = cli.cmd else { unreachable!() };
        return hub::run(cmd, fmt).await;
    }
    let profile = Profile::resolve(cli.profile.as_deref())?;

    // Daemon commands that must not auto-start the daemon.
    if let Cmd::Daemon(cmd) = &cli.cmd {
        match cmd {
            DaemonCmd::Run => {
                tracing_subscriber::fmt()
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| "info,tokio_xmpp=warn".into()),
                    )
                    .with_writer(std::io::stderr)
                    .init();
                return agent_chat_daemon::run(profile).await;
            }
            DaemonCmd::Stop => {
                let client = IpcClient::connect(&profile.socket_path())
                    .await
                    .map_err(|_| Exit(3, "daemon is not running".into()))?;
                client.call_value(Request::Shutdown).await?;
                // Wait until it has really exited (socket gone), so a command
                // run right after doesn't race the old daemon's lock.
                let socket = profile.socket_path();
                for _ in 0..100 {
                    if !socket.exists() || IpcClient::connect(&socket).await.is_err() {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        if !socket.exists() {
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                return Ok(());
            }
            DaemonCmd::Status => {
                let client = IpcClient::connect(&profile.socket_path())
                    .await
                    .map_err(|_| Exit(3, "daemon is not running".into()))?;
                let status: StatusInfo = client.call(Request::Status).await?;
                print_status(fmt, &status);
                return Ok(());
            }
            DaemonCmd::Start => {}
        }
    }

    // Local installation needs no daemon.
    match &cli.cmd {
        Cmd::Install {
            bin_dir,
            skills_dir,
            no_skill,
            mcp,
        } => {
            let out = install::install(
                &profile,
                install::InstallOptions {
                    bin_dir: bin_dir.clone(),
                    skills_dir: skills_dir.clone(),
                    skill: !no_skill,
                    mcp: *mcp,
                },
            )?;
            match fmt {
                Format::Json => print_json(&out),
                Format::Text => {
                    println!("Installed {}", out["cli"].as_str().unwrap_or_default());
                    if let Some(s) = out["skill"].as_str() {
                        println!("Agent skill: {s}");
                    }
                    if let Some(h) = out["path_hint"].as_str() {
                        println!("Note: {h}");
                    }
                    println!("{}", out["next"].as_str().unwrap_or_default());
                }
            }
            return Ok(());
        }
        Cmd::Gui {
            open,
            install: build,
            no_launch,
        } => {
            let mut built = false;
            let gui = match (install::find_gui(), *build) {
                (Some(g), false) => g,
                (_, true) => {
                    built = true;
                    install::build_and_install_gui()?
                }
                (None, false) => {
                    return Err(Exit(
                        1,
                        "The desktop app isn't installed. Run `agent-chat gui --install` to build and install it \
                         (needs the GPUI build dependencies; see the README)."
                            .into(),
                    )
                    .into());
                }
            };
            let pid = if *no_launch {
                None
            } else {
                Some(install::launch_gui(&gui, &profile, open.as_deref())?)
            };
            print_json(&serde_json::json!({"ok": true, "gui": gui, "built": built, "pid": pid}));
            return Ok(());
        }
        Cmd::Uninstall { bin_dir, skills_dir } => {
            print_json(&install::uninstall(bin_dir.clone(), skills_dir.clone())?);
            return Ok(());
        }
        _ => {}
    }

    // Registration runs before there is an account (no daemon needed yet).
    if let Cmd::Account(AccountCmd::Register { .. }) = &cli.cmd {
        let Cmd::Account(AccountCmd::Register {
            server,
            username,
            password_stdin,
            host,
            port,
            insecure_plaintext,
            threads,
            timeout,
            store,
        }) = cli.cmd
        else {
            unreachable!()
        };
        return register(
            &profile,
            agent_chat_daemon::register::RegisterOptions {
                domain: server,
                username,
                password: if password_stdin {
                    read_password("new account", true)?
                } else {
                    agent_chat_daemon::register::generate_password()
                },
                host: host.clone(),
                port,
                insecure_plaintext,
                threads: threads.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)),
                timeout: std::time::Duration::from_secs(timeout),
            },
            store,
        )
        .await;
    }

    let launcher = DaemonLauncher::new(std::env::current_exe()?);
    let client = IpcClient::connect_or_spawn(&profile, &launcher).await?;

    match cli.cmd {
        Cmd::Daemon(_) | Cmd::Status => {
            let status: StatusInfo = client.call(Request::Status).await?;
            print_status(fmt, &status);
        }
        Cmd::Wait { read, timeout } => {
            let m: Option<Message> = client
                .call(Request::Wait {
                    delivery: read.delivery(),
                    filter: read.filter(),
                    timeout_secs: timeout,
                    require_trusted: read.require_trusted,
                })
                .await?;
            match m {
                Some(m) => print_message(fmt, &m),
                None => return Err(Exit(2, "timeout: no new messages".into()).into()),
            }
        }
        Cmd::Inbox { read, peek, limit } => {
            let msgs: Vec<Message> = client
                .call(Request::Inbox {
                    delivery: read.delivery(),
                    filter: read.filter(),
                    peek,
                    limit,
                    require_trusted: read.require_trusted,
                })
                .await?;
            match fmt {
                Format::Json => print_json(&msgs),
                Format::Text => msgs.iter().for_each(|m| print_message(fmt, m)),
            }
        }
        Cmd::Listen { read } => loop {
            let m: Option<Message> = client
                .call(Request::Wait {
                    delivery: read.delivery(),
                    filter: read.filter(),
                    timeout_secs: None,
                    require_trusted: read.require_trusted,
                })
                .await?;
            if let Some(m) = m {
                print_message(fmt, &m);
            }
        },
        Cmd::Send {
            to,
            text,
            room,
            chat,
            plaintext,
            require_encryption,
        } => {
            let body = if text.is_empty() || text == ["-"] {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s.trim_end_matches('\n').to_owned()
            } else {
                text.join(" ")
            };
            let m: Message = client
                .call(Request::Send {
                    to,
                    body,
                    room: if room {
                        Some(true)
                    } else if chat {
                        Some(false)
                    } else {
                        None
                    },
                    encryption: if plaintext {
                        EncryptionMode::Plaintext
                    } else if require_encryption {
                        EncryptionMode::Require
                    } else {
                        EncryptionMode::Auto
                    },
                })
                .await?;
            print_message(fmt, &m);
        }
        Cmd::History {
            conversation,
            limit,
            before,
        } => {
            let msgs: Vec<Message> = client
                .call(Request::History {
                    conversation,
                    limit,
                    before_id: before,
                })
                .await?;
            match fmt {
                Format::Json => print_json(&msgs),
                Format::Text => msgs.iter().for_each(|m| print_message(fmt, m)),
            }
        }
        Cmd::Conversations => {
            let convs: Vec<ConversationSummary> = client.call(Request::Conversations).await?;
            match fmt {
                Format::Json => print_json(&convs),
                Format::Text => {
                    for c in convs {
                        let last = c.last_message.map(|m| m.body).unwrap_or_default();
                        println!("{:<40} {:>4} unread  {}", c.conversation, c.unread, one_line(&last, 60));
                    }
                }
            }
        }
        Cmd::Account(AccountCmd::Register { .. }) | Cmd::Install { .. } | Cmd::Uninstall { .. } | Cmd::Gui { .. } => {
            unreachable!("handled before connecting")
        }
        Cmd::Phone { register, contact } => {
            use agent_chat_proto::phone;
            let status: StatusInfo = client.call(Request::Status).await?;
            let Some(jid) = status.account.clone() else {
                return Err(Exit(
                    3,
                    "no account configured; run `agent-chat account add` or `account register` first".into(),
                )
                .into());
            };
            let domain = jid.rsplit('@').next().unwrap_or_default().to_owned();
            let devices: Vec<DeviceInfo> = client
                .call(Request::OmemoDevices { jid: None })
                .await
                .unwrap_or_default();
            let fingerprints: Vec<(u32, String)> = devices
                .iter()
                .filter(|d| d.this_device)
                .filter_map(|d| Some((d.device_id, d.fingerprint.clone()?)))
                .collect();
            let register_uri = phone::register_uri(&domain);
            let contact_uri = phone::contact_uri(&jid, &fingerprints);
            let local = phone::local_only(&domain);
            match fmt {
                Format::Json => print_json(&serde_json::json!({
                    "jid": jid,
                    "register_uri": register_uri,
                    "contact_uri": contact_uri,
                    "reachable_from_phone": !local,
                })),
                Format::Text => {
                    if local {
                        println!(
                            "Note: {jid} is on `{domain}`, which only this computer can reach. A phone needs a \
                             server it can reach over the network."
                        );
                    }
                    println!(
                        "Scan with your phone's camera; it opens your XMPP app (Monal or Siskin on iPhone, Conversations on Android).\n"
                    );
                    if !contact {
                        println!("1. Create your own account on {domain}:");
                        print!("{}", phone::qr_terminal(&register_uri));
                        println!("   {register_uri}\n");
                    }
                    if !register {
                        println!("2. Chat with {jid} (adds the contact and verifies its encryption keys):");
                        print!("{}", phone::qr_terminal(&contact_uri));
                        println!("   {contact_uri}");
                    }
                }
            }
        }
        Cmd::Mcp { consumer } => {
            return mcp::serve(client, consumer).await;
        }
        Cmd::Setup {
            copy,
            open,
            skill,
            mcp_json,
        } => {
            let status: StatusInfo = client.call(Request::Status).await?;
            let setup = agent_chat_proto::instructions::AgentSetup {
                profile: profile.name.clone(),
                jid: status.account.clone(),
                cli: agent_chat_daemon::web::preferred_cli_path()
                    .to_string_lossy()
                    .into_owned(),
                web_url: status.web_url.clone(),
            };
            let text = if skill {
                setup.skill_md()
            } else if mcp_json {
                setup.mcp_json()
            } else {
                setup.agent_prompt()
            };
            if copy {
                install::copy_to_clipboard(&text)?;
                eprintln!("Copied to the clipboard. Paste it into your agent.");
            }
            if open {
                match &status.web_url {
                    Some(u) => install::open_url(u)?,
                    None => return Err(anyhow!("the setup page is disabled (settings.http_port = 0)")),
                }
            }
            if !copy && !open {
                match fmt {
                    Format::Text => print!("{text}"),
                    Format::Json => print_json(&serde_json::json!({
                        "prompt": setup.agent_prompt(),
                        "web_url": status.web_url,
                        "claude_mcp_add": setup.claude_mcp_add(),
                        "mcp_json": serde_json::from_str::<serde_json::Value>(&setup.mcp_json())?,
                    })),
                }
            }
        }
        #[cfg(feature = "hub")]
        Cmd::Hub(_) => unreachable!("handled before resolving the profile"),
        Cmd::Account(AccountCmd::Show) => {
            let status: StatusInfo = client.call(Request::Status).await?;
            print_status(fmt, &status);
        }
        Cmd::Account(AccountCmd::Add {
            jid,
            password_stdin,
            hub,
            host,
            port,
            insecure_plaintext,
            store,
        }) => {
            let (jid, password, host, port, insecure_plaintext) = if hub {
                hub_account(&jid, password_stdin).await?
            } else {
                let password = read_password(&jid, password_stdin)?;
                (jid, password, host, port, insecure_plaintext)
            };
            let status: StatusInfo = client
                .call(Request::SetAccount(AccountParams {
                    jid,
                    password,
                    host,
                    port,
                    insecure_plaintext,
                    password_storage: match store {
                        Storage::Keychain => PasswordStorage::Keychain,
                        Storage::Config => PasswordStorage::ConfigFile,
                    },
                }))
                .await?;
            print_status(fmt, &status);
            if status.state != agent_chat_proto::model::ConnectionState::Online {
                return Err(Exit(
                    3,
                    format!(
                        "saved, but not connected yet (check the password and server); log: {}",
                        profile.log_path().display()
                    ),
                )
                .into());
            }
        }
        Cmd::Contacts { cmd } => match cmd.unwrap_or(ContactsCmd::List) {
            ContactsCmd::List => {
                let list: ContactList = client.call(Request::Contacts).await?;
                match fmt {
                    Format::Json => print_json(&list),
                    Format::Text => {
                        for c in &list.contacts {
                            println!(
                                "{:<40} {:<11} {:<5} {}",
                                c.jid,
                                c.presence.show.as_str(),
                                c.subscription,
                                c.name.clone().unwrap_or_default()
                            );
                        }
                        for r in &list.pending_requests {
                            println!("{:<40} wants to add you (agent-chat contacts approve {})", r.jid, r.jid);
                        }
                    }
                }
            }
            ContactsCmd::Add { jid, name } => ok(client.call_value(Request::ContactAdd { jid, name }).await?),
            ContactsCmd::Remove { jid } => ok(client.call_value(Request::ContactRemove { jid }).await?),
            ContactsCmd::Approve { jid } => ok(client.call_value(Request::ContactApprove { jid }).await?),
            ContactsCmd::Deny { jid } => ok(client.call_value(Request::ContactDeny { jid }).await?),
        },
        Cmd::Rooms { cmd } => match cmd.unwrap_or(RoomsCmd::List) {
            RoomsCmd::List => {
                let rooms: Vec<Room> = client.call(Request::Rooms).await?;
                match fmt {
                    Format::Json => print_json(&rooms),
                    Format::Text => {
                        for r in rooms {
                            println!(
                                "{:<40} as {:<16} {:<10} {:<9} {}",
                                r.jid,
                                r.nick,
                                if r.joined { "joined" } else { "not joined" },
                                if r.private { "encrypted" } else { "" },
                                r.subject.unwrap_or_default()
                            );
                        }
                    }
                }
            }
            RoomsCmd::Join {
                jid,
                nick,
                password,
                private,
            } => {
                let room: Room = client
                    .call(Request::RoomJoin {
                        jid,
                        nick,
                        password,
                        private,
                    })
                    .await?;
                match fmt {
                    Format::Json => print_json(&room),
                    Format::Text => println!(
                        "joined {} as {}{}",
                        room.jid,
                        room.nick,
                        if room.private { " (private, encrypted)" } else { "" }
                    ),
                }
            }
            RoomsCmd::Leave { jid } => ok(client.call_value(Request::RoomLeave { jid }).await?),
            RoomsCmd::Invite { room, jid, reason } => {
                ok(client.call_value(Request::RoomInvite { room, jid, reason }).await?)
            }
            RoomsCmd::Invites => {
                let invites: Vec<RoomInvite> = client.call(Request::RoomInvites).await?;
                match fmt {
                    Format::Json => print_json(&invites),
                    Format::Text => {
                        for i in invites {
                            println!(
                                "{:<40} from {:<30} {}",
                                i.room,
                                i.from.unwrap_or_default(),
                                i.reason.unwrap_or_default()
                            );
                        }
                    }
                }
            }
            RoomsCmd::Decline { room } => ok(client.call_value(Request::RoomInviteDecline { room }).await?),
        },
        Cmd::Presence { show, status } => {
            let show = Show::parse(&show).ok_or_else(|| {
                anyhow!("unknown presence {show:?}; use available, chat, away, xa, dnd or unavailable")
            })?;
            let status = (!status.is_empty()).then(|| status.join(" "));
            ok(client.call_value(Request::SetPresence { show, status }).await?);
        }
        Cmd::Omemo(cmd) => {
            let devices: Vec<DeviceInfo> = match cmd {
                OmemoCmd::Devices { jid } => client.call(Request::OmemoDevices { jid }).await?,
                OmemoCmd::Trust { jid, fingerprint } => vec![
                    client
                        .call(Request::OmemoSetTrust {
                            jid,
                            fingerprint,
                            trusted: true,
                        })
                        .await?,
                ],
                OmemoCmd::Distrust { jid, fingerprint } => vec![
                    client
                        .call(Request::OmemoSetTrust {
                            jid,
                            fingerprint,
                            trusted: false,
                        })
                        .await?,
                ],
            };
            match fmt {
                Format::Json => print_json(&devices),
                Format::Text => {
                    for d in devices {
                        println!(
                            "{:<32} {:<12} {:>10}  {:<9} {}{}",
                            d.jid,
                            d.protocol.as_str(),
                            d.device_id,
                            d.trust.as_str(),
                            d.fingerprint.unwrap_or_else(|| "(unknown)".into()),
                            if d.this_device { "  (this device)" } else { "" }
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

fn ok(_: serde_json::Value) {}

type AccountSettings = (String, String, Option<String>, Option<u16>, bool);

/// `account add --hub NAME`: create the account on the local hub and
/// connect to it over loopback. Until the client can pin the hub's
/// certificate (M6), that is plaintext SCRAM on loopback, which the hub
/// allows only for loopback peers.
#[cfg(feature = "hub")]
async fn hub_account(name: &str, password_stdin: bool) -> Result<AccountSettings> {
    let password = if password_stdin {
        Some(read_password(name, true)?)
    } else {
        None
    };
    let (jid, password, host, port) = hub::create_hub_account(name, password).await?;
    Ok((jid, password, Some(host), Some(port), true))
}

#[cfg(not(feature = "hub"))]
async fn hub_account(_name: &str, _password_stdin: bool) -> Result<AccountSettings> {
    Err(anyhow!("this agent-chat was built without the `hub` feature"))
}

/// `account register`: progress as JSON lines on stderr, one result object on stdout.
async fn register(profile: &Profile, opts: agent_chat_daemon::register::RegisterOptions, store: Storage) -> Result<()> {
    use agent_chat_daemon::register::{Progress, register as do_register};
    let progress = |p: Progress| eprintln!("{}", serde_json::to_string(&p).expect("serializable"));
    let registered = match do_register(&opts, &progress).await {
        Ok(r) => r,
        Err(e) => {
            println!(
                "{}",
                serde_json::json!({"ok": false, "error": e.kind(), "message": e.to_string()})
            );
            return Err(Exit(e.exit_code(), String::new()).into());
        }
    };
    // Use the new account for this profile (starts the daemon and logs in).
    let launcher = DaemonLauncher::new(std::env::current_exe()?);
    let client = IpcClient::connect_or_spawn(profile, &launcher).await?;
    let status: StatusInfo = client
        .call(Request::SetAccount(AccountParams {
            jid: registered.jid.clone(),
            password: opts.password.clone(),
            host: opts.host.clone(),
            port: opts.port,
            insecure_plaintext: opts.insecure_plaintext,
            password_storage: match store {
                Storage::Keychain => PasswordStorage::Keychain,
                Storage::Config => PasswordStorage::ConfigFile,
            },
        }))
        .await?;
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "jid": registered.jid,
            "profile": profile.name,
            "state": status.state,
            "password_stored": match store { Storage::Keychain => "keychain", Storage::Config => "config" },
            "pow": registered.pow,
        })
    );
    Ok(())
}

fn read_password(jid: &str, from_stdin: bool) -> Result<String> {
    if from_stdin {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        return Ok(s.trim_end_matches(['\r', '\n']).to_owned());
    }
    if let Ok(p) = std::env::var("AGENT_CHAT_PASSWORD")
        && !p.is_empty()
    {
        return Ok(p);
    }
    if !std::io::stdin().is_terminal() {
        return Err(anyhow!("no password: use --password-stdin or set AGENT_CHAT_PASSWORD"));
    }
    rpassword::prompt_password(format!("Password for {jid}: ")).context("reading password")
}

fn print_json<T: Serialize>(v: &T) {
    println!("{}", serde_json::to_string(v).expect("serializable"));
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() > max {
        format!("{}…", flat.chars().take(max).collect::<String>())
    } else {
        flat
    }
}

fn print_message(fmt: Format, m: &Message) {
    match fmt {
        Format::Json => print_json(m),
        Format::Text => {
            let who = match (m.kind, &m.nick) {
                (MessageKind::Groupchat, Some(n)) => format!("{} <{}>", m.conversation, n),
                _ if m.own => format!("me → {}", m.conversation),
                _ => m.from.clone(),
            };
            let lock = match &m.encryption {
                Some(e) if e.error.is_some() => " [undecryptable]",
                Some(e) if e.trust.is_trusted() => " [encrypted]",
                Some(_) => " [encrypted, untrusted device]",
                None => "",
            };
            println!(
                "#{} {} {}{}: {}",
                m.id,
                m.timestamp.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"),
                who,
                lock,
                m.body
            );
        }
    }
}

fn print_status(fmt: Format, s: &StatusInfo) {
    match fmt {
        Format::Json => print_json(s),
        Format::Text => {
            println!("profile:  {}", s.profile);
            println!("account:  {}", s.account.as_deref().unwrap_or("(none)"));
            println!("state:    {:?}", s.state);
            if let Some(j) = &s.bound_jid {
                println!("bound as: {j}");
            }
            if let Some(e) = &s.last_error {
                println!("error:    {e}");
            }
            println!("daemon:   pid {}", s.daemon_pid);
        }
    }
}
