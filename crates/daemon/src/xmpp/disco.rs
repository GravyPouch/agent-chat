//! Our service discovery identity (XEP-0030) and entity capabilities (XEP-0115).
//!
//! Advertising `…+notify` features is what makes the server push OMEMO
//! device-list updates to us (XEP-0163 filtered notifications).
//!
//! Agents advertise the identity `client/bot`, people `client/pc`. Others
//! learn which from the caps hash in our presence; we learn theirs the same
//! way ([`is_agent`], [`verify_caps`]).

use std::collections::BTreeSet;

use agent_chat_proto::model::AccountKind;
use xmpp_parsers::caps::{self, Caps};
use xmpp_parsers::disco::{DiscoInfoResult, Identity};
use xmpp_parsers::hashes::Algo;

pub const CAPS_NODE: &str = "urn:x-agent-chat:client";

pub const FEATURES: &[&str] = &[
    "http://jabber.org/protocol/caps",
    "http://jabber.org/protocol/chatstates",
    "http://jabber.org/protocol/disco#info",
    "http://jabber.org/protocol/muc",
    "urn:xmpp:ping",
    "urn:xmpp:carbons:2",
    "urn:xmpp:mam:2",
    "urn:xmpp:sid:0",
    "urn:xmpp:omemo:2",
    "urn:xmpp:omemo:2:devices+notify",
    "eu.siacs.conversations.axolotl",
    "eu.siacs.conversations.axolotl.devicelist+notify",
];

fn base_disco(kind: AccountKind) -> DiscoInfoResult {
    let type_ = match kind {
        AccountKind::Person => "pc",
        AccountKind::Agent => "bot",
    };
    DiscoInfoResult {
        node: None,
        identities: vec![Identity::new("client", type_, "en", "agent-chat")],
        features: FEATURES.iter().map(|f| f.to_string()).collect::<BTreeSet<_>>(),
        extensions: vec![],
    }
}

/// Answer for a disco#info query on `node` (our caps node is echoed back).
pub fn our_disco(node: Option<String>, kind: AccountKind) -> DiscoInfoResult {
    let mut d = base_disco(kind);
    d.node = node;
    d
}

pub fn caps(kind: AccountKind) -> Caps {
    let data = caps::compute_disco(&base_disco(kind));
    let hash = caps::hash_caps(&data, Algo::Sha_1).expect("sha-1 is supported");
    Caps::new(CAPS_NODE, hash)
}

/// Does this disco#info describe an automated client (XEP-0030 `client/bot`)?
pub fn is_agent(info: &DiscoInfoResult) -> bool {
    info.identities
        .iter()
        .any(|i| i.category == "client" && i.type_ == "bot")
}

/// The disco#info node to ask for a caps hash (XEP-0115 `node#ver`).
pub fn caps_node(c: &Caps) -> String {
    format!("{}#{}", c.node, base64_ver(c))
}

/// Cache key for a caps hash: the same hash means the same client identity.
pub fn caps_key(c: &Caps) -> String {
    format!("{:?}:{}", c.hash, base64_ver(c))
}

fn base64_ver(c: &Caps) -> String {
    xmpp_parsers::hashes::Hash::new(c.hash.clone(), c.ver.clone()).to_base64()
}

/// XEP-0115 §5.4: only trust (and cache) a disco#info result for a caps
/// hash if it really hashes to that value.
pub fn verify_caps(c: &Caps, info: &DiscoInfoResult) -> bool {
    let mut info = info.clone();
    info.node = None;
    let data = caps::compute_disco(&info);
    caps::hash_caps(&data, c.hash.clone()).is_ok_and(|h| h.hash == c.ver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_xmpp::minidom::Element;

    #[test]
    fn caps_hash_is_stable() {
        let a = super::caps(AccountKind::Person);
        let b = super::caps(AccountKind::Person);
        assert_eq!(a.ver, b.ver);
        assert_eq!(a.ver.len(), 20);
    }

    #[test]
    fn agents_and_people_advertise_different_identities() {
        assert!(is_agent(&our_disco(None, AccountKind::Agent)));
        assert!(!is_agent(&our_disco(None, AccountKind::Person)));
        assert_ne!(caps(AccountKind::Agent).ver, caps(AccountKind::Person).ver);
        // Each hash verifies against its own identity and not the other one.
        for kind in [AccountKind::Agent, AccountKind::Person] {
            let c = caps(kind);
            assert!(verify_caps(&c, &our_disco(Some(caps_node(&c)), kind)));
        }
        assert!(!verify_caps(
            &caps(AccountKind::Agent),
            &our_disco(None, AccountKind::Person)
        ));
    }

    #[test]
    fn parses_other_clients() {
        // A bot identity among others, as another client would send it.
        let el: Element = "<query xmlns='http://jabber.org/protocol/disco#info'>\
              <identity category='client' type='bot' name='helper'/>\
              <identity category='client' type='pc' name='helper'/>\
              <feature var='http://jabber.org/protocol/disco#info'/></query>"
            .parse()
            .unwrap();
        assert!(is_agent(&DiscoInfoResult::try_from(el).unwrap()));
        let el: Element = "<query xmlns='http://jabber.org/protocol/disco#info'>\
              <identity category='client' type='phone' name='Conversations'/>\
              <feature var='http://jabber.org/protocol/disco#info'/></query>"
            .parse()
            .unwrap();
        assert!(!is_agent(&DiscoInfoResult::try_from(el).unwrap()));
    }

    #[test]
    fn caps_from_presence() {
        // XEP-0115 §5.2 example (Exodus): the published hash must verify.
        let el: Element = "<c xmlns='http://jabber.org/protocol/caps' hash='sha-1' \
              node='http://code.google.com/p/exodus' ver='QgayPKawpkPSDYmwT/WM94uAlu0='/>"
            .parse()
            .unwrap();
        let c = Caps::try_from(el).unwrap();
        assert_eq!(
            caps_node(&c),
            "http://code.google.com/p/exodus#QgayPKawpkPSDYmwT/WM94uAlu0="
        );
        assert_eq!(caps_key(&c), "Sha_1:QgayPKawpkPSDYmwT/WM94uAlu0=");
        let info: Element = "<query xmlns='http://jabber.org/protocol/disco#info' \
              node='http://code.google.com/p/exodus#QgayPKawpkPSDYmwT/WM94uAlu0='>\
              <identity category='client' name='Exodus 0.9.1' type='pc'/>\
              <feature var='http://jabber.org/protocol/caps'/>\
              <feature var='http://jabber.org/protocol/disco#info'/>\
              <feature var='http://jabber.org/protocol/disco#items'/>\
              <feature var='http://jabber.org/protocol/muc'/></query>"
            .parse()
            .unwrap();
        let info = DiscoInfoResult::try_from(info).unwrap();
        assert!(verify_caps(&c, &info));
        assert!(!is_agent(&info));
        // A forged answer (claims to be a bot) doesn't match the hash.
        let mut forged = info.clone();
        forged.identities[0].type_ = "bot".into();
        assert!(!verify_caps(&c, &forged));
    }
}
