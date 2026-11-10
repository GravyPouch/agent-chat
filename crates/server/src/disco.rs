//! Stanzas to the hub's own domain: XEP-0199 ping, XEP-0030 disco and the
//! legacy session IQ (§5.7 "Other services").

use tokio_xmpp::minidom::Element;
use xmpp_parsers::ns;

use crate::stanza::{self, Condition, Kind, NS_SESSION};

/// Features the domain advertises. Grows with each milestone.
pub const DOMAIN_FEATURES: &[&str] = &[ns::DISCO_INFO, ns::DISCO_ITEMS, ns::PING];

/// The reply to a stanza addressed to the domain, if any.
pub fn handle_domain_stanza(domain: &str, el: &Element) -> Option<Element> {
    if Kind::of(el) != Some(Kind::Iq) {
        // Messages and presence to the server are dropped.
        return None;
    }
    let ty = stanza::stanza_type(el);
    if !matches!(ty, "get" | "set") {
        return None;
    }
    let payload = el.children().next();
    let reply = match (ty, payload) {
        ("get", Some(p)) if p.is("ping", ns::PING) => stanza::iq_result(el, Some(domain), None),
        ("set", Some(p)) if p.is("session", NS_SESSION) => stanza::iq_result(el, Some(domain), None),
        ("get", Some(p)) if p.is("query", ns::DISCO_INFO) => {
            if p.attr("node").is_some() {
                return stanza::error_reply(el, Condition::ServiceUnavailable, Some(domain));
            }
            stanza::iq_result(el, Some(domain), Some(disco_info()))
        }
        ("get", Some(p)) if p.is("query", ns::DISCO_ITEMS) => {
            if p.attr("node").is_some() {
                return stanza::error_reply(el, Condition::ServiceUnavailable, Some(domain));
            }
            // conference.<domain> is listed from M5.
            stanza::iq_result(el, Some(domain), Some(Element::bare("query", ns::DISCO_ITEMS)))
        }
        _ => return stanza::error_reply(el, Condition::ServiceUnavailable, Some(domain)),
    };
    Some(reply)
}

fn disco_info() -> Element {
    let mut q = Element::bare("query", ns::DISCO_INFO);
    q.append_child(stanza::element(
        "identity",
        ns::DISCO_INFO,
        &[("category", "server"), ("type", "im"), ("name", "agent-chat hub")],
    ));
    for f in DOMAIN_FEATURES {
        q.append_child(stanza::element("feature", ns::DISCO_INFO, &[("var", f)]));
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;
    use xmpp_parsers::disco::DiscoInfoResult;

    fn iq(payload: &str, ty: &str) -> Element {
        format!("<iq xmlns='jabber:client' type='{ty}' id='q1' from='a@hub.local/r' to='hub.local'>{payload}</iq>")
            .parse()
            .unwrap()
    }

    #[test]
    fn ping_and_session() {
        let r = handle_domain_stanza("hub.local", &iq("<ping xmlns='urn:xmpp:ping'/>", "get")).unwrap();
        assert_eq!((r.attr("type"), r.attr("id"), r.attr("to")), (Some("result"), Some("q1"), Some("a@hub.local/r")));
        let r = handle_domain_stanza("hub.local", &iq("<session xmlns='urn:ietf:params:xml:ns:xmpp-session'/>", "set")).unwrap();
        assert_eq!(r.attr("type"), Some("result"));
    }

    #[test]
    fn disco_info_parses() {
        let r = handle_domain_stanza("hub.local", &iq("<query xmlns='http://jabber.org/protocol/disco#info'/>", "get")).unwrap();
        let info = DiscoInfoResult::try_from(r.get_child("query", ns::DISCO_INFO).unwrap().clone()).unwrap();
        assert_eq!(info.identities[0].category, "server");
        assert!(info.features.iter().any(|f| f == ns::PING));
    }

    #[test]
    fn unknown_requests_get_service_unavailable() {
        let r = handle_domain_stanza("hub.local", &iq("<query xmlns='jabber:iq:version'/>", "get")).unwrap();
        assert_eq!(r.attr("type"), Some("error"));
        assert!(r.get_child("error", "jabber:client").unwrap().has_child("service-unavailable", ns::XMPP_STANZAS));
        assert!(handle_domain_stanza("hub.local", &iq("", "result")).is_none());
        let msg: Element = "<message xmlns='jabber:client' to='hub.local'><body>x</body></message>".parse().unwrap();
        assert!(handle_domain_stanza("hub.local", &msg).is_none());
    }
}
