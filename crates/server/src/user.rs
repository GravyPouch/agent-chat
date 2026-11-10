//! The per-account actor (§5.3). In M1 it owns the bound resources and
//! applies the delivery rules of §5.5 with every resource at priority 0.
//! Roster, presence, carbons, PEP and offline storage join it in M2–M4.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::{BareJid, FullJid, Jid};
use xmpp_parsers::stream_error::DefinedCondition as StreamCondition;

use crate::router::Router;
use crate::stanza::{self, Condition, Kind};
use crate::util::{hex, random_bytes};

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
    Unbind {
        resource: String,
        conn_id: u64,
    },
    /// A stanza addressed to this account (bare or full JID, or no `to`).
    Deliver(Element),
    CloseSessions(CloseReason),
}

pub struct UserActor {
    bare: BareJid,
    id: u64,
    router: Arc<Router>,
    sessions: BTreeMap<String, SessionLink>,
    max_resources: usize,
}

pub(crate) fn spawn(bare: BareJid, id: u64, router: Arc<Router>, rx: mpsc::Receiver<UserMsg>, max_resources: usize) {
    let actor = UserActor {
        bare,
        id,
        router,
        sessions: BTreeMap::new(),
        max_resources,
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
            self.handle(msg);
        }
        // Retired: hand anything still queued to the next actor.
        rx.close();
        while let Some(msg) = rx.recv().await {
            match msg {
                UserMsg::Bind { reply, .. } => {
                    let _ = reply.send(Err(BindError::Retry));
                }
                UserMsg::Deliver(el) => {
                    let _ = self.router.send_to_actor(&self.bare, UserMsg::Deliver(el));
                }
                UserMsg::Unbind { .. } | UserMsg::CloseSessions(_) => {}
            }
        }
    }

    fn handle(&mut self, msg: UserMsg) {
        match msg {
            UserMsg::Bind { resource, link, reply } => {
                let _ = reply.send(self.bind(resource, link));
            }
            UserMsg::Unbind { resource, conn_id } => {
                if self.sessions.get(&resource).is_some_and(|s| s.conn_id == conn_id) {
                    self.sessions.remove(&resource);
                }
            }
            UserMsg::Deliver(el) => self.deliver(el),
            UserMsg::CloseSessions(reason) => {
                for (_, s) in std::mem::take(&mut self.sessions) {
                    s.kick.kick(reason);
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
        // A conflict replaces the old session (§5.4 step 8).
        if let Some(old) = self.sessions.insert(resource, link) {
            tracing::info!(jid = %full, "resource conflict: replacing the old session");
            old.kick.kick(CloseReason::Conflict);
        }
        Ok(full)
    }

    fn deliver(&mut self, el: Element) {
        let to = el.attr("to").and_then(|t| Jid::new(t).ok());
        let resource = to.as_ref().and_then(|j| j.resource()).map(|r| r.as_str().to_owned());
        let ty = stanza::stanza_type(&el).to_owned();
        match Kind::of(&el) {
            Some(Kind::Message) => match resource {
                Some(r) if self.sessions.contains_key(&r) => self.send(&r, el),
                // RFC 6121 §8.5.3.2.1: offline resource.
                Some(_) if matches!(ty.as_str(), "groupchat" | "headline" | "error") => {}
                _ => self.deliver_to_bare(el, &ty),
            },
            Some(Kind::Presence) => {
                // Directed presence to an online resource; the rest is M2.
                if let Some(r) = resource
                    && self.sessions.contains_key(&r)
                {
                    self.send(&r, el);
                }
            }
            Some(Kind::Iq) => match resource {
                Some(r) if self.sessions.contains_key(&r) => self.send(&r, el),
                Some(_) => {
                    let from = to.map(|j| j.to_string());
                    self.router.bounce(&el, Condition::ServiceUnavailable, from.as_deref());
                }
                None => self.account_iq(&el, &ty),
            },
            None => {}
        }
    }

    /// RFC 6121 §8.5.2 with every resource at priority 0.
    fn deliver_to_bare(&mut self, el: Element, ty: &str) {
        match ty {
            "groupchat" => {
                self.router
                    .bounce(&el, Condition::ServiceUnavailable, Some(&self.bare.to_string()));
            }
            _ if self.sessions.is_empty() => {
                // Offline storage arrives in M2; until then the sender learns it failed.
                if matches!(ty, "chat" | "normal") {
                    self.router
                        .bounce(&el, Condition::ServiceUnavailable, Some(&self.bare.to_string()));
                }
            }
            _ => {
                let resources: Vec<String> = self.sessions.keys().cloned().collect();
                let last = resources.len() - 1;
                let mut el = Some(el);
                for (i, r) in resources.iter().enumerate() {
                    let copy = if i == last { el.take().unwrap() } else { el.clone().unwrap() };
                    self.send(r, copy);
                }
            }
        }
    }

    /// IQs to the bare JID: the server answers for the account. M1 knows
    /// ping; roster, PEP, MAM, vCard and disco on accounts come later.
    fn account_iq(&self, el: &Element, ty: &str) {
        if !matches!(ty, "get" | "set") {
            return;
        }
        let bare = self.bare.to_string();
        let is_ping = ty == "get" && el.has_child("ping", xmpp_parsers::ns::PING);
        let reply = if is_ping {
            Some(stanza::iq_result(el, Some(&bare), None))
        } else {
            stanza::error_reply(el, Condition::ServiceUnavailable, Some(&bare))
        };
        if let Some(mut r) = reply {
            if el.attr("to").is_none() {
                // Replies to no-`to` requests come from no one (the account itself).
                stanza::remove_attr(&mut r, "from");
            }
            self.router.deliver_reply(r);
        }
    }

    fn send(&mut self, resource: &str, el: Element) {
        let Some(s) = self.sessions.get(resource) else { return };
        match s.outbox.try_send(el) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(el)) => {
                tracing::warn!(account = %self.bare, resource, "session outbox full; bouncing");
                self.router
                    .bounce(&el, Condition::ResourceConstraint, Some(&self.bare.to_string()));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // The session is going away; its Unbind is on the way.
                self.sessions.remove(resource);
            }
        }
    }
}
