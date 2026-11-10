//! The RFC 6121 subscription state machine (Appendix A) as a pure function,
//! plus roster XML (§5.6).
//!
//! `apply(item, event)` changes the roster item of the account that
//! processes the event and says what the actor must do next. "Out" events
//! are the account's own subscription stanzas, "In" events arrive from a
//! contact. The same code will serve remote contacts once s2s exists.

use tokio_xmpp::minidom::Element;
use xmpp_parsers::ns;

use crate::stanza;
use crate::store::{RosterItem, Subscription};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubEvent {
    OutSubscribe,
    OutSubscribed,
    OutUnsubscribe,
    OutUnsubscribed,
    InSubscribe,
    InSubscribed,
    InUnsubscribe,
    InUnsubscribed,
}

impl SubEvent {
    /// The event for a presence `type` sent by the account (`out`) or to it.
    pub fn from_type(ty: &str, out: bool) -> Option<Self> {
        use SubEvent::*;
        Some(match (ty, out) {
            ("subscribe", true) => OutSubscribe,
            ("subscribed", true) => OutSubscribed,
            ("unsubscribe", true) => OutUnsubscribe,
            ("unsubscribed", true) => OutUnsubscribed,
            ("subscribe", false) => InSubscribe,
            ("subscribed", false) => InSubscribed,
            ("unsubscribe", false) => InUnsubscribe,
            ("unsubscribed", false) => InUnsubscribed,
            _ => return None,
        })
    }
}

/// What the actor does after applying an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    /// The item changed: store it and push it to interested resources.
    pub changed: bool,
    /// Out events: route the stanza to the contact. In events: deliver it
    /// to the account's resources.
    pub forward: bool,
    /// In events: answer the contact with `subscribed` right away (the
    /// subscription already exists, or was pre-approved).
    pub auto_subscribed: bool,
    /// Out `subscribed`: send our current presence to the new subscriber.
    pub send_presence: bool,
    /// Out `unsubscribed`: send unavailable presence to the former subscriber.
    pub send_unavailable: bool,
}

/// Apply `ev` to `item` (RFC 6121 §3 and Appendix A).
pub fn apply(item: &mut RosterItem, ev: SubEvent) -> Outcome {
    use Subscription::*;
    let before = item.clone();
    let mut o = Outcome::default();
    match ev {
        SubEvent::OutSubscribe => {
            // §3.1.2: always routed; marks the request pending unless we
            // are already subscribed.
            if !item.subscription.has_to() {
                item.ask_out = true;
            }
            item.hidden = false;
            o.forward = true;
        }
        SubEvent::InSubscribe => {
            // §3.1.3
            if item.subscription.has_from() {
                o.auto_subscribed = true;
            } else if item.approved {
                item.subscription = if item.subscription == To { Both } else { From };
                item.approved = false;
                item.pending_in = false;
                o.auto_subscribed = true;
            } else {
                item.pending_in = true;
                o.forward = true;
            }
        }
        SubEvent::OutSubscribed => {
            // §3.1.5 (approve) or §3.4 (pre-approve).
            if item.subscription.has_from() {
                // Nothing to do; never routed.
            } else if item.pending_in {
                item.subscription = if item.subscription == To { Both } else { From };
                item.pending_in = false;
                item.hidden = false;
                o.forward = true;
                o.send_presence = true;
            } else {
                item.approved = true;
                item.hidden = false;
            }
        }
        SubEvent::InSubscribed => {
            // §3.1.6: only meaningful while our request is pending.
            if item.ask_out && !item.subscription.has_to() {
                item.subscription = if item.subscription == From { Both } else { To };
                item.ask_out = false;
                o.forward = true;
            }
        }
        SubEvent::OutUnsubscribe => {
            // §3.3.2
            item.subscription = match item.subscription {
                To => None,
                Both => From,
                s => s,
            };
            item.ask_out = false;
            o.forward = true;
        }
        SubEvent::InUnsubscribe => {
            // §3.3.3
            if item.subscription.has_from() || item.pending_in {
                item.subscription = match item.subscription {
                    From => None,
                    Both => To,
                    s => s,
                };
                item.pending_in = false;
                o.forward = true;
            }
        }
        SubEvent::OutUnsubscribed => {
            // §3.2.2: cancel their subscription or deny their request.
            let had_from = item.subscription.has_from();
            if had_from || item.pending_in || item.approved {
                item.subscription = match item.subscription {
                    From => None,
                    Both => To,
                    s => s,
                };
                o.forward = had_from || item.pending_in;
                o.send_unavailable = had_from;
                item.pending_in = false;
                item.approved = false;
            }
        }
        SubEvent::InUnsubscribed => {
            // §3.2.3
            if item.subscription.has_to() || item.ask_out {
                item.subscription = match item.subscription {
                    To => None,
                    Both => From,
                    s => s,
                };
                item.ask_out = false;
                o.forward = true;
            }
        }
    }
    o.changed = *item != before;
    o
}

/// `<item/>` for roster results and pushes.
pub fn item_element(item: &RosterItem) -> Element {
    let mut el = stanza::element(
        "item",
        ns::ROSTER,
        &[("jid", &item.contact), ("subscription", item.subscription.as_str())],
    );
    if let Some(n) = &item.name {
        stanza::set_attr(&mut el, "name", n.as_str());
    }
    if item.ask_out && !item.subscription.has_to() {
        stanza::set_attr(&mut el, "ask", "subscribe");
    }
    if item.approved {
        stanza::set_attr(&mut el, "approved", "true");
    }
    for g in &item.groups {
        let mut group = Element::bare("group", ns::ROSTER);
        group.append_text(g.as_str());
        el.append_child(group);
    }
    el
}

/// The `<query xmlns='jabber:iq:roster'>` for a roster get.
pub fn query<'a>(items: impl Iterator<Item = &'a RosterItem>) -> Element {
    let mut q = Element::bare("query", ns::ROSTER);
    for item in items.filter(|i| !i.hidden) {
        q.append_child(item_element(item));
    }
    q
}

/// A roster push IQ for one item (`removed` → `subscription='remove'`).
pub fn push(to: &str, item: &RosterItem, removed: bool, id: &str) -> Element {
    let mut iq = stanza::element("iq", stanza::NS_CLIENT, &[("type", "set"), ("id", id), ("to", to)]);
    let mut q = Element::bare("query", ns::ROSTER);
    let el = if removed {
        stanza::element(
            "item",
            ns::ROSTER,
            &[("jid", &item.contact), ("subscription", "remove")],
        )
    } else {
        item_element(item)
    };
    q.append_child(el);
    iq.append_child(q);
    iq
}

/// A roster set request after parsing: one item to add, update or remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetRequest {
    pub jid: String,
    pub name: Option<String>,
    pub groups: Vec<String>,
    pub remove: bool,
}

/// Parse `<query xmlns='jabber:iq:roster'>` of a set (RFC 6121 §2.3):
/// exactly one item with a valid JID.
pub fn parse_set(query: &Element) -> Option<SetRequest> {
    let mut items = query.children().filter(|c| c.is("item", ns::ROSTER));
    let item = items.next()?;
    if items.next().is_some() {
        return None;
    }
    let jid = xmpp_parsers::jid::BareJid::new(item.attr("jid")?).ok()?.to_string();
    let mut groups: Vec<String> = item
        .children()
        .filter(|c| c.is("group", ns::ROSTER))
        .map(|g| g.text())
        .filter(|g| !g.is_empty())
        .collect();
    groups.sort();
    groups.dedup();
    Some(SetRequest {
        jid,
        name: item.attr("name").filter(|n| !n.is_empty()).map(str::to_owned),
        groups,
        remove: item.attr("subscription") == Some("remove"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use SubEvent::*;
    use Subscription::*;

    /// A compact state: subscription plus the pending flags.
    fn st(sub: Subscription, ask_out: bool, pending_in: bool) -> RosterItem {
        RosterItem {
            subscription: sub,
            ask_out,
            pending_in,
            ..RosterItem::new("contact@hub.local")
        }
    }

    fn check(from: RosterItem, ev: SubEvent, to: RosterItem, forward: bool) {
        let mut item = from.clone();
        let o = apply(&mut item, ev);
        assert_eq!(
            (item.subscription, item.ask_out, item.pending_in),
            (to.subscription, to.ask_out, to.pending_in),
            "{ev:?} from {:?}/{}/{}",
            from.subscription,
            from.ask_out,
            from.pending_in
        );
        assert_eq!(o.forward, forward, "forward for {ev:?} from {from:?}");
    }

    /// RFC 6121 Appendix A.2.1: outbound subscribe.
    #[test]
    fn a21_outbound_subscribe() {
        check(st(None, false, false), OutSubscribe, st(None, true, false), true);
        check(st(None, false, true), OutSubscribe, st(None, true, true), true);
        check(st(To, false, false), OutSubscribe, st(To, false, false), true);
        check(st(From, false, false), OutSubscribe, st(From, true, false), true);
        check(st(Both, false, false), OutSubscribe, st(Both, false, false), true);
    }

    /// A.2.2: outbound unsubscribe.
    #[test]
    fn a22_outbound_unsubscribe() {
        check(st(None, true, false), OutUnsubscribe, st(None, false, false), true);
        check(st(To, false, false), OutUnsubscribe, st(None, false, false), true);
        check(st(To, false, true), OutUnsubscribe, st(None, false, true), true);
        check(st(Both, false, false), OutUnsubscribe, st(From, false, false), true);
    }

    /// A.2.3: outbound subscribed (approve, or pre-approve).
    #[test]
    fn a23_outbound_subscribed() {
        check(st(None, false, true), OutSubscribed, st(From, false, false), true);
        check(st(None, true, true), OutSubscribed, st(From, true, false), true);
        check(st(To, false, true), OutSubscribed, st(Both, false, false), true);
        check(st(From, false, false), OutSubscribed, st(From, false, false), false);
        let mut pre = st(None, false, false);
        let o = apply(&mut pre, OutSubscribed);
        assert!(pre.approved && o.changed && !o.forward);
    }

    /// A.2.4: outbound unsubscribed (cancel, or deny).
    #[test]
    fn a24_outbound_unsubscribed() {
        check(st(None, false, true), OutUnsubscribed, st(None, false, false), true);
        check(st(From, false, false), OutUnsubscribed, st(None, false, false), true);
        check(st(Both, false, false), OutUnsubscribed, st(To, false, false), true);
        check(st(To, false, false), OutUnsubscribed, st(To, false, false), false);
        let mut both = st(Both, false, false);
        assert!(apply(&mut both, OutUnsubscribed).send_unavailable);
    }

    /// A.3.1: inbound subscribe.
    #[test]
    fn a31_inbound_subscribe() {
        check(st(None, false, false), InSubscribe, st(None, false, true), true);
        check(st(None, true, false), InSubscribe, st(None, true, true), true);
        check(st(To, false, false), InSubscribe, st(To, false, true), true);
        let mut from = st(From, false, false);
        let o = apply(&mut from, InSubscribe);
        assert!(o.auto_subscribed && !o.forward && !o.changed);
        let mut approved = st(To, false, false);
        approved.approved = true;
        let o = apply(&mut approved, InSubscribe);
        assert!(o.auto_subscribed && !o.forward);
        assert_eq!(approved.subscription, Both);
    }

    /// A.3.2: inbound unsubscribe.
    #[test]
    fn a32_inbound_unsubscribe() {
        check(st(None, false, true), InUnsubscribe, st(None, false, false), true);
        check(st(From, false, false), InUnsubscribe, st(None, false, false), true);
        check(st(Both, false, false), InUnsubscribe, st(To, false, false), true);
        check(st(None, false, false), InUnsubscribe, st(None, false, false), false);
    }

    /// A.3.3: inbound subscribed.
    #[test]
    fn a33_inbound_subscribed() {
        check(st(None, true, false), InSubscribed, st(To, false, false), true);
        check(st(None, true, true), InSubscribed, st(To, false, true), true);
        check(st(From, true, false), InSubscribed, st(Both, false, false), true);
        // Not pending: ignored.
        check(st(None, false, false), InSubscribed, st(None, false, false), false);
        check(st(To, false, false), InSubscribed, st(To, false, false), false);
    }

    /// A.3.4: inbound unsubscribed.
    #[test]
    fn a34_inbound_unsubscribed() {
        check(st(None, true, false), InUnsubscribed, st(None, false, false), true);
        check(st(To, false, false), InUnsubscribed, st(None, false, false), true);
        check(st(Both, false, false), InUnsubscribed, st(From, false, false), true);
        check(st(From, false, false), InUnsubscribed, st(From, false, false), false);
    }

    #[test]
    fn roster_xml() {
        let mut item = st(None, true, false);
        item.name = Some("Bob".into());
        item.groups = vec!["Friends".into()];
        let el = item_element(&item);
        assert_eq!(el.attr("ask"), Some("subscribe"));
        assert_eq!(el.attr("subscription"), Some("none"));
        assert_eq!(el.get_child("group", ns::ROSTER).unwrap().text(), "Friends");
        let mut hidden = st(None, false, true);
        hidden.hidden = true;
        assert_eq!(query([item.clone(), hidden].iter()).children().count(), 1);

        let q: Element = "<query xmlns='jabber:iq:roster'><item jid='Bob@Hub.Local' name='B'>\
            <group>x</group><group>x</group></item></query>"
            .parse()
            .unwrap();
        let req = parse_set(&q).unwrap();
        assert_eq!(req.jid, "bob@hub.local");
        assert_eq!(req.groups, ["x"]);
        let two: Element = "<query xmlns='jabber:iq:roster'><item jid='a@b'/><item jid='c@d'/></query>"
            .parse()
            .unwrap();
        assert!(parse_set(&two).is_none());
    }
}
