//! What agents reading through this daemon look like to others.
//!
//! **Typing.** When an agent consumer (CLI `wait`/`inbox`, MCP) reads an
//! inbound 1:1 message, the sender sees "composing" until the agent replies
//! or `settings.agent_typing_timeout_secs` passes. Only senders of messages
//! the agent actually read get chat states, and chat states never wake a
//! consumer, so nothing here can loop.
//!
//! **Listening.** An agent account's presence says whether anything reads
//! its messages: available "Listening" while a consumer is blocked in
//! `wait`/`listen` (or was active within `settings.agent_away_after_secs`),
//! away "Not listening; messages will wait" otherwise. Presence set
//! explicitly (`agent-chat presence …`) wins until `presence auto`.
//!
//! **Kind.** A profile of unknown kind becomes an agent's the first time an
//! agent consumer uses it; agents advertise the identity `client/bot`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_chat_proto::ipc::{ErrorCode, IpcError};
use agent_chat_proto::model::{
    AccountKind, ChatState, LISTENING_STATUS, Message, MessageKind, MessageSource, NOT_LISTENING_STATUS, PresenceInfo,
    Show,
};

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

/// Whether some consumer is reading this account's messages.
#[derive(Debug, Default)]
pub struct Listening {
    /// `wait` requests (CLI `wait`/`listen`, MCP `wait_for_message`) in flight.
    waiting: usize,
    /// When a consumer last did something.
    last_active: Option<Instant>,
}

impl Listening {
    pub fn begin_wait(&mut self, now: Instant) {
        self.waiting += 1;
        self.last_active = Some(now);
    }

    pub fn end_wait(&mut self, now: Instant) {
        self.waiting = self.waiting.saturating_sub(1);
        self.last_active = Some(now);
    }

    /// A consumer read, sent or otherwise acted just now.
    pub fn touch(&mut self, now: Instant) {
        self.last_active = Some(now);
    }

    pub fn is_listening(&self, now: Instant, grace: Duration) -> bool {
        self.waiting > 0
            || self
                .last_active
                .is_some_and(|t| now.saturating_duration_since(t) < grace)
    }

    /// When `is_listening` turns false if nothing else happens.
    pub fn next_change(&self, now: Instant, grace: Duration) -> Option<Instant> {
        let t = self.last_active?;
        (self.waiting == 0 && t + grace > now).then_some(t + grace)
    }
}

/// The presence an account should switch to, if any: agents say whether
/// they're listening; presence set by hand is left alone; a person's
/// account drops a leftover automatic agent status.
pub fn auto_presence(
    kind: AccountKind,
    listening: bool,
    explicit: bool,
    current: &PresenceInfo,
) -> Option<PresenceInfo> {
    if explicit {
        return None;
    }
    let want = match kind {
        AccountKind::Agent if listening => PresenceInfo::new(Show::Available, Some(LISTENING_STATUS.into())),
        AccountKind::Agent => PresenceInfo::new(Show::Away, Some(NOT_LISTENING_STATUS.into())),
        AccountKind::Person => {
            let automatic = matches!(current.status.as_deref(), Some(LISTENING_STATUS | NOT_LISTENING_STATUS));
            if !automatic {
                return None;
            }
            PresenceInfo::default()
        }
    };
    (want.show != current.show || want.status != current.status).then_some(want)
}

#[derive(Debug, Default)]
pub struct AgentState {
    pub typing: Typing,
    pub listening: Listening,
    /// Presence was set by hand (`agent-chat presence …`, the GUI's buttons).
    pub presence_explicit: bool,
}

impl AgentState {
    pub fn next_deadline(&self, now: Instant, grace: Duration) -> Option<Instant> {
        [self.typing.next_deadline(), self.listening.next_change(now, grace)]
            .into_iter()
            .flatten()
            .min()
    }
}

fn away_after(core: &Core) -> Duration {
    Duration::from_secs(core.config.lock().unwrap().settings.agent_away_after_secs.max(1))
}

pub fn is_listening(core: &Core) -> bool {
    let grace = away_after(core);
    core.agent().listening.is_listening(Instant::now(), grace)
}

/// Bring our presence in line with `auto_presence`. Sends it when online;
/// otherwise it's what the next login announces.
pub fn refresh_presence(core: &Core) {
    let kind = core.account_kind();
    let grace = away_after(core);
    let (listening, explicit) = {
        let agent = core.agent();
        (
            agent.listening.is_listening(Instant::now(), grace),
            agent.presence_explicit,
        )
    };
    let current = core.state().status.presence.clone();
    let Some(want) = auto_presence(kind, listening, explicit, &current) else {
        return;
    };
    match core.session() {
        Ok(handle) => {
            if let Err(e) = actions::set_presence(core, &handle, want) {
                tracing::warn!("could not update presence: {e}");
            }
        }
        Err(_) => {
            core.state().status.presence = want;
            core.emit(agent_chat_proto::ipc::Event::Status(core.status()));
        }
    }
}

/// Someone acted for this account (sent a message, typed). On an agent's
/// account that is the agent being there.
pub fn note_activity(core: &Core) {
    core.agent().listening.touch(Instant::now());
    core.agent_wake.notify_one();
}

/// An agent consumer did something (read, waited, replied).
pub fn note_consumer(core: &Core) {
    core.agent().listening.touch(Instant::now());
    adopt_agent_kind(core);
    core.agent_wake.notify_one();
}

/// The first agent consumer makes a profile of unknown kind an agent's.
fn adopt_agent_kind(core: &Core) {
    if core.config.lock().unwrap().settings.account_kind.is_some() {
        return;
    }
    if let Err(e) = set_account_kind(core, AccountKind::Agent) {
        tracing::warn!("could not mark this profile as an agent's: {e}");
    }
}

/// Save the account's kind and announce the new identity (its caps hash).
pub fn set_account_kind(core: &Core, kind: AccountKind) -> Result<(), IpcError> {
    let mut cfg = core.config.lock().unwrap().clone();
    if cfg.settings.account_kind == Some(kind) {
        return Ok(());
    }
    cfg.settings.account_kind = Some(kind);
    cfg.save(&core.profile.config_path())
        .map_err(|e| IpcError::new(ErrorCode::Internal, format!("{e:#}")))?;
    *core.config.lock().unwrap() = cfg;
    tracing::info!("this profile is used by {} now", kind.as_str());
    // The new identity goes out with our next presence; make sure there is one.
    let current = core.state().status.presence.clone();
    let grace = away_after(core);
    let (listening, explicit) = {
        let agent = core.agent();
        (
            agent.listening.is_listening(Instant::now(), grace),
            agent.presence_explicit,
        )
    };
    let next = auto_presence(kind, listening, explicit, &current).unwrap_or(current);
    if let Ok(handle) = core.session() {
        actions::set_presence(core, &handle, next)?;
    } else {
        core.state().status.presence = next;
        core.emit(agent_chat_proto::ipc::Event::Status(core.status()));
    }
    core.agent_wake.notify_one();
    Ok(())
}

/// Presence chosen by hand: automatic presence stays out of the way.
pub fn set_presence_explicitly(core: &Core, info: PresenceInfo) -> Result<(), IpcError> {
    core.agent().presence_explicit = true;
    match core.session() {
        Ok(handle) => actions::set_presence(core, &handle, info),
        // Remember it for the next login.
        Err(_) => {
            core.state().status.presence = info;
            Ok(())
        }
    }
}

/// `agent-chat presence auto`: hand presence back to the daemon.
pub fn auto_presence_again(core: &Core) {
    core.agent().presence_explicit = false;
    refresh_presence(core);
}

/// Held while a `wait` request blocks: the agent is listening.
pub struct WaitGuard<'a>(&'a Core);

impl<'a> WaitGuard<'a> {
    pub fn new(core: &'a Core) -> Self {
        core.agent().listening.begin_wait(Instant::now());
        adopt_agent_kind(core);
        core.agent_wake.notify_one();
        Self(core)
    }
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.0.agent().listening.end_wait(Instant::now());
        self.0.agent_wake.notify_one();
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
    if auto_stop {
        // Only agents (CLI, MCP) ask for this; the GUI handles its own.
        note_activity(core);
    }
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

/// The daemon's timer: ends typing that timed out and keeps an agent's
/// presence in line with whether anything is listening.
pub async fn run(core: Arc<Core>) {
    loop {
        let grace = away_after(&core);
        let next = core.agent().next_deadline(Instant::now(), grace);
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
    refresh_presence(core);
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

    const GRACE: Duration = Duration::from_secs(300);

    #[test]
    fn listening_while_waiting_and_for_a_while_after() {
        let mut l = Listening::default();
        let t0 = Instant::now();
        assert!(!l.is_listening(t0, GRACE), "nobody ever read");
        assert_eq!(l.next_change(t0, GRACE), None);
        l.begin_wait(t0);
        // Blocked in wait: listening however long it takes.
        assert!(l.is_listening(t0 + Duration::from_secs(3600), GRACE));
        assert_eq!(l.next_change(t0, GRACE), None);
        let t1 = t0 + Duration::from_secs(3600);
        l.end_wait(t1);
        assert!(
            l.is_listening(t1 + Duration::from_secs(299), GRACE),
            "working on a reply"
        );
        assert_eq!(l.next_change(t1, GRACE), Some(t1 + GRACE));
        assert!(!l.is_listening(t1 + GRACE, GRACE));
        assert_eq!(l.next_change(t1 + GRACE, GRACE), None);
        // Two waits at once (wait + MCP): listening until both end.
        l.begin_wait(t1);
        l.begin_wait(t1);
        l.end_wait(t1);
        assert!(l.is_listening(t1 + Duration::from_secs(10_000), GRACE));
        l.end_wait(t1);
        l.end_wait(t1); // never below zero
        assert!(!l.is_listening(t1 + GRACE, GRACE));
        // Any other consumer activity counts too.
        let t2 = t1 + Duration::from_secs(10_000);
        l.touch(t2);
        assert!(l.is_listening(t2 + Duration::from_secs(1), GRACE));
    }

    #[test]
    fn agent_presence_follows_listening_unless_set_by_hand() {
        let listening = PresenceInfo::new(Show::Available, Some(LISTENING_STATUS.into()));
        let away = PresenceInfo::new(Show::Away, Some(NOT_LISTENING_STATUS.into()));
        let fresh = PresenceInfo::default();
        assert_eq!(
            auto_presence(AccountKind::Agent, true, false, &fresh),
            Some(listening.clone())
        );
        assert_eq!(
            auto_presence(AccountKind::Agent, false, false, &fresh),
            Some(away.clone())
        );
        assert_eq!(
            auto_presence(AccountKind::Agent, false, false, &away),
            None,
            "no change, no presence"
        );
        assert_eq!(
            auto_presence(AccountKind::Agent, true, false, &away),
            Some(listening.clone())
        );
        // Set by hand: never overridden.
        let busy = PresenceInfo::new(Show::Dnd, Some("deploying".into()));
        assert_eq!(auto_presence(AccountKind::Agent, false, true, &busy), None);
        assert_eq!(auto_presence(AccountKind::Agent, true, true, &busy), None);
        // People keep whatever they chose…
        assert_eq!(auto_presence(AccountKind::Person, false, false, &busy), None);
        assert_eq!(auto_presence(AccountKind::Person, true, false, &fresh), None);
        // …but lose an agent status left over from before.
        assert_eq!(auto_presence(AccountKind::Person, false, false, &away), Some(fresh));
    }

    #[test]
    fn next_deadline_is_the_earliest() {
        let mut a = AgentState::default();
        let t0 = Instant::now();
        assert_eq!(a.next_deadline(t0, GRACE), None);
        a.listening.touch(t0);
        assert_eq!(a.next_deadline(t0, GRACE), Some(t0 + GRACE));
        a.typing.start("bob@example.org", t0, T);
        assert_eq!(a.next_deadline(t0, GRACE), Some(t0 + T));
    }

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
