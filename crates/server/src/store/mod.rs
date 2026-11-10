//! Persistent state behind the `Store` trait (§5.8).
//!
//! Handlers only use the trait. v1 ships SQLite; Postgres is a second
//! implementation later, checked by the same contract tests
//! (`contract_tests` below).

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod sqlite;

pub use sqlite::SqliteStore;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// SCRAM verifier for one hash function: never anything password-equivalent.
#[derive(Clone, PartialEq, Eq)]
pub struct ScramKeys {
    pub stored_key: Vec<u8>,
    pub server_key: Vec<u8>,
}

impl std::fmt::Debug for ScramKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScramKeys(..)")
    }
}

/// What the hub stores per account to verify passwords (§6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub salt: Vec<u8>,
    pub iterations: u32,
    pub sha1: ScramKeys,
    pub sha256: ScramKeys,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
    Probation,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
            Role::Probation => "probation",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "admin" => Role::Admin,
            "user" => Role::User,
            "probation" => Role::Probation,
            other => anyhow::bail!("unknown role {other:?}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountState {
    Active,
    Locked,
}

impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            AccountState::Active => "active",
            AccountState::Locked => "locked",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "active" => AccountState::Active,
            "locked" => AccountState::Locked,
            other => anyhow::bail!("unknown account state {other:?}"),
        })
    }
}

/// How an account came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CreatedVia {
    Admin,
    Invite,
    Pow,
    Open,
}

impl CreatedVia {
    pub fn as_str(self) -> &'static str {
        match self {
            CreatedVia::Admin => "admin",
            CreatedVia::Invite => "invite",
            CreatedVia::Pow => "pow",
            CreatedVia::Open => "open",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "admin" => CreatedVia::Admin,
            "invite" => CreatedVia::Invite,
            "pow" => CreatedVia::Pow,
            "open" => CreatedVia::Open,
            other => anyhow::bail!("unknown creation method {other:?}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub domain: String,
    pub localpart: String,
    pub role: Role,
    pub state: AccountState,
    pub created_ms: i64,
    pub created_via: CreatedVia,
    pub last_login_ms: Option<i64>,
    pub credentials: Credentials,
    pub password_changed_ms: i64,
}

impl Account {
    pub fn jid(&self) -> String {
        format!("{}@{}", self.localpart, self.domain)
    }
}

#[derive(Debug, Clone)]
pub struct NewAccount {
    pub domain: String,
    pub localpart: String,
    pub role: Role,
    pub created_via: CreatedVia,
    pub credentials: Credentials,
}

pub trait Store: Send + Sync + 'static {
    /// Register the hosted domain on first start; fail if the database
    /// belongs to another domain (domains are permanent).
    fn ensure_host<'a>(&'a self, domain: &'a str) -> BoxFut<'a, ()>;

    /// A random secret kept in `meta`, created on first use.
    fn secret<'a>(&'a self, key: &'a str, len: usize) -> BoxFut<'a, Vec<u8>>;

    /// `None` if the account already exists.
    fn create_account(&self, account: NewAccount) -> BoxFut<'_, Option<Account>>;

    fn account<'a>(&'a self, domain: &'a str, localpart: &'a str) -> BoxFut<'a, Option<Account>>;

    fn list_accounts<'a>(&'a self, domain: &'a str) -> BoxFut<'a, Vec<Account>>;

    fn count_accounts<'a>(&'a self, domain: &'a str) -> BoxFut<'a, u64>;

    /// `false` if there is no such account.
    fn set_credentials<'a>(&'a self, domain: &'a str, localpart: &'a str, credentials: Credentials)
    -> BoxFut<'a, bool>;

    /// `false` if there is no such account.
    fn delete_account<'a>(&'a self, domain: &'a str, localpart: &'a str) -> BoxFut<'a, bool>;

    fn record_login<'a>(&'a self, domain: &'a str, localpart: &'a str, ts_ms: i64) -> BoxFut<'a, ()>;
}

/// The contract every `Store` implementation must meet. Run against SQLite
/// here; the Postgres backend will call the same function.
#[cfg(test)]
pub(crate) async fn contract_tests(store: &dyn Store) {
    let creds = |b: u8| Credentials {
        salt: vec![b; 16],
        iterations: 4096,
        sha1: ScramKeys {
            stored_key: vec![b; 20],
            server_key: vec![b + 1; 20],
        },
        sha256: ScramKeys {
            stored_key: vec![b; 32],
            server_key: vec![b + 1; 32],
        },
    };
    store.ensure_host("hub.local").await.unwrap();
    store.ensure_host("hub.local").await.unwrap();
    assert!(
        store.ensure_host("other.local").await.is_err(),
        "the domain is permanent"
    );

    let s1 = store.secret("scram_mock_key", 32).await.unwrap();
    assert_eq!(s1.len(), 32);
    assert_eq!(store.secret("scram_mock_key", 32).await.unwrap(), s1);

    let new = |name: &str, role| NewAccount {
        domain: "hub.local".into(),
        localpart: name.into(),
        role,
        created_via: CreatedVia::Admin,
        credentials: creds(1),
    };
    let a = store.create_account(new("alice", Role::Admin)).await.unwrap().unwrap();
    assert_eq!(a.jid(), "alice@hub.local");
    assert!(store.create_account(new("alice", Role::User)).await.unwrap().is_none());
    store.create_account(new("bob", Role::User)).await.unwrap().unwrap();
    assert_eq!(store.count_accounts("hub.local").await.unwrap(), 2);

    let got = store.account("hub.local", "alice").await.unwrap().unwrap();
    assert_eq!(got, a);
    assert!(store.account("hub.local", "carol").await.unwrap().is_none());

    assert!(store.set_credentials("hub.local", "alice", creds(7)).await.unwrap());
    assert!(!store.set_credentials("hub.local", "carol", creds(7)).await.unwrap());
    let got = store.account("hub.local", "alice").await.unwrap().unwrap();
    assert_eq!(got.credentials, creds(7));

    store.record_login("hub.local", "bob", 1234).await.unwrap();
    let names: Vec<(String, Option<i64>)> = store
        .list_accounts("hub.local")
        .await
        .unwrap()
        .into_iter()
        .map(|a| (a.localpart, a.last_login_ms))
        .collect();
    assert_eq!(names, vec![("alice".into(), None), ("bob".into(), Some(1234))]);

    assert!(store.delete_account("hub.local", "bob").await.unwrap());
    assert!(!store.delete_account("hub.local", "bob").await.unwrap());
    assert_eq!(store.count_accounts("hub.local").await.unwrap(), 1);
}
