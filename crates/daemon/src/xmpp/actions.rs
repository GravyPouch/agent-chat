//! Outbound operations: login setup, roster, presence, archives, rooms, sending.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use agent_chat_proto::ipc::{EncryptionMode, ErrorCode, Event, IpcError};
use agent_chat_proto::model::{Message, MessageKind, MessageSource, PresenceInfo, Room, Show};
use anyhow::{Context, Result};
use chrono::Utc;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::carbons;
use xmpp_parsers::disco::{DiscoInfoQuery, DiscoInfoResult, DiscoItemsQuery, DiscoItemsResult};
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::mam::{self, QueryId};
use xmpp_parsers::message::{Id as MessageId, Lang, Message as XMessage};
use xmpp_parsers::muc::muc::{History, Muc};
use xmpp_parsers::presence::{Presence, Show as XShow};
use xmpp_parsers::roster::{Ask, Item as RosterItem, Roster, Subscription};
use xmpp_parsers::rsm::SetQuery;
use xmpp_parsers::stanza_id::OriginId;

use crate::core::{Core, RoomRuntime, RosterEntry};
use crate::store::NewMessage;
use crate::xmpp::session::SessionHandle;

const MAM_PAGE: usize = 100;
/// Safety valve for catch-up after long offline periods.
const MAM_MAX_PAGES: usize = 50;

/// Everything to do once a fresh stream is bound.
pub async fn after_login(core: &Arc<Core>, handle: &SessionHandle) -> Result<()> {
    if let Err(e) = fetch_roster(core, handle).await {
        tracing::warn!("roster fetch failed: {e:#}");
    }
    // An agent announces whether anything is listening from the start.
    crate::agent::refresh_presence(core);
    send_presence(core, handle)?;
    if let Err(e) = handle.iq_set(None, carbons::Enable).await {
        tracing::warn!("could not enable carbons: {e:#}");
    }
    if let Err(e) = crate::omemo::xmpp::after_login(core, handle).await {
        tracing::warn!("OMEMO setup failed: {e:#}");
    }
    if let Err(e) = sync_archive(core, handle, None).await {
        tracing::warn!("archive sync failed: {e:#}");
    }
    // The default consumer starts after the first backfill, so agents get
    // everything that arrives from now on but aren't flooded with old history.
    core.store().ensure_consumer("default")?;
    let rooms = core.store().rooms()?;
    for (jid, nick, password, autojoin) in rooms {
        if autojoin && let Err(e) = start_join(core, handle, &jid, &nick, password.as_deref(), false).await {
            tracing::warn!("rejoining {jid} failed: {e:#}");
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- roster

pub async fn fetch_roster(core: &Arc<Core>, handle: &SessionHandle) -> Result<()> {
    let el = handle
        .iq_get(
            None,
            Roster {
                ver: None,
                items: vec![],
            },
        )
        .await?
        .context("empty roster result")?;
    apply_roster(core, Roster::try_from(el)?, false);
    Ok(())
}

pub fn apply_roster(core: &Core, roster: Roster, push: bool) {
    {
        let mut st = core.state();
        if !push {
            st.roster.clear();
        }
        for item in roster.items {
            let jid = item.jid.to_string();
            if item.subscription == Subscription::Remove {
                st.roster.remove(&jid);
                continue;
            }
            st.roster.insert(
                jid,
                RosterEntry {
                    name: item.name,
                    subscription: subscription_str(&item.subscription).to_owned(),
                    pending_out: item.ask == Ask::Subscribe,
                    groups: item.groups.into_iter().map(|g| g.0).collect(),
                },
            );
        }
    }
    core.emit(Event::ContactsChanged);
}

fn subscription_str(s: &Subscription) -> &'static str {
    match s {
        Subscription::None => "none",
        Subscription::From => "from",
        Subscription::To => "to",
        Subscription::Both => "both",
        Subscription::Remove => "remove",
    }
}

fn parse_bare(jid: &str) -> Result<BareJid, IpcError> {
    BareJid::new(jid).map_err(|e| IpcError::new(ErrorCode::BadRequest, format!("invalid JID {jid:?}: {e}")))
}

fn internal(e: impl std::fmt::Display) -> IpcError {
    IpcError::new(ErrorCode::Internal, e.to_string())
}

pub async fn add_contact(handle: &SessionHandle, jid: &str, name: Option<String>) -> Result<(), IpcError> {
    use agent_chat_proto::address::{ChatTarget, parse_chat_target};
    // "bob" on our own server, or a pasted xmpp: link.
    let jid = match parse_chat_target(jid, Some(handle.own.domain().as_str())) {
        Some(ChatTarget::Contact(j)) => j,
        Some(ChatTarget::Room(r)) => {
            return Err(IpcError::new(
                ErrorCode::BadRequest,
                format!("{r} is a group chat; join it with `agent-chat rooms join {r}`"),
            ));
        }
        None => jid.to_owned(),
    };
    let bare = parse_bare(&jid)?;
    let item = RosterItem {
        jid: bare.clone(),
        name,
        subscription: Subscription::None,
        ask: Ask::None,
        groups: vec![],
        approved: None,
    };
    handle
        .iq_set(
            None,
            Roster {
                ver: None,
                items: vec![item],
            },
        )
        .await
        .map_err(internal)?;
    handle.send(Presence::subscribe().with_to(bare)).map_err(internal)?;
    Ok(())
}

pub async fn remove_contact(handle: &SessionHandle, jid: &str) -> Result<(), IpcError> {
    let bare = parse_bare(jid)?;
    let item = RosterItem {
        jid: bare,
        name: None,
        subscription: Subscription::Remove,
        ask: Ask::None,
        groups: vec![],
        approved: None,
    };
    handle
        .iq_set(
            None,
            Roster {
                ver: None,
                items: vec![item],
            },
        )
        .await
        .map_err(internal)?;
    Ok(())
}

pub fn approve_subscription(core: &Core, handle: &SessionHandle, jid: &str) -> Result<(), IpcError> {
    let bare = parse_bare(jid)?;
    handle
        .send(Presence::subscribed().with_to(bare.clone()))
        .map_err(internal)?;
    // Ask for theirs too, unless we already have it.
    let subscribed_to_them = core
        .state()
        .roster
        .get(&bare.to_string())
        .is_some_and(|e| e.subscription == "to" || e.subscription == "both" || e.pending_out);
    if !subscribed_to_them {
        handle
            .send(Presence::subscribe().with_to(bare.clone()))
            .map_err(internal)?;
    }
    core.state().pending_requests.remove(&bare.to_string());
    core.emit(Event::ContactsChanged);
    Ok(())
}

pub fn deny_subscription(core: &Core, handle: &SessionHandle, jid: &str) -> Result<(), IpcError> {
    let bare = parse_bare(jid)?;
    handle
        .send(Presence::new(xmpp_parsers::presence::Type::Unsubscribed).with_to(bare.clone()))
        .map_err(internal)?;
    core.state().pending_requests.remove(&bare.to_string());
    core.emit(Event::ContactsChanged);
    Ok(())
}

// ----------------------------------------------------------------- presence

fn make_presence(info: &PresenceInfo, kind: agent_chat_proto::model::AccountKind) -> Presence {
    let mut p = Presence::available();
    p.show = match info.show {
        Show::Available | Show::Unavailable => None,
        Show::Chat => Some(XShow::Chat),
        Show::Away => Some(XShow::Away),
        Show::Xa => Some(XShow::Xa),
        Show::Dnd => Some(XShow::Dnd),
    };
    if let Some(s) = &info.status {
        p.set_status("", s.as_str());
    }
    p.add_payload(crate::xmpp::disco::caps(kind));
    p
}

pub fn send_presence(core: &Core, handle: &SessionHandle) -> Result<()> {
    let info = core.state().status.presence.clone();
    if info.show == Show::Unavailable {
        handle.send(Presence::unavailable())?;
    } else {
        handle.send(make_presence(&info, core.account_kind()))?;
    }
    Ok(())
}

pub fn set_presence(core: &Core, handle: &SessionHandle, info: PresenceInfo) -> Result<(), IpcError> {
    core.state().status.presence = info;
    send_presence(core, handle).map_err(internal)?;
    // Rooms need directed presence to see the change.
    let rooms: Vec<(String, String)> = core
        .state()
        .rooms
        .iter()
        .filter(|(_, r)| r.joined)
        .map(|(j, r)| (j.clone(), r.nick.clone()))
        .collect();
    let info = core.state().status.presence.clone();
    for (room, nick) in rooms {
        if let Ok(to) = Jid::new(&format!("{room}/{nick}")) {
            let _ = handle.send(make_presence(&info, core.account_kind()).with_to(to));
        }
    }
    core.emit(Event::Status(core.status()));
    Ok(())
}

// ------------------------------------------------------------------ archive

/// Catch up on an archive (our account's when `room` is `None`).
///
/// With a stored position we page forward from it; on first sync we fetch
/// only the most recent page.
pub async fn sync_archive(core: &Arc<Core>, handle: &SessionHandle, room: Option<&str>) -> Result<()> {
    let archive = room.map(str::to_owned).unwrap_or_else(|| handle.own.to_string());
    let to = match room {
        Some(r) => Some(Jid::new(r)?),
        None => None,
    };
    let backfill = core.config.lock().unwrap().settings.mam_backfill as usize;
    let mut after = core.store().last_archive_id(&archive)?;
    let first_sync = after.is_none();

    for _ in 0..MAM_MAX_PAGES {
        let qid = uuid::Uuid::new_v4().simple().to_string();
        core.state().mam_queries.insert(qid.clone(), archive.clone());
        let set = match &after {
            Some(a) => SetQuery {
                max: Some(MAM_PAGE),
                after: Some(a.clone()),
                before: None,
                index: None,
            },
            None => SetQuery {
                max: Some(backfill.max(1)),
                after: None,
                before: Some(String::new()),
                index: None,
            },
        };
        let query = mam::Query {
            queryid: Some(QueryId(qid.clone())),
            node: None,
            form: None,
            set: Some(set),
            flip_page: false,
        };
        let result = handle.iq_set(to.clone(), query).await;
        core.state().mam_queries.remove(&qid);
        let el = result?.context("empty MAM result")?;
        let fin = mam::Fin::try_from(el)?;
        // Results arrived before the <fin/> and were stored in order.
        if let Some(last) = fin.set.last {
            core.store().set_last_archive_id(&archive, &last)?;
            after = Some(last);
        }
        if fin.complete || first_sync || after.is_none() {
            break;
        }
    }
    Ok(())
}

// -------------------------------------------------------------------- rooms

/// Begin joining a room; completion is signalled by our self-presence.
async fn room_disco(handle: &SessionHandle, room: &BareJid) -> Option<DiscoInfoResult> {
    handle
        .iq_get(Some(room.clone().into()), DiscoInfoQuery { node: None })
        .await
        .ok()
        .flatten()
        .and_then(|el| DiscoInfoResult::try_from(el).ok())
}

fn apply_room_features(rt: &mut RoomRuntime, info: &DiscoInfoResult) {
    rt.mam = info.features.contains(xmpp_parsers::ns::MAM);
    rt.nonanonymous = info.features.contains("muc_nonanonymous");
    rt.membersonly = info.features.contains("muc_membersonly");
    rt.name = info
        .identities
        .iter()
        .find(|i| i.category == "conference")
        .and_then(|i| i.name.clone())
        .filter(|n| !n.is_empty());
}

/// The account's group chat service: the first `conference/text` item of
/// our server (disco#items, then disco#info on each), else
/// `conference.<domain>`. Remembered for the session.
pub async fn muc_service(core: &Core, handle: &SessionHandle) -> String {
    if let Some(s) = core.state().muc_service.clone() {
        return s;
    }
    let domain = handle.own.domain().to_string();
    let mut found = None;
    let server: Option<Jid> = Jid::new(&domain).ok();
    if let Ok(Some(el)) = handle.iq_get(server, DiscoItemsQuery { node: None, rsm: None }).await
        && let Ok(items) = DiscoItemsResult::try_from(el)
    {
        for item in items.items.iter().filter(|i| i.node.is_none()).take(20) {
            let info = handle
                .iq_get(Some(item.jid.clone()), DiscoInfoQuery { node: None })
                .await
                .ok()
                .flatten()
                .and_then(|el| DiscoInfoResult::try_from(el).ok());
            if info.is_some_and(|i| {
                i.identities
                    .iter()
                    .any(|id| id.category == "conference" && id.type_ == "text")
            }) {
                found = Some(item.jid.to_string());
                break;
            }
        }
    }
    let service = found.unwrap_or_else(|| format!("conference.{domain}"));
    core.state().muc_service = Some(service.clone());
    service
}

/// Create a group chat with a readable address on our group chat service,
/// join it, wait until it's configured, and invite people.
pub async fn create_room(
    core: &Arc<Core>,
    handle: &SessionHandle,
    name: Option<String>,
    private: bool,
    invite: Vec<String>,
    nick: Option<String>,
) -> Result<Room, IpcError> {
    use agent_chat_proto::rooms::{random_room_name, random_u64, room_slug};
    let service = muc_service(core, handle).await;
    let name = name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    let base = name.as_deref().and_then(room_slug).unwrap_or_else(random_room_name);
    // Never walk into somebody else's room by accident: find a free address.
    let mut room = None;
    for attempt in 0..6 {
        let local = match attempt {
            0 => base.clone(),
            _ if name.is_none() => random_room_name(),
            _ => format!("{base}-{}", 10 + random_u64() % 90),
        };
        let jid = BareJid::new(&format!("{local}@{service}")).map_err(internal)?;
        if room_disco(handle, &jid).await.is_none() {
            room = Some(jid.to_string());
            break;
        }
    }
    let room = room.ok_or_else(|| IpcError::new(ErrorCode::Internal, "couldn't find a free room address"))?;
    {
        // Picked up when our join creates the room.
        let mut st = core.state();
        let rt = st.rooms.entry(room.to_lowercase()).or_default();
        rt.create_name = name.clone();
    }
    join_room(core, handle, &room, nick, None, private).await?;
    // Invitations only work once the new room is configured (unlocked).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut events = core.events.subscribe();
    loop {
        let ready = core
            .state()
            .rooms
            .get(&room.to_lowercase())
            .is_some_and(|r| r.configured && (!private || r.is_private()));
        if ready || tokio::time::timeout_at(deadline, events.recv()).await.is_err() {
            break;
        }
    }
    for jid in invite {
        if let Err(e) = invite_to_room(core, handle, &room, &jid, None).await {
            tracing::warn!("could not invite {jid} to {room}: {e}");
        }
    }
    core.rooms()
        .map_err(internal)?
        .into_iter()
        .find(|r| r.jid.eq_ignore_ascii_case(&room))
        .ok_or_else(|| internal("room vanished"))
}

async fn start_join(
    core: &Arc<Core>,
    handle: &SessionHandle,
    room: &str,
    nick: &str,
    password: Option<&str>,
    private: bool,
) -> Result<()> {
    let room_jid = BareJid::new(room)?;
    let info = room_disco(handle, &room_jid).await;
    let mam = info
        .as_ref()
        .is_some_and(|i| i.features.contains(xmpp_parsers::ns::MAM));
    {
        let mut st = core.state();
        let rt = st.rooms.entry(room_jid.to_string().to_lowercase()).or_default();
        rt.nick = nick.to_owned();
        rt.password = password.map(str::to_owned);
        rt.joined = false;
        rt.configured = false;
        rt.error = None;
        rt.create_private = private;
        if let Some(info) = &info {
            apply_room_features(rt, info);
        }
    }
    let mut muc = Muc::new();
    if let Some(pw) = password {
        muc = muc.with_password(pw.to_owned());
    }
    if mam {
        // History comes from the archive instead, deduplicated by stanza-id.
        muc = muc.with_history(History::new().with_maxstanzas(0));
    }
    let to = room_jid.with_resource_str(nick)?;
    let info = core.state().status.presence.clone();
    handle.send(make_presence(&info, core.account_kind()).with_to(to).with_payload(muc))?;
    core.emit(Event::RoomsChanged);
    Ok(())
}

pub async fn join_room(
    core: &Arc<Core>,
    handle: &SessionHandle,
    room: &str,
    nick: Option<String>,
    password: Option<String>,
    private: bool,
) -> Result<Room, IpcError> {
    // A bare name means a room on our own group chat service.
    let room = if room.contains('@') {
        room.to_owned()
    } else {
        agent_chat_proto::rooms::room_address(room, &muc_service(core, handle).await)
    };
    let room_jid = parse_bare(&room)?;
    if room_jid.node().is_none() {
        return Err(IpcError::new(
            ErrorCode::BadRequest,
            "room JIDs look like name@conference.server",
        ));
    }
    let nick = nick
        .or_else(|| core.config.lock().unwrap().settings.default_nick.clone())
        .unwrap_or_else(|| {
            handle
                .own
                .node()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "agent".into())
        });
    let room_s = room_jid.to_string();
    // Accepting an invitation: use its password and forget it.
    let invite = {
        let mut st = core.state();
        let pos = st
            .pending_invites
            .iter()
            .position(|i| i.room.eq_ignore_ascii_case(&room_s));
        pos.map(|p| st.pending_invites.remove(p))
    };
    let password = password.or_else(|| invite.and_then(|i| i.password));
    let mut events = core.events.subscribe();
    let already_saved = core.store().room(&room_s).map_err(internal)?.is_some();
    start_join(core, handle, &room_s, &nick, password.as_deref(), private)
        .await
        .map_err(internal)?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let failure = loop {
        {
            let st = core.state();
            if let Some(rt) = st.rooms.get(&room_s.to_lowercase()) {
                if rt.joined {
                    break None;
                }
                if let Some(err) = &rt.error {
                    break Some(IpcError::new(
                        ErrorCode::BadRequest,
                        format!("could not join {room_s}: {err}"),
                    ));
                }
            }
        }
        if tokio::time::timeout_at(deadline, events.recv()).await.is_err() {
            break Some(IpcError::new(
                ErrorCode::NotConnected,
                format!("timed out joining {room_s}"),
            ));
        }
    };
    if let Some(err) = failure {
        if !already_saved {
            core.state().rooms.remove(&room_s.to_lowercase());
            core.emit(Event::RoomsChanged);
        }
        return Err(err);
    }
    // If our join created the room, it stays locked (others get
    // item-not-found) until it's configured: wait for that, so whoever we
    // tell about the room can join it right away.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let configured = core
            .state()
            .rooms
            .get(&room_s.to_lowercase())
            .is_none_or(|r| r.configured);
        if configured || tokio::time::timeout_at(deadline, events.recv()).await.is_err() {
            break;
        }
    }
    core.store()
        .upsert_room(&room_s, &nick, password.as_deref())
        .map_err(internal)?;
    core.rooms()
        .map_err(internal)?
        .into_iter()
        .find(|r| r.jid.eq_ignore_ascii_case(&room_s))
        .ok_or_else(|| internal("room vanished"))
}

/// Configure a room we just created: private (members-only, non-anonymous,
/// persistent, hidden) for encrypted rooms, otherwise the default "instant
/// room" (XEP-0045 §10.1.2), with a display name if one was chosen.
/// Unlocks the room either way.
pub async fn configure_new_room(
    core: &Arc<Core>,
    handle: &SessionHandle,
    room: &str,
    private: bool,
    name: Option<&str>,
) -> Result<()> {
    let name_field = name
        .map(|n| {
            format!(
                "<field var='muc#roomconfig_roomname'><value>{}</value></field>",
                crate::omemo::xml::escape(n)
            )
        })
        .unwrap_or_default();
    let form = if private {
        format!(
            "<x xmlns='jabber:x:data' type='submit'>\
             <field var='FORM_TYPE'><value>http://jabber.org/protocol/muc#roomconfig</value></field>\
             <field var='muc#roomconfig_membersonly'><value>1</value></field>\
             <field var='muc#roomconfig_whois'><value>anyone</value></field>\
             <field var='muc#roomconfig_persistentroom'><value>1</value></field>\
             <field var='muc#roomconfig_publicroom'><value>0</value></field>\
             {name_field}</x>"
        )
    } else if name_field.is_empty() {
        "<x xmlns='jabber:x:data' type='submit'/>".to_owned()
    } else {
        format!(
            "<x xmlns='jabber:x:data' type='submit'>\
             <field var='FORM_TYPE'><value>http://jabber.org/protocol/muc#roomconfig</value></field>\
             {name_field}</x>"
        )
    };
    let query: Element = format!("<query xmlns='http://jabber.org/protocol/muc#owner'>{form}</query>").parse()?;
    handle.iq_set(Some(Jid::new(room)?), query).await?;
    if let Some(rt) = core.state().rooms.get_mut(&room.to_lowercase()) {
        rt.create_name = None;
    }
    refresh_room_info(core, handle, room).await
}

/// Re-read a room's features and (for private rooms) its member list.
pub async fn refresh_room_info(core: &Arc<Core>, handle: &SessionHandle, room: &str) -> Result<()> {
    let room_jid = BareJid::new(room)?;
    let key = room_jid.to_string().to_lowercase();
    let info = room_disco(handle, &room_jid).await;
    if let Some(rt) = core.state().rooms.get_mut(&key) {
        if let Some(info) = &info {
            apply_room_features(rt, info);
        }
        rt.configured = true;
    }
    if core.room_is_private(room) {
        let mut members = BTreeSet::new();
        for affiliation in ["owner", "admin", "member"] {
            let query: Element = format!(
                "<query xmlns='http://jabber.org/protocol/muc#admin'><item affiliation='{affiliation}'/></query>"
            )
            .parse()?;
            // Ordinary members may not be allowed to list affiliations; then
            // we rely on the occupants' real JIDs.
            if let Ok(Some(result)) = handle.iq_get(Some(room_jid.clone().into()), query).await {
                for item in result.children().filter(|c| c.name() == "item") {
                    if let Some(jid) = item.attr("jid").and_then(|j| BareJid::new(j).ok()) {
                        members.insert(jid.to_string());
                    }
                }
            }
        }
        if let Some(rt) = core.state().rooms.get_mut(&key) {
            rt.members.extend(members);
        }
    }
    core.emit(Event::RoomsChanged);
    Ok(())
}

/// Make `jid` a member (when we may) and send a mediated invitation.
pub async fn invite_to_room(
    core: &Arc<Core>,
    handle: &SessionHandle,
    room: &str,
    jid: &str,
    reason: Option<String>,
) -> Result<(), IpcError> {
    let room_jid = parse_bare(room)?;
    let invitee = parse_bare(jid)?;
    let key = room_jid.to_string().to_lowercase();
    if !core.state().rooms.get(&key).is_some_and(|r| r.joined) {
        return Err(IpcError::new(ErrorCode::BadRequest, format!("join {room_jid} first")));
    }
    let item = format!(
        "<query xmlns='http://jabber.org/protocol/muc#admin'><item affiliation='member' jid='{}'/></query>",
        crate::omemo::xml::escape(&invitee.to_string())
    );
    if let Err(e) = handle
        .iq_set(
            Some(room_jid.clone().into()),
            item.parse::<Element>().map_err(internal)?,
        )
        .await
    {
        tracing::info!("could not make {invitee} a member of {room_jid} ({e:#}); sending the invitation anyway");
    } else if let Some(rt) = core.state().rooms.get_mut(&key) {
        rt.members.insert(invitee.to_string());
    }
    let reason = reason
        .map(|r| format!("<reason>{}</reason>", crate::omemo::xml::escape(&r)))
        .unwrap_or_default();
    let invite: Element = format!(
        "<x xmlns='http://jabber.org/protocol/muc#user'><invite to='{}'>{reason}</invite></x>",
        crate::omemo::xml::escape(&invitee.to_string())
    )
    .parse()
    .map_err(internal)?;
    // XEP-0045 §7.8.2: mediated invitations are type "normal" (chat is rejected).
    let mut msg = XMessage::normal(Some(room_jid.into()));
    msg.payloads.push(invite);
    handle.send(msg).map_err(internal)?;
    core.emit(Event::RoomsChanged);
    Ok(())
}

pub fn leave_room(core: &Core, handle: &SessionHandle, room: &str) -> Result<(), IpcError> {
    let room_jid = parse_bare(room)?;
    let key = room_jid.to_string().to_lowercase();
    let nick = core.state().rooms.remove(&key).map(|r| r.nick);
    if let Some(nick) = nick
        && let Ok(to) = room_jid.with_resource_str(&nick)
    {
        let _ = handle.send(Presence::unavailable().with_to(to));
    }
    core.store().remove_room(&room_jid.to_string()).map_err(internal)?;
    core.emit(Event::RoomsChanged);
    Ok(())
}

// ------------------------------------------------------------------ sending

pub async fn send_message(
    core: &Arc<Core>,
    handle: &SessionHandle,
    to: &str,
    body: String,
    room: Option<bool>,
    mode: EncryptionMode,
) -> Result<Message, IpcError> {
    if body.is_empty() {
        return Err(IpcError::new(ErrorCode::BadRequest, "message body is empty"));
    }
    let to_jid = Jid::new(to).map_err(|e| IpcError::new(ErrorCode::BadRequest, format!("invalid JID {to:?}: {e}")))?;
    let to_bare = to_jid.to_bare().to_string();
    let known_room = core.room_nick(&to_bare);
    let joined_room = core.joined_room_nick(&to_bare);
    let kind = match (room, &known_room, to_jid.resource().is_some()) {
        (Some(true), _, _) => MessageKind::Groupchat,
        (Some(false), _, _) => MessageKind::Chat,
        (None, Some(_), true) => MessageKind::MucPrivate,
        (None, Some(_), false) => MessageKind::Groupchat,
        (None, None, _) => MessageKind::Chat,
    };
    if matches!(kind, MessageKind::Groupchat | MessageKind::MucPrivate) && joined_room.is_none() {
        return Err(IpcError::new(
            ErrorCode::BadRequest,
            format!("not in room {to_bare}; run `agent-chat rooms join {to_bare}` first"),
        ));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let target: Jid = if kind == MessageKind::Groupchat {
        BareJid::new(&to_bare).map_err(internal)?.into()
    } else {
        to_jid.clone()
    };
    let mut msg = match kind {
        MessageKind::Groupchat => XMessage::groupchat(Some(target)),
        _ => XMessage::chat(Some(target)),
    };
    msg.id = Some(MessageId(id.clone()));
    msg = msg.with_payload(OriginId { id: id.clone() });
    if kind != MessageKind::MucPrivate {
        // XEP-0085: a message ends "composing"; say so explicitly.
        msg = msg.with_payload(xmpp_parsers::chatstates::ChatState::Active);
    }

    // End-to-end encryption.
    let mut encryption = None;
    if mode != EncryptionMode::Plaintext {
        match crate::omemo::xmpp::encrypt_outgoing(core, handle, kind, &to_bare, &body).await {
            Ok(Some((payloads, info))) => {
                for p in payloads {
                    msg.payloads.push(p);
                }
                msg.bodies
                    .insert(Lang::default(), crate::omemo::FALLBACK_BODY.to_owned());
                encryption = Some(info);
            }
            Ok(None) if mode == EncryptionMode::Require => {
                return Err(IpcError::new(
                    ErrorCode::EncryptionUnavailable,
                    format!("{to_bare} has no usable OMEMO devices; refusing to send plaintext"),
                ));
            }
            Ok(None) => {}
            Err(e) if mode == EncryptionMode::Require => {
                return Err(IpcError::new(ErrorCode::EncryptionUnavailable, format!("{e:#}")));
            }
            Err(e) => {
                // Auto mode never silently downgrades after deciding to encrypt
                // fails for a contact that *has* OMEMO devices.
                return Err(IpcError::new(
                    ErrorCode::EncryptionUnavailable,
                    format!("encryption failed ({e:#}); retry, or send with --plaintext"),
                ));
            }
        }
    }
    if encryption.is_none() {
        msg.bodies.insert(Lang::default(), body.clone());
    }
    // Ask the server to archive even body-less encrypted messages (XEP-0334).
    msg.payloads.push(Element::builder("store", "urn:xmpp:hints").build());

    let (conversation, sender, nick) = match kind {
        MessageKind::Groupchat => {
            let nick = joined_room.clone().unwrap_or_default();
            (to_bare.clone(), format!("{to_bare}/{nick}"), Some(nick))
        }
        MessageKind::MucPrivate => (to_jid.to_string(), handle.own.to_string(), None),
        MessageKind::Chat => (to_bare.clone(), handle.own.to_string(), None),
    };
    let stored = core
        .store_message(&NewMessage {
            conversation,
            kind,
            sender,
            nick,
            body,
            timestamp: Utc::now(),
            delayed: false,
            source: MessageSource::Local,
            own: true,
            archive: None,
            stanza_id: None,
            origin_id: Some(id.clone()),
            xmpp_id: Some(id),
            encryption,
        })
        .map_err(internal)?
        .ok_or_else(|| internal("message id collision"))?;
    handle
        .send(msg)
        .map_err(|e| IpcError::new(ErrorCode::NotConnected, e.to_string()))?;
    Ok(stored)
}

/// XEP-0085: tell a contact or a joined room we're typing (or stopped).
/// Never stored (XEP-0334 `no-store`).
pub fn send_chat_state(
    core: &Core,
    handle: &SessionHandle,
    to: &str,
    state: agent_chat_proto::model::ChatState,
) -> Result<(), IpcError> {
    use agent_chat_proto::model::ChatState;
    use xmpp_parsers::chatstates::ChatState as XChatState;

    let to_jid =
        BareJid::new(to).map_err(|e| IpcError::new(ErrorCode::BadRequest, format!("invalid bare JID {to:?}: {e}")))?;
    let to_s = to_jid.to_string();
    let room = core.room_nick(&to_s).is_some();
    if room && core.joined_room_nick(&to_s).is_none() {
        return Err(IpcError::new(
            ErrorCode::BadRequest,
            format!("not in room {to_s}; run `agent-chat rooms join {to_s}` first"),
        ));
    }
    let payload = match state {
        ChatState::Active => XChatState::Active,
        ChatState::Composing => XChatState::Composing,
        ChatState::Paused => XChatState::Paused,
        ChatState::Inactive => XChatState::Inactive,
        ChatState::Gone => XChatState::Gone,
    };
    let mut msg = if room {
        XMessage::groupchat(Some(to_jid.into()))
    } else {
        XMessage::chat(Some(to_jid.into()))
    }
    .with_payload(payload);
    msg.payloads
        .push(Element::builder("no-store", "urn:xmpp:hints").build());
    handle
        .send(msg)
        .map_err(|e| IpcError::new(ErrorCode::NotConnected, e.to_string()))
}
