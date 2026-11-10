//! Address classification and the account registry (§5.5).
//!
//! Every stanza to a local account goes through that account's
//! [`UserActor`](crate::user::UserActor), whether it's addressed to the bare
//! or a full JID, so stanzas from one session to one entity stay in order.
//! The registry (`bare JID → mailbox`) is the only shared map; it is never
//! locked across an `.await`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::{BareJid, Jid};

use crate::disco;
use crate::stanza::{self, Condition, Kind};
use crate::store::Store;
use crate::user::{self, UserMsg};

/// Mailbox size of a [`UserActor`](crate::user::UserActor) (§5.3).
pub const USER_MAILBOX: usize = 1024;

/// Where a stanza is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The hub's own domain.
    Domain,
    /// A JID on our domain with a localpart.
    Local(Jid),
    /// `conference.<domain>` and rooms on it (M5).
    Component(Jid),
    /// Any other domain: no federation in v1.
    Remote(Jid),
}

pub fn classify(domain: &str, to: &Jid) -> Target {
    let d = to.domain().as_str();
    if d == domain {
        if to.node().is_some() {
            Target::Local(to.clone())
        } else {
            // `domain/resource` is the server too.
            Target::Domain
        }
    } else if d.strip_suffix(domain).is_some_and(|p| p.ends_with('.')) {
        Target::Component(to.clone())
    } else {
        Target::Remote(to.clone())
    }
}

#[derive(Clone)]
struct ActorHandle {
    id: u64,
    tx: mpsc::Sender<UserMsg>,
}

pub struct Router {
    domain: String,
    store: Arc<dyn Store>,
    max_resources: usize,
    users: Mutex<HashMap<BareJid, ActorHandle>>,
    next_actor: AtomicU64,
}

impl Router {
    pub fn new(domain: &str, store: Arc<dyn Store>, max_resources: usize) -> Arc<Self> {
        Arc::new(Self {
            domain: domain.to_owned(),
            store,
            max_resources,
            users: Mutex::default(),
            next_actor: AtomicU64::new(1),
        })
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn user_actors(&self) -> usize {
        self.users.lock().unwrap().len()
    }

    /// Whether `by` names an entity whose stanza-ids we stamp.
    pub fn is_our_jid(&self, jid: &str) -> bool {
        Jid::new(jid).is_ok_and(|j| classify(&self.domain, &j) != Target::Remote(j.clone()))
    }

    /// The running actor for `bare`, or a new one.
    fn actor(self: &Arc<Self>, bare: &BareJid) -> ActorHandle {
        let mut users = self.users.lock().unwrap();
        if let Some(h) = users.get(bare) {
            return h.clone();
        }
        let id = self.next_actor.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(USER_MAILBOX);
        user::spawn(bare.clone(), id, self.clone(), rx, self.max_resources);
        let h = ActorHandle { id, tx };
        users.insert(bare.clone(), h.clone());
        h
    }

    /// Called by an idle actor. Returns `false` if the registry already
    /// points at another actor.
    pub(crate) fn retire(&self, bare: &BareJid, id: u64) -> bool {
        let mut users = self.users.lock().unwrap();
        if users.get(bare).is_some_and(|h| h.id == id) {
            users.remove(bare);
            true
        } else {
            false
        }
    }

    /// Send to the actor of a local account that we know exists (it has a
    /// session, or the store said so). Retries once if the actor just retired.
    pub(crate) fn send_to_actor(self: &Arc<Self>, bare: &BareJid, msg: UserMsg) -> Result<(), UserMsg> {
        let mut msg = msg;
        for _ in 0..2 {
            let h = self.actor(bare);
            match h.tx.try_send(msg) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(m)) => return Err(m),
                Err(TrySendError::Closed(m)) => {
                    self.retire(bare, h.id);
                    msg = m;
                }
            }
        }
        Err(msg)
    }

    /// Route a stanza from a session. `from` has been stamped already and
    /// `to` (if present) parses.
    pub async fn route(self: &Arc<Self>, sender: &BareJid, el: Element) {
        let to = match el.attr("to") {
            None => None,
            Some(t) => match Jid::new(t) {
                Ok(j) => Some(j),
                Err(_) => {
                    self.bounce(&el, Condition::JidMalformed, Some(&self.domain.clone()));
                    return;
                }
            },
        };
        let Some(to) = to else {
            // No `to`: addressed to the sender's own account.
            if Kind::of(&el) == Some(Kind::Presence) {
                // Broadcast presence needs the roster (M2).
                return;
            }
            self.deliver_local(sender, el);
            return;
        };
        match classify(&self.domain, &to) {
            Target::Domain => {
                if let Some(reply) = disco::handle_domain_stanza(&self.domain, &el) {
                    self.deliver_reply(reply);
                }
            }
            Target::Local(jid) => {
                let bare = jid.to_bare();
                let running = self.users.lock().unwrap().contains_key(&bare);
                if running {
                    self.deliver_local(&bare, el);
                    return;
                }
                let exists = match bare.node() {
                    Some(node) => self.store.account(&self.domain, node.as_str()).await,
                    None => Ok(None),
                };
                match exists {
                    Ok(Some(_)) => self.deliver_local(&bare, el),
                    Ok(None) => self.no_such_account(&el, &to),
                    Err(e) => {
                        tracing::error!("account lookup failed: {e:#}");
                        self.bounce(&el, Condition::ServiceUnavailable, Some(&to.to_string()));
                    }
                }
            }
            // Rooms arrive in M5.
            Target::Component(_) => self.bounce(&el, Condition::ServiceUnavailable, Some(&to.to_string())),
            Target::Remote(_) => self.bounce(&el, Condition::RemoteServerNotFound, Some(&to.to_string())),
        }
    }

    /// RFC 6121 §8.5.1: messages and IQs to an account that doesn't exist
    /// get `service-unavailable`; presence is ignored.
    fn no_such_account(self: &Arc<Self>, el: &Element, to: &Jid) {
        let bounce = match Kind::of(el) {
            Some(Kind::Message) => !matches!(stanza::stanza_type(el), "headline" | "groupchat"),
            Some(Kind::Iq) => true,
            _ => false,
        };
        if bounce {
            self.bounce(el, Condition::ServiceUnavailable, Some(&to.to_bare().to_string()));
        }
    }

    fn deliver_local(self: &Arc<Self>, bare: &BareJid, el: Element) {
        if let Err(UserMsg::Deliver(el)) = self.send_to_actor(bare, UserMsg::Deliver(el)) {
            tracing::warn!(account = %bare, "account mailbox full; bouncing");
            self.bounce(&el, Condition::ResourceConstraint, Some(&bare.to_string()));
        }
    }

    /// Deliver a server-generated reply or error to a local full JID.
    /// Never awaits, so actors can call it from their message loop.
    pub fn deliver_reply(self: &Arc<Self>, reply: Element) {
        let Some(to) = reply.attr("to").and_then(|t| Jid::new(t).ok()) else {
            return;
        };
        let Target::Local(jid) = classify(&self.domain, &to) else {
            return;
        };
        let bare = jid.to_bare();
        let running = self.users.lock().unwrap().contains_key(&bare);
        if running {
            // Errors about errors are never generated, so a full mailbox just drops.
            let _ = self.send_to_actor(&bare, UserMsg::Deliver(reply));
        }
    }

    /// Answer `el` with a stanza error, if it may be answered.
    pub fn bounce(self: &Arc<Self>, el: &Element, cond: Condition, from: Option<&str>) {
        if let Some(reply) = stanza::error_reply(el, cond, from) {
            self.deliver_reply(reply);
        }
    }

    /// Close every session of an account (deleted account, shutdown).
    pub fn close_account(self: &Arc<Self>, bare: &BareJid, reason: user::CloseReason) {
        let h = self.users.lock().unwrap().get(bare).cloned();
        if let Some(h) = h {
            let _ = h.tx.try_send(UserMsg::CloseSessions(reason));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        let j = |s: &str| Jid::new(s).unwrap();
        assert_eq!(classify("hub.local", &j("hub.local")), Target::Domain);
        assert_eq!(classify("hub.local", &j("hub.local/x")), Target::Domain);
        assert_eq!(
            classify("hub.local", &j("a@hub.local/r")),
            Target::Local(j("a@hub.local/r"))
        );
        assert_eq!(
            classify("hub.local", &j("room@conference.hub.local")),
            Target::Component(j("room@conference.hub.local"))
        );
        assert_eq!(
            classify("hub.local", &j("a@xhub.local")),
            Target::Remote(j("a@xhub.local"))
        );
        assert_eq!(
            classify("hub.local", &j("a@example.org")),
            Target::Remote(j("a@example.org"))
        );
    }
}
