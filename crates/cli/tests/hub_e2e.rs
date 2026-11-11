//! End-to-end check of M1 (docs/design/embedded-server.md §9, acceptance
//! tests 5 and 6): the real `agent-chat` binary runs a hub and two daemons
//! in a temporary AGENT_CHAT_HOME.
//!
//! Scenario 1 of §8.3: bob → alice and back with `wait`/`send`. Then
//! `hub status` shows 2 sessions, `hub stop` makes both daemons report
//! `reconnecting` (the client's Suspended), and after `hub start` they are
//! back online (Reset).
//!
//! The server is chosen with `AGENT_CHAT_TEST_SERVER`: `hub` (default) or
//! anything else to skip (`prosody` runs are not wired up yet).

#![cfg(feature = "hub")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_agent-chat");

struct Env {
    home: tempfile::TempDir,
}

impl Env {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("AGENT_CHAT_HOME", self.home.path())
            .env_remove("AGENT_CHAT_PASSWORD")
            .env_remove("AGENT_CHAT_PROFILE")
            .env_remove("AGENT_CHAT_CONSUMER")
            .env_remove("AGENT_CHAT_HUB_CONFIG")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        let mut child = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        ok_json(self.run(args), args)
    }

    fn hub_dir(&self) -> PathBuf {
        self.home.path().join("hub")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        for p in ["alice", "bob", "agent"] {
            let _ = self.run(&["--profile", p, "daemon", "stop"]);
        }
        let _ = self.run(&["hub", "stop"]);
    }
}

fn ok_json(out: Output, what: &[&str]) -> Value {
    assert!(
        out.status.success(),
        "{what:?} failed ({}): {}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    serde_json::from_str(stdout.lines().last().unwrap_or("null")).unwrap_or_else(|e| panic!("{what:?}: {e}: {stdout}"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_state(env: &Env, profile: &str, state: &str, within: Duration) {
    let start = Instant::now();
    loop {
        let st = env.json(&["--profile", profile, "status"]);
        if st["state"] == state {
            return;
        }
        assert!(start.elapsed() < within, "{profile} never reached {state}: {st}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// `wait` in one profile while another sends; returns the received message.
fn exchange(env: &Env, from: &str, to: &str, to_jid: &str, body: &str) -> Value {
    let waiter = env
        .cmd(&["--profile", to, "wait", "--timeout", "30"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Let `wait` subscribe first (it only returns messages that arrive later
    // than the consumer's start anyway, so this is just to be safe).
    std::thread::sleep(Duration::from_millis(500));
    env.json(&["--profile", from, "send", to_jid, body]);
    ok_json(waiter.wait_with_output().unwrap(), &["wait"])
}

fn hub_server_selected() -> bool {
    match std::env::var("AGENT_CHAT_TEST_SERVER") {
        Err(_) => true,
        Ok(s) if s == "hub" => true,
        Ok(other) => {
            eprintln!("skipping: AGENT_CHAT_TEST_SERVER={other} is not wired up for this test");
            false
        }
    }
}

#[test]
fn two_agents_chat_through_the_hub_and_survive_a_restart() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let init = env.json(&["hub", "init", "--domain", "hubtest.local", "--listen", &listen]);
    assert_eq!(init["domain"], "hubtest.local");
    let started = env.json(&["hub", "start"]);
    assert_eq!(started["started"], true);
    assert!(Path::new(&env.hub_dir().join("hub.json")).exists());

    for (user, pw) in [("alice", "alice-password-123"), ("bob", "bob-password-4567")] {
        let created = ok_json(
            env.run_with_stdin(&["hub", "user", "add", user, "--password-stdin"], pw),
            &["user add"],
        );
        assert_eq!(created["jid"], format!("{user}@hubtest.local"));
        assert!(created.get("password").is_none(), "a chosen password is not echoed");
        assert_eq!(created["connect"]["port"], port);
        let jid = format!("{user}@hubtest.local");
        let port = port.to_string();
        let status = ok_json(
            env.run_with_stdin(
                &[
                    "--profile",
                    user,
                    "account",
                    "add",
                    &jid,
                    "--host",
                    "127.0.0.1",
                    "--port",
                    &port,
                    "--insecure-plaintext",
                    "--store",
                    "config",
                    "--password-stdin",
                ],
                pw,
            ),
            &["account add"],
        );
        assert_eq!(status["state"], "online", "{status}");
    }

    // Acceptance test 5: scenario 1 of §8.3, both directions.
    let m = exchange(&env, "bob", "alice", "alice@hubtest.local", "hello alice");
    assert_eq!(
        (m["from"].as_str(), m["body"].as_str()),
        (Some("bob@hubtest.local"), Some("hello alice"))
    );
    let m = exchange(&env, "alice", "bob", "bob@hubtest.local", "hi bob");
    assert_eq!(
        (m["from"].as_str(), m["body"].as_str()),
        (Some("alice@hubtest.local"), Some("hi bob"))
    );

    // Acceptance test 6.
    let status = env.json(&["hub", "status"]);
    assert_eq!(status["sessions"], 2, "{status}");
    env.json(&["hub", "stop"]);
    for p in ["alice", "bob"] {
        wait_for_state(&env, p, "reconnecting", Duration::from_secs(10));
    }
    env.json(&["hub", "start"]);
    for p in ["alice", "bob"] {
        wait_for_state(&env, p, "online", Duration::from_secs(60));
    }
    let m = exchange(&env, "bob", "alice", "alice@hubtest.local", "after the restart");
    assert_eq!(m["body"], "after the restart");
    assert_eq!(env.json(&["hub", "status"])["sessions"], 2);
}

/// A started hub on a free port with plaintext-loopback profiles for `users`.
fn hub_with_profiles(env: &Env, users: &[(&str, &str)]) -> u16 {
    let port = free_port();
    let listen = format!("127.0.0.1:{port}");
    let http = format!("127.0.0.1:{}", free_port());
    env.json(&[
        "hub",
        "init",
        "--domain",
        "hubtest.local",
        "--listen",
        &listen,
        "--http",
        &http,
    ]);
    env.json(&["hub", "start"]);
    for (user, pw) in users {
        ok_json(
            env.run_with_stdin(&["hub", "user", "add", user, "--password-stdin"], pw),
            &["user add"],
        );
        let jid = format!("{user}@hubtest.local");
        let port = port.to_string();
        let status = ok_json(
            env.run_with_stdin(
                &[
                    "--profile",
                    user,
                    "account",
                    "add",
                    &jid,
                    "--host",
                    "127.0.0.1",
                    "--port",
                    &port,
                    "--insecure-plaintext",
                    "--store",
                    "config",
                    "--password-stdin",
                ],
                pw,
            ),
            &["account add"],
        );
        assert_eq!(status["state"], "online", "{status}");
    }
    port
}

/// M3 demo and acceptance test 1 (§8.3 scenario 2 with 120 messages): a
/// daemon that was offline gets everything exactly once, from offline
/// storage and MAM (which share stanza-ids).
#[test]
fn offline_daemon_catches_up_exactly_once() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    hub_with_profiles(&env, &[("alice", "alice-password-123"), ("bob", "bob-password-4567")]);
    env.json(&["--profile", "alice", "daemon", "stop"]);
    for i in 0..120 {
        env.json(&[
            "--profile",
            "bob",
            "send",
            "alice@hubtest.local",
            &format!("catch-up {i}"),
        ]);
    }
    env.json(&["--profile", "alice", "daemon", "start"]);
    wait_for_state(&env, "alice", "online", Duration::from_secs(30));
    let start = Instant::now();
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    while start.elapsed() < Duration::from_secs(30) {
        let inbox = env.json(&["--profile", "alice", "inbox"]);
        for m in inbox.as_array().unwrap() {
            *seen
                .entry(m["body"].as_str().unwrap_or_default().to_owned())
                .or_default() += 1;
        }
        if seen.len() >= 120 {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert_eq!(
        seen.len(),
        120,
        "missing messages: {:?}",
        seen.keys().collect::<Vec<_>>()
    );
    let dups: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).collect();
    assert!(dups.is_empty(), "duplicates: {dups:?}");
    // Nothing more arrives later (e.g. a second copy from MAM).
    std::thread::sleep(Duration::from_secs(2));
    let late = env.json(&["--profile", "alice", "inbox"]);
    assert_eq!(late.as_array().unwrap().len(), 0, "{late}");
}

/// `send --require-encryption` (retried while the peer's devices are still
/// being published), then the receiving side's `inbox` entry for it.
fn encrypted_exchange(env: &Env, from: &str, to: &str, to_jid: &str, body: &str) -> Value {
    let start = Instant::now();
    loop {
        let out = env.run(&["--profile", from, "send", to_jid, body, "--require-encryption"]);
        if out.status.success() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "encrypted send failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let start = Instant::now();
    loop {
        let inbox = env.json(&["--profile", to, "inbox"]);
        if let Some(m) = inbox.as_array().unwrap().iter().find(|m| m["body"] == body) {
            return m.clone();
        }
        assert!(start.elapsed() < Duration::from_secs(20), "{to} never got {body:?}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// M4 acceptance test 1 (§8.3 scenario 5): OMEMO between two agent-chat
/// daemons over the hub's PEP, both directions. The daemons prefer
/// OMEMO:2; legacy OMEMO is covered by `dev/omemo-bot/interop-test.sh`
/// with `XMPP_SERVER=hub` (python-omemo).
#[test]
fn omemo_between_two_daemons() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    hub_with_profiles(&env, &[("alice", "alice-password-123"), ("bob", "bob-password-4567")]);
    for (from, to, to_jid, from_jid) in [
        ("bob", "alice", "alice@hubtest.local", "bob@hubtest.local"),
        ("alice", "bob", "bob@hubtest.local", "alice@hubtest.local"),
    ] {
        let body = format!("encrypted hello from {from}");
        let m = encrypted_exchange(&env, from, to, to_jid, &body);
        assert_eq!(m["from"], from_jid);
        assert_eq!(m["encryption"]["protocol"], "omemo2", "{m}");
        assert!(m["encryption"].get("error").is_none_or(|e| e.is_null()), "{m}");
    }
    // Each side sees the other's device.
    let devices = env.json(&["--profile", "alice", "omemo", "devices", "bob@hubtest.local"]);
    assert!(!devices.as_array().unwrap().is_empty(), "{devices}");
}

/// Poll `inbox` until a message with `body` shows up.
fn inbox_until(env: &Env, profile: &str, body: &str) -> Value {
    let start = Instant::now();
    loop {
        let inbox = env.json(&["--profile", profile, "inbox"]);
        if let Some(m) = inbox.as_array().unwrap().iter().find(|m| m["body"] == body) {
            return m.clone();
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{profile} never got {body:?}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// M5 acceptance test 1 (§8.3 scenarios 6 and 4): a private room with
/// OMEMO and an invitation, then a public room, with real daemons.
#[test]
fn private_omemo_room_and_public_room() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    hub_with_profiles(&env, &[("alice", "alice-password-123"), ("bob", "bob-password-4567")]);
    let room = "secret@conference.hubtest.local";

    // Scenario 6: alice creates a private room and invites bob.
    let joined = env.json(&["--profile", "alice", "rooms", "join", room, "--private"]);
    assert_eq!(joined["jid"], room, "{joined}");
    env.json(&["--profile", "alice", "rooms", "invite", room, "bob@hubtest.local"]);
    let start = Instant::now();
    loop {
        let invites = env.json(&["--profile", "bob", "rooms", "invites"]);
        if invites.as_array().unwrap().iter().any(|i| i["room"] == room) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "no invitation: {invites}");
        std::thread::sleep(Duration::from_millis(250));
    }
    env.json(&["--profile", "bob", "rooms", "join", room]);
    // alice learns bob's real JID from his presence, then encrypts for him.
    let start = Instant::now();
    loop {
        let out = env.run(&[
            "--profile",
            "alice",
            "send",
            room,
            "team secret",
            "--require-encryption",
        ]);
        if out.status.success() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "encrypted room send failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let m = inbox_until(&env, "bob", "team secret");
    assert_eq!(m["kind"], "groupchat", "{m}");
    assert_eq!(m["conversation"], room);
    assert!(m["encryption"]["protocol"].is_string(), "{m}");
    assert!(m["encryption"].get("error").is_none_or(|e| e.is_null()), "{m}");
    // And back.
    env.json(&["--profile", "bob", "send", room, "got it", "--require-encryption"]);
    let m = inbox_until(&env, "alice", "got it");
    assert!(m["encryption"]["protocol"].is_string(), "{m}");

    // Scenario 4: a public room, plaintext, `wait --room`.
    let public = "lobby@conference.hubtest.local";
    env.json(&["--profile", "alice", "rooms", "join", public]);
    env.json(&["--profile", "bob", "rooms", "join", public]);
    let waiter = env
        .cmd(&["--profile", "alice", "wait", "--room", public, "--timeout", "30"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    env.json(&["--profile", "bob", "send", public, "hello lobby"]);
    let m = ok_json(waiter.wait_with_output().unwrap(), &["wait --room"]);
    assert_eq!(
        (m["body"].as_str(), m["kind"].as_str()),
        (Some("hello lobby"), Some("groupchat"))
    );
}

/// Public-mode registration: `agent-chat account register` solves the hub's
/// `urn:xmpp:ibr-pow:0` challenge (low difficulty for the test) and the
/// new account logs in. Loopback is normally exempt; here it isn't.
#[test]
fn account_register_solves_the_hubs_proof_of_work() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    let port = free_port();
    env.json(&[
        "hub",
        "init",
        "--domain",
        "hubtest.local",
        "--listen",
        &format!("127.0.0.1:{port}"),
    ]);
    let paths = agent_chat_server::HubPaths::at(env.hub_dir());
    let mut cfg = agent_chat_server::Config::load(paths.config()).unwrap();
    cfg.registration.mode = Some(agent_chat_server::config::RegistrationMode::Pow);
    cfg.registration.exempt_loopback = false;
    cfg.registration.pow_bits = 14;
    cfg.registration.pow_count = 8;
    std::fs::write(paths.config(), cfg.to_toml().unwrap()).unwrap();
    env.json(&["hub", "start"]);
    let out = env.run(&[
        "--profile",
        "agent",
        "account",
        "register",
        "--server",
        "hubtest.local",
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--insecure-plaintext",
        "--store",
        "config",
    ]);
    let r = ok_json(out, &["account register"]);
    let jid = r["jid"].as_str().unwrap().to_owned();
    assert!(jid.ends_with("@hubtest.local"), "{r}");
    assert_eq!(r["pow"]["bits"], 14, "solved the hub's challenge: {r}");
    wait_for_state(&env, "agent", "online", Duration::from_secs(30));
    let users = env.json(&["hub", "user", "list"]);
    let created = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["jid"] == jid.as_str())
        .unwrap();
    assert_eq!(created["created_via"], "pow");
}

/// `hub init --public`: certificate files (absent at first: a temporary
/// certificate), PoW registration, the web page with sign-up.
#[test]
fn hub_init_public_writes_a_public_configuration() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    let (port, http) = (free_port(), free_port());
    let certs = env.home.path().join("certs");
    let out = env.run(&["hub", "init", "--public", "--domain", "localhost"]);
    assert!(!out.status.success(), "a public hub needs a real domain");
    let init = env.json(&[
        "hub",
        "init",
        "--public",
        "--domain",
        "chat.example.org",
        "--listen",
        &format!("127.0.0.1:{port}"),
        "--cert",
        certs.join("fullchain.pem").to_str().unwrap(),
        "--key",
        certs.join("privkey.pem").to_str().unwrap(),
        "--http",
        &format!("127.0.0.1:{http}"),
        "--web-signup",
    ]);
    assert_eq!(init["tls_mode"], "files", "{init}");
    let cfg = agent_chat_server::Config::load(&env.hub_dir().join("hub.toml")).unwrap();
    assert_eq!(
        cfg.registration_mode(),
        agent_chat_server::config::RegistrationMode::Pow
    );
    assert!(cfg.registration.web);
    env.json(&["hub", "start"]);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", http)).unwrap();
    use std::io::{Read, Write};
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut body = String::new();
    s.read_to_string(&mut body).unwrap();
    assert!(
        body.starts_with("HTTP/1.1 200") && body.contains("\"ok\":true"),
        "{body}"
    );
}

#[test]
fn account_add_hub_creates_and_configures_an_account() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    let port = free_port();
    env.json(&["hub", "init", "--listen", &format!("127.0.0.1:{port}")]);
    // `--hub` starts the hub if needed and generates a password.
    let st = env.json(&[
        "--profile",
        "alice",
        "account",
        "add",
        "--hub",
        "alice",
        "--store",
        "config",
    ]);
    assert_eq!(st["account"], "alice@localhost");
    assert_eq!(st["state"], "online", "{st}");
    // The name is taken now: exit code 6, like `account register`.
    let out = env.run(&[
        "--profile",
        "bob",
        "account",
        "add",
        "--hub",
        "alice",
        "--store",
        "config",
    ]);
    assert_eq!(out.status.code(), Some(6), "{}", String::from_utf8_lossy(&out.stderr));
    let users = env.json(&["hub", "user", "list"]);
    assert_eq!(users.as_array().unwrap().len(), 1);
    // A second init must not change the domain.
    let out = env.run(&["hub", "init", "--domain", "other.local"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("permanent"));
}

/// The agents' stop hook hands unread messages over once (for both the CLI's
/// and the MCP server's consumer), and does nothing without a daemon.
#[test]
fn stop_hook_pushes_unread_messages_once() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    hub_with_profiles(&env, &[("alice", "alice-password-123"), ("bob", "bob-password-4567")]);
    let hook = |agent: &str| env.json(&["--profile", "alice", "hook", "stop", "--agent", agent]);
    assert_eq!(hook("claude"), serde_json::json!({}));

    env.json(&["--profile", "bob", "send", "alice@hubtest.local", "push me"]);
    let start = Instant::now();
    let out = loop {
        let out = hook("codex");
        if out != serde_json::json!({}) {
            break out;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "the message never arrived");
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(out["decision"], "block", "{out}");
    let reason = out["reason"].as_str().unwrap();
    assert!(
        reason.contains("push me") && reason.contains("bob@hubtest.local"),
        "{reason}"
    );

    // Read for both consumers: nothing is returned twice.
    assert_eq!(env.json(&["--profile", "alice", "inbox"]), serde_json::json!([]));
    assert_eq!(
        env.json(&["--profile", "alice", "inbox", "--consumer", "mcp"]),
        serde_json::json!([])
    );
    assert_eq!(hook("cursor"), serde_json::json!({}));

    // Without a daemon the hook is a no-op and doesn't start one.
    env.json(&["--profile", "alice", "daemon", "stop"]);
    assert_eq!(hook("claude"), serde_json::json!({}));
    assert!(!env.run(&["--profile", "alice", "daemon", "status"]).status.success());
}

/// `delete-chat` removes a 1:1 conversation's history, and leaves and
/// removes a group chat.
#[test]
fn delete_chat_removes_history_and_leaves_rooms() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    hub_with_profiles(&env, &[("alice", "alice-password-123"), ("bob", "bob-password-4567")]);
    exchange(&env, "bob", "alice", "alice@hubtest.local", "to be deleted");
    let history = env.json(&["--profile", "alice", "history", "bob@hubtest.local"]);
    assert_eq!(history.as_array().unwrap().len(), 1, "{history}");

    let out = env.json(&["--profile", "alice", "delete-chat", "bob@hubtest.local"]);
    assert_eq!(out["deleted"], 1, "{out}");
    let history = env.json(&["--profile", "alice", "history", "bob@hubtest.local"]);
    assert_eq!(history, serde_json::json!([]));
    let convs = env.json(&["--profile", "alice", "conversations"]);
    assert!(!convs.to_string().contains("bob@hubtest.local"), "{convs}");

    let room = env.json(&[
        "--profile",
        "alice",
        "rooms",
        "create",
        "--name",
        "gone soon",
        "--public",
    ]);
    let jid = room["jid"].as_str().unwrap().to_owned();
    env.json(&["--profile", "alice", "send", &jid, "hello room"]);
    env.json(&["--profile", "alice", "delete-chat", &jid]);
    let rooms = env.json(&["--profile", "alice", "rooms", "list"]);
    assert!(!rooms.to_string().contains(&jid), "{rooms}");
    assert_eq!(
        env.json(&["--profile", "alice", "history", &jid]),
        serde_json::json!([])
    );
}

/// Photos and files: encrypted by default (the hub only stores
/// ciphertext), as a plain link with --plaintext; `wait` downloads them.
#[test]
fn files_are_uploaded_encrypted_and_downloaded() {
    if !hub_server_selected() {
        return;
    }
    let env = Env {
        home: tempfile::tempdir().unwrap(),
    };
    hub_with_profiles(&env, &[("alice", "alice-password-123"), ("bob", "bob-password-4567")]);
    let photo = env.home.path().join("cat photo.png");
    let content: Vec<u8> = (0..200_000u32).map(|i| (i * 7 % 251) as u8).collect();
    std::fs::write(&photo, &content).unwrap();

    for plaintext in [false, true] {
        let waiter = env
            .cmd(&["--profile", "alice", "wait", "--timeout", "30"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let path = photo.to_string_lossy().into_owned();
        let mut args = vec!["--profile", "bob", "send", "alice@hubtest.local", "--file", &path];
        if plaintext {
            args.push("--plaintext");
        }
        let sent = env.json(&args);
        assert_eq!(sent["attachment"]["name"], "cat photo.png", "{sent}");
        assert_eq!(sent["attachment"]["encrypted"], !plaintext, "{sent}");
        let got = ok_json(waiter.wait_with_output().unwrap(), &["wait"]);
        let att = &got["attachment"];
        assert_eq!(att["name"], "cat photo.png", "{got}");
        assert_eq!(att["mime"], "image/png", "{got}");
        assert_eq!(att["encrypted"], !plaintext, "{got}");
        let local = att["path"].as_str().unwrap_or_else(|| panic!("not downloaded: {got}"));
        assert_eq!(std::fs::read(local).unwrap(), content);
    }

    // The hub has two uploads; the encrypted one isn't the photo.
    let uploads: Vec<Vec<u8>> = std::fs::read_dir(env.hub_dir().join("uploads"))
        .unwrap()
        .flatten()
        .map(|d| std::fs::read(d.path().join("cat%20photo.png")).unwrap())
        .collect();
    assert_eq!(uploads.len(), 2);
    assert_eq!(
        uploads.iter().filter(|u| **u == content).count(),
        1,
        "only the plaintext one"
    );
    assert!(
        uploads.iter().any(|u| u.len() == content.len() + 16),
        "ciphertext with the GCM tag"
    );
}
