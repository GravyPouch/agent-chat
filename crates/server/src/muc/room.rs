//! One room's actor (§5.3): configuration, affiliations, occupants, subject,
//! history and the room archive. Occupants live in memory only; after a hub
//! restart clients rejoin (XEP-0410 helps them notice).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::ns;

use super::{
    Affiliation, MucService, NS_MUC, NS_MUC_ADMIN, NS_MUC_OWNER, NS_MUC_USER, NS_ROOMINFO, Role, RoomConfig, RoomMsg,
};
use crate::forms::{self, FormBuilder};
use crate::mam;
use crate::router::Router;
use crate::stanza::{self, Condition, Kind, NS_CLIENT};
use crate::store::{ArchiveQuery, ArchiveRow, Direction, RoomAffiliation, RoomRow};
use crate::user::message::{delay, has_body, is_encrypted, remove_children};
use crate::util::now_ms;

/// An empty room's actor stops after this long.
const IDLE: Duration = Duration::from_secs(60);

struct Occupant {
    /// Real bare JID.
    bare: String,
    /// The real full JIDs joined under this nick (one per client session).
    sessions: Vec<String>,
    role: Role,
    /// The last presence, without the MUC payloads.
    presence: Element,
}

pub struct RoomActor {
    jid: BareJid,
    jid_str: String,
    id: u64,
    svc: Arc<MucService>,
    router: Arc<Router>,
    /// The room exists (stored, or created and not yet destroyed).
    exists: bool,
    /// Created and not configured yet (XEP-0045 §10.1).
    locked: bool,
    config: RoomConfig,
    subject: Option<String>,
    subject_from: Option<String>,
    created_ms: i64,
    affiliations: BTreeMap<String, Affiliation>,
    occupants: BTreeMap<String, Occupant>,
}

pub(crate) fn spawn(jid: BareJid, id: u64, svc: Arc<MucService>, router: Arc<Router>, rx: mpsc::Receiver<RoomMsg>) {
    let actor = RoomActor {
        jid_str: jid.to_string(),
        jid,
        id,
        svc,
        router,
        exists: false,
        locked: false,
        config: RoomConfig::default(),
        subject: None,
        subject_from: None,
        created_ms: 0,
        affiliations: BTreeMap::new(),
        occupants: BTreeMap::new(),
    };
    tokio::spawn(actor.run(rx));
}

/// Copy `el` without the given direct children.
fn without(el: &Element, pred: impl Fn(&Element) -> bool) -> Element {
    let mut c = el.clone();
    remove_children(&mut c, pred);
    c
}

fn status(code: u16) -> Element {
    stanza::element("status", NS_MUC_USER, &[("code", &code.to_string())])
}

impl RoomActor {
    async fn run(mut self, mut rx: mpsc::Receiver<RoomMsg>) {
        self.load().await;
        loop {
            let msg = if self.occupants.is_empty() {
                match tokio::time::timeout(IDLE, rx.recv()).await {
                    Ok(Some(m)) => m,
                    Ok(None) => return,
                    Err(_) => {
                        if self.svc.retire(&self.jid, self.id) {
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
            let RoomMsg::Stanza(el) = msg;
            self.handle(el).await;
            if !self.exists && self.occupants.is_empty() && self.svc.retire(&self.jid, self.id) {
                break;
            }
        }
        // Retired: hand anything still queued to the next actor.
        rx.close();
        while let Some(RoomMsg::Stanza(el)) = rx.recv().await {
            crate::router::Component::deliver(self.svc.clone(), el);
        }
    }

    async fn load(&mut self) {
        let store = self.router.store().clone();
        match store.muc_room(&self.jid_str).await {
            Ok(Some(row)) => {
                self.exists = true;
                self.config = serde_json::from_str(&row.config_json).unwrap_or_default();
                self.subject = row.subject;
                self.subject_from = row.subject_from;
                self.created_ms = row.created_ms;
                for a in store.muc_affiliations(&self.jid_str).await.unwrap_or_default() {
                    if let Some(aff) = Affiliation::parse(&a.affiliation) {
                        self.affiliations.insert(a.jid, aff);
                    }
                }
            }
            Ok(None) => {}
            Err(e) => tracing::error!(room = %self.jid, "loading the room failed: {e:#}"),
        }
    }

    /// Store the room (persistent rooms only, once unlocked).
    async fn save(&self) {
        if !self.exists || self.locked || !self.config.persistent {
            return;
        }
        let row = RoomRow {
            jid: self.jid_str.clone(),
            config_json: serde_json::to_string(&self.config).unwrap_or_else(|_| "{}".into()),
            subject: self.subject.clone(),
            subject_from: self.subject_from.clone(),
            created_ms: self.created_ms,
            last_active_ms: now_ms(),
        };
        if let Err(e) = self.router.store().muc_room_put(row).await {
            tracing::error!(room = %self.jid, "saving the room failed: {e:#}");
        }
    }

    async fn save_affiliation(&self, jid: &str, aff: Affiliation) {
        if !self.exists || self.locked || !self.config.persistent {
            return;
        }
        let row = RoomAffiliation {
            jid: jid.to_owned(),
            affiliation: aff.as_str().to_owned(),
            nick: None,
            reason: None,
        };
        let remove = aff == Affiliation::None;
        if let Err(e) = self
            .router
            .store()
            .muc_affiliation_set(&self.jid_str, row, remove)
            .await
        {
            tracing::error!(room = %self.jid, "saving an affiliation failed: {e:#}");
        }
    }

    async fn save_all(&self) {
        self.save().await;
        for (jid, aff) in &self.affiliations {
            self.save_affiliation(jid, *aff).await;
        }
    }

    // ---------------------------------------------------------- helpers

    fn affiliation(&self, bare: &str) -> Affiliation {
        self.affiliations.get(bare).copied().unwrap_or(Affiliation::None)
    }

    fn nick_of(&self, full: &str) -> Option<String> {
        self.occupants
            .iter()
            .find(|(_, o)| o.sessions.iter().any(|s| s == full))
            .map(|(n, _)| n.clone())
    }

    fn occupant_jid(&self, nick: &str) -> String {
        format!("{}/{nick}", self.jid_str)
    }

    fn send(&self, el: Element) {
        self.router.deliver_reply(el);
    }

    fn error(&self, el: &Element, cond: Condition) {
        self.router.bounce(el, cond, None);
    }

    fn reply(&self, req: &Element, payload: Option<Element>) {
        self.send(stanza::iq_result(req, None, payload));
    }

    /// Whether a recipient with `role` sees real JIDs.
    fn shows_jids_to(&self, role: Role) -> bool {
        self.config.non_anonymous() || role == Role::Moderator
    }

    /// Presence about occupant `nick` for one recipient session.
    fn occupant_presence(&self, nick: &str, to: &str, show_jid: bool, codes: &[u16], ty: Option<&str>) -> Element {
        let o = &self.occupants[nick];
        let mut p = o.presence.clone();
        stanza::remove_attr(&mut p, "type");
        if let Some(t) = ty {
            stanza::set_attr(&mut p, "type", t);
        }
        stanza::set_attr(&mut p, "from", self.occupant_jid(nick));
        stanza::set_attr(&mut p, "to", to);
        let mut x = Element::bare("x", NS_MUC_USER);
        let aff = self.affiliation(&o.bare);
        let mut item = stanza::element(
            "item",
            NS_MUC_USER,
            &[("affiliation", aff.as_str()), ("role", o.role.as_str())],
        );
        if show_jid && let Some(s) = o.sessions.first() {
            stanza::set_attr(&mut item, "jid", s.as_str());
        }
        x.append_child(item);
        for c in codes {
            x.append_child(status(*c));
        }
        p.append_child(x);
        p
    }

    /// Send occupant `nick`'s presence to every occupant session; the
    /// occupant's own sessions get `self_codes` too.
    fn broadcast_presence(&self, nick: &str, codes: &[u16], self_codes: &[u16], ty: Option<&str>) {
        for (other, o) in &self.occupants {
            for s in &o.sessions {
                let mut c = codes.to_vec();
                if other == nick {
                    c.extend_from_slice(self_codes);
                }
                self.send(self.occupant_presence(nick, s, self.shows_jids_to(o.role) || other == nick, &c, ty));
            }
        }
    }

    fn sessions(&self) -> Vec<(String, Role)> {
        self.occupants
            .values()
            .flat_map(|o| o.sessions.iter().map(move |s| (s.clone(), o.role)))
            .collect()
    }

    fn default_role(&self, aff: Affiliation) -> Role {
        match aff {
            Affiliation::Owner | Affiliation::Admin => Role::Moderator,
            _ if self.config.moderated && aff < Affiliation::Member => Role::Visitor,
            _ => Role::Participant,
        }
    }

    // --------------------------------------------------------- dispatch

    async fn handle(&mut self, el: Element) {
        match Kind::of(&el) {
            Some(Kind::Presence) => self.presence(el).await,
            Some(Kind::Message) => self.message(el).await,
            Some(Kind::Iq) => self.iq(el).await,
            None => {}
        }
    }

    // --------------------------------------------------------- presence

    async fn presence(&mut self, el: Element) {
        let Some(from) = el.attr("from").map(str::to_owned) else {
            return;
        };
        let to = el.attr("to").and_then(|t| Jid::new(t).ok());
        let nick = to.as_ref().and_then(|t| t.resource()).map(|r| r.as_str().to_owned());
        match stanza::stanza_type(&el) {
            "available" => match nick {
                Some(n) => self.join_or_update(&from, &n, el).await,
                None => self.error(&el, Condition::JidMalformed),
            },
            "unavailable" | "error" => self.leave(&from, &el).await,
            _ => {}
        }
    }

    async fn may_create(&self, from: &Jid) -> bool {
        if from.domain().as_str() != self.router.domain() {
            return false;
        }
        if !self.svc.settings.admins_only {
            return true;
        }
        let Some(node) = from.node() else { return false };
        matches!(
            self.router.store().account(self.router.domain(), node.as_str()).await,
            Ok(Some(a)) if a.role == crate::store::Role::Admin
        )
    }

    async fn join_or_update(&mut self, from: &str, nick: &str, el: Element) {
        let Ok(from_jid) = Jid::new(from) else { return };
        let bare = from_jid.to_bare().to_string();
        let presence = without(&el, |c| c.ns() == NS_MUC || c.ns() == NS_MUC_USER);
        if let Some(cur) = self.nick_of(from) {
            if cur == nick {
                // A presence update: broadcast it.
                if let Some(o) = self.occupants.get_mut(nick) {
                    o.presence = presence;
                }
                self.broadcast_presence(nick, &[], &[110], None);
            } else {
                self.change_nick(&cur, nick, presence, &el);
            }
            return;
        }

        // A new occupant; maybe a new room.
        let mut created = false;
        if !self.exists {
            if !self.may_create(&from_jid).await {
                self.error(&el, Condition::NotAllowed);
                return;
            }
            self.exists = true;
            self.locked = true;
            self.created_ms = now_ms();
            self.config = RoomConfig {
                persistent: self.svc.settings.default_persistent,
                logging: self.svc.settings.archive,
                max_users: self.svc.settings.max_occupants,
                ..RoomConfig::default()
            };
            self.affiliations.insert(bare.clone(), Affiliation::Owner);
            created = true;
            tracing::info!(room = %self.jid, "room created");
        }
        let aff = self.affiliation(&bare);
        let cond = if self.locked && aff != Affiliation::Owner {
            Some(Condition::ItemNotFound)
        } else if aff == Affiliation::Outcast {
            Some(Condition::Forbidden)
        } else if self.config.members_only && aff < Affiliation::Member {
            Some(Condition::RegistrationRequired)
        } else if self.occupants.get(nick).is_some_and(|o| o.bare != bare) {
            Some(Condition::Conflict)
        } else if self.occupants.len() >= self.config.max_users && aff < Affiliation::Admin {
            Some(Condition::ServiceUnavailable)
        } else {
            None
        };
        if let Some(c) = cond {
            self.error(&el, c);
            return;
        }
        let muc = el.get_child("x", NS_MUC);
        if let Some(pw) = &self.config.password {
            let given = muc.and_then(|x| x.get_child("password", NS_MUC)).map(|p| p.text());
            if given.as_deref() != Some(pw.as_str()) {
                self.error(&el, Condition::NotAuthorized);
                return;
            }
        }
        let history = muc.and_then(|x| x.get_child("history", NS_MUC)).cloned();
        let role = self.default_role(aff);
        let existing_nicks: Vec<String> = self.occupants.keys().cloned().collect();
        match self.occupants.get_mut(nick) {
            // Another session of the same person under the same nick.
            Some(o) => {
                o.sessions.push(from.to_owned());
                o.presence = presence;
            }
            None => {
                self.occupants.insert(
                    nick.to_owned(),
                    Occupant {
                        bare: bare.clone(),
                        sessions: vec![from.to_owned()],
                        role,
                        presence,
                    },
                );
            }
        }
        // 1. Existing occupants to the newcomer.
        let joiner_sees_jids = self.shows_jids_to(role);
        for other in existing_nicks.iter().filter(|n| n.as_str() != nick) {
            self.send(self.occupant_presence(other, from, joiner_sees_jids, &[], None));
        }
        // 2. The newcomer to everyone else, 3. and to itself with 110.
        let mut self_codes = vec![110];
        if self.config.non_anonymous() {
            self_codes.push(100);
        }
        if self.config.logging && self.svc.settings.archive {
            self_codes.push(170);
        }
        if created {
            self_codes.push(201);
        }
        for (other, o) in &self.occupants {
            if other == nick {
                continue;
            }
            for s in &o.sessions {
                self.send(self.occupant_presence(nick, s, self.shows_jids_to(o.role), &[], None));
            }
        }
        self.send(self.occupant_presence(nick, from, true, &self_codes, None));
        if created {
            return;
        }
        // 4. History, 5. the subject.
        self.send_history(from, history.as_ref()).await;
        let mut subject = stanza::element("message", NS_CLIENT, &[("type", "groupchat"), ("to", from)]);
        let subject_from = self
            .subject_from
            .as_deref()
            .map(|n| self.occupant_jid(n))
            .unwrap_or_else(|| self.jid_str.clone());
        stanza::set_attr(&mut subject, "from", subject_from);
        stanza::set_attr(&mut subject, "id", crate::user::server_id("subject"));
        let mut s = Element::bare("subject", NS_CLIENT);
        if let Some(text) = &self.subject {
            s.append_text(text.as_str());
        }
        subject.append_child(s);
        self.send(subject);
    }

    fn change_nick(&mut self, old: &str, new: &str, presence: Element, el: &Element) {
        let Some(o) = self.occupants.get(old) else { return };
        if self.occupants.get(new).is_some_and(|n| n.bare != o.bare) {
            self.error(el, Condition::Conflict);
            return;
        }
        if o.sessions.len() > 1 {
            // Renaming one of several sessions isn't supported.
            self.error(el, Condition::NotAcceptable);
            return;
        }
        // XEP-0045 §7.6: unavailable with 303 and the new nick, then available.
        for (other, oc) in &self.occupants {
            for s in &oc.sessions {
                let mut p = self.occupant_presence(
                    old,
                    s,
                    self.shows_jids_to(oc.role) || other == old,
                    &[303],
                    Some("unavailable"),
                );
                if let Some(x) = p.get_child_mut("x", NS_MUC_USER)
                    && let Some(item) = x.get_child_mut("item", NS_MUC_USER)
                {
                    stanza::set_attr(item, "nick", new);
                }
                if other == old
                    && let Some(x) = p.get_child_mut("x", NS_MUC_USER)
                {
                    x.append_child(status(110));
                }
                self.send(p);
            }
        }
        let mut o = self.occupants.remove(old).unwrap();
        o.presence = presence;
        self.occupants.insert(new.to_owned(), o);
        self.broadcast_presence(new, &[], &[110], None);
    }

    async fn leave(&mut self, from: &str, el: &Element) {
        let Some(nick) = self.nick_of(from) else { return };
        let o = self.occupants.get_mut(&nick).unwrap();
        o.sessions.retain(|s| s != from);
        let mut out = without(el, |c| c.ns() == NS_MUC || c.ns() == NS_MUC_USER);
        stanza::remove_attr(&mut out, "type");
        if !o.sessions.is_empty() {
            // Another session keeps the nick; only this one gets its unavailable.
            let mut p = self.occupant_presence(&nick, from, true, &[110], Some("unavailable"));
            stanza::set_attr(&mut p, "to", from);
            self.send(p);
            return;
        }
        o.presence = out;
        // Everyone (and the leaver, with 110) sees unavailable.
        let mut p = self.occupant_presence(&nick, from, true, &[110], Some("unavailable"));
        stanza::set_attr(&mut p, "to", from);
        self.send(p);
        self.broadcast_presence(&nick, &[], &[], Some("unavailable"));
        self.occupants.remove(&nick);
        if self.occupants.is_empty() && (self.locked || !self.config.persistent) {
            self.destroy_storage().await;
        }
    }

    async fn destroy_storage(&mut self) {
        if let Err(e) = self.router.store().muc_room_delete(&self.jid_str).await {
            tracing::error!(room = %self.jid, "deleting the room failed: {e:#}");
        }
        self.exists = false;
        self.locked = false;
        self.affiliations.clear();
        self.subject = None;
        self.subject_from = None;
        tracing::info!(room = %self.jid, "room destroyed");
    }

    async fn send_history(&self, to: &str, history: Option<&Element>) {
        let attr = |name: &'static str| history.and_then(move |h| h.attr(name));
        let mut max = attr("maxstanzas")
            .and_then(|m| m.parse::<usize>().ok())
            .unwrap_or(self.svc.settings.default_history);
        if attr("maxchars") == Some("0") {
            max = 0;
        }
        if max == 0 || !self.config.logging || !self.svc.settings.archive {
            return;
        }
        let since = attr("since").and_then(mam::parse_datetime).or_else(|| {
            attr("seconds")
                .and_then(|s| s.parse::<i64>().ok())
                .map(|s| now_ms() - s * 1000)
        });
        let page = self
            .router
            .store()
            .archive_query(ArchiveQuery {
                archive_jid: self.jid_str.clone(),
                max: max.min(self.svc.settings.mam_max_page),
                rsm_before: Some(String::new()),
                start_ms: since,
                ..Default::default()
            })
            .await;
        for row in page.map(|p| p.rows).unwrap_or_default() {
            let Ok(mut m) = row.stanza.parse::<Element>() else {
                continue;
            };
            stanza::set_attr(&mut m, "to", to);
            m.append_child(delay(&self.jid_str, row.ts_ms));
            self.send(m);
        }
    }

    // --------------------------------------------------------- messages

    async fn message(&mut self, el: Element) {
        let Some(from) = el.attr("from").map(str::to_owned) else {
            return;
        };
        let to = el.attr("to").and_then(|t| Jid::new(t).ok());
        let ty = stanza::stanza_type(&el).to_owned();
        if !self.exists {
            self.error(&el, Condition::ItemNotFound);
            return;
        }
        if let Some(nick) = to.as_ref().and_then(|t| t.resource()).map(|r| r.as_str().to_owned()) {
            self.private_message(&from, &nick, el);
            return;
        }
        match ty.as_str() {
            "groupchat" => self.groupchat(&from, el).await,
            "error" => {}
            _ => {
                if let Some(x) = el.get_child("x", NS_MUC_USER).cloned() {
                    if x.has_child("invite", NS_MUC_USER) {
                        self.invite(&from, &el, &x).await;
                        return;
                    }
                    if let Some(d) = x.get_child("decline", NS_MUC_USER) {
                        self.decline(&from, d);
                        return;
                    }
                }
                self.error(&el, Condition::BadRequest);
            }
        }
    }

    async fn groupchat(&mut self, from: &str, el: Element) {
        let Some(nick) = self.nick_of(from) else {
            self.error(&el, Condition::NotAcceptable);
            return;
        };
        let role = self.occupants[&nick].role;
        if role == Role::Visitor {
            self.error(&el, Condition::Forbidden);
            return;
        }
        let sender_bare = self.occupants[&nick].bare.clone();
        let mut out = el.clone();
        stanza::remove_attr(&mut out, "to");
        stanza::set_attr(&mut out, "from", self.occupant_jid(&nick));
        let jid_str = self.jid_str.clone();
        remove_children(&mut out, |c| {
            c.is("stanza-id", ns::SID) && c.attr("by") == Some(jid_str.as_str())
        });

        if let Some(subject) = el.get_child("subject", NS_CLIENT)
            && !has_body(&el)
        {
            if !(self.config.change_subject || role == Role::Moderator) {
                self.error(&el, Condition::Forbidden);
                return;
            }
            let text = subject.text();
            self.subject = Some(text).filter(|t| !t.is_empty());
            self.subject_from = Some(nick.clone());
            self.save().await;
        } else if self.config.logging && self.svc.settings.archive && room_archivable(&el) {
            let id = mam::new_id();
            out.append_child(mam::stanza_id(&self.jid_str, &id));
            let mut archived = out.clone();
            if self.config.non_anonymous() {
                // Our client reads real JIDs from archived room messages.
                let mut x = Element::bare("x", NS_MUC_USER);
                x.append_child(stanza::element("item", NS_MUC_USER, &[("jid", &sender_bare)]));
                archived.append_child(x);
            }
            let row = ArchiveRow {
                id,
                archive_jid: self.jid_str.clone(),
                ts_ms: now_ms(),
                with_jid: sender_bare,
                direction: Direction::In,
                origin_id: mam::origin_id(&el),
                from_jid: self.occupant_jid(&nick),
                stanza: String::from(&archived),
            };
            if let Err(e) = self.router.store().archive_insert(row).await {
                tracing::error!(room = %self.jid, "archiving failed: {e:#}");
            }
        }
        for (s, _) in self.sessions() {
            let mut copy = out.clone();
            stanza::set_attr(&mut copy, "to", s.as_str());
            self.send(copy);
        }
    }

    fn private_message(&self, from: &str, nick: &str, el: Element) {
        let Some(sender) = self.nick_of(from) else {
            self.error(&el, Condition::NotAcceptable);
            return;
        };
        let sender_role = self.occupants[&sender].role;
        let allowed = match self.config.allow_pm.as_str() {
            "none" => false,
            "moderators" => sender_role == Role::Moderator,
            "participants" => sender_role >= Role::Participant,
            _ => true,
        };
        if !allowed {
            self.error(&el, Condition::Forbidden);
            return;
        }
        let Some(target) = self.occupants.get(nick) else {
            self.error(&el, Condition::ItemNotFound);
            return;
        };
        if Kind::of(&el) != Some(Kind::Message) {
            return;
        }
        for s in &target.sessions {
            let mut out = el.clone();
            stanza::set_attr(&mut out, "from", self.occupant_jid(&sender));
            stanza::set_attr(&mut out, "to", s.as_str());
            if !out.has_child("x", NS_MUC_USER) {
                out.append_child(Element::bare("x", NS_MUC_USER));
            }
            self.send(out);
        }
    }

    /// XEP-0045 §7.8.2 mediated invitations.
    async fn invite(&mut self, from: &str, el: &Element, x: &Element) {
        let Ok(from_jid) = Jid::new(from) else { return };
        let inviter = from_jid.to_bare().to_string();
        if self.nick_of(from).is_none() && self.affiliation(&inviter) < Affiliation::Member {
            self.error(el, Condition::NotAcceptable);
            return;
        }
        for inv in x.children().filter(|c| c.is("invite", NS_MUC_USER)) {
            let Some(to) = inv.attr("to").and_then(|t| BareJid::new(t).ok()) else {
                self.error(el, Condition::JidMalformed);
                return;
            };
            let invitee = to.to_string();
            if self.config.members_only && self.affiliation(&invitee) < Affiliation::Member {
                if self.affiliation(&inviter) >= Affiliation::Admin || self.config.allow_invites {
                    self.affiliations.insert(invitee.clone(), Affiliation::Member);
                    self.save_affiliation(&invitee, Affiliation::Member).await;
                } else {
                    self.error(el, Condition::Forbidden);
                    return;
                }
            }
            let mut m = stanza::element(
                "message",
                NS_CLIENT,
                &[
                    ("from", &self.jid_str),
                    ("to", &invitee),
                    ("id", &crate::user::server_id("invite")),
                ],
            );
            let mut ux = Element::bare("x", NS_MUC_USER);
            let mut i = stanza::element("invite", NS_MUC_USER, &[("from", &inviter)]);
            if let Some(r) = inv.get_child("reason", NS_MUC_USER) {
                i.append_child(r.clone());
            }
            ux.append_child(i);
            if let Some(pw) = &self.config.password {
                let mut p = Element::bare("password", NS_MUC_USER);
                p.append_text(pw.as_str());
                ux.append_child(p);
            }
            m.append_child(ux);
            let mut body = Element::bare("body", NS_CLIENT);
            body.append_text(format!("{inviter} invites you to the room {}", self.jid_str));
            m.append_child(body);
            self.router.route(m).await;
        }
    }

    fn decline(&self, from: &str, decline: &Element) {
        let Some(to) = decline.attr("to").and_then(|t| Jid::new(t).ok()) else {
            return;
        };
        let decliner = Jid::new(from).map(|j| j.to_bare().to_string()).unwrap_or_default();
        let mut m = stanza::element(
            "message",
            NS_CLIENT,
            &[("from", &self.jid_str), ("to", &to.to_string())],
        );
        let mut x = Element::bare("x", NS_MUC_USER);
        let mut d = stanza::element("decline", NS_MUC_USER, &[("from", &decliner)]);
        if let Some(r) = decline.get_child("reason", NS_MUC_USER) {
            d.append_child(r.clone());
        }
        x.append_child(d);
        m.append_child(x);
        self.send(m);
    }

    // -------------------------------------------------------------- IQs

    async fn iq(&mut self, el: Element) {
        let ty = stanza::stanza_type(&el).to_owned();
        if !matches!(ty.as_str(), "get" | "set") {
            return;
        }
        let Some(from) = el.attr("from").map(str::to_owned) else {
            return;
        };
        let from_bare = Jid::new(&from).map(|j| j.to_bare().to_string()).unwrap_or_default();
        let to_nick = el
            .attr("to")
            .and_then(|t| Jid::new(t).ok())
            .and_then(|j| j.resource().map(|r| r.as_str().to_owned()));
        let Some(payload) = el.children().next().cloned() else {
            self.error(&el, Condition::BadRequest);
            return;
        };
        if let Some(nick) = to_nick {
            // XEP-0410 self-ping; other IQs to occupants aren't relayed.
            if payload.is("ping", ns::PING) && self.nick_of(&from).as_deref() == Some(nick.as_str()) {
                self.reply(&el, None);
            } else if self.nick_of(&from).is_none() {
                self.error(&el, Condition::NotAcceptable);
            } else {
                self.error(&el, Condition::ServiceUnavailable);
            }
            return;
        }
        let visible = self.exists && (!self.locked || self.affiliation(&from_bare) == Affiliation::Owner);
        if !visible {
            self.error(&el, Condition::ItemNotFound);
            return;
        }
        let pns = payload.ns();
        match (ty.as_str(), payload.name(), pns.as_str()) {
            ("get", "ping", ns::PING) => self.reply(&el, None),
            ("get", "query", ns::DISCO_INFO) => {
                if payload.attr("node").is_some() {
                    self.error(&el, Condition::ItemNotFound);
                } else {
                    let info = self.disco_info();
                    self.reply(&el, Some(info));
                }
            }
            ("get", "query", ns::DISCO_ITEMS) => self.reply(&el, Some(Element::bare("query", ns::DISCO_ITEMS))),
            (_, "query", NS_MUC_OWNER) => self.owner_iq(&el, &payload, &from_bare).await,
            (_, "query", NS_MUC_ADMIN) => self.admin_iq(&el, &payload, &from, &from_bare).await,
            (_, "query" | "prefs" | "metadata", mam::NS_MAM) => self.mam_iq(&el, &payload, &from, &from_bare).await,
            _ => self.error(&el, Condition::ServiceUnavailable),
        }
    }

    fn disco_info(&self) -> Element {
        let mut q = Element::bare("query", ns::DISCO_INFO);
        let name = if self.config.name.is_empty() {
            self.jid.node().map(|n| n.to_string()).unwrap_or_default()
        } else {
            self.config.name.clone()
        };
        q.append_child(stanza::element(
            "identity",
            ns::DISCO_INFO,
            &[("category", "conference"), ("type", "text"), ("name", &name)],
        ));
        for f in self.config.features(self.svc.settings.archive) {
            q.append_child(stanza::element("feature", ns::DISCO_INFO, &[("var", f)]));
        }
        let mut b = FormBuilder::new("result", NS_ROOMINFO);
        b.field(
            "muc#roominfo_description",
            "text-single",
            Some("Description"),
            &[&self.config.description],
        );
        b.field(
            "muc#roominfo_occupants",
            "text-single",
            Some("Occupants"),
            &[&self.occupants.len().to_string()],
        );
        b.field(
            "muc#roominfo_subject",
            "text-single",
            Some("Subject"),
            &[self.subject.as_deref().unwrap_or("")],
        );
        q.append_child(b.build());
        q
    }

    async fn owner_iq(&mut self, el: &Element, query: &Element, from_bare: &str) {
        if self.affiliation(from_bare) != Affiliation::Owner {
            self.error(el, Condition::Forbidden);
            return;
        }
        let max = self.svc.settings.max_occupants;
        if stanza::stanza_type(el) == "get" {
            let mut q = Element::bare("query", NS_MUC_OWNER);
            q.append_child(self.config.form(max));
            self.reply(el, Some(q));
            return;
        }
        if let Some(d) = query.get_child("destroy", NS_MUC_OWNER) {
            self.destroy(d).await;
            self.reply(el, None);
            return;
        }
        let Some(x) = query.get_child("x", forms::NS_DATA) else {
            self.error(el, Condition::BadRequest);
            return;
        };
        if x.attr("type") == Some("cancel") {
            self.reply(el, None);
            if self.locked {
                // Cancelling the initial configuration destroys the room.
                let d = Element::bare("destroy", NS_MUC_OWNER);
                self.destroy(&d).await;
            }
            return;
        }
        let before = self.config.clone();
        let mut cfg = self.config.clone();
        if cfg.apply(&forms::values(x), max).is_err() {
            self.error(el, Condition::NotAcceptable);
            return;
        }
        self.config = cfg;
        let was_locked = self.locked;
        self.locked = false;
        if was_locked {
            tracing::info!(room = %self.jid, members_only = self.config.members_only, "room configured");
            self.save_all().await;
        } else if !self.config.persistent && before.persistent {
            if let Err(e) = self.router.store().muc_room_delete(&self.jid_str).await {
                tracing::warn!(room = %self.jid, "unpersisting failed: {e:#}");
            }
        } else {
            self.save_all().await;
        }
        self.reply(el, None);
        if was_locked {
            return;
        }
        // XEP-0045 §10.2.1 status codes for configuration changes.
        let mut codes = vec![104];
        if self.config.non_anonymous() && !before.non_anonymous() {
            codes.push(172);
        }
        if !self.config.non_anonymous() && before.non_anonymous() {
            codes.push(173);
        }
        for (s, _) in self.sessions() {
            let mut m = stanza::element(
                "message",
                NS_CLIENT,
                &[("from", &self.jid_str), ("to", &s), ("type", "groupchat")],
            );
            let mut x = Element::bare("x", NS_MUC_USER);
            for c in &codes {
                x.append_child(status(*c));
            }
            m.append_child(x);
            self.send(m);
        }
        if self.config.members_only && !before.members_only {
            let outsiders: Vec<String> = self
                .occupants
                .iter()
                .filter(|(_, o)| self.affiliation(&o.bare) < Affiliation::Member)
                .map(|(n, _)| n.clone())
                .collect();
            for n in outsiders {
                self.remove_occupant(&n, 322).await;
            }
        }
    }

    async fn destroy(&mut self, d: &Element) {
        for (nick, o) in &self.occupants {
            for s in &o.sessions {
                let mut p = stanza::element(
                    "presence",
                    NS_CLIENT,
                    &[("from", &self.occupant_jid(nick)), ("to", s), ("type", "unavailable")],
                );
                let mut x = Element::bare("x", NS_MUC_USER);
                x.append_child(stanza::element(
                    "item",
                    NS_MUC_USER,
                    &[("affiliation", "none"), ("role", "none")],
                ));
                let mut dd = Element::bare("destroy", NS_MUC_USER);
                if let Some(j) = d.attr("jid") {
                    stanza::set_attr(&mut dd, "jid", j);
                }
                if let Some(r) = d.get_child("reason", NS_MUC_OWNER) {
                    let mut reason = Element::bare("reason", NS_MUC_USER);
                    reason.append_text(r.text());
                    dd.append_child(reason);
                }
                x.append_child(dd);
                p.append_child(x);
                self.send(p);
            }
        }
        self.occupants.clear();
        self.destroy_storage().await;
    }

    /// Remove an occupant with a status code (301 banned, 307 kicked,
    /// 321 affiliation change, 322 members-only).
    async fn remove_occupant(&mut self, nick: &str, code: u16) {
        if !self.occupants.contains_key(nick) {
            return;
        }
        for (other, o) in &self.occupants {
            for s in &o.sessions {
                let mut codes = vec![code];
                if other == nick {
                    codes.push(110);
                }
                let mut p = self.occupant_presence(
                    nick,
                    s,
                    self.shows_jids_to(o.role) || other == nick,
                    &codes,
                    Some("unavailable"),
                );
                if let Some(x) = p.get_child_mut("x", NS_MUC_USER)
                    && let Some(item) = x.get_child_mut("item", NS_MUC_USER)
                {
                    stanza::set_attr(item, "role", "none");
                }
                self.send(p);
            }
        }
        self.occupants.remove(nick);
    }

    async fn admin_iq(&mut self, el: &Element, query: &Element, from: &str, from_bare: &str) {
        let my_aff = self.affiliation(from_bare);
        let my_role = self
            .nick_of(from)
            .map(|n| self.occupants[&n].role)
            .unwrap_or(Role::None);
        let items: Vec<Element> = query
            .children()
            .filter(|c| c.is("item", NS_MUC_ADMIN))
            .cloned()
            .collect();
        if items.is_empty() {
            self.error(el, Condition::BadRequest);
            return;
        }
        if stanza::stanza_type(el) == "get" {
            let item = &items[0];
            let mut q = Element::bare("query", NS_MUC_ADMIN);
            if let Some(aff) = item.attr("affiliation").and_then(Affiliation::parse) {
                let allowed =
                    my_aff >= Affiliation::Admin || (my_aff >= Affiliation::Member && aff >= Affiliation::Member);
                if !allowed || aff == Affiliation::None {
                    self.error(el, Condition::Forbidden);
                    return;
                }
                for (jid, a) in &self.affiliations {
                    if *a == aff {
                        q.append_child(stanza::element(
                            "item",
                            NS_MUC_ADMIN,
                            &[("affiliation", aff.as_str()), ("jid", jid)],
                        ));
                    }
                }
            } else if let Some(role) = item.attr("role").and_then(Role::parse) {
                if my_role != Role::Moderator {
                    self.error(el, Condition::Forbidden);
                    return;
                }
                for (nick, o) in &self.occupants {
                    if o.role == role {
                        let mut i = stanza::element(
                            "item",
                            NS_MUC_ADMIN,
                            &[
                                ("role", role.as_str()),
                                ("nick", nick),
                                ("affiliation", self.affiliation(&o.bare).as_str()),
                            ],
                        );
                        if let Some(s) = o.sessions.first() {
                            stanza::set_attr(&mut i, "jid", s.as_str());
                        }
                        q.append_child(i);
                    }
                }
            } else {
                self.error(el, Condition::BadRequest);
                return;
            }
            self.reply(el, Some(q));
            return;
        }
        // Set: check every change first, then apply them all.
        let mut aff_changes = Vec::new();
        let mut role_changes = Vec::new();
        for item in &items {
            if let Some(aff) = item.attr("affiliation").and_then(Affiliation::parse) {
                let Some(target) = item
                    .attr("jid")
                    .and_then(|j| Jid::new(j).ok())
                    .map(|j| j.to_bare().to_string())
                else {
                    self.error(el, Condition::BadRequest);
                    return;
                };
                let current = self.affiliation(&target);
                let ok = match my_aff {
                    Affiliation::Owner => true,
                    Affiliation::Admin => current < Affiliation::Admin && aff < Affiliation::Admin,
                    _ => false,
                };
                if !ok {
                    self.error(el, Condition::Forbidden);
                    return;
                }
                aff_changes.push((target, aff));
            } else if let Some(role) = item.attr("role").and_then(Role::parse) {
                let Some(nick) = item.attr("nick").map(str::to_owned) else {
                    self.error(el, Condition::BadRequest);
                    return;
                };
                let Some(o) = self.occupants.get(&nick) else {
                    self.error(el, Condition::ItemNotFound);
                    return;
                };
                let target_aff = self.affiliation(&o.bare);
                let ok = match role {
                    Role::Moderator => my_aff >= Affiliation::Admin,
                    _ => my_role == Role::Moderator && target_aff < my_aff.max(Affiliation::Member),
                };
                if !ok || (target_aff >= Affiliation::Admin && my_aff < Affiliation::Owner) {
                    self.error(el, Condition::NotAllowed);
                    return;
                }
                role_changes.push((nick, role));
            } else {
                self.error(el, Condition::BadRequest);
                return;
            }
        }
        // The last owner stays (XEP-0045 §10.6).
        let owners_left = self
            .affiliations
            .iter()
            .filter(|(j, a)| {
                **a == Affiliation::Owner && !aff_changes.iter().any(|(t, n)| t == *j && *n != Affiliation::Owner)
            })
            .count()
            + aff_changes.iter().filter(|(_, a)| *a == Affiliation::Owner).count();
        if owners_left == 0 {
            self.error(el, Condition::Conflict);
            return;
        }
        for (target, aff) in aff_changes {
            if aff == Affiliation::None {
                self.affiliations.remove(&target);
            } else {
                self.affiliations.insert(target.clone(), aff);
            }
            self.save_affiliation(&target, aff).await;
            let nicks: Vec<String> = self
                .occupants
                .iter()
                .filter(|(_, o)| o.bare == target)
                .map(|(n, _)| n.clone())
                .collect();
            for n in nicks {
                if aff == Affiliation::Outcast {
                    self.remove_occupant(&n, 301).await;
                } else if self.config.members_only && aff < Affiliation::Member {
                    self.remove_occupant(&n, 321).await;
                } else {
                    let role = self.default_role(aff);
                    if let Some(o) = self.occupants.get_mut(&n) {
                        o.role = role;
                    }
                    self.broadcast_presence(&n, &[], &[110], None);
                }
            }
        }
        for (nick, role) in role_changes {
            if role == Role::None {
                self.remove_occupant(&nick, 307).await;
            } else if let Some(o) = self.occupants.get_mut(&nick) {
                o.role = role;
                self.broadcast_presence(&nick, &[], &[110], None);
            }
        }
        self.reply(el, None);
    }

    async fn mam_iq(&mut self, el: &Element, payload: &Element, from: &str, from_bare: &str) {
        let allowed = if self.config.members_only {
            self.affiliation(from_bare) >= Affiliation::Member || self.nick_of(from).is_some()
        } else {
            self.affiliation(from_bare) != Affiliation::Outcast
        };
        if !allowed {
            self.error(el, Condition::Forbidden);
            return;
        }
        if !(self.config.logging && self.svc.settings.archive) {
            self.error(el, Condition::ServiceUnavailable);
            return;
        }
        match (stanza::stanza_type(el), payload.name()) {
            ("get", "query") => {
                self.reply(el, Some(mam::form()));
                return;
            }
            (_, "prefs") | ("get", "metadata") => {
                self.error(el, Condition::FeatureNotImplemented);
                return;
            }
            ("set", "query") => {}
            _ => {
                self.error(el, Condition::BadRequest);
                return;
            }
        }
        let req = match mam::parse(payload, &self.jid_str, self.svc.settings.mam_max_page) {
            Ok(r) => r,
            Err(c) => {
                self.error(el, c);
                return;
            }
        };
        let page = match self.router.store().archive_query(req.query).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(room = %self.jid, "room MAM query failed: {e:#}");
                self.error(el, Condition::InternalServerError);
                return;
            }
        };
        if page.id_not_found {
            self.error(el, Condition::ItemNotFound);
            return;
        }
        let mut rows: Vec<&ArchiveRow> = page.rows.iter().collect();
        if req.flip {
            rows.reverse();
        }
        for row in &rows {
            if let Some(m) = mam::result_message(from, Some(&self.jid_str), req.queryid.as_deref(), row) {
                self.send(m);
            }
        }
        self.reply(el, Some(mam::fin(&page, &rows)));
    }
}

/// Room archiving policy: real messages, not chat states or markers.
fn room_archivable(el: &Element) -> bool {
    let hint = |n: &str| el.has_child(n, crate::user::message::NS_HINTS);
    (has_body(el) || is_encrypted(el) || hint("store")) && !hint("no-store") && !hint("no-permanent-store")
}
