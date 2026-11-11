//! IQs in the account's actor: requests the server answers for the account
//! (roster, carbons, blocking, vcard-temp, private XML, disco, ping, and
//! later MAM and PEP), responses to the server's own requests, and IQs
//! between resources.

use std::collections::BTreeSet;

use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::ns;

use super::presence::presence;
use super::roster;
use super::{UserActor, blocked_error, jid_attr, server_id};
use crate::disco;
use crate::stanza::{self, Condition, NS_CLIENT};
use crate::store::RosterItem;

pub const NS_BLOCKING: &str = "urn:xmpp:blocking";
const NS_VCARD: &str = "vcard-temp";
const NS_PRIVATE: &str = "jabber:iq:private";
/// Largest private XML or vCard we keep.
const MAX_PRIVATE_BYTES: usize = 64 * 1024;

impl UserActor {
    pub(super) async fn outbound_iq(&mut self, resource: &str, el: Element) {
        let ty = stanza::stanza_type(&el).to_owned();
        let to = jid_attr(&el, "to");
        let to_us = to.as_ref().is_none_or(|t| self.is_own(t) && t.resource().is_none());
        let to_domain = to
            .as_ref()
            .is_some_and(|t| t.node().is_none() && t.domain().as_str() == self.router.domain());
        match ty.as_str() {
            "result" | "error" => {
                if to_us || to_domain {
                    self.iq_response(resource, el).await;
                } else if to.as_ref().is_some_and(|t| self.is_own(t)) {
                    self.inbound_iq(el).await;
                } else {
                    self.route(el).await;
                }
            }
            "get" | "set" => {
                if to_us {
                    self.account_iq(el).await;
                } else if to_domain {
                    self.domain_iq(resource, el).await;
                } else if to.as_ref().is_some_and(|t| self.is_own(t)) {
                    // Another resource of ours.
                    self.inbound_iq(el).await;
                } else if let Some(t) = &to
                    && self.is_blocked(t).await
                {
                    if let Some(e) = blocked_error(&el, &t.to_string()) {
                        self.send(resource, e);
                    }
                } else {
                    self.route(el).await;
                }
            }
            _ => {}
        }
    }

    pub(super) async fn inbound_iq(&mut self, el: Element) {
        let ty = stanza::stanza_type(&el).to_owned();
        let to = jid_attr(&el, "to");
        match to.as_ref().and_then(|t| t.resource()).map(|r| r.as_str().to_owned()) {
            Some(r) => {
                if self.sessions.contains_key(&r) {
                    self.send(&r, el);
                } else {
                    let from = to.map(|t| t.to_string());
                    self.router.bounce(&el, Condition::ServiceUnavailable, from.as_deref());
                }
            }
            None if matches!(ty.as_str(), "get" | "set") => self.account_iq(el).await,
            // Results and errors to our bare JID that we didn't ask for.
            None => {}
        }
    }

    /// Send a reply: straight to our own session, otherwise routed.
    pub(super) async fn respond(&mut self, reply: Element) {
        if let Some(to) = jid_attr(&reply, "to")
            && self.is_own(&to)
            && let Some(r) = to.resource()
        {
            let r = r.as_str().to_owned();
            self.send(&r, reply);
            return;
        }
        self.route(reply).await;
    }

    pub(super) async fn reply_result(&mut self, req: &Element, payload: Option<Element>) {
        let mut r = stanza::iq_result(req, Some(&self.reply_from(req)), payload);
        if req.attr("to").is_none() {
            // A request to "no one" is answered by no one (RFC 6120 §10.3.3).
            stanza::remove_attr(&mut r, "from");
        }
        self.respond(r).await;
    }

    pub(super) async fn reply_error(&mut self, req: &Element, cond: Condition) {
        if let Some(mut r) = stanza::error_reply(req, cond, Some(&self.reply_from(req))) {
            if req.attr("to").is_none() {
                stanza::remove_attr(&mut r, "from");
            }
            self.respond(r).await;
        }
    }

    /// IQs to the server domain that change session state.
    async fn domain_iq(&mut self, resource: &str, el: Element) {
        if let Some(p) = el.children().next()
            && p.ns() == super::message::NS_CARBONS
            && stanza::stanza_type(&el) == "set"
        {
            self.carbons(resource, &el, p.name() == "enable").await;
            return;
        }
        if let Some(p) = el.children().next()
            && p.is("query", crate::register::NS_REGISTER)
        {
            let p = p.clone();
            self.account_register(resource, &el, &p).await;
            return;
        }
        if let Some(reply) = disco::handle_domain_stanza(self.router.domain(), &self.router.components(), &el) {
            self.send(resource, reply);
        }
    }

    /// XEP-0077 on an authenticated stream: info, password change, removal.
    async fn account_register(&mut self, resource: &str, el: &Element, query: &Element) {
        let domain = self.router.domain().to_owned();
        let localpart = self.bare.node().map(|n| n.to_string()).unwrap_or_default();
        let reply_err =
            |cond: Condition, text: &str| stanza::error_reply_with(el, cond, Some(&domain), Some(text), None);
        if stanza::stanza_type(el) == "get" {
            let mut q = Element::bare("query", crate::register::NS_REGISTER);
            q.append_child(Element::bare("registered", crate::register::NS_REGISTER));
            let mut u = Element::bare("username", crate::register::NS_REGISTER);
            u.append_text(localpart.as_str());
            q.append_child(u);
            self.send(resource, stanza::iq_result(el, Some(&domain), Some(q)));
            return;
        }
        if query.has_child("remove", crate::register::NS_REGISTER) {
            match self.router.store().delete_account(&domain, &localpart).await {
                Ok(true) => {
                    tracing::info!(account = %self.bare, "account removed by its owner");
                    self.send(resource, stanza::iq_result(el, Some(&domain), None));
                    // Give the result a moment to go out, then close every session.
                    let router = self.router.clone();
                    let bare = self.bare.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        router.close_account(&bare, super::CloseReason::AccountDeleted);
                    });
                }
                _ => {
                    if let Some(e) = reply_err(Condition::InternalServerError, "The account could not be removed.") {
                        self.send(resource, e);
                    }
                }
            }
            return;
        }
        let Some(req) = crate::register::Registrar::parse_set(query, std::net::Ipv4Addr::LOCALHOST.into(), None) else {
            if let Some(e) = reply_err(Condition::BadRequest, "A username and a password are required.") {
                self.send(resource, e);
            }
            return;
        };
        if crate::accounts::normalize_username(&req.username).ok().as_deref() != Some(localpart.as_str()) {
            if let Some(e) = reply_err(Condition::NotAllowed, "You can only change your own password.") {
                self.send(resource, e);
            }
            return;
        }
        let s = self.router.settings();
        let changed = crate::accounts::set_password_min(
            self.router.store(),
            &domain,
            s.scram_iterations,
            &localpart,
            Some(req.password),
            s.min_password,
        )
        .await;
        let reply = match changed {
            Ok(_) => Some(stanza::iq_result(el, Some(&domain), None)),
            Err(e) => reply_err(Condition::NotAcceptable, &e.to_string()),
        };
        if let Some(r) = reply {
            self.send(resource, r);
        }
    }

    async fn carbons(&mut self, resource: &str, el: &Element, enable: bool) {
        if let Some(s) = self.sessions.get_mut(resource) {
            s.carbons = enable;
        }
        let mut r = stanza::iq_result(el, None, None);
        stanza::remove_attr(&mut r, "from");
        if el.attr("to").is_some() {
            stanza::set_attr(&mut r, "from", self.router.domain());
        }
        self.send(resource, r);
    }

    /// Requests to the account's bare JID (from the owner or anyone else).
    pub(super) async fn account_iq(&mut self, el: Element) {
        let ty = stanza::stanza_type(&el).to_owned();
        let from = jid_attr(&el, "from");
        let owner = from.as_ref().is_some_and(|f| self.is_own(f));
        let own_resource = from
            .as_ref()
            .filter(|_| owner)
            .and_then(|f| f.resource())
            .map(|r| r.as_str().to_owned());
        let Some(payload) = el.children().next().cloned() else {
            self.reply_error(&el, Condition::BadRequest).await;
            return;
        };
        let pns = payload.ns();
        match (ty.as_str(), payload.name(), pns.as_str()) {
            ("get", "ping", ns::PING) => self.reply_result(&el, None).await,
            ("get", "query", ns::DISCO_INFO) => self.disco_info(&el, &payload).await,
            ("get", "query", ns::DISCO_ITEMS) => self.disco_items(&el, &payload).await,
            (_, "query", ns::ROSTER) if owner => match ty.as_str() {
                "get" => {
                    if let Some(r) = &own_resource
                        && let Some(s) = self.sessions.get_mut(r)
                    {
                        s.interested = true;
                    }
                    let q = roster::query(self.roster().await.values());
                    self.reply_result(&el, Some(q)).await;
                }
                _ => self.roster_set(&el, &payload).await,
            },
            ("set", "enable" | "disable", super::message::NS_CARBONS) if owner => {
                if let Some(r) = own_resource {
                    self.carbons(&r, &el, payload.name() == "enable").await;
                }
            }
            ("get", "blocklist", NS_BLOCKING) if owner => {
                if let Some(r) = &own_resource
                    && let Some(s) = self.sessions.get_mut(r)
                {
                    s.blocklist_interest = true;
                }
                let mut list = Element::bare("blocklist", NS_BLOCKING);
                for j in self.blocklist().await.clone() {
                    list.append_child(stanza::element("item", NS_BLOCKING, &[("jid", &j)]));
                }
                self.reply_result(&el, Some(list)).await;
            }
            ("set", "block" | "unblock", NS_BLOCKING) if owner => self.block(&el, &payload).await,
            ("get", "vCard", NS_VCARD) => {
                let stored = self
                    .router
                    .store()
                    .private_get(&self.bare_str, "vcard")
                    .await
                    .ok()
                    .flatten();
                let card = stored
                    .and_then(|x| x.parse::<Element>().ok())
                    .unwrap_or_else(|| Element::bare("vCard", NS_VCARD));
                self.reply_result(&el, Some(card)).await;
            }
            ("set", "vCard", NS_VCARD) if owner => {
                let xml = String::from(&payload);
                if xml.len() > MAX_PRIVATE_BYTES {
                    self.reply_error(&el, Condition::NotAcceptable).await;
                    return;
                }
                let empty = payload.children().next().is_none();
                let value = (!empty).then_some(xml);
                match self.router.store().private_put(&self.bare_str, "vcard", value).await {
                    Ok(()) => self.reply_result(&el, None).await,
                    Err(_) => self.reply_error(&el, Condition::InternalServerError).await,
                }
            }
            (_, "query", NS_PRIVATE) if owner => self.private_xml(&el, &payload).await,
            (_, "query" | "prefs" | "metadata", crate::mam::NS_MAM) => self.mam_query(&el, &payload, owner).await,
            (_, "pubsub", super::pep::NS_PUBSUB) | (_, "pubsub", super::pep::NS_PUBSUB_OWNER) => {
                self.pubsub_iq(&el, &payload, owner).await;
            }
            (_, _, NS_PRIVATE | ns::ROSTER | NS_BLOCKING) => self.reply_error(&el, Condition::Forbidden).await,
            _ => self.reply_error(&el, Condition::ServiceUnavailable).await,
        }
    }

    /// RFC 6121 §2.3 (add or update) and §2.5 (remove).
    async fn roster_set(&mut self, el: &Element, query: &Element) {
        let Some(req) = roster::parse_set(query) else {
            self.reply_error(el, Condition::BadRequest).await;
            return;
        };
        if req.jid == self.bare_str {
            self.reply_error(el, Condition::NotAllowed).await;
            return;
        }
        if req.remove {
            let Some(item) = self.roster_item(&req.jid).await.filter(|i| !i.hidden) else {
                self.reply_error(el, Condition::ItemNotFound).await;
                return;
            };
            let me = self.bare_str.clone();
            if item.subscription.has_to() || item.ask_out {
                self.route(presence(Some("unsubscribe"), &me, &item.contact)).await;
            }
            if item.subscription.has_from() || item.pending_in {
                self.route(presence(Some("unsubscribed"), &me, &item.contact)).await;
                if item.subscription.has_from() {
                    let fulls: Vec<String> = self
                        .sessions
                        .values()
                        .filter(|s| s.available())
                        .map(|s| s.full.clone())
                        .collect();
                    for f in fulls {
                        self.route(presence(Some("unavailable"), &f, &item.contact)).await;
                    }
                }
            }
            self.drop_roster_item(&req.jid).await;
            self.push_roster(&item, true);
            self.reply_result(el, None).await;
            return;
        }
        let mut item = self
            .roster_item(&req.jid)
            .await
            .unwrap_or_else(|| RosterItem::new(&req.jid));
        let max = 2000;
        if self.roster().await.len() >= max && !self.roster().await.contains_key(&req.jid) {
            self.reply_error(el, Condition::ResourceConstraint).await;
            return;
        }
        item.name = req.name;
        item.groups = req.groups;
        item.hidden = false;
        self.save_roster_item(item.clone()).await;
        self.push_roster(&item, false);
        self.reply_result(el, None).await;
    }

    /// XEP-0191 block and unblock.
    async fn block(&mut self, el: &Element, payload: &Element) {
        let block = payload.name() == "block";
        let mut jids = Vec::new();
        for item in payload.children().filter(|c| c.is("item", NS_BLOCKING)) {
            match item.attr("jid").map(Jid::new) {
                Some(Ok(j)) => jids.push(j.to_string()),
                _ => {
                    self.reply_error(el, Condition::JidMalformed).await;
                    return;
                }
            }
        }
        if block && jids.is_empty() {
            self.reply_error(el, Condition::BadRequest).await;
            return;
        }
        let clear = !block && jids.is_empty();
        let before: BTreeSet<String> = self.blocklist().await.clone();
        let (add, remove) = if block {
            (jids.clone(), vec![])
        } else {
            (vec![], jids.clone())
        };
        if let Err(e) = self
            .router
            .store()
            .blocklist_update(&self.bare_str, add, remove, clear)
            .await
        {
            tracing::error!(account = %self.bare, "updating the block list failed: {e:#}");
            self.reply_error(el, Condition::InternalServerError).await;
            return;
        }
        self.blocklist = None;
        self.reply_result(el, None).await;
        // Push to resources that asked for the list (§3.3).
        let mut push_payload = Element::bare(payload.name(), NS_BLOCKING);
        for j in &jids {
            push_payload.append_child(stanza::element("item", NS_BLOCKING, &[("jid", j)]));
        }
        let targets: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.blocklist_interest)
            .map(|(r, s)| (r.clone(), s.full.clone()))
            .collect();
        for (r, full) in targets {
            let mut push = stanza::element(
                "iq",
                NS_CLIENT,
                &[("type", "set"), ("id", &server_id("block")), ("to", &full)],
            );
            push.append_child(push_payload.clone());
            self.send(&r, push);
        }
        // Blocked contacts see us go offline; unblocked subscribers see us again.
        let fulls: Vec<(String, Option<Element>)> = self
            .sessions
            .values()
            .filter(|s| s.available())
            .map(|s| (s.full.clone(), s.presence.clone()))
            .collect();
        let changed: Vec<String> = if block {
            jids.iter().filter(|j| !before.contains(*j)).cloned().collect()
        } else if clear {
            before.into_iter().collect()
        } else {
            jids.into_iter().filter(|j| before.contains(j)).collect()
        };
        for target in changed {
            let Ok(bare) = BareJid::new(&target) else { continue };
            let subscribed = self
                .roster_item(&bare.to_string())
                .await
                .is_some_and(|i| i.subscription.has_from());
            if !subscribed {
                continue;
            }
            for (full, p) in &fulls {
                let out = if block {
                    presence(Some("unavailable"), full, &target)
                } else {
                    let mut p = p.clone().unwrap_or_else(|| presence(None, full, &target));
                    stanza::set_attr(&mut p, "to", target.as_str());
                    p
                };
                self.route(out).await;
            }
        }
    }

    /// XEP-0049: one stored element per (name, namespace).
    async fn private_xml(&mut self, el: &Element, query: &Element) {
        let ty = stanza::stanza_type(el).to_owned();
        let children: Vec<Element> = query.children().cloned().collect();
        if children.is_empty() || children.iter().any(|c| c.ns() == NS_PRIVATE || c.ns() == NS_CLIENT) {
            self.reply_error(el, Condition::NotAcceptable).await;
            return;
        }
        let key = |c: &Element| format!("private:{{{}}}{}", c.ns(), c.name());
        if ty == "get" {
            let mut out = Element::bare("query", NS_PRIVATE);
            for c in &children {
                let stored = self
                    .router
                    .store()
                    .private_get(&self.bare_str, &key(c))
                    .await
                    .ok()
                    .flatten();
                out.append_child(
                    stored
                        .and_then(|x| x.parse::<Element>().ok())
                        .unwrap_or_else(|| Element::bare(c.name(), c.ns())),
                );
            }
            self.reply_result(el, Some(out)).await;
            return;
        }
        for c in &children {
            let xml = String::from(c);
            if xml.len() > MAX_PRIVATE_BYTES {
                self.reply_error(el, Condition::NotAcceptable).await;
                return;
            }
            if let Err(e) = self
                .router
                .store()
                .private_put(&self.bare_str, &key(c), Some(xml))
                .await
            {
                tracing::error!(account = %self.bare, "private storage failed: {e:#}");
                self.reply_error(el, Condition::InternalServerError).await;
                return;
            }
        }
        self.reply_result(el, None).await;
    }

    async fn disco_info(&mut self, el: &Element, query: &Element) {
        if let Some(node) = query.attr("node") {
            match self.pep_node_info(el, node).await {
                Some(info) => self.reply_result(el, Some(info)).await,
                None => self.reply_error(el, Condition::ItemNotFound).await,
            }
            return;
        }
        let mut q = Element::bare("query", ns::DISCO_INFO);
        q.append_child(stanza::element(
            "identity",
            ns::DISCO_INFO,
            &[("category", "account"), ("type", "registered")],
        ));
        q.append_child(stanza::element(
            "identity",
            ns::DISCO_INFO,
            &[("category", "pubsub"), ("type", "pep")],
        ));
        for f in disco::account_features(self.router.settings().archive_enabled) {
            q.append_child(stanza::element("feature", ns::DISCO_INFO, &[("var", f)]));
        }
        self.reply_result(el, Some(q)).await;
    }

    async fn disco_items(&mut self, el: &Element, query: &Element) {
        if query.attr("node").is_some() {
            self.reply_result(el, Some(Element::bare("query", ns::DISCO_ITEMS)))
                .await;
            return;
        }
        let requester = jid_attr(el, "from");
        let items = self.pep_disco_items(requester.as_ref()).await;
        self.reply_result(el, Some(items)).await;
    }
}
