//! Outbound operations: login setup, roster, presence, archives, rooms, sending.

use std::sync::Arc;
use std::time::Duration;

use agent_chat_proto::ipc::{EncryptionMode, ErrorCode, Event, IpcError};
use agent_chat_proto::model::{Message, MessageKind, MessageSource, PresenceInfo, Room, Show};
use anyhow::{Context, Result};
use chrono::Utc;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::carbons;
use xmpp_parsers::disco::{DiscoInfoQuery, DiscoInfoResult};
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
        if autojoin
            && let Err(e) = start_join(core, handle, &jid, &nick, password.as_deref()).await
        {
            tracing::warn!("rejoining {jid} failed: {e:#}");
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- roster

pub async fn fetch_roster(core: &Arc<Core>, handle: &SessionHandle) -> Result<()> {
    let el = handle
        .iq_get(None, Roster { ver: None, items: vec![] })
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
    let bare = parse_bare(jid)?;
    let item = RosterItem {
        jid: bare.clone(),
        name,
        subscription: Subscription::None,
        ask: Ask::None,
        groups: vec![],
        approved: None,
    };
    handle
        .iq_set(None, Roster { ver: None, items: vec![item] })
        .await
        .map_err(internal)?;
    handle
        .send(Presence::subscribe().with_to(bare))
        .map_err(internal)?;
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
        .iq_set(None, Roster { ver: None, items: vec![item] })
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
        handle.send(Presence::subscribe().with_to(bare.clone())).map_err(internal)?;
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

fn make_presence(info: &PresenceInfo) -> Presence {
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
    p.add_payload(crate::xmpp::disco::caps());
    p
}

pub fn send_presence(core: &Core, handle: &SessionHandle) -> Result<()> {
    let info = core.state().status.presence.clone();
    if info.show == Show::Unavailable {
        handle.send(Presence::unavailable())?;
    } else {
        handle.send(make_presence(&info))?;
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
            let _ = handle.send(make_presence(&info).with_to(to));
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
async fn start_join(
    core: &Arc<Core>,
    handle: &SessionHandle,
    room: &str,
    nick: &str,
    password: Option<&str>,
) -> Result<()> {
    let room_jid = BareJid::new(room)?;
    let info: Option<DiscoInfoResult> = handle
        .iq_get(Some(room_jid.clone().into()), DiscoInfoQuery { node: None })
        .await
        .ok()
        .flatten()
        .and_then(|el| DiscoInfoResult::try_from(el).ok());
    let mam = info
        .as_ref()
        .is_some_and(|i| i.features.contains(xmpp_parsers::ns::MAM));
    {
        let mut st = core.state();
        let rt = st.rooms.entry(room_jid.to_string().to_lowercase()).or_insert_with(RoomRuntime::default);
        rt.nick = nick.to_owned();
        rt.password = password.map(str::to_owned);
        rt.joined = false;
        rt.error = None;
        rt.mam = mam;
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
    handle.send(make_presence(&info).with_to(to).with_payload(muc))?;
    core.emit(Event::RoomsChanged);
    Ok(())
}

pub async fn join_room(
    core: &Arc<Core>,
    handle: &SessionHandle,
    room: &str,
    nick: Option<String>,
    password: Option<String>,
) -> Result<Room, IpcError> {
    let room_jid = parse_bare(room)?;
    if room_jid.node().is_none() {
        return Err(IpcError::new(ErrorCode::BadRequest, "room JIDs look like name@conference.server"));
    }
    let nick = nick
        .or_else(|| core.config.lock().unwrap().settings.default_nick.clone())
        .unwrap_or_else(|| handle.own.node().map(|n| n.to_string()).unwrap_or_else(|| "agent".into()));
    let room_s = room_jid.to_string();
    let mut events = core.events.subscribe();
    let already_saved = core.store().room(&room_s).map_err(internal)?.is_some();
    start_join(core, handle, &room_s, &nick, password.as_deref())
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
                    break Some(IpcError::new(ErrorCode::BadRequest, format!("could not join {room_s}: {err}")));
                }
            }
        }
        if tokio::time::timeout_at(deadline, events.recv()).await.is_err() {
            break Some(IpcError::new(ErrorCode::NotConnected, format!("timed out joining {room_s}")));
        }
    };
    if let Some(err) = failure {
        if !already_saved {
            core.state().rooms.remove(&room_s.to_lowercase());
            core.emit(Event::RoomsChanged);
        }
        return Err(err);
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

    // End-to-end encryption.
    let mut encryption = None;
    if mode != EncryptionMode::Plaintext {
        match crate::omemo::xmpp::encrypt_outgoing(core, handle, kind, &to_bare, &body).await {
            Ok(Some((payloads, info))) => {
                for p in payloads {
                    msg.payloads.push(p);
                }
                msg.bodies.insert(Lang::default(), crate::omemo::FALLBACK_BODY.to_owned());
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
    handle.send(msg).map_err(|e| IpcError::new(ErrorCode::NotConnected, e.to_string()))?;
    Ok(stored)
}
