//! XMPP side of OMEMO: PEP device lists and bundles, encryption policy,
//! sending and receiving.

use std::sync::Arc;
use std::time::Duration;

use agent_chat_proto::ipc::{ErrorCode, Event, IpcError};
use agent_chat_proto::model::{DeviceInfo, EncryptionInfo, EncryptionProtocol, MessageKind, Trust};
use anyhow::{Context as _, Result, anyhow, bail};
use omemo::{RemoteBundle, Version, fingerprint, payload};
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::message::Message;

use super::xml::{self, Encrypted, KeyEntry};
use super::{Manager, NS_LEGACY, NS_OMEMO2, VERSIONS};
use crate::core::Core;
use crate::xmpp::inbound::Parsed;
use crate::xmpp::session::SessionHandle;

/// Refetch a contact's device lists when sending if older than this.
const DEVICE_LIST_MAX_AGE_SECS: i64 = 600;

pub fn protocol_of(v: Version) -> EncryptionProtocol {
    match v {
        Version::Omemo2 => EncryptionProtocol::Omemo2,
        Version::Legacy => EncryptionProtocol::OmemoLegacy,
    }
}

fn version_of(p: EncryptionProtocol) -> Version {
    match p {
        EncryptionProtocol::Omemo2 => Version::Omemo2,
        EncryptionProtocol::OmemoLegacy => Version::Legacy,
    }
}

/// The OMEMO manager, opened on first use.
pub fn manager(core: &Core) -> Result<Arc<Manager>> {
    let mut slot = core.omemo.lock().unwrap();
    if let Some(m) = slot.as_ref() {
        return Ok(m.clone());
    }
    let m = Arc::new(Manager::open(&core.profile.db_path())?);
    *slot = Some(m.clone());
    Ok(m)
}

// --------------------------------------------------------------------- PEP

pub fn is_pep_event(m: &Message) -> bool {
    m.payloads.iter().any(|el| el.is("event", xml::NS_PUBSUB_EVENT))
}

/// Device-list notifications (we advertise `+notify` for both versions).
pub fn on_pep_event(core: &Arc<Core>, handle: &SessionHandle, m: &Message) {
    let from = m
        .from
        .as_ref()
        .map(|f| f.to_bare().to_string())
        .unwrap_or_else(|| handle.own.to_string());
    let Ok(mgr) = manager(core) else { return };
    for event in m.payloads.iter().filter(|el| el.is("event", xml::NS_PUBSUB_EVENT)) {
        let Some((node, payloads)) = xml::event_payloads(event) else { continue };
        for v in VERSIONS {
            if node != xml::devices_node(v) {
                continue;
            }
            let Some(list) = payloads.first().and_then(|p| xml::parse_device_list(v, p)) else {
                continue;
            };
            if let Err(e) = mgr.db.set_device_list(v, &from, &list, chrono::Utc::now().timestamp()) {
                tracing::warn!("storing device list: {e:#}");
                continue;
            }
            core.emit(Event::DevicesChanged {
                jid: from.clone(),
                protocol: protocol_of(v),
            });
            // Someone (another client of ours) overwrote our list without us.
            let ours = mgr.device_id(v);
            if from == handle.own.to_string() && !list.iter().any(|(id, _)| *id == ours) {
                let core = core.clone();
                let handle = handle.clone();
                tokio::spawn(async move {
                    if let Err(e) = publish_own(&core, &handle, v).await {
                        tracing::warn!("re-adding our device to the {v:?} list failed: {e:#}");
                    }
                });
            }
        }
    }
}

async fn fetch_device_list(handle: &SessionHandle, jid: Option<&BareJid>, v: Version) -> Result<Vec<(u32, Option<String>)>> {
    let to: Option<Jid> = jid.map(|j| j.clone().into());
    match handle.iq_get(to, xml::items_query(xml::devices_node(v), None)).await {
        Ok(Some(el)) => Ok(xml::result_payloads(&el)
            .first()
            .and_then(|p| xml::parse_device_list(v, p))
            .unwrap_or_default()),
        Ok(None) => Ok(vec![]),
        // item-not-found etc.: the account simply doesn't use this version.
        Err(e) if format!("{e}").contains("ItemNotFound") => Ok(vec![]),
        Err(e) => Err(e),
    }
}

async fn fetch_bundle(handle: &SessionHandle, jid: &str, device_id: u32, v: Version) -> Result<xml::ParsedBundle> {
    let to = Jid::new(jid)?;
    let item = match v {
        Version::Omemo2 => Some(device_id.to_string()),
        Version::Legacy => None,
    };
    let el = handle
        .iq_get(Some(to), xml::items_query(&xml::bundles_node(v, device_id), item.as_deref()))
        .await?
        .context("empty bundle result")?;
    xml::result_payloads(&el)
        .iter()
        .find_map(|p| xml::parse_bundle(v, p))
        .ok_or_else(|| anyhow!("no usable bundle for {jid}/{device_id}"))
}

async fn publish(handle: &SessionHandle, node: &str, item: &str, payload: Element, options: &[(&str, &str)]) -> Result<()> {
    match handle.iq_set(None, xml::publish(node, item, payload.clone(), options)).await {
        Ok(_) => Ok(()),
        Err(e) if format!("{e}").contains("PreconditionNotMet") || format!("{e}").contains("Conflict") => {
            // Node exists with other settings; publish without options.
            tracing::warn!("publish-options rejected for {node} ({e}); publishing without them");
            handle.iq_set(None, xml::publish(node, item, payload, &[])).await.map(|_| ())
        }
        Err(e) => Err(e),
    }
}

async fn publish_bundle(mgr: &Manager, handle: &SessionHandle, v: Version) -> Result<()> {
    let device_id = mgr.device_id(v);
    let bundle = mgr.own_bundle(v)?;
    let mut options = vec![("pubsub#access_model", "open")];
    if v == Version::Omemo2 {
        options.push(("pubsub#max_items", "max"));
    }
    publish(
        handle,
        &xml::bundles_node(v, device_id),
        &xml::bundle_item_id(v, device_id),
        xml::bundle(v, &bundle),
        &options,
    )
    .await
}

/// Make sure our device is in our published list and our bundle is current.
async fn publish_own(core: &Arc<Core>, handle: &SessionHandle, v: Version) -> Result<()> {
    let mgr = manager(core)?;
    mgr.refresh_keys(v)?;
    publish_bundle(&mgr, handle, v).await?;
    let ours = mgr.device_id(v);
    let mut list = fetch_device_list(handle, None, v).await?;
    if !list.iter().any(|(id, _)| *id == ours) {
        list.push((ours, None));
        let ids: Vec<u32> = list.iter().map(|(id, _)| *id).collect();
        publish(
            handle,
            xml::devices_node(v),
            "current",
            xml::device_list(v, &ids),
            &[("pubsub#access_model", "open")],
        )
        .await?;
    }
    mgr.db
        .set_device_list(v, &handle.own.to_string(), &list, chrono::Utc::now().timestamp())?;
    Ok(())
}

pub async fn after_login(core: &Arc<Core>, handle: &SessionHandle) -> Result<()> {
    for v in VERSIONS {
        if let Err(e) = publish_own(core, handle, v).await {
            tracing::warn!("publishing our {v:?} OMEMO device failed: {e:#}");
        }
    }
    Ok(())
}

/// Fetch device lists we don't know (or that are stale).
async fn refresh_device_lists(mgr: &Manager, handle: &SessionHandle, jid: &BareJid) {
    let key = jid.to_string();
    let now = chrono::Utc::now().timestamp();
    for v in VERSIONS {
        let stale = {
            let c = mgr.db.conn();
            c.query_row(
                "SELECT fetched_at FROM omemo_device_lists WHERE version = ?1 AND jid = ?2",
                rusqlite::params![super::store::vkey(v), key],
                |r| r.get::<_, i64>(0),
            )
            .map(|t| now - t > DEVICE_LIST_MAX_AGE_SECS)
            .unwrap_or(true)
        };
        if !stale {
            continue;
        }
        let target = if key == handle.own.to_string() { None } else { Some(jid) };
        match fetch_device_list(handle, target, v).await {
            Ok(list) => {
                let _ = mgr.db.set_device_list(v, &key, &list, now);
            }
            Err(e) => tracing::debug!("device list of {key} ({v:?}): {e:#}"),
        }
    }
}

// ----------------------------------------------------------------- sending

/// Ensure a session with (jid, device); fetches the bundle if needed.
async fn ensure_session(mgr: &Manager, handle: &SessionHandle, v: Version, jid: &str, device_id: u32) -> Result<()> {
    if mgr.with(v, |e| e.ctx.has_session(jid, device_id)) {
        return Ok(());
    }
    let b = fetch_bundle(handle, jid, device_id, v).await?;
    if b.pre_keys.is_empty() {
        bail!("bundle of {jid}/{device_id} has no pre keys");
    }
    let idx = (uuid::Uuid::new_v4().as_bytes()[0] as usize) % b.pre_keys.len();
    let (pk_id, pk) = b.pre_keys[idx].clone();
    let remote = RemoteBundle {
        identity_key: b.identity_key,
        signed_pre_key_id: b.signed_pre_key_id,
        signed_pre_key: b.signed_pre_key,
        signed_pre_key_signature: b.signature,
        pre_key_id: pk_id,
        pre_key: pk,
    };
    mgr.with(v, |e| e.ctx.process_bundle(jid, device_id, &remote))
        .map_err(|e| anyhow!("bundle of {jid}/{device_id}: {e}"))
}

/// Recipient devices of `jid` that are active and trusted (BTBV).
fn trusted_devices(mgr: &Manager, v: Version, jid: &str, exclude: Option<u32>) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for d in mgr.db.devices(v, jid)? {
        if !d.active || Some(d.device_id) == exclude {
            continue;
        }
        if mgr.db.effective_trust(jid, d.trust)?.is_trusted() {
            out.push(d.device_id);
        }
    }
    Ok(out)
}

fn has_active_devices(mgr: &Manager, v: Version, jid: &str) -> bool {
    mgr.db
        .devices(v, jid)
        .map(|d| d.iter().any(|d| d.active))
        .unwrap_or(false)
}

/// Encrypt an outgoing message when possible.
///
/// * 1:1 chats: encrypt if the recipient publishes OMEMO devices;
///   `Ok(None)` means plaintext is the only option.
/// * Private rooms (members-only, non-anonymous): encrypt for every member;
///   fails rather than falling back to plaintext.
/// * Other rooms and MUC private messages: `Ok(None)`.
pub async fn encrypt_outgoing(
    core: &Arc<Core>,
    handle: &SessionHandle,
    kind: MessageKind,
    to: &str,
    body: &str,
) -> Result<Option<(Vec<Element>, EncryptionInfo)>> {
    let mgr = manager(core)?;
    let own = handle.own.to_string();
    match kind {
        MessageKind::Chat => {
            let peer = BareJid::new(to)?.to_string();
            refresh_lists(&mgr, handle, &[peer.clone(), own]).await;
            let Some(v) = VERSIONS.into_iter().find(|v| has_active_devices(&mgr, *v, &peer)) else {
                return Ok(None);
            };
            encrypt_for(&mgr, handle, v, &[peer], None, body).await.map(Some)
        }
        MessageKind::Groupchat if core.room_is_private(to) => {
            let room = BareJid::new(to)?.to_string();
            let members: Vec<String> = core
                .room_members(&room)
                .into_iter()
                .filter(|m| !m.eq_ignore_ascii_case(&own))
                .collect();
            if members.is_empty() {
                bail!("nobody else is in {room} yet; invite someone first (or send with --plaintext)");
            }
            let mut all = members.clone();
            all.push(own);
            refresh_lists(&mgr, handle, &all).await;
            // One message, one version: pick one every member supports.
            let Some(v) = VERSIONS
                .into_iter()
                .find(|v| members.iter().all(|m| has_active_devices(&mgr, *v, m)))
            else {
                let missing: Vec<&String> = members
                    .iter()
                    .filter(|m| !VERSIONS.iter().any(|v| has_active_devices(&mgr, *v, m)))
                    .collect();
                if missing.is_empty() {
                    bail!("the members of {room} don't share an OMEMO version");
                }
                bail!("these members of {room} have no OMEMO devices: {missing:?}");
            };
            encrypt_for(&mgr, handle, v, &members, Some(&room), body).await.map(Some)
        }
        _ => Ok(None),
    }
}

async fn refresh_lists(mgr: &Manager, handle: &SessionHandle, jids: &[String]) {
    for j in jids {
        if let Ok(bare) = BareJid::new(j) {
            refresh_device_lists(mgr, handle, &bare).await;
        }
    }
}

/// Encrypt `body` for every trusted device of `peers` and our other devices.
/// Each peer must have at least one reachable, trusted device.
async fn encrypt_for(
    mgr: &Manager,
    handle: &SessionHandle,
    v: Version,
    peers: &[String],
    room: Option<&str>,
    body: &str,
) -> Result<(Vec<Element>, EncryptionInfo)> {
    let own_s = handle.own.to_string();
    let our_id = mgr.device_id(v);

    // Establish sessions, then re-check trust (a fetched bundle may have
    // revealed a changed identity key, which resets trust).
    let mut targets: Vec<(String, u32)> = Vec::new();
    let recipients = peers.iter().map(|p| (p.clone(), None)).chain([(own_s.clone(), Some(our_id))]);
    for (jid, exclude) in recipients {
        for dev in trusted_devices(mgr, v, &jid, exclude)? {
            match ensure_session(mgr, handle, v, &jid, dev).await {
                Ok(()) => targets.push((jid.clone(), dev)),
                Err(e) => tracing::warn!("skipping {jid}/{dev}: {e:#}"),
            }
        }
    }
    let mut still_trusted = Vec::new();
    for (jid, dev) in targets {
        let exclude = (jid == own_s).then_some(our_id);
        if trusted_devices(mgr, v, &jid, exclude)?.contains(&dev) {
            still_trusted.push((jid, dev));
        }
    }
    let targets = still_trusted;
    for peer in peers {
        if !targets.iter().any(|(jid, _)| jid == peer) {
            bail!("no trusted OMEMO device of {peer} is reachable (see `agent-chat omemo devices {peer}`)");
        }
    }

    let sealed = match v {
        Version::Omemo2 => payload::omemo2::seal(&xml::envelope(body, &own_s, room)),
        Version::Legacy => payload::legacy::seal(body.as_bytes()),
    };
    let mut keys = Vec::with_capacity(targets.len());
    for (jid, dev) in &targets {
        let (data, kex) = mgr
            .with(v, |e| e.ctx.encrypt(jid, *dev, &sealed.key_material))
            .map_err(|e| anyhow!("encrypting for {jid}/{dev}: {e}"))?;
        keys.push(KeyEntry {
            jid: if v == Version::Omemo2 { jid.clone() } else { String::new() },
            rid: *dev,
            kex,
            data,
        });
    }
    let encrypted = xml::build_encrypted(
        v,
        &Encrypted {
            sid: our_id,
            keys,
            iv: sealed.iv.clone(),
            payload: Some(sealed.ciphertext.clone()),
        },
    );
    Ok((
        vec![encrypted, xml::eme(v)],
        EncryptionInfo {
            protocol: protocol_of(v),
            sender_device: our_id,
            trust: Trust::Trusted,
            error: None,
        },
    ))
}

/// Empty OMEMO message (§5.5.3) to one device: completes a session after a
/// key exchange we received.
fn send_empty(mgr: &Manager, handle: &SessionHandle, v: Version, jid: &str, device_id: u32) -> Result<()> {
    let key_material: Vec<u8> = match v {
        Version::Omemo2 => payload::omemo2::EMPTY_KEY_MATERIAL.to_vec(),
        Version::Legacy => payload::legacy::empty_key_material().to_vec(),
    };
    let (data, kex) = mgr
        .with(v, |e| e.ctx.encrypt(jid, device_id, &key_material))
        .map_err(|e| anyhow!("{e}"))?;
    let iv = (v == Version::Legacy).then(|| uuid::Uuid::new_v4().as_bytes()[..12].to_vec());
    let encrypted = xml::build_encrypted(
        v,
        &Encrypted {
            sid: mgr.device_id(v),
            keys: vec![KeyEntry {
                jid: if v == Version::Omemo2 { jid.to_owned() } else { String::new() },
                rid: device_id,
                kex,
                data,
            }],
            iv,
            payload: None,
        },
    );
    let mut msg = Message::chat(Some(Jid::new(jid)?));
    msg.payloads.push(encrypted);
    msg.payloads.push(Element::builder("store", "urn:xmpp:hints").build());
    handle.send(msg)
}

// --------------------------------------------------------------- receiving

fn decrypt_error(v: Version, sid: u32, why: impl Into<String>) -> Option<(String, Option<EncryptionInfo>)> {
    let why = why.into();
    Some((
        format!("[OMEMO message could not be decrypted: {why}]"),
        Some(EncryptionInfo {
            protocol: protocol_of(v),
            sender_device: sid,
            trust: Trust::Untrusted,
            error: Some(why),
        }),
    ))
}

/// Decrypt an OMEMO message that has already passed deduplication.
/// Returns `None` for empty (session-management) messages.
pub async fn decrypt_message(core: &Arc<Core>, handle: &SessionHandle, p: &Parsed) -> Option<(String, Option<EncryptionInfo>)> {
    // A message may carry both versions' <encrypted/>; collect them, OMEMO:2 first.
    let mut candidates: Vec<(Version, Encrypted)> = Vec::new();
    for v in VERSIONS {
        let ns = if v == Version::Omemo2 { NS_OMEMO2 } else { NS_LEGACY };
        for el in p.message.payloads.iter().filter(|el| el.is("encrypted", ns)) {
            match xml::parse_encrypted(v, el) {
                Some(enc) => candidates.push((v, enc)),
                None => return decrypt_error(v, 0, "malformed <encrypted/> element"),
            }
        }
    }
    let (first_v, first_sid) = candidates.first().map(|(v, e)| (*v, e.sid))?;
    let mgr = match manager(core) {
        Ok(m) => m,
        Err(e) => return decrypt_error(first_v, first_sid, format!("{e:#}")),
    };
    let own = handle.own.to_string();
    // Session name: the sender's real bare JID (ourselves, for messages from
    // our other devices; the occupant's real JID in rooms).
    let room = (p.kind == MessageKind::Groupchat).then(|| p.conversation.clone());
    let sender = match p.kind {
        MessageKind::Chat if p.own => own.clone(),
        MessageKind::Chat => p.sender.clone(),
        MessageKind::Groupchat => {
            let real = p
                .nick
                .as_deref()
                .and_then(|nick| core.room_real_jid(&p.conversation, nick))
                .or_else(|| real_jid_in_message(&p.message));
            match real {
                Some(j) => j,
                None => return decrypt_error(first_v, first_sid, "can't tell who sent this (anonymous room?)"),
            }
        }
        MessageKind::MucPrivate => {
            return decrypt_error(first_v, first_sid, "OMEMO in private room messages isn't supported");
        }
    };
    let chosen = candidates.iter().find_map(|(v, enc)| {
        let our_id = mgr.device_id(*v);
        enc.keys
            .iter()
            .find(|k| k.rid == our_id && (*v == Version::Legacy || k.jid.eq_ignore_ascii_case(&own)))
            .map(|k| (*v, enc, k))
    });
    let Some((v, enc, key)) = chosen else {
        // Empty OMEMO messages (session management, no payload) that aren't
        // for us — e.g. our own key-exchange answers replayed from the
        // archive — carry nothing to show.
        if candidates.iter().all(|(_, e)| e.payload.is_none()) {
            return None;
        }
        if p.own && candidates.iter().any(|(v, e)| e.sid == mgr.device_id(*v)) {
            return decrypt_error(first_v, first_sid, "sent from this device (not encrypted to itself)");
        }
        return decrypt_error(first_v, first_sid, "not encrypted for this device");
    };

    let decrypted = match mgr.with(v, |e| e.ctx.decrypt(&sender, enc.sid, &key.data, key.kex)) {
        Ok(d) => d,
        // Already processed (e.g. an empty session message replayed from the
        // archive after a restart): any content was stored the first time.
        Err(omemo::Error::DuplicateMessage) => return None,
        // Nothing to show for a failed empty (session-management) message.
        Err(e) if enc.payload.is_none() => {
            tracing::debug!("ignoring undecryptable empty OMEMO message from {sender}/{}: {e}", enc.sid);
            return None;
        }
        // XEP-0384 §8: never rebuild sessions automatically on errors.
        Err(e) => return decrypt_error(v, enc.sid, e.to_string()),
    };

    if decrypted.used_pre_key.is_some() {
        // Replace the consumed pre key and answer the key exchange.
        let core = core.clone();
        let handle = handle.clone();
        let sender = sender.clone();
        let sid = enc.sid;
        tokio::spawn(async move {
            let Ok(mgr) = manager(&core) else { return };
            if mgr.refresh_keys(v).unwrap_or(false)
                && let Err(e) = publish_bundle(&mgr, &handle, v).await
            {
                tracing::warn!("republishing bundle failed: {e:#}");
            }
            if let Err(e) = send_empty(&mgr, &handle, v, &sender, sid) {
                tracing::warn!("answering key exchange from {sender}/{sid} failed: {e:#}");
            }
        });
    }

    let Some(ciphertext) = enc.payload.as_ref() else {
        return None; // empty OMEMO message
    };
    let body = match v {
        Version::Omemo2 => {
            let plain = match payload::omemo2::open(&decrypted.plaintext, ciphertext) {
                Ok(b) => b,
                Err(e) => return decrypt_error(v, enc.sid, e.to_string()),
            };
            let Some(env) = xml::parse_envelope(&plain) else {
                return decrypt_error(v, enc.sid, "invalid SCE envelope");
            };
            // The envelope's <from/> must match the stanza's sender, and a
            // group message's <to/> must name this room (XEP-0384 §5.5.1).
            if let Some(from) = &env.from
                && !from.eq_ignore_ascii_case(&sender)
            {
                return decrypt_error(v, enc.sid, format!("envelope from {from} doesn't match sender {sender}"));
            }
            if let Some(room) = &room
                && !env.to.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(room))
            {
                return decrypt_error(v, enc.sid, "envelope isn't addressed to this room");
            }
            env.body.unwrap_or_default()
        }
        Version::Legacy => {
            let Some(iv) = enc.iv.as_ref() else {
                return decrypt_error(v, enc.sid, "missing IV");
            };
            match payload::legacy::open(&decrypted.plaintext, iv, ciphertext) {
                Ok(b) => String::from_utf8_lossy(&b).into_owned(),
                Err(e) => return decrypt_error(v, enc.sid, e.to_string()),
            }
        }
    };

    let trust = mgr
        .db
        .devices(v, &sender)
        .ok()
        .and_then(|rows| rows.into_iter().find(|d| d.device_id == enc.sid))
        .map(|d| mgr.db.effective_trust(&sender, d.trust).unwrap_or(Trust::Untrusted))
        .unwrap_or(Trust::Untrusted);
    Some((
        body,
        Some(EncryptionInfo {
            protocol: protocol_of(v),
            sender_device: enc.sid,
            trust,
            error: None,
        }),
    ))
}

/// Real JID from a `<x xmlns='muc#user'><item jid/></x>` in an (archived)
/// group message, as non-anonymous rooms may include.
fn real_jid_in_message(m: &Message) -> Option<String> {
    m.payloads
        .iter()
        .filter(|el| el.is("x", "http://jabber.org/protocol/muc#user"))
        .find_map(|x| x.get_child("item", "http://jabber.org/protocol/muc#user"))
        .and_then(|item| item.attr("jid"))
        .and_then(|j| Jid::new(j).ok())
        .map(|j| j.to_bare().to_string())
}

// ------------------------------------------------------- devices and trust

fn own_identity_public(mgr: &Manager, v: Version) -> Option<Vec<u8>> {
    mgr.db
        .conn()
        .query_row(
            "SELECT identity_public FROM omemo_own WHERE version = ?1",
            rusqlite::params![super::store::vkey(v)],
            |r| r.get(0),
        )
        .ok()
}

fn device_info(mgr: &Manager, v: Version, row: &super::store::DeviceRow, own: &str) -> DeviceInfo {
    let this = row.jid == own && row.device_id == mgr.device_id(v);
    let key = if this { own_identity_public(mgr, v) } else { row.identity_key.clone() };
    DeviceInfo {
        jid: row.jid.clone(),
        protocol: protocol_of(v),
        device_id: row.device_id,
        fingerprint: key.as_deref().and_then(|k| fingerprint::fingerprint(Version::Legacy, k)),
        trust: if this {
            Trust::Verified
        } else {
            mgr.db.effective_trust(&row.jid, row.trust).unwrap_or(Trust::Untrusted)
        },
        this_device: this,
    }
}

fn internal(e: impl std::fmt::Display) -> IpcError {
    IpcError::new(ErrorCode::Internal, e.to_string())
}

/// Devices of `jid` (our own when `None`), both versions.
pub async fn list_devices(core: &Arc<Core>, jid: Option<String>) -> Result<Vec<DeviceInfo>, IpcError> {
    let mgr = manager(core).map_err(internal)?;
    let own = core
        .status()
        .account
        .ok_or_else(|| IpcError::new(ErrorCode::NoAccount, "no account configured"))?;
    let target = match jid {
        Some(j) => BareJid::new(&j)
            .map_err(|e| IpcError::new(ErrorCode::BadRequest, format!("invalid JID: {e}")))?
            .to_string(),
        None => own.clone(),
    };
    // Learn identity keys of devices we have no session with yet.
    if let Ok(handle) = core.session()
        && let Ok(bare) = BareJid::new(&target)
    {
        refresh_device_lists(&mgr, &handle, &bare).await;
        for v in VERSIONS {
            let missing: Vec<u32> = mgr
                .db
                .devices(v, &target)
                .unwrap_or_default()
                .into_iter()
                .filter(|d| d.active && d.identity_key.is_none() && !(target == own && d.device_id == mgr.device_id(v)))
                .map(|d| d.device_id)
                .collect();
            for dev in missing {
                if let Ok(b) = tokio::time::timeout(Duration::from_secs(10), fetch_bundle(&handle, &target, dev, v)).await
                    && let Ok(b) = b
                    && let Some(curve) = fingerprint::curve25519_bytes(v, &b.identity_key)
                {
                    let mut key = vec![0x05];
                    key.extend_from_slice(&curve);
                    let _ = mgr.db.record_identity(v, &target, dev, &key);
                }
            }
        }
    }
    let mut out = Vec::new();
    for v in VERSIONS {
        for row in mgr.db.devices(v, &target).map_err(internal)? {
            if row.active {
                out.push(device_info(&mgr, v, &row, &own));
            }
        }
        if target == own && !out.iter().any(|d| d.this_device && d.protocol == protocol_of(v)) {
            // Our own device before the first publish round-trip.
            let public = own_identity_public(&mgr, v);
            out.push(DeviceInfo {
                jid: own.clone(),
                protocol: protocol_of(v),
                device_id: mgr.device_id(v),
                fingerprint: public.as_deref().and_then(|k| fingerprint::fingerprint(Version::Legacy, k)),
                trust: Trust::Verified,
                this_device: true,
            });
        }
    }
    Ok(out)
}

/// Mark the device(s) of `jid` with this fingerprint as verified or untrusted.
pub async fn set_trust(core: &Arc<Core>, jid: &str, fingerprint_input: &str, trusted: bool) -> Result<DeviceInfo, IpcError> {
    let wanted = fingerprint::normalize(fingerprint_input);
    if wanted.len() != 64 {
        return Err(IpcError::new(ErrorCode::BadRequest, "fingerprints are 64 hex digits"));
    }
    let devices = list_devices(core, Some(jid.to_owned())).await?;
    let mgr = manager(core).map_err(internal)?;
    let own = core.status().account.unwrap_or_default();
    let mut updated = None;
    for d in devices {
        if d.this_device || d.fingerprint.as_deref() != Some(wanted.as_str()) {
            continue;
        }
        let v = version_of(d.protocol);
        mgr.db
            .set_trust(v, &d.jid, d.device_id, if trusted { Trust::Verified } else { Trust::Untrusted })
            .map_err(internal)?;
        let row = mgr
            .db
            .devices(v, &d.jid)
            .map_err(internal)?
            .into_iter()
            .find(|r| r.device_id == d.device_id)
            .ok_or_else(|| internal("device vanished"))?;
        updated = Some(device_info(&mgr, v, &row, &own));
        core.emit(Event::DevicesChanged {
            jid: d.jid.clone(),
            protocol: d.protocol,
        });
    }
    updated.ok_or_else(|| {
        IpcError::new(
            ErrorCode::NotFound,
            format!("no device of {jid} has fingerprint {}", fingerprint::display(&wanted)),
        )
    })
}
