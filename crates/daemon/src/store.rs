//! SQLite message store, consumer delivery tracking and sync state.

use std::path::Path;

use agent_chat_proto::model::{
    Attachment, ConversationSummary, Delivery, EncryptionInfo, EncryptionProtocol, Filter, Message, MessageKind,
    MessageSource, Reaction, Room, Trust,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE messages (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation  TEXT NOT NULL,
    kind          TEXT NOT NULL,
    sender        TEXT NOT NULL,
    nick          TEXT,
    body          TEXT NOT NULL,
    timestamp     TEXT NOT NULL,
    delayed       INTEGER NOT NULL,
    source        TEXT NOT NULL,
    own           INTEGER NOT NULL,
    archive       TEXT,
    stanza_id     TEXT,
    origin_id     TEXT,
    xmpp_id       TEXT,
    enc_protocol  TEXT,
    enc_device    INTEGER,
    enc_trust     TEXT,
    enc_error     TEXT
);
CREATE UNIQUE INDEX messages_stanza_id ON messages(archive, stanza_id) WHERE stanza_id IS NOT NULL;
CREATE UNIQUE INDEX messages_origin_id ON messages(conversation, sender, origin_id) WHERE origin_id IS NOT NULL;
CREATE INDEX messages_conversation ON messages(conversation, id);

CREATE TABLE consumers (
    name      TEXT PRIMARY KEY,
    start_id  INTEGER NOT NULL
);
CREATE TABLE deliveries (
    consumer    TEXT NOT NULL,
    message_id  INTEGER NOT NULL,
    PRIMARY KEY (consumer, message_id)
) WITHOUT ROWID;

CREATE TABLE read_markers (
    conversation  TEXT PRIMARY KEY,
    last_id       INTEGER NOT NULL
);

CREATE TABLE sync_state (
    archive         TEXT PRIMARY KEY,
    last_stanza_id  TEXT NOT NULL
);

CREATE TABLE rooms (
    jid       TEXT PRIMARY KEY,
    nick      TEXT NOT NULL,
    password  TEXT,
    autojoin  INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE kv (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);
"#,
    r#"
-- Deleted conversations: messages from before `before` (archive and offline
-- copies arriving later) are not stored again.
CREATE TABLE cleared (
    conversation  TEXT PRIMARY KEY COLLATE NOCASE,
    before        TEXT NOT NULL
);
"#,
    r#"
-- Shared photos and files (XEP-0363/0066/0454).
ALTER TABLE messages ADD COLUMN att_url TEXT;
ALTER TABLE messages ADD COLUMN att_name TEXT;
ALTER TABLE messages ADD COLUMN att_mime TEXT;
ALTER TABLE messages ADD COLUMN att_size INTEGER;
ALTER TABLE messages ADD COLUMN att_path TEXT;
"#,
    r#"
-- Corrections, retractions, replies, reactions and read markers.
ALTER TABLE messages ADD COLUMN edited INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN retracted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN deleted INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN reply_to INTEGER;
ALTER TABLE messages ADD COLUMN seen INTEGER NOT NULL DEFAULT 0;
CREATE TABLE reactions (
    message_id  INTEGER NOT NULL,
    sender      TEXT NOT NULL,
    emoji       TEXT NOT NULL,
    PRIMARY KEY (message_id, sender, emoji)
) WITHOUT ROWID;
-- Edits, retractions and reactions already applied (carbons and the archive
-- repeat them).
CREATE TABLE applied (
    key  TEXT PRIMARY KEY
) WITHOUT ROWID;
"#,
];

/// A message about to be stored, before it has a local id.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub conversation: String,
    pub kind: MessageKind,
    pub sender: String,
    pub nick: Option<String>,
    pub body: String,
    pub timestamp: DateTime<Utc>,
    pub delayed: bool,
    pub source: MessageSource,
    pub own: bool,
    /// The archive that assigned `stanza_id` (our bare JID, or the room's).
    pub archive: Option<String>,
    pub stanza_id: Option<String>,
    pub origin_id: Option<String>,
    pub xmpp_id: Option<String>,
    pub encryption: Option<EncryptionInfo>,
    pub attachment: Option<agent_chat_proto::model::Attachment>,
    /// Local id of the message this replies to.
    pub reply_to: Option<i64>,
}

/// Identifiers used to detect duplicates, without the (possibly encrypted) body.
#[derive(Debug, Clone, Default)]
pub struct DedupeKeys<'a> {
    pub conversation: &'a str,
    pub sender: &'a str,
    pub archive: Option<&'a str>,
    pub stanza_id: Option<&'a str>,
    pub origin_id: Option<&'a str>,
    pub xmpp_id: Option<&'a str>,
}

/// A saved room: JID, nickname, password, autojoin.
pub type RoomRow = (String, String, Option<String>, bool);

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(m)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    /// Raw connection, for other modules that keep their own tables (OMEMO).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ---------------------------------------------------------------- messages

    /// Find an already-stored message matching these identifiers.
    ///
    /// Call this *before* decrypting: replaying OMEMO ciphertext would fail or
    /// advance the ratchet.
    pub fn find_duplicate(&self, k: &DedupeKeys<'_>) -> Result<Option<i64>> {
        if let (Some(archive), Some(sid)) = (k.archive, k.stanza_id)
            && let Some(id) = self
                .conn
                .query_row(
                    "SELECT id FROM messages WHERE archive = ?1 AND stanza_id = ?2",
                    params![archive, sid],
                    |r| r.get(0),
                )
                .optional()?
        {
            return Ok(Some(id));
        }
        if let Some(oid) = k.origin_id
            && let Some(id) = self
                .conn
                .query_row(
                    "SELECT id FROM messages WHERE conversation = ?1 AND sender = ?2 AND origin_id = ?3",
                    params![k.conversation, k.sender, oid],
                    |r| r.get(0),
                )
                .optional()?
        {
            return Ok(Some(id));
        }
        if let Some(xid) = k.xmpp_id
            && let Some(id) = self
                .conn
                .query_row(
                    "SELECT id FROM messages WHERE conversation = ?1 AND sender = ?2 AND xmpp_id = ?3",
                    params![k.conversation, k.sender, xid],
                    |r| r.get(0),
                )
                .optional()?
        {
            return Ok(Some(id));
        }
        Ok(None)
    }

    /// Insert a message unless it's a duplicate. Returns the stored message if new.
    ///
    /// If a duplicate exists and lacks the archive id we now know, the id is
    /// recorded so later archive queries recognise it.
    pub fn insert_message(&self, m: &NewMessage) -> Result<Option<Message>> {
        let keys = DedupeKeys {
            conversation: &m.conversation,
            sender: &m.sender,
            archive: m.archive.as_deref(),
            stanza_id: m.stanza_id.as_deref(),
            origin_id: m.origin_id.as_deref(),
            xmpp_id: m.xmpp_id.as_deref(),
        };
        if self.cleared_after(&m.conversation, m.timestamp)? {
            return Ok(None);
        }
        if let Some(existing) = self.find_duplicate(&keys)? {
            if let (Some(archive), Some(sid)) = (&m.archive, &m.stanza_id) {
                self.conn.execute(
                    "UPDATE OR IGNORE messages SET archive = ?1, stanza_id = ?2
                     WHERE id = ?3 AND stanza_id IS NULL",
                    params![archive, sid, existing],
                )?;
            }
            return Ok(None);
        }
        let enc = m.encryption.as_ref();
        let att = m.attachment.as_ref();
        self.conn.execute(
            "INSERT INTO messages (conversation, kind, sender, nick, body, timestamp, delayed,
                source, own, archive, stanza_id, origin_id, xmpp_id,
                enc_protocol, enc_device, enc_trust, enc_error,
                att_url, att_name, att_mime, att_size, att_path, reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     ?18, ?19, ?20, ?21, ?22, ?23)",
            params![
                m.conversation,
                m.kind.as_str(),
                m.sender,
                m.nick,
                m.body,
                m.timestamp.to_rfc3339(),
                m.delayed,
                m.source.as_str(),
                m.own,
                m.archive,
                m.stanza_id,
                m.origin_id,
                m.xmpp_id,
                enc.map(|e| e.protocol.as_str()),
                enc.map(|e| e.sender_device),
                enc.map(|e| e.trust.as_str()),
                enc.and_then(|e| e.error.clone()),
                att.map(|a| a.url.clone()),
                att.map(|a| a.name.clone()),
                att.and_then(|a| a.mime.clone()),
                att.and_then(|a| a.size.map(|s| s as i64)),
                att.and_then(|a| a.path.clone()),
                m.reply_to,
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        self.get_message(id)
    }

    /// Whether the conversation was deleted after `timestamp`.
    fn cleared_after(&self, conversation: &str, timestamp: DateTime<Utc>) -> Result<bool> {
        let before: Option<String> = self
            .conn
            .query_row(
                "SELECT before FROM cleared WHERE conversation = ?1",
                params![conversation],
                |r| r.get(0),
            )
            .optional()?;
        Ok(before
            .and_then(|b| DateTime::parse_from_rfc3339(&b).ok())
            .is_some_and(|b| timestamp <= b))
    }

    /// Delete a conversation's messages, deliveries and read marker, and keep
    /// archive copies of them from coming back. Returns how many messages.
    pub fn delete_conversation(&self, conversation: &str) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM deliveries WHERE message_id IN
               (SELECT id FROM messages WHERE conversation = ?1 COLLATE NOCASE)",
            params![conversation],
        )?;
        let n = tx.execute(
            "DELETE FROM messages WHERE conversation = ?1 COLLATE NOCASE",
            params![conversation],
        )?;
        tx.execute(
            "DELETE FROM read_markers WHERE conversation = ?1 COLLATE NOCASE",
            params![conversation],
        )?;
        tx.execute(
            "INSERT INTO cleared (conversation, before) VALUES (?1, ?2)
             ON CONFLICT(conversation) DO UPDATE SET before = excluded.before",
            params![conversation, Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Record where an attachment was saved (and its size).
    pub fn set_attachment_file(&self, id: i64, path: &str, size: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET att_path = ?2, att_size = ?3 WHERE id = ?1",
            params![id, path, size as i64],
        )?;
        Ok(())
    }

    pub fn get_message(&self, id: i64) -> Result<Option<Message>> {
        let m = self
            .conn
            .query_row(&format!("{SELECT_MESSAGE} WHERE id = ?1"), params![id], row_to_message)
            .optional()?;
        Ok(match m {
            Some(mut m) => {
                self.fill_reactions(std::slice::from_mut(&mut m))?;
                Some(m)
            }
            None => None,
        })
    }

    /// Add the reactions to messages read from the table.
    fn fill_reactions(&self, msgs: &mut [Message]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT emoji, sender FROM reactions WHERE message_id = ?1 ORDER BY emoji, sender")?;
        for m in msgs.iter_mut() {
            let rows = stmt.query_map(params![m.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out: Vec<Reaction> = Vec::new();
            for row in rows {
                let (emoji, sender) = row?;
                match out.iter_mut().find(|x| x.emoji == emoji) {
                    Some(x) => x.by.push(sender),
                    None => out.push(Reaction {
                        emoji,
                        by: vec![sender],
                    }),
                }
            }
            m.reactions = out;
        }
        Ok(())
    }

    /// The message an edit, retraction, reaction or reply points at: by the
    /// room's stanza-id in group chats, else by origin-id or message id.
    pub fn find_target(&self, conversation: &str, groupchat: bool, ref_id: &str) -> Result<Option<Message>> {
        let sql = if groupchat {
            "SELECT id FROM messages WHERE conversation = ?1 COLLATE NOCASE
               AND (stanza_id = ?2 OR origin_id = ?2 OR xmpp_id = ?2) AND deleted = 0 ORDER BY id DESC LIMIT 1"
        } else {
            "SELECT id FROM messages WHERE conversation = ?1 COLLATE NOCASE
               AND (origin_id = ?2 OR xmpp_id = ?2) AND deleted = 0 ORDER BY id DESC LIMIT 1"
        };
        let id: Option<i64> = self
            .conn
            .query_row(sql, params![conversation, ref_id], |r| r.get(0))
            .optional()?;
        match id {
            Some(id) => self.get_message(id),
            None => Ok(None),
        }
    }

    /// The groupchat reference (stanza-id) and the 1:1 one, separately.
    pub fn reference_ids(&self, id: i64) -> Result<(Option<String>, Option<String>, Option<String>)> {
        Ok(self
            .conn
            .query_row(
                "SELECT stanza_id, origin_id, xmpp_id FROM messages WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .unwrap_or((None, None, None)))
    }

    /// Record that an edit, retraction or reaction was applied; false if it
    /// already was.
    pub fn apply_once(&self, key: &str) -> Result<bool> {
        Ok(self
            .conn
            .execute("INSERT OR IGNORE INTO applied (key) VALUES (?1)", params![key])?
            > 0)
    }

    pub fn edit_message(&self, id: i64, body: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET body = ?2, edited = 1 WHERE id = ?1 AND retracted = 0",
            params![id, body],
        )?;
        Ok(())
    }

    /// Deleted for everyone: the text and file are gone.
    pub fn retract_message(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET body = '', retracted = 1, att_url = NULL, att_name = NULL,
                att_mime = NULL, att_size = NULL, att_path = NULL WHERE id = ?1",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM reactions WHERE message_id = ?1", params![id])?;
        Ok(())
    }

    /// Deleted on this device only. The row stays (hidden) so archive copies
    /// are recognised and don't bring it back.
    pub fn delete_message(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET body = '', deleted = 1, att_url = NULL, att_path = NULL WHERE id = ?1",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM reactions WHERE message_id = ?1", params![id])?;
        Ok(())
    }

    /// `sender`'s reactions to a message are now exactly `emojis` (XEP-0444).
    pub fn set_reactions(&self, id: i64, sender: &str, emojis: &[String]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM reactions WHERE message_id = ?1 AND sender = ?2",
            params![id, sender],
        )?;
        for e in emojis {
            tx.execute(
                "INSERT OR IGNORE INTO reactions (message_id, sender, emoji) VALUES (?1, ?2, ?3)",
                params![id, sender, e],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The recipient saw our messages up to `id` (XEP-0333).
    pub fn mark_seen(&self, conversation: &str, up_to_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET seen = 1 WHERE conversation = ?1 COLLATE NOCASE AND own = 1 AND id <= ?2",
            params![conversation, up_to_id],
        )?;
        Ok(())
    }

    pub fn max_message_id(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))?)
    }

    /// Messages of one conversation, oldest first.
    pub fn history(&self, conversation: &str, limit: u32, before_id: Option<i64>) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT * FROM ({SELECT_MESSAGE}
               WHERE conversation = ?1 COLLATE NOCASE AND id < ?2 AND deleted = 0
               ORDER BY id DESC LIMIT ?3)
             ORDER BY id ASC"
        ))?;
        let rows = stmt.query_map(
            params![conversation, before_id.unwrap_or(i64::MAX), limit],
            row_to_message,
        )?;
        let mut out: Vec<Message> = rows.collect::<Result<_, _>>()?;
        self.fill_reactions(&mut out)?;
        Ok(out)
    }

    pub fn conversations(&self) -> Result<Vec<ConversationSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.conversation, MAX(m.id),
                    (SELECT COUNT(*) FROM messages u
                      WHERE u.conversation = m.conversation AND u.own = 0 AND u.deleted = 0
                        AND u.retracted = 0
                        AND u.id > COALESCE((SELECT last_id FROM read_markers r
                                             WHERE r.conversation = m.conversation), 0))
             FROM messages m WHERE m.deleted = 0
             GROUP BY m.conversation ORDER BY MAX(m.id) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (conversation, last_id, unread) = row?;
            let last = self.get_message(last_id)?;
            out.push(ConversationSummary {
                kind: last.as_ref().map(|m| m.kind).unwrap_or(MessageKind::Chat),
                conversation,
                last_message: last,
                unread: unread as u64,
            });
        }
        Ok(out)
    }

    pub fn mark_read(&self, conversation: &str, up_to_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO read_markers (conversation, last_id) VALUES (?1, ?2)
             ON CONFLICT(conversation) DO UPDATE SET last_id = MAX(last_id, excluded.last_id)",
            params![conversation, up_to_id],
        )?;
        Ok(())
    }

    // --------------------------------------------------------------- consumers

    /// Ensure the consumer exists; new consumers start at "now" unless `from_start`.
    fn consumer_start(&self, d: &Delivery) -> Result<i64> {
        let start = if d.from_start { 0 } else { self.max_message_id()? };
        self.conn.execute(
            "INSERT OR IGNORE INTO consumers (name, start_id) VALUES (?1, ?2)",
            params![d.consumer, start],
        )?;
        Ok(self.conn.query_row(
            "SELECT start_id FROM consumers WHERE name = ?1",
            params![d.consumer],
            |r| r.get(0),
        )?)
    }

    /// Create a consumer starting at the current end of the store, if it
    /// doesn't exist yet.
    pub fn ensure_consumer(&self, name: &str) -> Result<()> {
        self.consumer_start(&Delivery {
            consumer: name.to_owned(),
            include_own: false,
            from_start: false,
        })?;
        Ok(())
    }

    /// Undelivered messages for a consumer, oldest first, filtered.
    pub fn undelivered(
        &self,
        d: &Delivery,
        filter: &Filter,
        require_trusted: bool,
        limit: Option<u32>,
    ) -> Result<Vec<Message>> {
        let start = self.consumer_start(d)?;
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_MESSAGE}
             WHERE id > ?1 AND (?2 OR own = 0) AND deleted = 0 AND retracted = 0
               AND NOT EXISTS (SELECT 1 FROM deliveries d WHERE d.consumer = ?3 AND d.message_id = messages.id)
             ORDER BY id ASC"
        ))?;
        let rows = stmt.query_map(params![start, d.include_own, d.consumer], row_to_message)?;
        let mut out = Vec::new();
        for m in rows {
            let m = m?;
            if !filter.matches(&m) {
                continue;
            }
            if require_trusted && !message_trusted(&m) {
                continue;
            }
            out.push(m);
            if limit.is_some_and(|l| out.len() >= l as usize) {
                break;
            }
        }
        Ok(out)
    }

    /// Record delivery. Returns false if it was already delivered to this consumer.
    pub fn mark_delivered(&self, consumer: &str, message_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO deliveries (consumer, message_id) VALUES (?1, ?2)",
            params![consumer, message_id],
        )?;
        Ok(n == 1)
    }

    /// Whether a freshly stored message should be handed to a waiting consumer.
    pub fn deliverable_to(&self, d: &Delivery, filter: &Filter, require_trusted: bool, m: &Message) -> Result<bool> {
        if m.own && !d.include_own {
            return Ok(false);
        }
        if !filter.matches(m) || (require_trusted && !message_trusted(m)) {
            return Ok(false);
        }
        let start = self.consumer_start(d)?;
        Ok(m.id > start)
    }

    // ------------------------------------------------------------- sync state

    pub fn last_archive_id(&self, archive: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_stanza_id FROM sync_state WHERE archive = ?1",
                params![archive],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_last_archive_id(&self, archive: &str, stanza_id: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sync_state (archive, last_stanza_id) VALUES (?1, ?2)
             ON CONFLICT(archive) DO UPDATE SET last_stanza_id = excluded.last_stanza_id",
            params![archive, stanza_id],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------ rooms

    pub fn rooms(&self) -> Result<Vec<RoomRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT jid, nick, password, autojoin FROM rooms ORDER BY jid")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn upsert_room(&self, jid: &str, nick: &str, password: Option<&str>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO rooms (jid, nick, password, autojoin) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(jid) DO UPDATE SET nick = excluded.nick, password = excluded.password, autojoin = 1",
            params![jid, nick, password],
        )?;
        Ok(())
    }

    pub fn remove_room(&self, jid: &str) -> Result<()> {
        self.conn.execute("DELETE FROM rooms WHERE jid = ?1", params![jid])?;
        Ok(())
    }

    pub fn room(&self, jid: &str) -> Result<Option<Room>> {
        Ok(self
            .conn
            .query_row(
                "SELECT jid, nick, autojoin FROM rooms WHERE jid = ?1 COLLATE NOCASE",
                params![jid],
                |r| {
                    Ok(Room {
                        jid: r.get(0)?,
                        nick: r.get(1)?,
                        autojoin: r.get(2)?,
                        joined: false,
                        subject: None,
                        occupants: vec![],
                        private: false,
                        name: None,
                    })
                },
            )
            .optional()?)
    }

    // --------------------------------------------------------------------- kv

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

fn message_trusted(m: &Message) -> bool {
    match &m.encryption {
        None => true,
        Some(e) => e.error.is_none() && e.trust.is_trusted(),
    }
}

const SELECT_MESSAGE: &str = "SELECT id, conversation, kind, sender, nick, body, timestamp, delayed,
    source, own, enc_protocol, enc_device, enc_trust, enc_error,
    att_url, att_name, att_mime, att_size, att_path, edited, retracted, reply_to, seen FROM messages";

fn row_to_message(r: &Row<'_>) -> rusqlite::Result<Message> {
    let kind: String = r.get(2)?;
    let ts: String = r.get(6)?;
    let source: String = r.get(8)?;
    let enc_protocol: Option<String> = r.get(10)?;
    let encryption = match enc_protocol.as_deref().and_then(EncryptionProtocol::parse) {
        Some(protocol) => Some(EncryptionInfo {
            protocol,
            sender_device: r.get::<_, Option<u32>>(11)?.unwrap_or(0),
            trust: r
                .get::<_, Option<String>>(12)?
                .as_deref()
                .and_then(Trust::parse)
                .unwrap_or(Trust::Untrusted),
            error: r.get(13)?,
        }),
        None => None,
    };
    Ok(Message {
        id: r.get(0)?,
        conversation: r.get(1)?,
        kind: MessageKind::parse(&kind).unwrap_or(MessageKind::Chat),
        from: r.get(3)?,
        nick: r.get(4)?,
        body: r.get(5)?,
        timestamp: DateTime::parse_from_rfc3339(&ts)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_default(),
        delayed: r.get(7)?,
        source: MessageSource::parse(&source).unwrap_or(MessageSource::Live),
        own: r.get(9)?,
        encryption,
        attachment: match r.get::<_, Option<String>>(14)? {
            Some(url) => Some(Attachment {
                encrypted: url.starts_with("aesgcm://"),
                url,
                name: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
                mime: r.get(16)?,
                size: r.get::<_, Option<i64>>(17)?.map(|s| s as u64),
                path: r.get(18)?,
            }),
            None => None,
        },
        edited: r.get(19)?,
        retracted: r.get(20)?,
        reply_to: r.get(21)?,
        reactions: vec![],
        seen: r.get(22)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(conv: &str, sender: &str, body: &str) -> NewMessage {
        NewMessage {
            conversation: conv.into(),
            kind: MessageKind::Chat,
            sender: sender.into(),
            nick: None,
            body: body.into(),
            timestamp: Utc::now(),
            delayed: false,
            source: MessageSource::Live,
            own: false,
            archive: None,
            stanza_id: None,
            origin_id: None,
            xmpp_id: None,
            encryption: None,
            attachment: None,
            reply_to: None,
        }
    }

    fn delivery(name: &str) -> Delivery {
        Delivery {
            consumer: name.into(),
            include_own: false,
            from_start: false,
        }
    }

    #[test]
    fn dedupes_by_stanza_id_and_backfills_archive_id() {
        let s = Store::open_in_memory().unwrap();
        let mut a = msg("bob@x", "bob@x", "hi");
        a.origin_id = Some("o1".into());
        assert!(s.insert_message(&a).unwrap().is_some());

        // Same message from the archive: matched by origin-id, gains the stanza-id.
        let mut b = a.clone();
        b.source = MessageSource::Mam;
        b.archive = Some("alice@x".into());
        b.stanza_id = Some("s1".into());
        assert!(s.insert_message(&b).unwrap().is_none());

        // A later copy carrying only the stanza-id is now also a duplicate.
        let mut c = msg("bob@x", "bob@x", "hi");
        c.archive = Some("alice@x".into());
        c.stanza_id = Some("s1".into());
        assert!(s.insert_message(&c).unwrap().is_none());

        // Different sender, same origin-id: not a duplicate.
        let mut d = msg("bob@x", "eve@x", "hi");
        d.origin_id = Some("o1".into());
        assert!(s.insert_message(&d).unwrap().is_some());
    }

    #[test]
    fn deleted_conversation_stays_deleted() {
        let s = Store::open_in_memory().unwrap();
        let old = msg("bob@x", "bob@x", "old");
        s.insert_message(&old).unwrap().unwrap();
        s.insert_message(&msg("carol@x", "carol@x", "keep")).unwrap().unwrap();
        s.mark_read("bob@x", 1).unwrap();
        assert_eq!(s.delete_conversation("Bob@X").unwrap(), 1);
        assert!(s.history("bob@x", 10, None).unwrap().is_empty());
        assert_eq!(s.conversations().unwrap().len(), 1);
        // An archive copy of an old message doesn't bring it back…
        let mut replay = old.clone();
        replay.delayed = true;
        replay.source = MessageSource::Mam;
        assert!(s.insert_message(&replay).unwrap().is_none());
        // …but a new message starts the conversation again.
        let mut new = msg("bob@x", "bob@x", "new");
        new.timestamp = Utc::now() + chrono::Duration::seconds(1);
        assert_eq!(s.insert_message(&new).unwrap().unwrap().body, "new");
        assert_eq!(s.conversations().unwrap()[0].unread, 1);
    }

    #[test]
    fn consumers_start_now_and_see_each_message_once() {
        let s = Store::open_in_memory().unwrap();
        s.insert_message(&msg("bob@x", "bob@x", "before")).unwrap();

        let d = delivery("agent1");
        assert!(s.undelivered(&d, &Filter::default(), false, None).unwrap().is_empty());

        let m = s.insert_message(&msg("bob@x", "bob@x", "after")).unwrap().unwrap();
        let got = s.undelivered(&d, &Filter::default(), false, None).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "after");
        assert!(s.mark_delivered("agent1", m.id).unwrap());
        assert!(!s.mark_delivered("agent1", m.id).unwrap());
        assert!(s.undelivered(&d, &Filter::default(), false, None).unwrap().is_empty());

        // A second consumer created with from_start sees everything.
        let d2 = Delivery {
            from_start: true,
            ..delivery("agent2")
        };
        assert_eq!(s.undelivered(&d2, &Filter::default(), false, None).unwrap().len(), 2);
    }

    #[test]
    fn own_messages_are_excluded_unless_requested() {
        let s = Store::open_in_memory().unwrap();
        let d = delivery("a");
        s.undelivered(&d, &Filter::default(), false, None).unwrap();
        let mut own = msg("bob@x", "alice@x", "my reply");
        own.own = true;
        own.source = MessageSource::Local;
        let m = s.insert_message(&own).unwrap().unwrap();
        assert!(s.undelivered(&d, &Filter::default(), false, None).unwrap().is_empty());
        assert!(!s.deliverable_to(&d, &Filter::default(), false, &m).unwrap());
        let d_own = Delivery {
            include_own: true,
            ..delivery("a")
        };
        assert_eq!(s.undelivered(&d_own, &Filter::default(), false, None).unwrap().len(), 1);
    }

    #[test]
    fn filters_do_not_consume_other_messages() {
        let s = Store::open_in_memory().unwrap();
        let d = delivery("a");
        s.undelivered(&d, &Filter::default(), false, None).unwrap();
        s.insert_message(&msg("bob@x", "bob@x", "from bob")).unwrap();
        s.insert_message(&msg("carol@x", "carol@x", "from carol")).unwrap();
        let only_carol = Filter {
            from: Some("carol@x".into()),
            room: None,
        };
        let got = s.undelivered(&d, &only_carol, false, None).unwrap();
        assert_eq!(got.len(), 1);
        s.mark_delivered("a", got[0].id).unwrap();
        // Bob's message is still unread for this consumer.
        let rest = s.undelivered(&d, &Filter::default(), false, None).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].body, "from bob");
    }

    #[test]
    fn require_trusted_skips_untrusted_and_undecryptable() {
        let s = Store::open_in_memory().unwrap();
        let d = delivery("a");
        s.undelivered(&d, &Filter::default(), false, None).unwrap();
        let mut untrusted = msg("bob@x", "bob@x", "secret");
        untrusted.encryption = Some(EncryptionInfo {
            protocol: EncryptionProtocol::Omemo2,
            sender_device: 42,
            trust: Trust::Untrusted,
            error: None,
        });
        let stored = s.insert_message(&untrusted).unwrap().unwrap();
        assert_eq!(stored.encryption.as_ref().unwrap().sender_device, 42);
        assert!(s.undelivered(&d, &Filter::default(), true, None).unwrap().is_empty());
        assert_eq!(s.undelivered(&d, &Filter::default(), false, None).unwrap().len(), 1);
    }

    #[test]
    fn history_and_unread_counts() {
        let s = Store::open_in_memory().unwrap();
        for i in 0..5 {
            s.insert_message(&msg("bob@x", "bob@x", &format!("m{i}"))).unwrap();
        }
        let h = s.history("bob@x", 3, None).unwrap();
        assert_eq!(
            h.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
            ["m2", "m3", "m4"]
        );
        let older = s.history("bob@x", 10, Some(h[0].id)).unwrap();
        assert_eq!(older.len(), 2);
        let conv = s.conversations().unwrap();
        assert_eq!(conv[0].unread, 5);
        s.mark_read("bob@x", h[1].id).unwrap();
        assert_eq!(s.conversations().unwrap()[0].unread, 1);
    }
}
