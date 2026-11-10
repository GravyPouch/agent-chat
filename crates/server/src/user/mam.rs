//! The account's archive (XEP-0313) and stanza-ids (XEP-0359): archiving
//! in the actor, and queries from the owner.

use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::Jid;
use xmpp_parsers::ns;

use super::message::{archivable, remove_children};
use super::{UserActor, jid_attr};
use crate::mam;
use crate::stanza::{self, Condition};
use crate::store::{ArchiveQuery, ArchiveRow, Direction};
use crate::util::now_ms;

impl UserActor {
    /// Stamp and archive a message delivered to this account. Returns the
    /// archive id, which is also its stanza-id (offline copies share it).
    pub(super) async fn archive_incoming(&mut self, el: &mut Element) -> Option<String> {
        if !self.router.settings().archive_enabled || !archivable(el) {
            return None;
        }
        let me = self.bare_str.clone();
        // Only we may claim our `by` (§5.4 step 9 strips client copies too).
        remove_children(el, |c| c.is("stanza-id", ns::SID) && c.attr("by") == Some(me.as_str()));
        let id = mam::new_id();
        el.append_child(mam::stanza_id(&me, &id));
        let from = jid_attr(el, "from");
        let row = ArchiveRow {
            id: id.clone(),
            archive_jid: me,
            ts_ms: now_ms(),
            with_jid: from.as_ref().map(|f| f.to_bare().to_string()).unwrap_or_default(),
            direction: Direction::In,
            origin_id: mam::origin_id(el),
            from_jid: from.map(|f| f.to_string()).unwrap_or_default(),
            stanza: String::from(&*el),
        };
        match self.router.store().archive_insert(row).await {
            Ok(()) => Some(id),
            Err(e) => {
                tracing::error!(account = %self.bare, "archiving failed: {e:#}");
                None
            }
        }
    }

    /// Archive a message this account sends. Returns the archive id, which
    /// the sent carbons carry as stanza-id; the routed copy doesn't.
    pub(super) async fn archive_outgoing(&mut self, el: &Element, to: &Jid) -> Option<String> {
        if !self.router.settings().archive_enabled || !archivable(el) {
            return None;
        }
        let id = mam::new_id();
        let mut copy = el.clone();
        copy.append_child(mam::stanza_id(&self.bare_str, &id));
        let row = ArchiveRow {
            id: id.clone(),
            archive_jid: self.bare_str.clone(),
            ts_ms: now_ms(),
            with_jid: to.to_bare().to_string(),
            direction: Direction::Out,
            origin_id: mam::origin_id(el),
            from_jid: el.attr("from").unwrap_or_default().to_owned(),
            stanza: String::from(&copy),
        };
        match self.router.store().archive_insert(row).await {
            Ok(()) => Some(id),
            Err(e) => {
                tracing::error!(account = %self.bare, "archiving failed: {e:#}");
                None
            }
        }
    }

    /// `<query xmlns='urn:xmpp:mam:2'>` (and prefs, metadata) on our archive.
    pub(super) async fn mam_query(&mut self, el: &Element, payload: &Element, owner: bool) {
        if !owner {
            self.reply_error(el, Condition::Forbidden).await;
            return;
        }
        if !self.router.settings().archive_enabled {
            self.reply_error(el, Condition::ServiceUnavailable).await;
            return;
        }
        let ty = stanza::stanza_type(el).to_owned();
        match (ty.as_str(), payload.name()) {
            ("get", "query") => self.reply_result(el, Some(mam::form())).await,
            (_, "prefs") => self.reply_result(el, Some(mam::prefs())).await,
            ("get", "metadata") => {
                let store = self.router.store().clone();
                let base = ArchiveQuery {
                    archive_jid: self.bare_str.clone(),
                    max: 1,
                    ..Default::default()
                };
                let first = store
                    .archive_query(base.clone())
                    .await
                    .ok()
                    .and_then(|p| p.rows.into_iter().next());
                let last = store
                    .archive_query(ArchiveQuery {
                        rsm_before: Some(String::new()),
                        ..base
                    })
                    .await
                    .ok()
                    .and_then(|p| p.rows.into_iter().next());
                self.reply_result(el, Some(mam::metadata(first.as_ref(), last.as_ref())))
                    .await;
            }
            ("set", "query") => self.run_query(el, payload).await,
            _ => self.reply_error(el, Condition::BadRequest).await,
        }
    }

    async fn run_query(&mut self, el: &Element, payload: &Element) {
        let max_page = self.router.settings().mam_max_page;
        let req = match mam::parse(payload, &self.bare_str, max_page) {
            Ok(r) => r,
            Err(c) => {
                self.reply_error(el, c).await;
                return;
            }
        };
        let page = match self.router.store().archive_query(req.query).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(account = %self.bare, "MAM query failed: {e:#}");
                self.reply_error(el, Condition::InternalServerError).await;
                return;
            }
        };
        if page.id_not_found {
            self.reply_error(el, Condition::ItemNotFound).await;
            return;
        }
        let Some(to) = el.attr("from").map(str::to_owned) else {
            return;
        };
        let Some(resource) = jid_attr(el, "from").and_then(|j| j.resource().map(|r| r.as_str().to_owned())) else {
            return;
        };
        let mut rows: Vec<&ArchiveRow> = page.rows.iter().collect();
        if req.flip {
            rows.reverse();
        }
        // Results strictly before the <fin/>: one outbox, in order.
        for row in &rows {
            if let Some(m) = mam::result_message(&to, Some(&self.bare_str), req.queryid.as_deref(), row) {
                self.send(&resource, m);
            }
        }
        let fin = mam::fin(&page, &rows);
        self.reply_result(el, Some(fin)).await;
    }
}
