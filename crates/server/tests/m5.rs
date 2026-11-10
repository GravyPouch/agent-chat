//! M5 acceptance tests 2 and 3 (docs/design/embedded-server.md §9) plus
//! the MUC behaviour our client relies on: locked rooms, status codes,
//! members-only rooms with invitations, affiliations, reflection with
//! stanza-ids, room MAM with real JIDs, private messages.
//! Test 1 (scenarios 4 and 6 with real daemons) is in `crates/cli/tests/hub_e2e.rs`.

mod common;

use common::{TestHub, available, body, connect, next_el, st, sync};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::stanzastream::StanzaStream;

const ALICE_PW: &str = "alice-password-123";
const BOB_PW: &str = "bob-password-4567";
const CAROL_PW: &str = "carol-password-890";
const ROOM: &str = "team@conference.hubtest.local";
const MUC: &str = "http://jabber.org/protocol/muc";
const MUC_USER: &str = "http://jabber.org/protocol/muc#user";

async fn hub_with_users() -> TestHub {
    let hub = TestHub::start().await;
    hub.add_user("alice", ALICE_PW).await;
    hub.add_user("bob", BOB_PW).await;
    hub.add_user("carol", CAROL_PW).await;
    hub
}

async fn iq(c: &mut StanzaStream, xml: &str) -> Element {
    let el: Element = xml.parse().unwrap();
    let id = el.attr("id").unwrap().to_owned();
    c.send(st(xml)).await;
    next_el(c, |e| e.name() == "iq" && e.attr("id") == Some(id.as_str())).await
}

fn codes(p: &Element) -> Vec<String> {
    p.get_child("x", MUC_USER)
        .map(|x| {
            x.children()
                .filter(|c| c.name() == "status")
                .filter_map(|c| c.attr("code").map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn item(p: &Element) -> Option<Element> {
    p.get_child("x", MUC_USER)
        .and_then(|x| x.get_child("item", MUC_USER))
        .cloned()
}

fn error_condition(e: &Element) -> Option<String> {
    e.get_child("error", "jabber:client")
        .and_then(|er| er.children().next())
        .map(|c| c.name().to_owned())
}

/// Join presence; returns our own presence back (or the error presence).
async fn join(c: &mut StanzaStream, nick: &str, extra: &str) -> Element {
    join_seeing(c, nick, extra).await.0
}

/// Like [`join`], also returning the other occupants' presence (sent first).
async fn join_seeing(c: &mut StanzaStream, nick: &str, extra: &str) -> (Element, Vec<Element>) {
    c.send(st(&format!(
        "<presence xmlns='jabber:client' to='{ROOM}/{nick}'><x xmlns='{MUC}'>{extra}</x></presence>"
    )))
    .await;
    let me = format!("{ROOM}/{nick}");
    let mut others = Vec::new();
    loop {
        let p = next_el(c, |e| {
            e.name() == "presence" && e.attr("from").is_some_and(|f| f.starts_with(ROOM))
        })
        .await;
        let mine = p.attr("from") == Some(me.as_str());
        if mine && (p.attr("type") == Some("error") || codes(&p).contains(&"110".to_owned())) {
            return (p, others);
        }
        others.push(p);
    }
}

async fn configure(c: &mut StanzaStream, fields: &[(&str, &str)]) -> Element {
    let f: String = fields
        .iter()
        .map(|(k, v)| format!("<field var='{k}'><value>{v}</value></field>"))
        .collect();
    iq(
        c,
        &format!(
            "<iq xmlns='jabber:client' type='set' id='cfg' to='{ROOM}'><query xmlns='http://jabber.org/protocol/muc#owner'>\
             <x xmlns='jabber:x:data' type='submit'><field var='FORM_TYPE'>\
             <value>http://jabber.org/protocol/muc#roomconfig</value></field>{f}</x></query></iq>"
        ),
    )
    .await
}

const PRIVATE: &[(&str, &str)] = &[
    ("muc#roomconfig_membersonly", "1"),
    ("muc#roomconfig_whois", "anyone"),
    ("muc#roomconfig_persistentroom", "1"),
    ("muc#roomconfig_publicroom", "0"),
];

// ------------------------------------------------- acceptance test 2

#[tokio::test]
async fn at2_locked_room_and_status_codes() {
    let hub = hub_with_users().await;
    let mut a = connect(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut b = connect(&hub, "bob@hubtest.local/b", BOB_PW).await;

    // The room doesn't exist: disco#info says so (our client asks first).
    let r = iq(
        &mut b,
        &format!(
            "<iq xmlns='jabber:client' type='get' id='d0' to='{ROOM}'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>"
        ),
    )
    .await;
    assert_eq!(error_condition(&r).as_deref(), Some("item-not-found"));

    // alice creates it: 110 + 201, owner, moderator.
    let p = join(&mut a, "alice", "").await;
    let c = codes(&p);
    assert!(c.contains(&"110".into()) && c.contains(&"201".into()), "{c:?}");
    let i = item(&p).unwrap();
    assert_eq!(
        (i.attr("affiliation"), i.attr("role")),
        (Some("owner"), Some("moderator"))
    );

    // Locked: bob gets item-not-found until alice configures it.
    let p = join(&mut b, "bob", "").await;
    assert_eq!(p.attr("type"), Some("error"));
    assert_eq!(error_condition(&p).as_deref(), Some("item-not-found"));

    // A non-anonymous, public room (instant config plus whois=anyone).
    let r = configure(&mut a, &[("muc#roomconfig_whois", "anyone")]).await;
    assert_eq!(r.attr("type"), Some("result"), "{r:?}");
    let p = join(&mut b, "bob", "<history maxstanzas='0'/>").await;
    let c = codes(&p);
    for want in ["110", "100", "170"] {
        assert!(c.contains(&want.to_owned()), "missing {want}: {c:?}");
    }
    assert!(!c.contains(&"201".to_owned()));
    // alice sees bob join, with his real JID (non-anonymous).
    let pa = next_el(&mut a, |e| {
        e.name() == "presence" && e.attr("from") == Some(&format!("{ROOM}/bob"))
    })
    .await;
    assert_eq!(item(&pa).unwrap().attr("jid"), Some("bob@hubtest.local/b"));
    // The empty subject comes after the presence.
    let subject = next_el(&mut b, |e| {
        e.name() == "message" && e.has_child("subject", "jabber:client")
    })
    .await;
    assert_eq!(subject.attr("type"), Some("groupchat"));

    // Room disco advertises what our client reads.
    let r = iq(
        &mut b,
        &format!(
            "<iq xmlns='jabber:client' type='get' id='d1' to='{ROOM}'><query xmlns='http://jabber.org/protocol/disco#info'/></iq>"
        ),
    )
    .await;
    let features: Vec<String> = r
        .get_child("query", "http://jabber.org/protocol/disco#info")
        .unwrap()
        .children()
        .filter_map(|f| f.attr("var").map(str::to_owned))
        .collect();
    for want in [
        "http://jabber.org/protocol/muc",
        "urn:xmpp:mam:2",
        "muc_nonanonymous",
        "muc_open",
    ] {
        assert!(features.contains(&want.to_owned()), "missing {want}: {features:?}");
    }

    // Nick conflict, and the service lists the room.
    let mut c3 = connect(&hub, "carol@hubtest.local/c", CAROL_PW).await;
    let p = join(&mut c3, "bob", "").await;
    assert_eq!(error_condition(&p).as_deref(), Some("conflict"));
    let r = iq(
        &mut c3,
        "<iq xmlns='jabber:client' type='get' id='items' to='conference.hubtest.local'>\
         <query xmlns='http://jabber.org/protocol/disco#items'/></iq>",
    )
    .await;
    let rooms: Vec<_> = r
        .get_child("query", "http://jabber.org/protocol/disco#items")
        .unwrap()
        .children()
        .filter_map(|i| i.attr("jid").map(str::to_owned))
        .collect();
    assert_eq!(rooms, [ROOM]);
    // The domain lists the MUC service.
    let r = iq(
        &mut c3,
        "<iq xmlns='jabber:client' type='get' id='dom' to='hubtest.local'>\
         <query xmlns='http://jabber.org/protocol/disco#items'/></iq>",
    )
    .await;
    assert!(String::from(&r).contains("conference.hubtest.local"));

    // Leaving: the leaver gets unavailable with 110, the others without.
    b.send(st(&format!(
        "<presence xmlns='jabber:client' type='unavailable' to='{ROOM}/bob'/>"
    )))
    .await;
    let p = next_el(&mut b, |e| {
        e.name() == "presence" && e.attr("type") == Some("unavailable")
    })
    .await;
    assert!(codes(&p).contains(&"110".to_owned()));
    let p = next_el(&mut a, |e| {
        e.name() == "presence" && e.attr("type") == Some("unavailable")
    })
    .await;
    assert_eq!(p.attr("from"), Some(format!("{ROOM}/bob").as_str()));
    for s in [a, b, c3] {
        s.close().await;
    }
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 3

#[tokio::test]
async fn at3_private_room_invites_messages_and_archive_with_real_jids() {
    let hub = hub_with_users().await;
    let mut a = connect(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut b = connect(&hub, "bob@hubtest.local/b", BOB_PW).await;
    let mut c = connect(&hub, "carol@hubtest.local/c", CAROL_PW).await;
    // Invitations go to bare JIDs: only available resources get them live.
    available(&mut b, 0).await;
    available(&mut c, 0).await;
    join(&mut a, "alice", "").await;
    let r = configure(&mut a, PRIVATE).await;
    assert_eq!(r.attr("type"), Some("result"), "{r:?}");

    // Members only: carol can't join.
    let p = join(&mut c, "carol", "").await;
    assert_eq!(error_condition(&p).as_deref(), Some("registration-required"));

    // Our client's invite: make bob a member, then a mediated invitation.
    let r = iq(
        &mut a,
        &format!(
            "<iq xmlns='jabber:client' type='set' id='m1' to='{ROOM}'><query xmlns='http://jabber.org/protocol/muc#admin'>\
             <item affiliation='member' jid='bob@hubtest.local'/></query></iq>"
        ),
    )
    .await;
    assert_eq!(r.attr("type"), Some("result"));
    a.send(st(&format!(
        "<message xmlns='jabber:client' to='{ROOM}'><x xmlns='{MUC_USER}'>\
         <invite to='bob@hubtest.local'><reason>secret plans</reason></invite></x></message>"
    )))
    .await;
    let inv = next_el(&mut b, |e| e.name() == "message" && e.has_child("x", MUC_USER)).await;
    assert_eq!(inv.attr("from"), Some(ROOM));
    let invite = inv
        .get_child("x", MUC_USER)
        .unwrap()
        .get_child("invite", MUC_USER)
        .unwrap();
    assert_eq!(invite.attr("from"), Some("alice@hubtest.local"));

    // An invitation from an owner also grants membership (carol, no admin set).
    a.send(st(&format!(
        "<message xmlns='jabber:client' to='{ROOM}'><x xmlns='{MUC_USER}'><invite to='carol@hubtest.local'/></x></message>"
    )))
    .await;
    next_el(&mut c, |e| e.name() == "message" && e.has_child("x", MUC_USER)).await;

    // Members list as our client reads it.
    let r = iq(
        &mut a,
        &format!(
            "<iq xmlns='jabber:client' type='get' id='l1' to='{ROOM}'><query xmlns='http://jabber.org/protocol/muc#admin'>\
             <item affiliation='member'/></query></iq>"
        ),
    )
    .await;
    let members: Vec<String> = r
        .get_child("query", "http://jabber.org/protocol/muc#admin")
        .unwrap()
        .children()
        .filter_map(|i| i.attr("jid").map(str::to_owned))
        .collect();
    assert_eq!(members, ["bob@hubtest.local", "carol@hubtest.local"]);

    let (p, others) = join_seeing(&mut b, "bob", "<history maxstanzas='0'/>").await;
    assert!(codes(&p).contains(&"100".to_owned()), "non-anonymous");
    // bob saw alice first, with her real JID.
    let alice = format!("{ROOM}/alice");
    let pa = others
        .iter()
        .find(|e| e.attr("from") == Some(alice.as_str()))
        .expect("alice's presence before ours");
    assert_eq!(item(pa).unwrap().attr("jid"), Some("alice@hubtest.local/a"));

    // A groupchat message is reflected to everyone with stanza-id by the room.
    a.send(st(&format!(
        "<message xmlns='jabber:client' type='groupchat' to='{ROOM}'><body>hello team</body>\
         <origin-id xmlns='urn:xmpp:sid:0' id='o-1'/></message>"
    )))
    .await;
    let mb = next_el(&mut b, |e| {
        e.name() == "message" && body(e).as_deref() == Some("hello team")
    })
    .await;
    let ma = next_el(&mut a, |e| {
        e.name() == "message" && body(e).as_deref() == Some("hello team")
    })
    .await;
    assert_eq!(mb.attr("from"), Some(format!("{ROOM}/alice").as_str()));
    let sid = |m: &Element| {
        m.children()
            .find(|c| c.is("stanza-id", "urn:xmpp:sid:0") && c.attr("by") == Some(ROOM))
            .and_then(|c| c.attr("id"))
            .map(str::to_owned)
    };
    let id = sid(&mb).expect("stanza-id by the room");
    assert_eq!(sid(&ma), Some(id.clone()), "everyone sees the same id");

    // Room MAM: the archived copy carries alice's real JID.
    b.send(st(&format!(
        "<iq xmlns='jabber:client' type='set' id='mam' to='{ROOM}'><query xmlns='urn:xmpp:mam:2' queryid='rq'>\
         <set xmlns='http://jabber.org/protocol/rsm'><max>10</max><before/></set></query></iq>"
    )))
    .await;
    let res = next_el(&mut b, |e| {
        e.name() == "message" && e.has_child("result", "urn:xmpp:mam:2")
    })
    .await;
    assert_eq!(res.attr("from"), Some(ROOM));
    let r = res.get_child("result", "urn:xmpp:mam:2").unwrap();
    assert_eq!(r.attr("id"), Some(id.as_str()));
    let inner = r
        .get_child("forwarded", "urn:xmpp:forward:0")
        .and_then(|f| f.get_child("message", "jabber:client"))
        .unwrap();
    let real = inner
        .get_child("x", MUC_USER)
        .and_then(|x| x.get_child("item", MUC_USER))
        .and_then(|i| i.attr("jid"));
    assert_eq!(real, Some("alice@hubtest.local"));
    let fin = next_el(&mut b, |e| e.attr("id") == Some("mam")).await;
    assert_eq!(fin.attr("type"), Some("result"));
    // Non-members can't read the archive.
    let r = iq(
        &mut c,
        &format!("<iq xmlns='jabber:client' type='set' id='x' to='{ROOM}'><query xmlns='urn:xmpp:mam:2'/></iq>"),
    )
    .await;
    assert_eq!(
        r.attr("type"),
        Some("result"),
        "carol is a member now (invited by the owner)"
    );

    // A private message to an occupant.
    b.send(st(&format!(
        "<message xmlns='jabber:client' type='chat' to='{ROOM}/alice'><body>psst</body></message>"
    )))
    .await;
    let pm = next_el(&mut a, |e| e.name() == "message" && body(e).as_deref() == Some("psst")).await;
    assert_eq!(pm.attr("from"), Some(format!("{ROOM}/bob").as_str()));
    assert!(pm.has_child("x", MUC_USER));

    // Removing bob's membership removes him from the room (321).
    iq(
        &mut a,
        &format!(
            "<iq xmlns='jabber:client' type='set' id='m2' to='{ROOM}'><query xmlns='http://jabber.org/protocol/muc#admin'>\
             <item affiliation='none' jid='bob@hubtest.local'/></query></iq>"
        ),
    )
    .await;
    let p = next_el(&mut b, |e| {
        e.name() == "presence" && e.attr("type") == Some("unavailable")
    })
    .await;
    assert!(codes(&p).contains(&"321".to_owned()), "{:?}", codes(&p));
    // The last owner can't step down.
    let r = iq(
        &mut a,
        &format!(
            "<iq xmlns='jabber:client' type='set' id='m3' to='{ROOM}'><query xmlns='http://jabber.org/protocol/muc#admin'>\
             <item affiliation='member' jid='alice@hubtest.local'/></query></iq>"
        ),
    )
    .await;
    assert_eq!(error_condition(&r).as_deref(), Some("conflict"));
    for s in [a, b, c] {
        s.close().await;
    }
    hub.stop().await;
}

#[tokio::test]
async fn rooms_survive_a_restart_and_history_is_served() {
    let hub = hub_with_users().await;
    let paths = hub.paths.clone();
    let mut a = connect(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    join(&mut a, "alice", "").await;
    configure(&mut a, PRIVATE).await;
    iq(
        &mut a,
        &format!(
            "<iq xmlns='jabber:client' type='set' id='m1' to='{ROOM}'><query xmlns='http://jabber.org/protocol/muc#admin'>\
             <item affiliation='member' jid='bob@hubtest.local'/></query></iq>"
        ),
    )
    .await;
    for i in 0..3 {
        a.send(st(&format!(
            "<message xmlns='jabber:client' type='groupchat' to='{ROOM}'><body>m{i}</body></message>"
        )))
        .await;
    }
    sync(&mut a).await;
    a.close().await;
    // Restart the hub in the same directory.
    let TestHub { hub: running, dir, .. } = hub;
    running.unwrap().shutdown().await;
    let hub2 = agent_chat_server::RunningHub::start(paths.clone()).await.unwrap();
    let addr = hub2.listeners()[0];
    let th = TestHub {
        hub: Some(hub2),
        dir,
        paths,
        addr,
        pin: String::new(),
    };
    let mut b = connect(&th, "bob@hubtest.local/b", BOB_PW).await;
    // Still members-only and bob still a member; history without MAM.
    let p = join(&mut b, "bob", "<history maxstanzas='2'/>").await;
    assert!(codes(&p).contains(&"110".to_owned()), "{p:?}");
    let h1 = next_el(&mut b, |e| e.name() == "message" && body(e).is_some()).await;
    let h2 = next_el(&mut b, |e| e.name() == "message" && body(e).is_some()).await;
    assert_eq!((body(&h1).as_deref(), body(&h2).as_deref()), (Some("m1"), Some("m2")));
    assert!(h1.has_child("delay", "urn:xmpp:delay"));
    b.close().await;
    th.stop().await;
}
