//! The per-account actor (§5.3). It owns the bound resources and their
//! presence, the roster, carbons and caps state, and applies the delivery
//! rules of §5.5.
//!
//! Every stanza a session sends goes to its own account's actor first
//! ([`UserMsg::Outbound`]): outbound processing (roster updates for
//! subscriptions, presence broadcast, sent carbons, the archive) happens
//! there, then the stanza is routed. Stanzas for the account arrive as
//! [`UserMsg::Deliver`] (inbound processing: subscriptions, delivery rules,
//! received carbons, offline storage). One FIFO hop per account keeps the
//! stanzas of one session to one entity in order.
//!
//! The actor awaits the store but never another actor: requests to other
//! actors (probes, PEP) are messages, answered by later messages.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::{BareJid, FullJid, Jid};
use xmpp_parsers::stream_error::DefinedCondition as StreamCondition;

use crate::router::Router;
use crate::stanza::{self, Condition, Kind};
use crate::store::RosterItem;
use crate::util::{hex, random_bytes};

mod caps;
mod iq;
mod mam;
pub mod message;
pub mod pep;
mod presence;
pub mod roster;

/// An actor with no sessions stops after this long without messages.
const IDLE: Duration = Duration::from_secs(300);

/// Why the server closes a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Another session bound the same resource.
    Conflict,
    /// The account was deleted or its access revoked.
    AccountDeleted,
}

impl CloseReason {
    pub fn condition(self) -> StreamCondition {
        match self {
            CloseReason::Conflict => StreamCondition::Conflict,
            CloseReason::AccountDeleted => StreamCondition::NotAuthorized,
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            CloseReason::Conflict => "Replaced by a new session with the same resource",
            CloseReason::AccountDeleted => "The account was removed",
        }
    }
}

/// Lets an actor close a session even when its outbox is full.
#[derive(Debug, Default)]
pub struct Kick {
    token: CancellationToken,
    reason: Mutex<Option<CloseReason>>,
}

impl Kick {
    pub fn kick(&self, reason: CloseReason) {
        self.reason.lock().unwrap().get_or_insert(reason);
        self.token.cancel();
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    pub fn reason(&self) -> Option<CloseReason> {
        *self.reason.lock().unwrap()
    }
}

/// How the actor reaches one bound session.
pub struct SessionLink {
    pub conn_id: u64,
    pub outbox: mpsc::Sender<Element>,
    pub kick: Arc<Kick>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindError {
    /// `max_resources_per_account` reached.
    ResourceConstraint,
    /// The actor was retiring; ask the router again.
    Retry,
}

pub enum UserMsg {
    Bind {
        /// Already resourceprep'd; `None` lets the server pick one.
        resource: Option<String>,
        link: SessionLink,
        reply: oneshot::Sender<Result<FullJid, BindError>>,
    },
    /// The session ended for good (with stream management: after the
    /// resumption timeout).
    Unbind {
        resource: String,
        conn_id: u64,
    },
    /// A stanza one of this account's sessions sent (`from` is stamped).
    Outbound {
        resource: String,
        el: Element,
    },
    /// A stanza addressed to this account (bare or full JID).
    Deliver(Element),
    /// A stanza a session could not deliver before it ended (XEP-0198):
    /// store it offline unless another resource is available.
    Requeue(Element),
    /// A PEP event from a contact's (or our own) PEP service, for our
    /// resources that `+notify` the node.
    PepEvent(pep::PepEvent),
    /// A resource of `requester` wants the last items of our nodes it
    /// `+notify`s (it came online or its caps were learned).
    PepLastItems(pep::LastItemsRequest),
    CloseSessions(CloseReason),
}

/// One bound resource.
struct Session {
    link: SessionLink,
    full: String,
    /// The last presence broadcast by this resource (no `to`); `None`
    /// until initial presence, and after unavailable presence.
    presence: Option<Element>,
    priority: i8,
    /// Requested the roster, so it gets roster pushes (RFC 6121 §2.1.6).
    interested: bool,
    /// Requested the block list, so it gets block pushes (XEP-0191).
    blocklist_interest: bool,
    carbons: bool,
    /// XEP-0115 verification string from its presence.
    caps_ver: Option<String>,
    /// Features from its caps, once known.
    features: Option<Arc<Vec<String>>>,
    /// Entities that got directed presence from this resource.
    directed: BTreeSet<String>,
}

impl Session {
    fn available(&self) -> bool {
        self.presence.is_some()
    }
}

/// A request the server sent to one of our resources (caps disco).
enum Pending {
    Caps { resource: String, ver: String },
}

pub struct UserActor {
    bare: BareJid,
    bare_str: String,
    id: u64,
    router: Arc<Router>,
    sessions: BTreeMap<String, Session>,
    max_resources: usize,
    /// Loaded on first use.
    roster: Option<BTreeMap<String, RosterItem>>,
    blocklist: Option<BTreeSet<String>>,
    pending: HashMap<String, Pending>,
}

pub(crate) fn spawn(bare: BareJid, id: u64, router: Arc<Router>, rx: mpsc::Receiver<UserMsg>, max_resources: usize) {
    let actor = UserActor {
        bare_str: bare.to_string(),
        bare,
        id,
        router,
        sessions: BTreeMap::new(),
        max_resources,
        roster: None,
        blocklist: None,
        pending: HashMap::new(),
    };
    tokio::spawn(actor.run(rx));
}

impl UserActor {
    async fn run(mut self, mut rx: mpsc::Receiver<UserMsg>) {
        loop {
            let msg = if self.sessions.is_empty() {
                match tokio::time::timeout(IDLE, rx.recv()).await {
                    Ok(Some(m)) => m,
                    Ok(None) => return,
                    Err(_) => {
                        if self.router.retire(&self.bare, self.id) {
                            break;
                        }
                        continue;
                    }
                }
            } else {
                match rx.recv().await {
                    Some(m) => m,
                    None => return,
                }
            };
            self.handle(msg).await;
        }
        // Retired: hand anything still queued to the next actor.
        rx.close();
        while let Some(msg) = rx.recv().await {
            match msg {
                UserMsg::Bind { reply, .. } => {
                    let _ = reply.send(Err(BindError::Retry));
                }
                UserMsg::Unbind { .. } | UserMsg::CloseSessions(_) => {}
                other => {
                    let _ = self.router.send_to_actor(&self.bare, other);
                }
            }
        }
    }

    async fn handle(&mut self, msg: UserMsg) {
        match msg {
            UserMsg::Bind { resource, link, reply } => {
                let _ = reply.send(self.bind(resource, link));
            }
            UserMsg::Unbind { resource, conn_id } => {
                if self.sessions.get(&resource).is_some_and(|s| s.link.conn_id == conn_id) {
                    self.session_gone(&resource).await;
                }
            }
            UserMsg::Outbound { resource, el } => {
                if self.sessions.contains_key(&resource) {
                    self.outbound(&resource, el).await;
                }
            }
            UserMsg::Deliver(el) => self.inbound(el).await,
            UserMsg::Requeue(el) => self.requeue(el).await,
            UserMsg::PepEvent(ev) => self.pep_event(ev),
            UserMsg::PepLastItems(req) => self.pep_last_items(req).await,
            UserMsg::CloseSessions(reason) => {
                for (_, s) in std::mem::take(&mut self.sessions) {
                    s.link.kick.kick(reason);
                }
            }
        }
    }

    fn bind(&mut self, resource: Option<String>, link: SessionLink) -> Result<FullJid, BindError> {
        let resource = resource.unwrap_or_else(|| hex(&random_bytes::<8>()));
        let full = self
            .bare
            .with_resource_str(&resource)
            .map_err(|_| BindError::ResourceConstraint)?;
        if !self.sessions.contains_key(&resource) && self.sessions.len() >= self.max_resources {
            return Err(BindError::ResourceConstraint);
        }
        let session = Session {
            link,
            full: full.to_string(),
            presence: None,
            priority: 0,
            interested: false,
            blocklist_interest: false,
            carbons: false,
            caps_ver: None,
            features: None,
            directed: BTreeSet::new(),
        };
        // A conflict replaces the old session (§5.4 step 8). Its presence
        // goes with it; the new session sends its own.
        if let Some(old) = self.sessions.insert(resource, session) {
            tracing::info!(jid = %full, "resource conflict: replacing the old session");
            old.link.kick.kick(CloseReason::Conflict);
        }
        Ok(full)
    }

    // --------------------------------------------------------- dispatch

    async fn outbound(&mut self, resource: &str, el: Element) {
        match Kind::of(&el) {
            Some(Kind::Presence) => self.outbound_presence(resource, el).await,
            Some(Kind::Message) => self.outbound_message(resource, el).await,
            Some(Kind::Iq) => self.outbound_iq(resource, el).await,
            None => {}
        }
    }

    async fn inbound(&mut self, el: Element) {
        let from = el.attr("from").and_then(|f| Jid::new(f).ok());
        if let Some(from) = &from
            && self.is_blocked(from).await
        {
            // XEP-0191 §3.4: drop, but don't leave IQ requests hanging.
            if Kind::of(&el) == Some(Kind::Iq) {
                self.router
                    .bounce(&el, Condition::ServiceUnavailable, Some(&self.bare_str));
            }
            return;
        }
        match Kind::of(&el) {
            Some(Kind::Message) => self.inbound_message(el).await,
            Some(Kind::Presence) => self.inbound_presence(el).await,
            Some(Kind::Iq) => self.inbound_iq(el).await,
            None => {}
        }
    }

    // ---------------------------------------------------------- helpers

    /// Write to one session's outbox.
    fn send(&mut self, resource: &str, el: Element) {
        let Some(s) = self.sessions.get(resource) else { return };
        match s.link.outbox.try_send(el) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(el)) => {
                tracing::warn!(account = %self.bare, resource, "session outbox full; bouncing");
                self.router
                    .bounce(&el, Condition::ResourceConstraint, Some(&self.bare_str));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // The session is going away; its Unbind is on the way.
            }
        }
    }

    /// Resources that sent available presence.
    fn available(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|(_, s)| s.available())
            .map(|(r, _)| r.clone())
            .collect()
    }

    /// The JID we answer from, for stanzas addressed to `el`'s `to` (or
    /// the bare JID when there is none).
    fn reply_from(&self, el: &Element) -> String {
        el.attr("to")
            .map(str::to_owned)
            .unwrap_or_else(|| self.bare_str.clone())
    }

    /// Send a server-generated stanza to whoever it is addressed to.
    async fn route(&self, el: Element) {
        self.router.route(el).await;
    }

    async fn roster(&mut self) -> &mut BTreeMap<String, RosterItem> {
        if self.roster.is_none() {
            let items = match self.router.store().roster(&self.bare_str).await {
                Ok(items) => items,
                Err(e) => {
                    tracing::error!(account = %self.bare, "loading the roster failed: {e:#}");
                    Vec::new()
                }
            };
            self.roster = Some(items.into_iter().map(|i| (i.contact.clone(), i)).collect());
        }
        self.roster.as_mut().unwrap()
    }

    async fn roster_item(&mut self, contact: &str) -> Option<RosterItem> {
        self.roster().await.get(contact).cloned()
    }

    async fn save_roster_item(&mut self, item: RosterItem) {
        if let Err(e) = self.router.store().roster_put(&self.bare_str, item.clone()).await {
            tracing::error!(account = %self.bare, "saving a roster item failed: {e:#}");
        }
        self.roster().await.insert(item.contact.clone(), item);
    }

    async fn blocklist(&mut self) -> &BTreeSet<String> {
        if self.blocklist.is_none() {
            let list = self.router.store().blocklist(&self.bare_str).await.unwrap_or_else(|e| {
                tracing::error!(account = %self.bare, "loading the block list failed: {e:#}");
                Vec::new()
            });
            self.blocklist = Some(list.into_iter().collect());
        }
        self.blocklist.as_ref().unwrap()
    }

    /// XEP-0191 §3.4 matching: full JID, bare JID or domain.
    async fn is_blocked(&mut self, jid: &Jid) -> bool {
        let list = self.blocklist().await;
        if list.is_empty() {
            return false;
        }
        list.contains(&jid.to_string())
            || list.contains(&jid.to_bare().to_string())
            || list.contains(jid.domain().as_str())
    }

    fn is_own(&self, jid: &Jid) -> bool {
        jid.to_bare() == self.bare
    }
}

/// A fresh id for server-generated stanzas.
pub fn server_id(prefix: &str) -> String {
    format!("{prefix}-{}", hex(&random_bytes::<8>()))
}

/// Parse a JID attribute.
fn jid_attr(el: &Element, name: &str) -> Option<Jid> {
    el.attr(name).and_then(|v| Jid::new(v).ok())
}

/// The error a client gets for a stanza to a JID it blocked (XEP-0191 §3.5).
fn blocked_error(el: &Element, from: &str) -> Option<Element> {
    let mut e = stanza::error_reply(el, Condition::NotAcceptable, Some(from))?;
    if let Some(err) = e.get_child_mut("error", stanza::NS_CLIENT) {
        err.append_child(Element::bare("blocked", "urn:xmpp:blocking:errors"));
    }
    Some(e)
}
