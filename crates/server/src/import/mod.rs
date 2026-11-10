//! Importing data from other servers into a hub (§10.5, plan item 40).
//! Run against a stopped hub: it writes to the store directly.

use anyhow::{Context, Result, bail};

use crate::HubPaths;
use crate::config::{Config, KNOWN_MECHANISMS};
use crate::store::{SqliteStore, Store};

pub mod lua;
pub mod prosody;

pub use prosody::{ImportOptions, ImportReport};

/// The full result of `hub import-prosody`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Imported {
    #[serde(flatten)]
    pub report: ImportReport,
    /// `auth.mechanisms` was changed to SCRAM-SHA-1 (+PLAIN) because some
    /// accounts have only SHA-1 keys (tokio-xmpp, like most clients, picks
    /// SCRAM-SHA-256 when offered and would fail for those accounts).
    pub mechanisms: Vec<String>,
    pub mechanisms_changed: bool,
}

/// Import a Prosody `data_path` into the (stopped) hub at `paths`.
pub async fn import_prosody(paths: &HubPaths, opts: ImportOptions) -> Result<Imported> {
    let lock = std::fs::File::create(paths.lock()).context("creating the hub lock")?;
    if lock.try_lock().is_err() {
        bail!("the hub is running; stop it first (agent-chat hub stop)");
    }
    let mut cfg = Config::load(paths.config())?;
    let store: std::sync::Arc<dyn Store> = std::sync::Arc::new(SqliteStore::open(&paths.db())?);
    store.ensure_host(&cfg.domain).await?;
    let report = prosody::import(&store, &cfg.domain, &opts).await?;
    let offers_sha256 = cfg.auth.mechanisms.iter().any(|m| m.starts_with("SCRAM-SHA-256"));
    let mut changed = false;
    if report.sha1_only > 0 && offers_sha256 {
        cfg.auth.mechanisms = KNOWN_MECHANISMS
            .iter()
            .filter(|m| !m.starts_with("SCRAM-SHA-256"))
            .map(|m| m.to_string())
            .collect();
        crate::paths::write_private(paths.config(), cfg.to_toml()?.as_bytes())?;
        changed = true;
    }
    drop(lock);
    Ok(Imported {
        report,
        mechanisms: cfg.auth.mechanisms.clone(),
        mechanisms_changed: changed,
    })
}
