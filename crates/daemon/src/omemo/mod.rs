//! OMEMO end-to-end encryption (XEP-0384): OMEMO:2 and legacy, via libomemo-c.

pub mod store;
pub mod xml;
pub mod xmpp;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow};
use omemo::{Context, OwnBundle, Version};

use self::store::{Db, SqliteStore};

/// OMEMO:2 (XEP-0384 0.4+).
pub const NS_OMEMO2: &str = "urn:xmpp:omemo:2";
/// Legacy OMEMO (XEP-0384 0.3).
pub const NS_LEGACY: &str = "eu.siacs.conversations.axolotl";

/// Body sent alongside encrypted payloads for clients without OMEMO.
pub const FALLBACK_BODY: &str = "This message is encrypted with OMEMO, which your client doesn't seem to support.";

/// Preferred first.
pub const VERSIONS: [Version; 2] = [Version::Omemo2, Version::Legacy];

/// XEP-0384: bundles SHOULD hold ~100 pre keys and MUST hold at least 25.
const PRE_KEY_TARGET: usize = 100;
const SIGNED_PRE_KEY_MAX_AGE_MS: i64 = 7 * 24 * 3600 * 1000;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A random device id in 1..2^31.
fn random_device_id() -> u32 {
    loop {
        let b = uuid::Uuid::new_v4().into_bytes();
        let id = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & 0x7fff_ffff;
        if id != 0 {
            return id;
        }
    }
}

pub struct Engine {
    pub ctx: Context,
    pub device_id: u32,
}

/// Both OMEMO engines plus the shared database.
pub struct Manager {
    pub db: Db,
    engines: Mutex<HashMap<Version, Engine>>,
}

impl Manager {
    pub fn open(db_path: &Path) -> Result<Self> {
        let db = Db::open(db_path).context("opening OMEMO tables")?;
        Self::with_db(db)
    }

    pub fn with_db(db: Db) -> Result<Self> {
        let mut engines = HashMap::new();
        for v in VERSIONS {
            let ctx = Context::new(
                v,
                Box::new(SqliteStore {
                    db: db.clone(),
                    version: v,
                }),
            )
            .map_err(|e| anyhow!("creating {v:?} context: {e}"))?;
            let own = match db.own(v)? {
                Some(o) => o,
                None => {
                    let (public, private) = ctx
                        .generate_identity_key_pair()
                        .map_err(|e| anyhow!("generating identity: {e}"))?;
                    db.create_own(v, random_device_id(), &public, &private)?;
                    db.own(v)?.context("own device row missing after insert")?
                }
            };
            let engine = Engine {
                ctx,
                device_id: own.device_id,
            };
            ensure_keys(&db, v, &engine)?;
            engines.insert(v, engine);
        }
        Ok(Self {
            db,
            engines: Mutex::new(engines),
        })
    }

    /// Run `f` with the engine for `v`. Don't hold `db.conn()` inside.
    pub fn with<T>(&self, v: Version, f: impl FnOnce(&Engine) -> T) -> T {
        let engines = self.engines.lock().unwrap();
        f(engines.get(&v).expect("both versions are initialised"))
    }

    pub fn device_id(&self, v: Version) -> u32 {
        self.with(v, |e| e.device_id)
    }

    pub fn own_bundle(&self, v: Version) -> Result<OwnBundle> {
        let own = self.db.own(v)?.context("no own device")?;
        let spk = self
            .db
            .signed_pre_key(v, own.signed_pre_key_id)?
            .context("signed pre key missing")?;
        let pre_keys = self.db.pre_keys(v)?;
        self.with(v, |e| e.ctx.own_bundle(&spk, &pre_keys))
            .map_err(|e| anyhow!("building bundle: {e}"))
    }

    /// Top up pre keys / rotate the signed pre key. Returns true if the
    /// bundle changed and should be republished.
    pub fn refresh_keys(&self, v: Version) -> Result<bool> {
        let engines = self.engines.lock().unwrap();
        ensure_keys(&self.db, v, engines.get(&v).expect("initialised"))
    }
}

fn ensure_keys(db: &Db, v: Version, engine: &Engine) -> Result<bool> {
    let own = db.own(v)?.context("no own device")?;
    let mut changed = false;
    let now = now_ms();
    if own.signed_pre_key_id == 0 || now - own.spk_created > SIGNED_PRE_KEY_MAX_AGE_MS {
        let id = own.signed_pre_key_id + 1;
        let record = engine
            .ctx
            .generate_signed_pre_key(id, now as u64)
            .map_err(|e| anyhow!("generating signed pre key: {e}"))?;
        db.put_signed_pre_key(v, id, &record)?;
        db.set_signed_pre_key(v, id, now)?;
        // Keep the previous one for key exchanges already in flight.
        db.prune_signed_pre_keys(v, id.saturating_sub(1))?;
        changed = true;
    }
    let have = db.pre_keys(v)?.len();
    if have < PRE_KEY_TARGET {
        let count = (PRE_KEY_TARGET - have) as u32;
        let keys = engine
            .ctx
            .generate_pre_keys(own.next_pre_key_id.max(1), count)
            .map_err(|e| anyhow!("generating pre keys: {e}"))?;
        let next = keys.last().map(|(id, _)| id + 1).unwrap_or(own.next_pre_key_id);
        for (id, record) in keys {
            db.put_pre_key(v, id, &record)?;
        }
        db.set_next_pre_key_id(v, next)?;
        changed = true;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_creates_devices_and_full_bundles() {
        let m = Manager::with_db(Db::open_in_memory().unwrap()).unwrap();
        for v in VERSIONS {
            let b = m.own_bundle(v).unwrap();
            assert_eq!(b.pre_keys.len(), PRE_KEY_TARGET);
            assert_eq!(b.signed_pre_key_id, 1);
            assert!(m.device_id(v) > 0 && m.device_id(v) < 1 << 31);
        }
        assert_ne!(m.device_id(Version::Omemo2), m.device_id(Version::Legacy));
        // Nothing to do on a second pass.
        assert!(!m.refresh_keys(Version::Omemo2).unwrap());
    }
}
