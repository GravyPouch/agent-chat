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

use super::{
    Account, AccountState, ArchivePage, ArchiveQuery, ArchiveRow, BoxFut, CreatedVia, Credentials, Direction,
    NewAccount, PepItem, Role, RoomAffiliation, RoomRow, RosterItem, ScramKeys, Store, Subscription,
};
use crate::util::{now_ms, random_bytes};

/// Applied in order and tracked with `PRAGMA user_version`.
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.sql")),
    ("0002_im", include_str!("migrations/0002_im.sql")),
    ("0003_invites", include_str!("migrations/0003_invites.sql")),
];

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

    fn write<T: Send + 'static>(&self, f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static) -> BoxFut<'_, T> {
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
    opts.open(path)
        .with_context(|| format!("creating {}", path.display()))?;
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
        tx.execute_batch(sql)
            .with_context(|| format!("applying migration {name}"))?;
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
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM accounts WHERE domain = ?1", [&domain], |r| {
                r.get(0)
            })?;
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
            let tx = conn.transaction()?;
            let n = tx.execute(
                "DELETE FROM accounts WHERE domain = ?1 AND localpart = ?2",
                params![domain, localpart],
            )?;
            if n > 0 {
                let jid = format!("{localpart}@{domain}");
                for sql in [
                    "DELETE FROM roster_items WHERE account = ?1",
                    "DELETE FROM archive WHERE archive_jid = ?1",
                    "DELETE FROM offline WHERE account = ?1",
                    "DELETE FROM pep_items WHERE owner = ?1",
                    "DELETE FROM pep_nodes WHERE owner = ?1",
                    "DELETE FROM blocklist WHERE account = ?1",
                    "DELETE FROM private_xml WHERE account = ?1",
                ] {
                    tx.execute(sql, [&jid])?;
                }
            }
            tx.commit()?;
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

    // ------------------------------------------------------------ roster

    fn roster<'a>(&'a self, account: &'a str) -> BoxFut<'a, Vec<RosterItem>> {
        let account = account.to_owned();
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT contact, name, subscription, ask_out, pending_in, approved, hidden, groups_json \
                 FROM roster_items WHERE account = ?1 ORDER BY contact",
            )?;
            let rows = stmt.query_map([&account], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, bool>(6)?,
                    r.get::<_, String>(7)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (contact, name, sub, ask_out, pending_in, approved, hidden, groups) = row?;
                out.push(RosterItem {
                    contact,
                    name,
                    subscription: Subscription::parse(&sub).unwrap_or(Subscription::None),
                    ask_out,
                    pending_in,
                    approved,
                    hidden,
                    groups: serde_json::from_str(&groups).unwrap_or_default(),
                });
            }
            Ok(out)
        })
    }

    fn roster_put<'a>(&'a self, account: &'a str, item: RosterItem) -> BoxFut<'a, ()> {
        let account = account.to_owned();
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO roster_items (account, contact, name, subscription, ask_out, pending_in, approved, hidden, \
                 groups_json, updated_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT (account, contact) DO UPDATE SET name = excluded.name, \
                 subscription = excluded.subscription, ask_out = excluded.ask_out, \
                 pending_in = excluded.pending_in, approved = excluded.approved, hidden = excluded.hidden, \
                 groups_json = excluded.groups_json, updated_ms = excluded.updated_ms",
                params![
                    account,
                    item.contact,
                    item.name,
                    item.subscription.as_str(),
                    item.ask_out,
                    item.pending_in,
                    item.approved,
                    item.hidden,
                    serde_json::to_string(&item.groups)?,
                    now_ms(),
                ],
            )?;
            Ok(())
        })
    }

    fn roster_remove<'a>(&'a self, account: &'a str, contact: &'a str) -> BoxFut<'a, ()> {
        let (account, contact) = (account.to_owned(), contact.to_owned());
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM roster_items WHERE account = ?1 AND contact = ?2",
                params![account, contact],
            )?;
            Ok(())
        })
    }

    // ------------------------------------------------ archive and offline

    fn archive_insert(&self, row: ArchiveRow) -> BoxFut<'_, ()> {
        self.write(move |conn| {
            let seq: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM archive", [], |r| r.get(0))?;
            conn.execute(
                "INSERT INTO archive (id, seq, archive_jid, ts_ms, with_jid, direction, origin_id, from_jid, stanza) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.id,
                    seq,
                    row.archive_jid,
                    row.ts_ms,
                    row.with_jid,
                    row.direction.as_str(),
                    row.origin_id,
                    row.from_jid,
                    row.stanza,
                ],
            )?;
            Ok(())
        })
    }

    fn archive_query(&self, q: ArchiveQuery) -> BoxFut<'_, ArchivePage> {
        self.read(move |conn| query_archive(conn, &q))
    }

    fn archive_get<'a>(&'a self, archive_jid: &'a str, id: &'a str) -> BoxFut<'a, Option<ArchiveRow>> {
        let (archive_jid, id) = (archive_jid.to_owned(), id.to_owned());
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {ARCHIVE_COLUMNS} FROM archive WHERE archive_jid = ?1 AND id = ?2"),
                    params![archive_jid, id],
                    archive_from_row,
                )
                .optional()?)
        })
    }

    fn archive_expire(&self, before_ms: i64) -> BoxFut<'_, u64> {
        self.write(move |conn| {
            // In batches, so the writer isn't blocked for long.
            let mut total = 0u64;
            loop {
                let n = conn.execute(
                    "DELETE FROM archive WHERE id IN (SELECT id FROM archive WHERE ts_ms < ?1 LIMIT 1000)",
                    [before_ms],
                )?;
                total += n as u64;
                if n < 1000 {
                    break;
                }
            }
            conn.execute("DELETE FROM offline WHERE added_ms < ?1", [before_ms])?;
            Ok(total)
        })
    }

    fn archive_clear<'a>(&'a self, archive_jid: &'a str) -> BoxFut<'a, ()> {
        let archive_jid = archive_jid.to_owned();
        self.write(move |conn| {
            conn.execute("DELETE FROM archive WHERE archive_jid = ?1", [&archive_jid])?;
            Ok(())
        })
    }

    fn offline_push<'a>(
        &'a self,
        account: &'a str,
        archive_id: Option<String>,
        stanza: String,
        cap: usize,
    ) -> BoxFut<'a, ()> {
        let account = account.to_owned();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let seq: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM offline WHERE account = ?1",
                [&account],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO offline (account, seq, archive_id, stanza, added_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![account, seq, archive_id, stanza, now_ms()],
            )?;
            let dropped = tx.execute(
                "DELETE FROM offline WHERE account = ?1 AND seq NOT IN \
                 (SELECT seq FROM offline WHERE account = ?1 ORDER BY seq DESC LIMIT ?2)",
                params![account, cap as i64],
            )?;
            if dropped > 0 {
                tracing::warn!(account, dropped, "offline queue full; dropped the oldest messages");
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn offline_take<'a>(&'a self, account: &'a str) -> BoxFut<'a, Vec<(String, i64)>> {
        let account = account.to_owned();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let out: Vec<(String, i64)> = tx
                .prepare("SELECT stanza, added_ms FROM offline WHERE account = ?1 ORDER BY seq")?
                .query_map([&account], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            tx.execute("DELETE FROM offline WHERE account = ?1", [&account])?;
            tx.commit()?;
            Ok(out)
        })
    }

    fn offline_count<'a>(&'a self, account: &'a str) -> BoxFut<'a, u64> {
        let account = account.to_owned();
        self.read(move |conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM offline WHERE account = ?1", [&account], |r| {
                r.get(0)
            })?;
            Ok(n as u64)
        })
    }

    // --------------------------------------------------------------- PEP

    fn pep_nodes<'a>(&'a self, owner: &'a str) -> BoxFut<'a, Vec<(String, String)>> {
        let owner = owner.to_owned();
        self.read(move |conn| {
            Ok(conn
                .prepare("SELECT node, config_json FROM pep_nodes WHERE owner = ?1 ORDER BY node")?
                .query_map([&owner], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    fn pep_node_put<'a>(&'a self, owner: &'a str, node: &'a str, config_json: String) -> BoxFut<'a, ()> {
        let (owner, node) = (owner.to_owned(), node.to_owned());
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO pep_nodes (owner, node, config_json, created_ms) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (owner, node) DO UPDATE SET config_json = excluded.config_json",
                params![owner, node, config_json, now_ms()],
            )?;
            Ok(())
        })
    }

    fn pep_node_delete<'a>(&'a self, owner: &'a str, node: &'a str) -> BoxFut<'a, bool> {
        let (owner, node) = (owner.to_owned(), node.to_owned());
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let n = tx.execute(
                "DELETE FROM pep_nodes WHERE owner = ?1 AND node = ?2",
                params![owner, node],
            )?;
            tx.execute(
                "DELETE FROM pep_items WHERE owner = ?1 AND node = ?2",
                params![owner, node],
            )?;
            tx.commit()?;
            Ok(n > 0)
        })
    }

    fn pep_publish<'a>(
        &'a self,
        owner: &'a str,
        node: &'a str,
        item: PepItem,
        max_items: Option<u64>,
    ) -> BoxFut<'a, ()> {
        let (owner, node) = (owner.to_owned(), node.to_owned());
        self.write(move |conn| {
            let tx = conn.transaction()?;
            let seq: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM pep_items WHERE owner = ?1 AND node = ?2",
                params![owner, node],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO pep_items (owner, node, item_id, seq, publisher, payload, published_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (owner, node, item_id) DO UPDATE SET \
                 seq = excluded.seq, publisher = excluded.publisher, payload = excluded.payload, \
                 published_ms = excluded.published_ms",
                params![
                    owner,
                    node,
                    item.id,
                    seq,
                    item.publisher,
                    item.payload,
                    item.published_ms
                ],
            )?;
            if let Some(max) = max_items {
                tx.execute(
                    "DELETE FROM pep_items WHERE owner = ?1 AND node = ?2 AND seq NOT IN \
                     (SELECT seq FROM pep_items WHERE owner = ?1 AND node = ?2 ORDER BY seq DESC LIMIT ?3)",
                    params![owner, node, max.min(i64::MAX as u64) as i64],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn pep_items<'a>(
        &'a self,
        owner: &'a str,
        node: &'a str,
        ids: Vec<String>,
        max: Option<usize>,
    ) -> BoxFut<'a, Vec<PepItem>> {
        let (owner, node) = (owner.to_owned(), node.to_owned());
        self.read(move |conn| {
            let map = |r: &Row<'_>| {
                Ok(PepItem {
                    id: r.get(0)?,
                    publisher: r.get(1)?,
                    payload: r.get(2)?,
                    published_ms: r.get(3)?,
                })
            };
            let cols = "item_id, publisher, payload, published_ms";
            if ids.is_empty() {
                let limit = max.map(|m| m as i64).unwrap_or(-1);
                Ok(conn
                    .prepare(&format!(
                        "SELECT {cols} FROM pep_items WHERE owner = ?1 AND node = ?2 ORDER BY seq DESC LIMIT ?3"
                    ))?
                    .query_map(params![owner, node, limit], map)?
                    .collect::<rusqlite::Result<_>>()?)
            } else {
                let mut out = Vec::new();
                let mut stmt = conn.prepare(&format!(
                    "SELECT {cols} FROM pep_items WHERE owner = ?1 AND node = ?2 AND item_id = ?3"
                ))?;
                for id in ids {
                    if let Some(item) = stmt.query_row(params![owner, node, id], map).optional()? {
                        out.push(item);
                    }
                }
                Ok(out)
            }
        })
    }

    fn pep_retract<'a>(&'a self, owner: &'a str, node: &'a str, id: &'a str) -> BoxFut<'a, bool> {
        let (owner, node, id) = (owner.to_owned(), node.to_owned(), id.to_owned());
        self.write(move |conn| {
            let n = conn.execute(
                "DELETE FROM pep_items WHERE owner = ?1 AND node = ?2 AND item_id = ?3",
                params![owner, node, id],
            )?;
            Ok(n > 0)
        })
    }

    fn pep_purge<'a>(&'a self, owner: &'a str, node: &'a str) -> BoxFut<'a, ()> {
        let (owner, node) = (owner.to_owned(), node.to_owned());
        self.write(move |conn| {
            conn.execute(
                "DELETE FROM pep_items WHERE owner = ?1 AND node = ?2",
                params![owner, node],
            )?;
            Ok(())
        })
    }

    // -------------------------------------------------------------- caps

    fn caps_get<'a>(&'a self, ver: &'a str) -> BoxFut<'a, Option<Vec<String>>> {
        let ver = ver.to_owned();
        self.read(move |conn| {
            let json: Option<String> = conn
                .query_row("SELECT features_json FROM caps WHERE ver = ?1", [&ver], |r| r.get(0))
                .optional()?;
            Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
        })
    }

    fn caps_put<'a>(&'a self, ver: &'a str, features: Vec<String>) -> BoxFut<'a, ()> {
        let ver = ver.to_owned();
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO caps (ver, features_json, verified_ms) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (ver) DO NOTHING",
                params![ver, serde_json::to_string(&features)?, now_ms()],
            )?;
            Ok(())
        })
    }

    // --------------------------------------------------------------- MUC

    fn muc_room<'a>(&'a self, jid: &'a str) -> BoxFut<'a, Option<RoomRow>> {
        let jid = jid.to_owned();
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {ROOM_COLUMNS} FROM muc_rooms WHERE jid = ?1"),
                    [&jid],
                    room_from_row,
                )
                .optional()?)
        })
    }

    fn muc_rooms(&self) -> BoxFut<'_, Vec<RoomRow>> {
        self.read(move |conn| {
            Ok(conn
                .prepare(&format!("SELECT {ROOM_COLUMNS} FROM muc_rooms ORDER BY jid"))?
                .query_map([], room_from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    fn muc_room_put(&self, row: RoomRow) -> BoxFut<'_, ()> {
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO muc_rooms (jid, config_json, subject, subject_from, created_ms, last_active_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (jid) DO UPDATE SET \
                 config_json = excluded.config_json, subject = excluded.subject, \
                 subject_from = excluded.subject_from, last_active_ms = excluded.last_active_ms",
                params![
                    row.jid,
                    row.config_json,
                    row.subject,
                    row.subject_from,
                    row.created_ms,
                    row.last_active_ms
                ],
            )?;
            Ok(())
        })
    }

    fn muc_room_delete<'a>(&'a self, jid: &'a str) -> BoxFut<'a, ()> {
        let jid = jid.to_owned();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM muc_rooms WHERE jid = ?1", [&jid])?;
            tx.execute("DELETE FROM muc_affiliations WHERE room = ?1", [&jid])?;
            tx.execute("DELETE FROM archive WHERE archive_jid = ?1", [&jid])?;
            tx.commit()?;
            Ok(())
        })
    }

    fn muc_affiliations<'a>(&'a self, room: &'a str) -> BoxFut<'a, Vec<RoomAffiliation>> {
        let room = room.to_owned();
        self.read(move |conn| {
            Ok(conn
                .prepare("SELECT jid, affiliation, nick, reason FROM muc_affiliations WHERE room = ?1 ORDER BY jid")?
                .query_map([&room], |r| {
                    Ok(RoomAffiliation {
                        jid: r.get(0)?,
                        affiliation: r.get(1)?,
                        nick: r.get(2)?,
                        reason: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    fn muc_affiliation_set<'a>(&'a self, room: &'a str, aff: RoomAffiliation, remove: bool) -> BoxFut<'a, ()> {
        let room = room.to_owned();
        self.write(move |conn| {
            if remove {
                conn.execute(
                    "DELETE FROM muc_affiliations WHERE room = ?1 AND jid = ?2",
                    params![room, aff.jid],
                )?;
            } else {
                conn.execute(
                    "INSERT INTO muc_affiliations (room, jid, affiliation, nick, reason) VALUES (?1, ?2, ?3, ?4, ?5) \
                     ON CONFLICT (room, jid) DO UPDATE SET affiliation = excluded.affiliation, \
                     nick = excluded.nick, reason = excluded.reason",
                    params![room, aff.jid, aff.affiliation, aff.nick, aff.reason],
                )?;
            }
            Ok(())
        })
    }

    // ------------------------------------------- blocking, private storage

    fn blocklist<'a>(&'a self, account: &'a str) -> BoxFut<'a, Vec<String>> {
        let account = account.to_owned();
        self.read(move |conn| {
            Ok(conn
                .prepare("SELECT jid FROM blocklist WHERE account = ?1 ORDER BY jid")?
                .query_map([&account], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    fn blocklist_update<'a>(
        &'a self,
        account: &'a str,
        block: Vec<String>,
        unblock: Vec<String>,
        clear: bool,
    ) -> BoxFut<'a, ()> {
        let account = account.to_owned();
        self.write(move |conn| {
            let tx = conn.transaction()?;
            if clear {
                tx.execute("DELETE FROM blocklist WHERE account = ?1", [&account])?;
            }
            for j in unblock {
                tx.execute(
                    "DELETE FROM blocklist WHERE account = ?1 AND jid = ?2",
                    params![account, j],
                )?;
            }
            for j in block {
                tx.execute(
                    "INSERT INTO blocklist (account, jid) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
                    params![account, j],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn private_get<'a>(&'a self, account: &'a str, key: &'a str) -> BoxFut<'a, Option<String>> {
        let (account, key) = (account.to_owned(), key.to_owned());
        self.read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT xml FROM private_xml WHERE account = ?1 AND key = ?2",
                    params![account, key],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }

    fn private_put<'a>(&'a self, account: &'a str, key: &'a str, xml: Option<String>) -> BoxFut<'a, ()> {
        let (account, key) = (account.to_owned(), key.to_owned());
        self.write(move |conn| {
            match xml {
                Some(x) => conn.execute(
                    "INSERT INTO private_xml (account, key, xml) VALUES (?1, ?2, ?3) \
                     ON CONFLICT (account, key) DO UPDATE SET xml = excluded.xml",
                    params![account, key, x],
                )?,
                None => conn.execute(
                    "DELETE FROM private_xml WHERE account = ?1 AND key = ?2",
                    params![account, key],
                )?,
            };
            Ok(())
        })
    }

    // ----------------------------------------------------------- invites

    fn invite_put<'a>(
        &'a self,
        token_hash: &'a str,
        expires_ms: i64,
        uses: u32,
        note: Option<String>,
    ) -> BoxFut<'a, ()> {
        let token_hash = token_hash.to_owned();
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO invites (token_hash, created_ms, expires_ms, uses_left, note) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![token_hash, now_ms(), expires_ms, uses as i64, note],
            )?;
            Ok(())
        })
    }

    fn invite_valid<'a>(&'a self, token_hash: &'a str, now_ms: i64) -> BoxFut<'a, bool> {
        let token_hash = token_hash.to_owned();
        self.read(move |conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM invites WHERE token_hash = ?1 AND expires_ms > ?2 AND uses_left > 0",
                params![token_hash, now_ms],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    fn invite_use<'a>(&'a self, token_hash: &'a str, now_ms: i64) -> BoxFut<'a, bool> {
        let token_hash = token_hash.to_owned();
        self.write(move |conn| {
            let n = conn.execute(
                "UPDATE invites SET uses_left = uses_left - 1 \
                 WHERE token_hash = ?1 AND expires_ms > ?2 AND uses_left > 0",
                params![token_hash, now_ms],
            )?;
            Ok(n > 0)
        })
    }
}

const ARCHIVE_COLUMNS: &str = "id, archive_jid, ts_ms, with_jid, direction, origin_id, from_jid, stanza";

fn archive_from_row(r: &Row<'_>) -> rusqlite::Result<ArchiveRow> {
    let dir: String = r.get(4)?;
    Ok(ArchiveRow {
        id: r.get(0)?,
        archive_jid: r.get(1)?,
        ts_ms: r.get(2)?,
        with_jid: r.get(3)?,
        direction: if dir == "out" { Direction::Out } else { Direction::In },
        origin_id: r.get(5)?,
        from_jid: r.get(6)?,
        stanza: r.get(7)?,
    })
}

const ROOM_COLUMNS: &str = "jid, config_json, subject, subject_from, created_ms, last_active_ms";

fn room_from_row(r: &Row<'_>) -> rusqlite::Result<RoomRow> {
    Ok(RoomRow {
        jid: r.get(0)?,
        config_json: r.get(1)?,
        subject: r.get(2)?,
        subject_from: r.get(3)?,
        created_ms: r.get(4)?,
        last_active_ms: r.get(5)?,
    })
}

/// XEP-0313 + XEP-0059 paging over one archive (§5.7).
fn query_archive(conn: &Connection, q: &ArchiveQuery) -> Result<ArchivePage> {
    use rusqlite::types::Value;
    let seq_of = |id: &str| -> Result<Option<i64>> {
        Ok(conn
            .query_row(
                "SELECT seq FROM archive WHERE archive_jid = ?1 AND id = ?2",
                params![q.archive_jid, id],
                |r| r.get(0),
            )
            .optional()?)
    };
    let mut page = ArchivePage::default();
    let mut filter = String::from("archive_jid = ?");
    let mut args: Vec<Value> = vec![Value::Text(q.archive_jid.clone())];
    if let Some(w) = &q.with {
        filter.push_str(" AND with_jid = ?");
        args.push(Value::Text(w.clone()));
    }
    if let Some(s) = q.start_ms {
        filter.push_str(" AND ts_ms >= ?");
        args.push(Value::Integer(s));
    }
    if let Some(e) = q.end_ms {
        filter.push_str(" AND ts_ms <= ?");
        args.push(Value::Integer(e));
    }
    for (id, op) in [(&q.after_id, ">"), (&q.before_id, "<")] {
        if let Some(id) = id {
            match seq_of(id)? {
                Some(s) => {
                    filter.push_str(&format!(" AND seq {op} ?"));
                    args.push(Value::Integer(s));
                }
                None => {
                    page.id_not_found = true;
                    return Ok(page);
                }
            }
        }
    }
    if !q.ids.is_empty() {
        filter.push_str(&format!(" AND id IN ({})", vec!["?"; q.ids.len()].join(", ")));
        args.extend(q.ids.iter().map(|i| Value::Text(i.clone())));
    }
    let count: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM archive WHERE {filter}"),
        rusqlite::params_from_iter(args.iter()),
        |r| r.get(0),
    )?;
    page.count = count as u64;
    if q.max == 0 {
        page.complete = count == 0;
        return Ok(page);
    }
    let mut bound = String::new();
    let mut bound_args = Vec::new();
    let mut backwards = false;
    if let Some(after) = &q.rsm_after {
        match seq_of(after)? {
            Some(s) => {
                bound.push_str(" AND seq > ?");
                bound_args.push(Value::Integer(s));
            }
            None => {
                page.id_not_found = true;
                return Ok(page);
            }
        }
    }
    if let Some(before) = &q.rsm_before {
        backwards = true;
        if !before.is_empty() {
            match seq_of(before)? {
                Some(s) => {
                    bound.push_str(" AND seq < ?");
                    bound_args.push(Value::Integer(s));
                }
                None => {
                    page.id_not_found = true;
                    return Ok(page);
                }
            }
        }
    }
    let order = if backwards { "DESC" } else { "ASC" };
    let sql = format!(
        "SELECT {ARCHIVE_COLUMNS} FROM archive WHERE {filter}{bound} ORDER BY seq {order} LIMIT {}",
        q.max + 1
    );
    let all: Vec<Value> = args.into_iter().chain(bound_args).collect();
    let mut rows: Vec<ArchiveRow> = conn
        .prepare(&sql)?
        .query_map(rusqlite::params_from_iter(all.iter()), archive_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    page.complete = rows.len() <= q.max;
    rows.truncate(q.max);
    if backwards {
        rows.reverse();
    }
    page.rows = rows;
    Ok(page)
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
