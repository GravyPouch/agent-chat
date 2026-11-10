//! `agent-chat`: command-line XMPP client for humans and AI agents.
//!
//! Agent-facing commands print JSON to stdout (one compact document per
//! line). Exit codes: 0 ok, 1 error, 2 timeout, 3 not connected / no
//! account, 4 encryption unavailable.

use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use agent_chat_proto::ipc::{
    AccountParams, ContactList, EncryptionMode, ErrorCode, PasswordStorage, Request,
};
use agent_chat_proto::model::{
    ConversationSummary, Delivery, DeviceInfo, Filter, Message, MessageKind, Room, RoomInvite,
    Show, StatusInfo,
};
use agent_chat_proto::{CallError, DaemonLauncher, IpcClient, Profile};
use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;

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
        jid: String,
        /// Read the password from stdin instead of prompting.
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
    let profile = Profile::resolve(cli.profile.as_deref())?;
    let fmt = cli.format;

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
                threads: threads.unwrap_or_else(|| {
                    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
                }),
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
        Cmd::Account(AccountCmd::Register { .. }) => unreachable!("handled before connecting"),
        Cmd::Account(AccountCmd::Show) => {
            let status: StatusInfo = client.call(Request::Status).await?;
            print_status(fmt, &status);
        }
        Cmd::Account(AccountCmd::Add {
            jid,
            password_stdin,
            host,
            port,
            insecure_plaintext,
            store,
        }) => {
            let password = read_password(&jid, password_stdin)?;
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
            let show = Show::parse(&show)
                .ok_or_else(|| anyhow!("unknown presence {show:?}; use available, chat, away, xa, dnd or unavailable"))?;
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

/// `account register`: progress as JSON lines on stderr, one result object on stdout.
async fn register(
    profile: &Profile,
    opts: agent_chat_daemon::register::RegisterOptions,
    store: Storage,
) -> Result<()> {
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
        return Err(anyhow!(
            "no password: use --password-stdin or set AGENT_CHAT_PASSWORD"
        ));
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
