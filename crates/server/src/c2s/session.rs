//! The session loop after binding (§5.4 step 9): inbound stanzas are
//! checked, stamped and handed to the account's actor; deliveries from the
//! actor are written out; the hub's shutdown and kicks close the stream.
//!
//! With XEP-0198 the session outlives its connection: a lost connection
//! makes a resumable session hibernate until the client resumes or the
//! timeout passes (§5.4.1). Then the session ends for good: the actor gets
//! `Unbind`, and unacknowledged messages go back to it for offline storage.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::xmlstream::ReadError;
use xmpp_parsers::jid::{FullJid, Jid};
use xmpp_parsers::ns;
use xmpp_parsers::stream_error::{DefinedCondition as StreamCondition, StreamError};

use super::negotiate::Bound;
use super::sm::{self, Conn, Handle, NS_CSI, NS_SM, ResumeRequest, SmState};
use super::{close, close_with_error};
use crate::hub::HubCtx;
use crate::stanza::{self, Condition, Kind, NS_CLIENT, NS_SESSION};
use crate::stats::Stats;
use crate::user::{Kick, UserMsg};
use crate::util::random_id;

/// A peer that can't take our writes for this long is disconnected.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Ask for an acknowledgement after this many unacknowledged stanzas…
const ACK_EVERY: usize = 5;
/// …or after this long.
const ACK_INTERVAL: Duration = Duration::from_secs(30);

static KEEPALIVE_ID: AtomicU64 = AtomicU64::new(1);

/// How one connection's part of a session ended.
enum End {
    /// The session is over (client closed, error, kick, shutdown).
    Closed(&'static str),
    /// The connection broke; a resumable session may hibernate.
    Lost(&'static str),
    /// The client resumed on another connection.
    Resume(Box<ResumeRequest>),
}

struct Ctx<'a> {
    hub: &'a Arc<HubCtx>,
    jid: &'a FullJid,
    resource: String,
    kick: &'a Kick,
    resume_tx: mpsc::Sender<ResumeRequest>,
}

pub async fn run(hub: &Arc<HubCtx>, b: Box<Bound>) {
    let Bound {
        conn,
        jid,
        conn_id,
        mut outbox,
        kick,
    } = *b;
    let bare = jid.to_bare();
    let full = jid.to_string();
    Stats::inc(&hub.stats.sessions);
    tracing::info!(jid = %full, "session bound");
    let (resume_tx, mut resume_rx) = mpsc::channel::<ResumeRequest>(2);
    let cx = Ctx {
        hub,
        jid: &jid,
        resource: jid.resource().as_str().to_owned(),
        kick: &kick,
        resume_tx,
    };
    let mut sm = SmState::default();
    let mut conn = Some(conn);

    let reason: &str = loop {
        let Some(c) = conn.as_mut() else { break "no connection" };
        match connected(&cx, c, &mut sm, &mut outbox, &mut resume_rx).await {
            End::Closed(r) => break r,
            End::Resume(req) => {
                // The client came back before we noticed the old connection died.
                conn = Some(req.conn);
                if !resume(&cx, conn.as_mut().unwrap(), &mut sm, req.h).await {
                    break "resumption failed";
                }
            }
            End::Lost(r) => {
                if sm.id.is_none() {
                    break r;
                }
                tracing::info!(jid = %full, reason = r, "connection lost; session hibernating");
                drop(conn.take());
                match hibernate(&cx, &mut sm, &mut outbox, &mut resume_rx).await {
                    Ok(req) => {
                        conn = Some(req.conn);
                        if !resume(&cx, conn.as_mut().unwrap(), &mut sm, req.h).await {
                            break "resumption failed";
                        }
                    }
                    Err(r) => break r,
                }
            }
        }
    };

    if let Some(id) = &sm.id {
        hub.sm.remove(id);
    }
    let _ = hub.router.send_to_actor(
        &bare,
        UserMsg::Unbind {
            resource: cx.resource.clone(),
            conn_id,
        },
    );
    // Stanzas the client never acknowledged: messages go back to the
    // account (offline storage); IQ requests get an error.
    for el in sm.take_unacked() {
        match Kind::of(&el) {
            Some(Kind::Message) => {
                let _ = hub.router.send_to_actor(&bare, UserMsg::Requeue(el));
            }
            Some(Kind::Iq) if matches!(stanza::stanza_type(&el), "get" | "set") => {
                hub.router.bounce(&el, Condition::ServiceUnavailable, Some(&full));
            }
            _ => {}
        }
    }
    Stats::dec(&hub.stats.sessions);
    tracing::info!(jid = %full, reason, "session ended");
}

/// Write a stanza, counting it for stream management first (so a failed
/// write is still retransmitted on resumption).
async fn write(conn: &mut Conn, sm: &mut SmState, el: &Element, stats: &Stats) -> bool {
    if Kind::of(el).is_some() {
        sm.on_sent(el);
    }
    match tokio::time::timeout(WRITE_TIMEOUT, conn.stream.send(el)).await {
        Ok(Ok(())) => {
            Stats::inc(&stats.stanzas_out);
            true
        }
        _ => false,
    }
}

/// Run on one connection until it ends.
async fn connected(
    cx: &Ctx<'_>,
    conn: &mut Conn,
    sm: &mut SmState,
    outbox: &mut mpsc::Receiver<Element>,
    resume_rx: &mut mpsc::Receiver<ResumeRequest>,
) -> End {
    let hub = cx.hub;
    let max_queue = hub.config.sm.max_queue;
    let mut ack_timer = tokio::time::interval_at(Instant::now() + ACK_INTERVAL, ACK_INTERVAL);
    loop {
        tokio::select! {
            _ = hub.shutdown.cancelled() => {
                close_with_error(&mut conn.stream, StreamCondition::SystemShutdown, "The hub is shutting down").await;
                return End::Closed("shutdown");
            }
            _ = cx.kick.token().cancelled() => {
                let r = cx.kick.reason().unwrap_or(crate::user::CloseReason::AccountDeleted);
                close_with_error(&mut conn.stream, r.condition(), r.text()).await;
                return End::Closed("closed by the server");
            }
            Some(req) = resume_rx.recv() => return End::Resume(Box::new(req)),
            Some(el) = outbox.recv() => {
                if !write(conn, sm, &el, &hub.stats).await {
                    return End::Lost("write failed");
                }
                if sm.unacked() > max_queue {
                    tracing::info!("too many unacknowledged stanzas; closing");
                    Stats::inc(&hub.stats.limit_hits);
                    close_with_error(&mut conn.stream, StreamCondition::ResourceConstraint, "Too many unacknowledged stanzas").await;
                    return End::Closed("unacknowledged queue full");
                }
                if sm.enabled && !sm.r_pending && sm.unacked() >= ACK_EVERY {
                    sm.r_pending = true;
                    if conn.stream.send(&sm::request()).await.is_err() {
                        return End::Lost("write failed");
                    }
                }
            }
            _ = ack_timer.tick() => {
                if sm.enabled && !sm.r_pending && sm.unacked() > 0 {
                    sm.r_pending = true;
                    if conn.stream.send(&sm::request()).await.is_err() {
                        return End::Lost("write failed");
                    }
                }
            }
            item = conn.stream.next() => match item {
                Some(Ok(el)) => {
                    conn.budget.reset();
                    Stats::inc(&hub.stats.stanzas_in);
                    if el.ns() == NS_SM {
                        if let Some(end) = sm_nonza(cx, conn, sm, &el).await {
                            return end;
                        }
                        continue;
                    }
                    if el.ns() == NS_CSI {
                        // XEP-0352: accepted; the hub doesn't hold anything back yet.
                        continue;
                    }
                    if Kind::of(&el).is_some() {
                        sm.on_received();
                    }
                    match inbound(hub, cx.jid, el) {
                        Inbound::Route(el) => {
                            let msg = UserMsg::Outbound { resource: cx.resource.clone(), el };
                            if let Err(UserMsg::Outbound { el, .. }) = hub.router.send_to_actor(&cx.jid.to_bare(), msg) {
                                tracing::warn!("own account mailbox full; bouncing");
                                if let Some(e) = stanza::error_reply(&el, Condition::ResourceConstraint, Some(&hub.domain))
                                    && !write(conn, sm, &e, &hub.stats).await
                                {
                                    return End::Lost("write failed");
                                }
                            }
                        }
                        Inbound::Reply(el) => {
                            if !write(conn, sm, &el, &hub.stats).await {
                                return End::Lost("write failed");
                            }
                        }
                        Inbound::Ignore => {}
                        Inbound::StreamError(cond, text) => {
                            close_with_error(&mut conn.stream, cond, text).await;
                            return End::Closed(text);
                        }
                    }
                }
                Some(Err(ReadError::SoftTimeout)) => {
                    // Silence: ask the client for a sign of life.
                    let ok = if sm.enabled {
                        sm.r_pending = true;
                        conn.stream.send(&sm::request()).await.is_ok()
                    } else {
                        let id = format!("keepalive-{}", KEEPALIVE_ID.fetch_add(1, Ordering::Relaxed));
                        let mut ping = stanza::element("iq", NS_CLIENT, &[("type", "get"), ("id", &id)]);
                        stanza::set_attr(&mut ping, "from", hub.domain.as_str());
                        stanza::set_attr(&mut ping, "to", cx.jid.to_string());
                        ping.append_child(Element::bare("ping", ns::PING));
                        write(conn, sm, &ping, &hub.stats).await
                    };
                    if !ok {
                        return End::Lost("write failed");
                    }
                }
                Some(Err(ReadError::HardError(e))) => {
                    if conn.budget.exceeded() {
                        tracing::info!(limit = conn.budget.limit(), "stanza too large; closing");
                        Stats::inc(&hub.stats.limit_hits);
                        close_with_error(&mut conn.stream, StreamCondition::PolicyViolation, "Stanza too large").await;
                        return End::Closed("stanza too large");
                    }
                    if sm::is_disconnect(&e) {
                        return End::Lost("connection lost");
                    }
                    if e.kind() == std::io::ErrorKind::InvalidData {
                        close_with_error(&mut conn.stream, StreamCondition::NotWellFormed, "Malformed XML").await;
                        return End::Closed("malformed XML");
                    }
                    return End::Lost("connection lost");
                }
                Some(Err(ReadError::ParseError(_))) => {
                    close_with_error(&mut conn.stream, StreamCondition::BadFormat, "Unparsable element").await;
                    return End::Closed("unparsable element");
                }
                Some(Err(ReadError::StreamFooterReceived)) => {
                    close(&mut conn.stream).await;
                    return End::Closed("closed by the client");
                }
                None => return End::Lost("connection lost"),
            }
        }
    }
}

/// `<enable/>`, `<r/>`, `<a/>` after binding. `Some` ends the connection.
async fn sm_nonza(cx: &Ctx<'_>, conn: &mut Conn, sm: &mut SmState, el: &Element) -> Option<End> {
    let hub = cx.hub;
    let sent = match el.name() {
        "enable" if !sm.enabled => {
            sm.enabled = true;
            let mut enabled = Element::bare("enabled", NS_SM);
            if matches!(el.attr("resume"), Some("true" | "1")) {
                let id = random_id();
                hub.sm.insert(
                    id.clone(),
                    Handle {
                        bare: cx.jid.to_bare(),
                        tx: cx.resume_tx.clone(),
                    },
                );
                stanza::set_attr(&mut enabled, "id", id.as_str());
                stanza::set_attr(&mut enabled, "resume", "true");
                let max = hub.config.sm.resume_timeout.0.as_secs().max(1);
                stanza::set_attr(&mut enabled, "max", max.to_string());
                sm.id = Some(id);
            }
            conn.stream.send(&enabled).await
        }
        "enable" | "resume" => conn.stream.send(&sm::failed("unexpected-request", sm.inbound)).await,
        "r" if sm.enabled => conn.stream.send(&sm::ack(sm.inbound)).await,
        "a" if sm.enabled => {
            let Some(h) = el.attr("h").and_then(|h| h.parse::<u32>().ok()) else {
                close_with_error(&mut conn.stream, StreamCondition::BadFormat, "Bad <a/>").await;
                return Some(End::Closed("bad ack"));
            };
            sm.r_pending = false;
            if let Err(e) = sm.ack(h) {
                tracing::info!(h = e.h, sent = e.sent, "client acknowledged more than we sent");
                let err: StreamError = xmpp_parsers::sm::HandledCountTooHigh {
                    h: e.h,
                    send_count: e.sent,
                }
                .into();
                let _ = tokio::time::timeout(Duration::from_secs(5), async {
                    if conn.stream.send(&err).await.is_ok() {
                        let _ = SinkExt::<&Element>::close(&mut conn.stream).await;
                    }
                })
                .await;
                return Some(End::Closed("handled-count-too-high"));
            }
            Ok(())
        }
        _ => Ok(()),
    };
    match sent {
        Ok(()) => None,
        Err(_) => Some(End::Lost("write failed")),
    }
}

/// Continue the session on `conn` after `<resume h/>` (XEP-0198 §5).
async fn resume(cx: &Ctx<'_>, conn: &mut Conn, sm: &mut SmState, h: u32) -> bool {
    let hub = cx.hub;
    if let Err(e) = sm.ack(h) {
        let err: StreamError = xmpp_parsers::sm::HandledCountTooHigh {
            h: e.h,
            send_count: e.sent,
        }
        .into();
        let _ = tokio::time::timeout(Duration::from_secs(5), conn.stream.send(&err)).await;
        return false;
    }
    let Some(id) = sm.id.clone() else { return false };
    let resumed = stanza::element("resumed", NS_SM, &[("h", &sm.inbound.to_string()), ("previd", &id)]);
    if conn.stream.send(&resumed).await.is_err() {
        return false;
    }
    let pending: Vec<Element> = sm.pending().cloned().collect();
    for el in &pending {
        if tokio::time::timeout(WRITE_TIMEOUT, conn.stream.send(el))
            .await
            .map(|r| r.is_ok())
            != Ok(true)
        {
            return false;
        }
    }
    sm.r_pending = false;
    Stats::inc(&hub.stats.sm_resumes);
    tracing::info!(jid = %cx.jid, retransmitted = pending.len(), "session resumed");
    true
}

/// Wait for a resumption, queueing what the actor sends meanwhile.
async fn hibernate(
    cx: &Ctx<'_>,
    sm: &mut SmState,
    outbox: &mut mpsc::Receiver<Element>,
    resume_rx: &mut mpsc::Receiver<ResumeRequest>,
) -> Result<ResumeRequest, &'static str> {
    let hub = cx.hub;
    Stats::inc(&hub.stats.hibernating);
    let deadline = Instant::now() + hub.config.sm.resume_timeout.0;
    let r = loop {
        tokio::select! {
            _ = hub.shutdown.cancelled() => break Err("shutdown"),
            _ = cx.kick.token().cancelled() => break Err("closed by the server"),
            _ = tokio::time::sleep_until(deadline) => break Err("resumption timed out"),
            Some(req) = resume_rx.recv() => break Ok(req),
            Some(el) = outbox.recv() => {
                sm.on_sent(&el);
                if sm.unacked() > hub.config.sm.max_queue {
                    break Err("unacknowledged queue full");
                }
            }
        }
    };
    Stats::dec(&hub.stats.hibernating);
    r
}

enum Inbound {
    Route(Element),
    Reply(Element),
    Ignore,
    StreamError(StreamCondition, &'static str),
}

/// Checks on every inbound element: only stanzas, `from` must be ours,
/// `from` gets stamped, `to` must parse, spoofed stanza-ids are removed.
fn inbound(ctx: &HubCtx, jid: &FullJid, mut el: Element) -> Inbound {
    let Some(kind) = Kind::of(&el) else {
        return Inbound::StreamError(StreamCondition::UnsupportedStanzaType, "Unsupported element");
    };
    if let Some(from) = el.attr("from") {
        let ok = Jid::new(from).is_ok_and(|f| f == *jid || f == jid.to_bare());
        if !ok {
            return Inbound::StreamError(StreamCondition::InvalidFrom, "The from address is not yours");
        }
    }
    stanza::set_attr(&mut el, "from", jid.to_string());
    if let Some(to) = el.attr("to")
        && Jid::new(to).is_err()
    {
        return match stanza::error_reply(&el, Condition::JidMalformed, Some(&ctx.domain)) {
            Some(e) => Inbound::Reply(e),
            None => Inbound::Ignore,
        };
    }
    if kind == Kind::Iq {
        let ty = stanza::stanza_type(&el);
        let to_server = el.attr("to").is_none_or(|t| t == ctx.domain);
        // The legacy session IQ (RFC 3921) is a no-op.
        if ty == "set" && to_server && el.has_child("session", NS_SESSION) {
            return Inbound::Reply(stanza::iq_result(&el, Some(&ctx.domain), None));
        }
        // One resource per stream.
        if ty == "set" && to_server && el.has_child("bind", ns::BIND) {
            return match stanza::error_reply(&el, Condition::NotAllowed, Some(&ctx.domain)) {
                Some(e) => Inbound::Reply(e),
                None => Inbound::Ignore,
            };
        }
    }
    if kind == Kind::Message {
        let removed = stanza::strip_our_stanza_ids(&mut el, |by| ctx.router.is_our_jid(by));
        if removed > 0 {
            tracing::debug!(removed, "stripped client-supplied stanza-ids");
        }
    }
    Inbound::Route(el)
}
