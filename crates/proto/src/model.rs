//! Data types shared by the daemon, CLI and GUI.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What kind of conversation a message belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// One-to-one chat. `conversation` is the peer's bare JID.
    Chat,
    /// Group chat (MUC). `conversation` is the room's bare JID.
    Groupchat,
    /// Private message inside a MUC. `conversation` is `room@service/nick`.
    MucPrivate,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Chat => "chat",
            MessageKind::Groupchat => "groupchat",
            MessageKind::MucPrivate => "muc_private",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "chat" => MessageKind::Chat,
            "groupchat" => MessageKind::Groupchat,
            "muc_private" => MessageKind::MucPrivate,
            _ => return None,
        })
    }
}

/// How a message reached the local store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSource {
    /// Received live on the stream.
    Live,
    /// Copied from another of our own devices (XEP-0280 Message Carbons).
    Carbon,
    /// Fetched from a server archive (XEP-0313 MAM).
    Mam,
    /// Sent by this client.
    Local,
}

impl MessageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageSource::Live => "live",
            MessageSource::Carbon => "carbon",
            MessageSource::Mam => "mam",
            MessageSource::Local => "local",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "live" => MessageSource::Live,
            "carbon" => MessageSource::Carbon,
            "mam" => MessageSource::Mam,
            "local" => MessageSource::Local,
            _ => return None,
        })
    }
}

/// A stored chat message. This is the JSON object agents receive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Local, monotonically increasing id.
    pub id: i64,
    /// Where to reply: a bare JID for chats and rooms, `room/nick` for MUC private messages.
    pub conversation: String,
    pub kind: MessageKind,
    /// Sender: a bare JID for chats, `room/nick` for room messages.
    pub from: String,
    /// Sender's nickname in a room.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub nick: Option<String>,
    pub body: String,
    pub timestamp: DateTime<Utc>,
    /// True if the message was delivered late (offline storage, history, archive).
    pub delayed: bool,
    pub source: MessageSource,
    /// True if this account sent the message (from this or another device).
    pub own: bool,
    /// End-to-end encryption details, or `null` if the message was plaintext.
    #[serde(default)]
    pub encryption: Option<EncryptionInfo>,
}

/// Which OMEMO version protected a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionProtocol {
    /// XEP-0384 0.4+ (`urn:xmpp:omemo:2`).
    Omemo2,
    /// XEP-0384 0.3 (`eu.siacs.conversations.axolotl`).
    OmemoLegacy,
}

impl EncryptionProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            EncryptionProtocol::Omemo2 => "omemo2",
            EncryptionProtocol::OmemoLegacy => "omemo_legacy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "omemo2" => EncryptionProtocol::Omemo2,
            "omemo_legacy" => EncryptionProtocol::OmemoLegacy,
            _ => return None,
        })
    }
}

/// Trust level of the device that sent (or will receive) a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// Fingerprint manually verified.
    Verified,
    /// Trusted blindly (Blind Trust Before Verification).
    Trusted,
    /// Explicitly distrusted, or new device of a contact with verified devices.
    Untrusted,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Verified => "verified",
            Trust::Trusted => "trusted",
            Trust::Untrusted => "untrusted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "verified" => Trust::Verified,
            "trusted" => Trust::Trusted,
            "untrusted" => Trust::Untrusted,
            _ => return None,
        })
    }

    pub fn is_trusted(self) -> bool {
        matches!(self, Trust::Verified | Trust::Trusted)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptionInfo {
    pub protocol: EncryptionProtocol,
    /// OMEMO device id of the sender.
    pub sender_device: u32,
    pub trust: Trust,
    /// Set when the message could not be decrypted; `body` is then a placeholder.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// An OMEMO device of some account, with its trust state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub jid: String,
    pub protocol: EncryptionProtocol,
    pub device_id: u32,
    /// Hex fingerprint of the identity key, if known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub fingerprint: Option<String>,
    pub trust: Trust,
    /// True if this is one of our own devices on this daemon.
    pub this_device: bool,
}

/// Presence availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Show {
    #[default]
    Available,
    Chat,
    Away,
    Xa,
    Dnd,
    Unavailable,
}

impl Show {
    pub fn as_str(self) -> &'static str {
        match self {
            Show::Available => "available",
            Show::Chat => "chat",
            Show::Away => "away",
            Show::Xa => "xa",
            Show::Dnd => "dnd",
            Show::Unavailable => "unavailable",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "available" | "online" => Show::Available,
            "chat" => Show::Chat,
            "away" => Show::Away,
            "xa" => Show::Xa,
            "dnd" => Show::Dnd,
            "unavailable" | "offline" => Show::Unavailable,
            _ => return None,
        })
    }

    /// Ordering used to pick the "best" presence across resources.
    pub fn rank(self) -> u8 {
        match self {
            Show::Chat => 5,
            Show::Available => 4,
            Show::Away => 3,
            Show::Xa => 2,
            Show::Dnd => 1,
            Show::Unavailable => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PresenceInfo {
    pub show: Show,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub status: Option<String>,
}

/// A roster entry with its aggregated presence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    pub jid: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
    /// none | to | from | both
    pub subscription: String,
    /// True if we asked to see their presence and they haven't answered yet.
    pub pending_out: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    pub presence: PresenceInfo,
}

/// An incoming subscription request awaiting approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscriptionRequest {
    pub jid: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Room {
    pub jid: String,
    pub nick: String,
    pub autojoin: bool,
    pub joined: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub occupants: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub conversation: String,
    pub kind: MessageKind,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_message: Option<Message>,
    /// Unread count for the GUI (independent of agent consumers).
    pub unread: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    NoAccount,
    Connecting,
    Online,
    Reconnecting,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub account: Option<String>,
    pub state: ConnectionState,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bound_jid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_error: Option<String>,
    pub presence: PresenceInfo,
    pub daemon_pid: u32,
}

/// Restricts which messages `inbox`/`wait` return.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Filter {
    /// Only messages in this 1:1 conversation (bare JID).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from: Option<String>,
    /// Only messages in this room (bare JID).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub room: Option<String>,
}

impl Filter {
    pub fn matches(&self, m: &Message) -> bool {
        if let Some(from) = &self.from
            && !(m.kind != MessageKind::Groupchat && m.conversation.eq_ignore_ascii_case(from))
        {
            return false;
        }
        if let Some(room) = &self.room
            && !(m.kind == MessageKind::Groupchat && m.conversation.eq_ignore_ascii_case(room))
        {
            return false;
        }
        true
    }
}

/// Which consumer is reading, and how.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    /// Each consumer receives every inbound message exactly once.
    pub consumer: String,
    /// Also return messages this account sent.
    #[serde(default)]
    pub include_own: bool,
    /// When creating a new consumer, start from the beginning of history
    /// instead of "now".
    #[serde(default)]
    pub from_start: bool,
}

impl Default for Delivery {
    fn default() -> Self {
        Self {
            consumer: "default".into(),
            include_own: false,
            from_start: false,
        }
    }
}
