//! Shared daemon state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use agent_chat_proto::Profile;
use agent_chat_proto::ipc::{ErrorCode, Event, IpcError};
use agent_chat_proto::model::{AccountKind, ConnectionState, Contact, Message, PresenceInfo, Room, Show, StatusInfo};
use anyhow::Result;
use tokio::sync::{Notify, broadcast};
use xmpp_parsers::jid::Jid;

use crate::config::Config;
use crate::store::{NewMessage, Store};
use crate::xmpp::session::SessionHandle;

/// A roster entry as received from the server.
#[derive(Debug, Clone)]
pub struct RosterEntry {
    pub name: Option<String>,
    pub subscription: String,
    pub pending_out: bool,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RoomRuntime {
    pub nick: String,
    pub password: Option<String>,
    pub joined: bool,
    pub subject: Option<String>,
    pub occupants: BTreeSet<String>,
    /// Real JIDs of occupants when the room exposes them (non-anonymous rooms).
    pub occupant_jids: BTreeMap<String, String>,
    /// The room keeps a MAM archive.
    pub mam: bool,
    /// Why the last join attempt failed.
    pub error: Option<String>,
    /// Room features (XEP-0045 disco): real JIDs visible to everyone.
    pub nonanonymous: bool,
    /// Room features: only members may enter.
    pub membersonly: bool,
    /// Bare JIDs with an affiliation (owner/admin/member), when we may list them.
    pub members: BTreeSet<String>,
    /// If our join creates the room, configure it private (encrypted).
    pub create_private: bool,
    /// If our join creates the room, give it this display name.
    pub create_name: Option<String>,
    /// The room's display name (its disco#info identity), when it has one.
    pub name: Option<String>,
    /// Joined and set up (configured if we created it, features read).
    pub configured: bool,
}

impl RoomRuntime {
    /// Members-only and non-anonymous: the XEP-0384 prerequisite for OMEMO.
    pub fn is_private(&self) -> bool {
        self.nonanonymous && self.membersonly
    }
}

#[derive(Debug)]
pub struct State {
    pub status: StatusInfo,
    pub roster: BTreeMap<String, RosterEntry>,
    /// bare JID → resource → presence
    pub presence: HashMap<String, HashMap<String, PresenceInfo>>,
    pub pending_requests: BTreeSet<String>,
    pub pending_invites: Vec<agent_chat_proto::model::RoomInvite>,
    /// lower-cased room JID → runtime state
    pub rooms: HashMap<String, RoomRuntime>,
    /// MAM query id → archive JID that must answer it
    pub mam_queries: HashMap<String, String>,
    /// Bare JID → whether its client is an agent, as last learned (also
    /// remembered in the store while the contact is offline).
    pub contact_agents: HashMap<String, bool>,
    /// Caps hash (`algo:ver`) → is it an agent's client; verified hashes only.
    pub caps_agents: HashMap<String, bool>,
    /// disco#info queries in flight (caps keys or full JIDs).
    pub disco_pending: BTreeSet<String>,
    /// The account's group chat service, once discovered.
    pub muc_service: Option<String>,
    /// XEP-0363 upload service, once found.
    pub upload_service: Option<crate::media::UploadService>,
}

pub struct Core {
    pub profile: Profile,
    store: Mutex<Store>,
    pub config: Mutex<Config>,
    pub events: broadcast::Sender<Event>,
    state: Mutex<State>,
    session: Mutex<Option<SessionHandle>>,
    /// OMEMO engines, opened on first use.
    pub omemo: Mutex<Option<Arc<crate::omemo::Manager>>>,
    /// What agents reading through this daemon are doing (typing).
    agent: Mutex<crate::agent::AgentState>,
    /// Wakes the agent timer when its deadlines change.
    pub agent_wake: Notify,
    pub shutdown: Notify,
}

impl Core {
    pub fn new(profile: Profile, store: Store, config: Config) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        let status = StatusInfo {
            profile: profile.name.clone(),
            account: config.account.as_ref().map(|a| a.jid.clone()),
            state: if config.account.is_some() {
                ConnectionState::Disconnected
            } else {
                ConnectionState::NoAccount
            },
            bound_jid: None,
            last_error: None,
            presence: PresenceInfo::default(),
            kind: config.settings.account_kind.unwrap_or_default(),
            listening: None,
            daemon_pid: std::process::id(),
            web_url: None,
        };
        Arc::new(Self {
            profile,
            store: Mutex::new(store),
            config: Mutex::new(config),
            events,
            state: Mutex::new(State {
                status,
                roster: BTreeMap::new(),
                presence: HashMap::new(),
                pending_requests: BTreeSet::new(),
                pending_invites: Vec::new(),
                rooms: HashMap::new(),
                mam_queries: HashMap::new(),
                contact_agents: HashMap::new(),
                caps_agents: HashMap::new(),
                disco_pending: BTreeSet::new(),
                muc_service: None,
                upload_service: None,
            }),
            session: Mutex::new(None),
            omemo: Mutex::new(None),
            agent: Mutex::new(crate::agent::AgentState::default()),
            agent_wake: Notify::new(),
            shutdown: Notify::new(),
        })
    }

    pub fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap()
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn agent(&self) -> MutexGuard<'_, crate::agent::AgentState> {
        self.agent.lock().unwrap()
    }

    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Store a message and announce it if it's new.
    pub fn store_message(&self, m: &NewMessage) -> Result<Option<Message>> {
        let stored = self.store().insert_message(m)?;
        if let Some(msg) = &stored {
            if msg.own {
                crate::agent::on_own_message(self, msg);
            }
            self.emit(Event::Message(msg.clone()));
        }
        Ok(stored)
    }

    // ------------------------------------------------------------- session

    /// (Re)start the XMPP session from the current config.
    pub async fn restart_session(self: &Arc<Self>) -> Result<()> {
        self.stop_session().await;
        let acct = self.config.lock().unwrap().account.clone();
        let Some(acct) = acct else {
            self.set_state(ConnectionState::NoAccount, None);
            return Ok(());
        };
        {
            let mut st = self.state();
            st.status.account = Some(acct.jid.clone());
        }
        let password = match crate::config::resolve_password(&acct) {
            Ok(p) => p,
            Err(e) => {
                self.set_state(ConnectionState::Disconnected, Some(e.to_string()));
                return Err(e);
            }
        };
        self.set_state(ConnectionState::Connecting, None);
        let handle = crate::xmpp::session::start(self.clone(), &acct, password)?;
        *self.session.lock().unwrap() = Some(handle);
        Ok(())
    }

    pub async fn stop_session(&self) {
        let handle = self.session.lock().unwrap().take();
        if let Some(h) = handle {
            h.stop().await;
        }
    }

    /// The live session, for operations that need the network.
    pub fn session(&self) -> Result<SessionHandle, IpcError> {
        let state = self.state().status.state;
        match (&*self.session.lock().unwrap(), state) {
            (Some(h), ConnectionState::Online | ConnectionState::Reconnecting) => Ok(h.clone()),
            (_, ConnectionState::NoAccount) => Err(IpcError::new(
                ErrorCode::NoAccount,
                "no account configured; run `agent-chat account add JID`",
            )),
            _ => Err(IpcError::new(
                ErrorCode::NotConnected,
                "not connected to the XMPP server yet",
            )),
        }
    }

    fn set_state(&self, state: ConnectionState, error: Option<String>) {
        {
            let mut st = self.state();
            st.status.state = state;
            if error.is_some() {
                st.status.last_error = error;
            }
            if state != ConnectionState::Online && state != ConnectionState::Reconnecting {
                st.status.bound_jid = None;
            }
        }
        self.emit(Event::Status(self.status()));
    }

    pub fn status(&self) -> StatusInfo {
        let mut status = self.state().status.clone();
        status.kind = self.account_kind();
        status.listening = (status.kind == AccountKind::Agent).then(|| crate::agent::is_listening(self));
        status
    }

    /// Who uses this account (a person unless configured or detected).
    pub fn account_kind(&self) -> AccountKind {
        self.config.lock().unwrap().settings.account_kind.unwrap_or_default()
    }

    /// Is `bare` an agent? What we last learned, else what the store remembers.
    pub fn contact_is_agent(&self, bare: &str) -> bool {
        let key = bare.to_lowercase();
        if let Some(known) = self.state().contact_agents.get(&key) {
            return *known;
        }
        let remembered = self
            .store()
            .kv_get(&format!("contact-kind:{key}"))
            .ok()
            .flatten()
            .is_some_and(|v| v == "agent");
        self.state().contact_agents.insert(key, remembered);
        remembered
    }

    /// Called by the session actor when a fresh (non-resumed) stream is up.
    pub fn on_online(self: &Arc<Self>, handle: &SessionHandle, bound_jid: Jid) {
        {
            let mut st = self.state();
            st.status.bound_jid = Some(bound_jid.to_string());
            st.status.last_error = None;
            st.presence.clear();
            st.mam_queries.clear();
            for room in st.rooms.values_mut() {
                room.joined = false;
                room.occupants.clear();
                room.occupant_jids.clear();
            }
        }
        self.set_state(ConnectionState::Online, None);
        let core = self.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::xmpp::actions::after_login(&core, &handle).await {
                tracing::warn!("post-login setup failed: {e:#}");
            }
        });
    }

    pub fn on_suspended(&self) {
        self.set_state(ConnectionState::Reconnecting, None);
    }

    pub fn on_resumed(&self) {
        self.set_state(ConnectionState::Online, None);
    }

    pub fn on_disconnected(&self, reason: &str) {
        let has_account = self.config.lock().unwrap().account.is_some();
        self.set_state(
            if has_account {
                ConnectionState::Disconnected
            } else {
                ConnectionState::NoAccount
            },
            Some(reason.to_owned()),
        );
    }

    // ------------------------------------------------------- views for IPC

    pub fn contacts(&self) -> Vec<Contact> {
        let jids: Vec<String> = self.state().roster.keys().cloned().collect();
        let agents: HashMap<String, bool> = jids.iter().map(|j| (j.clone(), self.contact_is_agent(j))).collect();
        let st = self.state();
        st.roster
            .iter()
            .map(|(jid, e)| Contact {
                jid: jid.clone(),
                name: e.name.clone(),
                subscription: e.subscription.clone(),
                pending_out: e.pending_out,
                groups: e.groups.clone(),
                presence: best_presence(st.presence.get(jid), agents.get(jid).copied().unwrap_or(false)),
                is_agent: agents.get(jid).copied().unwrap_or(false),
            })
            .collect()
    }

    pub fn rooms(&self) -> Result<Vec<Room>> {
        let stored = self.store().rooms()?;
        let st = self.state();
        Ok(stored
            .into_iter()
            .map(|(jid, nick, _pw, autojoin)| {
                let rt = st.rooms.get(&jid.to_lowercase());
                Room {
                    nick: rt.map(|r| r.nick.clone()).unwrap_or(nick),
                    autojoin,
                    joined: rt.is_some_and(|r| r.joined),
                    subject: rt.and_then(|r| r.subject.clone()),
                    occupants: rt.map(|r| r.occupants.iter().cloned().collect()).unwrap_or_default(),
                    private: rt.is_some_and(|r| r.is_private()),
                    name: rt.and_then(|r| r.name.clone()),
                    jid,
                }
            })
            .collect())
    }

    /// Is `jid` (bare) a room we have joined or are joining?
    pub fn is_room(&self, bare: &str) -> bool {
        self.state().rooms.contains_key(&bare.to_lowercase())
    }

    /// Our nick in a room we are currently joined to.
    pub fn joined_room_nick(&self, bare: &str) -> Option<String> {
        self.state()
            .rooms
            .get(&bare.to_lowercase())
            .filter(|r| r.joined)
            .map(|r| r.nick.clone())
    }

    /// Everyone whose devices a private-room message must be encrypted for:
    /// present occupants' real JIDs plus the affiliation list.
    pub fn room_members(&self, bare: &str) -> Vec<String> {
        let st = self.state();
        let Some(rt) = st.rooms.get(&bare.to_lowercase()) else {
            return vec![];
        };
        let mut all: BTreeSet<String> = rt.members.clone();
        all.extend(rt.occupant_jids.values().cloned());
        all.into_iter().collect()
    }

    /// Real bare JID behind a nick, in rooms that expose it.
    pub fn room_real_jid(&self, room: &str, nick: &str) -> Option<String> {
        self.state()
            .rooms
            .get(&room.to_lowercase())
            .and_then(|r| r.occupant_jids.get(nick).cloned())
    }

    pub fn room_is_private(&self, bare: &str) -> bool {
        self.state()
            .rooms
            .get(&bare.to_lowercase())
            .is_some_and(|r| r.is_private())
    }

    pub fn room_nick(&self, bare: &str) -> Option<String> {
        self.state().rooms.get(&bare.to_lowercase()).map(|r| r.nick.clone())
    }
}

/// The presence to show for a contact: its most available client. For an
/// agent (`agent_contact`, or any agent client online) that is its most
/// available agent client, so whether it listens isn't hidden or faked by
/// another client on the same account; with none online it is offline.
pub fn best_presence(resources: Option<&HashMap<String, PresenceInfo>>, agent_contact: bool) -> PresenceInfo {
    let offline = PresenceInfo::new(Show::Unavailable, None);
    let Some(resources) = resources else {
        return offline;
    };
    let agent = agent_contact || resources.values().any(|p| p.is_agent);
    resources
        .values()
        .filter(|p| p.is_agent || !agent)
        .max_by_key(|p| p.show.rank())
        .cloned()
        .unwrap_or(offline)
}

impl Core {
    /// What a contact's presence looks like now (see [`best_presence`]).
    pub fn contact_presence(&self, bare: &str) -> PresenceInfo {
        let agent = self.contact_is_agent(bare);
        best_presence(self.state().presence.get(bare), agent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_presence_prefers_agent_clients() {
        let mut r = HashMap::new();
        assert_eq!(best_presence(None, false).show, Show::Unavailable);
        assert_eq!(best_presence(Some(&r), false).show, Show::Unavailable);
        r.insert("phone".to_owned(), PresenceInfo::new(Show::Away, None));
        r.insert("laptop".to_owned(), PresenceInfo::new(Show::Available, None));
        assert_eq!(best_presence(Some(&r), false).show, Show::Available);
        // A known agent whose agent client is offline is offline, whatever
        // other clients of the account do.
        assert_eq!(best_presence(Some(&r), true).show, Show::Unavailable);
        // An agent on the same account: its presence is what counts.
        let mut agent = PresenceInfo::new(Show::Away, Some(agent_chat_proto::model::NOT_LISTENING_STATUS.into()));
        agent.is_agent = true;
        r.insert("agent".to_owned(), agent.clone());
        assert_eq!(best_presence(Some(&r), false), agent);
        assert_eq!(best_presence(Some(&r), true), agent);
    }
}
