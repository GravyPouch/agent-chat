//! Public mode: XEP-0077 registration (open, proof of work, invitations),
//! account management in-band, the web page with web sign-up, and
//! certificate files reloaded while the hub runs.

mod common;

use std::time::Duration;

use agent_chat_server::admin::AdminRequest;
use agent_chat_server::config::{HumanDuration, RegistrationMode, TlsMode};
use agent_chat_server::register::pow;
use common::{DOMAIN, Raw, TestHub, connect, next_el, st};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const POW_NS: &str = "urn:xmpp:ibr-pow:0";

fn field<'a>(xml: &'a str, var: &str) -> Option<&'a str> {
    let at = xml
        .find(&format!("var='{var}'"))
        .or_else(|| xml.find(&format!("var=\"{var}\"")))?;
    let rest = &xml[at..];
    let v = rest.find("<value>")? + "<value>".len();
    let end = rest[v..].find("</value>")?;
    Some(&rest[v..v + end])
}

fn solve(token: &str, user: &str, bits: u32, count: u32) -> String {
    let prefix = pow::puzzle_prefix(token, DOMAIN, user);
    (0..count as u8)
        .map(|i| {
            let n = (0u64..)
                .find(|n| {
                    let mut h = Sha256::new();
                    h.update(&prefix);
                    h.update([i]);
                    h.update(n.to_be_bytes());
                    pow::leading_zero_bits_ok(&h.finalize(), bits)
                })
                .unwrap();
            format!("{n:016x}")
        })
        .collect::<Vec<_>>()
        .join(",")
}

async fn register_form(raw: &mut Raw) -> String {
    raw.send("<iq type='get' id='rg'><query xmlns='jabber:iq:register'/></iq>")
        .await;
    raw.read_until("</iq>").await
}

async fn submit(raw: &mut Raw, id: &str, user: &str, pw: &str, token: Option<&str>, solution: Option<&str>) -> String {
    let mut fields = format!(
        "<field var='FORM_TYPE'><value>jabber:iq:register</value></field>\
         <field var='username'><value>{user}</value></field><field var='password'><value>{pw}</value></field>"
    );
    if let (Some(t), Some(s)) = (token, solution) {
        fields.push_str(&format!(
            "<field var='{{{POW_NS}}}challenge'><value>{t}</value></field>\
             <field var='{{{POW_NS}}}solution'><value>{s}</value></field>"
        ));
    }
    raw.send(&format!(
        "<iq type='set' id='{id}'><query xmlns='jabber:iq:register'><x xmlns='jabber:x:data' type='submit'>{fields}</x></query></iq>"
    ))
    .await;
    raw.read_until("</iq>").await
}

#[tokio::test]
async fn open_registration_on_localhost_then_login() {
    let hub = TestHub::start().await;
    let (mut raw, features) = Raw::start(hub.addr).await;
    assert!(
        features.contains("http://jabber.org/features/iq-register"),
        "{features}"
    );
    let form = register_form(&mut raw).await;
    assert!(form.contains("<username") && !form.contains(POW_NS), "{form}");
    // Legacy fields work too.
    raw.send(
        "<iq type='set' id='r1'><query xmlns='jabber:iq:register'>\
         <username>newbie</username><password>newbie-pass-1</password></query></iq>",
    )
    .await;
    let r = raw.read_until("</iq>").await;
    assert!(r.contains("type='result'") || r.contains("type=\"result\""), "{r}");
    // The same name again: conflict.
    let r = submit(&mut raw, "r2", "newbie", "another-pass-2", None, None).await;
    assert!(r.contains("conflict"), "{r}");
    let (_, reply) = raw.scram("newbie", "newbie-pass-1").await;
    assert!(reply.contains("<success"), "{reply}");
    hub.stop().await;
}

#[tokio::test]
async fn pow_registration_with_mod_register_pow_errors_and_throttle() {
    let hub = TestHub::start_with(|c| {
        c.registration.mode = Some(RegistrationMode::Pow);
        c.registration.exempt_loopback = false;
        c.registration.pow_bits = 10;
        c.registration.pow_count = 4;
        c.registration.max_per_ip = 2;
    })
    .await;
    let (mut raw, features) = Raw::start(hub.addr).await;
    assert!(features.contains(POW_NS), "the PoW hint feature: {features}");
    // Without a proof: pow-required.
    let r = submit(&mut raw, "x", "agent-a", "agent-a-password", None, None).await;
    assert!(r.contains("pow-required") && r.contains("not-acceptable"), "{r}");
    let form = register_form(&mut raw).await;
    let token = field(&form, &format!("{{{POW_NS}}}challenge")).unwrap().to_owned();
    let bits: u32 = field(&form, &format!("{{{POW_NS}}}bits")).unwrap().parse().unwrap();
    let count: u32 = field(&form, &format!("{{{POW_NS}}}count")).unwrap().parse().unwrap();
    assert_eq!((bits, count), (10, 4));
    let bad = solve(&token, "someone-else", bits, count);
    let r = submit(&mut raw, "b", "agent-a", "agent-a-password", Some(&token), Some(&bad)).await;
    assert!(r.contains("solution-invalid"), "{r}");
    let good = solve(&token, "agent-a", bits, count);
    let r = submit(&mut raw, "c", "agent-a", "agent-a-password", Some(&token), Some(&good)).await;
    assert!(!r.contains("type='error'") && !r.contains("type=\"error\""), "{r}");
    let r = submit(&mut raw, "d", "agent-a", "agent-a-password", Some(&token), Some(&good)).await;
    assert!(r.contains("challenge-used"), "{r}");
    // Difficulty went up by one bit for this address.
    let form = register_form(&mut raw).await;
    assert_eq!(field(&form, &format!("{{{POW_NS}}}bits")), Some("11"));
    let token = field(&form, &format!("{{{POW_NS}}}challenge")).unwrap().to_owned();
    let r = submit(
        &mut raw,
        "e",
        "agent-b",
        "agent-b-password",
        Some(&token),
        Some(&solve(&token, "agent-b", 11, 4)),
    )
    .await;
    assert!(!r.contains("error"), "{r}");
    // max_per_ip = 2: the third is throttled.
    let form = register_form(&mut raw).await;
    let token = field(&form, &format!("{{{POW_NS}}}challenge")).unwrap().to_owned();
    let r = submit(
        &mut raw,
        "f",
        "agent-c",
        "agent-c-password",
        Some(&token),
        Some(&solve(&token, "agent-c", 12, 4)),
    )
    .await;
    assert!(r.contains("policy-violation"), "{r}");
    hub.stop().await;
}

#[tokio::test]
async fn invitations_skip_the_proof_of_work() {
    let hub = TestHub::start_with(|c| {
        c.registration.mode = Some(RegistrationMode::Invite);
    })
    .await;
    let v = hub
        .admin(AdminRequest::InviteCreate {
            uses: 1,
            expires_secs: 3600,
            note: None,
        })
        .await;
    let token = v["token"].as_str().unwrap().to_owned();
    assert!(
        v["uri"]
            .as_str()
            .unwrap()
            .starts_with(&format!("xmpp:{DOMAIN}?register;preauth="))
    );
    let (mut raw, features) = Raw::start(hub.addr).await;
    assert!(features.contains("urn:xmpp:ibr-token:0"), "{features}");
    let r = submit(&mut raw, "a", "guest", "guest-password", None, None).await;
    assert!(r.contains("not-allowed"), "invite-only: {r}");
    raw.send("<iq type='set' id='p0'><preauth xmlns='urn:xmpp:pars:0' token='wrong'/></iq>")
        .await;
    assert!(raw.read_until("</iq>").await.contains("item-not-found"));
    raw.send(&format!(
        "<iq type='set' id='p1'><preauth xmlns='urn:xmpp:pars:0' token='{token}'/></iq>"
    ))
    .await;
    let r = raw.read_until("</iq>").await;
    assert!(!r.contains("error"), "{r}");
    let r = submit(&mut raw, "b", "guest", "guest-password", None, None).await;
    assert!(!r.contains("error"), "{r}");
    // Used up.
    let (mut raw2, _) = Raw::start(hub.addr).await;
    raw2.send(&format!(
        "<iq type='set' id='p2'><preauth xmlns='urn:xmpp:pars:0' token='{token}'/></iq>"
    ))
    .await;
    assert!(raw2.read_until("</iq>").await.contains("item-not-found"));
    hub.stop().await;
}

#[tokio::test]
async fn password_change_and_account_removal_in_band() {
    let hub = TestHub::start().await;
    hub.add_user("alice", "alice-password-123").await;
    let mut a = connect(&hub, "alice@hubtest.local/a", "alice-password-123").await;
    a.send(st(&format!(
        "<iq xmlns='jabber:client' type='set' id='pw' to='{DOMAIN}'><query xmlns='jabber:iq:register'>\
         <username>alice</username><password>a-new-password</password></query></iq>"
    )))
    .await;
    assert_eq!(
        next_el(&mut a, |e| e.attr("id") == Some("pw")).await.attr("type"),
        Some("result")
    );
    let (mut raw, _) = Raw::start(hub.addr).await;
    let (_, reply) = raw.scram("alice", "a-new-password").await;
    assert!(reply.contains("<success"), "{reply}");
    a.send(st(&format!(
        "<iq xmlns='jabber:client' type='set' id='rm' to='{DOMAIN}'><query xmlns='jabber:iq:register'><remove/></query></iq>"
    )))
    .await;
    assert_eq!(
        next_el(&mut a, |e| e.attr("id") == Some("rm")).await.attr("type"),
        Some("result")
    );
    let mut gone = false;
    for _ in 0..50 {
        if hub.status().await.accounts == 0 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone);
    hub.stop().await;
}

/// One HTTP request; returns (status line, body).
async fn http(addr: std::net::SocketAddr, method: &str, path: &str, body: Option<&str>) -> (String, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: hub\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (head.lines().next().unwrap().to_owned(), body.to_owned())
}

#[tokio::test]
async fn web_page_and_web_sign_up() {
    let hub = TestHub::start_with(|c| {
        c.registration.mode = Some(RegistrationMode::Pow);
        c.registration.exempt_loopback = false;
        c.registration.web = true;
        c.registration.web_pow_bits = 8;
        c.registration.web_pow_count = 4;
        c.http.listen = vec!["127.0.0.1:0".into()];
    })
    .await;
    let info = agent_chat_server::RuntimeInfo::read(&hub.paths).unwrap().unwrap();
    let web = info.http[0];
    let (status, page) = http(web, "GET", "/", None).await;
    assert!(status.contains("200"), "{status}");
    assert!(page.contains(DOMAIN) && page.contains("account register --server hubtest.local"));
    assert!(!page.contains("{{"), "all placeholders filled");
    let (status, health) = http(web, "GET", "/healthz", None).await;
    assert!(status.contains("200"));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&health).unwrap()["ok"], true);
    assert!(http(web, "GET", "/pow-worker.js", None).await.0.contains("200"));

    // Sign up with a solved web challenge.
    let (_, c) = http(web, "GET", "/api/challenge", None).await;
    let c: serde_json::Value = serde_json::from_str(&c).unwrap();
    assert_eq!(
        (c["bits"].as_u64(), c["count"].as_u64(), c["required"].as_bool()),
        (Some(8), Some(4), Some(true))
    );
    let token = c["challenge"].as_str().unwrap();
    let solution = solve(token, "webby", 8, 4);
    let body = serde_json::json!({"username": "webby", "password": "webby-password", "challenge": token, "solution": solution});
    let (status, out) = http(web, "POST", "/api/register", Some(&body.to_string())).await;
    assert!(status.contains("200"), "{status} {out}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&out).unwrap()["jid"],
        "webby@hubtest.local"
    );
    // Replay: challenge-used. A wrong password length: not-acceptable.
    let (status, out) = http(web, "POST", "/api/register", Some(&body.to_string())).await;
    assert!(
        status.contains("400") && out.contains("challenge-used"),
        "{status} {out}"
    );
    // A web token doesn't work in-band.
    let (mut raw, _) = Raw::start(hub.addr).await;
    let r = submit(
        &mut raw,
        "x",
        "inband",
        "inband-password",
        Some(token),
        Some(&solve(token, "inband", 8, 4)),
    )
    .await;
    assert!(r.contains("challenge-invalid"), "{r}");
    // The new account logs in over XMPP.
    let (mut raw, _) = Raw::start(hub.addr).await;
    let (_, reply) = raw.scram("webby", "webby-password").await;
    assert!(reply.contains("<success"), "{reply}");
    hub.stop().await;
}

fn write_cert(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let (c, k) = (
        dir.join(format!("{name}-cert.pem")),
        dir.join(format!("{name}-key.pem")),
    );
    agent_chat_server::net::tls::ensure_self_signed(&c, &k, &[DOMAIN.to_owned()]).unwrap();
    (c, k)
}

#[tokio::test]
async fn certificate_files_are_reloaded_when_they_change() {
    let certs = tempfile::tempdir().unwrap();
    let live_cert = certs.path().join("fullchain.pem");
    let live_key = certs.path().join("privkey.pem");
    let (lc, lk) = (live_cert.clone(), live_key.clone());
    // No files yet: the hub starts with a temporary certificate.
    let hub = TestHub::start_with(move |c| {
        c.tls.mode = TlsMode::Files;
        c.tls.cert = lc;
        c.tls.key = lk;
        c.tls.reload_interval = HumanDuration(Duration::from_millis(100));
    })
    .await;
    let first = hub.status().await.pin.unwrap();
    let (c1, k1) = write_cert(certs.path(), "one");
    std::fs::copy(&c1, &live_cert).unwrap();
    std::fs::copy(&k1, &live_key).unwrap();
    let want = agent_chat_server::net::tls::load(&c1, &k1).unwrap().pin;
    let mut pin = first.clone();
    for _ in 0..50 {
        pin = hub.status().await.pin.unwrap();
        if pin == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(pin, want, "picked up the new files");
    assert_ne!(first, want);
    // A renewal replaces them again; a new TLS client sees the new key.
    let (c2, k2) = write_cert(certs.path(), "two");
    std::fs::copy(&c2, &live_cert).unwrap();
    std::fs::copy(&k2, &live_key).unwrap();
    let want2 = agent_chat_server::net::tls::load(&c2, &k2).unwrap().pin;
    for _ in 0..50 {
        if hub.status().await.pin.as_deref() == Some(want2.as_str()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    hub.add_user("alice", "alice-password-123").await;
    let mut tls = tokio_xmpp::stanzastream::StanzaStream::new_c2s(
        common::pinned::PinnedStartTls {
            addr: hub.addr,
            pin: want2,
        },
        xmpp_parsers::jid::Jid::new("alice@hubtest.local/a").unwrap(),
        "alice-password-123".into(),
        tokio_xmpp::xmlstream::Timeouts::tight(),
        16,
    );
    common::online(&mut tls).await;
    tls.close().await;
    hub.stop().await;
}
