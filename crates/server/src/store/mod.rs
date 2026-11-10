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
    /// `None` for accounts imported with SCRAM-SHA-1 keys only (Prosody).
    pub sha256: Option<ScramKeys>,
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
    /// Imported from another server (Prosody).
    Import,
}

impl CreatedVia {
    pub fn as_str(self) -> &'static str {
        match self {
            CreatedVia::Admin => "admin",
            CreatedVia::Invite => "invite",
            CreatedVia::Pow => "pow",
            CreatedVia::Open => "open",
            CreatedVia::Import => "import",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "admin" => CreatedVia::Admin,
            "invite" => CreatedVia::Invite,
            "pow" => CreatedVia::Pow,
            "open" => CreatedVia::Open,
            "import" => CreatedVia::Import,
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

    // ------------------------------------------------------------ roster

    fn roster<'a>(&'a self, account: &'a str) -> BoxFut<'a, Vec<RosterItem>>;
    fn roster_put<'a>(&'a self, account: &'a str, item: RosterItem) -> BoxFut<'a, ()>;
    fn roster_remove<'a>(&'a self, account: &'a str, contact: &'a str) -> BoxFut<'a, ()>;

    // ------------------------------------------------ archive and offline

    /// Insert an archive row; the store assigns its order.
    fn archive_insert(&self, row: ArchiveRow) -> BoxFut<'_, ()>;
    fn archive_query(&self, q: ArchiveQuery) -> BoxFut<'_, ArchivePage>;
    fn archive_get<'a>(&'a self, archive_jid: &'a str, id: &'a str) -> BoxFut<'a, Option<ArchiveRow>>;
    /// Delete rows older than `before_ms`; returns how many.
    fn archive_expire(&self, before_ms: i64) -> BoxFut<'_, u64>;
    fn archive_clear<'a>(&'a self, archive_jid: &'a str) -> BoxFut<'a, ()>;
    /// Queue a stanza for an offline account, keeping at most `cap` (oldest dropped).
    fn offline_push<'a>(
        &'a self,
        account: &'a str,
        archive_id: Option<String>,
        stanza: String,
        cap: usize,
    ) -> BoxFut<'a, ()>;
    /// Take (and delete) everything queued for `account`, oldest first: (stanza, added_ms).
    fn offline_take<'a>(&'a self, account: &'a str) -> BoxFut<'a, Vec<(String, i64)>>;
    fn offline_count<'a>(&'a self, account: &'a str) -> BoxFut<'a, u64>;

    // --------------------------------------------------------------- PEP

    /// (node, config JSON) for every node of `owner`.
    fn pep_nodes<'a>(&'a self, owner: &'a str) -> BoxFut<'a, Vec<(String, String)>>;
    fn pep_node_put<'a>(&'a self, owner: &'a str, node: &'a str, config_json: String) -> BoxFut<'a, ()>;
    /// `false` if there was no such node. Deletes its items too.
    fn pep_node_delete<'a>(&'a self, owner: &'a str, node: &'a str) -> BoxFut<'a, bool>;
    /// Insert or replace an item (it becomes the newest), then keep only the
    /// newest `max_items` if given.
    fn pep_publish<'a>(
        &'a self,
        owner: &'a str,
        node: &'a str,
        item: PepItem,
        max_items: Option<u64>,
    ) -> BoxFut<'a, ()>;
    /// Items newest first: all (up to `max`), or those with the given ids.
    fn pep_items<'a>(
        &'a self,
        owner: &'a str,
        node: &'a str,
        ids: Vec<String>,
        max: Option<usize>,
    ) -> BoxFut<'a, Vec<PepItem>>;
    fn pep_retract<'a>(&'a self, owner: &'a str, node: &'a str, id: &'a str) -> BoxFut<'a, bool>;
    fn pep_purge<'a>(&'a self, owner: &'a str, node: &'a str) -> BoxFut<'a, ()>;

    // -------------------------------------------------------------- caps

    fn caps_get<'a>(&'a self, ver: &'a str) -> BoxFut<'a, Option<Vec<String>>>;
    fn caps_put<'a>(&'a self, ver: &'a str, features: Vec<String>) -> BoxFut<'a, ()>;

    // --------------------------------------------------------------- MUC

    fn muc_room<'a>(&'a self, jid: &'a str) -> BoxFut<'a, Option<RoomRow>>;
    fn muc_rooms(&self) -> BoxFut<'_, Vec<RoomRow>>;
    fn muc_room_put(&self, row: RoomRow) -> BoxFut<'_, ()>;
    /// Deletes the room, its affiliations and its archive.
    fn muc_room_delete<'a>(&'a self, jid: &'a str) -> BoxFut<'a, ()>;
    fn muc_affiliations<'a>(&'a self, room: &'a str) -> BoxFut<'a, Vec<RoomAffiliation>>;
    /// `affiliation = None` removes the row (affiliation `none`).
    fn muc_affiliation_set<'a>(&'a self, room: &'a str, aff: RoomAffiliation, remove: bool) -> BoxFut<'a, ()>;

    // ------------------------------------------- blocking, private storage

    fn blocklist<'a>(&'a self, account: &'a str) -> BoxFut<'a, Vec<String>>;
    /// Add `block`, remove `unblock`; `clear` removes everything first.
    fn blocklist_update<'a>(
        &'a self,
        account: &'a str,
        block: Vec<String>,
        unblock: Vec<String>,
        clear: bool,
    ) -> BoxFut<'a, ()>;
    fn private_get<'a>(&'a self, account: &'a str, key: &'a str) -> BoxFut<'a, Option<String>>;
    /// `None` deletes.
    fn private_put<'a>(&'a self, account: &'a str, key: &'a str, xml: Option<String>) -> BoxFut<'a, ()>;

    // ----------------------------------------------------------- invites

    fn invite_put<'a>(
        &'a self,
        token_hash: &'a str,
        expires_ms: i64,
        uses: u32,
        note: Option<String>,
    ) -> BoxFut<'a, ()>;
    /// Whether the invitation exists, hasn't expired and has uses left.
    fn invite_valid<'a>(&'a self, token_hash: &'a str, now_ms: i64) -> BoxFut<'a, bool>;
    /// Use one of its uses; `false` if it isn't valid.
    fn invite_use<'a>(&'a self, token_hash: &'a str, now_ms: i64) -> BoxFut<'a, bool>;
}

// ------------------------------------------------------------- data types

/// RFC 6121 subscription states (the `subscription` attribute).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Subscription {
    None,
    To,
    From,
    Both,
}

impl Subscription {
    pub fn as_str(self) -> &'static str {
        match self {
            Subscription::None => "none",
            Subscription::To => "to",
            Subscription::From => "from",
            Subscription::Both => "both",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Subscription::None,
            "to" => Subscription::To,
            "from" => Subscription::From,
            "both" => Subscription::Both,
            _ => return None,
        })
    }

    /// The contact receives our presence.
    pub fn has_from(self) -> bool {
        matches!(self, Subscription::From | Subscription::Both)
    }

    /// We receive the contact's presence.
    pub fn has_to(self) -> bool {
        matches!(self, Subscription::To | Subscription::Both)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterItem {
    /// Bare JID.
    pub contact: String,
    pub name: Option<String>,
    pub subscription: Subscription,
    /// We asked to subscribe and have no answer yet ("ask='subscribe'").
    pub ask_out: bool,
    /// They asked to subscribe and we haven't answered.
    pub pending_in: bool,
    /// Pre-approved inbound subscription.
    pub approved: bool,
    /// The row only holds a pending inbound request from someone who is
    /// not in the roster (RFC 6121 §3.1.3); roster gets don't show it.
    pub hidden: bool,
    pub groups: Vec<String>,
}

impl RosterItem {
    pub fn new(contact: &str) -> Self {
        Self {
            contact: contact.to_owned(),
            name: None,
            subscription: Subscription::None,
            ask_out: false,
            pending_in: false,
            approved: false,
            hidden: false,
            groups: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRow {
    /// Also the XEP-0359 stanza-id.
    pub id: String,
    /// The account or room the archive belongs to.
    pub archive_jid: String,
    pub ts_ms: i64,
    /// The peer's bare JID (rooms: the sender's real bare JID, or the room).
    pub with_jid: String,
    pub direction: Direction,
    pub origin_id: Option<String>,
    pub from_jid: String,
    /// The serialized stanza as it was delivered.
    pub stanza: String,
}

/// A MAM query after form and RSM parsing (§5.7).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveQuery {
    pub archive_jid: String,
    pub with: Option<String>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    /// `#extended` exclusive bounds.
    pub after_id: Option<String>,
    pub before_id: Option<String>,
    /// `#extended` explicit ids.
    pub ids: Vec<String>,
    /// RSM `max` (already capped).
    pub max: usize,
    /// RSM `after`.
    pub rsm_after: Option<String>,
    /// RSM `before`; `Some("")` asks for the last page.
    pub rsm_before: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchivePage {
    /// In archive order (oldest first).
    pub rows: Vec<ArchiveRow>,
    /// No more results in the paging direction.
    pub complete: bool,
    /// Rows matching the filter, ignoring RSM.
    pub count: u64,
    /// An RSM or `#extended` id was not in the archive.
    pub id_not_found: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PepItem {
    pub id: String,
    pub publisher: String,
    /// Serialized payload element.
    pub payload: String,
    pub published_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomRow {
    pub jid: String,
    pub config_json: String,
    pub subject: Option<String>,
    pub subject_from: Option<String>,
    pub created_ms: i64,
    pub last_active_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomAffiliation {
    /// Bare JID.
    pub jid: String,
    /// owner | admin | member | outcast
    pub affiliation: String,
    pub nick: Option<String>,
    pub reason: Option<String>,
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
        sha256: Some(ScramKeys {
            stored_key: vec![b; 32],
            server_key: vec![b + 1; 32],
        }),
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

    im_contract_tests(store).await;
}

#[cfg(test)]
async fn im_contract_tests(store: &dyn Store) {
    // Roster.
    let a = "alice@hub.local";
    let mut item = RosterItem::new("bob@hub.local");
    item.ask_out = true;
    item.groups = vec!["Friends".into()];
    store.roster_put(a, item.clone()).await.unwrap();
    item.subscription = Subscription::To;
    item.ask_out = false;
    store.roster_put(a, item.clone()).await.unwrap();
    assert_eq!(store.roster(a).await.unwrap(), vec![item]);
    store.roster_remove(a, "bob@hub.local").await.unwrap();
    assert!(store.roster(a).await.unwrap().is_empty());

    // Archive paging (§9 M3 acceptance test 2 at the store level).
    for i in 0..10 {
        store
            .archive_insert(ArchiveRow {
                id: format!("id{i}"),
                archive_jid: a.into(),
                ts_ms: 1000 + i,
                with_jid: if i % 2 == 0 { "bob@hub.local" } else { "carol@hub.local" }.into(),
                direction: Direction::In,
                origin_id: None,
                from_jid: "bob@hub.local/r".into(),
                stanza: format!("<message xmlns='jabber:client'><body>{i}</body></message>"),
            })
            .await
            .unwrap();
    }
    let ids = |p: &ArchivePage| p.rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    let q = |max| ArchiveQuery {
        archive_jid: a.into(),
        max,
        ..Default::default()
    };
    // Last page.
    let p = store
        .archive_query(ArchiveQuery {
            rsm_before: Some(String::new()),
            ..q(3)
        })
        .await
        .unwrap();
    assert_eq!(ids(&p), ["id7", "id8", "id9"]);
    assert!(!p.complete);
    assert_eq!(p.count, 10);
    // Everything fits: complete.
    let p = store
        .archive_query(ArchiveQuery {
            rsm_before: Some(String::new()),
            ..q(50)
        })
        .await
        .unwrap();
    assert_eq!(p.rows.len(), 10);
    assert!(p.complete);
    // After the last id: empty and complete.
    let p = store
        .archive_query(ArchiveQuery {
            rsm_after: Some("id9".into()),
            ..q(5)
        })
        .await
        .unwrap();
    assert!(p.rows.is_empty() && p.complete);
    // Forward pages cover everything once.
    let mut seen = Vec::new();
    let mut after = None;
    loop {
        let p = store
            .archive_query(ArchiveQuery {
                rsm_after: after.clone(),
                ..q(4)
            })
            .await
            .unwrap();
        seen.extend(ids(&p));
        after = p.rows.last().map(|r| r.id.clone());
        if p.complete {
            break;
        }
    }
    assert_eq!(seen.len(), 10);
    // Backwards from an id; filters; unknown ids; max = 0.
    let p = store
        .archive_query(ArchiveQuery {
            rsm_before: Some("id5".into()),
            ..q(2)
        })
        .await
        .unwrap();
    assert_eq!(ids(&p), ["id3", "id4"]);
    let p = store
        .archive_query(ArchiveQuery {
            with: Some("carol@hub.local".into()),
            start_ms: Some(1002),
            end_ms: Some(1007),
            ..q(10)
        })
        .await
        .unwrap();
    assert_eq!(ids(&p), ["id3", "id5", "id7"]);
    let p = store
        .archive_query(ArchiveQuery {
            rsm_after: Some("nope".into()),
            ..q(10)
        })
        .await
        .unwrap();
    assert!(p.id_not_found);
    let p = store.archive_query(q(0)).await.unwrap();
    assert!(p.rows.is_empty() && p.count == 10 && !p.complete);
    let p = store
        .archive_query(ArchiveQuery {
            ids: vec!["id2".into(), "id4".into()],
            ..q(10)
        })
        .await
        .unwrap();
    assert_eq!(ids(&p), ["id2", "id4"]);
    assert!(store.archive_get(a, "id4").await.unwrap().is_some());
    assert_eq!(store.archive_expire(1005).await.unwrap(), 5);
    assert_eq!(store.archive_query(q(0)).await.unwrap().count, 5);

    // Offline queue with a cap.
    for i in 0..4 {
        store
            .offline_push(a, Some(format!("id{i}")), format!("<m{i}/>"), 3)
            .await
            .unwrap();
    }
    assert_eq!(store.offline_count(a).await.unwrap(), 3);
    let taken: Vec<String> = store.offline_take(a).await.unwrap().into_iter().map(|t| t.0).collect();
    assert_eq!(taken, ["<m1/>", "<m2/>", "<m3/>"]);
    assert_eq!(store.offline_count(a).await.unwrap(), 0);

    // PEP: replace, trim, order.
    store.pep_node_put(a, "n", "{}".into()).await.unwrap();
    let item = |id: &str| PepItem {
        id: id.into(),
        publisher: a.into(),
        payload: format!("<p id='{id}'/>"),
        published_ms: 1,
    };
    for id in ["1", "2", "3", "1"] {
        store.pep_publish(a, "n", item(id), Some(2)).await.unwrap();
    }
    let got: Vec<String> = store
        .pep_items(a, "n", vec![], None)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect();
    assert_eq!(got, ["1", "3"]);
    assert_eq!(store.pep_items(a, "n", vec!["3".into()], None).await.unwrap().len(), 1);
    assert!(store.pep_retract(a, "n", "3").await.unwrap());
    assert_eq!(
        store.pep_nodes(a).await.unwrap(),
        vec![("n".to_owned(), "{}".to_owned())]
    );
    assert!(store.pep_node_delete(a, "n").await.unwrap());
    assert!(store.pep_items(a, "n", vec![], None).await.unwrap().is_empty());

    // Caps, MUC, blocking, private storage.
    store.caps_put("ver1", vec!["f1".into()]).await.unwrap();
    assert_eq!(store.caps_get("ver1").await.unwrap(), Some(vec!["f1".to_owned()]));
    let room = RoomRow {
        jid: "r@conference.hub.local".into(),
        config_json: "{}".into(),
        subject: Some("hi".into()),
        subject_from: None,
        created_ms: 1,
        last_active_ms: 1,
    };
    store.muc_room_put(room.clone()).await.unwrap();
    assert_eq!(store.muc_room(&room.jid).await.unwrap(), Some(room.clone()));
    let aff = RoomAffiliation {
        jid: a.into(),
        affiliation: "owner".into(),
        nick: None,
        reason: None,
    };
    store.muc_affiliation_set(&room.jid, aff.clone(), false).await.unwrap();
    assert_eq!(store.muc_affiliations(&room.jid).await.unwrap(), vec![aff.clone()]);
    store.muc_room_delete(&room.jid).await.unwrap();
    assert!(store.muc_rooms().await.unwrap().is_empty());
    assert!(store.muc_affiliations(&room.jid).await.unwrap().is_empty());
    store
        .blocklist_update(a, vec!["x@y".into(), "z@y".into()], vec![], false)
        .await
        .unwrap();
    store
        .blocklist_update(a, vec![], vec!["x@y".into()], false)
        .await
        .unwrap();
    assert_eq!(store.blocklist(a).await.unwrap(), ["z@y"]);
    store.private_put(a, "vcard", Some("<vCard/>".into())).await.unwrap();
    assert_eq!(
        store.private_get(a, "vcard").await.unwrap().as_deref(),
        Some("<vCard/>")
    );
    store.private_put(a, "vcard", None).await.unwrap();
    assert!(store.private_get(a, "vcard").await.unwrap().is_none());

    // Invitations: uses and expiry.
    store.invite_put("h1", 2000, 2, None).await.unwrap();
    assert!(store.invite_valid("h1", 1000).await.unwrap());
    assert!(!store.invite_valid("h1", 3000).await.unwrap(), "expired");
    assert!(store.invite_use("h1", 1000).await.unwrap());
    assert!(store.invite_use("h1", 1000).await.unwrap());
    assert!(!store.invite_use("h1", 1000).await.unwrap(), "used up");
    assert!(!store.invite_valid("nope", 1000).await.unwrap());
}
