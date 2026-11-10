//! Messages in the account's actor: the delivery rules of RFC 6121 §8.5
//! (§5.5 of the plan), carbons (XEP-0280) and offline storage.

use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::Jid;
use xmpp_parsers::ns;

use super::{UserActor, blocked_error, jid_attr};
use crate::stanza::{self, Condition, NS_CLIENT};

pub const NS_CARBONS: &str = "urn:xmpp:carbons:2";
pub const NS_FORWARD: &str = "urn:xmpp:forward:0";
pub const NS_HINTS: &str = "urn:xmpp:hints";
pub const NS_DELAY: &str = "urn:xmpp:delay";
pub const NS_MUC_USER: &str = "http://jabber.org/protocol/muc#user";
const NS_OMEMO2: &str = "urn:xmpp:omemo:2";
const NS_OMEMO_LEGACY: &str = "eu.siacs.conversations.axolotl";
const NS_CONFERENCE: &str = "jabber:x:conference";

fn hint(el: &Element, name: &str) -> bool {
    el.has_child(name, NS_HINTS)
}

pub fn has_body(el: &Element) -> bool {
    el.has_child("body", NS_CLIENT)
}

pub fn is_encrypted(el: &Element) -> bool {
    el.has_child("encrypted", NS_OMEMO2) || el.has_child("encrypted", NS_OMEMO_LEGACY)
}

/// Carbon copies and archive results wrap other messages; they are never
/// stored or copied again.
pub fn is_wrapper(el: &Element) -> bool {
    el.children().any(|c| {
        (c.ns() == NS_CARBONS && matches!(c.name(), "received" | "sent"))
            || c.is("result", "urn:xmpp:mam:2")
            || c.is("event", "http://jabber.org/protocol/pubsub#event")
    })
}

/// Worth keeping: a body, OMEMO, a store hint, or an invitation.
fn has_content(el: &Element) -> bool {
    has_body(el)
        || is_encrypted(el)
        || hint(el, "store")
        || el.has_child("x", NS_CONFERENCE)
        || el
            .get_child("x", NS_MUC_USER)
            .is_some_and(|x| x.has_child("invite", NS_MUC_USER))
}

/// The archiving policy for user archives (§5.7 MAM).
pub fn archivable(el: &Element) -> bool {
    matches!(stanza::stanza_type(el), "chat" | "normal")
        && !is_wrapper(el)
        && !hint(el, "no-store")
        && !hint(el, "no-permanent-store")
        && has_content(el)
}

/// Stored for an offline account (RFC 6121 §8.5.2.2.1). Bodiless chat
/// states never are.
pub fn offline_storable(el: &Element) -> bool {
    matches!(stanza::stanza_type(el), "chat" | "normal") && !is_wrapper(el) && !hint(el, "no-store") && has_content(el)
}

/// XEP-0280 §6: which messages are copied.
pub fn carbon_eligible(el: &Element) -> bool {
    let ok_type = match stanza::stanza_type(el) {
        "chat" => true,
        "normal" => has_body(el),
        _ => false,
    };
    ok_type && !is_wrapper(el) && !el.has_child("private", NS_CARBONS) && !hint(el, "no-copy")
}

/// `<message from=bare to=full><received|sent><forwarded>original…`.
fn carbon(kind: &str, original: &Element, from: &str, to: &str) -> Element {
    let mut outer = stanza::element("message", NS_CLIENT, &[("from", from), ("to", to)]);
    stanza::set_attr(&mut outer, "type", stanza::stanza_type(original));
    let mut fwd = Element::bare("forwarded", NS_FORWARD);
    fwd.append_child(original.clone());
    let mut wrap = Element::bare(kind, NS_CARBONS);
    wrap.append_child(fwd);
    outer.append_child(wrap);
    outer
}

/// XEP-0203 `<delay/>`.
pub fn delay(from: &str, ts_ms: i64) -> Element {
    let stamp = chrono::DateTime::from_timestamp_millis(ts_ms)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();
    stanza::element("delay", NS_DELAY, &[("from", from), ("stamp", &stamp)])
}

/// Remove direct children matching `pred`.
pub fn remove_children(el: &mut Element, pred: impl Fn(&Element) -> bool) {
    if !el.children().any(&pred) {
        return;
    }
    for node in el.take_nodes() {
        match node {
            tokio_xmpp::minidom::Node::Element(c) if pred(&c) => {}
            other => el.append_node(other),
        }
    }
}

impl UserActor {
    pub(super) async fn outbound_message(&mut self, resource: &str, mut el: Element) {
        let to = match jid_attr(&el, "to") {
            Some(t) => t,
            None => {
                // No `to`: the account itself.
                stanza::set_attr(&mut el, "to", self.bare_str.as_str());
                self.bare.clone().into()
            }
        };
        if self.is_own(&to) {
            self.inbound_message(el).await;
            return;
        }
        if self.is_blocked(&to).await {
            if let Some(e) = blocked_error(&el, &to.to_string()) {
                self.send(resource, e);
            }
            return;
        }
        if carbon_eligible(&el) {
            let targets: Vec<(String, String)> = self
                .sessions
                .iter()
                .filter(|(r, s)| r.as_str() != resource && s.carbons)
                .map(|(r, s)| (r.clone(), s.full.clone()))
                .collect();
            for (r, full) in targets {
                let c = carbon("sent", &el, &self.bare_str.clone(), &full);
                self.send(&r, c);
            }
        }
        // XEP-0280 §7: the private marker is for us, not the recipient.
        remove_children(&mut el, |c| c.is("private", NS_CARBONS));
        self.archive_outgoing(&mut el, &to).await;
        self.route(el).await;
    }

    pub(super) async fn inbound_message(&mut self, mut el: Element) {
        let ty = stanza::stanza_type(&el).to_owned();
        let resource = jid_attr(&el, "to").and_then(|j| j.resource().map(|r| r.as_str().to_owned()));
        if let Some(r) = &resource
            && self.sessions.contains_key(r)
        {
            self.archive_incoming(&mut el).await;
            let delivered = [r.clone()];
            self.send(r, el.clone());
            self.received_carbons(&el, &delivered);
            return;
        }
        if resource.is_some() && matches!(ty.as_str(), "groupchat" | "headline" | "error") {
            // RFC 6121 §8.5.3.2.1: offline resource.
            return;
        }
        match ty.as_str() {
            "groupchat" => {
                self.router
                    .bounce(&el, Condition::ServiceUnavailable, Some(&self.bare_str));
            }
            "headline" => {
                for r in self.non_negative(false) {
                    self.send(&r, el.clone());
                }
            }
            "error" => {
                for r in self.non_negative(true) {
                    self.send(&r, el.clone());
                }
            }
            _ => {
                let targets = self.non_negative(ty == "chat");
                if targets.is_empty() {
                    self.store_offline(el).await;
                    return;
                }
                self.archive_incoming(&mut el).await;
                for r in &targets {
                    self.send(r, el.clone());
                }
                self.received_carbons(&el, &targets);
            }
        }
    }

    /// Available resources with a non-negative priority; with `top`, only
    /// those with the highest priority (all of them on a tie).
    fn non_negative(&self, top: bool) -> Vec<String> {
        let candidates: Vec<(&String, i8)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.available() && s.priority >= 0)
            .map(|(r, s)| (r, s.priority))
            .collect();
        let max = candidates.iter().map(|c| c.1).max();
        candidates
            .into_iter()
            .filter(|c| !top || Some(c.1) == max)
            .map(|c| c.0.clone())
            .collect()
    }

    /// `received` carbons to carbons-enabled resources that didn't get `el`.
    fn received_carbons(&mut self, el: &Element, delivered: &[String]) {
        if !carbon_eligible(el) {
            return;
        }
        let targets: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter(|(r, s)| s.carbons && !delivered.contains(r))
            .map(|(r, s)| (r.clone(), s.full.clone()))
            .collect();
        for (r, full) in targets {
            let c = carbon("received", el, &self.bare_str.clone(), &full);
            self.send(&r, c);
        }
    }

    /// No resource can take it now: keep it for the next initial presence.
    async fn store_offline(&mut self, mut el: Element) {
        if !offline_storable(&el) {
            // Chat states and the like: dropped silently (§8.5.2.2.1 lets
            // us choose; an error would be noise).
            return;
        }
        let archive_id = self.archive_incoming(&mut el).await;
        let cap = self.router.settings().offline_cap;
        if let Err(e) = self
            .router
            .store()
            .offline_push(&self.bare_str, archive_id, String::from(&el), cap)
            .await
        {
            tracing::error!(account = %self.bare, "storing an offline message failed: {e:#}");
            self.router
                .bounce(&el, Condition::ServiceUnavailable, Some(&self.bare_str));
        }
    }

    /// Hand the offline queue to a resource that just became available.
    pub(super) async fn deliver_offline(&mut self, resource: &str) {
        let queued = match self.router.store().offline_take(&self.bare_str).await {
            Ok(q) => q,
            Err(e) => {
                tracing::error!(account = %self.bare, "reading offline messages failed: {e:#}");
                return;
            }
        };
        let domain = self.router.domain().to_owned();
        for (xml, added_ms) in queued {
            let Ok(mut el) = xml.parse::<Element>() else { continue };
            if !el.has_child("delay", NS_DELAY) {
                el.append_child(delay(&domain, added_ms));
            }
            self.send(resource, el.clone());
            self.received_carbons(&el, &[resource.to_owned()]);
        }
    }

    /// A stanza a hibernated session never acknowledged (XEP-0198 §5).
    pub(super) async fn requeue(&mut self, el: Element) {
        if crate::stanza::Kind::of(&el) != Some(crate::stanza::Kind::Message) || !offline_storable(&el) {
            return;
        }
        if !self.non_negative(false).is_empty() {
            // Another resource is online; it got the message or a carbon,
            // and the archive has it.
            return;
        }
        let mut el = el;
        stanza::set_attr(&mut el, "to", self.bare_str.as_str());
        let cap = self.router.settings().offline_cap;
        let archive_id = el
            .children()
            .find(|c| c.is("stanza-id", ns::SID) && c.attr("by") == Some(self.bare_str.as_str()))
            .and_then(|c| c.attr("id"))
            .map(str::to_owned);
        if let Err(e) = self
            .router
            .store()
            .offline_push(&self.bare_str, archive_id, String::from(&el), cap)
            .await
        {
            tracing::error!(account = %self.bare, "requeueing a message failed: {e:#}");
        }
    }

    /// Stamp and archive a message delivered to this account; returns the
    /// archive id (also its stanza-id). Archiving arrives with MAM (M3).
    async fn archive_incoming(&mut self, el: &mut Element) -> Option<String> {
        let _ = el;
        None
    }

    async fn archive_outgoing(&mut self, el: &mut Element, to: &Jid) {
        let _ = (el, to);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> Element {
        s.parse().unwrap()
    }

    #[test]
    fn policies() {
        let chat = m("<message xmlns='jabber:client' type='chat'><body>hi</body></message>");
        assert!(archivable(&chat) && offline_storable(&chat) && carbon_eligible(&chat));
        let state = m("<message xmlns='jabber:client' type='chat'>\
            <composing xmlns='http://jabber.org/protocol/chatstates'/>\
            <no-store xmlns='urn:xmpp:hints'/></message>");
        assert!(!archivable(&state) && !offline_storable(&state));
        assert!(carbon_eligible(&state), "chat states are copied (XEP-0280 §6)");
        let omemo = m("<message xmlns='jabber:client' type='chat'><encrypted xmlns='urn:xmpp:omemo:2'/></message>");
        assert!(archivable(&omemo));
        let private = m("<message xmlns='jabber:client' type='chat'><body>x</body>\
            <private xmlns='urn:xmpp:carbons:2'/></message>");
        assert!(!carbon_eligible(&private));
        let normal_no_body = m("<message xmlns='jabber:client'><x xmlns='jabber:x:conference' jid='r@c'/></message>");
        assert!(!carbon_eligible(&normal_no_body));
        assert!(offline_storable(&normal_no_body));
        let headline = m("<message xmlns='jabber:client' type='headline'><body>x</body></message>");
        assert!(!archivable(&headline) && !offline_storable(&headline));
        let npst = m("<message xmlns='jabber:client' type='chat'><body>x</body>\
            <no-permanent-store xmlns='urn:xmpp:hints'/></message>");
        assert!(!archivable(&npst) && offline_storable(&npst));
    }

    #[test]
    fn carbons_wrap_from_the_bare_jid() {
        let orig = m("<message xmlns='jabber:client' type='chat' from='b@h/x' to='a@h/y'><body>hi</body></message>");
        let c = carbon("received", &orig, "a@h", "a@h/z");
        assert_eq!(c.attr("from"), Some("a@h"));
        let inner = c
            .get_child("received", NS_CARBONS)
            .and_then(|r| r.get_child("forwarded", NS_FORWARD))
            .and_then(|f| f.get_child("message", NS_CLIENT))
            .unwrap();
        assert_eq!(inner.attr("from"), Some("b@h/x"));
        assert!(is_wrapper(&c));
    }
}
