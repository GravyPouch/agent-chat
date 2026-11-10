//! Inbound stanza handling.
//!
//! [`classify`] is a pure function that unwraps carbons and MAM results,
//! applies the anti-spoofing checks and works out which conversation a
//! message belongs to. [`on_message`] then deduplicates (before any
//! decryption), decrypts if needed and stores.

use std::sync::Arc;

use agent_chat_proto::ipc::Event;
use agent_chat_proto::model::{ChatState, MessageKind, MessageSource, PresenceInfo, Show, SubscriptionRequest};
use chrono::{DateTime, Utc};
use tokio_xmpp::FromElementError;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::carbons;
use xmpp_parsers::chatstates::ChatState as XChatState;
use xmpp_parsers::delay::Delay;
use xmpp_parsers::disco::DiscoInfoQuery;
use xmpp_parsers::iq::Iq;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::mam;
use xmpp_parsers::message::{Lang, Message, MessageType};
use xmpp_parsers::muc::user::{MucUser, Status as MucStatus};
use xmpp_parsers::ping::Ping;
use xmpp_parsers::presence::{Presence, Show as XShow, Type as PresenceType};
use xmpp_parsers::roster::Roster;
use xmpp_parsers::stanza_error::{DefinedCondition, ErrorType, StanzaError};
use xmpp_parsers::stanza_id::{OriginId, StanzaId};

use crate::core::Core;
use crate::store::{DedupeKeys, NewMessage};
use crate::xmpp::session::SessionHandle;

/// Lookups `classify` needs from daemon state.
pub struct Ctx<'a> {
    pub own: &'a BareJid,
    /// MAM query id → archive JID expected to answer.
    pub mam_archive: &'a dyn Fn(&str) -> Option<String>,
    /// Our nick in a room (by bare room JID), if we're in it.
    pub room_nick: &'a dyn Fn(&str) -> Option<String>,
}

#[derive(Debug)]
pub struct Parsed {
    pub conversation: String,
    pub kind: MessageKind,
    pub sender: String,
    pub nick: Option<String>,
    pub own: bool,
    pub source: MessageSource,
    pub archive: Option<String>,
    pub stanza_id: Option<String>,
    pub origin_id: Option<String>,
    pub xmpp_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub delayed: bool,
    /// The (unwrapped) message, for body or encrypted-payload extraction.
    pub message: Message,
}

#[derive(Debug)]
pub enum Classified {
    Store(Box<Parsed>),
    RoomSubject {
        room: String,
        subject: String,
    },
    /// XEP-0085 chat state from a contact (bare JID), or from a room
    /// occupant (room JID and nick).
    ChatState {
        jid: String,
        state: ChatState,
        nick: Option<String>,
    },
    Ignore(&'static str),
}

/// First payload of type `T`, without removing it.
pub fn find_payload<T>(m: &Message) -> Option<T>
where
    T: TryFrom<Element, Error = FromElementError>,
{
    m.payloads.iter().find_map(|el| T::try_from(el.clone()).ok())
}

fn all_payloads<T>(m: &Message) -> Vec<T>
where
    T: TryFrom<Element, Error = FromElementError>,
{
    m.payloads
        .iter()
        .filter_map(|el| T::try_from(el.clone()).ok())
        .collect()
}

pub fn body_of(m: &Message) -> Option<&String> {
    m.bodies.get(&Lang(String::new())).or_else(|| m.bodies.values().next())
}

/// Does the message carry an OMEMO `<encrypted/>` element (either version)?
pub fn has_omemo(m: &Message) -> bool {
    m.payloads.iter().any(|el| {
        el.name() == "encrypted" && (el.ns() == crate::omemo::NS_OMEMO2 || el.ns() == crate::omemo::NS_LEGACY)
    })
}

fn same_bare(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

pub fn classify(ctx: &Ctx<'_>, outer: Message) -> Classified {
    let own = ctx.own.to_string();

    // XEP-0313: archived message. Only accept results for queries we sent,
    // from the archive we sent them to.
    if let Some(result) = find_payload::<mam::Result_>(&outer) {
        let Some(qid) = result.queryid.as_ref().map(|q| q.0.clone()) else {
            return Classified::Ignore("MAM result without queryid");
        };
        let Some(archive) = (ctx.mam_archive)(&qid) else {
            return Classified::Ignore("MAM result for unknown query");
        };
        let from_ok = match &outer.from {
            None => same_bare(&archive, &own),
            Some(f) => f.is_bare() && same_bare(f.as_str(), &archive),
        };
        if !from_ok {
            return Classified::Ignore("MAM result from unexpected sender");
        }
        let delay = result.forwarded.delay.as_ref().map(|d| d.stamp.0.to_utc());
        return classify_inner(
            ctx,
            result.forwarded.message,
            MessageSource::Mam,
            Some((archive, result.id)),
            delay,
            false,
        );
    }

    // XEP-0280: carbons. Must come from our own bare JID, or anyone could
    // inject messages "from" arbitrary contacts.
    let received = find_payload::<carbons::Received>(&outer).map(|c| (c.forwarded, false));
    let sent = find_payload::<carbons::Sent>(&outer).map(|c| (c.forwarded, true));
    if let Some((fwd, is_sent)) = received.or(sent) {
        let from_ok = matches!(&outer.from, Some(f) if f.is_bare() && same_bare(f.as_str(), &own));
        if !from_ok {
            return Classified::Ignore("carbon from foreign JID");
        }
        let delay = fwd.delay.as_ref().map(|d| d.stamp.0.to_utc());
        return classify_inner(ctx, fwd.message, MessageSource::Carbon, None, delay, is_sent);
    }

    classify_inner(ctx, outer, MessageSource::Live, None, None, false)
}

fn classify_inner(
    ctx: &Ctx<'_>,
    m: Message,
    source: MessageSource,
    mam: Option<(String, String)>,
    forwarded_delay: Option<DateTime<Utc>>,
    sent_carbon: bool,
) -> Classified {
    let own = ctx.own.to_string();
    if m.type_ == MessageType::Error {
        return Classified::Ignore("error message");
    }
    if m.type_ == MessageType::Headline {
        return Classified::Ignore("headline");
    }
    let has_body = body_of(&m).is_some_and(|b| !b.is_empty());
    let encrypted = has_omemo(&m);

    let stamp = forwarded_delay.or_else(|| find_payload::<Delay>(&m).map(|d| d.stamp.0.to_utc()));
    let delayed = stamp.is_some() || source == MessageSource::Mam;
    let timestamp = stamp.unwrap_or_else(Utc::now);
    let origin_id = find_payload::<OriginId>(&m).map(|o| o.id);
    let xmpp_id = m.id.as_ref().map(|i| i.0.clone());

    // Pick the server-assigned id from the right archive (XEP-0359: only trust
    // stanza-ids whose `by` is the archive we expect).
    let stanza_id_by = |archive: &str| {
        all_payloads::<StanzaId>(&m)
            .into_iter()
            .find(|s| same_bare(s.by.as_str(), archive))
            .map(|s| s.id)
    };

    if m.type_ == MessageType::Groupchat {
        let Some(from) = m.from.clone() else {
            return Classified::Ignore("groupchat without from");
        };
        let room = from.to_bare().to_string();
        let nick = from.resource().map(|r| r.to_string());
        if !has_body
            && !encrypted
            && let Some(subject) = m.subjects.values().next()
        {
            return Classified::RoomSubject {
                room,
                subject: subject.clone(),
            };
        }
        if !has_body && !encrypted {
            // XEP-0085 in a room: someone else typing. Our own states come
            // back reflected; archived ones are stale.
            if source == MessageSource::Live
                && let Some(state) = find_payload::<XChatState>(&m).map(chat_state_of)
                && let Some(nick) = &nick
                && (ctx.room_nick)(&room).as_ref() != Some(nick)
            {
                return Classified::ChatState {
                    jid: room,
                    state,
                    nick: Some(nick.clone()),
                };
            }
            return Classified::Ignore("groupchat without body");
        }
        let our_nick = (ctx.room_nick)(&room);
        let is_own = nick.is_some() && nick == our_nick;
        let (archive, stanza_id) = match mam {
            Some((a, id)) => (Some(a), Some(id)),
            None => (Some(room.clone()), stanza_id_by(&room)),
        };
        return Classified::Store(Box::new(Parsed {
            conversation: room,
            kind: MessageKind::Groupchat,
            sender: from.to_string(),
            nick,
            own: is_own,
            source,
            archive,
            stanza_id,
            origin_id,
            xmpp_id,
            timestamp,
            delayed,
            message: m,
        }));
    }

    if !has_body && !encrypted {
        if let Some(c) = chat_state(ctx, &m, source, sent_carbon) {
            return c;
        }
        return Classified::Ignore("no body");
    }

    let from: Jid = m.from.clone().unwrap_or_else(|| ctx.own.clone().into());
    let from_bare = from.to_bare().to_string();
    let to_bare =
        m.to.as_ref()
            .map(|t| t.to_bare().to_string())
            .unwrap_or_else(|| own.clone());
    let (archive, stanza_id) = match mam {
        Some((a, id)) => (Some(a), Some(id)),
        None => (Some(own.clone()), stanza_id_by(&own)),
    };

    // Private message through a room we're in.
    if (ctx.room_nick)(&from_bare).is_some() && from.resource().is_some() {
        return Classified::Store(Box::new(Parsed {
            conversation: from.to_string(),
            kind: MessageKind::MucPrivate,
            sender: from.to_string(),
            nick: from.resource().map(|r| r.to_string()),
            own: false,
            source,
            archive,
            stanza_id,
            origin_id,
            xmpp_id,
            timestamp,
            delayed,
            message: m,
        }));
    }
    if same_bare(&from_bare, &own)
        && let Some(to) = m.to.as_ref()
        && (ctx.room_nick)(&to_bare).is_some()
        && to.resource().is_some()
    {
        return Classified::Store(Box::new(Parsed {
            conversation: to.to_string(),
            kind: MessageKind::MucPrivate,
            sender: own.clone(),
            nick: None,
            own: true,
            source,
            archive,
            stanza_id,
            origin_id,
            xmpp_id,
            timestamp,
            delayed,
            message: m,
        }));
    }

    let is_own = sent_carbon || same_bare(&from_bare, &own);
    let conversation = if is_own { to_bare } else { from_bare.clone() };
    Classified::Store(Box::new(Parsed {
        conversation,
        kind: MessageKind::Chat,
        sender: if is_own { own } else { from_bare },
        nick: None,
        own: is_own,
        source,
        archive,
        stanza_id,
        origin_id,
        xmpp_id,
        timestamp,
        delayed,
        message: m,
    }))
}

/// XEP-0085: a body-less chat state from a contact, live or through a received
/// carbon. Archived states are stale, our own (sent carbons) aren't news, and
/// rooms and their private messages are left out.
fn chat_state(ctx: &Ctx<'_>, m: &Message, source: MessageSource, sent_carbon: bool) -> Option<Classified> {
    if source == MessageSource::Mam || sent_carbon {
        return None;
    }
    let state = chat_state_of(find_payload::<XChatState>(m)?);
    let from = m.from.as_ref()?.to_bare().to_string();
    if same_bare(&from, &ctx.own.to_string()) || (ctx.room_nick)(&from).is_some() {
        return None;
    }
    Some(Classified::ChatState {
        jid: from,
        state,
        nick: None,
    })
}

fn chat_state_of(state: XChatState) -> ChatState {
    match state {
        XChatState::Active => ChatState::Active,
        XChatState::Composing => ChatState::Composing,
        XChatState::Paused => ChatState::Paused,
        XChatState::Inactive => ChatState::Inactive,
        XChatState::Gone => ChatState::Gone,
    }
}

/// A group chat invitation: XEP-0045 mediated (`muc#user` from the room) or
/// XEP-0249 direct (`jabber:x:conference` from the inviter).
pub fn parse_invite(m: &Message) -> Option<agent_chat_proto::model::RoomInvite> {
    let from = m.from.as_ref().map(|f| f.to_bare().to_string());
    for el in &m.payloads {
        if el.is("x", "jabber:x:conference") {
            let room = BareJid::new(el.attr("jid")?).ok()?.to_string();
            return Some(agent_chat_proto::model::RoomInvite {
                room,
                from,
                reason: el.attr("reason").map(str::to_owned),
                password: el.attr("password").map(str::to_owned),
            });
        }
        if el.is("x", "http://jabber.org/protocol/muc#user")
            && let Some(invite) = el.get_child("invite", "http://jabber.org/protocol/muc#user")
        {
            return Some(agent_chat_proto::model::RoomInvite {
                room: from.clone()?,
                from: invite
                    .attr("from")
                    .and_then(|f| BareJid::new(f).ok().or_else(|| Jid::new(f).ok().map(|j| j.to_bare())))
                    .map(|j| j.to_string()),
                reason: invite
                    .get_child("reason", "http://jabber.org/protocol/muc#user")
                    .map(|r| r.text())
                    .filter(|r| !r.is_empty()),
                password: el
                    .get_child("password", "http://jabber.org/protocol/muc#user")
                    .map(|p| p.text()),
            });
        }
    }
    None
}

pub async fn on_message(core: &Arc<Core>, handle: &SessionHandle, m: Message) {
    if m.type_ != MessageType::Groupchat
        && m.type_ != MessageType::Error
        && let Some(invite) = parse_invite(&m)
    {
        if core.is_room(&invite.room) {
            return; // already there
        }
        {
            let mut st = core.state();
            st.pending_invites
                .retain(|i| !i.room.eq_ignore_ascii_case(&invite.room));
            st.pending_invites.push(invite.clone());
        }
        tracing::info!("invited to {} by {:?}", invite.room, invite.from);
        core.emit(Event::RoomInvite(invite));
        return;
    }
    // PEP notifications (OMEMO device lists) are handled separately.
    if crate::omemo::xmpp::is_pep_event(&m) {
        crate::omemo::xmpp::on_pep_event(core, handle, &m);
        return;
    }
    let classified = {
        let state = core.state();
        let mam_archive = |q: &str| state.mam_queries.get(q).cloned();
        let room_nick = |r: &str| state.rooms.get(&r.to_lowercase()).map(|rt| rt.nick.clone());
        let ctx = Ctx {
            own: &handle.own,
            mam_archive: &mam_archive,
            room_nick: &room_nick,
        };
        classify(&ctx, m)
    };
    match classified {
        Classified::Ignore(why) => tracing::trace!("ignoring message: {why}"),
        Classified::RoomSubject { room, subject } => {
            if let Some(rt) = core.state().rooms.get_mut(&room.to_lowercase()) {
                rt.subject = Some(subject.clone());
            }
            core.emit(Event::RoomSubject { room, subject });
        }
        Classified::ChatState { jid, state, nick } => core.emit(Event::ChatState { jid, state, nick }),
        Classified::Store(p) => {
            if let Err(e) = store_parsed(core, handle, *p).await {
                tracing::warn!("failed to store message: {e:#}");
            }
        }
    }
}

async fn store_parsed(core: &Arc<Core>, handle: &SessionHandle, p: Parsed) -> anyhow::Result<()> {
    // Deduplicate BEFORE decrypting: carbons and MAM replay the same OMEMO
    // ciphertext, and decrypting it twice would fail or advance the ratchet.
    let duplicate = core.store().find_duplicate(&DedupeKeys {
        conversation: &p.conversation,
        sender: &p.sender,
        archive: p.archive.as_deref(),
        stanza_id: p.stanza_id.as_deref(),
        origin_id: p.origin_id.as_deref(),
        xmpp_id: p.xmpp_id.as_deref(),
    })?;
    if duplicate.is_some() {
        // Still record the archive id on the existing row.
        if p.stanza_id.is_some() {
            core.store().insert_message(&new_message(&p, String::new(), None))?;
        }
        return Ok(());
    }

    let (body, encryption) = if has_omemo(&p.message) {
        match crate::omemo::xmpp::decrypt_message(core, handle, &p).await {
            Some(result) => result,
            // Key-transport / heartbeat message: nothing to show.
            None => return Ok(()),
        }
    } else {
        (body_of(&p.message).cloned().unwrap_or_default(), None)
    };
    core.store_message(&new_message(&p, body, encryption))?;
    Ok(())
}

fn new_message(p: &Parsed, body: String, encryption: Option<agent_chat_proto::model::EncryptionInfo>) -> NewMessage {
    NewMessage {
        conversation: p.conversation.clone(),
        kind: p.kind,
        sender: p.sender.clone(),
        nick: p.nick.clone(),
        body,
        timestamp: p.timestamp,
        delayed: p.delayed,
        source: p.source,
        own: p.own,
        archive: p.archive.clone(),
        stanza_id: p.stanza_id.clone(),
        origin_id: p.origin_id.clone(),
        xmpp_id: p.xmpp_id.clone(),
        encryption,
    }
}

pub fn presence_info(p: &Presence) -> PresenceInfo {
    let show = match p.type_ {
        PresenceType::Unavailable => Show::Unavailable,
        _ => match p.show {
            None => Show::Available,
            Some(XShow::Chat) => Show::Chat,
            Some(XShow::Away) => Show::Away,
            Some(XShow::Xa) => Show::Xa,
            Some(XShow::Dnd) => Show::Dnd,
        },
    };
    PresenceInfo {
        show,
        status: p.statuses.values().next().cloned(),
    }
}

pub fn on_presence(core: &Arc<Core>, handle: &SessionHandle, p: Presence) {
    let Some(from) = p.from.clone() else { return };
    let bare = from.to_bare().to_string();

    if core.is_room(&bare) {
        on_room_presence(core, handle, &bare, &from, p);
        return;
    }
    if same_bare(&bare, &handle.own.to_string()) {
        return; // our own other resources
    }

    match p.type_ {
        PresenceType::Subscribe => {
            let auto = core.config.lock().unwrap().settings.auto_accept_subscriptions;
            if auto {
                if let Err(e) = crate::xmpp::actions::approve_subscription(core, handle, &bare) {
                    tracing::warn!("auto-accept failed: {e:#}");
                }
            } else {
                core.state().pending_requests.insert(bare.clone());
                core.emit(Event::SubscriptionRequest(SubscriptionRequest { jid: bare }));
            }
        }
        PresenceType::None | PresenceType::Unavailable => {
            let resource = from.resource().map(|r| r.to_string()).unwrap_or_default();
            let info = presence_info(&p);
            let best = {
                let mut st = core.state();
                let entry = st.presence.entry(bare.clone()).or_default();
                if info.show == Show::Unavailable {
                    entry.remove(&resource);
                } else {
                    entry.insert(resource, info);
                }
                crate::core::best_presence(st.presence.get(&bare))
            };
            core.emit(Event::Presence {
                jid: bare,
                presence: best,
            });
        }
        _ => {}
    }
}

fn on_room_presence(core: &Arc<Core>, handle: &SessionHandle, room: &str, from: &Jid, p: Presence) {
    let Some(nick) = from.resource().map(|r| r.to_string()) else {
        return;
    };
    let muc_user = p.payloads.iter().find_map(|el| MucUser::try_from(el.clone()).ok());
    let is_self = muc_user
        .as_ref()
        .is_some_and(|u| u.status.contains(&MucStatus::SelfPresence));
    // XEP-0045 §10.1: a room we just created stays locked (others get
    // item-not-found) until we accept the default configuration.
    let created = is_self
        && muc_user
            .as_ref()
            .is_some_and(|u| u.status.contains(&MucStatus::RoomHasBeenCreated));
    let real_jid = muc_user
        .as_ref()
        .and_then(|u| u.items.first())
        .and_then(|i| i.jid.as_ref())
        .map(|j| j.to_bare().to_string());

    let mut start_sync = false;
    let mut just_joined = false;
    let mut failed = false;
    {
        let mut st = core.state();
        let Some(rt) = st.rooms.get_mut(&room.to_lowercase()) else {
            return;
        };
        match p.type_ {
            PresenceType::Error => {
                if rt.nick == nick || !rt.joined {
                    failed = true;
                    let condition = p
                        .payloads
                        .iter()
                        .find_map(|el| xmpp_parsers::stanza_error::StanzaError::try_from(el.clone()).ok())
                        .map(|e| format!("{:?}", e.defined_condition))
                        .unwrap_or_else(|| "join refused".into());
                    rt.error = Some(condition);
                }
            }
            PresenceType::Unavailable => {
                rt.occupants.remove(&nick);
                rt.occupant_jids.remove(&nick);
                if is_self {
                    rt.joined = false;
                }
            }
            _ => {
                rt.occupants.insert(nick.clone());
                if let Some(j) = real_jid {
                    rt.occupant_jids.insert(nick.clone(), j);
                }
                if is_self {
                    if !rt.joined && rt.mam {
                        start_sync = true;
                    }
                    just_joined = !rt.joined;
                    rt.joined = true;
                    // The service may have changed our nick.
                    rt.nick = nick.clone();
                }
            }
        }
    }
    if failed {
        let why = core
            .state()
            .rooms
            .get(&room.to_lowercase())
            .and_then(|r| r.error.clone())
            .unwrap_or_default();
        tracing::warn!("could not join {room}: {why}");
    }
    core.emit(Event::RoomsChanged);
    if created || just_joined {
        let core = core.clone();
        let handle = handle.clone();
        let room = room.to_owned();
        let private = core
            .state()
            .rooms
            .get(&room.to_lowercase())
            .is_some_and(|r| r.create_private);
        tokio::spawn(async move {
            let result = if created {
                crate::xmpp::actions::configure_new_room(&core, &handle, &room, private).await
            } else {
                crate::xmpp::actions::refresh_room_info(&core, &handle, &room).await
            };
            if let Err(e) = result {
                tracing::warn!("setting up room {room} failed: {e:#}");
            }
        });
    }
    if start_sync {
        let core = core.clone();
        let handle = handle.clone();
        let room = room.to_owned();
        tokio::spawn(async move {
            if let Err(e) = crate::xmpp::actions::sync_archive(&core, &handle, Some(&room)).await {
                tracing::warn!("room archive sync for {room} failed: {e:#}");
            }
        });
    }
}

fn service_unavailable(to: Option<Jid>, id: String) -> Iq {
    Iq::Error {
        from: None,
        to,
        id,
        error: StanzaError::new(ErrorType::Cancel, DefinedCondition::ServiceUnavailable, "en", ""),
        payload: None,
    }
}

/// Answer IQ requests addressed to us. Returns the reply to send, if any.
pub fn on_iq_request(core: &Arc<Core>, handle: &SessionHandle, iq: Iq) -> Option<Iq> {
    match iq {
        Iq::Get { from, id, payload, .. } => {
            if Ping::try_from(payload.clone()).is_ok() {
                return Some(Iq::Result {
                    from: None,
                    to: from,
                    id,
                    payload: None,
                });
            }
            if let Ok(q) = DiscoInfoQuery::try_from(payload.clone()) {
                let disco = crate::xmpp::disco::our_disco(q.node);
                return Some(Iq::Result {
                    from: None,
                    to: from,
                    id,
                    payload: Some(disco.into()),
                });
            }
            Some(service_unavailable(from, id))
        }
        Iq::Set { from, id, payload, .. } => {
            let from_self = match &from {
                None => true,
                Some(f) => f.is_bare() && same_bare(f.as_str(), &handle.own.to_string()),
            };
            if from_self && let Ok(roster) = Roster::try_from(payload.clone()) {
                crate::xmpp::actions::apply_roster(core, roster, true);
                return Some(Iq::Result {
                    from: None,
                    to: from,
                    id,
                    payload: None,
                });
            }
            Some(service_unavailable(from, id))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "alice@example.org";

    fn parse(xml: &str) -> Message {
        let el: Element = xml.parse().unwrap();
        Message::try_from(el).unwrap()
    }

    fn run(xml: &str, mam: Option<(&str, &str)>, room: Option<(&str, &str)>) -> Classified {
        let own = BareJid::new(OWN).unwrap();
        let mam_archive = move |q: &str| mam.filter(|(id, _)| *id == q).map(|(_, a)| a.to_owned());
        let room_nick = move |r: &str| room.filter(|(j, _)| *j == r).map(|(_, n)| n.to_owned());
        let ctx = Ctx {
            own: &own,
            mam_archive: &mam_archive,
            room_nick: &room_nick,
        };
        classify(&ctx, parse(xml))
    }

    fn stored(c: Classified) -> Parsed {
        match c {
            Classified::Store(p) => *p,
            other => panic!("expected Store, got {other:?}"),
        }
    }

    #[test]
    fn live_chat_message() {
        let p = stored(run(
            r#"<message xmlns='jabber:client' type='chat' from='bob@example.org/phone' to='alice@example.org/ac' id='m1'>
                 <body>hello</body>
                 <stanza-id xmlns='urn:xmpp:sid:0' id='S1' by='alice@example.org'/>
                 <stanza-id xmlns='urn:xmpp:sid:0' id='EVIL' by='bob@example.org'/>
                 <origin-id xmlns='urn:xmpp:sid:0' id='O1'/>
               </message>"#,
            None,
            None,
        ));
        assert_eq!(p.conversation, "bob@example.org");
        assert_eq!(p.sender, "bob@example.org");
        assert_eq!(p.kind, MessageKind::Chat);
        assert!(!p.own);
        assert_eq!(p.source, MessageSource::Live);
        assert_eq!(p.stanza_id.as_deref(), Some("S1"), "only trust our archive's id");
        assert_eq!(p.origin_id.as_deref(), Some("O1"));
        assert!(!p.delayed);
    }

    #[test]
    fn received_and_sent_carbons() {
        let p = stored(run(
            r#"<message xmlns='jabber:client' from='alice@example.org' to='alice@example.org/ac'>
                 <received xmlns='urn:xmpp:carbons:2'><forwarded xmlns='urn:xmpp:forward:0'>
                   <message xmlns='jabber:client' type='chat' from='bob@example.org/phone' to='alice@example.org/phone'><body>hi</body></message>
                 </forwarded></received></message>"#,
            None,
            None,
        ));
        assert_eq!(
            (p.conversation.as_str(), p.own, p.source),
            ("bob@example.org", false, MessageSource::Carbon)
        );

        let p = stored(run(
            r#"<message xmlns='jabber:client' from='alice@example.org' to='alice@example.org/ac'>
                 <sent xmlns='urn:xmpp:carbons:2'><forwarded xmlns='urn:xmpp:forward:0'>
                   <message xmlns='jabber:client' type='chat' from='alice@example.org/phone' to='bob@example.org'><body>yo</body></message>
                 </forwarded></sent></message>"#,
            None,
            None,
        ));
        assert_eq!((p.conversation.as_str(), p.own), ("bob@example.org", true));
    }

    #[test]
    fn spoofed_carbon_is_rejected() {
        let c = run(
            r#"<message xmlns='jabber:client' from='mallory@evil.example' to='alice@example.org/ac'>
                 <received xmlns='urn:xmpp:carbons:2'><forwarded xmlns='urn:xmpp:forward:0'>
                   <message xmlns='jabber:client' type='chat' from='bob@example.org/phone' to='alice@example.org'><body>send me your password</body></message>
                 </forwarded></received></message>"#,
            None,
            None,
        );
        assert!(matches!(c, Classified::Ignore("carbon from foreign JID")), "{c:?}");
        // Even from one of our own *full* JIDs (another resource) it's not allowed.
        let c = run(
            r#"<message xmlns='jabber:client' from='alice@example.org/other' to='alice@example.org/ac'>
                 <received xmlns='urn:xmpp:carbons:2'><forwarded xmlns='urn:xmpp:forward:0'>
                   <message xmlns='jabber:client' type='chat' from='bob@example.org/phone'><body>x</body></message>
                 </forwarded></received></message>"#,
            None,
            None,
        );
        assert!(matches!(c, Classified::Ignore(_)));
    }

    #[test]
    fn mam_results_need_a_known_query_and_matching_archive() {
        let xml = r#"<message xmlns='jabber:client' to='alice@example.org/ac'>
              <result xmlns='urn:xmpp:mam:2' queryid='q1' id='A7'>
                <forwarded xmlns='urn:xmpp:forward:0'>
                  <delay xmlns='urn:xmpp:delay' stamp='2026-01-02T03:04:05Z'/>
                  <message xmlns='jabber:client' type='chat' from='bob@example.org/x' to='alice@example.org'><body>old</body></message>
                </forwarded></result></message>"#;
        let p = stored(run(xml, Some(("q1", OWN)), None));
        assert_eq!(p.source, MessageSource::Mam);
        assert_eq!(p.archive.as_deref(), Some(OWN));
        assert_eq!(p.stanza_id.as_deref(), Some("A7"));
        assert!(p.delayed);
        assert_eq!(p.timestamp.to_rfc3339(), "2026-01-02T03:04:05+00:00");

        assert!(matches!(
            run(xml, None, None),
            Classified::Ignore("MAM result for unknown query")
        ));
        // Result for our account archive claiming to come from someone else.
        let spoofed = xml.replace(
            "<message xmlns='jabber:client' to=",
            "<message xmlns='jabber:client' from='mallory@evil.example' to=",
        );
        assert!(matches!(run(&spoofed, Some(("q1", OWN)), None), Classified::Ignore(_)));
    }

    #[test]
    fn groupchat_own_reflection_and_subject() {
        let p = stored(run(
            r#"<message xmlns='jabber:client' type='groupchat' from='room@conf.example.org/alice' to='alice@example.org/ac'>
                 <body>hello room</body>
                 <stanza-id xmlns='urn:xmpp:sid:0' id='R1' by='room@conf.example.org'/>
               </message>"#,
            None,
            Some(("room@conf.example.org", "alice")),
        ));
        assert_eq!(p.conversation, "room@conf.example.org");
        assert_eq!(p.kind, MessageKind::Groupchat);
        assert!(p.own);
        assert_eq!(p.sender, "room@conf.example.org/alice");
        assert_eq!(p.archive.as_deref(), Some("room@conf.example.org"));
        assert_eq!(p.stanza_id.as_deref(), Some("R1"));

        let c = run(
            r#"<message xmlns='jabber:client' type='groupchat' from='room@conf.example.org/bob'><subject>Topic</subject></message>"#,
            None,
            Some(("room@conf.example.org", "alice")),
        );
        assert!(matches!(c, Classified::RoomSubject { ref subject, .. } if subject == "Topic"));
    }

    #[test]
    fn muc_private_message() {
        let p = stored(run(
            r#"<message xmlns='jabber:client' type='chat' from='room@conf.example.org/bob' to='alice@example.org/ac'>
                 <body>psst</body><x xmlns='http://jabber.org/protocol/muc#user'/>
               </message>"#,
            None,
            Some(("room@conf.example.org", "alice")),
        ));
        assert_eq!(p.kind, MessageKind::MucPrivate);
        assert_eq!(p.conversation, "room@conf.example.org/bob");
    }

    #[test]
    fn parses_mediated_and_direct_invites() {
        let mediated = parse(
            r#"<message xmlns='jabber:client' from='team@conf.example.org' to='alice@example.org'>
                 <x xmlns='http://jabber.org/protocol/muc#user'>
                   <invite from='bob@example.org/laptop'><reason>join us</reason></invite>
                   <password>pw</password>
                 </x><body>bob invites you</body></message>"#,
        );
        let i = parse_invite(&mediated).unwrap();
        assert_eq!(i.room, "team@conf.example.org");
        assert_eq!(i.from.as_deref(), Some("bob@example.org"));
        assert_eq!(i.reason.as_deref(), Some("join us"));
        assert_eq!(i.password.as_deref(), Some("pw"));

        let direct = parse(
            r#"<message xmlns='jabber:client' from='bob@example.org/phone' to='alice@example.org'>
                 <x xmlns='jabber:x:conference' jid='team@conf.example.org' reason='hi'/></message>"#,
        );
        let i = parse_invite(&direct).unwrap();
        assert_eq!(
            (i.room.as_str(), i.from.as_deref()),
            ("team@conf.example.org", Some("bob@example.org"))
        );
        assert!(
            parse_invite(&parse(
                "<message xmlns='jabber:client' from='bob@x'><body>hi</body></message>"
            ))
            .is_none()
        );
    }

    #[test]
    fn chat_states_without_body() {
        let state = |xml: &str, room: Option<(&str, &str)>| match run(xml, None, room) {
            Classified::ChatState { jid, state, nick: None } => Some((jid, state)),
            Classified::Ignore("no body") => None,
            other => panic!("unexpected {other:?}"),
        };
        for (name, want) in [
            ("composing", ChatState::Composing),
            ("paused", ChatState::Paused),
            ("active", ChatState::Active),
            ("inactive", ChatState::Inactive),
            ("gone", ChatState::Gone),
        ] {
            let xml = format!(
                "<message xmlns='jabber:client' type='chat' from='Bob@example.org/phone'><{name} xmlns='http://jabber.org/protocol/chatstates'/></message>"
            );
            assert_eq!(state(&xml, None), Some(("bob@example.org".to_owned(), want)), "{name}");
        }
        // Through a received carbon: still the contact typing.
        let carbon = r#"<message xmlns='jabber:client' from='alice@example.org' to='alice@example.org/ac'>
                 <received xmlns='urn:xmpp:carbons:2'><forwarded xmlns='urn:xmpp:forward:0'>
                   <message xmlns='jabber:client' type='chat' from='bob@example.org/phone' to='alice@example.org/phone'><composing xmlns='http://jabber.org/protocol/chatstates'/></message>
                 </forwarded></received></message>"#;
        assert_eq!(
            state(carbon, None),
            Some(("bob@example.org".to_owned(), ChatState::Composing))
        );
        // Our own other device typing (sent carbon): not news.
        let sent = carbon.replace("received", "sent").replace(
            "from='bob@example.org/phone' to='alice@example.org/phone'",
            "from='alice@example.org/phone' to='bob@example.org'",
        );
        assert_eq!(state(&sent, None), None);
        // Room private messages and plain body-less messages carry no state.
        let pm = "<message xmlns='jabber:client' type='chat' from='room@conf.example.org/bob'><composing xmlns='http://jabber.org/protocol/chatstates'/></message>";
        assert_eq!(state(pm, Some(("room@conf.example.org", "alice"))), None);
        assert_eq!(
            state(
                "<message xmlns='jabber:client' type='chat' from='bob@example.org/phone'/>",
                None
            ),
            None
        );
        // In a room: the occupant typing, by nick; our own reflection isn't news.
        let c = run(
            "<message xmlns='jabber:client' type='groupchat' from='room@conf.example.org/bob'><composing xmlns='http://jabber.org/protocol/chatstates'/></message>",
            None,
            Some(("room@conf.example.org", "alice")),
        );
        assert!(
            matches!(&c, Classified::ChatState { jid, state: ChatState::Composing, nick: Some(n) }
                if jid == "room@conf.example.org" && n == "bob"),
            "{c:?}"
        );
        let c = run(
            "<message xmlns='jabber:client' type='groupchat' from='room@conf.example.org/alice'><composing xmlns='http://jabber.org/protocol/chatstates'/></message>",
            None,
            Some(("room@conf.example.org", "alice")),
        );
        assert!(matches!(c, Classified::Ignore("groupchat without body")), "{c:?}");
        // A message with a body is stored as usual.
        let p = stored(run(
            "<message xmlns='jabber:client' type='chat' from='bob@example.org/phone'><body>hi</body><active xmlns='http://jabber.org/protocol/chatstates'/></message>",
            None,
            None,
        ));
        assert_eq!(p.conversation, "bob@example.org");
    }
}
