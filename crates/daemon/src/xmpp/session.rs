//! The session actor: owns the `StanzaStream`, serialises outbound stanzas,
//! tracks IQ requests and dispatches inbound stanzas in order.
//!
//! We use `tokio_xmpp::stanzastream::StanzaStream` directly rather than
//! `tokio_xmpp::Client`: its `send` only waits for enqueueing, so the actor can
//! keep reading while the connection is down (XEP-0198 resumption is handled
//! inside the stream).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_xmpp::connect::{DnsConfig, StartTlsServerConnector, TcpServerConnector};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::stanzastream::{Event as StreamEvent, StanzaStream, StreamEvent as Lifecycle};
use tokio_xmpp::xmlstream::Timeouts;
use xmpp_parsers::iq::Iq;
use xmpp_parsers::jid::{BareJid, FullJid, Jid};
use xmpp_parsers::stanza::Stanza;

use crate::config::{AccountConfig, is_loopback_host};
use crate::core::Core;

const QUEUE_DEPTH: usize = 64;
pub const IQ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
enum Cmd {
    Send(Box<Stanza>),
    Iq {
        to: Option<Jid>,
        get: bool,
        payload: Element,
        reply: oneshot::Sender<Result<Option<Element>>>,
    },
    Stop(oneshot::Sender<()>),
}

/// Cheap, cloneable handle used by the rest of the daemon to talk to the session.
#[derive(Clone, Debug)]
pub struct SessionHandle {
    tx: mpsc::UnboundedSender<Cmd>,
    /// Our account's bare JID.
    pub own: BareJid,
}

impl SessionHandle {
    pub fn send(&self, stanza: impl Into<Stanza>) -> Result<()> {
        self.tx
            .send(Cmd::Send(Box::new(stanza.into())))
            .map_err(|_| anyhow!("XMPP session has stopped"))
    }

    async fn iq(&self, to: Option<Jid>, get: bool, payload: Element) -> Result<Option<Element>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Iq {
                to,
                get,
                payload,
                reply,
            })
            .map_err(|_| anyhow!("XMPP session has stopped"))?;
        tokio::time::timeout(IQ_TIMEOUT, rx)
            .await
            .map_err(|_| anyhow!("IQ request timed out"))?
            .map_err(|_| anyhow!("XMPP session has stopped"))?
    }

    /// Send an IQ get and wait for the result payload.
    pub async fn iq_get(&self, to: Option<Jid>, payload: impl Into<Element>) -> Result<Option<Element>> {
        self.iq(to, true, payload.into()).await
    }

    /// Send an IQ set and wait for the result payload.
    pub async fn iq_set(&self, to: Option<Jid>, payload: impl Into<Element>) -> Result<Option<Element>> {
        self.iq(to, false, payload.into()).await
    }

    /// Close the stream cleanly and stop the actor.
    pub async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Stop(tx)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
        }
    }
}

/// Start a session for the account. Connection happens in the background.
pub fn start(core: Arc<Core>, acct: &AccountConfig, password: String) -> Result<SessionHandle> {
    acct.validate()?;
    let bare = BareJid::new(&acct.jid).with_context(|| format!("invalid JID {:?}", acct.jid))?;
    let full: FullJid = bare
        .with_resource_str(&acct.resource)
        .context("invalid resource")?;
    let dns = match &acct.host {
        Some(host) => DnsConfig::no_srv(host, acct.port.unwrap_or(5222)),
        None => DnsConfig::srv_default_client(bare.domain().as_str()),
    };
    let stream = if acct.insecure_plaintext {
        let host = acct.host.clone().unwrap_or_else(|| bare.domain().to_string());
        if !is_loopback_host(&host) {
            bail!("refusing plaintext connection to non-local host {host}");
        }
        StanzaStream::new_c2s(
            TcpServerConnector(dns),
            full.into(),
            password,
            Timeouts::default(),
            QUEUE_DEPTH,
        )
    } else {
        StanzaStream::new_c2s(
            StartTlsServerConnector(dns),
            full.into(),
            password,
            Timeouts::default(),
            QUEUE_DEPTH,
        )
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let handle = SessionHandle { tx, own: bare };
    let actor = Actor {
        core,
        handle: handle.clone(),
        pending: HashMap::new(),
        next_iq: 0,
    };
    tokio::spawn(actor.run(stream, rx));
    Ok(handle)
}

struct Actor {
    core: Arc<Core>,
    handle: SessionHandle,
    /// (expected responder, id) → waiter
    pending: HashMap<(String, String), oneshot::Sender<Result<Option<Element>>>>,
    next_iq: u64,
}

impl Actor {
    async fn run(mut self, mut stream: StanzaStream, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            tokio::select! {
                ev = stream.next() => match ev {
                    Some(ev) => self.on_event(ev).await,
                    None => {
                        self.core.on_disconnected("stream closed");
                        break;
                    }
                },
                cmd = rx.recv() => match cmd {
                    Some(Cmd::Send(st)) => {
                        // Only waits for queue space; never for the network.
                        let _token = stream.send(st).await;
                    }
                    Some(Cmd::Iq { to, get, payload, reply }) => {
                        self.next_iq += 1;
                        let id = format!("ac-{}", self.next_iq);
                        let key = (self.responder_key(to.as_ref()), id.clone());
                        let iq = if get {
                            Iq::Get { from: None, to, id, payload }
                        } else {
                            Iq::Set { from: None, to, id, payload }
                        };
                        self.pending.insert(key, reply);
                        let _token = stream.send(Box::new(iq.into())).await;
                    }
                    Some(Cmd::Stop(done)) => {
                        let mut unavailable = xmpp_parsers::presence::Presence::unavailable();
                        unavailable.id = Some("bye".into());
                        let _ = tokio::time::timeout(
                            Duration::from_secs(2),
                            stream.send(Box::new(unavailable.into())),
                        )
                        .await;
                        let _ = tokio::time::timeout(Duration::from_secs(3), stream.close()).await;
                        self.core.on_disconnected("stopped");
                        let _ = done.send(());
                        return;
                    }
                    None => {
                        let _ = tokio::time::timeout(Duration::from_secs(3), stream.close()).await;
                        return;
                    }
                }
            }
        }
    }

    /// Key under which a response from `to` will be matched.
    fn responder_key(&self, to: Option<&Jid>) -> String {
        match to {
            None => self.handle.own.to_string(),
            Some(j) => j.to_string(),
        }
    }

    async fn on_event(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Stream(Lifecycle::Reset { bound_jid, .. }) => {
                // State was lost: outstanding IQs will never be answered.
                for (_, waiter) in self.pending.drain() {
                    let _ = waiter.send(Err(anyhow!("connection was reset")));
                }
                self.core.on_online(&self.handle, bound_jid);
            }
            StreamEvent::Stream(Lifecycle::Suspended) => self.core.on_suspended(),
            StreamEvent::Stream(Lifecycle::Resumed) => self.core.on_resumed(),
            StreamEvent::Stanza(Stanza::Iq(iq)) => self.on_iq(iq),
            StreamEvent::Stanza(Stanza::Message(m)) => {
                crate::xmpp::inbound::on_message(&self.core, &self.handle, m).await;
            }
            StreamEvent::Stanza(Stanza::Presence(p)) => {
                crate::xmpp::inbound::on_presence(&self.core, &self.handle, p);
            }
        }
    }

    fn on_iq(&mut self, iq: Iq) {
        let own = self.handle.own.to_string();
        let from_key = |from: &Option<Jid>| match from {
            None => own.clone(),
            Some(j) => j.to_string(),
        };
        match iq {
            Iq::Result { from, id, payload, .. } => {
                if let Some(w) = self.pending.remove(&(from_key(&from), id)) {
                    let _ = w.send(Ok(payload));
                }
            }
            Iq::Error { from, id, error, .. } => {
                if let Some(w) = self.pending.remove(&(from_key(&from), id)) {
                    let text = error
                        .texts
                        .values()
                        .next()
                        .cloned()
                        .unwrap_or_default();
                    let _ = w.send(Err(anyhow!(
                        "{:?}{}{}",
                        error.defined_condition,
                        if text.is_empty() { "" } else { ": " },
                        text
                    )));
                }
            }
            request => {
                if let Some(reply) = crate::xmpp::inbound::on_iq_request(&self.core, &self.handle, request) {
                    let _ = self.handle.send(reply);
                }
            }
        }
    }
}
