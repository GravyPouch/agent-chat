//! XEP-0313 queries on the account's archive. Arrives in M3.

use tokio_xmpp::minidom::Element;

use super::UserActor;
use crate::stanza::Condition;

pub const NS_MAM: &str = "urn:xmpp:mam:2";

impl UserActor {
    pub(super) async fn mam_query(&mut self, el: &Element, _query: &Element, _owner: bool) {
        self.reply_error(el, Condition::ServiceUnavailable).await;
    }
}
