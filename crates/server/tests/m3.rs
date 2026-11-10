//! M3 acceptance tests (docs/design/embedded-server.md §9): XEP-0198
//! stream management with resumption, XEP-0313 MAM with RSM paging, and
//! XEP-0359 stanza-ids.

mod common;

use std::time::Duration;

use common::{Proxy, TIMEOUT, TestHub, available, body, connect, next_el, next_event, online, st, sync};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::stanzastream::{Event, StanzaStream, StreamEvent};

const ALICE_PW: &str = "alice-password-123";
const BOB_PW: &str = "bob-password-4567";

async fn users(hub: &TestHub) {
    hub.add_user("alice", ALICE_PW).await;
    hub.add_user("bob", BOB_PW).await;
}

/// Send a MAM query; returns (result messages, the fin element).
async fn mam(c: &mut StanzaStream, id: &str, set: &str, extra: &str) -> (Vec<Element>, Element) {
    c.send(st(&format!(
        "<iq xmlns='jabber:client' type='set' id='{id}'><query xmlns='urn:xmpp:mam:2' queryid='q-{id}'>{extra}\
         <set xmlns='http://jabber.org/protocol/rsm'>{set}</set></query></iq>"
    )))
    .await;
    let mut results = Vec::new();
    loop {
        let el = next_el(c, |e| {
            (e.name() == "message" && e.has_child("result", "urn:xmpp:mam:2")) || e.attr("id") == Some(id)
        })
        .await;
        if el.name() == "iq" {
            assert_eq!(el.attr("type"), Some("result"), "{el:?}");
            return (results, el.get_child("fin", "urn:xmpp:mam:2").unwrap().clone());
        }
        let r = el.get_child("result", "urn:xmpp:mam:2").unwrap();
        assert_eq!(r.attr("queryid"), Some(format!("q-{id}").as_str()));
        results.push(el);
    }
}

fn result_body(m: &Element) -> String {
    let inner = m
        .get_child("result", "urn:xmpp:mam:2")
        .and_then(|r| r.get_child("forwarded", "urn:xmpp:forward:0"))
        .and_then(|f| f.get_child("message", "jabber:client"))
        .unwrap();
    body(inner).unwrap_or_default()
}

fn result_id(m: &Element) -> String {
    m.get_child("result", "urn:xmpp:mam:2")
        .unwrap()
        .attr("id")
        .unwrap()
        .to_owned()
}

fn rsm(fin: &Element, name: &str) -> Option<String> {
    fin.get_child("set", "http://jabber.org/protocol/rsm")
        .and_then(|s| s.get_child(name, "http://jabber.org/protocol/rsm"))
        .map(|e| e.text())
}

fn stanza_id_by(m: &Element, by: &str) -> Option<String> {
    m.children()
        .find(|c| c.is("stanza-id", "urn:xmpp:sid:0") && c.attr("by") == Some(by))
        .and_then(|c| c.attr("id"))
        .map(str::to_owned)
}

// ------------------------------------------------- acceptance test 2

#[tokio::test]
async fn at2_last_page_then_empty_complete_page() {
    let hub = TestHub::start().await;
    users(&hub).await;
    let mut b = connect(&hub, "bob@hubtest.local/b", BOB_PW).await;
    for i in 0..60 {
        b.send(st(&format!(
            "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'><body>m{i}</body></message>"
        )))
        .await;
    }
    sync(&mut b).await;
    let mut a = connect(&hub, "alice@hubtest.local/a", ALICE_PW).await;

    // before='' with max=50: the last 50, oldest first, not complete.
    let (rs, fin) = mam(&mut a, "p1", "<max>50</max><before/>", "").await;
    let bodies: Vec<String> = rs.iter().map(result_body).collect();
    assert_eq!(bodies.len(), 50);
    assert_eq!(bodies.first().map(String::as_str), Some("m10"));
    assert_eq!(bodies.last().map(String::as_str), Some("m59"));
    assert_eq!(fin.attr("complete"), None);
    assert_eq!(rsm(&fin, "count").as_deref(), Some("60"));
    let last = rsm(&fin, "last").unwrap();
    assert_eq!(last, result_id(rs.last().unwrap()));
    assert_eq!(rsm(&fin, "first").unwrap(), result_id(&rs[0]));
    // Results come from the archive's bare JID.
    assert_eq!(rs[0].attr("from"), Some("alice@hubtest.local"));

    // after=<last>: empty and complete.
    let (rs, fin) = mam(&mut a, "p2", &format!("<max>100</max><after>{last}</after>"), "").await;
    assert!(rs.is_empty());
    assert_eq!(fin.attr("complete"), Some("true"));

    // Paging forward from the start visits everything once.
    let mut seen = Vec::new();
    let mut after = String::new();
    for n in 0.. {
        let set = if after.is_empty() {
            "<max>25</max>".to_owned()
        } else {
            format!("<max>25</max><after>{after}</after>")
        };
        let (rs, fin) = mam(&mut a, &format!("f{n}"), &set, "").await;
        seen.extend(rs.iter().map(result_body));
        if fin.attr("complete") == Some("true") {
            break;
        }
        after = rsm(&fin, "last").unwrap();
    }
    assert_eq!(seen.len(), 60);
    assert_eq!(seen[0], "m0");

    // max=0 gives just the count; an unknown id is item-not-found.
    let (rs, fin) = mam(&mut a, "c0", "<max>0</max>", "").await;
    assert!(rs.is_empty());
    assert_eq!(rsm(&fin, "count").as_deref(), Some("60"));
    a.send(st(
        "<iq xmlns='jabber:client' type='set' id='bad'><query xmlns='urn:xmpp:mam:2'>\
        <set xmlns='http://jabber.org/protocol/rsm'><after>nope</after></set></query></iq>",
    ))
    .await;
    let e = next_el(&mut a, |e| e.attr("id") == Some("bad")).await;
    assert!(
        e.get_child("error", "jabber:client")
            .unwrap()
            .has_child("item-not-found", "urn:ietf:params:xml:ns:xmpp-stanzas")
    );
    // flip-page reverses the page; `with` filters.
    let (rs, _) = mam(&mut a, "fl", "<max>3</max><before/>", "<flip-page/>").await;
    let bodies: Vec<String> = rs.iter().map(result_body).collect();
    assert_eq!(bodies, ["m59", "m58", "m57"]);
    let form = "<x xmlns='jabber:x:data' type='submit'><field var='FORM_TYPE' type='hidden'>\
        <value>urn:xmpp:mam:2</value></field><field var='with'><value>carol@hubtest.local</value></field></x>";
    let (rs, fin) = mam(&mut a, "w", "<max>10</max>", form).await;
    assert!(rs.is_empty() && fin.attr("complete") == Some("true"));
    a.close().await;
    b.close().await;
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 3

#[tokio::test]
async fn at3_offline_and_archive_copies_share_the_stanza_id() {
    let hub = TestHub::start().await;
    users(&hub).await;
    let mut b = connect(&hub, "bob@hubtest.local/b", BOB_PW).await;
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'>\
        <body>offline</body><origin-id xmlns='urn:xmpp:sid:0' id='o-1'/></message>",
    ))
    .await;
    sync(&mut b).await;
    let mut a = connect(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    a.send(st("<presence xmlns='jabber:client'/>")).await;
    let m = next_el(&mut a, |e| e.name() == "message").await;
    let offline_id = stanza_id_by(&m, "alice@hubtest.local").expect("stanza-id on offline delivery");
    let (rs, _) = mam(&mut a, "q", "<max>10</max><before/>", "").await;
    assert_eq!(rs.len(), 1);
    assert_eq!(result_id(&rs[0]), offline_id);
    // The bob's outgoing copy is in bob's own archive with another id.
    let (rs, _) = mam(&mut b, "qb", "<max>10</max><before/>", "").await;
    assert_eq!(rs.len(), 1);
    assert_ne!(result_id(&rs[0]), offline_id);
    // Someone else's archive is not ours to query.
    b.send(st(
        "<iq xmlns='jabber:client' type='set' id='x' to='alice@hubtest.local'><query xmlns='urn:xmpp:mam:2'/></iq>",
    ))
    .await;
    assert_eq!(
        next_el(&mut b, |e| e.attr("id") == Some("x")).await.attr("type"),
        Some("error")
    );
    a.close().await;
    b.close().await;
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 4

#[tokio::test]
async fn at4_client_supplied_stanza_ids_are_stripped() {
    let hub = TestHub::start().await;
    users(&hub).await;
    let a = connect(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut b = connect(&hub, "bob@hubtest.local/b", BOB_PW).await;
    available(&mut b, 0).await;
    a.send(st(
        "<message xmlns='jabber:client' type='chat' to='bob@hubtest.local'><body>evil</body>\
        <stanza-id xmlns='urn:xmpp:sid:0' by='bob@hubtest.local' id='EVIL'/>\
        <stanza-id xmlns='urn:xmpp:sid:0' by='alice@hubtest.local' id='EVIL2'/></message>",
    ))
    .await;
    let m = next_el(&mut b, |e| e.name() == "message").await;
    let ids: Vec<(String, String)> = m
        .children()
        .filter(|c| c.is("stanza-id", "urn:xmpp:sid:0"))
        .map(|c| (c.attr("by").unwrap().to_owned(), c.attr("id").unwrap().to_owned()))
        .collect();
    assert_eq!(ids.len(), 1, "{ids:?}");
    assert_eq!(ids[0].0, "bob@hubtest.local");
    assert_ne!(ids[0].1, "EVIL");
    a.close().await;
    b.close().await;
    hub.stop().await;
}

// ------------------------------------------------- stream management

async fn wait_for_stream_event(c: &mut StanzaStream, want: fn(&StreamEvent) -> bool) {
    next_event(c, |ev| match ev {
        Event::Stream(s) if want(&s) => Some(()),
        _ => None,
    })
    .await
}

#[tokio::test]
async fn sm_resumes_after_a_network_drop_and_replays_what_was_missed() {
    let hub = TestHub::start().await;
    users(&hub).await;
    let proxy = Proxy::start(hub.addr).await;
    let mut a = proxy.client("alice@hubtest.local/phone", ALICE_PW);
    online(&mut a).await;
    available(&mut a, 0).await;
    let mut b = connect(&hub, "bob@hubtest.local/b", BOB_PW).await;
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local/phone'><body>before</body></message>",
    ))
    .await;
    assert_eq!(
        body(&next_el(&mut a, |e| e.name() == "message").await).as_deref(),
        Some("before")
    );

    // The network drops; the hub notices and hibernates the session.
    proxy.cut();
    let mut hibernating = 0;
    for _ in 0..100 {
        hibernating = hub.status().await.hibernating;
        if hibernating == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(hibernating, 1, "the session should hibernate, not end");
    // Meanwhile bob writes; it waits in the hibernating session.
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local/phone'><body>during</body></message>",
    ))
    .await;
    sync(&mut b).await;

    // tokio-xmpp reconnects by itself and resumes.
    wait_for_stream_event(&mut a, |s| matches!(s, StreamEvent::Resumed)).await;
    let m = next_el(&mut a, |e| e.name() == "message").await;
    assert_eq!(body(&m).as_deref(), Some("during"));
    let st_ = hub.status().await;
    assert_eq!((st_.sm_resumes, st_.hibernating, st_.sessions), (1, 0, 2));
    // Still the same session: presence was not interrupted, nothing duplicated.
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'><body>after</body></message>",
    ))
    .await;
    let m = next_el(&mut a, |e| e.name() == "message").await;
    assert_eq!(body(&m).as_deref(), Some("after"));
    a.close().await;
    b.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn sm_unacked_messages_go_offline_when_resumption_times_out() {
    let hub =
        TestHub::start_with(|c| c.sm.resume_timeout = agent_chat_server::config::HumanDuration(Duration::from_secs(1)))
            .await;
    users(&hub).await;
    // A raw client: enable SM, then vanish without acknowledging anything.
    let (mut raw, _) = common::Raw::login(hub.addr, "alice", ALICE_PW).await;
    raw.bind("laptop").await;
    raw.send("<enable xmlns='urn:xmpp:sm:3' resume='true'/>").await;
    let enabled = raw.read_until("</enabled>").await;
    assert!(enabled.contains("enabled") && enabled.contains("resume="), "{enabled}");
    raw.send("<presence/>").await;
    raw.read_until("/laptop").await;
    let b = connect(&hub, "bob@hubtest.local/b", BOB_PW).await;
    b.send(st(
        "<message xmlns='jabber:client' type='chat' to='alice@hubtest.local'><body>unacked</body></message>",
    ))
    .await;
    raw.read_until("unacked").await;
    drop(raw);
    // After the timeout the session ends and the message waits offline.
    let mut gone = false;
    for _ in 0..200 {
        if hub.status().await.sessions == 1 {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(gone, "the hibernating session should end");
    let mut a = connect(&hub, "alice@hubtest.local/again", ALICE_PW).await;
    a.send(st("<presence xmlns='jabber:client'/>")).await;
    let m = tokio::time::timeout(TIMEOUT, next_el(&mut a, |e| e.name() == "message"))
        .await
        .unwrap();
    assert_eq!(body(&m).as_deref(), Some("unacked"));
    a.close().await;
    b.close().await;
    hub.stop().await;
}

#[tokio::test]
async fn sm_resume_with_an_unknown_id_fails_and_binding_still_works() {
    let hub = TestHub::start().await;
    users(&hub).await;
    let (mut raw, features) = common::Raw::login(hub.addr, "alice", ALICE_PW).await;
    assert!(features.contains("urn:xmpp:sm:3"), "{features}");
    raw.send("<resume xmlns='urn:xmpp:sm:3' previd='nope' h='0'/>").await;
    let failed = raw.read_until("</failed>").await;
    assert!(failed.contains("item-not-found"), "{failed}");
    let r = raw.bind("x").await;
    assert!(r.contains("alice@hubtest.local/x"), "{r}");
    // <r/> before enable is ignored; after enable it is answered.
    raw.send("<enable xmlns='urn:xmpp:sm:3'/>").await;
    raw.read_until("enabled").await;
    raw.send("<iq type='get' id='p'><ping xmlns='urn:xmpp:ping'/></iq><r xmlns='urn:xmpp:sm:3'/>")
        .await;
    raw.read_until("<a ").await;
    let a = raw.read_until("</a>").await;
    assert!(a.contains("h=\"1\"") || a.contains("h='1'"), "{a}");
    hub.stop().await;
}
