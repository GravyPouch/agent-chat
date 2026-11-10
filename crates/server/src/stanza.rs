//! Helpers for raw stanzas. Stanzas are routed as `minidom::Element` so
//! payloads we don't model (OMEMO, new XEPs) pass through untouched (§5.4).

use tokio_xmpp::minidom::Element;
use tokio_xmpp::minidom::rxml::{Namespace, NcName};
use xmpp_parsers::ns;

pub const NS_CLIENT: &str = ns::JABBER_CLIENT;
pub const NS_SESSION: &str = "urn:ietf:params:xml:ns:xmpp-session";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Message,
    Presence,
    Iq,
}

impl Kind {
    pub fn of(el: &Element) -> Option<Kind> {
        if el.ns() != NS_CLIENT {
            return None;
        }
        match el.name() {
            "message" => Some(Kind::Message),
            "presence" => Some(Kind::Presence),
            "iq" => Some(Kind::Iq),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Message => "message",
            Kind::Presence => "presence",
            Kind::Iq => "iq",
        }
    }
}

/// The stanza's `type`, with the RFC defaults (`normal` for messages,
/// `available` for presence).
pub fn stanza_type(el: &Element) -> &str {
    match (el.attr("type"), el.name()) {
        (Some(t), _) => t,
        (None, "message") => "normal",
        (None, "presence") => "available",
        (None, _) => "",
    }
}

fn ncname(name: &str) -> NcName {
    name.try_into().expect("valid attribute name")
}

pub fn set_attr(el: &mut Element, name: &str, value: impl Into<String>) {
    el.set_attr(Namespace::NONE, ncname(name), value.into());
}

pub fn remove_attr(el: &mut Element, name: &str) {
    el.attrs_mut().remove(&Namespace::NONE, name);
}

/// Build an element with plain attributes.
pub fn element(name: &str, ns: &str, attrs: &[(&str, &str)]) -> Element {
    let mut b = Element::builder(name, ns);
    for (k, v) in attrs {
        b = b.attr(ncname(k), *v);
    }
    b.build()
}

/// `<iq type='result'>` answering `request`, optionally with a payload.
pub fn iq_result(request: &Element, from: Option<&str>, payload: Option<Element>) -> Element {
    let mut iq = element("iq", NS_CLIENT, &[("type", "result")]);
    if let Some(id) = request.attr("id") {
        set_attr(&mut iq, "id", id);
    }
    if let Some(to) = request.attr("from") {
        set_attr(&mut iq, "to", to);
    }
    if let Some(from) = from.or(request.attr("to")) {
        set_attr(&mut iq, "from", from);
    }
    if let Some(p) = payload {
        iq.append_child(p);
    }
    iq
}

/// Stanza error conditions we use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    BadRequest,
    FeatureNotImplemented,
    JidMalformed,
    NotAllowed,
    RemoteServerNotFound,
    ResourceConstraint,
    ServiceUnavailable,
}

impl Condition {
    pub fn name(self) -> &'static str {
        match self {
            Condition::BadRequest => "bad-request",
            Condition::FeatureNotImplemented => "feature-not-implemented",
            Condition::JidMalformed => "jid-malformed",
            Condition::NotAllowed => "not-allowed",
            Condition::RemoteServerNotFound => "remote-server-not-found",
            Condition::ResourceConstraint => "resource-constraint",
            Condition::ServiceUnavailable => "service-unavailable",
        }
    }

    /// RFC 6120 §8.3.2 error types.
    pub fn error_type(self) -> &'static str {
        match self {
            Condition::BadRequest | Condition::JidMalformed => "modify",
            Condition::ResourceConstraint => "wait",
            _ => "cancel",
        }
    }
}

/// Whether we may answer this stanza with an error: never for errors,
/// never for IQ results (RFC 6120 §8.3.1).
pub fn may_bounce(el: &Element) -> bool {
    let t = stanza_type(el);
    !(t == "error" || (el.name() == "iq" && t == "result"))
}

/// An error reply to `original`: addresses swapped, same id, no payload.
/// `from` overrides the error's sender (default: the original `to`).
/// `None` if the original must not be answered.
pub fn error_reply(original: &Element, cond: Condition, from: Option<&str>) -> Option<Element> {
    if !may_bounce(original) {
        return None;
    }
    let mut reply = element(original.name(), NS_CLIENT, &[("type", "error")]);
    if let Some(id) = original.attr("id") {
        set_attr(&mut reply, "id", id);
    }
    if let Some(to) = original.attr("from") {
        set_attr(&mut reply, "to", to);
    }
    if let Some(f) = from.or(original.attr("to")) {
        set_attr(&mut reply, "from", f);
    }
    let mut error = element("error", NS_CLIENT, &[("type", cond.error_type())]);
    error.append_child(Element::bare(cond.name(), ns::XMPP_STANZAS));
    reply.append_child(error);
    Some(reply)
}

/// Remove `<stanza-id xmlns='urn:xmpp:sid:0'>` children whose `by` we would
/// stamp ourselves, so clients can't spoof archive ids (§5.4 step 9).
pub fn strip_our_stanza_ids(el: &mut Element, is_ours: impl Fn(&str) -> bool) -> usize {
    let spoofed = |c: &Element| c.is("stanza-id", ns::SID) && c.attr("by").is_some_and(&is_ours);
    if !el.children().any(spoofed) {
        return 0;
    }
    let nodes = el.take_nodes();
    let mut removed = 0;
    for node in nodes {
        match node {
            tokio_xmpp::minidom::Node::Element(c) if spoofed(&c) => removed += 1,
            other => el.append_node(other),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Element {
        s.parse().unwrap()
    }

    #[test]
    fn kinds_and_types() {
        let m = parse("<message xmlns='jabber:client'/>");
        assert_eq!(Kind::of(&m), Some(Kind::Message));
        assert_eq!(stanza_type(&m), "normal");
        assert_eq!(Kind::of(&parse("<message xmlns='urn:other'/>")), None);
        assert_eq!(stanza_type(&parse("<presence xmlns='jabber:client'/>")), "available");
    }

    #[test]
    fn error_replies() {
        let iq = parse("<iq xmlns='jabber:client' type='get' id='1' from='a@h/r' to='h'><ping xmlns='urn:xmpp:ping'/></iq>");
        let e = error_reply(&iq, Condition::ServiceUnavailable, None).unwrap();
        assert_eq!(e.attr("type"), Some("error"));
        assert_eq!(e.attr("to"), Some("a@h/r"));
        assert_eq!(e.attr("from"), Some("h"));
        assert_eq!(e.attr("id"), Some("1"));
        let err = e.get_child("error", NS_CLIENT).unwrap();
        assert_eq!(err.attr("type"), Some("cancel"));
        assert!(err.has_child("service-unavailable", ns::XMPP_STANZAS));
        assert!(e.get_child("ping", "urn:xmpp:ping").is_none());

        let result = parse("<iq xmlns='jabber:client' type='result' id='1'/>");
        assert!(error_reply(&result, Condition::BadRequest, None).is_none());
        let error = parse("<message xmlns='jabber:client' type='error'/>");
        assert!(error_reply(&error, Condition::BadRequest, None).is_none());
    }

    #[test]
    fn strips_spoofed_stanza_ids() {
        let mut m = parse(
            "<message xmlns='jabber:client'><body>hi</body>\
             <stanza-id xmlns='urn:xmpp:sid:0' by='alice@hub.local' id='evil'/>\
             <stanza-id xmlns='urn:xmpp:sid:0' by='other.example' id='ok'/></message>",
        );
        assert_eq!(strip_our_stanza_ids(&mut m, |by| by.ends_with("hub.local")), 1);
        let ids: Vec<_> = m.children().filter(|c| c.is("stanza-id", ns::SID)).filter_map(|c| c.attr("id")).collect();
        assert_eq!(ids, ["ok"]);
        assert!(m.has_child("body", NS_CLIENT));
    }

    #[test]
    fn attributes() {
        let mut m = element("message", NS_CLIENT, &[("to", "a@b")]);
        set_attr(&mut m, "from", "c@d/e");
        set_attr(&mut m, "to", "x@y");
        assert_eq!(m.attr("to"), Some("x@y"));
        remove_attr(&mut m, "to");
        assert_eq!(m.attr("to"), None);
        assert_eq!(m.attr("from"), Some("c@d/e"));
    }
}
