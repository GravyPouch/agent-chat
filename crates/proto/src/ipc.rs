//! Local IPC protocol between the daemon and its clients (CLI, GUI).
//!
//! Transport: a Unix domain socket carrying newline-delimited JSON.
//! Clients send [`ClientFrame`]s; the daemon answers each with a
//! [`ServerFrame::Response`] carrying the same `id`, and pushes
//! [`ServerFrame::Event`]s to connections that sent [`Request::Subscribe`].

use serde::{Deserialize, Serialize};

use crate::model::{
    AccountKind, ChatState, Contact, Delivery, EncryptionProtocol, Filter, Message, PresenceInfo, RoomInvite, Show,
    StatusInfo, SubscriptionRequest,
};

/// How `send` should treat end-to-end encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionMode {
    /// Encrypt if the recipient publishes OMEMO devices, otherwise plaintext.
    #[default]
    Auto,
    /// Fail rather than send plaintext.
    Require,
    /// Never encrypt.
    Plaintext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Daemon and connection status. Result: [`StatusInfo`].
    Status,
    /// Configure (or replace) the account and reconnect. Result: [`StatusInfo`].
    SetAccount(AccountParams),
    /// Send a message. Result: [`Message`].
    Send {
        to: String,
        body: String,
        /// Force groupchat (`Some(true)`) or chat (`Some(false)`); `None` auto-detects
        /// joined rooms.
        #[serde(default)]
        room: Option<bool>,
        #[serde(default)]
        encryption: EncryptionMode,
    },
    /// Unread messages for a consumer. Result: `Vec<Message>`.
    Inbox {
        #[serde(default)]
        delivery: Delivery,
        #[serde(default)]
        filter: Filter,
        /// Don't mark returned messages as delivered.
        #[serde(default)]
        peek: bool,
        #[serde(default)]
        limit: Option<u32>,
        /// Skip messages from untrusted OMEMO devices (they stay unread).
        #[serde(default)]
        require_trusted: bool,
        /// Don't tell the senders we're typing (see `settings.agent_typing`).
        #[serde(default, skip_serializing_if = "is_false")]
        no_typing: bool,
    },
    /// Block until the next unread message. Result: `Option<Message>` (`null` on timeout).
    Wait {
        #[serde(default)]
        delivery: Delivery,
        #[serde(default)]
        filter: Filter,
        /// Seconds; `None` waits forever.
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        require_trusted: bool,
        /// Don't tell the sender we're typing (`listen`, which only watches).
        #[serde(default, skip_serializing_if = "is_false")]
        no_typing: bool,
    },
    /// Stored history of a conversation, oldest first. Result: `Vec<Message>`.
    History {
        conversation: String,
        #[serde(default = "default_history_limit")]
        limit: u32,
        /// Only messages with a local id lower than this.
        #[serde(default)]
        before_id: Option<i64>,
    },
    /// Result: `Vec<model::ConversationSummary>`.
    Conversations,
    /// Mark a conversation read for the GUI. Result: `null`.
    MarkRead { conversation: String, up_to_id: i64 },
    /// Result: [`ContactList`].
    Contacts,
    /// Add a contact and request their presence. Result: `null`.
    ContactAdd {
        jid: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Result: `null`.
    ContactRemove { jid: String },
    /// Approve an incoming subscription request. Result: `null`.
    ContactApprove { jid: String },
    /// Deny an incoming subscription request. Result: `null`.
    ContactDeny { jid: String },
    /// Result: `Vec<Room>`.
    Rooms,
    /// Create a group chat with a readable generated address (e.g.
    /// `quiet-otter-42@conference.example.org`) on the account's group chat
    /// service, join it and invite people. Result: `model::Room`.
    RoomCreate {
        /// Display name; the address is made from it. Default: generated.
        #[serde(default)]
        name: Option<String>,
        /// Members-only and end-to-end encrypted (the default).
        #[serde(default = "yes")]
        private: bool,
        /// Bare JIDs to invite (they become members).
        #[serde(default)]
        invite: Vec<String>,
        #[serde(default)]
        nick: Option<String>,
    },
    /// The account's group chat (MUC) service, e.g. `conference.example.org`.
    /// Result: `{"service": "..."}`.
    RoomService,
    /// Join a room (and remember it for autojoin). A bare name (no `@`)
    /// means a room on the account's group chat service. Result: `model::Room`.
    RoomJoin {
        jid: String,
        #[serde(default)]
        nick: Option<String>,
        #[serde(default)]
        password: Option<String>,
        /// If the room doesn't exist yet, create it members-only and
        /// non-anonymous so messages can be end-to-end encrypted.
        #[serde(default)]
        private: bool,
    },
    /// Invite someone to a room (and make them a member). Result: `null`.
    RoomInvite {
        room: String,
        jid: String,
        #[serde(default)]
        reason: Option<String>,
    },
    /// Pending invitations. Result: `Vec<model::RoomInvite>`.
    RoomInvites,
    /// Forget a pending invitation. Result: `null`.
    RoomInviteDecline { room: String },
    /// Leave a room and forget it. Result: `null`.
    RoomLeave { jid: String },
    /// Change our presence. Agents keep it until [`Request::AutoPresence`].
    /// Result: `null`.
    SetPresence {
        show: Show,
        #[serde(default)]
        status: Option<String>,
    },
    /// Let the daemon manage presence again (agents: "Listening" while a
    /// consumer waits, away when nobody has for a while). Result: `null`.
    AutoPresence,
    /// Say whether a person or an agent uses this account. Result: [`StatusInfo`].
    SetAccountKind { kind: AccountKind },
    /// Identify the client on this connection: `cli`, `mcp` or `gui`. An
    /// MCP session makes an account of unknown kind an agent's. Result: `null`.
    Hello { client: String },
    /// OMEMO devices of a JID (or our own when `None`). Result: `Vec<model::DeviceInfo>`.
    OmemoDevices {
        #[serde(default)]
        jid: Option<String>,
    },
    /// Set the trust of a device identified by fingerprint. Result: `model::DeviceInfo`.
    OmemoSetTrust {
        jid: String,
        fingerprint: String,
        /// `true` = verified, `false` = untrusted.
        trusted: bool,
    },
    /// Send an XEP-0085 chat state (e.g. "composing") to a contact or a
    /// joined room. Result: `null`.
    SendChatState {
        to: String,
        state: ChatState,
        /// After "composing", send "active" by itself if no message follows
        /// within the typing timeout (`settings.agent_typing_timeout_secs`).
        #[serde(default, skip_serializing_if = "is_false")]
        auto_stop: bool,
    },
    /// Start receiving [`Event`]s on this connection. Result: `null`.
    Subscribe,
    /// Stop the daemon. Result: `null`.
    Shutdown,
}

fn default_history_limit() -> u32 {
    50
}

fn is_false(b: &bool) -> bool {
    !b
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountParams {
    pub jid: String,
    pub password: String,
    /// Connect to this host instead of resolving SRV records for the JID's domain.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Allow unencrypted TCP. Only honoured for localhost (development).
    #[serde(default)]
    pub insecure_plaintext: bool,
    /// Where to keep the password.
    #[serde(default)]
    pub password_storage: PasswordStorage,
    /// Who uses the account; `None` keeps what the profile says (or lets
    /// the first agent consumer decide).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<AccountKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PasswordStorage {
    /// The OS keychain (macOS Keychain, Secret Service, Windows Credential Manager).
    #[default]
    Keychain,
    /// Plaintext in the profile's `config.toml` (mode 0600). For headless/dev setups.
    ConfigFile,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactList {
    pub contacts: Vec<Contact>,
    pub pending_requests: Vec<SubscriptionRequest>,
}

/// Something that happened, pushed to subscribed connections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// A message was stored (inbound or outbound).
    Message(Message),
    Status(StatusInfo),
    Presence {
        jid: String,
        presence: PresenceInfo,
    },
    ContactsChanged,
    SubscriptionRequest(SubscriptionRequest),
    RoomsChanged,
    RoomSubject {
        room: String,
        subject: String,
    },
    RoomInvite(RoomInvite),
    DevicesChanged {
        jid: String,
        protocol: EncryptionProtocol,
    },
    /// XEP-0085: someone's chat state, e.g. typing. In a 1:1 chat `jid` is
    /// the contact's bare JID; in a room it is the room and `nick` says who.
    ChatState {
        jid: String,
        state: ChatState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        nick: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    NoAccount,
    NotConnected,
    NotFound,
    EncryptionUnavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub code: ErrorCode,
    pub message: String,
}

impl IpcError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for IpcError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Response {
        id: u64,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        result: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        error: Option<IpcError>,
    },
    Event {
        event: Event,
    },
}

// Re-exported so users of the protocol only need this module.
pub use crate::model::ConnectionState;

/// Convenience alias for typed results in the daemon.
pub type IpcResult<T> = Result<T, IpcError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_frame_round_trip() {
        let frame = ClientFrame {
            id: 7,
            request: Request::Send {
                to: "bob@example.org".into(),
                body: "hi".into(),
                room: None,
                encryption: EncryptionMode::Auto,
            },
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""method":"send""#), "{json}");
        let back: ClientFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn chat_state_wire_format() {
        let json = serde_json::to_string(&Event::ChatState {
            jid: "bob@example.org".into(),
            state: ChatState::Paused,
            nick: None,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"chat_state","data":{"jid":"bob@example.org","state":"paused"}}"#
        );
        // In a room the occupant's nick comes along.
        let room: Event = serde_json::from_str(
            r#"{"type":"chat_state","data":{"jid":"team@conf.example.org","state":"composing","nick":"bob"}}"#,
        )
        .unwrap();
        assert_eq!(
            room,
            Event::ChatState {
                jid: "team@conf.example.org".into(),
                state: ChatState::Composing,
                nick: Some("bob".into())
            }
        );
        let f: ClientFrame = serde_json::from_str(
            r#"{"id":4,"method":"send_chat_state","params":{"to":"bob@example.org","state":"composing"}}"#,
        )
        .unwrap();
        assert_eq!(
            f.request,
            Request::SendChatState {
                to: "bob@example.org".into(),
                state: ChatState::Composing,
                auto_stop: false,
            }
        );
        // Optional flags stay off the wire unless set.
        let json = serde_json::to_string(&ClientFrame {
            id: 5,
            request: Request::SendChatState {
                to: "bob@example.org".into(),
                state: ChatState::Composing,
                auto_stop: true,
            },
        })
        .unwrap();
        assert!(json.contains(r#""auto_stop":true"#), "{json}");
    }

    #[test]
    fn no_typing_is_optional_and_omitted_when_off() {
        let f: ClientFrame = serde_json::from_str(r#"{"id":6,"method":"inbox","params":{"peek":false}}"#).unwrap();
        assert!(matches!(f.request, Request::Inbox { no_typing: false, .. }));
        let wait = Request::Wait {
            delivery: Delivery::default(),
            filter: Filter::default(),
            timeout_secs: None,
            require_trusted: false,
            no_typing: false,
        };
        let json = serde_json::to_string(&wait).unwrap();
        assert!(!json.contains("no_typing"), "{json}");
        let json = serde_json::to_string(&Request::Wait {
            delivery: Delivery::default(),
            filter: Filter::default(),
            timeout_secs: None,
            require_trusted: false,
            no_typing: true,
        })
        .unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::Wait { no_typing: true, .. }));
    }

    #[test]
    fn room_create_defaults_to_private() {
        let f: ClientFrame = serde_json::from_str(r#"{"id":11,"method":"room_create","params":{}}"#).unwrap();
        assert_eq!(
            f.request,
            Request::RoomCreate {
                name: None,
                private: true,
                invite: vec![],
                nick: None
            }
        );
        let f: ClientFrame = serde_json::from_str(
            r#"{"id":12,"method":"room_create","params":{"name":"Release planning","private":false,"invite":["bob@example.org"]}}"#,
        )
        .unwrap();
        assert!(matches!(
            f.request,
            Request::RoomCreate { private: false, ref invite, .. } if invite == &["bob@example.org".to_owned()]
        ));
    }

    #[test]
    fn agent_requests() {
        let f: ClientFrame =
            serde_json::from_str(r#"{"id":8,"method":"set_account_kind","params":{"kind":"agent"}}"#).unwrap();
        assert_eq!(
            f.request,
            Request::SetAccountKind {
                kind: AccountKind::Agent
            }
        );
        let f: ClientFrame = serde_json::from_str(r#"{"id":9,"method":"auto_presence"}"#).unwrap();
        assert_eq!(f.request, Request::AutoPresence);
        let f: ClientFrame = serde_json::from_str(r#"{"id":10,"method":"hello","params":{"client":"mcp"}}"#).unwrap();
        assert_eq!(f.request, Request::Hello { client: "mcp".into() });
        // Account params from older clients have no kind.
        let p: AccountParams = serde_json::from_str(r#"{"jid":"a@b","password":"x"}"#).unwrap();
        assert_eq!(p.kind, None);
    }

    #[test]
    fn unit_request_round_trip() {
        let frame = ClientFrame {
            id: 1,
            request: Request::Status,
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: ClientFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back, frame);
        // Hand-written frames without params must parse too.
        let back: ClientFrame = serde_json::from_str(r#"{"id":2,"method":"status"}"#).unwrap();
        assert_eq!(back.request, Request::Status);
    }

    #[test]
    fn wait_defaults() {
        let f: ClientFrame = serde_json::from_str(r#"{"id":3,"method":"wait","params":{}}"#).unwrap();
        match f.request {
            Request::Wait {
                delivery,
                filter,
                timeout_secs,
                require_trusted,
                no_typing,
            } => {
                assert_eq!(delivery.consumer, "default");
                assert_eq!(filter, Filter::default());
                assert_eq!(timeout_secs, None);
                assert!(!require_trusted);
                assert!(!no_typing);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn server_frames_round_trip() {
        let frames = vec![
            ServerFrame::Response {
                id: 1,
                result: Some(serde_json::json!({"a": 1})),
                error: None,
            },
            ServerFrame::Response {
                id: 2,
                result: None,
                error: Some(IpcError::new(ErrorCode::NotConnected, "offline")),
            },
            ServerFrame::Event {
                event: Event::ContactsChanged,
            },
            ServerFrame::Event {
                event: Event::ChatState {
                    jid: "bob@example.org".into(),
                    state: ChatState::Composing,
                    nick: None,
                },
            },
        ];
        for f in frames {
            let json = serde_json::to_string(&f).unwrap();
            let back: ServerFrame = serde_json::from_str(&json).unwrap();
            assert_eq!(back, f);
        }
    }
}
