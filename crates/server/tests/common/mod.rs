//! Test harness (§8.2): a hub on 127.0.0.1:0 in a temporary directory,
//! tokio-xmpp clients (plaintext loopback or pinned STARTTLS), and a raw
//! byte-level client for negotiation details.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_chat_server::admin::{AdminClient, AdminRequest, HubStatus, UserCreated};
use agent_chat_server::{HubPaths, InitOptions, RunningHub};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::StreamExt;
use sasl::client::Mechanism;
use sasl::client::mechanisms::Scram;
use sasl::common::ChannelBinding;
use sasl::common::scram::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_xmpp::connect::{DnsConfig, TcpServerConnector};
use tokio_xmpp::stanzastream::{Event, StanzaStream, StreamEvent};
use tokio_xmpp::xmlstream::Timeouts;
use xmpp_parsers::jid::Jid;

pub mod pinned;

pub const DOMAIN: &str = "hubtest.local";
pub const TIMEOUT: Duration = Duration::from_secs(10);

pub fn install_crypto_provider() {
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
}

pub struct TestHub {
    pub hub: Option<RunningHub>,
    pub dir: tempfile::TempDir,
    pub paths: HubPaths,
    pub addr: SocketAddr,
    pub pin: String,
}

impl TestHub {
    pub async fn start() -> Self {
        install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let paths = HubPaths::at(dir.path().join("hub"));
        agent_chat_server::init(
            &paths,
            InitOptions {
                domain: Some(DOMAIN.into()),
                listen: vec!["127.0.0.1:0".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let hub = RunningHub::start(paths.clone()).await.unwrap();
        let addr = hub.listeners()[0];
        let pin = hub.ctx.tls.as_ref().unwrap().pin.clone();
        Self {
            hub: Some(hub),
            dir,
            paths,
            addr,
            pin,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.paths.socket()
    }

    pub async fn admin(&self, req: AdminRequest) -> serde_json::Value {
        let mut c = AdminClient::connect(&self.socket()).await.unwrap();
        c.call_value(req).await.unwrap()
    }

    pub async fn add_user(&self, name: &str, password: &str) -> UserCreated {
        let v = self
            .admin(AdminRequest::UserAdd {
                username: name.into(),
                password: Some(password.into()),
                admin: false,
            })
            .await;
        serde_json::from_value(v).unwrap()
    }

    pub async fn status(&self) -> HubStatus {
        serde_json::from_value(self.admin(AdminRequest::Status).await).unwrap()
    }

    pub fn auth_count(status: &HubStatus, mechanism: &str, result: &str) -> u64 {
        status
            .auth
            .iter()
            .find(|a| a.mechanism == mechanism && a.result == result)
            .map(|a| a.count)
            .unwrap_or(0)
    }

    pub async fn stop(mut self) {
        if let Some(h) = self.hub.take() {
            h.shutdown().await;
        }
    }

    /// A tokio-xmpp client over plaintext loopback (SCRAM without TLS).
    pub fn plaintext_client(&self, jid: &str, password: &str) -> StanzaStream {
        StanzaStream::new_c2s(
            TcpServerConnector(DnsConfig::addr(&self.addr.to_string())),
            Jid::new(jid).unwrap(),
            password.into(),
            Timeouts::tight(),
            16,
        )
    }

    /// A tokio-xmpp client over STARTTLS that trusts the hub's pin.
    pub fn tls_client(&self, jid: &str, password: &str) -> StanzaStream {
        StanzaStream::new_c2s(
            pinned::PinnedStartTls {
                addr: self.addr,
                pin: self.pin.clone(),
            },
            Jid::new(jid).unwrap(),
            password.into(),
            Timeouts::tight(),
            16,
        )
    }
}

/// Wait for the next event matching `f`.
pub async fn next_event<T>(s: &mut StanzaStream, mut f: impl FnMut(Event) -> Option<T>) -> T {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let ev = s.next().await.expect("stream ended");
            if let Some(t) = f(ev) {
                return t;
            }
        }
    })
    .await
    .expect("timed out waiting for an event")
}

/// Wait until the stream is bound; returns the bound JID.
pub async fn online(s: &mut StanzaStream) -> Jid {
    next_event(s, |ev| match ev {
        Event::Stream(StreamEvent::Reset { bound_jid, .. }) => Some(bound_jid),
        _ => None,
    })
    .await
}

/// Raw TCP client speaking literal XML, for byte-level checks.
pub struct Raw {
    pub stream: TcpStream,
    buf: Vec<u8>,
}

impl Raw {
    pub async fn connect(addr: SocketAddr) -> Self {
        Self {
            stream: TcpStream::connect(addr).await.unwrap(),
            buf: Vec::new(),
        }
    }

    pub async fn send(&mut self, s: &str) {
        self.stream.write_all(s.as_bytes()).await.unwrap();
    }

    pub async fn open(&mut self, to: &str) {
        self.send(&format!(
            "<?xml version='1.0'?><stream:stream xmlns='jabber:client' \
             xmlns:stream='http://etherx.jabber.org/streams' to='{to}' version='1.0'>"
        ))
        .await;
    }

    /// Everything up to and including `needle` (panics on timeout or EOF).
    pub async fn read_until(&mut self, needle: &str) -> String {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Some(pos) = find(&self.buf, needle.as_bytes()) {
                    let end = pos + needle.len();
                    let out: Vec<u8> = self.buf.drain(..end).collect();
                    return String::from_utf8(out).unwrap();
                }
                let mut chunk = [0u8; 8192];
                let n = self.stream.read(&mut chunk).await.unwrap();
                assert!(
                    n > 0,
                    "EOF while waiting for {needle:?}; got {:?}",
                    String::from_utf8_lossy(&self.buf)
                );
                self.buf.extend_from_slice(&chunk[..n]);
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {needle:?}; got {:?}",
                String::from_utf8_lossy(&self.buf)
            )
        })
    }

    /// Everything until the peer closes (or resets) the connection.
    pub async fn read_to_end(&mut self) -> String {
        tokio::time::timeout(TIMEOUT, async {
            loop {
                let mut chunk = [0u8; 8192];
                match self.stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                }
            }
        })
        .await
        .expect("the server did not close the connection");
        String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned()
    }

    /// Open the stream and read the features.
    pub async fn start(addr: SocketAddr) -> (Self, String) {
        let mut raw = Self::connect(addr).await;
        raw.open(DOMAIN).await;
        let features = raw.read_until("</stream:features>").await;
        (raw, features)
    }

    /// SCRAM-SHA-256 without channel binding. Returns the server's
    /// challenge (server-first) and the final reply element.
    pub async fn scram(&mut self, user: &str, password: &str) -> (String, String) {
        let mut client = Scram::<Sha256>::new(user, password, ChannelBinding::None).unwrap();
        self.send(&format!(
            "<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='SCRAM-SHA-256'>{}</auth>",
            B64.encode(client.initial())
        ))
        .await;
        let challenge = self.read_until("</challenge>").await;
        let data = between(&challenge, ">", "</challenge>");
        let server_first = String::from_utf8(B64.decode(data).unwrap()).unwrap();
        let response = client.response(server_first.as_bytes()).unwrap();
        self.send(&format!(
            "<response xmlns='urn:ietf:params:xml:ns:xmpp-sasl'>{}</response>",
            B64.encode(response)
        ))
        .await;
        let mut reply = String::new();
        while !reply.contains("</success>") && !reply.contains("</failure>") {
            let more = self.read_some().await;
            reply.push_str(&more);
        }
        (server_first, reply)
    }

    async fn read_some(&mut self) -> String {
        if !self.buf.is_empty() {
            return String::from_utf8(std::mem::take(&mut self.buf)).unwrap();
        }
        let mut chunk = [0u8; 8192];
        let n = tokio::time::timeout(TIMEOUT, self.stream.read(&mut chunk))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0, "EOF");
        String::from_utf8(chunk[..n].to_vec()).unwrap()
    }

    /// Log in over plaintext loopback and restart the stream; returns the
    /// post-auth features.
    pub async fn login(addr: SocketAddr, user: &str, password: &str) -> (Self, String) {
        let (mut raw, _) = Self::start(addr).await;
        let (_, reply) = raw.scram(user, password).await;
        assert!(reply.contains("<success"), "login failed: {reply}");
        raw.open(DOMAIN).await;
        let features = raw.read_until("</stream:features>").await;
        (raw, features)
    }

    /// Bind `resource`; returns the reply IQ.
    pub async fn bind(&mut self, resource: &str) -> String {
        self.send(&format!(
            "<iq type='set' id='bind-1'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'>\
             <resource>{resource}</resource></bind></iq>"
        ))
        .await;
        self.read_until("</iq>").await
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub fn between<'a>(s: &'a str, start: &str, end: &str) -> &'a str {
    let a = s.find(start).unwrap() + start.len();
    let b = s[a..].find(end).unwrap() + a;
    &s[a..b]
}

pub type Shared<T> = Arc<tokio::sync::Mutex<T>>;
