//! PEP (XEP-0163 over a subset of XEP-0060) in the owner's actor (§5.7).
//!
//! Nodes are keyed by (owner bare JID, node). Publishing auto-creates a
//! node; publish-options must match an existing node's configuration or the
//! publish fails with `precondition-not-met`. Access models: `open`,
//! `presence` (contacts with a `from`/`both` subscription), `roster` (same
//! as presence) and `whitelist`/`authorize` (owner only).
//!
//! Notifications follow `+notify` (XEP-0115 caps): the owner's actor sends
//! one [`PepEvent`] to each subscriber's actor, which forwards it to its own
//! available resources that `+notify` the node. When a resource comes
//! online or its caps become known, its actor asks each contact's actor for
//! the last items ([`LastItemsRequest`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::ns;

use super::{UserActor, jid_attr, server_id};
use crate::forms::{self, FormBuilder};
use crate::router::{Target, classify};
use crate::stanza::{self, Condition, NS_CLIENT};
use crate::store::PepItem;
use crate::user::UserMsg;
use crate::util::now_ms;

pub const NS_PUBSUB: &str = "http://jabber.org/protocol/pubsub";
pub const NS_PUBSUB_OWNER: &str = "http://jabber.org/protocol/pubsub#owner";
pub const NS_PUBSUB_EVENT: &str = "http://jabber.org/protocol/pubsub#event";
pub const NS_PUBSUB_ERRORS: &str = "http://jabber.org/protocol/pubsub#errors";
const NS_NODE_CONFIG: &str = "http://jabber.org/protocol/pubsub#node_config";
const NS_PUBLISH_OPTIONS: &str = "http://jabber.org/protocol/pubsub#publish-options";

/// Limits (§6.4).
pub const MAX_NODES: usize = 100;
pub const MAX_ITEMS: u64 = 1000;
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// pubsub features advertised on account JIDs.
pub const FEATURES: &[&str] = &[
    NS_PUBSUB,
    "http://jabber.org/protocol/pubsub#publish",
    "http://jabber.org/protocol/pubsub#publish-options",
    "http://jabber.org/protocol/pubsub#auto-create",
    "http://jabber.org/protocol/pubsub#access-open",
    "http://jabber.org/protocol/pubsub#access-presence",
    "http://jabber.org/protocol/pubsub#access-roster",
    "http://jabber.org/protocol/pubsub#access-whitelist",
    "http://jabber.org/protocol/pubsub#retrieve-items",
    "http://jabber.org/protocol/pubsub#item-ids",
    "http://jabber.org/protocol/pubsub#persistent-items",
    "http://jabber.org/protocol/pubsub#last-published",
    "http://jabber.org/protocol/pubsub#config-node",
    "http://jabber.org/protocol/pubsub#config-node-max",
    "http://jabber.org/protocol/pubsub#create-nodes",
    "http://jabber.org/protocol/pubsub#create-and-configure",
    "http://jabber.org/protocol/pubsub#delete-nodes",
    "http://jabber.org/protocol/pubsub#delete-items",
    "http://jabber.org/protocol/pubsub#retract-items",
    "http://jabber.org/protocol/pubsub#purge-nodes",
    "http://jabber.org/protocol/pubsub#multi-items",
    "http://jabber.org/protocol/pubsub#auto-subscribe",
    "http://jabber.org/protocol/pubsub#filtered-notifications",
    "http://jabber.org/protocol/pubsub#presence-notifications",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Open,
    Presence,
    Roster,
    Whitelist,
    Authorize,
}

impl Access {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => Access::Open,
            "presence" => Access::Presence,
            "roster" => Access::Roster,
            "whitelist" => Access::Whitelist,
            "authorize" => Access::Authorize,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Access::Open => "open",
            Access::Presence => "presence",
            Access::Roster => "roster",
            Access::Whitelist => "whitelist",
            Access::Authorize => "authorize",
        }
    }
}

/// `pubsub#max_items`: a number, or `max` (the service limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MaxItems {
    Max,
    #[serde(untagged)]
    N(u64),
}

impl MaxItems {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "max" => Some(MaxItems::Max),
            n => n.parse::<u64>().ok().filter(|n| *n >= 1).map(MaxItems::N),
        }
    }

    pub fn limit(self) -> u64 {
        match self {
            MaxItems::Max => MAX_ITEMS,
            MaxItems::N(n) => n.min(MAX_ITEMS),
        }
    }

    fn as_string(self) -> String {
        match self {
            MaxItems::Max => "max".into(),
            MaxItems::N(n) => n.to_string(),
        }
    }
}

/// A node's configuration (stored as JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    pub access_model: Access,
    pub max_items: MaxItems,
    pub persist_items: bool,
    /// `never`, `on_sub` or `on_sub_and_presence`.
    pub send_last_published_item: String,
    pub notify_retract: bool,
    pub notify_delete: bool,
    pub deliver_payloads: bool,
    pub title: Option<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            access_model: Access::Presence,
            max_items: MaxItems::N(1),
            persist_items: true,
            send_last_published_item: "on_sub_and_presence".into(),
            notify_retract: true,
            notify_delete: true,
            deliver_payloads: true,
            title: None,
        }
    }
}

/// A submitted value the hub can't use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadValue;

impl NodeConfig {
    /// Apply submitted `pubsub#…` values. Unknown fields are ignored.
    pub fn apply(&mut self, v: &BTreeMap<String, Vec<String>>) -> Result<(), BadValue> {
        for (var, vals) in v {
            let Some(val) = vals.first().map(String::as_str) else {
                continue;
            };
            match var.as_str() {
                "pubsub#access_model" => self.access_model = Access::parse(val).ok_or(BadValue)?,
                "pubsub#max_items" => self.max_items = MaxItems::parse(val).ok_or(BadValue)?,
                "pubsub#persist_items" => self.persist_items = forms::parse_bool(val).ok_or(BadValue)?,
                "pubsub#send_last_published_item" => {
                    if !matches!(val, "never" | "on_sub" | "on_sub_and_presence") {
                        return Err(BadValue);
                    }
                    self.send_last_published_item = val.to_owned();
                }
                "pubsub#notify_retract" => self.notify_retract = forms::parse_bool(val).ok_or(BadValue)?,
                "pubsub#notify_delete" => self.notify_delete = forms::parse_bool(val).ok_or(BadValue)?,
                "pubsub#deliver_payloads" => self.deliver_payloads = forms::parse_bool(val).ok_or(BadValue)?,
                "pubsub#title" => self.title = Some(val.to_owned()).filter(|t| !t.is_empty()),
                _ => {}
            }
        }
        Ok(())
    }

    /// XEP-0060 §7.1.5: every given option must match the node.
    pub fn matches(&self, v: &BTreeMap<String, Vec<String>>) -> bool {
        let mut probe = self.clone();
        if probe.apply(v).is_err() {
            return false;
        }
        probe == *self
    }

    fn form(&self) -> Element {
        let mut b = FormBuilder::new("form", NS_NODE_CONFIG);
        b.field(
            "pubsub#title",
            "text-single",
            Some("Title"),
            &[self.title.as_deref().unwrap_or("")],
        );
        b.list(
            "pubsub#access_model",
            Some("Access model"),
            self.access_model.as_str(),
            &["open", "presence", "roster", "whitelist", "authorize"],
        );
        b.field(
            "pubsub#max_items",
            "text-single",
            Some("Max items"),
            &[&self.max_items.as_string()],
        );
        b.boolean("pubsub#persist_items", Some("Persist items"), self.persist_items);
        b.list(
            "pubsub#send_last_published_item",
            Some("Send the last item"),
            &self.send_last_published_item,
            &["never", "on_sub", "on_sub_and_presence"],
        );
        b.boolean(
            "pubsub#notify_retract",
            Some("Notify on retraction"),
            self.notify_retract,
        );
        b.boolean("pubsub#notify_delete", Some("Notify on deletion"), self.notify_delete);
        b.boolean(
            "pubsub#deliver_payloads",
            Some("Deliver payloads"),
            self.deliver_payloads,
        );
        b.build()
    }
}

/// The owner's node configurations.
pub type Nodes = BTreeMap<String, NodeConfig>;

/// A notification from a PEP service to a subscriber's actor.
#[derive(Debug, Clone)]
pub struct PepEvent {
    pub node: String,
    /// `<message type='headline'>` with the `<event/>`.
    pub message: Element,
}

/// "Send `to` (a full JID) the last items of these nodes it +notifies."
#[derive(Debug, Clone)]
pub struct LastItemsRequest {
    pub to: String,
    pub nodes: Vec<String>,
}

/// `<error type='cancel'><conflict/><precondition-not-met/></error>` etc.
fn pubsub_error(req: &Element, cond: Condition, app: &str, from: &str) -> Option<Element> {
    let app_el = (!app.is_empty()).then(|| Element::bare(app, NS_PUBSUB_ERRORS));
    stanza::error_reply_with(req, cond, Some(from), None, app_el)
}

fn item_element(item: &PepItem, payload: bool) -> Element {
    let mut el = stanza::element("item", NS_PUBSUB_EVENT, &[("id", &item.id)]);
    if payload && let Ok(p) = item.payload.parse::<Element>() {
        el.append_child(p);
    }
    el
}

impl UserActor {
    async fn nodes(&mut self) -> &mut Nodes {
        if self.pep.is_none() {
            let rows = self.router.store().pep_nodes(&self.bare_str).await.unwrap_or_else(|e| {
                tracing::error!(account = %self.bare, "loading PEP nodes failed: {e:#}");
                Vec::new()
            });
            let nodes = rows
                .into_iter()
                .map(|(n, json)| (n, serde_json::from_str(&json).unwrap_or_default()))
                .collect();
            self.pep = Some(nodes);
        }
        self.pep.as_mut().unwrap()
    }

    async fn save_node(&mut self, node: &str, cfg: NodeConfig) {
        let json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".into());
        if let Err(e) = self.router.store().pep_node_put(&self.bare_str, node, json).await {
            tracing::error!(account = %self.bare, "saving a PEP node failed: {e:#}");
        }
        self.nodes().await.insert(node.to_owned(), cfg);
    }

    /// May `requester` (bare JID) read a node with this config?
    async fn may_read(&mut self, requester: &str, cfg: &NodeConfig) -> bool {
        if requester == self.bare_str {
            return true;
        }
        match cfg.access_model {
            Access::Open => true,
            Access::Presence | Access::Roster => self
                .roster_item(requester)
                .await
                .is_some_and(|i| i.subscription.has_from() && !i.hidden),
            Access::Whitelist | Access::Authorize => false,
        }
    }

    pub(super) async fn pubsub_iq(&mut self, el: &Element, payload: &Element, owner: bool) {
        let ty = stanza::stanza_type(el).to_owned();
        let me = self.reply_from(el);
        let requester = jid_attr(el, "from")
            .map(|j| j.to_bare().to_string())
            .unwrap_or_default();
        let Some(op) = payload.children().next().cloned() else {
            self.reply_error(el, Condition::BadRequest).await;
            return;
        };
        let owner_ns = payload.ns() == NS_PUBSUB_OWNER;
        if (owner_ns || ty == "set") && !owner {
            if let Some(e) = pubsub_error(el, Condition::Forbidden, "", &me) {
                self.respond(e).await;
            }
            return;
        }
        let node = op.attr("node").map(str::to_owned);
        match (owner_ns, ty.as_str(), op.name()) {
            (false, "set", "publish") => self.publish(el, payload, &op).await,
            (false, "set", "retract") => self.retract(el, &op).await,
            (false, "set", "create") => self.create(el, payload, &op).await,
            (false, "get", "items") => self.items(el, &op, &requester).await,
            (false, "get", "subscriptions" | "affiliations") => {
                let mut ps = Element::bare("pubsub", NS_PUBSUB);
                ps.append_child(Element::bare(op.name(), NS_PUBSUB));
                self.reply_result(el, Some(ps)).await;
            }
            (true, "get", "default") => {
                let mut ps = Element::bare("pubsub", NS_PUBSUB_OWNER);
                let mut d = Element::bare("default", NS_PUBSUB_OWNER);
                d.append_child(NodeConfig::default().form());
                ps.append_child(d);
                self.reply_result(el, Some(ps)).await;
            }
            (true, _, "configure" | "delete" | "purge") => {
                let Some(node) = node else {
                    self.reply_error(el, Condition::BadRequest).await;
                    return;
                };
                match self.nodes().await.get(&node).cloned() {
                    Some(cfg) => self.owner_op(el, &op, &node, cfg).await,
                    None => self.reply_error(el, Condition::ItemNotFound).await,
                }
            }
            (_, _, "subscribe" | "unsubscribe" | "options") => {
                // PEP delivery follows presence and +notify, not explicit
                // subscriptions (XEP-0163 §1.2).
                if let Some(e) = pubsub_error(el, Condition::FeatureNotImplemented, "unsupported", &me) {
                    self.respond(e).await;
                }
            }
            _ => self.reply_error(el, Condition::FeatureNotImplemented).await,
        }
    }

    /// `pubsub#owner` configure (get/set), delete and purge on an existing node.
    async fn owner_op(&mut self, el: &Element, op: &Element, node: &str, mut cfg: NodeConfig) {
        let ty = stanza::stanza_type(el).to_owned();
        match (ty.as_str(), op.name()) {
            ("get", "configure") => {
                let mut ps = Element::bare("pubsub", NS_PUBSUB_OWNER);
                let mut c = stanza::element("configure", NS_PUBSUB_OWNER, &[("node", node)]);
                c.append_child(cfg.form());
                ps.append_child(c);
                self.reply_result(el, Some(ps)).await;
            }
            ("set", "configure") => {
                if let Some(x) = op.get_child("x", forms::NS_DATA)
                    && cfg.apply(&forms::values(x)).is_err()
                {
                    self.reply_error(el, Condition::NotAcceptable).await;
                    return;
                }
                let limit = cfg.max_items.limit() as usize;
                self.save_node(node, cfg).await;
                // Shrinking max_items trims the node (newest first from the store).
                if let Ok(items) = self.router.store().pep_items(&self.bare_str, node, vec![], None).await {
                    for extra in items.iter().skip(limit) {
                        let _ = self.router.store().pep_retract(&self.bare_str, node, &extra.id).await;
                    }
                }
                self.reply_result(el, None).await;
            }
            ("set", "delete") => {
                self.nodes().await.remove(node);
                let _ = self.router.store().pep_node_delete(&self.bare_str, node).await;
                self.reply_result(el, None).await;
                if cfg.notify_delete {
                    let ev = stanza::element("delete", NS_PUBSUB_EVENT, &[("node", node)]);
                    self.notify(node, &cfg, ev).await;
                }
            }
            ("set", "purge") => {
                let _ = self.router.store().pep_purge(&self.bare_str, node).await;
                self.reply_result(el, None).await;
                let ev = stanza::element("purge", NS_PUBSUB_EVENT, &[("node", node)]);
                self.notify(node, &cfg, ev).await;
            }
            _ => self.reply_error(el, Condition::BadRequest).await,
        }
    }

    /// `<create node/>` with an optional `<configure>` form.
    async fn create(&mut self, el: &Element, pubsub: &Element, op: &Element) {
        let Some(node) = op.attr("node").filter(|n| !n.is_empty()).map(str::to_owned) else {
            // Instant nodes make no sense for PEP.
            self.reply_error(el, Condition::NotAcceptable).await;
            return;
        };
        if self.nodes().await.contains_key(&node) {
            self.reply_error(el, Condition::Conflict).await;
            return;
        }
        if self.nodes().await.len() >= MAX_NODES {
            self.reply_error(el, Condition::ResourceConstraint).await;
            return;
        }
        let mut cfg = NodeConfig::default();
        if let Some(x) = pubsub
            .get_child("configure", NS_PUBSUB)
            .and_then(|c| c.get_child("x", forms::NS_DATA))
            && cfg.apply(&forms::values(x)).is_err()
        {
            self.reply_error(el, Condition::NotAcceptable).await;
            return;
        }
        self.save_node(&node, cfg).await;
        self.reply_result(el, None).await;
    }

    /// XEP-0060 §7.1 publish, with publish-options (§7.1.5) and auto-create.
    async fn publish(&mut self, el: &Element, pubsub: &Element, op: &Element) {
        let me = self.reply_from(el);
        let Some(node) = op.attr("node").filter(|n| !n.is_empty()).map(str::to_owned) else {
            if let Some(e) = pubsub_error(el, Condition::BadRequest, "nodeid-required", &me) {
                self.respond(e).await;
            }
            return;
        };
        let options = pubsub
            .get_child("publish-options", NS_PUBSUB)
            .and_then(|p| p.get_child("x", forms::NS_DATA))
            .map(forms::values)
            .unwrap_or_default();
        if let Some(t) = forms::single(&options, "FORM_TYPE")
            && t != NS_PUBLISH_OPTIONS
        {
            self.reply_error(el, Condition::BadRequest).await;
            return;
        }
        let existing = self.nodes().await.get(&node).cloned();
        let cfg = match existing {
            Some(cfg) if cfg.matches(&options) => cfg,
            Some(_) => {
                if let Some(e) = pubsub_error(el, Condition::Conflict, "precondition-not-met", &me) {
                    self.respond(e).await;
                }
                return;
            }
            None => {
                if self.nodes().await.len() >= MAX_NODES {
                    self.reply_error(el, Condition::ResourceConstraint).await;
                    return;
                }
                let mut cfg = NodeConfig::default();
                if cfg.apply(&options).is_err() {
                    if let Some(e) = pubsub_error(el, Condition::Conflict, "precondition-not-met", &me) {
                        self.respond(e).await;
                    }
                    return;
                }
                self.save_node(&node, cfg.clone()).await;
                cfg
            }
        };
        let item = op.get_child("item", NS_PUBSUB);
        let id = item
            .and_then(|i| i.attr("id"))
            .filter(|i| !i.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let payload_xml = item
            .and_then(|i| i.children().next())
            .map(String::from)
            .unwrap_or_default();
        if payload_xml.len() > MAX_PAYLOAD {
            if let Some(e) = pubsub_error(el, Condition::NotAcceptable, "payload-too-big", &me) {
                self.respond(e).await;
            }
            return;
        }
        let stored = PepItem {
            id: id.clone(),
            publisher: el.attr("from").unwrap_or_default().to_owned(),
            payload: payload_xml,
            published_ms: now_ms(),
        };
        if cfg.persist_items
            && let Err(e) = self
                .router
                .store()
                .pep_publish(&self.bare_str, &node, stored.clone(), Some(cfg.max_items.limit()))
                .await
        {
            tracing::error!(account = %self.bare, "PEP publish failed: {e:#}");
            self.reply_error(el, Condition::InternalServerError).await;
            return;
        }
        let mut ps = Element::bare("pubsub", NS_PUBSUB);
        let mut p = stanza::element("publish", NS_PUBSUB, &[("node", &node)]);
        p.append_child(stanza::element("item", NS_PUBSUB, &[("id", &id)]));
        ps.append_child(p);
        self.reply_result(el, Some(ps)).await;
        let mut items = stanza::element("items", NS_PUBSUB_EVENT, &[("node", &node)]);
        items.append_child(item_element(&stored, cfg.deliver_payloads));
        self.notify(&node, &cfg, items).await;
    }

    async fn retract(&mut self, el: &Element, op: &Element) {
        let Some(node) = op.attr("node").map(str::to_owned) else {
            self.reply_error(el, Condition::BadRequest).await;
            return;
        };
        let Some(cfg) = self.nodes().await.get(&node).cloned() else {
            self.reply_error(el, Condition::ItemNotFound).await;
            return;
        };
        let ids: Vec<String> = op
            .children()
            .filter(|i| i.is("item", NS_PUBSUB))
            .filter_map(|i| i.attr("id"))
            .map(str::to_owned)
            .collect();
        if ids.is_empty() {
            self.reply_error(el, Condition::BadRequest).await;
            return;
        }
        let mut removed = Vec::new();
        for id in &ids {
            if self
                .router
                .store()
                .pep_retract(&self.bare_str, &node, id)
                .await
                .unwrap_or(false)
            {
                removed.push(id.clone());
            }
        }
        if removed.is_empty() {
            self.reply_error(el, Condition::ItemNotFound).await;
            return;
        }
        self.reply_result(el, None).await;
        let notify = matches!(op.attr("notify"), Some("true" | "1")) || cfg.notify_retract;
        if notify {
            let mut items = stanza::element("items", NS_PUBSUB_EVENT, &[("node", &node)]);
            for id in removed {
                items.append_child(stanza::element("retract", NS_PUBSUB_EVENT, &[("id", &id)]));
            }
            self.notify(&node, &cfg, items).await;
        }
    }

    /// XEP-0060 §6.5: all items (newest `max_items`) or by id, oldest first.
    async fn items(&mut self, el: &Element, op: &Element, requester: &str) {
        let me = self.reply_from(el);
        let Some(node) = op.attr("node").map(str::to_owned) else {
            self.reply_error(el, Condition::BadRequest).await;
            return;
        };
        let Some(cfg) = self.nodes().await.get(&node).cloned() else {
            self.reply_error(el, Condition::ItemNotFound).await;
            return;
        };
        if !self.may_read(requester, &cfg).await {
            let (cond, app) = match cfg.access_model {
                Access::Presence | Access::Roster => (Condition::NotAuthorized, "presence-subscription-required"),
                _ => (Condition::NotAllowed, "closed-node"),
            };
            if let Some(e) = pubsub_error(el, cond, app, &me) {
                self.respond(e).await;
            }
            return;
        }
        let ids: Vec<String> = op
            .children()
            .filter(|i| i.is("item", NS_PUBSUB))
            .filter_map(|i| i.attr("id"))
            .map(str::to_owned)
            .collect();
        let max = op.attr("max_items").and_then(|m| m.parse::<usize>().ok());
        let mut found = match self
            .router
            .store()
            .pep_items(&self.bare_str, &node, ids.clone(), max)
            .await
        {
            Ok(f) => f,
            Err(_) => {
                self.reply_error(el, Condition::InternalServerError).await;
                return;
            }
        };
        if ids.is_empty() {
            // The store gives newest first.
            found.reverse();
        } else if found.is_empty() {
            self.reply_error(el, Condition::ItemNotFound).await;
            return;
        }
        let mut items = stanza::element("items", NS_PUBSUB, &[("node", &node)]);
        for i in &found {
            let mut e = stanza::element("item", NS_PUBSUB, &[("id", &i.id)]);
            if let Ok(p) = i.payload.parse::<Element>() {
                e.append_child(p);
            }
            items.append_child(e);
        }
        let mut ps = Element::bare("pubsub", NS_PUBSUB);
        ps.append_child(items);
        self.reply_result(el, Some(ps)).await;
    }

    /// The headline carrying `event_child` from our bare JID.
    fn event_message(&self, event_child: Element, to: &str) -> Element {
        let mut m = stanza::element(
            "message",
            NS_CLIENT,
            &[
                ("from", &self.bare_str),
                ("to", to),
                ("type", "headline"),
                ("id", &server_id("pep")),
            ],
        );
        let mut ev = Element::bare("event", NS_PUBSUB_EVENT);
        ev.append_child(event_child);
        m.append_child(ev);
        m
    }

    /// Notify the owner's resources and the subscribers (contacts with a
    /// `from`/`both` subscription, unless the node is owner-only).
    async fn notify(&mut self, node: &str, cfg: &NodeConfig, event_child: Element) {
        let own = self.event_message(event_child.clone(), &self.bare_str.clone());
        self.pep_event(PepEvent {
            node: node.to_owned(),
            message: own,
        });
        if matches!(cfg.access_model, Access::Whitelist | Access::Authorize) {
            return;
        }
        let subscribers: Vec<String> = self
            .roster()
            .await
            .values()
            .filter(|i| i.subscription.has_from() && !i.hidden)
            .map(|i| i.contact.clone())
            .collect();
        for contact in subscribers {
            let Ok(bare) = BareJid::new(&contact) else { continue };
            if !matches!(classify(self.router.domain(), &bare.clone().into()), Target::Local(_)) {
                continue;
            }
            let message = self.event_message(event_child.clone(), &contact);
            self.router.deliver_to_account(
                &bare,
                UserMsg::PepEvent(PepEvent {
                    node: node.to_owned(),
                    message,
                }),
            );
        }
    }

    /// A PEP event for this account: forward to available resources that
    /// +notify the node (XEP-0163 §4.3.2 filtered notifications).
    pub(super) fn pep_event(&mut self, ev: PepEvent) {
        let targets: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.available() && s.notifies(&ev.node))
            .map(|(r, s)| (r.clone(), s.full.clone()))
            .collect();
        for (r, full) in targets {
            let mut m = ev.message.clone();
            stanza::set_attr(&mut m, "to", full.as_str());
            self.send(&r, m);
        }
    }

    /// A resource (ours or a contact's) wants our last items.
    pub(super) async fn pep_last_items(&mut self, req: LastItemsRequest) {
        let Ok(to) = Jid::new(&req.to) else { return };
        let requester = to.to_bare().to_string();
        for node in req.nodes {
            let Some(cfg) = self.nodes().await.get(&node).cloned() else {
                continue;
            };
            if cfg.send_last_published_item == "never" || !self.may_read(&requester, &cfg).await {
                continue;
            }
            let Ok(mut last) = self
                .router
                .store()
                .pep_items(&self.bare_str, &node, vec![], Some(1))
                .await
            else {
                continue;
            };
            let Some(item) = last.pop() else { continue };
            let mut items = stanza::element("items", NS_PUBSUB_EVENT, &[("node", &node)]);
            items.append_child(item_element(&item, cfg.deliver_payloads));
            let mut m = self.event_message(items, &req.to);
            m.append_child(crate::user::message::delay(&self.bare_str, item.published_ms));
            if self.is_own(&to) {
                if let Some(r) = to.resource() {
                    let r = r.as_str().to_owned();
                    self.send(&r, m);
                }
            } else {
                self.route(m).await;
            }
        }
    }

    /// A resource became available with known caps: ask our own PEP and our
    /// contacts' for the last items it +notifies (XEP-0163 §4.3.2).
    pub(super) async fn request_last_items(&mut self, resource: &str) {
        let Some(s) = self.sessions.get(resource) else { return };
        let nodes = s.notify_nodes();
        if nodes.is_empty() {
            return;
        }
        let to = s.full.clone();
        self.pep_last_items(LastItemsRequest {
            to: to.clone(),
            nodes: nodes.clone(),
        })
        .await;
        let contacts: Vec<String> = self
            .roster()
            .await
            .values()
            .filter(|i| i.subscription.has_to() && !i.hidden)
            .map(|i| i.contact.clone())
            .collect();
        for contact in contacts {
            let Ok(bare) = BareJid::new(&contact) else { continue };
            if !matches!(classify(self.router.domain(), &bare.clone().into()), Target::Local(_)) {
                continue;
            }
            self.router.deliver_to_account(
                &bare,
                UserMsg::PepLastItems(LastItemsRequest {
                    to: to.clone(),
                    nodes: nodes.clone(),
                }),
            );
        }
    }

    /// disco#info with a node: the node exists and the requester may see it.
    pub(super) async fn pep_node_info(&mut self, req: &Element, node: &str) -> Option<Element> {
        let cfg = self.nodes().await.get(node).cloned()?;
        let requester = jid_attr(req, "from")
            .map(|j| j.to_bare().to_string())
            .unwrap_or_default();
        if !self.may_read(&requester, &cfg).await {
            return None;
        }
        let mut q = stanza::element("query", ns::DISCO_INFO, &[("node", node)]);
        q.append_child(stanza::element(
            "identity",
            ns::DISCO_INFO,
            &[("category", "pubsub"), ("type", "leaf")],
        ));
        q.append_child(stanza::element("feature", ns::DISCO_INFO, &[("var", NS_PUBSUB)]));
        Some(q)
    }

    /// disco#items on the account: the nodes the requester may read.
    pub(super) async fn pep_disco_items(&mut self, requester: Option<&Jid>) -> Element {
        let requester = requester.map(|j| j.to_bare().to_string()).unwrap_or_default();
        let nodes: Vec<(String, NodeConfig)> = self.nodes().await.iter().map(|(n, c)| (n.clone(), c.clone())).collect();
        let mut q = Element::bare("query", ns::DISCO_ITEMS);
        for (node, cfg) in nodes {
            if self.may_read(&requester, &cfg).await {
                q.append_child(stanza::element(
                    "item",
                    ns::DISCO_ITEMS,
                    &[("jid", &self.bare_str), ("node", &node)],
                ));
            }
        }
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), vec![v.to_string()]))
            .collect()
    }

    #[test]
    fn publish_options_matching() {
        let mut cfg = NodeConfig::default();
        cfg.apply(&opts(&[("pubsub#access_model", "open"), ("pubsub#max_items", "max")]))
            .unwrap();
        assert_eq!(cfg.access_model, Access::Open);
        assert_eq!(cfg.max_items, MaxItems::Max);
        assert_eq!(cfg.max_items.limit(), MAX_ITEMS);
        assert!(cfg.matches(&opts(&[("pubsub#access_model", "open")])));
        assert!(cfg.matches(&opts(&[("pubsub#max_items", "max"), ("FORM_TYPE", NS_PUBLISH_OPTIONS)])));
        assert!(!cfg.matches(&opts(&[("pubsub#access_model", "presence")])));
        assert!(!cfg.matches(&opts(&[("pubsub#max_items", "5")])));
        assert!(!cfg.matches(&opts(&[("pubsub#access_model", "bogus")])));
        assert!(
            cfg.matches(&opts(&[("pubsub#unknown_option", "x")])),
            "unknown fields are ignored"
        );
        assert!(
            NodeConfig::default()
                .apply(&opts(&[("pubsub#max_items", "0")]))
                .is_err()
        );
    }

    #[test]
    fn config_round_trips_through_json_and_forms() {
        let cfg = NodeConfig {
            access_model: Access::Whitelist,
            max_items: MaxItems::Max,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<NodeConfig>(&json).unwrap(), cfg);
        let n = NodeConfig {
            max_items: MaxItems::N(7),
            ..Default::default()
        };
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(serde_json::from_str::<NodeConfig>(&json).unwrap(), n);
        let mut back = NodeConfig::default();
        back.apply(&forms::values(&cfg.form())).unwrap();
        assert_eq!(back, cfg);
    }
}
