//! SQLite persistence for OMEMO: libomemo-c records, our own device state,
//! and the device/trust table for everyone else.
//!
//! All tables are keyed by `version` ("omemo2" / "legacy") so the two
//! protocol versions never share sessions or keys.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use agent_chat_proto::model::Trust;
use anyhow::Result;
use omemo::{OmemoStore, StoreResult, Version};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS omemo_own (
    version          TEXT PRIMARY KEY,
    device_id        INTEGER NOT NULL,
    identity_public  BLOB NOT NULL,
    identity_private BLOB NOT NULL,
    signed_pre_key_id INTEGER NOT NULL DEFAULT 0,
    spk_created      INTEGER NOT NULL DEFAULT 0,
    next_pre_key_id  INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS omemo_sessions (
    version TEXT NOT NULL, jid TEXT NOT NULL, device_id INTEGER NOT NULL, record BLOB NOT NULL,
    PRIMARY KEY (version, jid, device_id)
);
CREATE TABLE IF NOT EXISTS omemo_pre_keys (
    version TEXT NOT NULL, id INTEGER NOT NULL, record BLOB NOT NULL,
    PRIMARY KEY (version, id)
);
CREATE TABLE IF NOT EXISTS omemo_signed_pre_keys (
    version TEXT NOT NULL, id INTEGER NOT NULL, record BLOB NOT NULL,
    PRIMARY KEY (version, id)
);
-- Devices of every JID we've seen (including our own other devices).
-- identity_key is 0x05||Curve25519 (libomemo-c serialization); trust NULL = undecided.
CREATE TABLE IF NOT EXISTS omemo_devices (
    version      TEXT NOT NULL,
    jid          TEXT NOT NULL,
    device_id    INTEGER NOT NULL,
    active       INTEGER NOT NULL DEFAULT 1,
    identity_key BLOB,
    trust        TEXT,
    label        TEXT,
    PRIMARY KEY (version, jid, device_id)
);
CREATE TABLE IF NOT EXISTS omemo_device_lists (
    version    TEXT NOT NULL,
    jid        TEXT NOT NULL,
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (version, jid)
);
"#;

pub fn vkey(v: Version) -> &'static str {
    match v {
        Version::Omemo2 => "omemo2",
        Version::Legacy => "legacy",
    }
}

/// Shared connection. Never hold the lock while calling into libomemo-c
/// (its callbacks take the same lock).
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

#[derive(Debug, Clone)]
pub struct OwnDevice {
    pub device_id: u32,
    pub signed_pre_key_id: u32,
    pub spk_created: i64,
    pub next_pre_key_id: u32,
}

#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub jid: String,
    pub device_id: u32,
    pub active: bool,
    pub identity_key: Option<Vec<u8>>,
    pub trust: Option<Trust>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap()
    }

    // ------------------------------------------------------------ own device

    pub fn own(&self, v: Version) -> Result<Option<OwnDevice>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT device_id, signed_pre_key_id, spk_created, next_pre_key_id FROM omemo_own WHERE version = ?1",
                params![vkey(v)],
                |r| {
                    Ok(OwnDevice {
                        device_id: r.get(0)?,
                        signed_pre_key_id: r.get(1)?,
                        spk_created: r.get(2)?,
                        next_pre_key_id: r.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn create_own(&self, v: Version, device_id: u32, public: &[u8], private: &[u8]) -> Result<()> {
        self.conn().execute(
            "INSERT INTO omemo_own (version, device_id, identity_public, identity_private) VALUES (?1, ?2, ?3, ?4)",
            params![vkey(v), device_id, public, private],
        )?;
        Ok(())
    }

    pub fn set_signed_pre_key(&self, v: Version, id: u32, created: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE omemo_own SET signed_pre_key_id = ?2, spk_created = ?3 WHERE version = ?1",
            params![vkey(v), id, created],
        )?;
        Ok(())
    }

    pub fn set_next_pre_key_id(&self, v: Version, next: u32) -> Result<()> {
        self.conn().execute(
            "UPDATE omemo_own SET next_pre_key_id = ?2 WHERE version = ?1",
            params![vkey(v), next],
        )?;
        Ok(())
    }

    pub fn pre_keys(&self, v: Version) -> Result<Vec<(u32, Vec<u8>)>> {
        let c = self.conn();
        let mut st = c.prepare("SELECT id, record FROM omemo_pre_keys WHERE version = ?1 ORDER BY id")?;
        let rows = st.query_map(params![vkey(v)], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn put_pre_key(&self, v: Version, id: u32, record: &[u8]) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO omemo_pre_keys (version, id, record) VALUES (?1, ?2, ?3)",
            params![vkey(v), id, record],
        )?;
        Ok(())
    }

    pub fn signed_pre_key(&self, v: Version, id: u32) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT record FROM omemo_signed_pre_keys WHERE version = ?1 AND id = ?2",
                params![vkey(v), id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn put_signed_pre_key(&self, v: Version, id: u32, record: &[u8]) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO omemo_signed_pre_keys (version, id, record) VALUES (?1, ?2, ?3)",
            params![vkey(v), id, record],
        )?;
        Ok(())
    }

    /// Keep the current and the previous signed pre key (late key exchanges).
    pub fn prune_signed_pre_keys(&self, v: Version, keep_from: u32) -> Result<()> {
        self.conn().execute(
            "DELETE FROM omemo_signed_pre_keys WHERE version = ?1 AND id < ?2",
            params![vkey(v), keep_from],
        )?;
        Ok(())
    }

    // --------------------------------------------------------------- devices

    /// Replace the active device set of `jid` from a fresh device list.
    pub fn set_device_list(&self, v: Version, jid: &str, ids: &[(u32, Option<String>)], now: i64) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE omemo_devices SET active = 0 WHERE version = ?1 AND jid = ?2",
            params![vkey(v), jid],
        )?;
        for (id, label) in ids {
            tx.execute(
                "INSERT INTO omemo_devices (version, jid, device_id, active, label) VALUES (?1, ?2, ?3, 1, ?4)
                 ON CONFLICT(version, jid, device_id) DO UPDATE SET active = 1, label = excluded.label",
                params![vkey(v), jid, id, label],
            )?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO omemo_device_lists (version, jid, fetched_at) VALUES (?1, ?2, ?3)",
            params![vkey(v), jid, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn device_list_fetched(&self, v: Version, jid: &str) -> Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM omemo_device_lists WHERE version = ?1 AND jid = ?2",
                params![vkey(v), jid],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn devices(&self, v: Version, jid: &str) -> Result<Vec<DeviceRow>> {
        let c = self.conn();
        let mut st = c.prepare(
            "SELECT jid, device_id, active, identity_key, trust FROM omemo_devices
             WHERE version = ?1 AND jid = ?2 ORDER BY device_id",
        )?;
        let rows = st.query_map(params![vkey(v), jid], |r| {
            let trust: Option<String> = r.get(4)?;
            Ok(DeviceRow {
                jid: r.get(0)?,
                device_id: r.get(1)?,
                active: r.get(2)?,
                identity_key: r.get(3)?,
                trust: trust.as_deref().and_then(Trust::parse),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Record a device's identity key. A *changed* key resets trust.
    pub fn record_identity(&self, v: Version, jid: &str, device_id: u32, key: &[u8]) -> Result<()> {
        let c = self.conn();
        let existing: Option<Option<Vec<u8>>> = c
            .query_row(
                "SELECT identity_key FROM omemo_devices WHERE version = ?1 AND jid = ?2 AND device_id = ?3",
                params![vkey(v), jid, device_id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            None => {
                c.execute(
                    "INSERT INTO omemo_devices (version, jid, device_id, active, identity_key) VALUES (?1, ?2, ?3, 1, ?4)",
                    params![vkey(v), jid, device_id, key],
                )?;
            }
            Some(Some(old)) if old == key => {}
            Some(old) => {
                if old.is_some() {
                    tracing::warn!("identity key of {jid} device {device_id} changed; trust reset");
                }
                c.execute(
                    "UPDATE omemo_devices SET identity_key = ?4, trust = NULL
                     WHERE version = ?1 AND jid = ?2 AND device_id = ?3",
                    params![vkey(v), jid, device_id, key],
                )?;
            }
        }
        Ok(())
    }

    pub fn set_trust(&self, v: Version, jid: &str, device_id: u32, trust: Trust) -> Result<()> {
        self.conn().execute(
            "UPDATE omemo_devices SET trust = ?4 WHERE version = ?1 AND jid = ?2 AND device_id = ?3",
            params![vkey(v), jid, device_id, trust.as_str()],
        )?;
        Ok(())
    }

    /// Blind Trust Before Verification: undecided devices are trusted until
    /// the user has verified at least one device of that JID (any version).
    pub fn effective_trust(&self, jid: &str, row_trust: Option<Trust>) -> Result<Trust> {
        if let Some(t) = row_trust {
            return Ok(t);
        }
        let any_verified: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM omemo_devices WHERE jid = ?1 AND trust = 'verified')",
            params![jid],
            |r| r.get(0),
        )?;
        Ok(if any_verified { Trust::Untrusted } else { Trust::Trusted })
    }
}

/// libomemo-c store for one version, backed by [`Db`].
pub struct SqliteStore {
    pub db: Db,
    pub version: Version,
}

fn e(err: impl std::fmt::Display) -> String {
    err.to_string()
}

impl OmemoStore for SqliteStore {
    fn identity_key_pair(&mut self) -> StoreResult<(Vec<u8>, Vec<u8>)> {
        self.db
            .conn()
            .query_row(
                "SELECT identity_public, identity_private FROM omemo_own WHERE version = ?1",
                params![vkey(self.version)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(e)
    }

    fn local_registration_id(&mut self) -> StoreResult<u32> {
        self.db
            .conn()
            .query_row(
                "SELECT device_id FROM omemo_own WHERE version = ?1",
                params![vkey(self.version)],
                |r| r.get(0),
            )
            .map_err(e)
    }

    fn save_identity(&mut self, name: &str, device_id: i32, key: &[u8]) -> StoreResult<()> {
        if key.is_empty() {
            return Ok(());
        }
        self.db
            .record_identity(self.version, name, device_id as u32, key)
            .map_err(e)
    }

    fn load_session(&mut self, name: &str, device_id: i32) -> StoreResult<Option<Vec<u8>>> {
        self.db
            .conn()
            .query_row(
                "SELECT record FROM omemo_sessions WHERE version = ?1 AND jid = ?2 AND device_id = ?3",
                params![vkey(self.version), name, device_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(e)
    }

    fn store_session(&mut self, name: &str, device_id: i32, record: &[u8]) -> StoreResult<()> {
        self.db
            .conn()
            .execute(
                "INSERT OR REPLACE INTO omemo_sessions (version, jid, device_id, record) VALUES (?1, ?2, ?3, ?4)",
                params![vkey(self.version), name, device_id, record],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn contains_session(&mut self, name: &str, device_id: i32) -> StoreResult<bool> {
        Ok(self.load_session(name, device_id)?.is_some())
    }

    fn delete_session(&mut self, name: &str, device_id: i32) -> StoreResult<()> {
        self.db
            .conn()
            .execute(
                "DELETE FROM omemo_sessions WHERE version = ?1 AND jid = ?2 AND device_id = ?3",
                params![vkey(self.version), name, device_id],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn delete_all_sessions(&mut self, name: &str) -> StoreResult<usize> {
        self.db
            .conn()
            .execute(
                "DELETE FROM omemo_sessions WHERE version = ?1 AND jid = ?2",
                params![vkey(self.version), name],
            )
            .map_err(e)
    }

    fn sub_device_sessions(&mut self, name: &str) -> StoreResult<Vec<i32>> {
        let c = self.db.conn();
        let mut st = c
            .prepare("SELECT device_id FROM omemo_sessions WHERE version = ?1 AND jid = ?2")
            .map_err(e)?;
        let rows = st
            .query_map(params![vkey(self.version), name], |r| r.get(0))
            .map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)
    }

    fn load_pre_key(&mut self, id: u32) -> StoreResult<Option<Vec<u8>>> {
        self.db
            .conn()
            .query_row(
                "SELECT record FROM omemo_pre_keys WHERE version = ?1 AND id = ?2",
                params![vkey(self.version), id],
                |r| r.get(0),
            )
            .optional()
            .map_err(e)
    }

    fn store_pre_key(&mut self, id: u32, record: &[u8]) -> StoreResult<()> {
        self.db.put_pre_key(self.version, id, record).map_err(e)
    }

    fn contains_pre_key(&mut self, id: u32) -> StoreResult<bool> {
        Ok(self.load_pre_key(id)?.is_some())
    }

    fn remove_pre_key(&mut self, id: u32) -> StoreResult<()> {
        self.db
            .conn()
            .execute(
                "DELETE FROM omemo_pre_keys WHERE version = ?1 AND id = ?2",
                params![vkey(self.version), id],
            )
            .map(|_| ())
            .map_err(e)
    }

    fn load_signed_pre_key(&mut self, id: u32) -> StoreResult<Option<Vec<u8>>> {
        self.db.signed_pre_key(self.version, id).map_err(e)
    }

    fn store_signed_pre_key(&mut self, id: u32, record: &[u8]) -> StoreResult<()> {
        self.db.put_signed_pre_key(self.version, id, record).map_err(e)
    }

    fn contains_signed_pre_key(&mut self, id: u32) -> StoreResult<bool> {
        Ok(self.load_signed_pre_key(id)?.is_some())
    }

    fn remove_signed_pre_key(&mut self, id: u32) -> StoreResult<()> {
        self.db
            .conn()
            .execute(
                "DELETE FROM omemo_signed_pre_keys WHERE version = ?1 AND id = ?2",
                params![vkey(self.version), id],
            )
            .map(|_| ())
            .map_err(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btbv_and_identity_change() {
        let db = Db::open_in_memory().unwrap();
        let v = Version::Omemo2;
        db.set_device_list(v, "bob@x", &[(1, None), (2, None)], 0).unwrap();
        db.record_identity(v, "bob@x", 1, b"key-1").unwrap();
        let rows = db.devices(v, "bob@x").unwrap();
        assert_eq!(rows.len(), 2);
        // Nothing verified yet: blind trust.
        assert_eq!(db.effective_trust("bob@x", rows[0].trust).unwrap(), Trust::Trusted);

        // Verify device 1; undecided device 2 becomes untrusted.
        db.set_trust(v, "bob@x", 1, Trust::Verified).unwrap();
        let rows = db.devices(v, "bob@x").unwrap();
        assert_eq!(db.effective_trust("bob@x", rows[0].trust).unwrap(), Trust::Verified);
        assert_eq!(db.effective_trust("bob@x", rows[1].trust).unwrap(), Trust::Untrusted);

        // A changed identity key resets trust.
        db.record_identity(v, "bob@x", 1, b"key-EVIL").unwrap();
        let rows = db.devices(v, "bob@x").unwrap();
        assert_eq!(rows[0].trust, None);

        // A device list update deactivates missing devices.
        db.set_device_list(v, "bob@x", &[(2, None)], 1).unwrap();
        let rows = db.devices(v, "bob@x").unwrap();
        assert!(!rows[0].active && rows[1].active);
    }
}
