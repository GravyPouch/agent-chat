//! XEP-0045 multi-user chat on `conference.<domain>` (§5.7 MUC).
//!
//! [`MucService`] is the router's component: it answers the service's own
//! disco and hands everything addressed to a room (or an occupant) to that
//! room's actor ([`room::RoomActor`]), started on demand. A room actor owns
//! the room's configuration, affiliations, occupants and subject, archives
//! its messages (room MAM) and talks to occupants through the router.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::ns;

use crate::forms::{self, FormBuilder};
use crate::router::{Component, Router};
use crate::stanza::{self, Condition, Kind};
use crate::store::Store;

pub mod room;

pub const NS_MUC: &str = "http://jabber.org/protocol/muc";
pub const NS_MUC_USER: &str = "http://jabber.org/protocol/muc#user";
pub const NS_MUC_OWNER: &str = "http://jabber.org/protocol/muc#owner";
pub const NS_MUC_ADMIN: &str = "http://jabber.org/protocol/muc#admin";
pub const NS_ROOMCONFIG: &str = "http://jabber.org/protocol/muc#roomconfig";
pub const NS_ROOMINFO: &str = "http://jabber.org/protocol/muc#roominfo";

/// Mailbox size of a room actor (§5.3).
pub const ROOM_MAILBOX: usize = 2048;

/// Settings from `[muc]` in `hub.toml`.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Who may create rooms: local accounts, or admins only.
    pub admins_only: bool,
    /// New rooms are persistent unless configured otherwise.
    pub default_persistent: bool,
    /// Archive every room (like Prosody's `muc_log_all_rooms`).
    pub archive: bool,
    /// History sent on join when the client doesn't limit it.
    pub default_history: usize,
    pub max_occupants: usize,
    pub mam_max_page: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Affiliation {
    Outcast,
    None,
    Member,
    Admin,
    Owner,
}

impl Affiliation {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "owner" => Affiliation::Owner,
            "admin" => Affiliation::Admin,
            "member" => Affiliation::Member,
            "outcast" => Affiliation::Outcast,
            "none" => Affiliation::None,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Affiliation::Owner => "owner",
            Affiliation::Admin => "admin",
            Affiliation::Member => "member",
            Affiliation::Outcast => "outcast",
            Affiliation::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    None,
    Visitor,
    Participant,
    Moderator,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "moderator" => Role::Moderator,
            "participant" => Role::Participant,
            "visitor" => Role::Visitor,
            "none" => Role::None,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Moderator => "moderator",
            Role::Participant => "participant",
            Role::Visitor => "visitor",
            Role::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Whois {
    Moderators,
    Anyone,
}

/// A submitted configuration value the hub can't use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidValue;

/// A room's configuration (`muc#roomconfig`), stored as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoomConfig {
    pub name: String,
    pub description: String,
    pub persistent: bool,
    pub public: bool,
    pub members_only: bool,
    pub moderated: bool,
    pub whois: Whois,
    pub password: Option<String>,
    pub allow_invites: bool,
    pub change_subject: bool,
    pub max_users: usize,
    pub logging: bool,
    /// Who may send private messages: anyone, participants, moderators, none.
    pub allow_pm: String,
    pub language: String,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            persistent: true,
            public: true,
            members_only: false,
            moderated: false,
            whois: Whois::Moderators,
            password: None,
            allow_invites: false,
            change_subject: false,
            max_users: 200,
            logging: true,
            allow_pm: "anyone".into(),
            language: "en".into(),
        }
    }
}

impl RoomConfig {
    pub fn non_anonymous(&self) -> bool {
        self.whois == Whois::Anyone
    }

    /// The configuration form (XEP-0045 §10.2).
    pub fn form(&self, max_users: usize) -> Element {
        let mut b = FormBuilder::new("form", NS_ROOMCONFIG).title("Room configuration");
        b.field("muc#roomconfig_roomname", "text-single", Some("Name"), &[&self.name]);
        b.field(
            "muc#roomconfig_roomdesc",
            "text-single",
            Some("Description"),
            &[&self.description],
        );
        b.boolean("muc#roomconfig_persistentroom", Some("Persistent"), self.persistent);
        b.boolean(
            "muc#roomconfig_publicroom",
            Some("Listed in the directory"),
            self.public,
        );
        b.boolean("muc#roomconfig_membersonly", Some("Members only"), self.members_only);
        b.boolean("muc#roomconfig_moderatedroom", Some("Moderated"), self.moderated);
        b.list(
            "muc#roomconfig_whois",
            Some("Who may see real JIDs"),
            match self.whois {
                Whois::Anyone => "anyone",
                Whois::Moderators => "moderators",
            },
            &["moderators", "anyone"],
        );
        b.boolean(
            "muc#roomconfig_passwordprotectedroom",
            Some("Password protected"),
            self.password.is_some(),
        );
        b.field(
            "muc#roomconfig_roomsecret",
            "text-private",
            Some("Password"),
            &[self.password.as_deref().unwrap_or("")],
        );
        b.boolean(
            "muc#roomconfig_allowinvites",
            Some("Members may invite"),
            self.allow_invites,
        );
        b.boolean(
            "muc#roomconfig_changesubject",
            Some("Participants may change the subject"),
            self.change_subject,
        );
        let max = self.max_users.to_string();
        let limit = max_users.to_string();
        b.list(
            "muc#roomconfig_maxusers",
            Some("Maximum occupants"),
            &max,
            &["10", "20", "50", "100", &limit],
        );
        b.boolean("muc#roomconfig_enablelogging", Some("Archive messages"), self.logging);
        b.list(
            "muc#roomconfig_allowpm",
            Some("Who may send private messages"),
            &self.allow_pm,
            &["anyone", "participants", "moderators", "none"],
        );
        b.field(
            "muc#roomconfig_lang",
            "text-single",
            Some("Language"),
            &[&self.language],
        );
        b.build()
    }

    /// Apply a submitted form; unknown fields are ignored. `Err` on a value
    /// we can't use.
    pub fn apply(&mut self, v: &BTreeMap<String, Vec<String>>, max_users: usize) -> Result<(), InvalidValue> {
        let b = |s: &str| forms::parse_bool(s).ok_or(InvalidValue);
        let mut protected = self.password.is_some();
        let mut secret = self.password.clone();
        for (var, vals) in v {
            let val = vals.first().map(String::as_str).unwrap_or("");
            match var.as_str() {
                "muc#roomconfig_roomname" => self.name = val.to_owned(),
                "muc#roomconfig_roomdesc" => self.description = val.to_owned(),
                "muc#roomconfig_persistentroom" => self.persistent = b(val)?,
                "muc#roomconfig_publicroom" => self.public = b(val)?,
                "muc#roomconfig_membersonly" => self.members_only = b(val)?,
                "muc#roomconfig_moderatedroom" => self.moderated = b(val)?,
                "muc#roomconfig_whois" => {
                    self.whois = match val {
                        "anyone" => Whois::Anyone,
                        "moderators" => Whois::Moderators,
                        _ => return Err(InvalidValue),
                    }
                }
                "muc#roomconfig_passwordprotectedroom" => protected = b(val)?,
                "muc#roomconfig_roomsecret" => secret = Some(val.to_owned()).filter(|s| !s.is_empty()),
                "muc#roomconfig_allowinvites" => self.allow_invites = b(val)?,
                "muc#roomconfig_changesubject" => self.change_subject = b(val)?,
                "muc#roomconfig_maxusers" => {
                    let n: usize = if val == "none" {
                        max_users
                    } else {
                        val.parse().map_err(|_| InvalidValue)?
                    };
                    self.max_users = n.clamp(1, max_users);
                }
                "muc#roomconfig_enablelogging" => self.logging = b(val)?,
                "muc#roomconfig_allowpm" => {
                    if !matches!(val, "anyone" | "participants" | "moderators" | "none") {
                        return Err(InvalidValue);
                    }
                    self.allow_pm = val.to_owned();
                }
                "muc#roomconfig_lang" => self.language = val.to_owned(),
                _ => {}
            }
        }
        self.password = if protected { secret } else { None };
        Ok(())
    }

    /// disco#info features (XEP-0045 §6.4).
    pub fn features(&self, archive: bool) -> Vec<&'static str> {
        let mut f = vec![
            NS_MUC,
            "http://jabber.org/protocol/muc#stable_id",
            "http://jabber.org/protocol/muc#self-ping-optimization",
            ns::DISCO_INFO,
            ns::DISCO_ITEMS,
            "urn:xmpp:sid:0",
            if self.persistent {
                "muc_persistent"
            } else {
                "muc_temporary"
            },
            if self.public { "muc_public" } else { "muc_hidden" },
            if self.members_only {
                "muc_membersonly"
            } else {
                "muc_open"
            },
            if self.moderated {
                "muc_moderated"
            } else {
                "muc_unmoderated"
            },
            if self.non_anonymous() {
                "muc_nonanonymous"
            } else {
                "muc_semianonymous"
            },
            if self.password.is_some() {
                "muc_passwordprotected"
            } else {
                "muc_unsecured"
            },
        ];
        if archive && self.logging {
            f.extend([crate::mam::NS_MAM, crate::mam::NS_MAM_EXTENDED]);
        }
        f
    }
}

pub(crate) enum RoomMsg {
    Stanza(Element),
}

#[derive(Clone)]
struct RoomHandle {
    id: u64,
    tx: mpsc::Sender<RoomMsg>,
}

/// The `conference.<domain>` component.
pub struct MucService {
    host: String,
    domain: String,
    router: Weak<Router>,
    store: Arc<dyn Store>,
    settings: Settings,
    rooms: Mutex<HashMap<BareJid, RoomHandle>>,
    next_id: AtomicU64,
}

impl MucService {
    pub fn new(host: &str, router: &Arc<Router>, settings: Settings) -> Arc<Self> {
        Arc::new(Self {
            host: host.to_owned(),
            domain: router.domain().to_owned(),
            router: Arc::downgrade(router),
            store: router.store().clone(),
            settings,
            rooms: Mutex::default(),
            next_id: AtomicU64::new(1),
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn active_rooms(&self) -> usize {
        self.rooms.lock().unwrap().len()
    }

    fn router(&self) -> Option<Arc<Router>> {
        self.router.upgrade()
    }

    fn reply(&self, el: Element) {
        if let Some(r) = self.router() {
            r.deliver_reply(el);
        }
    }

    fn bounce(&self, el: &Element, cond: Condition) {
        if let Some(r) = self.router() {
            r.bounce(el, cond, None);
        }
    }

    /// The room's actor, started (and loaded) on demand.
    fn room(self: &Arc<Self>, jid: &BareJid) -> Option<RoomHandle> {
        let router = self.router()?;
        let mut rooms = self.rooms.lock().unwrap();
        if let Some(h) = rooms.get(jid) {
            return Some(h.clone());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(ROOM_MAILBOX);
        room::spawn(jid.clone(), id, self.clone(), router, rx);
        let h = RoomHandle { id, tx };
        rooms.insert(jid.clone(), h.clone());
        Some(h)
    }

    /// Called by an idle room actor.
    pub(crate) fn retire(&self, jid: &BareJid, id: u64) -> bool {
        let mut rooms = self.rooms.lock().unwrap();
        if rooms.get(jid).is_some_and(|h| h.id == id) {
            rooms.remove(jid);
            true
        } else {
            false
        }
    }

    fn to_room(self: &Arc<Self>, room: &BareJid, el: Element) {
        let mut msg = RoomMsg::Stanza(el);
        for _ in 0..2 {
            let Some(h) = self.room(room) else { return };
            match h.tx.try_send(msg) {
                Ok(()) => return,
                Err(TrySendError::Full(RoomMsg::Stanza(el))) => {
                    tracing::warn!(room = %room, "room mailbox full; bouncing");
                    self.bounce(&el, Condition::ResourceConstraint);
                    return;
                }
                Err(TrySendError::Closed(m)) => {
                    self.retire(room, h.id);
                    msg = m;
                }
            }
        }
    }

    /// Stanzas to the service itself: disco, ping.
    fn service_stanza(&self, el: &Element) {
        if Kind::of(el) != Some(Kind::Iq) || !matches!(stanza::stanza_type(el), "get" | "set") {
            return;
        }
        let payload = el.children().next();
        let reply = match (stanza::stanza_type(el), payload) {
            ("get", Some(p)) if p.is("ping", ns::PING) => stanza::iq_result(el, Some(&self.host), None),
            ("get", Some(p)) if p.is("query", ns::DISCO_INFO) && p.attr("node").is_none() => {
                let mut q = Element::bare("query", ns::DISCO_INFO);
                q.append_child(stanza::element(
                    "identity",
                    ns::DISCO_INFO,
                    &[("category", "conference"), ("type", "text"), ("name", "Chatrooms")],
                ));
                for f in [NS_MUC, ns::DISCO_INFO, ns::DISCO_ITEMS, ns::PING, "urn:xmpp:sid:0"] {
                    q.append_child(stanza::element("feature", ns::DISCO_INFO, &[("var", f)]));
                }
                stanza::iq_result(el, Some(&self.host), Some(q))
            }
            ("get", Some(p)) if p.is("query", ns::DISCO_ITEMS) => {
                // Listing public rooms needs the store; answer from a task.
                let store = self.store.clone();
                let router = self.router.clone();
                let el = el.clone();
                let host = self.host.clone();
                tokio::spawn(async move {
                    let mut q = Element::bare("query", ns::DISCO_ITEMS);
                    for row in store.muc_rooms().await.unwrap_or_default() {
                        let cfg: RoomConfig = serde_json::from_str(&row.config_json).unwrap_or_default();
                        if cfg.public {
                            let mut item = stanza::element("item", ns::DISCO_ITEMS, &[("jid", &row.jid)]);
                            if !cfg.name.is_empty() {
                                stanza::set_attr(&mut item, "name", cfg.name.as_str());
                            }
                            q.append_child(item);
                        }
                    }
                    if let Some(r) = router.upgrade() {
                        r.deliver_reply(stanza::iq_result(&el, Some(&host), Some(q)));
                    }
                });
                return;
            }
            _ => match stanza::error_reply(el, Condition::ServiceUnavailable, Some(&self.host)) {
                Some(e) => e,
                None => return,
            },
        };
        self.reply(reply);
    }
}

impl Component for MucService {
    fn deliver(self: Arc<Self>, el: Element) {
        let Some(to) = el.attr("to").and_then(|t| Jid::new(t).ok()) else {
            return;
        };
        if to.domain().as_str() != self.host {
            self.bounce(&el, Condition::ServiceUnavailable);
            return;
        }
        // Only local accounts reach the service (no federation in v1).
        let from_local = el
            .attr("from")
            .and_then(|f| Jid::new(f).ok())
            .is_some_and(|f| f.domain().as_str() == self.domain || f.domain().as_str() == self.host);
        if !from_local {
            self.bounce(&el, Condition::NotAllowed);
            return;
        }
        match to.node() {
            None => self.service_stanza(&el),
            Some(_) => self.to_room(&to.to_bare(), el),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), vec![v.to_string()]))
            .collect()
    }

    #[test]
    fn private_room_config_as_our_client_submits_it() {
        let mut c = RoomConfig::default();
        c.apply(
            &v(&[
                ("FORM_TYPE", NS_ROOMCONFIG),
                ("muc#roomconfig_membersonly", "1"),
                ("muc#roomconfig_whois", "anyone"),
                ("muc#roomconfig_persistentroom", "1"),
                ("muc#roomconfig_publicroom", "0"),
            ]),
            200,
        )
        .unwrap();
        assert!(c.members_only && c.non_anonymous() && c.persistent && !c.public);
        let f = c.features(true);
        for want in ["muc_membersonly", "muc_nonanonymous", "urn:xmpp:mam:2", "muc_hidden"] {
            assert!(f.contains(&want), "{want}");
        }
        // The form reflects the config and applies back to the same thing.
        let mut back = RoomConfig::default();
        back.apply(&forms::values(&c.form(200)), 200).unwrap();
        assert_eq!(back, c);
        assert!(c.clone().apply(&v(&[("muc#roomconfig_whois", "nobody")]), 200).is_err());
    }

    #[test]
    fn passwords_need_the_protected_flag() {
        let mut c = RoomConfig::default();
        c.apply(
            &v(&[
                ("muc#roomconfig_passwordprotectedroom", "1"),
                ("muc#roomconfig_roomsecret", "s3cret"),
            ]),
            200,
        )
        .unwrap();
        assert_eq!(c.password.as_deref(), Some("s3cret"));
        c.apply(&v(&[("muc#roomconfig_passwordprotectedroom", "0")]), 200)
            .unwrap();
        assert_eq!(c.password, None);
    }

    #[test]
    fn affiliation_order() {
        assert!(Affiliation::Owner > Affiliation::Admin);
        assert!(Affiliation::Member > Affiliation::None);
        assert!(Affiliation::None > Affiliation::Outcast);
        assert_eq!(Affiliation::parse("member"), Some(Affiliation::Member));
    }
}
