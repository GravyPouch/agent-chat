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
    /// A shared photo or file; `body` is then its link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<Attachment>,
    /// The sender corrected the text (XEP-0308); `body` is the latest version.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
    /// The sender deleted the message for everyone (XEP-0424); `body` is empty.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retracted: bool,
    /// The local id of the message this one replies to (XEP-0461).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<i64>,
    /// Emoji reactions (XEP-0444).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
    /// Our message was seen by the recipient (XEP-0333; 1:1 chats).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub seen: bool,
}

/// One emoji on a message and who reacted with it: bare JIDs in 1:1 chats,
/// `room/nick` in group chats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub emoji: String,
    pub by: Vec<String>,
}

/// A photo or file shared in a message: an XEP-0363 upload, linked with
/// XEP-0066 or, encrypted, as an XEP-0454 `aesgcm://` link (the key is in
/// the link's fragment, so the server never sees the content).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub url: String,
    /// File name, from the link.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Encrypted on the server (an `aesgcm://` link).
    pub encrypted: bool,
    /// The local copy, once downloaded (`agent-chat download ID`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Attachment {
    /// The attachment a message body stands for: an `aesgcm://` link, or an
    /// http(s) link the message also carries as XEP-0066 out-of-band data.
    pub fn from_body(body: &str, oob_url: Option<&str>) -> Option<Self> {
        let url = body.trim();
        if url.is_empty() || url.contains(char::is_whitespace) {
            return None;
        }
        let encrypted = url.starts_with("aesgcm://");
        let linked = (url.starts_with("https://") || url.starts_with("http://")) && oob_url == Some(url);
        if !encrypted && !linked {
            return None;
        }
        let path = url.split(['#', '?']).next().unwrap_or(url);
        let name = percent_decode(path.rsplit('/').next().unwrap_or(""));
        let name = if name.is_empty() { "file".to_owned() } else { name };
        Some(Self {
            url: url.to_owned(),
            mime: mime_for(&name).map(str::to_owned),
            name,
            size: None,
            encrypted,
            path: None,
        })
    }

    pub fn is_image(&self) -> bool {
        self.mime.as_deref().is_some_and(|m| m.starts_with("image/"))
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok());
        if b[i] == b'%'
            && let Some(Ok(v)) = hex.map(|h| u8::from_str_radix(h, 16))
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The MIME type for a file name's extension (common types only).
pub fn mime_for(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" => "text/plain",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        _ => return None,
    })
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
    /// The client behind this presence is an AI agent (it advertises the
    /// XEP-0030 identity `client/bot`).
    #[serde(skip_serializing_if = "is_false", default)]
    pub is_agent: bool,
}

fn is_false(b: &bool) -> bool {
    !b
}

/// Status text of an agent while a consumer is waiting for messages.
pub const LISTENING_STATUS: &str = "Listening";
/// Status text of an agent nobody is reading for (presence `away`).
pub const NOT_LISTENING_STATUS: &str = "Not listening; messages will wait";

impl PresenceInfo {
    pub fn new(show: Show, status: Option<String>) -> Self {
        Self {
            show,
            status,
            is_agent: false,
        }
    }

    /// How an agent's presence reads to people: "listening", "not listening
    /// (messages will wait)", "away — reason", "offline"…
    pub fn agent_label(&self) -> String {
        let status = self.status.as_deref().filter(|s| !s.is_empty());
        match (self.show, status) {
            (Show::Unavailable, _) => "offline".into(),
            (Show::Available | Show::Chat, None | Some(LISTENING_STATUS)) => "listening".into(),
            (Show::Away | Show::Xa, Some(NOT_LISTENING_STATUS)) => "not listening (messages will wait)".into(),
            (Show::Available | Show::Chat, Some(s)) => format!("online — {s}"),
            (Show::Away | Show::Xa, None) => "away (messages will wait)".into(),
            (Show::Away | Show::Xa, Some(s)) => format!("away — {s}"),
            (Show::Dnd, None) => "busy".into(),
            (Show::Dnd, Some(s)) => format!("busy — {s}"),
        }
    }
}

/// Who uses an account: a person (desktop app, phone) or an AI agent. Agents
/// advertise the XEP-0030 identity `client/bot` and say in their presence
/// whether anything is listening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    #[default]
    Person,
    Agent,
}

impl AccountKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountKind::Person => "person",
            AccountKind::Agent => "agent",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "person" | "human" => AccountKind::Person,
            "agent" | "bot" => AccountKind::Agent,
            _ => return None,
        })
    }
}

/// XEP-0085 chat state notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatState {
    Active,
    Composing,
    Paused,
    Inactive,
    Gone,
}

impl ChatState {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatState::Active => "active",
            ChatState::Composing => "composing",
            ChatState::Paused => "paused",
            ChatState::Inactive => "inactive",
            ChatState::Gone => "gone",
        }
    }
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
    /// An AI agent (learned from its client's identity; remembered while
    /// it is offline).
    #[serde(default)]
    pub is_agent: bool,
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
    /// Members-only and non-anonymous: messages are OMEMO-encrypted.
    #[serde(default)]
    pub private: bool,
    /// The room's display name, if it has one besides its address.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub name: Option<String>,
}

/// An invitation to a group chat, waiting for us to join or ignore it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomInvite {
    pub room: String,
    /// Who invited us (bare JID), if known.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub password: Option<String>,
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
    /// Whether this account is used by a person or an agent.
    #[serde(default)]
    pub kind: AccountKind,
    /// Agents: is a consumer reading messages right now (or was, recently)?
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub listening: Option<bool>,
    pub daemon_pid: u32,
    /// The daemon's local setup page (instructions, MCP config).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub web_url: Option<String>,
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

#[cfg(test)]
mod tests {

    #[test]
    fn attachments_from_bodies() {
        let a = Attachment::from_body("aesgcm://up.example.org/upload/abc/My%20Photo.JPG#00112233", None).unwrap();
        assert!(a.encrypted);
        assert_eq!(a.name, "My Photo.JPG");
        assert_eq!(a.mime.as_deref(), Some("image/jpeg"));
        assert!(a.is_image());
        let url = "https://up.example.org/upload/abc/report.pdf";
        assert!(Attachment::from_body(url, None).is_none(), "a plain link needs OOB");
        let a = Attachment::from_body(url, Some(url)).unwrap();
        assert!(!a.encrypted && a.mime.as_deref() == Some("application/pdf"));
        assert!(Attachment::from_body("look: aesgcm://x/y", None).is_none());
    }

    use super::*;

    #[test]
    fn agent_labels() {
        let p = |show, status: Option<&str>| PresenceInfo::new(show, status.map(str::to_owned)).agent_label();
        assert_eq!(p(Show::Available, Some(LISTENING_STATUS)), "listening");
        assert_eq!(p(Show::Available, None), "listening");
        assert_eq!(
            p(Show::Away, Some(NOT_LISTENING_STATUS)),
            "not listening (messages will wait)"
        );
        assert_eq!(p(Show::Away, Some("lunch")), "away — lunch");
        assert_eq!(p(Show::Xa, None), "away (messages will wait)");
        assert_eq!(p(Show::Dnd, Some("deploying")), "busy — deploying");
        assert_eq!(p(Show::Available, Some("on call")), "online — on call");
        assert_eq!(p(Show::Unavailable, Some(LISTENING_STATUS)), "offline");
    }

    #[test]
    fn new_fields_are_optional_in_json() {
        // Older daemons and clients don't know about agents.
        let c: Contact = serde_json::from_str(
            r#"{"jid":"bob@example.org","subscription":"both","pending_out":false,"presence":{"show":"available"}}"#,
        )
        .unwrap();
        assert!(!c.is_agent);
        assert!(!c.presence.is_agent);
        let s: StatusInfo =
            serde_json::from_str(r#"{"profile":"default","state":"online","presence":{"show":"away"},"daemon_pid":1}"#)
                .unwrap();
        assert_eq!(s.kind, AccountKind::Person);
        assert_eq!(s.listening, None);
        // A person's presence stays as compact as before.
        let json = serde_json::to_string(&PresenceInfo::default()).unwrap();
        assert_eq!(json, r#"{"show":"available"}"#);
        let agent = PresenceInfo {
            is_agent: true,
            ..PresenceInfo::new(Show::Available, Some(LISTENING_STATUS.into()))
        };
        let json = serde_json::to_string(&agent).unwrap();
        assert_eq!(json, r#"{"show":"available","status":"Listening","is_agent":true}"#);
        assert_eq!(serde_json::from_str::<PresenceInfo>(&json).unwrap(), agent);
    }

    #[test]
    fn account_kind_names() {
        assert_eq!(AccountKind::parse("agent"), Some(AccountKind::Agent));
        assert_eq!(AccountKind::parse("bot"), Some(AccountKind::Agent));
        assert_eq!(AccountKind::parse("person"), Some(AccountKind::Person));
        assert_eq!(AccountKind::parse("robot"), None);
        assert_eq!(serde_json::to_string(&AccountKind::Agent).unwrap(), r#""agent""#);
    }
}
