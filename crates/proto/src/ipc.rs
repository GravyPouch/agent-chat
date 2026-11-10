//! Local IPC protocol between the daemon and its clients (CLI, GUI).
//!
//! Transport: a Unix domain socket carrying newline-delimited JSON.
//! Clients send [`ClientFrame`]s; the daemon answers each with a
//! [`ServerFrame::Response`] carrying the same `id`, and pushes
//! [`ServerFrame::Event`]s to connections that sent [`Request::Subscribe`].

use serde::{Deserialize, Serialize};

use crate::model::{
    Contact, Delivery, EncryptionProtocol, Filter, Message, PresenceInfo, RoomInvite, Show,
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
    /// Join a room (and remember it for autojoin). Result: `model::Room`.
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
    /// Change our presence. Result: `null`.
    SetPresence {
        show: Show,
        #[serde(default)]
        status: Option<String>,
    },
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
    /// Start receiving [`Event`]s on this connection. Result: `null`.
    Subscribe,
    /// Stop the daemon. Result: `null`.
    Shutdown,
}

fn default_history_limit() -> u32 {
    50
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
    Presence { jid: String, presence: PresenceInfo },
    ContactsChanged,
    SubscriptionRequest(SubscriptionRequest),
    RoomsChanged,
    RoomSubject { room: String, subject: String },
    RoomInvite(RoomInvite),
    DevicesChanged {
        jid: String,
        protocol: EncryptionProtocol,
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
        let f: ClientFrame =
            serde_json::from_str(r#"{"id":3,"method":"wait","params":{}}"#).unwrap();
        match f.request {
            Request::Wait {
                delivery,
                filter,
                timeout_secs,
                require_trusted,
            } => {
                assert_eq!(delivery.consumer, "default");
                assert_eq!(filter, Filter::default());
                assert_eq!(timeout_secs, None);
                assert!(!require_trusted);
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
        ];
        for f in frames {
            let json = serde_json::to_string(&f).unwrap();
            let back: ServerFrame = serde_json::from_str(&json).unwrap();
            assert_eq!(back, f);
        }
    }
}
