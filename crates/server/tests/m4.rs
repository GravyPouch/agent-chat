//! M4 acceptance tests 3 and 4 (docs/design/embedded-server.md §9): PEP
//! publish-options, access models, `+notify` notifications and last items.
//! Tests 1 and 2 (OMEMO between daemons, python-omemo interop) are
//! end-to-end runs: `crates/cli/tests/hub_e2e.rs` and
//! `dev/omemo-bot/interop-test.sh` with `XMPP_SERVER=hub`.

mod common;

use base64::Engine;
use common::{TestHub, available, connect, next_el, st, sync};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::stanzastream::StanzaStream;
use xmpp_parsers::disco::{DiscoInfoResult, Identity};

const ALICE_PW: &str = "alice-password-123";
const BOB_PW: &str = "bob-password-4567";
const CAROL_PW: &str = "carol-password-890";
const NS_PUBSUB: &str = "http://jabber.org/protocol/pubsub";
const DEVICES: &str = "urn:xmpp:omemo:2:devices";
const CAPS_NODE: &str = "https://example.org/test-client";

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

fn error_has(e: &Element, cond: &str, ns: &str) -> bool {
    e.attr("type") == Some("error")
        && e.get_child("error", "jabber:client")
            .is_some_and(|er| er.has_child(cond, ns))
}

fn publish(id: &str, node: &str, item: &str, payload: &str, options: &[(&str, &str)]) -> String {
    let opts = if options.is_empty() {
        String::new()
    } else {
        let fields: String = options
            .iter()
            .map(|(k, v)| format!("<field var='{k}'><value>{v}</value></field>"))
            .collect();
        format!(
            "<publish-options><x xmlns='jabber:x:data' type='submit'><field var='FORM_TYPE' type='hidden'>\
             <value>http://jabber.org/protocol/pubsub#publish-options</value></field>{fields}</x></publish-options>"
        )
    };
    format!(
        "<iq xmlns='jabber:client' type='set' id='{id}'><pubsub xmlns='{NS_PUBSUB}'>\
         <publish node='{node}'><item id='{item}'>{payload}</item></publish>{opts}</pubsub></iq>"
    )
}

fn items_query(id: &str, to: &str, node: &str) -> String {
    format!(
        "<iq xmlns='jabber:client' type='get' id='{id}' to='{to}'><pubsub xmlns='{NS_PUBSUB}'>\
         <items node='{node}'/></pubsub></iq>"
    )
}

fn device_list(ids: &[u32]) -> String {
    let devs: String = ids.iter().map(|i| format!("<device id='{i}'/>")).collect();
    format!("<devices xmlns='urn:xmpp:omemo:2'>{devs}</devices>")
}

/// A disco#info answer for `features` and its XEP-0115 ver.
fn caps(features: &[&str]) -> (DiscoInfoResult, String) {
    let info = DiscoInfoResult {
        node: None,
        identities: vec![Identity::new("client", "bot", "en", "test")],
        features: features.iter().map(|f| f.to_string()).collect(),
        extensions: vec![],
    };
    let data = xmpp_parsers::caps::compute_disco(&info);
    let hash = xmpp_parsers::caps::hash_caps(&data, xmpp_parsers::hashes::Algo::Sha_1).unwrap();
    (info, base64::engine::general_purpose::STANDARD.encode(hash.hash))
}

/// Send presence with caps and answer the hub's disco#info.
async fn present_with_caps(c: &mut StanzaStream, features: &[&str]) {
    let (mut info, ver) = caps(features);
    c.send(st(&format!(
        "<presence xmlns='jabber:client'><c xmlns='http://jabber.org/protocol/caps' hash='sha-1' \
         node='{CAPS_NODE}' ver='{ver}'/></presence>"
    )))
    .await;
    let q = next_el(c, |e| {
        e.name() == "iq"
            && e.attr("type") == Some("get")
            && e.has_child("query", "http://jabber.org/protocol/disco#info")
    })
    .await;
    info.node = Some(format!("{CAPS_NODE}#{ver}"));
    let mut reply = tokio_xmpp::minidom::Element::builder("iq", "jabber:client")
        .attr("type".try_into().unwrap(), "result")
        .attr("id".try_into().unwrap(), q.attr("id").unwrap())
        .attr("to".try_into().unwrap(), q.attr("from").unwrap())
        .build();
    reply.append_child(info.into());
    c.send(st(&String::from(&reply))).await;
}

/// `x` asks `y` for a subscription and `y` approves.
async fn subscribe(x: &mut StanzaStream, y: &mut StanzaStream, x_jid: &str, y_jid: &str) {
    x.send(st(&format!(
        "<presence xmlns='jabber:client' type='subscribe' to='{y_jid}'/>"
    )))
    .await;
    sync(x).await;
    y.send(st(&format!(
        "<presence xmlns='jabber:client' type='subscribed' to='{x_jid}'/>"
    )))
    .await;
    sync(y).await;
}

/// alice and bob are mutual contacts.
async fn befriend(a: &mut StanzaStream, b: &mut StanzaStream) {
    subscribe(a, b, "alice@hubtest.local", "bob@hubtest.local").await;
    subscribe(b, a, "bob@hubtest.local", "alice@hubtest.local").await;
}

// ------------------------------------------------- acceptance test 3

#[tokio::test]
async fn at3_publish_options_and_access_models() {
    let hub = hub_with_users().await;
    let mut a = connect(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut c = connect(&hub, "carol@hubtest.local/c", CAROL_PW).await;

    // OMEMO:2 bundles: open, max_items=max (as our client publishes them).
    let r = iq(
        &mut a,
        &publish(
            "p1",
            "urn:xmpp:omemo:2:bundles",
            "1001",
            "<bundle xmlns='urn:xmpp:omemo:2'/>",
            &[("pubsub#access_model", "open"), ("pubsub#max_items", "max")],
        ),
    )
    .await;
    assert_eq!(r.attr("type"), Some("result"), "{r:?}");
    // A second bundle item is kept next to the first (max_items=max).
    let r = iq(
        &mut a,
        &publish(
            "p2",
            "urn:xmpp:omemo:2:bundles",
            "1002",
            "<bundle xmlns='urn:xmpp:omemo:2'/>",
            &[("pubsub#access_model", "open"), ("pubsub#max_items", "max")],
        ),
    )
    .await;
    assert_eq!(r.attr("type"), Some("result"));
    // Mismatching options: precondition-not-met (conflict).
    let r = iq(
        &mut a,
        &publish(
            "p3",
            "urn:xmpp:omemo:2:bundles",
            "1003",
            "<bundle xmlns='urn:xmpp:omemo:2'/>",
            &[("pubsub#access_model", "presence")],
        ),
    )
    .await;
    assert!(
        error_has(&r, "conflict", "urn:ietf:params:xml:ns:xmpp-stanzas"),
        "{r:?}"
    );
    assert!(error_has(
        &r,
        "precondition-not-met",
        "http://jabber.org/protocol/pubsub#errors"
    ));
    // Without options it works (our client's retry).
    let r = iq(
        &mut a,
        &publish(
            "p4",
            "urn:xmpp:omemo:2:bundles",
            "1003",
            "<bundle xmlns='urn:xmpp:omemo:2'/>",
            &[],
        ),
    )
    .await;
    assert_eq!(r.attr("type"), Some("result"));

    // A non-contact reads the open node, item by id…
    let r = iq(
        &mut c,
        "<iq xmlns='jabber:client' type='get' id='g1' to='alice@hubtest.local'>\
         <pubsub xmlns='http://jabber.org/protocol/pubsub'><items node='urn:xmpp:omemo:2:bundles'>\
         <item id='1002'/></items></pubsub></iq>",
    )
    .await;
    let items = r
        .get_child("pubsub", NS_PUBSUB)
        .unwrap()
        .get_child("items", NS_PUBSUB)
        .unwrap();
    let ids: Vec<_> = items.children().filter_map(|i| i.attr("id")).collect();
    assert_eq!(ids, ["1002"]);
    // …and all of them, oldest first.
    let r = iq(
        &mut c,
        &items_query("g2", "alice@hubtest.local", "urn:xmpp:omemo:2:bundles"),
    )
    .await;
    let items = r
        .get_child("pubsub", NS_PUBSUB)
        .unwrap()
        .get_child("items", NS_PUBSUB)
        .unwrap();
    let ids: Vec<_> = items.children().filter_map(|i| i.attr("id")).collect();
    assert_eq!(ids, ["1001", "1002", "1003"]);

    // A presence-access node (the default) is closed to non-contacts.
    let r = iq(
        &mut a,
        &publish(
            "p5",
            "urn:xmpp:avatar:metadata",
            "x",
            "<metadata xmlns='urn:xmpp:avatar:metadata'/>",
            &[],
        ),
    )
    .await;
    assert_eq!(r.attr("type"), Some("result"));
    let r = iq(
        &mut c,
        &items_query("g3", "alice@hubtest.local", "urn:xmpp:avatar:metadata"),
    )
    .await;
    assert!(
        error_has(&r, "not-authorized", "urn:ietf:params:xml:ns:xmpp-stanzas"),
        "{r:?}"
    );
    // Missing nodes and items: item-not-found (our client reads that as "no devices").
    let r = iq(&mut c, &items_query("g4", "alice@hubtest.local", DEVICES)).await;
    assert!(
        error_has(&r, "item-not-found", "urn:ietf:params:xml:ns:xmpp-stanzas"),
        "{r:?}"
    );
    // Only the owner publishes; the owner reads its own nodes without `to`.
    let r = iq(
        &mut c,
        "<iq xmlns='jabber:client' type='set' id='x' to='alice@hubtest.local'>\
         <pubsub xmlns='http://jabber.org/protocol/pubsub'><publish node='n'><item/></publish></pubsub></iq>",
    )
    .await;
    assert!(
        error_has(&r, "forbidden", "urn:ietf:params:xml:ns:xmpp-stanzas"),
        "{r:?}"
    );
    let r = iq(
        &mut a,
        "<iq xmlns='jabber:client' type='get' id='own'><pubsub xmlns='http://jabber.org/protocol/pubsub'>\
         <items node='urn:xmpp:avatar:metadata'/></pubsub></iq>",
    )
    .await;
    assert_eq!(r.attr("type"), Some("result"));

    // Owner configuration: read, change, delete.
    let r = iq(
        &mut a,
        "<iq xmlns='jabber:client' type='get' id='cfg'><pubsub xmlns='http://jabber.org/protocol/pubsub#owner'>\
         <configure node='urn:xmpp:omemo:2:bundles'/></pubsub></iq>",
    )
    .await;
    let x = r
        .get_child("pubsub", "http://jabber.org/protocol/pubsub#owner")
        .and_then(|p| p.get_child("configure", "http://jabber.org/protocol/pubsub#owner"))
        .and_then(|c| c.get_child("x", "jabber:x:data"))
        .unwrap();
    let access = x
        .children()
        .find(|f| f.attr("var") == Some("pubsub#access_model"))
        .and_then(|f| f.get_child("value", "jabber:x:data"))
        .map(|v| v.text());
    assert_eq!(access.as_deref(), Some("open"));
    let r = iq(
        &mut a,
        "<iq xmlns='jabber:client' type='set' id='del'><pubsub xmlns='http://jabber.org/protocol/pubsub#owner'>\
         <delete node='urn:xmpp:avatar:metadata'/></pubsub></iq>",
    )
    .await;
    assert_eq!(r.attr("type"), Some("result"));
    // disco#items lists what a non-contact may read.
    let r = iq(
        &mut c,
        "<iq xmlns='jabber:client' type='get' id='di' to='alice@hubtest.local'>\
         <query xmlns='http://jabber.org/protocol/disco#items'/></iq>",
    )
    .await;
    let nodes: Vec<_> = r
        .get_child("query", "http://jabber.org/protocol/disco#items")
        .unwrap()
        .children()
        .filter_map(|i| i.attr("node"))
        .collect();
    assert_eq!(nodes, ["urn:xmpp:omemo:2:bundles"]);
    a.close().await;
    c.close().await;
    hub.stop().await;
}

// ------------------------------------------------- acceptance test 4

#[tokio::test]
async fn at4_notify_on_publish_and_last_item_on_coming_online() {
    let hub = hub_with_users().await;
    let mut a = connect(&hub, "alice@hubtest.local/a", ALICE_PW).await;
    let mut b = connect(&hub, "bob@hubtest.local/b", BOB_PW).await;
    available(&mut a, 0).await;
    available(&mut b, 0).await;
    befriend(&mut a, &mut b).await;
    // bob's client wants OMEMO:2 device lists.
    present_with_caps(
        &mut b,
        &["http://jabber.org/protocol/caps", "urn:xmpp:omemo:2:devices+notify"],
    )
    .await;
    sync(&mut b).await;

    // alice publishes her device list: bob is notified.
    let r = iq(
        &mut a,
        &publish(
            "d1",
            DEVICES,
            "current",
            &device_list(&[11]),
            &[("pubsub#access_model", "open")],
        ),
    )
    .await;
    assert_eq!(r.attr("type"), Some("result"));
    let ev = next_el(&mut b, |e| {
        e.name() == "message" && e.has_child("event", "http://jabber.org/protocol/pubsub#event")
    })
    .await;
    assert_eq!(ev.attr("from"), Some("alice@hubtest.local"));
    assert_eq!(ev.attr("type"), Some("headline"));
    let items = ev
        .get_child("event", "http://jabber.org/protocol/pubsub#event")
        .unwrap()
        .get_child("items", "http://jabber.org/protocol/pubsub#event")
        .unwrap();
    assert_eq!(items.attr("node"), Some(DEVICES));
    // A node bob doesn't +notify sends him nothing.
    iq(
        &mut a,
        &publish(
            "t1",
            "http://jabber.org/protocol/tune",
            "current",
            "<tune xmlns='http://jabber.org/protocol/tune'/>",
            &[],
        ),
    )
    .await;
    iq(
        &mut a,
        &publish(
            "d2",
            DEVICES,
            "current",
            &device_list(&[11, 12]),
            &[("pubsub#access_model", "open")],
        ),
    )
    .await;
    let ev = next_el(&mut b, |e| {
        e.name() == "message" && e.has_child("event", "http://jabber.org/protocol/pubsub#event")
    })
    .await;
    let node = ev
        .get_child("event", "http://jabber.org/protocol/pubsub#event")
        .and_then(|e| e.get_child("items", "http://jabber.org/protocol/pubsub#event"))
        .and_then(|i| i.attr("node"));
    assert_eq!(node, Some(DEVICES), "the tune event must not reach bob");

    // A new bob resource with the same caps gets the last item when it comes online.
    let mut b2 = connect(&hub, "bob@hubtest.local/b2", BOB_PW).await;
    let (_, ver) = caps(&["http://jabber.org/protocol/caps", "urn:xmpp:omemo:2:devices+notify"]);
    b2.send(st(&format!(
        "<presence xmlns='jabber:client'><c xmlns='http://jabber.org/protocol/caps' hash='sha-1' \
         node='{CAPS_NODE}' ver='{ver}'/></presence>"
    )))
    .await;
    // The caps are cached: no disco#info query this time, straight to the last item.
    let ev = next_el(&mut b2, |e| {
        (e.name() == "message" && e.has_child("event", "http://jabber.org/protocol/pubsub#event"))
            || (e.name() == "iq" && e.attr("type") == Some("get"))
    })
    .await;
    assert_eq!(ev.name(), "message", "caps should come from the cache: {ev:?}");
    let item = ev
        .get_child("event", "http://jabber.org/protocol/pubsub#event")
        .and_then(|e| e.get_child("items", "http://jabber.org/protocol/pubsub#event"))
        .and_then(|i| i.get_child("item", "http://jabber.org/protocol/pubsub#event"))
        .unwrap();
    let devices = item
        .get_child("devices", "urn:xmpp:omemo:2")
        .unwrap()
        .children()
        .count();
    assert_eq!(devices, 2, "the latest list");
    // Owners get their own notifications too.
    present_with_caps(&mut a, &["urn:xmpp:omemo:2:devices+notify"]).await;
    let ev = next_el(&mut a, |e| {
        e.name() == "message" && e.has_child("event", "http://jabber.org/protocol/pubsub#event")
    })
    .await;
    assert_eq!(ev.attr("from"), Some("alice@hubtest.local"));
    for s in [a, b, b2] {
        s.close().await;
    }
    hub.stop().await;
}
