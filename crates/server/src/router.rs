//! Address classification and the account registry (§5.5).
//!
//! Every stanza to a local account goes through that account's
//! [`UserActor`](crate::user::UserActor), whether it's addressed to the bare
//! or a full JID, so stanzas from one session to one entity stay in order.
//! The registry (`bare JID → mailbox`) is the only shared map; it is never
//! locked across an `.await`. The caps cache is shared read-mostly data.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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
    /// `conference.<domain>` and rooms on it.
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

/// Settings the actors need, from `hub.toml`.
#[derive(Debug, Clone)]
pub struct Settings {
    pub max_resources: usize,
    /// Offline messages kept per account (§6.4).
    pub offline_cap: usize,
    /// Keep user archives (XEP-0313).
    pub archive_enabled: bool,
    /// Largest MAM page (RSM `max` cap).
    pub mam_max_page: usize,
    pub scram_iterations: u32,
    /// For XEP-0077 password changes.
    pub min_password: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_resources: 10,
            offline_cap: 1000,
            archive_enabled: true,
            mam_max_page: 250,
            scram_iterations: 10_000,
            min_password: 8,
        }
    }
}

/// Handles stanzas for `conference.<domain>` (M5).
pub trait Component: Send + Sync {
    fn deliver(self: Arc<Self>, el: Element);
}

#[derive(Clone)]
struct ActorHandle {
    id: u64,
    tx: mpsc::Sender<UserMsg>,
}

pub struct Router {
    domain: String,
    store: Arc<dyn Store>,
    settings: Settings,
    users: Mutex<HashMap<BareJid, ActorHandle>>,
    next_actor: AtomicU64,
    /// XEP-0115 ver → features.
    caps: Mutex<HashMap<String, Arc<Vec<String>>>>,
    component: OnceLock<Arc<dyn Component>>,
    /// Component JIDs listed in the domain's disco#items.
    components: OnceLock<Vec<String>>,
}

impl Router {
    pub fn new(domain: &str, store: Arc<dyn Store>, settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            domain: domain.to_owned(),
            store,
            settings,
            users: Mutex::default(),
            next_actor: AtomicU64::new(1),
            caps: Mutex::default(),
            component: OnceLock::new(),
            components: OnceLock::new(),
        })
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Register the subdomain component (the MUC service) at `jid`.
    pub fn set_component(&self, jid: &str, c: Arc<dyn Component>) {
        let _ = self.component.set(c);
        let _ = self.components.set(vec![jid.to_owned()]);
    }

    /// Component JIDs, for the domain's disco#items.
    pub fn components(&self) -> &[String] {
        self.components.get().map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn user_actors(&self) -> usize {
        self.users.lock().unwrap().len()
    }

    /// Whether `by` names an entity whose stanza-ids we stamp.
    pub fn is_our_jid(&self, jid: &str) -> bool {
        Jid::new(jid).is_ok_and(|j| classify(&self.domain, &j) != Target::Remote(j.clone()))
    }

    /// Features for a caps `ver`: memory, then the store.
    pub async fn caps_lookup(&self, ver: &str) -> Option<Arc<Vec<String>>> {
        if let Some(f) = self.caps.lock().unwrap().get(ver) {
            return Some(f.clone());
        }
        let features = self.store.caps_get(ver).await.ok().flatten()?;
        let f = Arc::new(features);
        self.caps.lock().unwrap().insert(ver.to_owned(), f.clone());
        Some(f)
    }

    /// Remember verified caps.
    pub async fn caps_store(&self, ver: &str, features: Vec<String>) -> Arc<Vec<String>> {
        if let Err(e) = self.store.caps_put(ver, features.clone()).await {
            tracing::warn!("storing caps failed: {e:#}");
        }
        let f = Arc::new(features);
        self.caps.lock().unwrap().insert(ver.to_owned(), f.clone());
        f
    }

    /// The running actor for `bare`, or a new one.
    fn actor(self: &Arc<Self>, bare: &BareJid) -> ActorHandle {
        let mut users = self.users.lock().unwrap();
        if let Some(h) = users.get(bare) {
            return h.clone();
        }
        let id = self.next_actor.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(USER_MAILBOX);
        user::spawn(bare.clone(), id, self.clone(), rx, self.settings.max_resources);
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
    // The message comes back on failure so the caller can bounce it; failure is rare.
    #[allow(clippy::result_large_err)]
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

    /// Whether `bare` is a local account (running actor, or in the store).
    pub async fn account_exists(&self, bare: &BareJid) -> bool {
        if self.users.lock().unwrap().contains_key(bare) {
            return true;
        }
        match bare.node() {
            Some(node) => matches!(self.store.account(&self.domain, node.as_str()).await, Ok(Some(_))),
            None => false,
        }
    }

    /// Route a stanza to its `to` (§5.5). The sender's actor has already
    /// done the outbound processing, and `from` is set.
    pub async fn route(self: &Arc<Self>, el: Element) {
        let Some(to) = el.attr("to") else { return };
        let to = match Jid::new(to) {
            Ok(j) => j,
            Err(_) => {
                self.bounce(&el, Condition::JidMalformed, Some(&self.domain.clone()));
                return;
            }
        };
        match classify(&self.domain, &to) {
            Target::Domain => {
                if let Some(reply) = disco::handle_domain_stanza(&self.domain, self.components(), &el) {
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
            Target::Component(_) => match self.component.get() {
                Some(c) => c.clone().deliver(el),
                None => self.bounce(&el, Condition::ServiceUnavailable, Some(&to.to_string())),
            },
            Target::Remote(_) => self.bounce(&el, Condition::RemoteServerNotFound, Some(&to.to_string())),
        }
    }

    /// RFC 6121 §8.5.1: messages and IQs to an account that doesn't exist
    /// get `service-unavailable`; presence is ignored, except that a
    /// subscription request is answered with `unsubscribed`.
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

    /// Deliver a server-generated reply or error to a local JID, or to the
    /// MUC component. Never awaits, so actors can call it from their loop.
    pub fn deliver_reply(self: &Arc<Self>, reply: Element) {
        let Some(to) = reply.attr("to").and_then(|t| Jid::new(t).ok()) else {
            return;
        };
        match classify(&self.domain, &to) {
            Target::Local(jid) => {
                let bare = jid.to_bare();
                // Errors about errors are never generated, so a full mailbox just drops.
                let _ = self.send_to_actor(&bare, UserMsg::Deliver(reply));
            }
            Target::Component(_) => {
                if let Some(c) = self.component.get() {
                    c.clone().deliver(reply);
                }
            }
            _ => {}
        }
    }

    /// Hand a message to a local account's actor without the existence
    /// check (the caller knows the account exists).
    pub fn deliver_to_account(self: &Arc<Self>, bare: &BareJid, msg: UserMsg) {
        if self.send_to_actor(bare, msg).is_err() {
            tracing::warn!(account = %bare, "account mailbox full; dropping a server message");
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
