//! XEP-0115 entity capabilities (§5.6 "Caps"): the server learns what each
//! resource supports, so PEP knows which nodes it `+notify`s.
//!
//! An unknown verification string triggers a disco#info from the account's
//! bare JID to the resource. The answer is hashed; a matching hash goes into
//! the global cache (memory and store), a mismatch is used for that
//! resource only.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::disco::DiscoInfoResult;
use xmpp_parsers::hashes::Algo;
use xmpp_parsers::ns;

use super::{Pending, UserActor, server_id};
use crate::stanza::{self, NS_CLIENT};

/// Verify a disco#info answer against a caps `ver` (SHA-1 only, as XEP-0115
/// clients use). Returns the features if the hash matches.
pub fn verify(query: &Element, ver: &str) -> Result<Vec<String>, Vec<String>> {
    let Ok(info) = DiscoInfoResult::try_from(query.clone()) else {
        return Err(Vec::new());
    };
    let features: Vec<String> = info.features.iter().map(|f| f.to_string()).collect();
    let data = xmpp_parsers::caps::compute_disco(&info);
    match xmpp_parsers::caps::hash_caps(&data, Algo::Sha_1) {
        Ok(h) if B64.encode(&h.hash) == ver => Ok(features),
        _ => Err(features),
    }
}

impl UserActor {
    /// A resource's presence carried `<c ver hash>`.
    pub(super) async fn learn_caps(&mut self, resource: &str, ver: String, hash: String, node: Option<String>) {
        let Some(s) = self.sessions.get_mut(resource) else {
            return;
        };
        if s.caps_ver.as_deref() == Some(ver.as_str()) {
            return;
        }
        s.caps_ver = Some(ver.clone());
        if let Some(features) = self.router.caps_lookup(&ver).await {
            self.set_features(resource, features).await;
            return;
        }
        if hash != "sha-1" {
            return;
        }
        if self
            .pending
            .values()
            .any(|p| matches!(p, Pending::Caps { resource: r, ver: v, .. } if r == resource && *v == ver))
        {
            return;
        }
        let id = server_id("caps");
        let full = self.sessions[resource].full.clone();
        let mut iq = stanza::element(
            "iq",
            NS_CLIENT,
            &[("type", "get"), ("id", &id), ("from", &self.bare_str), ("to", &full)],
        );
        let mut q = Element::bare("query", ns::DISCO_INFO);
        if let Some(n) = node {
            stanza::set_attr(&mut q, "node", format!("{n}#{ver}"));
        }
        iq.append_child(q);
        self.pending.insert(
            id,
            Pending::Caps {
                resource: resource.to_owned(),
                ver,
            },
        );
        self.send(resource, iq);
    }

    /// Caps are learned after the resource's presence was processed, so it
    /// is available here: send it the last PEP items it +notifies.
    async fn set_features(&mut self, resource: &str, features: Arc<Vec<String>>) {
        if let Some(s) = self.sessions.get_mut(resource) {
            s.features = Some(features);
        }
        if self.sessions.get(resource).is_some_and(|s| s.available()) {
            self.request_last_items(resource).await;
        }
    }

    /// A result or error from one of our resources to our bare JID: the
    /// answer to a request the server sent.
    pub(super) async fn iq_response(&mut self, resource: &str, el: Element) {
        let Some(id) = el.attr("id") else { return };
        let Some(pending) = self.pending.remove(id) else { return };
        match pending {
            Pending::Caps { resource: r, ver, .. } => {
                if r != resource || stanza::stanza_type(&el) != "result" {
                    return;
                }
                let Some(query) = el.get_child("query", ns::DISCO_INFO) else {
                    return;
                };
                match verify(query, &ver) {
                    Ok(features) => {
                        let features = self.router.caps_store(&ver, features).await;
                        // Every resource with the same ver benefits.
                        let same: Vec<String> = self
                            .sessions
                            .iter()
                            .filter(|(_, s)| s.caps_ver.as_deref() == Some(ver.as_str()) && s.features.is_none())
                            .map(|(r, _)| r.clone())
                            .collect();
                        for r in same {
                            self.set_features(&r, features.clone()).await;
                        }
                    }
                    Err(features) => {
                        tracing::debug!(account = %self.bare, "caps hash mismatch; not cached");
                        self.set_features(resource, Arc::new(features)).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// XEP-0115 §5.2 simple example.
    #[test]
    fn verifies_the_xep_example() {
        let q: Element = "<query xmlns='http://jabber.org/protocol/disco#info'>\
            <identity category='client' name='Exodus 0.9.1' type='pc'/>\
            <feature var='http://jabber.org/protocol/caps'/>\
            <feature var='http://jabber.org/protocol/disco#info'/>\
            <feature var='http://jabber.org/protocol/disco#items'/>\
            <feature var='http://jabber.org/protocol/muc'/></query>"
            .parse()
            .unwrap();
        let f = verify(&q, "QgayPKawpkPSDYmwT/WM94uAlu0=").unwrap();
        assert!(f.iter().any(|v| v == "http://jabber.org/protocol/muc"));
        assert!(verify(&q, "AAAAAAAAAAAAAAAAAAAAAAAAAAA=").is_err());
    }
}
