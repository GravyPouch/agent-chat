//! M1 acceptance tests 1–4 (docs/design/embedded-server.md §9) and the
//! routing rules of §5.5, against an in-process hub.

mod common;

use std::collections::BTreeSet;

use common::{DOMAIN, Raw, TestHub, available, between, next_event, online};
use sasl::common::Credentials;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_xmpp::Stanza;
use tokio_xmpp::connect::ServerConnector;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::stanzastream::{Event, StanzaStream};
use tokio_xmpp::xmlstream::Timeouts;
use xmpp_parsers::iq::Iq;
use xmpp_parsers::jid::Jid;
use xmpp_parsers::message::{Message, MessageType};
use xmpp_parsers::stanza_error::DefinedCondition;

const ALICE_PW: &str = "alice-password-123";
const BOB_PW: &str = "bob-password-4567";

fn jid(s: &str) -> Jid {
    Jid::new(s).unwrap()
}

async fn hub_with_users() -> TestHub {
    let hub = TestHub::start().await;
    hub.add_user("alice", ALICE_PW).await;
    hub.add_user("bob", BOB_PW).await;
    hub
}

async fn next_message(s: &mut StanzaStream) -> Message {
    next_event(s, |ev| match ev {
        Event::Stanza(Stanza::Message(m)) => Some(m),
        _ => None,
    })
    .await
}

async fn next_iq(s: &mut StanzaStream) -> Iq {
    next_event(s, |ev| match ev {
        Event::Stanza(Stanza::Iq(iq)) => Some(iq),
        _ => None,
    })
    .await
}

fn chat(to: &str, body: &str) -> Box<Stanza> {
    Box::new(Message::chat(jid(to)).with_body(Default::default(), body.into()).into())
}

fn iq_get(to: &str, id: &str, payload: &str) -> Box<Stanza> {
    let payload: Element = payload.parse().unwrap();
    Box::new(
        Iq::Get {
            from: None,
            to: Some(jid(to)),
            id: id.into(),
            payload,
        }
        .into(),
    )
}

// ------------------------------------------------- acceptance test 1

#[tokio::test]
async fn at1_scram_sha256_plus_over_tls13() {
    let hub = hub_with_users().await;
    let mut c = hub.tls_client("alice@hubtest.local/laptop", ALICE_PW);
    assert_eq!(online(&mut c).await, jid("alice@hubtest.local/laptop"));
    let st = hub.status().await;
    assert_eq!(
        TestHub::auth_count(&st, "SCRAM-SHA-256-PLUS", "success"),
        1,
        "{:?}",
        st.auth
    );
    assert_eq!(st.sessions, 1);
    c.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn at1_scram_sha256_over_loopback_plaintext() {
    let hub = hub_with_users().await;
    let mut c = hub.plaintext_client("alice@hubtest.local/cli", ALICE_PW);
    assert_eq!(online(&mut c).await, jid("alice@hubtest.local/cli"));
    let st = hub.status().await;
    assert_eq!(TestHub::auth_count(&st, "SCRAM-SHA-256", "success"), 1, "{:?}", st.auth);
    c.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn at1_plain_is_refused_before_tls() {
    let hub = hub_with_users().await;
    let (mut raw, features) = Raw::start(hub.addr).await;
    assert!(features.contains("starttls"), "{features}");
    assert!(features.contains("SCRAM-SHA-256"), "{features}");
    assert!(!features.contains("PLAIN"), "PLAIN offered before TLS: {features}");
    assert!(!features.contains("-PLUS"), "-PLUS without channel binding: {features}");
    // "\0alice\0alice-password-123"
    raw.send(
        "<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='PLAIN'>AGFsaWNlAGFsaWNlLXBhc3N3b3JkLTEyMw==</auth>",
    )
    .await;
    let reply = raw.read_until("</failure>").await;
    assert!(reply.contains("encryption-required"), "{reply}");
    hub.stop().await;
}

#[tokio::test]
async fn plain_and_plus_offers_under_tls() {
    let hub = hub_with_users().await;
    let connector = common::pinned::PinnedStartTls {
        addr: hub.addr,
        pin: hub.pin.clone(),
    };
    let me = jid("alice@hubtest.local");
    for (password, ok) in [("wrong-password-1", false), (ALICE_PW, true)] {
        let (pending, cb) = connector
            .connect(&me, "jabber:client", Timeouts::tight())
            .await
            .unwrap();
        let (features, stream) = pending.recv_features().await.unwrap();
        for m in [
            "SCRAM-SHA-256-PLUS",
            "SCRAM-SHA-1-PLUS",
            "SCRAM-SHA-256",
            "SCRAM-SHA-1",
            "PLAIN",
        ] {
            assert!(
                features.sasl_mechanisms.contains(m),
                "{m} missing: {:?}",
                features.sasl_mechanisms
            );
        }
        assert!(
            features.sasl_cb.is_some(),
            "XEP-0440 channel-binding types not advertised"
        );
        let creds = Credentials::default()
            .with_username("alice")
            .with_password(password)
            .with_channel_binding(cb);
        let only_plain: BTreeSet<String> = ["PLAIN".to_owned()].into();
        assert_eq!(tokio_xmpp::client_login(stream, only_plain, creds).await.is_ok(), ok);
    }
    let st = hub.status().await;
    assert_eq!(TestHub::auth_count(&st, "PLAIN", "success"), 1, "{:?}", st.auth);
    assert_eq!(TestHub::auth_count(&st, "PLAIN", "not-authorized"), 1, "{:?}", st.auth);
    hub.stop().await;
}

#[tokio::test]
async fn wrong_password_over_plus_fails() {
    let hub = hub_with_users().await;
    let connector = common::pinned::PinnedStartTls {
        addr: hub.addr,
        pin: hub.pin.clone(),
    };
    let me = jid("alice@hubtest.local");
    let (pending, cb) = connector
        .connect(&me, "jabber:client", Timeouts::tight())
        .await
        .unwrap();
    let (features, stream) = pending.recv_features().await.unwrap();
    let creds = Credentials::default()
        .with_username("alice")
        .with_password("not-the-password")
        .with_channel_binding(cb);
    assert!(
        tokio_xmpp::client_login(stream, features.sasl_mechanisms, creds)
            .await
            .is_err()
    );
    let st = hub.status().await;
    assert_eq!(
        TestHub::auth_count(&st, "SCRAM-SHA-256-PLUS", "not-authorized"),
        1,
        "{:?}",
        st.auth
    );
    hub.stop().await;
}

#[tokio::test]
async fn a_wrong_pin_fails_before_any_sasl() {
    let hub = hub_with_users().await;
    let connector = common::pinned::PinnedStartTls {
        addr: hub.addr,
        pin: "sha256/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
    };
    assert!(
        connector
            .connect(&jid("alice@hubtest.local"), "jabber:client", Timeouts::tight())
            .await
            .is_err()
    );
    assert!(hub.status().await.auth.is_empty());
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 2

/// The server's side of a failed SCRAM login with nonces and salt masked.
async fn failed_login_transcript(hub: &TestHub, user: &str, password: &str) -> (String, Vec<u8>) {
    let mut raw = Raw::connect(hub.addr).await;
    raw.open(DOMAIN).await;
    let head = raw.read_until("</stream:features>").await;
    let id = between(&head, "id=", " ");
    let head = head.replace(id, "ID");
    let (server_first, reply) = raw.scram(user, password).await;
    let parts: Vec<&str> = server_first.split(',').collect();
    assert_eq!(parts.len(), 3, "{server_first}");
    assert!(parts[0].starts_with("r=") && parts[1].starts_with("s=") && parts[2].starts_with("i="));
    let salt = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &parts[1][2..]).unwrap();
    (format!("{head}|r=*,s=*,{}|{reply}", parts[2]), salt)
}

#[tokio::test]
async fn at2_wrong_password_and_unknown_user_look_the_same() {
    let hub = hub_with_users().await;
    let (wrong_pw, real_salt) = failed_login_transcript(&hub, "alice", "wrong-password-0").await;
    let (unknown, fake_salt) = failed_login_transcript(&hub, "mallory", "wrong-password-0").await;
    assert_eq!(wrong_pw, unknown);
    assert!(wrong_pw.contains("not-authorized"), "{wrong_pw}");
    assert_eq!(real_salt.len(), fake_salt.len());
    // The fake salt is stable, like a real account's.
    let (_, again) = failed_login_transcript(&hub, "mallory", "another-password").await;
    assert_eq!(again, fake_salt);
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 3

fn message_of_len(len: usize) -> String {
    let head = "<message to='bob@hubtest.local'><body>";
    let tail = "</body></message>";
    format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
}

#[tokio::test]
async fn at3_unauthenticated_stanza_over_10000_bytes_is_a_policy_violation() {
    let hub = hub_with_users().await;
    // Exactly at the limit: read in full, then refused as unauthenticated.
    let (mut raw, _) = Raw::start(hub.addr).await;
    raw.send(&message_of_len(10_000)).await;
    let out = raw.read_to_end().await;
    assert!(
        out.contains("not-authorized") && !out.contains("policy-violation"),
        "{out}"
    );
    // One byte more: policy-violation.
    let (mut raw, _) = Raw::start(hub.addr).await;
    raw.send(&message_of_len(10_001)).await;
    let out = raw.read_to_end().await;
    assert!(out.contains("policy-violation"), "{out}");
    hub.stop().await;
}

#[tokio::test]
async fn at3_300_kib_stanza_after_auth_is_a_policy_violation() {
    let hub = hub_with_users().await;
    let (raw, _) = Raw::login(hub.addr, "alice", ALICE_PW).await;
    let (mut rd, mut wr) = raw.stream.into_split();
    let big = message_of_len(300 * 1024);
    // Write in the background: the hub stops reading at its limit.
    let writer = tokio::spawn(async move {
        let _ = wr.write_all(big.as_bytes()).await;
        wr
    });
    let mut out = Vec::new();
    let _ = tokio::time::timeout(common::TIMEOUT, async {
        let mut chunk = [0u8; 4096];
        loop {
            match rd.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await;
    let out = String::from_utf8_lossy(&out);
    assert!(out.contains("policy-violation"), "{out}");
    let _ = writer.await;
    let st = hub.status().await;
    assert!(st.limit_hits >= 1);
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 4

#[tokio::test]
async fn at4_second_bind_with_same_resource_replaces_the_first() {
    let hub = hub_with_users().await;
    let (mut first, features) = Raw::login(hub.addr, "alice", ALICE_PW).await;
    assert!(features.contains("urn:ietf:params:xml:ns:xmpp-bind"), "{features}");
    assert!(
        !features.contains("mechanisms"),
        "empty SASL mechanisms after auth: {features}"
    );
    let r = first.bind("desk").await;
    assert!(r.contains("alice@hubtest.local/desk"), "{r}");
    let (mut second, _) = Raw::login(hub.addr, "alice", ALICE_PW).await;
    let r = second.bind("desk").await;
    assert!(r.contains("alice@hubtest.local/desk"), "{r}");
    let out = first.read_to_end().await;
    assert!(out.contains("conflict"), "{out}");
    // The new session works.
    second
        .send("<iq type='get' id='p1' to='hubtest.local'><ping xmlns='urn:xmpp:ping'/></iq>")
        .await;
    let reply = second.read_until("p1").await + &second.read_until(">").await;
    assert!(reply.contains("result"), "{reply}");
    hub.stop().await;
}

// ------------------------------------------------------- routing (§5.5)

#[tokio::test]
async fn messages_and_iqs_route_between_accounts() {
    let hub = hub_with_users().await;
    let mut a = hub.plaintext_client("alice@hubtest.local/a1", ALICE_PW);
    let mut b = hub.tls_client("bob@hubtest.local/b1", BOB_PW);
    online(&mut a).await;
    online(&mut b).await;
    assert_eq!(hub.status().await.sessions, 2);
    // Bare-JID messages go to available resources (RFC 6121 §8.5.2).
    available(&mut a, 0).await;
    available(&mut b, 0).await;

    // To the bare JID; `from` is stamped with the sender's full JID.
    a.send(chat("bob@hubtest.local", "hi bob")).await;
    let m = next_message(&mut b).await;
    assert_eq!(m.from, Some(jid("alice@hubtest.local/a1")));
    assert_eq!(m.bodies.values().next().map(String::as_str), Some("hi bob"));

    // To a full JID.
    b.send(chat("alice@hubtest.local/a1", "hi alice")).await;
    assert_eq!(
        next_message(&mut a).await.bodies.values().next().map(String::as_str),
        Some("hi alice")
    );

    // To an offline resource: treated as the bare JID.
    b.send(chat("alice@hubtest.local/gone", "anyone?")).await;
    let m = next_message(&mut a).await;
    assert_eq!(m.bodies.values().next().map(String::as_str), Some("anyone?"));

    // IQs to an online full JID are delivered; the answer comes back.
    a.send(iq_get(
        "bob@hubtest.local/b1",
        "v1",
        "<query xmlns='jabber:iq:version'/>",
    ))
    .await;
    let req = next_iq(&mut b).await;
    let Iq::Get {
        from: Some(from), id, ..
    } = req
    else {
        panic!("{req:?}")
    };
    assert_eq!(from, jid("alice@hubtest.local/a1"));
    b.send(Box::new(
        Iq::Result {
            from: None,
            to: Some(from),
            id,
            payload: None,
        }
        .into(),
    ))
    .await;
    assert!(matches!(next_iq(&mut a).await, Iq::Result { id, .. } if id == "v1"));

    // IQ to an offline full JID: service-unavailable.
    a.send(iq_get(
        "bob@hubtest.local/nope",
        "v2",
        "<query xmlns='jabber:iq:version'/>",
    ))
    .await;
    match next_iq(&mut a).await {
        Iq::Error { id, error, .. } => {
            assert_eq!(id, "v2");
            assert_eq!(error.defined_condition, DefinedCondition::ServiceUnavailable);
        }
        other => panic!("{other:?}"),
    }

    // Unknown account, remote domain, rooms (not yet): errors.
    for (to, cond) in [
        ("nobody@hubtest.local", DefinedCondition::ServiceUnavailable),
        ("carol@example.org", DefinedCondition::RemoteServerNotFound),
        ("room@conference.hubtest.local", DefinedCondition::ServiceUnavailable),
    ] {
        a.send(chat(to, "hello?")).await;
        let m = next_message(&mut a).await;
        assert_eq!(m.type_, MessageType::Error, "{to}");
        assert_eq!(m.from, Some(jid(to)));
        let err = m.payloads.iter().find(|p| p.name() == "error").expect("error payload");
        assert!(
            err.has_child(cond_name(&cond), "urn:ietf:params:xml:ns:xmpp-stanzas"),
            "{to}: {err:?}"
        );
    }

    a.close().await;
    b.close().await;
    hub.stop().await;
}

fn cond_name(c: &DefinedCondition) -> String {
    match c {
        DefinedCondition::ServiceUnavailable => "service-unavailable".into(),
        DefinedCondition::RemoteServerNotFound => "remote-server-not-found".into(),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn the_domain_answers_ping_and_disco() {
    let hub = hub_with_users().await;
    let mut a = hub.plaintext_client("alice@hubtest.local/a1", ALICE_PW);
    online(&mut a).await;
    a.send(iq_get(DOMAIN, "ping", "<ping xmlns='urn:xmpp:ping'/>")).await;
    assert!(matches!(next_iq(&mut a).await, Iq::Result { id, .. } if id == "ping"));
    a.send(iq_get(
        DOMAIN,
        "info",
        "<query xmlns='http://jabber.org/protocol/disco#info'/>",
    ))
    .await;
    match next_iq(&mut a).await {
        Iq::Result {
            id,
            payload: Some(p),
            from,
            ..
        } => {
            assert_eq!(id, "info");
            assert_eq!(from, Some(jid(DOMAIN)));
            let info = xmpp_parsers::disco::DiscoInfoResult::try_from(p).unwrap();
            assert!(info.features.iter().any(|f| f == "urn:xmpp:ping"));
            assert_eq!(info.identities[0].type_, "im");
        }
        other => panic!("{other:?}"),
    }
    // The account answers for itself: an empty roster.
    let roster: Element = "<query xmlns='jabber:iq:roster'/>".parse().unwrap();
    a.send(Box::new(
        Iq::Get {
            from: None,
            to: None,
            id: "r1".into(),
            payload: roster,
        }
        .into(),
    ))
    .await;
    assert!(matches!(next_iq(&mut a).await, Iq::Result { id, .. } if id == "r1"));
    a.close().await;
    hub.stop().await;
}

// ------------------------------------------- negotiation and lifecycle

#[tokio::test]
async fn unknown_host_gets_host_unknown() {
    let hub = hub_with_users().await;
    let mut raw = Raw::connect(hub.addr).await;
    raw.open("example.org").await;
    let out = raw.read_to_end().await;
    assert!(out.contains("host-unknown"), "{out}");
    hub.stop().await;
}

#[tokio::test]
async fn stanzas_before_auth_are_refused() {
    let hub = hub_with_users().await;
    let (mut raw, _) = Raw::start(hub.addr).await;
    raw.send("<message to='bob@hubtest.local'><body>hi</body></message>")
        .await;
    let out = raw.read_to_end().await;
    assert!(out.contains("not-authorized"), "{out}");
    hub.stop().await;
}

#[tokio::test]
async fn data_pipelined_after_starttls_closes_the_connection() {
    let hub = hub_with_users().await;
    let (mut raw, _) = Raw::start(hub.addr).await;
    raw.send("<starttls xmlns='urn:ietf:params:xml:ns:xmpp-tls'/><iq type='get' id='evil'/>")
        .await;
    let out = raw.read_to_end().await;
    assert!(out.contains("proceed"), "{out}");
    hub.stop().await;
}

#[tokio::test]
async fn three_failures_close_the_connection() {
    let hub = hub_with_users().await;
    let (mut raw, _) = Raw::start(hub.addr).await;
    for _ in 0..2 {
        let (_, reply) = raw.scram("alice", "wrong-password-x").await;
        assert!(reply.contains("not-authorized"), "{reply}");
    }
    let (_, reply) = raw.scram("alice", "wrong-password-x").await;
    let out = reply + &raw.read_to_end().await;
    assert!(out.contains("policy-violation"), "{out}");
    hub.stop().await;
}

#[tokio::test]
async fn shutdown_sends_system_shutdown_and_removes_runtime_files() {
    let hub = hub_with_users().await;
    let runtime = hub.paths.runtime();
    let info = agent_chat_server::RuntimeInfo::read(&hub.paths).unwrap().unwrap();
    assert_eq!(info.domain, DOMAIN);
    assert_eq!(info.pin.as_deref(), Some(hub.pin.as_str()));
    assert_eq!(info.c2s, vec![hub.addr]);
    let (mut raw, _) = Raw::login(hub.addr, "alice", ALICE_PW).await;
    raw.bind("desk").await;
    let socket = hub.socket();
    let reader = tokio::spawn(async move { raw.read_to_end().await });
    hub.stop().await;
    let out = reader.await.unwrap();
    assert!(out.contains("system-shutdown"), "{out}");
    assert!(!runtime.exists());
    assert!(!socket.exists());
}

#[tokio::test]
async fn deleting_an_account_closes_its_sessions() {
    let hub = hub_with_users().await;
    let (mut raw, _) = Raw::login(hub.addr, "bob", BOB_PW).await;
    raw.bind("x").await;
    hub.admin(agent_chat_server::admin::AdminRequest::UserDel { username: "bob".into() })
        .await;
    let out = raw.read_to_end().await;
    assert!(out.contains("not-authorized"), "{out}");
    let users: Vec<agent_chat_server::admin::UserInfo> =
        serde_json::from_value(hub.admin(agent_chat_server::admin::AdminRequest::UserList).await).unwrap();
    assert_eq!(
        users.iter().map(|u| u.jid.as_str()).collect::<Vec<_>>(),
        ["alice@hubtest.local"]
    );
    hub.stop().await;
}

#[tokio::test]
async fn sessions_are_counted() {
    let hub = hub_with_users().await;
    let mut a = hub.plaintext_client("alice@hubtest.local/one", ALICE_PW);
    let mut b = hub.plaintext_client("alice@hubtest.local/two", ALICE_PW);
    online(&mut a).await;
    online(&mut b).await;
    available(&mut a, 0).await;
    available(&mut b, 0).await;
    assert_eq!(hub.status().await.sessions, 2);
    a.close().await;
    let mut n = 2;
    for _ in 0..50 {
        n = hub.status().await.sessions;
        if n == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(n, 1);
    // Messages to the bare JID reach the remaining resource.
    let mut c = hub.plaintext_client("bob@hubtest.local/c", BOB_PW);
    online(&mut c).await;
    c.send(chat("alice@hubtest.local", "still there?")).await;
    assert_eq!(
        next_message(&mut b).await.bodies.values().next().map(String::as_str),
        Some("still there?")
    );
    b.close().await;
    c.close().await;
    hub.stop().await;
}
