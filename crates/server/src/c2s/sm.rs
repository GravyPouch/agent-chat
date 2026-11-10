//! XEP-0198 stream management (§5.4.1): counters, the queue of
//! unacknowledged stanzas, and the registry of resumable sessions.
//!
//! A resumable session that loses its connection hibernates: its task keeps
//! running, queues what the account's actor sends it, and waits for a
//! `<resume/>` on a new connection, which the negotiator hands over through
//! [`Registry`]. Presence is unchanged while it hibernates.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::BareJid;

use super::Stream;
use crate::net::io::Budget;
use crate::net::listener::AdmissionGuard;

pub const NS_SM: &str = "urn:xmpp:sm:3";
pub const NS_CSI: &str = "urn:xmpp:csi:0";

/// One client connection after negotiation: what a session runs on, and
/// what a resumption hands over.
pub struct Conn {
    pub stream: Stream,
    pub budget: Arc<Budget>,
    /// Holds the per-IP connection slot while the connection lives.
    pub guard: AdmissionGuard,
}

/// "Continue session `previd` on this connection; I handled `h` stanzas."
pub struct ResumeRequest {
    pub conn: Conn,
    pub h: u32,
}

#[derive(Clone)]
pub struct Handle {
    pub bare: BareJid,
    pub tx: mpsc::Sender<ResumeRequest>,
}

/// Resumable sessions by stream id.
#[derive(Default)]
pub struct Registry {
    map: Mutex<HashMap<String, Handle>>,
}

impl Registry {
    pub fn insert(&self, id: String, h: Handle) {
        self.map.lock().unwrap().insert(id, h);
    }

    pub fn remove(&self, id: &str) {
        self.map.lock().unwrap().remove(id);
    }

    pub fn get(&self, id: &str) -> Option<Handle> {
        self.map.lock().unwrap().get(id).cloned()
    }
}

/// The acknowledgement in `<a h>` (or `<resume h>`) is more than we sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooHigh {
    pub h: u32,
    pub sent: u32,
}

/// Counters and the unacknowledged queue for one session (XEP-0198 §4).
#[derive(Debug, Default)]
pub struct SmState {
    pub enabled: bool,
    /// Set when resumption was negotiated.
    pub id: Option<String>,
    /// Stanzas received from the client (our `h`).
    pub inbound: u32,
    /// The client's last `h`.
    acked: u32,
    /// Sent (or queued while hibernating) but not acknowledged; entry `i`
    /// is stanza number `acked + i + 1`.
    queue: VecDeque<Element>,
    /// We sent `<r/>` and await the `<a/>`.
    pub r_pending: bool,
}

impl SmState {
    /// How many stanzas we have sent in total (mod 2³²).
    pub fn sent(&self) -> u32 {
        self.acked.wrapping_add(self.queue.len() as u32)
    }

    pub fn unacked(&self) -> usize {
        self.queue.len()
    }

    /// Record an outbound stanza (before it is written).
    pub fn on_sent(&mut self, el: &Element) {
        if self.enabled {
            self.queue.push_back(el.clone());
        }
    }

    pub fn on_received(&mut self) {
        if self.enabled {
            self.inbound = self.inbound.wrapping_add(1);
        }
    }

    /// Apply the client's `h`. An `h` behind the last one is ignored.
    pub fn ack(&mut self, h: u32) -> Result<(), TooHigh> {
        let n = h.wrapping_sub(self.acked);
        if n as usize > self.queue.len() {
            if n > u32::MAX / 2 {
                // Went backwards: stale, harmless.
                return Ok(());
            }
            return Err(TooHigh { h, sent: self.sent() });
        }
        self.queue.drain(..n as usize);
        self.acked = h;
        Ok(())
    }

    /// The unacknowledged stanzas, oldest first (to retransmit).
    pub fn pending(&self) -> impl Iterator<Item = &Element> {
        self.queue.iter()
    }

    /// Everything still unacknowledged, for the end of the session.
    pub fn take_unacked(&mut self) -> Vec<Element> {
        self.queue.drain(..).collect()
    }
}

/// `<a h='…'/>`, `<r/>`, `<enabled/>`, `<resumed/>`, `<failed/>`.
pub fn ack(h: u32) -> Element {
    crate::stanza::element("a", NS_SM, &[("h", &h.to_string())])
}

pub fn request() -> Element {
    Element::bare("r", NS_SM)
}

/// `h` is required by tokio-xmpp 6's parser (and allowed by XEP-0198), so
/// it is always present; 0 when the session is unknown.
pub fn failed(condition: &str, h: u32) -> Element {
    let mut f = crate::stanza::element("failed", NS_SM, &[("h", &h.to_string())]);
    f.append_child(Element::bare(condition, xmpp_parsers::ns::XMPP_STANZAS));
    f
}

/// Whether a read error means the peer vanished (EOF or reset) rather than
/// sent bad XML. rxml reports EOF inside the stream as `InvalidEof`.
pub fn is_disconnect(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    if matches!(
        e.kind(),
        UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | TimedOut
    ) {
        return true;
    }
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<tokio_xmpp::minidom::rxml::Error>())
        .is_some_and(|x| matches!(x, tokio_xmpp::minidom::rxml::Error::InvalidEof(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(n: u32) -> Element {
        crate::stanza::element("message", crate::stanza::NS_CLIENT, &[("id", &n.to_string())])
    }

    #[test]
    fn counts_acks_and_queue() {
        let mut sm = SmState::default();
        sm.on_sent(&el(0));
        assert_eq!(sm.unacked(), 0, "nothing counted before enable");
        sm.enabled = true;
        for n in 1..=5 {
            sm.on_sent(&el(n));
        }
        assert_eq!(sm.sent(), 5);
        sm.ack(3).unwrap();
        let ids: Vec<_> = sm.pending().filter_map(|e| e.attr("id").map(str::to_owned)).collect();
        assert_eq!(ids, ["4", "5"]);
        // Stale acks are ignored; too-high acks are errors.
        sm.ack(2).unwrap();
        assert_eq!(sm.unacked(), 2);
        assert_eq!(sm.ack(9), Err(TooHigh { h: 9, sent: 5 }));
        sm.ack(5).unwrap();
        assert_eq!(sm.unacked(), 0);
        sm.on_received();
        assert_eq!(sm.inbound, 1);
    }

    #[test]
    fn counters_wrap() {
        let mut sm = SmState {
            enabled: true,
            acked: u32::MAX - 1,
            ..Default::default()
        };
        for n in 0..4 {
            sm.on_sent(&el(n));
        }
        assert_eq!(sm.sent(), 2);
        sm.ack(1).unwrap();
        assert_eq!(sm.unacked(), 1);
    }
}
