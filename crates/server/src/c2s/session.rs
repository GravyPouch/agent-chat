//! The session loop after binding (§5.4 step 9): inbound stanzas are
//! checked, stamped and routed; deliveries from the account's actor are
//! written out; the hub's shutdown and kicks close the stream.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::xmlstream::ReadError;
use xmpp_parsers::jid::Jid;
use xmpp_parsers::ns;
use xmpp_parsers::stream_error::DefinedCondition as StreamCondition;

use super::negotiate::Bound;
use super::{close, close_with_error};
use crate::hub::HubCtx;
use crate::stanza::{self, Condition, Kind, NS_CLIENT, NS_SESSION};
use crate::stats::Stats;
use crate::user::UserMsg;

/// A peer that can't take our writes for this long is disconnected.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

static KEEPALIVE_ID: AtomicU64 = AtomicU64::new(1);

pub async fn run(ctx: &Arc<HubCtx>, b: Bound) {
    let Bound {
        mut stream,
        budget,
        jid,
        conn_id,
        mut outbox,
        kick,
    } = b;
    let bare = jid.to_bare();
    let full = jid.to_string();
    let resource = jid.resource().as_str().to_owned();
    Stats::inc(&ctx.stats.sessions);
    tracing::info!(jid = %full, "session bound");

    let reason: &str = loop {
        tokio::select! {
            _ = ctx.shutdown.cancelled() => {
                close_with_error(&mut stream, StreamCondition::SystemShutdown, "The hub is shutting down").await;
                break "shutdown";
            }
            _ = kick.token().cancelled() => {
                let r = kick.reason().unwrap_or(crate::user::CloseReason::AccountDeleted);
                close_with_error(&mut stream, r.condition(), r.text()).await;
                break "closed by the server";
            }
            Some(el) = outbox.recv() => {
                match tokio::time::timeout(WRITE_TIMEOUT, stream.send(&el)).await {
                    Ok(Ok(())) => Stats::inc(&ctx.stats.stanzas_out),
                    Ok(Err(_)) => break "write failed",
                    Err(_) => break "write timed out",
                }
            }
            item = stream.next() => match item {
                Some(Ok(el)) => {
                    budget.reset();
                    Stats::inc(&ctx.stats.stanzas_in);
                    match inbound(ctx, &jid, el) {
                        Inbound::Route(el) => {
                            let msg = UserMsg::Outbound { resource: resource.clone(), el };
                            if let Err(UserMsg::Outbound { el, .. }) = ctx.router.send_to_actor(&bare, msg) {
                                tracing::warn!("own account mailbox full; bouncing");
                                if let Some(e) = stanza::error_reply(&el, Condition::ResourceConstraint, Some(&ctx.domain))
                                    && stream.send(&e).await.is_err()
                                {
                                    break "write failed";
                                }
                            }
                        }
                        Inbound::Reply(el) => {
                            if stream.send(&el).await.is_err() {
                                break "write failed";
                            }
                        }
                        Inbound::Ignore => {}
                        Inbound::StreamError(cond, text) => {
                            close_with_error(&mut stream, cond, text).await;
                            break text;
                        }
                    }
                }
                Some(Err(ReadError::SoftTimeout)) => {
                    // Silence: ask the client for a sign of life (XEP-0199).
                    let id = format!("keepalive-{}", KEEPALIVE_ID.fetch_add(1, Ordering::Relaxed));
                    let mut ping = stanza::element("iq", NS_CLIENT, &[("type", "get"), ("id", &id)]);
                    stanza::set_attr(&mut ping, "from", ctx.domain.as_str());
                    stanza::set_attr(&mut ping, "to", full.as_str());
                    ping.append_child(Element::bare("ping", ns::PING));
                    if stream.send(&ping).await.is_err() {
                        break "write failed";
                    }
                }
                Some(Err(ReadError::HardError(e))) => {
                    if budget.exceeded() {
                        tracing::info!(limit = budget.limit(), "stanza too large; closing");
                        Stats::inc(&ctx.stats.limit_hits);
                        close_with_error(&mut stream, StreamCondition::PolicyViolation, "Stanza too large").await;
                        break "stanza too large";
                    }
                    if e.kind() == std::io::ErrorKind::InvalidData {
                        close_with_error(&mut stream, StreamCondition::NotWellFormed, "Malformed XML").await;
                        break "malformed XML";
                    }
                    break "connection lost";
                }
                Some(Err(ReadError::ParseError(_))) => {
                    close_with_error(&mut stream, StreamCondition::BadFormat, "Unparsable element").await;
                    break "unparsable element";
                }
                Some(Err(ReadError::StreamFooterReceived)) | None => {
                    close(&mut stream).await;
                    break "closed by the client";
                }
            }
        }
    };

    let _ = ctx.router.send_to_actor(
        &bare,
        UserMsg::Unbind {
            resource: jid.resource().as_str().to_owned(),
            conn_id,
        },
    );
    Stats::dec(&ctx.stats.sessions);
    tracing::info!(jid = %full, reason, "session ended");
}

enum Inbound {
    Route(Element),
    Reply(Element),
    Ignore,
    StreamError(StreamCondition, &'static str),
}

/// Checks on every inbound element: only stanzas, `from` must be ours,
/// `from` gets stamped, `to` must parse, spoofed stanza-ids are removed.
fn inbound(ctx: &HubCtx, jid: &xmpp_parsers::jid::FullJid, mut el: Element) -> Inbound {
    let Some(kind) = Kind::of(&el) else {
        // Nonzas we don't offer (stream management is M3) or foreign elements.
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
