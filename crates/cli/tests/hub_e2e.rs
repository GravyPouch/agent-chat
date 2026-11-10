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
        for p in ["alice", "bob"] {
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
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
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
        let created = ok_json(env.run_with_stdin(&["hub", "user", "add", user, "--password-stdin"], pw), &["user add"]);
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
    assert_eq!((m["from"].as_str(), m["body"].as_str()), (Some("bob@hubtest.local"), Some("hello alice")));
    let m = exchange(&env, "alice", "bob", "bob@hubtest.local", "hi bob");
    assert_eq!((m["from"].as_str(), m["body"].as_str()), (Some("alice@hubtest.local"), Some("hi bob")));

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
    let st = env.json(&["--profile", "alice", "account", "add", "--hub", "alice", "--store", "config"]);
    assert_eq!(st["account"], "alice@localhost");
    assert_eq!(st["state"], "online", "{st}");
    // The name is taken now: exit code 6, like `account register`.
    let out = env.run(&["--profile", "bob", "account", "add", "--hub", "alice", "--store", "config"]);
    assert_eq!(out.status.code(), Some(6), "{}", String::from_utf8_lossy(&out.stderr));
    let users = env.json(&["hub", "user", "list"]);
    assert_eq!(users.as_array().unwrap().len(), 1);
    // A second init must not change the domain.
    let out = env.run(&["hub", "init", "--domain", "other.local"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("permanent"));
}
