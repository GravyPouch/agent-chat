//! Stream negotiation: header checks, STARTTLS, SASL and resource binding
//! (§5.4 steps 2–8), on tokio-xmpp's responder API.

use std::borrow::Cow;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::{SinkExt, StreamExt};
use subtle::ConstantTimeEq;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::xmlstream::{AcceptedStream, PendingFeaturesSend, ReadError, StreamHeader, accept_stream};
use xmpp_parsers::bind::BindFeature;
use xmpp_parsers::jid::{BareJid, FullJid, Jid, ResourcePart};
use xmpp_parsers::ns;
use xmpp_parsers::sasl::{Challenge, Success};
use xmpp_parsers::sasl_cb::{SaslChannelBinding, Type as CbType};
use xmpp_parsers::sm::StreamManagement;
use xmpp_parsers::starttls::{Proceed, StartTls};
use xmpp_parsers::stream_error::{DefinedCondition as StreamCondition, StreamError};
use xmpp_parsers::stream_features::StreamFeatures;

use super::sasl::scram::{self, CbFlag, Exchange, ScramError};
use super::sasl::{self, Channel, Failure, Mechanism, plain};
use super::sm::{self, Conn, NS_CSI, NS_SM, ResumeRequest};
use super::{Io, Stream, close, close_with_error, timeouts};
use crate::accounts::normalize_username;
use crate::hub::HubCtx;
use crate::net::io::{Budget, ConnIo, RateLimit, Transport};
use crate::net::listener::AdmissionGuard;
use crate::net::tls;
use crate::register::{NS_IBR_TOKEN, NS_PARS, NS_REGISTER, RegError, Registrar};
use crate::stanza::{self, Condition, Kind, NS_CLIENT, NS_SESSION};
use crate::stats::Stats;
use crate::store::{AccountState, ScramKeys};
use crate::user::{BindError, Kick, SessionLink, UserMsg};
use crate::util::{ip_prefix, is_loopback, random_bytes, random_id};

/// Session outbox size (§5.3).
pub const OUTBOX: usize = 512;
/// SASL attempts per connection (§5.4 step 6).
const MAX_SASL_FAILURES: u32 = 3;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A connection that finished negotiation and bound a resource.
pub struct Bound {
    pub conn: Conn,
    pub jid: FullJid,
    pub conn_id: u64,
    pub outbox: mpsc::Receiver<Element>,
    pub kick: Arc<Kick>,
}

/// How negotiation ended.
pub enum Negotiated {
    /// A new session.
    Bound(Box<Bound>),
    /// The connection was handed to a hibernating session (XEP-0198).
    Resumed,
}

struct Authenticated {
    localpart: String,
}

/// What we know about the channel so far.
struct ChannelState {
    ip: IpAddr,
    loopback: bool,
    /// A valid XEP-0445 token from `<preauth/>` on this stream.
    invite: Option<String>,
    /// The peer's network prefix (/24 or /56), for throttling and logs.
    prefix: String,
    tls: bool,
    /// `tls-exporter` value (TLS 1.3 only).
    cb_data: Option<Vec<u8>>,
}

enum Next {
    El(Element),
    /// The stream ended or failed; already closed or reported.
    End,
}

/// Read the next top-level element, handling errors the same way in every
/// stage: budget overruns get `policy-violation`, broken XML `not-well-formed`.
async fn next(stream: &mut Stream, budget: &Budget, ctx: &HubCtx) -> Next {
    loop {
        match stream.next().await {
            Some(Ok(el)) => {
                budget.reset();
                Stats::inc(&ctx.stats.stanzas_in);
                return Next::El(el);
            }
            // The negotiation deadline bounds silence before binding.
            Some(Err(ReadError::SoftTimeout)) => continue,
            Some(Err(ReadError::HardError(e))) => {
                if budget.exceeded() {
                    tracing::info!(limit = budget.limit(), "stanza too large; closing");
                    Stats::inc(&ctx.stats.limit_hits);
                    close_with_error(stream, StreamCondition::PolicyViolation, "Stanza too large").await;
                } else if e.kind() == std::io::ErrorKind::InvalidData {
                    close_with_error(stream, StreamCondition::NotWellFormed, "Malformed XML").await;
                } else {
                    tracing::debug!("read failed: {e}");
                }
                return Next::End;
            }
            Some(Err(ReadError::ParseError(e))) => {
                tracing::debug!("parse error: {e}");
                close_with_error(stream, StreamCondition::BadFormat, "Unparsable element").await;
                return Next::End;
            }
            Some(Err(ReadError::StreamFooterReceived)) | None => {
                close(stream).await;
                return Next::End;
            }
        }
    }
}

/// Run stages A–C. `Ok(None)` means the connection ended (already handled).
pub async fn negotiate(
    ctx: &Arc<HubCtx>,
    tcp: TcpStream,
    peer: IpAddr,
    conn_id: u64,
    guard: AdmissionGuard,
) -> Result<Option<Negotiated>> {
    let loopback = is_loopback(peer);
    let limits = &ctx.config.limits;
    let budget = Budget::new(limits.unauth_stanza_bytes.0 as usize);
    let rate = (!loopback).then(|| RateLimit::new(limits.session_rate.0, limits.session_burst.0));
    let io: Io = ConnIo::new(Transport::Plain(tcp), budget.clone(), rate);
    let mut accepted = accept_stream(io, NS_CLIENT, timeouts())
        .await
        .context("reading the stream header")?;
    let mut ch = ChannelState {
        ip: peer,
        loopback,
        invite: None,
        prefix: ip_prefix(peer),
        tls: false,
        cb_data: None,
    };
    let mut auth: Option<Authenticated> = None;
    let mut failures = 0u32;

    'stream: loop {
        budget.reset();
        let Some(pending) = respond_to_header(ctx, accepted).await? else {
            return Ok(None);
        };
        let Some(authed) = &auth else {
            // Stages A (before TLS) and B (TLS, not authenticated).
            let channel = Channel {
                tls: ch.tls,
                channel_binding: ch.cb_data.is_some(),
                plaintext_scram: ch.loopback && ctx.config.listen.allow_plaintext_loopback,
            };
            let offered = sasl::offered(&ctx.config.auth.mechanisms, channel);
            let features = pre_auth_features(ctx, &ch, &offered);
            budget.arm_features_filter();
            let mut stream: Stream = pending.send_features::<Element>(&features).await?;
            loop {
                let Next::El(el) = next(&mut stream, &budget, ctx).await else {
                    return Ok(None);
                };
                if el.is("starttls", ns::TLS) {
                    if ch.tls {
                        close_with_error(&mut stream, StreamCondition::PolicyViolation, "TLS is already on").await;
                        return Ok(None);
                    }
                    let material = ctx.tls.current();
                    stream.send(&Proceed).await?;
                    let io = stream.into_inner();
                    // Bytes pipelined after <starttls/> would be read as if
                    // they came over TLS: the STARTTLS injection class.
                    if io.buffered() > 0 {
                        tracing::warn!("data sent after <starttls/> before the handshake; closing");
                        Stats::inc(&ctx.stats.limit_hits);
                        return Ok(None);
                    }
                    let (transport, rate) = io.into_parts();
                    let Transport::Plain(tcp) = transport else {
                        bail!("STARTTLS on a TLS stream");
                    };
                    let tls_stream = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, material.acceptor.accept(tcp))
                        .await
                        .context("TLS handshake timed out")?
                        .context("TLS handshake failed")?;
                    let (_, conn) = tls_stream.get_ref();
                    ch.cb_data = tls::channel_binding(conn);
                    tracing::debug!(
                        version = ?conn.protocol_version(),
                        channel_binding = ch.cb_data.is_some(),
                        "TLS established"
                    );
                    ch.tls = true;
                    let io = ConnIo::new(Transport::Tls(Box::new(tls_stream)), budget.clone(), rate);
                    accepted = accept_stream(io, NS_CLIENT, timeouts()).await?;
                    continue 'stream;
                }
                if el.is("auth", ns::SASL) {
                    match authenticate(ctx, &mut stream, &budget, &el, &ch, channel, &offered).await? {
                        SaslOutcome::Success { localpart, additional } => {
                            accepted = stream.accept_reset(&Success { data: additional }).await?;
                            budget.set_limit(ctx.config.limits.max_stanza_bytes.0 as usize);
                            auth = Some(Authenticated { localpart });
                            continue 'stream;
                        }
                        SaslOutcome::Failure(f) => {
                            let failure = xmpp_parsers::sasl::Failure {
                                defined_condition: f.condition(),
                                texts: Default::default(),
                            };
                            stream.send(&failure).await?;
                            failures += 1;
                            if failures >= MAX_SASL_FAILURES {
                                close_with_error(
                                    &mut stream,
                                    StreamCondition::PolicyViolation,
                                    "Too many authentication failures",
                                )
                                .await;
                                return Ok(None);
                            }
                            continue;
                        }
                        SaslOutcome::End => return Ok(None),
                    }
                }
                // In-band registration (XEP-0077) and invitations (XEP-0445).
                if let Some(reply) = registration_iq(ctx, &el, &mut ch).await {
                    stream.send(&reply).await?;
                    continue;
                }
                close_with_error(&mut stream, StreamCondition::NotAuthorized, "Authenticate first").await;
                return Ok(None);
            }
        };

        // Stage C: authenticated, before bind.
        let mut features = StreamFeatures {
            bind: Some(BindFeature { required: false }),
            stream_management: Some(StreamManagement { optional: false }),
            ..Default::default()
        };
        features.others.push(Element::bare("csi", NS_CSI));
        let mut session = Element::bare("session", NS_SESSION);
        session.append_child(Element::bare("optional", NS_SESSION));
        features.others.push(session);
        budget.arm_features_filter();
        let mut stream: Stream = pending.send_features::<Element>(&features).await?;
        let bare = BareJid::new(&format!("{}@{}", authed.localpart, ctx.domain)).context("account JID")?;
        loop {
            let Next::El(el) = next(&mut stream, &budget, ctx).await else {
                return Ok(None);
            };
            if el.is("resume", NS_SM) {
                // XEP-0198 §5: continue a hibernating session instead of binding.
                let h = el.attr("h").and_then(|h| h.parse::<u32>().ok());
                let handle = el
                    .attr("previd")
                    .and_then(|id| ctx.sm.get(id))
                    .filter(|hd| hd.bare == bare);
                if let (Some(h), Some(hd)) = (h, handle) {
                    let conn = Conn { stream, budget, guard };
                    // If the session ended in between, the connection just closes.
                    let _ = hd.tx.try_send(ResumeRequest { conn, h });
                    return Ok(Some(Negotiated::Resumed));
                }
                stream.send(&sm::failed("item-not-found", 0)).await?;
                continue;
            }
            if el.is("enable", NS_SM) {
                stream.send(&sm::failed("unexpected-request", 0)).await?;
                continue;
            }
            if el.ns() == NS_CSI {
                continue;
            }
            if Kind::of(&el) != Some(Kind::Iq) {
                close_with_error(&mut stream, StreamCondition::NotAuthorized, "Bind a resource first").await;
                return Ok(None);
            }
            if stanza::stanza_type(&el) == "set" && el.has_child("session", NS_SESSION) {
                stream.send(&stanza::iq_result(&el, Some(&ctx.domain), None)).await?;
                continue;
            }
            let Some(bind) = el
                .get_child("bind", ns::BIND)
                .filter(|_| stanza::stanza_type(&el) == "set")
            else {
                if let Some(e) = stanza::error_reply(&el, Condition::NotAllowed, Some(&ctx.domain)) {
                    stream.send(&e).await?;
                }
                continue;
            };
            let requested = bind
                .get_child("resource", ns::BIND)
                .map(|r| r.text())
                .filter(|r| !r.trim().is_empty());
            let resource = match requested.as_deref().map(ResourcePart::new) {
                None => None,
                Some(Ok(r)) => Some(r.into_owned().to_string()),
                Some(Err(_)) => {
                    if let Some(e) = stanza::error_reply(&el, Condition::BadRequest, Some(&ctx.domain)) {
                        stream.send(&e).await?;
                    }
                    continue;
                }
            };
            match bind_resource(ctx, &bare, resource, conn_id).await {
                Ok((jid, outbox, kick)) => {
                    let mut response = Element::bare("bind", ns::BIND);
                    let mut j = Element::bare("jid", ns::BIND);
                    j.append_text(jid.to_string());
                    response.append_child(j);
                    let mut result = stanza::iq_result(&el, Some(&ctx.domain), Some(response));
                    stanza::remove_attr(&mut result, "to");
                    stream.send(&result).await?;
                    return Ok(Some(Negotiated::Bound(Box::new(Bound {
                        conn: Conn { stream, budget, guard },
                        jid,
                        conn_id,
                        outbox,
                        kick,
                    }))));
                }
                Err(cond) => {
                    tracing::info!(account = %bare, "bind refused: {}", cond.name());
                    if let Some(e) = stanza::error_reply(&el, cond, Some(&ctx.domain)) {
                        stream.send(&e).await?;
                    }
                }
            }
        }
    }
}

/// Answer the initiator's header. `None`: refused with `host-unknown`.
async fn respond_to_header(ctx: &HubCtx, mut accepted: AcceptedStream<Io>) -> Result<Option<PendingFeaturesSend<Io>>> {
    let header = accepted.take_header();
    let host_ok = header
        .to
        .as_deref()
        .and_then(|t| crate::config::normalize_domain(t).ok())
        .is_some_and(|d| d == ctx.domain);
    // RFC 6120 §4.7.2: echo the initiator's `from` as our `to` if it parses.
    let to = header
        .from
        .as_deref()
        .and_then(|f| Jid::new(f).ok())
        .map(|j| Cow::Owned(j.to_string()));
    let pending = accepted
        .send_header(StreamHeader {
            from: Some(Cow::Owned(ctx.domain.clone())),
            to,
            id: Some(Cow::Owned(random_id())),
        })
        .await?;
    if host_ok {
        return Ok(Some(pending));
    }
    tracing::info!(to = ?header.to, "refused stream for another host");
    pending
        .send_error(&StreamError::new(
            StreamCondition::HostUnknown,
            "en",
            format!("This hub serves {}", ctx.domain),
        ))
        .await?;
    Ok(None)
}

/// Registration IQs on an unauthenticated stream. `None`: not one of ours.
async fn registration_iq(ctx: &HubCtx, el: &Element, ch: &mut ChannelState) -> Option<Element> {
    if Kind::of(el) != Some(Kind::Iq) || el.attr("to").is_some_and(|t| t != ctx.domain) {
        return None;
    }
    let payload = el.children().next()?;
    let ty = stanza::stanza_type(el);
    let is_register = payload.is("query", NS_REGISTER);
    let is_preauth = payload.is("preauth", NS_PARS);
    if !(is_register || is_preauth) || !matches!(ty, "get" | "set") {
        return None;
    }
    let reg = &ctx.registrar;
    if !reg.offered(ch.ip, ch.tls) {
        return stanza::error_reply_with(
            el,
            Condition::NotAllowed,
            Some(&ctx.domain),
            Some("Registration is not available on this connection (it needs TLS)."),
            None,
        );
    }
    if is_preauth {
        let token = payload.attr("token").unwrap_or_default().to_owned();
        if ty == "set" && reg.invite_valid(&token).await {
            ch.invite = Some(token);
            return Some(stanza::iq_result(el, Some(&ctx.domain), None));
        }
        return RegError::BadInvite.reply(el, &ctx.domain);
    }
    if ty == "get" {
        let form = reg.form_query(ch.ip, ch.invite.is_some());
        return Some(stanza::iq_result(el, Some(&ctx.domain), Some(form)));
    }
    if payload.has_child("remove", NS_REGISTER) {
        return stanza::error_reply(el, Condition::BadRequest, Some(&ctx.domain));
    }
    let Some(req) = Registrar::parse_set(payload, ch.ip, ch.invite.clone()) else {
        return stanza::error_reply_with(
            el,
            Condition::BadRequest,
            Some(&ctx.domain),
            Some("A username and a password are required."),
            None,
        );
    };
    match reg.register(req).await {
        Ok(_) => {
            ch.invite = None;
            Some(stanza::iq_result(el, Some(&ctx.domain), None))
        }
        Err(e) => e.reply(el, &ctx.domain),
    }
}

fn pre_auth_features(ctx: &HubCtx, ch: &ChannelState, offered: &[Mechanism]) -> StreamFeatures {
    let mut f = StreamFeatures::default();
    if ctx.registrar.offered(ch.ip, ch.tls) {
        f.in_band_registration = true;
        if ctx.registrar.mode() == crate::config::RegistrationMode::Pow {
            f.others.push(Element::bare("register", crate::register::pow::NS));
        }
        f.others.push(Element::bare("register", NS_IBR_TOKEN));
    }
    if !ch.tls {
        f.starttls = Some(StartTls {
            required: !(ch.loopback && ctx.config.listen.allow_plaintext_loopback),
        });
    }
    if !offered.is_empty() {
        // Built by hand to keep our order of preference (the typed field is a set).
        let mut mechs = Element::bare("mechanisms", ns::SASL);
        for m in offered {
            let mut e = Element::bare("mechanism", ns::SASL);
            e.append_text(m.name());
            mechs.append_child(e);
        }
        f.others.push(mechs);
    }
    if offered.iter().any(|m| m.is_plus()) {
        f.sasl_cb = Some(SaslChannelBinding {
            types: vec![CbType::TlsExporter],
        });
    }
    f
}

enum SaslOutcome {
    Success { localpart: String, additional: Vec<u8> },
    Failure(Failure),
    End,
}

enum Response {
    Data(Vec<u8>),
    Abort,
    Bad,
    End,
}

async fn read_response(stream: &mut Stream, budget: &Budget, ctx: &HubCtx) -> Response {
    match next(stream, budget, ctx).await {
        Next::End => Response::End,
        Next::El(el) if el.is("response", ns::SASL) => match decode_sasl_text(&el.text()) {
            Some(d) => Response::Data(d.unwrap_or_default()),
            None => Response::Bad,
        },
        Next::El(el) if el.is("abort", ns::SASL) => Response::Abort,
        Next::El(_) => Response::Bad,
    }
}

/// `None`: bad base64. `Some(None)`: no data. `=` is an empty response.
fn decode_sasl_text(text: &str) -> Option<Option<Vec<u8>>> {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    match t.as_str() {
        "" => Some(None),
        "=" => Some(Some(Vec::new())),
        _ => B64.decode(&t).ok().map(Some),
    }
}

async fn authenticate(
    ctx: &Arc<HubCtx>,
    stream: &mut Stream,
    budget: &Budget,
    auth: &Element,
    ch: &ChannelState,
    channel: Channel,
    offered: &[Mechanism],
) -> Result<SaslOutcome> {
    let name = auth.attr("mechanism").unwrap_or("");
    let Some(mech) = Mechanism::from_name(name).filter(|m| offered.contains(m)) else {
        let f = match Mechanism::from_name(name) {
            // Known and not plus: it needs TLS on this connection.
            Some(m) if !m.is_plus() && !channel.tls => Failure::EncryptionRequired,
            _ => Failure::InvalidMechanism,
        };
        tracing::info!(mechanism = name, result = f.as_str(), "authentication refused");
        ctx.stats.auth(name, f.as_str());
        return Ok(SaslOutcome::Failure(f));
    };
    let initial = match decode_sasl_text(&auth.text()) {
        None => return Ok(SaslOutcome::Failure(Failure::IncorrectEncoding)),
        Some(Some(d)) => d,
        Some(None) => {
            stream.send(&Challenge { data: Vec::new() }).await?;
            match read_response(stream, budget, ctx).await {
                Response::Data(d) => d,
                Response::Abort => return Ok(SaslOutcome::Failure(Failure::Aborted)),
                Response::Bad => return Ok(SaslOutcome::Failure(Failure::MalformedRequest)),
                Response::End => return Ok(SaslOutcome::End),
            }
        }
    };
    let result = match mech.hash() {
        Some(hash) => scram_exchange(ctx, stream, budget, mech, hash, &initial, ch, channel).await?,
        None => plain_exchange(ctx, &initial, ch).await?,
    };
    match &result {
        SaslOutcome::Success { localpart, .. } => {
            tracing::info!(mechanism = mech.name(), user = %localpart, "authenticated");
            ctx.stats.auth(mech.name(), "success");
            if let Err(e) = ctx
                .store
                .record_login(&ctx.domain, localpart, crate::util::now_ms())
                .await
            {
                tracing::warn!("recording the login failed: {e:#}");
            }
        }
        SaslOutcome::Failure(f) => {
            tracing::info!(mechanism = mech.name(), result = f.as_str(), "authentication failed");
            ctx.stats.auth(mech.name(), f.as_str());
        }
        SaslOutcome::End => {}
    }
    Ok(result)
}

/// Throttle keys: the account, and the network prefix unless on loopback.
fn throttle_keys(ctx: &HubCtx, username: &str, ch: &ChannelState) -> Vec<String> {
    let mut keys = vec![format!("acct:{}@{}", username, ctx.domain)];
    if !ch.loopback {
        keys.push(format!("net:{}", ch.prefix));
    }
    keys
}

#[allow(clippy::too_many_arguments)]
async fn scram_exchange(
    ctx: &Arc<HubCtx>,
    stream: &mut Stream,
    budget: &Budget,
    mech: Mechanism,
    hash: scram::Hash,
    initial: &[u8],
    ch: &ChannelState,
    channel: Channel,
) -> Result<SaslOutcome> {
    let first = match scram::parse_client_first(initial) {
        Ok(f) => f,
        Err(_) => return Ok(SaslOutcome::Failure(Failure::MalformedRequest)),
    };
    // Channel-binding rules (RFC 5802 §6, RFC 9266).
    let cb_ok = match (&first.cb, mech.is_plus()) {
        (CbFlag::Used(t), true) => t == "tls-exporter",
        (_, true) => false,
        (CbFlag::None, false) => true,
        // `y` means the client thinks we can't bind; we offered -PLUS, so
        // someone stripped it: a downgrade.
        (CbFlag::ClientOnly, false) => !channel.channel_binding,
        (CbFlag::Used(_), false) => false,
    };
    if !cb_ok {
        return Ok(SaslOutcome::Failure(
            if mech.is_plus() || matches!(first.cb, CbFlag::Used(_)) {
                Failure::MalformedRequest
            } else {
                Failure::NotAuthorized
            },
        ));
    }
    let username = normalize_username(&first.username).unwrap_or_else(|_| first.username.clone());
    if let Some(a) = &first.authzid
        && Jid::new(a).map(|j| j.to_string()) != Ok(format!("{username}@{}", ctx.domain))
    {
        return Ok(SaslOutcome::Failure(Failure::InvalidAuthzid));
    }
    let keys = throttle_keys(ctx, &username, ch);
    if !ctx.throttle.allowed(&keys) {
        return Ok(SaslOutcome::Failure(Failure::TemporaryAuthFailure));
    }
    let account = ctx.store.account(&ctx.domain, &username).await?;
    // Unknown users get a stable fake salt, the normal iteration count and
    // random keys, so the exchange is indistinguishable until the end.
    let random_keys = || ScramKeys {
        stored_key: random_bytes::<32>()[..hash.output_len()].to_vec(),
        server_key: random_bytes::<32>()[..hash.output_len()].to_vec(),
    };
    let (salt, iterations, scram_keys) = match &account {
        Some(a) => (
            a.credentials.salt.clone(),
            a.credentials.iterations,
            match hash {
                scram::Hash::Sha1 => a.credentials.sha1.clone(),
                // An imported account without SHA-256 keys: fails like a
                // wrong password (offer only SCRAM-SHA-1 on migrated hubs).
                scram::Hash::Sha256 => a.credentials.sha256.clone().unwrap_or_else(random_keys),
            },
        ),
        None => (
            sasl::fake_salt(&ctx.mock_key, &ctx.domain, &username),
            ctx.config.auth.scram_iterations,
            ScramKeys {
                stored_key: random_bytes::<32>()[..hash.output_len()].to_vec(),
                server_key: random_bytes::<32>()[..hash.output_len()].to_vec(),
            },
        ),
    };
    let cb_data = if mech.is_plus() {
        ch.cb_data.clone().unwrap_or_default()
    } else {
        Vec::new()
    };
    let server_nonce = B64.encode(random_bytes::<18>());
    let (exchange, server_first) = Exchange::start(hash, first, &salt, iterations, scram_keys, &server_nonce, cb_data);
    stream
        .send(&Challenge {
            data: server_first.into_bytes(),
        })
        .await?;
    let client_final = match read_response(stream, budget, ctx).await {
        Response::Data(d) => d,
        Response::Abort => return Ok(SaslOutcome::Failure(Failure::Aborted)),
        Response::Bad => return Ok(SaslOutcome::Failure(Failure::MalformedRequest)),
        Response::End => return Ok(SaslOutcome::End),
    };
    let outcome = match (exchange.finish(&client_final), account) {
        (Ok(server_final), Some(a)) if a.state == AccountState::Active => SaslOutcome::Success {
            localpart: a.localpart,
            additional: server_final.into_bytes(),
        },
        (Ok(_), Some(_)) => SaslOutcome::Failure(Failure::AccountDisabled),
        (Ok(_), None) | (Err(ScramError::NotAuthorized), _) => SaslOutcome::Failure(Failure::NotAuthorized),
        (Err(ScramError::InvalidAuthzid), _) => SaslOutcome::Failure(Failure::InvalidAuthzid),
        (Err(ScramError::Malformed(_)), _) => SaslOutcome::Failure(Failure::MalformedRequest),
    };
    record_throttle(ctx, &keys, &outcome);
    Ok(outcome)
}

async fn plain_exchange(ctx: &Arc<HubCtx>, initial: &[u8], ch: &ChannelState) -> Result<SaslOutcome> {
    let Some(msg) = plain::parse(initial) else {
        return Ok(SaslOutcome::Failure(Failure::MalformedRequest));
    };
    let username = normalize_username(&msg.username).unwrap_or_else(|_| msg.username.clone());
    if let Some(a) = &msg.authzid
        && Jid::new(a).map(|j| j.to_string()) != Ok(format!("{username}@{}", ctx.domain))
    {
        return Ok(SaslOutcome::Failure(Failure::InvalidAuthzid));
    }
    let keys = throttle_keys(ctx, &username, ch);
    if !ctx.throttle.allowed(&keys) {
        return Ok(SaslOutcome::Failure(Failure::TemporaryAuthFailure));
    }
    let account = ctx.store.account(&ctx.domain, &username).await?;
    // Verify with SHA-256 keys, or SHA-1 keys for imported accounts.
    let (salt, iterations, hash, stored) = match &account {
        Some(a) => match &a.credentials.sha256 {
            Some(k) => (
                a.credentials.salt.clone(),
                a.credentials.iterations,
                scram::Hash::Sha256,
                k.stored_key.clone(),
            ),
            None => (
                a.credentials.salt.clone(),
                a.credentials.iterations,
                scram::Hash::Sha1,
                a.credentials.sha1.stored_key.clone(),
            ),
        },
        None => (
            sasl::fake_salt(&ctx.mock_key, &ctx.domain, &username),
            ctx.config.auth.scram_iterations,
            scram::Hash::Sha256,
            random_bytes::<32>().to_vec(),
        ),
    };
    let password = zeroize::Zeroizing::new(msg.password);
    let prepped = stringprep::saslprep(&password)
        .ok()
        .map(|p| zeroize::Zeroizing::new(p.into_owned()));
    // Same PBKDF2 work for known and unknown users.
    let check = prepped.clone();
    let salt2 = salt.clone();
    let derived =
        tokio::task::spawn_blocking(move || check.map(|p| hash.keys(&p, &salt2, iterations).stored_key)).await?;
    let ok = derived.is_some_and(|d| bool::from(d.ct_eq(&stored)));
    if ok
        && hash == scram::Hash::Sha1
        && let (Some(a), Some(p)) = (&account, prepped)
    {
        // Upgrade an imported account: we have the password now.
        let mut creds = a.credentials.clone();
        let keys = tokio::task::spawn_blocking(move || scram::Hash::Sha256.keys(&p, &salt, iterations)).await?;
        creds.sha256 = Some(keys);
        match ctx.store.set_credentials(&ctx.domain, &a.localpart, creds).await {
            Ok(_) => tracing::info!(user = %a.localpart, "added SCRAM-SHA-256 keys to an imported account"),
            Err(e) => tracing::warn!("upgrading imported credentials failed: {e:#}"),
        }
    }
    let outcome = match account {
        Some(a) if ok && a.state == AccountState::Active => SaslOutcome::Success {
            localpart: a.localpart,
            additional: Vec::new(),
        },
        Some(_) if ok => SaslOutcome::Failure(Failure::AccountDisabled),
        _ => SaslOutcome::Failure(Failure::NotAuthorized),
    };
    record_throttle(ctx, &keys, &outcome);
    Ok(outcome)
}

fn record_throttle(ctx: &HubCtx, keys: &[String], outcome: &SaslOutcome) {
    match outcome {
        SaslOutcome::Success { .. } => ctx.throttle.success(&keys[..1]),
        SaslOutcome::Failure(f) if f.counts_as_guess() => ctx.throttle.failure(keys),
        _ => {}
    }
}

/// Register the session with the account's actor. A conflict replaces the
/// old session; the actor caps resources per account.
async fn bind_resource(
    ctx: &Arc<HubCtx>,
    bare: &BareJid,
    resource: Option<String>,
    conn_id: u64,
) -> Result<(FullJid, mpsc::Receiver<Element>, Arc<Kick>), Condition> {
    let (tx, rx) = mpsc::channel(OUTBOX);
    let kick = Arc::new(Kick::default());
    for _ in 0..3 {
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = UserMsg::Bind {
            resource: resource.clone(),
            link: SessionLink {
                conn_id,
                outbox: tx.clone(),
                kick: kick.clone(),
            },
            reply: reply_tx,
        };
        if ctx.router.send_to_actor(bare, msg).is_err() {
            return Err(Condition::ResourceConstraint);
        }
        match reply_rx.await {
            Ok(Ok(jid)) => return Ok((jid, rx, kick)),
            Ok(Err(BindError::ResourceConstraint)) => return Err(Condition::ResourceConstraint),
            Ok(Err(BindError::Retry)) | Err(_) => continue,
        }
    }
    Err(Condition::ResourceConstraint)
}
