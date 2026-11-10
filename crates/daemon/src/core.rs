//! Shared daemon state.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use agent_chat_proto::Profile;
use agent_chat_proto::ipc::{ErrorCode, Event, IpcError};
use agent_chat_proto::model::{
    ConnectionState, Contact, Message, PresenceInfo, Room, Show, StatusInfo,
};
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
}

#[derive(Debug)]
pub struct State {
    pub status: StatusInfo,
    pub roster: BTreeMap<String, RosterEntry>,
    /// bare JID → resource → presence
    pub presence: HashMap<String, HashMap<String, PresenceInfo>>,
    pub pending_requests: BTreeSet<String>,
    /// lower-cased room JID → runtime state
    pub rooms: HashMap<String, RoomRuntime>,
    /// MAM query id → archive JID that must answer it
    pub mam_queries: HashMap<String, String>,
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
            daemon_pid: std::process::id(),
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
                rooms: HashMap::new(),
                mam_queries: HashMap::new(),
            }),
            session: Mutex::new(None),
            omemo: Mutex::new(None),
            shutdown: Notify::new(),
        })
    }

    pub fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap()
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Store a message and announce it if it's new.
    pub fn store_message(&self, m: &NewMessage) -> Result<Option<Message>> {
        let stored = self.store().insert_message(m)?;
        if let Some(msg) = &stored {
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
        let status = {
            let mut st = self.state();
            st.status.state = state;
            if error.is_some() {
                st.status.last_error = error;
            }
            if state != ConnectionState::Online && state != ConnectionState::Reconnecting {
                st.status.bound_jid = None;
            }
            st.status.clone()
        };
        self.emit(Event::Status(status));
    }

    pub fn status(&self) -> StatusInfo {
        self.state().status.clone()
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
        let st = self.state();
        st.roster
            .iter()
            .map(|(jid, e)| Contact {
                jid: jid.clone(),
                name: e.name.clone(),
                subscription: e.subscription.clone(),
                pending_out: e.pending_out,
                groups: e.groups.clone(),
                presence: best_presence(st.presence.get(jid)),
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
                    occupants: rt
                        .map(|r| r.occupants.iter().cloned().collect())
                        .unwrap_or_default(),
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

    pub fn room_nick(&self, bare: &str) -> Option<String> {
        self.state()
            .rooms
            .get(&bare.to_lowercase())
            .map(|r| r.nick.clone())
    }
}

pub fn best_presence(resources: Option<&HashMap<String, PresenceInfo>>) -> PresenceInfo {
    resources
        .and_then(|r| r.values().max_by_key(|p| p.show.rank()).cloned())
        .unwrap_or(PresenceInfo {
            show: Show::Unavailable,
            status: None,
        })
}
