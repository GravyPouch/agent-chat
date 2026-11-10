//! SQLite implementation of [`Store`]: one writer thread, a small pool of
//! read-only connections used through `spawn_blocking` (§5.8).
//!
//! M1 runs each write in its own transaction. Batching several writes into
//! one transaction per ≤10 ms arrives with the archive in M3, where it pays.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, params};
use tokio::sync::oneshot;

use super::{Account, AccountState, BoxFut, CreatedVia, Credentials, NewAccount, Role, ScramKeys, Store};
use crate::util::{now_ms, random_bytes};

/// Applied in order and tracked with `PRAGMA user_version`.
const MIGRATIONS: &[(&str, &str)] = &[("0001_init", include_str!("migrations/0001_init.sql"))];

const READERS: usize = 4;

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

pub struct SqliteStore {
    writer: Mutex<std_mpsc::Sender<WriteJob>>,
    readers: Arc<Mutex<Vec<Connection>>>,
    path: PathBuf,
}

impl SqliteStore {
    /// Open (creating with mode 0600 if needed) and migrate the database.
    pub fn open(path: &Path) -> Result<Self> {
        create_private_file(path)?;
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        configure(&conn)?;
        migrate(&mut conn)?;

        let (tx, rx) = std_mpsc::channel::<WriteJob>();
        std::thread::Builder::new()
            .name("hub-store-writer".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job(&mut conn);
                }
            })
            .context("starting the store writer thread")?;

        Ok(Self {
            writer: Mutex::new(tx),
            readers: Arc::default(),
            path: path.to_owned(),
        })
    }

    fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> BoxFut<'_, T> {
        let (tx, rx) = oneshot::channel();
        let job: WriteJob = Box::new(move |conn| {
            let _ = tx.send(f(conn));
        });
        let sent = self.writer.lock().unwrap().send(job);
        Box::pin(async move {
            sent.map_err(|_| anyhow!("the store writer has stopped"))?;
            rx.await.map_err(|_| anyhow!("the store writer has stopped"))?
        })
    }

    fn read<T: Send + 'static>(&self, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> BoxFut<'_, T> {
        let readers = self.readers.clone();
        let path = self.path.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let conn = match readers.lock().unwrap().pop() {
                    Some(c) => c,
                    None => open_reader(&path)?,
                };
                let out = f(&conn);
                let mut pool = readers.lock().unwrap();
                if pool.len() < READERS {
                    pool.push(conn);
                }
                out
            })
            .await
            .map_err(|e| anyhow!("store reader task failed: {e}"))?
        })
    }
}

fn create_private_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).with_context(|| format!("creating {}", path.display()))?;
    Ok(())
}

fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("opening {} read-only", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: usize = conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? as usize;
    if current > MIGRATIONS.len() {
        bail!(
            "the hub database has schema version {current}, newer than this agent-chat ({}); upgrade agent-chat",
            MIGRATIONS.len()
        );
    }
    for (i, (name, sql)) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql).with_context(|| format!("applying migration {name}"))?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
        tracing::info!(migration = name, "applied database migration");
    }
    Ok(())
}

const ACCOUNT_COLUMNS: &str = "id, domain, localpart, role, state, created_ms, created_via, last_login_ms, \
     scram_salt, scram_iterations, sha1_stored_key, sha1_server_key, sha256_stored_key, sha256_server_key, \
     password_changed_ms";

fn account_from_row(r: &Row<'_>) -> rusqlite::Result<Result<Account>> {
    let role: String = r.get(3)?;
    let state: String = r.get(4)?;
    let via: String = r.get(6)?;
    let iterations: i64 = r.get(9)?;
    let base = (
        r.get::<_, String>(0)?,
        r.get::<_, String>(1)?,
        r.get::<_, String>(2)?,
        r.get::<_, i64>(5)?,
        r.get::<_, Option<i64>>(7)?,
        Credentials {
            salt: r.get(8)?,
            iterations: iterations as u32,
            sha1: ScramKeys {
                stored_key: r.get(10)?,
                server_key: r.get(11)?,
            },
            sha256: ScramKeys {
                stored_key: r.get(12)?,
                server_key: r.get(13)?,
            },
        },
        r.get::<_, i64>(14)?,
    );
    Ok((|| {
        let (id, domain, localpart, created_ms, last_login_ms, credentials, password_changed_ms) = base;
        Ok(Account {
            id,
            domain,
            localpart,
            role: Role::parse(&role)?,
            state: AccountState::parse(&state)?,
            created_ms,
            created_via: CreatedVia::parse(&via)?,
            last_login_ms,
            credentials,
            password_changed_ms,
        })
    })())
}

fn get_account(conn: &Connection, domain: &str, localpart: &str) -> Result<Option<Account>> {
    conn.query_row(
        &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE domain = ?1 AND localpart = ?2"),
        params![domain, localpart],
        account_from_row,
    )
    .optional()?
    .transpose()
}

impl Store for SqliteStore {
    fn ensure_host<'a>(&'a self, domain: &'a str) -> BoxFut<'a, ()> {
        let domain = domain.to_owned();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let existing: Vec<String> = tx
                .prepare("SELECT domain FROM hosts ORDER BY domain")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            match existing.as_slice() {
                [] => {
                    tx.execute(
                        "INSERT INTO hosts (domain, created_ms, config_json) VALUES (?1, ?2, '{}')",
                        params![domain, now_ms()],
                    )?;
                }
                [d] if *d == domain => {}
                others => bail!(
                    "this hub database belongs to {} but the configuration says {domain:?}; \
                     the domain is permanent (accounts can never move to another domain)",
                    others.join(", ")
                ),
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn secret<'a>(&'a self, key: &'a str, len: usize) -> BoxFut<'a, Vec<u8>> {
        let key = key.to_owned();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let existing: Option<Vec<u8>> = tx
                .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |r| r.get(0))
                .optional()?;
            let value = match existing {
                Some(v) => v,
                None => {
                    let v: Vec<u8> = random_bytes::<64>()[..len.min(64)].to_vec();
                    tx.execute("INSERT INTO meta (key, value) VALUES (?1, ?2)", params![key, v])?;
                    v
                }
            };
            tx.commit()?;
            Ok(value)
        })
    }

    fn create_account(&self, a: NewAccount) -> BoxFut<'_, Option<Account>> {
        self.write(move |conn| {
            let tx = conn.transaction()?;
            if get_account(&tx, &a.domain, &a.localpart)?.is_some() {
                return Ok(None);
            }
            let now = now_ms();
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO accounts (id, domain, localpart, role, state, created_ms, created_via, \
                 scram_salt, scram_iterations, sha1_stored_key, sha1_server_key, sha256_stored_key, \
                 sha256_server_key, password_changed_ms) \
                 VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?5)",
                params![
                    id,
                    a.domain,
                    a.localpart,
                    a.role.as_str(),
                    now,
                    a.created_via.as_str(),
                    a.credentials.salt,
                    a.credentials.iterations as i64,
                    a.credentials.sha1.stored_key,
                    a.credentials.sha1.server_key,
                    a.credentials.sha256.stored_key,
                    a.credentials.sha256.server_key,
                ],
            )?;
            let created = get_account(&tx, &a.domain, &a.localpart)?;
            tx.commit()?;
            Ok(created)
        })
    }

    fn account<'a>(&'a self, domain: &'a str, localpart: &'a str) -> BoxFut<'a, Option<Account>> {
        let (domain, localpart) = (domain.to_owned(), localpart.to_owned());
        self.read(move |conn| get_account(conn, &domain, &localpart))
    }

    fn list_accounts<'a>(&'a self, domain: &'a str) -> BoxFut<'a, Vec<Account>> {
        let domain = domain.to_owned();
        self.read(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE domain = ?1 ORDER BY localpart"
            ))?;
            let rows = stmt.query_map([&domain], account_from_row)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r??);
            }
            Ok(out)
        })
    }

    fn count_accounts<'a>(&'a self, domain: &'a str) -> BoxFut<'a, u64> {
        let domain = domain.to_owned();
        self.read(move |conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM accounts WHERE domain = ?1", [&domain], |r| r.get(0))?;
            Ok(n as u64)
        })
    }

    fn set_credentials<'a>(&'a self, domain: &'a str, localpart: &'a str, c: Credentials) -> BoxFut<'a, bool> {
        let (domain, localpart) = (domain.to_owned(), localpart.to_owned());
        self.write(move |conn| {
            let n = conn.execute(
                "UPDATE accounts SET scram_salt = ?3, scram_iterations = ?4, sha1_stored_key = ?5, \
                 sha1_server_key = ?6, sha256_stored_key = ?7, sha256_server_key = ?8, password_changed_ms = ?9 \
                 WHERE domain = ?1 AND localpart = ?2",
                params![
                    domain,
                    localpart,
                    c.salt,
                    c.iterations as i64,
                    c.sha1.stored_key,
                    c.sha1.server_key,
                    c.sha256.stored_key,
                    c.sha256.server_key,
                    now_ms(),
                ],
            )?;
            Ok(n > 0)
        })
    }

    fn delete_account<'a>(&'a self, domain: &'a str, localpart: &'a str) -> BoxFut<'a, bool> {
        let (domain, localpart) = (domain.to_owned(), localpart.to_owned());
        self.write(move |conn| {
            let n = conn.execute(
                "DELETE FROM accounts WHERE domain = ?1 AND localpart = ?2",
                params![domain, localpart],
            )?;
            Ok(n > 0)
        })
    }

    fn record_login<'a>(&'a self, domain: &'a str, localpart: &'a str, ts_ms: i64) -> BoxFut<'a, ()> {
        let (domain, localpart) = (domain.to_owned(), localpart.to_owned());
        self.write(move |conn| {
            conn.execute(
                "UPDATE accounts SET last_login_ms = ?3 WHERE domain = ?1 AND localpart = ?2",
                params![domain, localpart, ts_ms],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_meets_the_store_contract() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("hub.db")).unwrap();
        super::super::contract_tests(&store).await;
    }

    #[tokio::test]
    async fn migrations_are_idempotent_and_files_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.db");
        {
            let store = SqliteStore::open(&path).unwrap();
            store.ensure_host("localhost").await.unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        store.ensure_host("localhost").await.unwrap();
        assert!(store.ensure_host("other.local").await.is_err());
        let conn = Connection::open(&path).unwrap();
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
