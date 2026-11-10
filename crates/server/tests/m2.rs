//! M2 acceptance tests (docs/design/embedded-server.md §9): roster and
//! subscriptions, presence (broadcast, probes, directed), priorities and
//! carbons, offline storage, blocking and caps. Test 1 (the RFC 6121
//! Appendix A tables) is in `src/user/roster.rs`.

mod common;

use common::{TestHub, available, body, next_el, online, st};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::stanzastream::StanzaStream;

const ALICE_PW: &str = "alice-password-123";
const BOB_PW: &str = "bob-password-4567";
const CAROL_PW: &str = "carol-password-890";

async fn hub_with_users() -> TestHub {
    let hub = TestHub::start().await;
    hub.add_user("alice", ALICE_PW).await;
    hub.add_user("bob", BOB_PW).await;
    hub.add_user("carol", CAROL_PW).await;
    hub
}

fn is(el: &Element, name: &str, ty: &str) -> bool {
    el.name() == name && el.attr("type").unwrap_or(if name == "message" { "normal" } else { "" }) == ty
}

async fn client(hub: &TestHub, jid: &str, pw: &str) -> StanzaStream {
    let mut c = hub.plaintext_client(jid, pw);
    online(&mut c).await;
    c
}

/// Fetch the roster (marks the resource interested in pushes).
async fn roster(c: &mut StanzaStream, id: &str) -> Element {
    c.send(st(&format!(
        "<iq xmlns='jabber:client' type='get' id='{id}'><query xmlns='jabber:iq:roster'/></iq>"
    )))
    .await;
    let r = next_el(c, |e| e.name() == "iq" && e.attr("id") == Some(id)).await;
    assert_eq!(r.attr("type"), Some("result"), "{r:?}");
    r.get_child("query", "jabber:iq:roster").unwrap().clone()
}

fn item<'a>(q: &'a Element, jid: &str) -> Option<&'a Element> {
    q.children().find(|i| i.attr("jid") == Some(jid))
}

/// alice and bob subscribe to each other (both).
async fn befriend(a: &mut StanzaStream, b: &mut StanzaStream) {
    a.send(st(
        "<presence xmlns='jabber:client' type='subscribe' to='bob@hubtest.local'/>",
    ))
    .await;
    let req = next_el(b, |e| is(e, "presence", "subscribe")).await;
    assert_eq!(
        req.attr("from"),
        Some("alice@hubtest.local"),
        "bare JID on subscriptions"
    );
    b.send(st(
        "<presence xmlns='jabber:client' type='subscribed' to='alice@hubtest.local'/>",
    ))
    .await;
    next_el(a, |e| is(e, "presence", "subscribed")).await;
    b.send(st(
        "<presence xmlns='jabber:client' type='subscribe' to='alice@hubtest.local'/>",
    ))
    .await;
    next_el(a, |e| is(e, "presence", "subscribe")).await;
    a.send(st(
        "<presence xmlns='jabber:client' type='subscribed' to='bob@hubtest.local'/>",
    ))
    .await;
    next_el(b, |e| is(e, "presence", "subscribed")).await;
}

// ------------------------------------------------- acceptance test 2

#[tokio::test]
async fn at2_subscription_round_trip_with_pushes_and_presence() {
    let hub = hub_with_users().await;
    let mut a = client(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut b = client(&hub, "bob@hubtest.local/b", BOB_PW).await;
    roster(&mut a, "r0").await;
    roster(&mut b, "r0").await;
    available(&mut a, 0).await;
    available(&mut b, 0).await;

    // alice asks; her roster gets bob with ask='subscribe' (a push).
    a.send(st(
        "<presence xmlns='jabber:client' type='subscribe' to='bob@hubtest.local'/>",
    ))
    .await;
    let push = next_el(&mut a, |e| e.name() == "iq" && e.attr("type") == Some("set")).await;
    let q = push.get_child("query", "jabber:iq:roster").unwrap();
    let i = item(q, "bob@hubtest.local").unwrap();
    assert_eq!(
        (i.attr("subscription"), i.attr("ask")),
        (Some("none"), Some("subscribe"))
    );
    // bob sees the request; bob's roster doesn't list alice yet.
    next_el(&mut b, |e| is(e, "presence", "subscribe")).await;
    assert!(item(&roster(&mut b, "r1").await, "alice@hubtest.local").is_none());

    // bob approves: alice gets `to`, bob gets `from`, and alice receives
    // bob's current presence.
    b.send(st(
        "<presence xmlns='jabber:client' type='subscribed' to='alice@hubtest.local'/>",
    ))
    .await;
    let push = next_el(&mut b, |e| e.name() == "iq" && e.attr("type") == Some("set")).await;
    let i = item(
        push.get_child("query", "jabber:iq:roster").unwrap(),
        "alice@hubtest.local",
    )
    .unwrap()
    .clone();
    assert_eq!(i.attr("subscription"), Some("from"));
    next_el(&mut a, |e| is(e, "presence", "subscribed")).await;
    let p = next_el(&mut a, |e| {
        e.name() == "presence" && e.attr("from") == Some("bob@hubtest.local/b")
    })
    .await;
    assert!(p.attr("type").is_none(), "{p:?}");
    let q = roster(&mut a, "r2").await;
    assert_eq!(item(&q, "bob@hubtest.local").unwrap().attr("subscription"), Some("to"));

    // bob's presence changes reach alice; alice's don't reach bob (one-way).
    b.send(st(
        "<presence xmlns='jabber:client'><show>away</show><status>lunch</status></presence>",
    ))
    .await;
    let p = next_el(&mut a, |e| {
        e.name() == "presence" && e.attr("from") == Some("bob@hubtest.local/b")
    })
    .await;
    assert_eq!(
        p.get_child("show", "jabber:client").map(|s| s.text()).as_deref(),
        Some("away")
    );

    // alice unsubscribes: bob's item goes back to none.
    a.send(st(
        "<presence xmlns='jabber:client' type='unsubscribe' to='bob@hubtest.local'/>",
    ))
    .await;
    next_el(&mut b, |e| is(e, "presence", "unsubscribe")).await;
    let q = roster(&mut b, "r3").await;
    assert_eq!(
        item(&q, "alice@hubtest.local").unwrap().attr("subscription"),
        Some("none")
    );

    // Roster removal sends unsubscribe/unsubscribed for us.
    let q = roster(&mut a, "r4").await;
    assert!(item(&q, "bob@hubtest.local").is_some());
    a.send(st(
        "<iq xmlns='jabber:client' type='set' id='rm'><query xmlns='jabber:iq:roster'>\
        <item jid='bob@hubtest.local' subscription='remove'/></query></iq>",
    ))
    .await;
    next_el(&mut a, |e| e.attr("id") == Some("rm")).await;
    assert!(item(&roster(&mut a, "r5").await, "bob@hubtest.local").is_none());
    a.close().await;
    b.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn at2_probe_is_answered_by_a_local_contact() {
    let hub = hub_with_users().await;
    let mut a = client(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut b = client(&hub, "bob@hubtest.local/b", BOB_PW).await;
    available(&mut a, 0).await;
    available(&mut b, 0).await;
    befriend(&mut a, &mut b).await;
    b.send(st("<presence xmlns='jabber:client'><status>here</status></presence>"))
        .await;
    a.close().await;
    // A new alice resource: its initial presence probes bob, and bob's
    // server answers with bob's current presence.
    let mut a2 = client(&hub, "alice@hubtest.local/a2", ALICE_PW).await;
    a2.send(st("<presence xmlns='jabber:client'/>")).await;
    let p = next_el(&mut a2, |e| {
        e.name() == "presence" && e.attr("from") == Some("bob@hubtest.local/b")
    })
    .await;
    assert_eq!(p.attr("to"), Some("alice@hubtest.local/a2"));
    assert_eq!(
        p.get_child("status", "jabber:client").map(|s| s.text()).as_deref(),
        Some("here")
    );
    // bob gets alice's initial presence (they are subscribed to her).
    next_el(&mut b, |e| {
        e.name() == "presence" && e.attr("from") == Some("alice@hubtest.local/a2")
    })
    .await;
    // When alice goes away, bob sees unavailable.
    a2.close().await;
    let p = next_el(&mut b, |e| is(e, "presence", "unavailable")).await;
    assert_eq!(p.attr("from"), Some("alice@hubtest.local/a2"));
    b.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn at2_directed_presence_then_unavailable() {
    let hub = hub_with_users().await;
    let mut a = client(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut c = client(&hub, "carol@hubtest.local/c", CAROL_PW).await;
    available(&mut a, 0).await;
    available(&mut c, 0).await;
    // Not contacts: carol sees alice only through directed presence.
    a.send(st(
        "<presence xmlns='jabber:client' to='carol@hubtest.local/c'><status>psst</status></presence>",
    ))
    .await;
    let p = next_el(&mut c, |e| {
        e.name() == "presence" && e.attr("from") == Some("alice@hubtest.local/a")
    })
    .await;
    assert!(p.attr("type").is_none());
    // Disconnecting sends unavailable to everyone who got directed presence.
    a.close().await;
    let p = next_el(&mut c, |e| {
        e.name() == "presence" && e.attr("from") == Some("alice@hubtest.local/a")
    })
    .await;
    assert_eq!(p.attr("type"), Some("unavailable"));
    c.close().await;
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 3

#[tokio::test]
async fn at3_bare_jid_message_goes_to_highest_priority_and_carbons_to_the_other() {
    let hub = hub_with_users().await;
    let mut high = client(&hub, "alice@hubtest.local/high", ALICE_PW).await;
    let mut low = client(&hub, "alice@hubtest.local/low", ALICE_PW).await;
    let mut b = client(&hub, "bob@hubtest.local/b", BOB_PW).await;
    available(&mut high, 5).await;
    available(&mut low, 0).await;
    available(&mut b, 0).await;
    for (c, id) in [(&mut high, "c1"), (&mut low, "c2")] {
        c.send(st(&format!(
            "<iq xmlns='jabber:client' type='set' id='{id}'><enable xmlns='urn:xmpp:carbons:2'/></iq>"
        )))
        .await;
        let r = next_el(c, |e| e.attr("id") == Some(id)).await;
        assert_eq!(r.attr("type"), Some("result"));
    }
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'><body>hi</body></message>",
    ))
    .await;
    let m = next_el(&mut high, |e| e.name() == "message").await;
    assert_eq!(body(&m).as_deref(), Some("hi"));
    assert!(m.get_child("received", "urn:xmpp:carbons:2").is_none());
    let carbon = next_el(&mut low, |e| e.name() == "message").await;
    assert_eq!(
        carbon.attr("from"),
        Some("alice@hubtest.local"),
        "carbons come from the bare JID"
    );
    let inner = carbon
        .get_child("received", "urn:xmpp:carbons:2")
        .and_then(|r| r.get_child("forwarded", "urn:xmpp:forward:0"))
        .and_then(|f| f.get_child("message", "jabber:client"))
        .expect("a received carbon");
    assert_eq!(body(inner).as_deref(), Some("hi"));
    assert_eq!(inner.attr("from"), Some("bob@hubtest.local/b"));

    // A message sent by `low` shows up as a sent carbon on `high`.
    low.send(st(
        "<message xmlns='jabber:client' type='chat' to='bob@hubtest.local'><body>from low</body></message>",
    ))
    .await;
    let sent = next_el(&mut high, |e| e.name() == "message").await;
    let inner = sent
        .get_child("sent", "urn:xmpp:carbons:2")
        .and_then(|r| r.get_child("forwarded", "urn:xmpp:forward:0"))
        .and_then(|f| f.get_child("message", "jabber:client"))
        .expect("a sent carbon");
    assert_eq!(body(inner).as_deref(), Some("from low"));
    // A private message is not copied.
    low.send(st(
        "<message xmlns='jabber:client' type='chat' to='bob@hubtest.local'><body>secret</body>\
        <private xmlns='urn:xmpp:carbons:2'/></message>",
    ))
    .await;
    let m = next_el(&mut b, |e| body(e).as_deref() == Some("secret")).await;
    assert!(
        m.get_child("private", "urn:xmpp:carbons:2").is_none(),
        "stripped before routing"
    );
    high.send(st(
        "<message xmlns='jabber:client' type='chat' to='bob@hubtest.local'><body>marker</body></message>",
    ))
    .await;
    let next = next_el(&mut low, |e| e.name() == "message").await;
    let inner = next
        .get_child("sent", "urn:xmpp:carbons:2")
        .and_then(|r| r.get_child("forwarded", "urn:xmpp:forward:0"))
        .and_then(|f| f.get_child("message", "jabber:client"))
        .unwrap();
    assert_eq!(
        body(inner).as_deref(),
        Some("marker"),
        "no carbon of the private message"
    );
    for c in [high, low, b] {
        c.close().await;
    }
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 4

#[tokio::test]
async fn at4_offline_message_is_delivered_on_initial_presence_with_delay() {
    let hub = hub_with_users().await;
    let mut b = client(&hub, "bob@hubtest.local/b", BOB_PW).await;
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'><body>while you were out</body></message>",
    ))
    .await;
    // Chat states are not stored.
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'>\
        <composing xmlns='http://jabber.org/protocol/chatstates'/><no-store xmlns='urn:xmpp:hints'/></message>",
    ))
    .await;
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'><body>second</body></message>",
    ))
    .await;
    // Make sure bob's messages were processed before alice logs in.
    b.send(st(
        "<iq xmlns='jabber:client' type='get' id='sync'><ping xmlns='urn:xmpp:ping'/></iq>",
    ))
    .await;
    next_el(&mut b, |e| e.attr("id") == Some("sync")).await;
    let mut a = client(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    // Bound but not available: nothing yet.
    a.send(st("<presence xmlns='jabber:client'/>")).await;
    let first = next_el(&mut a, |e| e.name() == "message").await;
    assert_eq!(body(&first).as_deref(), Some("while you were out"));
    let delay = first.get_child("delay", "urn:xmpp:delay").expect("delay");
    assert_eq!(delay.attr("from"), Some("hubtest.local"));
    assert!(delay.attr("stamp").is_some());
    let second = next_el(&mut a, |e| e.name() == "message").await;
    assert_eq!(body(&second).as_deref(), Some("second"));
    a.close().await;
    b.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn subscription_requests_wait_for_an_offline_contact() {
    let hub = hub_with_users().await;
    let mut a = client(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    a.send(st(
        "<presence xmlns='jabber:client' type='subscribe' to='carol@hubtest.local'/>",
    ))
    .await;
    a.send(st(
        "<iq xmlns='jabber:client' type='get' id='sync'><ping xmlns='urn:xmpp:ping'/></iq>",
    ))
    .await;
    next_el(&mut a, |e| e.attr("id") == Some("sync")).await;
    let mut c = client(&hub, "carol@hubtest.local/c", CAROL_PW).await;
    c.send(st("<presence xmlns='jabber:client'/>")).await;
    let p = next_el(&mut c, |e| is(e, "presence", "subscribe")).await;
    assert_eq!(p.attr("from"), Some("alice@hubtest.local"));
    a.close().await;
    c.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn blocking_drops_stanzas_and_errors_outbound() {
    let hub = hub_with_users().await;
    let mut a = client(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut b = client(&hub, "bob@hubtest.local/b", BOB_PW).await;
    available(&mut a, 0).await;
    available(&mut b, 0).await;
    a.send(st(
        "<iq xmlns='jabber:client' type='set' id='bl'><block xmlns='urn:xmpp:blocking'>\
        <item jid='bob@hubtest.local'/></block></iq>",
    ))
    .await;
    next_el(&mut a, |e| e.attr("id") == Some("bl")).await;
    a.send(st(
        "<iq xmlns='jabber:client' type='get' id='bg'><blocklist xmlns='urn:xmpp:blocking'/></iq>",
    ))
    .await;
    let list = next_el(&mut a, |e| e.attr("id") == Some("bg")).await;
    let items: Vec<_> = list
        .get_child("blocklist", "urn:xmpp:blocking")
        .unwrap()
        .children()
        .filter_map(|i| i.attr("jid").map(str::to_owned))
        .collect();
    assert_eq!(items, ["bob@hubtest.local"]);
    // bob's message is dropped; alice's to bob errors.
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'><body>blocked?</body></message>",
    ))
    .await;
    // Routed to alice's actor before bob's ping is answered.
    common::sync(&mut b).await;
    a.send(st(
        "<message xmlns='jabber:client' type='chat' to='bob@hubtest.local'><body>hi</body></message>",
    ))
    .await;
    let e = next_el(&mut a, |e| e.name() == "message").await;
    assert_eq!(e.attr("type"), Some("error"));
    assert!(
        e.get_child("error", "jabber:client")
            .unwrap()
            .has_child("blocked", "urn:xmpp:blocking:errors")
    );
    // Unblock and the next message arrives (not the dropped one).
    a.send(st(
        "<iq xmlns='jabber:client' type='set' id='ub'><unblock xmlns='urn:xmpp:blocking'/></iq>",
    ))
    .await;
    next_el(&mut a, |e| e.attr("id") == Some("ub")).await;
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'><body>again</body></message>",
    ))
    .await;
    let m = next_el(&mut a, |e| e.name() == "message").await;
    assert_eq!(body(&m).as_deref(), Some("again"));
    a.close().await;
    b.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn unknown_caps_are_queried_from_the_bare_jid() {
    let hub = hub_with_users().await;
    let mut a = client(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    a.send(st(
        "<presence xmlns='jabber:client'><c xmlns='http://jabber.org/protocol/caps' \
        hash='sha-1' node='http://example.org/client' ver='QgayPKawpkPSDYmwT/WM94uAlu0='/></presence>",
    ))
    .await;
    let q = next_el(&mut a, |e| e.name() == "iq" && e.attr("type") == Some("get")).await;
    assert_eq!(q.attr("from"), Some("alice@hubtest.local"));
    let query = q.get_child("query", "http://jabber.org/protocol/disco#info").unwrap();
    assert_eq!(
        query.attr("node"),
        Some("http://example.org/client#QgayPKawpkPSDYmwT/WM94uAlu0=")
    );
    a.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn account_answers_disco_and_vcard() {
    let hub = hub_with_users().await;
    let mut a = client(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut b = client(&hub, "bob@hubtest.local/b", BOB_PW).await;
    a.send(st(
        "<iq xmlns='jabber:client' type='set' id='v1'><vCard xmlns='vcard-temp'><FN>Alice</FN></vCard></iq>",
    ))
    .await;
    assert_eq!(
        next_el(&mut a, |e| e.attr("id") == Some("v1")).await.attr("type"),
        Some("result")
    );
    b.send(st(
        "<iq xmlns='jabber:client' type='get' id='v2' to='alice@hubtest.local'><vCard xmlns='vcard-temp'/></iq>",
    ))
    .await;
    let r = next_el(&mut b, |e| e.attr("id") == Some("v2")).await;
    assert_eq!(r.attr("from"), Some("alice@hubtest.local"));
    let fn_ = r
        .get_child("vCard", "vcard-temp")
        .and_then(|v| v.get_child("FN", "vcard-temp"))
        .map(|f| f.text());
    assert_eq!(fn_.as_deref(), Some("Alice"));
    b.send(st(
        "<iq xmlns='jabber:client' type='get' id='d1' to='alice@hubtest.local'>\
        <query xmlns='http://jabber.org/protocol/disco#info'/></iq>",
    ))
    .await;
    let r = next_el(&mut b, |e| e.attr("id") == Some("d1")).await;
    let q = r.get_child("query", "http://jabber.org/protocol/disco#info").unwrap();
    assert!(q.children().any(|c| c.attr("category") == Some("account")));
    // Someone else's roster is not ours to read.
    b.send(st(
        "<iq xmlns='jabber:client' type='get' id='r1' to='alice@hubtest.local'><query xmlns='jabber:iq:roster'/></iq>",
    ))
    .await;
    assert_eq!(
        next_el(&mut b, |e| e.attr("id") == Some("r1")).await.attr("type"),
        Some("error")
    );
    a.close().await;
    b.close().await;
    hub.stop().await;
}
