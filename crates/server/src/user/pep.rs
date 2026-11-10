//! PEP (XEP-0163) in the owner's actor. Arrives in M4; until then the
//! account answers pubsub requests with `service-unavailable`.

use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::Jid;
use xmpp_parsers::ns;

use super::UserActor;
use crate::stanza::Condition;

pub const NS_PUBSUB: &str = "http://jabber.org/protocol/pubsub";
pub const NS_PUBSUB_OWNER: &str = "http://jabber.org/protocol/pubsub#owner";

/// pubsub features advertised on account JIDs.
pub const FEATURES: &[&str] = &[];

/// A notification from a PEP service to a subscriber's actor.
#[derive(Debug, Clone)]
pub struct PepEvent {
    pub node: String,
    /// `<message type='headline'>` with the `<event/>`, `to` the bare JID.
    pub message: Element,
}

/// "Send `resource` the last items it +notifies."
#[derive(Debug, Clone)]
pub struct LastItemsRequest {
    pub to: String,
    pub nodes: Vec<String>,
}

impl UserActor {
    pub(super) fn pep_event(&mut self, _ev: PepEvent) {}

    pub(super) async fn pep_last_items(&mut self, _req: LastItemsRequest) {}

    pub(super) async fn request_last_items(&mut self, _resource: &str) {}

    pub(super) async fn pubsub_iq(&mut self, el: &Element, _payload: &Element, _owner: bool) {
        self.reply_error(el, Condition::ServiceUnavailable).await;
    }

    pub(super) async fn pep_node_info(&mut self, _req: &Element, _node: &str) -> Option<Element> {
        None
    }

    pub(super) async fn pep_disco_items(&mut self, _requester: Option<&Jid>) -> Element {
        Element::bare("query", ns::DISCO_ITEMS)
    }
}
