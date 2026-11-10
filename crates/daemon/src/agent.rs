//! What agents reading through this daemon look like to others.
//!
//! **Typing.** When an agent consumer (CLI `wait`/`inbox`, MCP) reads an
//! inbound 1:1 message, the sender sees "composing" until the agent replies
//! or `settings.agent_typing_timeout_secs` passes. Only senders of messages
//! the agent actually read get chat states, and chat states never wake a
//! consumer, so nothing here can loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_chat_proto::ipc::IpcError;
use agent_chat_proto::model::{ChatState, Message, MessageKind, MessageSource};

use crate::core::Core;
use crate::xmpp::actions;

/// Conversations we told "composing", and when to give up on each.
#[derive(Debug, Default)]
pub struct Typing {
    /// lower-case bare JID → deadline for sending "active"
    composing: HashMap<String, Instant>,
}

impl Typing {
    /// We start (or keep) typing to `jid`. Returns true when "composing"
    /// should be sent, i.e. it wasn't showing already; either way the
    /// deadline moves to `now + timeout`.
    pub fn start(&mut self, jid: &str, now: Instant, timeout: Duration) -> bool {
        self.composing.insert(jid.to_lowercase(), now + timeout).is_none()
    }

    /// We replied or stopped typing. Returns true if "composing" was showing.
    pub fn stop(&mut self, jid: &str) -> bool {
        self.composing.remove(&jid.to_lowercase()).is_some()
    }

    pub fn is_typing(&self, jid: &str) -> bool {
        self.composing.contains_key(&jid.to_lowercase())
    }

    /// Conversations whose typing timed out; they are forgotten.
    pub fn expired(&mut self, now: Instant) -> Vec<String> {
        let mut out: Vec<String> = self
            .composing
            .iter()
            .filter(|(_, deadline)| **deadline <= now)
            .map(|(jid, _)| jid.clone())
            .collect();
        out.sort();
        for jid in &out {
            self.composing.remove(jid);
        }
        out
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.composing.values().min().copied()
    }
}

/// Should reading `m` make us type to its sender? Only real 1:1 messages
/// from someone else that we could read.
pub fn types_back(m: &Message) -> bool {
    !m.own && m.kind == MessageKind::Chat && m.encryption.as_ref().is_none_or(|e| e.error.is_none())
}

#[derive(Debug, Default)]
pub struct AgentState {
    pub typing: Typing,
}

impl AgentState {
    pub fn next_deadline(&self) -> Option<Instant> {
        self.typing.next_deadline()
    }
}

fn typing_settings(core: &Core) -> (bool, Duration) {
    let cfg = core.config.lock().unwrap();
    (
        cfg.settings.agent_typing,
        Duration::from_secs(cfg.settings.agent_typing_timeout_secs.max(1)),
    )
}

/// An agent consumer just read these messages (`wait`, `inbox`, MCP).
pub fn on_read(core: &Core, msgs: &[Message]) {
    let (enabled, timeout) = typing_settings(core);
    if !enabled {
        return;
    }
    let now = Instant::now();
    let mut start = Vec::new();
    {
        let mut agent = core.agent();
        for m in msgs.iter().filter(|m| types_back(m)) {
            if agent.typing.start(&m.conversation, now, timeout) {
                start.push(m.conversation.clone());
            }
        }
    }
    core.agent_wake.notify_one();
    if start.is_empty() {
        return;
    }
    let Ok(handle) = core.session() else {
        // Offline: nobody can see us type anyway.
        let mut agent = core.agent();
        for jid in &start {
            agent.typing.stop(jid);
        }
        return;
    };
    for jid in start {
        if let Err(e) = actions::send_chat_state(core, &handle, &jid, ChatState::Composing) {
            tracing::debug!("could not send composing to {jid}: {e}");
            core.agent().typing.stop(&jid);
        }
    }
}

/// This account sent a message: typing there is over. Our own sends carry
/// `<active/>`; for one from another device (a carbon) we say it ourselves.
pub fn on_own_message(core: &Core, m: &Message) {
    if !matches!(m.kind, MessageKind::Chat | MessageKind::Groupchat) || !core.agent().typing.stop(&m.conversation) {
        return;
    }
    if m.source != MessageSource::Local
        && let Ok(handle) = core.session()
    {
        let _ = actions::send_chat_state(core, &handle, &m.conversation, ChatState::Active);
    }
}

/// `SendChatState` from a client. With `auto_stop`, "composing" ends by
/// itself after the typing timeout unless a message or another state follows.
pub fn send_chat_state(core: &Core, to: &str, state: ChatState, auto_stop: bool) -> Result<(), IpcError> {
    let handle = core.session()?;
    actions::send_chat_state(core, &handle, to, state)?;
    match state {
        ChatState::Composing if auto_stop => {
            let (_, timeout) = typing_settings(core);
            core.agent().typing.start(to, Instant::now(), timeout);
            core.agent_wake.notify_one();
        }
        ChatState::Composing => {}
        _ => {
            core.agent().typing.stop(to);
        }
    }
    Ok(())
}

/// The daemon's timer: ends typing that timed out.
pub async fn run(core: Arc<Core>) {
    loop {
        let next = core.agent().next_deadline();
        let sleep = async {
            match next {
                Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = core.agent_wake.notified() => {}
            _ = sleep => {}
        }
        tick(&core);
    }
}

fn tick(core: &Core) {
    let expired = core.agent().typing.expired(Instant::now());
    if expired.is_empty() {
        return;
    }
    let Ok(handle) = core.session() else { return };
    for jid in expired {
        if let Err(e) = actions::send_chat_state(core, &handle, &jid, ChatState::Active) {
            tracing::debug!("could not end typing to {jid}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_chat_proto::model::{EncryptionInfo, EncryptionProtocol, Trust};

    const T: Duration = Duration::from_secs(120);

    #[test]
    fn typing_starts_once_and_refreshes() {
        let mut t = Typing::default();
        let now = Instant::now();
        assert!(t.start("Bob@Example.org", now, T), "first read sends composing");
        assert!(
            !t.start("bob@example.org", now + Duration::from_secs(60), T),
            "already typing"
        );
        assert!(t.is_typing("bob@example.org"));
        // The second read moved the deadline.
        assert_eq!(t.next_deadline(), Some(now + Duration::from_secs(180)));
        assert!(t.expired(now + Duration::from_secs(179)).is_empty());
        assert_eq!(t.expired(now + Duration::from_secs(180)), vec!["bob@example.org"]);
        assert!(!t.is_typing("bob@example.org"));
        assert_eq!(t.next_deadline(), None);
    }

    #[test]
    fn replying_stops_typing_and_nothing_expires() {
        let mut t = Typing::default();
        let now = Instant::now();
        t.start("bob@example.org", now, T);
        t.start("carol@example.org", now + Duration::from_secs(5), T);
        assert!(t.stop("BOB@example.org"));
        assert!(!t.stop("bob@example.org"), "only once");
        assert_eq!(t.next_deadline(), Some(now + Duration::from_secs(125)));
        assert_eq!(t.expired(now + Duration::from_secs(500)), vec!["carol@example.org"]);
        assert!(t.expired(now + Duration::from_secs(900)).is_empty());
    }

    fn msg(kind: MessageKind, own: bool) -> Message {
        Message {
            id: 1,
            conversation: "bob@example.org".into(),
            kind,
            from: "bob@example.org".into(),
            nick: None,
            body: "hi".into(),
            timestamp: chrono::Utc::now(),
            delayed: false,
            source: MessageSource::Live,
            own,
            encryption: None,
        }
    }

    #[test]
    fn only_readable_one_to_one_messages_from_others_type_back() {
        assert!(types_back(&msg(MessageKind::Chat, false)));
        assert!(!types_back(&msg(MessageKind::Chat, true)), "our own messages");
        assert!(!types_back(&msg(MessageKind::Groupchat, false)), "rooms");
        assert!(
            !types_back(&msg(MessageKind::MucPrivate, false)),
            "room private messages"
        );
        let mut broken = msg(MessageKind::Chat, false);
        broken.encryption = Some(EncryptionInfo {
            protocol: EncryptionProtocol::Omemo2,
            sender_device: 1,
            trust: Trust::Trusted,
            error: Some("no session".into()),
        });
        assert!(!types_back(&broken), "undecryptable");
        broken.encryption.as_mut().unwrap().error = None;
        assert!(types_back(&broken));
    }
}
