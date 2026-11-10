//! Our service discovery identity (XEP-0030) and entity capabilities (XEP-0115).
//!
//! Advertising `…+notify` features is what makes the server push OMEMO
//! device-list updates to us (XEP-0163 filtered notifications).

use std::collections::BTreeSet;

use xmpp_parsers::caps::{self, Caps};
use xmpp_parsers::disco::{DiscoInfoResult, Identity};
use xmpp_parsers::hashes::Algo;

pub const CAPS_NODE: &str = "urn:x-agent-chat:client";

pub const FEATURES: &[&str] = &[
    "http://jabber.org/protocol/caps",
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

fn base_disco() -> DiscoInfoResult {
    DiscoInfoResult {
        node: None,
        identities: vec![Identity::new("client", "pc", "en", "agent-chat")],
        features: FEATURES.iter().map(|f| f.to_string()).collect::<BTreeSet<_>>(),
        extensions: vec![],
    }
}

/// Answer for a disco#info query on `node` (our caps node is echoed back).
pub fn our_disco(node: Option<String>) -> DiscoInfoResult {
    let mut d = base_disco();
    d.node = node;
    d
}

pub fn caps() -> Caps {
    let data = caps::compute_disco(&base_disco());
    let hash = caps::hash_caps(&data, Algo::Sha_1).expect("sha-1 is supported");
    Caps::new(CAPS_NODE, hash)
}

#[cfg(test)]
mod tests {
    #[test]
    fn caps_hash_is_stable() {
        let a = super::caps();
        let b = super::caps();
        assert_eq!(a.ver, b.ver);
        assert_eq!(a.ver.len(), 20);
    }
}
