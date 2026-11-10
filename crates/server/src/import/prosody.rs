//! Import a Prosody server's internal storage (`storage = "internal"`, the
//! files under `data_path`, e.g. `/var/lib/prosody`) into the hub.
//!
//! Credentials are imported as they are: Prosody's `internal_hashed`
//! stores SCRAM `stored_key`/`server_key` (hex), the salt and the iteration
//! count, for SCRAM-SHA-1 unless `password_hash = "SHA-256"`. The hub's own
//! SCRAM uses exactly those values, so passwords keep working without
//! anyone knowing them. Accounts with SHA-1 keys only can't use
//! SCRAM-SHA-256; see [`ImportReport::sha1_only`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_xmpp::minidom::Element;

use super::lua::{self, Key, Table, Value};
use crate::muc::{Affiliation, RoomConfig, Whois};
use crate::stanza::{self, NS_CLIENT};
use crate::store::{
    ArchiveRow, CreatedVia, Credentials, Direction, NewAccount, PepItem, Role, RoomAffiliation, RoomRow, RosterItem,
    ScramKeys, Store, Subscription,
};
use crate::user::pep::NodeConfig;
use crate::util::now_ms;

#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// Prosody's `data_path`.
    pub data_path: PathBuf,
    /// The Prosody host to import (default: the hub's domain).
    pub host: Option<String>,
    /// Import message archives (MAM and room logs).
    pub archives: bool,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct ImportReport {
    pub accounts: usize,
    /// Accounts that already existed in the hub (left alone).
    pub accounts_skipped: Vec<String>,
    /// Accounts with SCRAM-SHA-1 keys only (Prosody's default).
    pub sha1_only: usize,
    /// Accounts with SCRAM-SHA-256 keys only (`password_hash = "SHA-256"`).
    pub sha256_only: usize,
    pub roster_items: usize,
    pub pep_nodes: usize,
    pub pep_items: usize,
    pub archived_messages: usize,
    pub offline_messages: usize,
    pub blocklist_entries: usize,
    pub private_items: usize,
    pub rooms: usize,
    pub room_affiliations: usize,
    pub room_messages: usize,
    pub warnings: Vec<String>,
}

/// File-name encoding, the inverse of [`lua::decode_name`].
pub fn encode_name(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'_' {
                (b as char).to_string()
            } else {
                format!("%{b:02x}")
            }
        })
        .collect()
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

/// `(decoded stem, path)` of the files in `dir` with extension `ext`.
fn files(dir: &Path, ext: &str) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(ext))
        .filter_map(|p| Some((lua::decode_name(p.file_stem()?.to_str()?), p)))
        .collect();
    out.sort();
    out
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A Prosody stanza table (`{ name, attr, [1..] = children or text }`).
pub fn element(t: &Table, parent_ns: &str) -> Option<Element> {
    let name = t.str("name")?;
    let attr = t.table("attr");
    let ns = attr.and_then(|a| a.str("xmlns")).unwrap_or(parent_ns).to_owned();
    let mut el = Element::bare(name, ns.as_str());
    if let Some(a) = attr {
        for (k, v) in a.string_entries() {
            // xml:lang and other prefixed attributes, and Prosody's own `stamp`.
            if k == "xmlns" || k.contains(':') || (k == "stamp" && matches!(name, "message" | "presence" | "iq")) {
                continue;
            }
            if let Some(v) = v.as_str() {
                stanza::set_attr(&mut el, k, v);
            }
        }
    }
    for child in &t.array {
        match child {
            Value::Str(s) => el.append_text(s.as_str()),
            Value::Table(c) => {
                if let Some(e) = element(c, &ns) {
                    el.append_child(e);
                }
            }
            _ => {}
        }
    }
    Some(el)
}

fn when_ms(t: &Table) -> i64 {
    t.get("when")
        .and_then(Value::as_f64)
        .map(|w| (w * 1000.0) as i64)
        .unwrap_or_else(now_ms)
}

/// The pieces of Prosody's account file we use.
fn credentials(t: &Table, iterations_default: u32) -> Option<(Credentials, bool, bool)> {
    let salt = t.str("salt")?.as_bytes().to_vec();
    let iterations = t
        .get("iteration_count")
        .and_then(Value::as_f64)
        .map(|n| n as u32)
        .unwrap_or(iterations_default);
    let stored = unhex(t.str("stored_key")?)?;
    let server = unhex(t.str("server_key")?)?;
    let keys = ScramKeys {
        stored_key: stored,
        server_key: server,
    };
    let random = |len: usize| ScramKeys {
        stored_key: crate::util::random_bytes::<32>()[..len].to_vec(),
        server_key: crate::util::random_bytes::<32>()[..len].to_vec(),
    };
    match keys.stored_key.len() {
        20 => Some((
            Credentials {
                salt,
                iterations,
                sha1: keys,
                sha256: None,
            },
            true,
            false,
        )),
        // password_hash = "SHA-256": no usable SHA-1 keys.
        32 => Some((
            Credentials {
                salt,
                iterations,
                sha1: random(20),
                sha256: Some(keys),
            },
            false,
            true,
        )),
        _ => None,
    }
}

/// Import everything for `domain` from Prosody's data directory.
pub async fn import(store: &Arc<dyn Store>, domain: &str, opts: &ImportOptions) -> Result<ImportReport> {
    let host = opts.host.clone().unwrap_or_else(|| domain.to_owned());
    let base = opts.data_path.join(encode_name(&host));
    if !base.is_dir() {
        anyhow::bail!(
            "{} has no data for {host} (expected {})",
            opts.data_path.display(),
            base.display()
        );
    }
    let mut r = ImportReport::default();
    let bare = |user: &str| format!("{user}@{domain}");

    // Accounts.
    let mut users = Vec::new();
    for (user, path) in files(&base.join("accounts"), "dat") {
        let Ok(Value::Table(t)) = lua::parse_dat(&read(&path)?) else {
            r.warnings.push(format!("{user}: unreadable account file"));
            continue;
        };
        let creds = match credentials(&t, 10_000) {
            Some(c) => c,
            None if t.str("password").is_some() => {
                // internal_plain: derive keys from the stored password.
                let pw = t.str("password").unwrap();
                match crate::accounts::derive_credentials(pw, 10_000).await {
                    Ok(c) => (c, false, false),
                    Err(e) => {
                        r.warnings.push(format!("{user}: {e}"));
                        continue;
                    }
                }
            }
            None => {
                r.warnings.push(format!("{user}: no SCRAM keys in the account file"));
                continue;
            }
        };
        let (credentials, sha1_only, sha256_only) = creds;
        let created = store
            .create_account(NewAccount {
                domain: domain.to_owned(),
                localpart: user.clone(),
                role: Role::User,
                created_via: CreatedVia::Import,
                credentials,
            })
            .await?;
        if created.is_none() {
            r.accounts_skipped.push(bare(&user));
            continue;
        }
        r.accounts += 1;
        r.sha1_only += usize::from(sha1_only);
        r.sha256_only += usize::from(sha256_only);
        users.push(user);
    }

    for user in &users {
        let me = bare(user);
        // Roster, with pending inbound requests.
        let path = base.join("roster").join(format!("{}.dat", encode_name(user)));
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(Value::Table(t)) = lua::parse_dat(&text)
        {
            let mut items: BTreeMap<String, RosterItem> = BTreeMap::new();
            for (jid, v) in t.string_entries() {
                let Some(it) = v.as_table() else { continue };
                let mut item = RosterItem::new(jid);
                item.subscription = it
                    .str("subscription")
                    .and_then(Subscription::parse)
                    .unwrap_or(Subscription::None);
                item.name = it.str("name").filter(|n| !n.is_empty()).map(str::to_owned);
                item.ask_out = it.str("ask") == Some("subscribe");
                item.approved = it.get("approved").and_then(Value::as_bool).unwrap_or(false);
                if let Some(g) = it.table("groups") {
                    item.groups = g
                        .string_entries()
                        .map(|(k, _)| k.to_owned())
                        .chain(g.array.iter().filter_map(|v| v.as_str().map(str::to_owned)))
                        .collect();
                }
                items.insert(jid.to_owned(), item);
            }
            if let Some(pending) = t
                .map
                .get(&Key::Bool(false))
                .and_then(Value::as_table)
                .and_then(|m| m.table("pending"))
            {
                for (jid, _) in pending.string_entries() {
                    items
                        .entry(jid.to_owned())
                        .or_insert_with(|| {
                            let mut i = RosterItem::new(jid);
                            i.hidden = true;
                            i
                        })
                        .pending_in = true;
                }
            }
            for item in items.into_values() {
                store.roster_put(&me, item).await?;
                r.roster_items += 1;
            }
        }

        // PEP node configurations.
        let path = base.join("pep").join(format!("{}.dat", encode_name(user)));
        let mut node_limits: BTreeMap<String, u64> = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(Value::Table(t)) = lua::parse_dat(&text)
        {
            for (node, v) in t.string_entries() {
                let Some(n) = v.as_table() else { continue };
                let mut cfg = NodeConfig::default();
                let mut form: BTreeMap<String, Vec<String>> = BTreeMap::new();
                if let Some(c) = n.table("config") {
                    for (k, v) in c.string_entries() {
                        let val = match v {
                            Value::Str(s) => s.clone(),
                            Value::Bool(b) => b.to_string(),
                            Value::Num(x) if x.fract() == 0.0 => (*x as i64).to_string(),
                            _ => continue,
                        };
                        form.insert(format!("pubsub#{k}"), vec![val]);
                    }
                }
                if cfg.apply(&form).is_err() {
                    r.warnings
                        .push(format!("{me}: PEP node {node}: some settings were not understood"));
                }
                node_limits.insert(node.to_owned(), cfg.max_items.limit());
                store.pep_node_put(&me, node, serde_json::to_string(&cfg)?).await?;
                r.pep_nodes += 1;
            }
        }
        // PEP items live in one directory per node: `pep_<node>/<user>.list`.
        for entry in std::fs::read_dir(&base)?.filter_map(|e| e.ok()) {
            let dir_name = lua::decode_name(&entry.file_name().to_string_lossy());
            let Some(node) = dir_name.strip_prefix("pep_") else {
                continue;
            };
            let file = entry.path().join(format!("{}.list", encode_name(user)));
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let Some(&limit) = node_limits.get(node) else {
                r.warnings.push(format!("{me}: items for unknown PEP node {node}"));
                continue;
            };
            let mut list = lua::parse_list(&text).with_context(|| format!("parsing {}", file.display()))?;
            list.sort_by_key(when_ms);
            for it in &list {
                let Some(id) = it.str("key") else { continue };
                let payload = element(it, "").map(|e| String::from(&e)).unwrap_or_default();
                store
                    .pep_publish(
                        &me,
                        node,
                        PepItem {
                            id: id.to_owned(),
                            publisher: me.clone(),
                            payload,
                            published_ms: when_ms(it),
                        },
                        Some(limit),
                    )
                    .await?;
                r.pep_items += 1;
            }
        }

        // Message archive (MAM), keeping Prosody's ids.
        if opts.archives {
            let file = base.join("archive").join(format!("{}.list", encode_name(user)));
            if let Ok(text) = std::fs::read_to_string(&file) {
                let mut list = lua::parse_list(&text).with_context(|| format!("parsing {}", file.display()))?;
                list.sort_by_key(when_ms);
                for it in &list {
                    let (Some(id), Some(mut el)) = (it.str("key"), element(it, NS_CLIENT)) else {
                        continue;
                    };
                    let from = el.attr("from").unwrap_or_default().to_owned();
                    let out = from.split('/').next() == Some(me.as_str());
                    el.append_child(crate::mam::stanza_id(&me, id));
                    store
                        .archive_insert(ArchiveRow {
                            id: id.to_owned(),
                            archive_jid: me.clone(),
                            ts_ms: when_ms(it),
                            with_jid: it.str("with").unwrap_or_default().to_owned(),
                            direction: if out { Direction::Out } else { Direction::In },
                            origin_id: crate::mam::origin_id(&el),
                            from_jid: from,
                            stanza: String::from(&el),
                        })
                        .await?;
                    r.archived_messages += 1;
                }
            }
        }

        // Offline messages.
        let file = base.join("offline").join(format!("{}.list", encode_name(user)));
        if let Ok(text) = std::fs::read_to_string(&file) {
            for it in lua::parse_list(&text)? {
                let Some(mut el) = element(&it, NS_CLIENT) else {
                    continue;
                };
                if !el.has_child("delay", crate::user::message::NS_DELAY) {
                    el.append_child(crate::user::message::delay(domain, when_ms(&it)));
                }
                store.offline_push(&me, None, String::from(&el), 1000).await?;
                r.offline_messages += 1;
            }
        }

        // Block list (mod_blocklist).
        let file = base.join("blocklist").join(format!("{}.dat", encode_name(user)));
        if let Ok(text) = std::fs::read_to_string(&file)
            && let Ok(Value::Table(t)) = lua::parse_dat(&text)
        {
            let jids: Vec<String> = t.string_entries().map(|(k, _)| k.to_owned()).collect();
            r.blocklist_entries += jids.len();
            store.blocklist_update(&me, jids, vec![], false).await?;
        }

        // Private XML storage (XEP-0049), keyed "name:namespace".
        let file = base.join("private").join(format!("{}.dat", encode_name(user)));
        if let Ok(text) = std::fs::read_to_string(&file)
            && let Ok(Value::Table(t)) = lua::parse_dat(&text)
        {
            for (key, v) in t.string_entries() {
                let (Some((name, ns)), Some(tbl)) = (key.split_once(':'), v.as_table()) else {
                    continue;
                };
                if let Some(el) = element(tbl, ns) {
                    store
                        .private_put(&me, &format!("private:{{{ns}}}{name}"), Some(String::from(&el)))
                        .await?;
                    r.private_items += 1;
                }
            }
        }
    }

    // Rooms: any component directory with a `config` store, e.g. conference.<domain>.
    for entry in std::fs::read_dir(&opts.data_path)?.filter_map(|e| e.ok()) {
        let component = lua::decode_name(&entry.file_name().to_string_lossy());
        let config_dir = entry.path().join("config");
        if component == host || !component.ends_with(&format!(".{host}")) || !config_dir.is_dir() {
            continue;
        }
        let muc_host = component.replacen(&host, domain, 1);
        for (room, path) in files(&config_dir, "dat") {
            let Ok(Value::Table(t)) = lua::parse_dat(&read(&path)?) else {
                r.warnings.push(format!("room {room}: unreadable"));
                continue;
            };
            let jid = format!("{room}@{muc_host}");
            let data = t.table("_data").cloned().unwrap_or_default();
            let flag = |k: &str| data.get(k).and_then(Value::as_bool);
            let mut cfg = RoomConfig {
                persistent: flag("persistent").unwrap_or(true),
                public: !flag("hidden").unwrap_or(false),
                members_only: flag("members_only").unwrap_or(false),
                moderated: flag("moderated").unwrap_or(false),
                whois: if data.str("whois") == Some("anyone") {
                    Whois::Anyone
                } else {
                    Whois::Moderators
                },
                password: data.str("password").filter(|p| !p.is_empty()).map(str::to_owned),
                allow_invites: flag("allow_member_invites").unwrap_or(false),
                change_subject: flag("changesubject").unwrap_or(false),
                logging: flag("archiving").unwrap_or(true),
                ..RoomConfig::default()
            };
            if let Some(n) = data.str("name") {
                cfg.name = n.to_owned();
            }
            if let Some(d) = data.str("description") {
                cfg.description = d.to_owned();
            }
            if let Some(l) = data.str("language") {
                cfg.language = l.to_owned();
            }
            store
                .muc_room_put(RoomRow {
                    jid: jid.clone(),
                    config_json: serde_json::to_string(&cfg)?,
                    subject: data.str("subject").map(str::to_owned),
                    subject_from: data
                        .str("subject_from")
                        .and_then(|f| f.rsplit('/').next())
                        .map(str::to_owned),
                    created_ms: now_ms(),
                    last_active_ms: now_ms(),
                })
                .await?;
            r.rooms += 1;
            for (who, v) in t.string_entries().filter(|(k, _)| !k.starts_with('_')) {
                let Some(aff) = v
                    .as_str()
                    .and_then(Affiliation::parse)
                    .filter(|a| *a != Affiliation::None)
                else {
                    continue;
                };
                store
                    .muc_affiliation_set(
                        &jid,
                        RoomAffiliation {
                            jid: who.to_owned(),
                            affiliation: aff.as_str().to_owned(),
                            nick: None,
                            reason: None,
                        },
                        false,
                    )
                    .await?;
                r.room_affiliations += 1;
            }
            if !opts.archives {
                continue;
            }
            let log = entry
                .path()
                .join("muc_log")
                .join(format!("{}.list", encode_name(&room)));
            if let Ok(text) = std::fs::read_to_string(&log) {
                let mut list = lua::parse_list(&text).with_context(|| format!("parsing {}", log.display()))?;
                list.sort_by_key(when_ms);
                for it in &list {
                    let (Some(id), Some(mut el)) = (it.str("key"), element(it, NS_CLIENT)) else {
                        continue;
                    };
                    if el.name() != "message" {
                        continue;
                    }
                    stanza::remove_attr(&mut el, "to");
                    el.append_child(crate::mam::stanza_id(&jid, id));
                    let real = el
                        .get_child("x", crate::muc::NS_MUC_USER)
                        .and_then(|x| x.get_child("item", crate::muc::NS_MUC_USER))
                        .and_then(|i| i.attr("jid"))
                        .map(|j| j.split('/').next().unwrap_or(j).to_owned());
                    store
                        .archive_insert(ArchiveRow {
                            id: id.to_owned(),
                            archive_jid: jid.clone(),
                            ts_ms: when_ms(it),
                            with_jid: real.unwrap_or_else(|| jid.clone()),
                            direction: Direction::In,
                            origin_id: crate::mam::origin_id(&el),
                            from_jid: el.attr("from").unwrap_or_default().to_owned(),
                            stanza: String::from(&el),
                        })
                        .await?;
                    r.room_messages += 1;
                }
            }
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c2s::sasl::scram::Hash;

    #[test]
    fn names_and_stanzas() {
        assert_eq!(encode_name("conference.localhost"), "conference%2elocalhost");
        assert_eq!(lua::decode_name(&encode_name("agent-3e45jec5hu")), "agent-3e45jec5hu");
        let t = lua::parse_list(
            "item({ { \"hi\"; [\"name\"] = \"body\"; [\"attr\"] = {}; };\
             { [\"name\"] = \"origin-id\"; [\"attr\"] = { [\"xmlns\"] = \"urn:xmpp:sid:0\"; [\"id\"] = \"o1\"; }; };\
             [\"key\"] = \"k1\"; [\"when\"] = 1.5; [\"name\"] = \"message\";\
             [\"attr\"] = { [\"to\"] = \"a@h\"; [\"type\"] = \"chat\"; [\"xml:lang\"] = \"en\"; [\"stamp\"] = \"x\"; }; });",
        )
        .unwrap();
        let el = element(&t[0], NS_CLIENT).unwrap();
        assert_eq!(el.ns(), NS_CLIENT);
        assert_eq!(el.attr("type"), Some("chat"));
        assert!(el.attr("stamp").is_none());
        assert_eq!(el.get_child("body", NS_CLIENT).unwrap().text(), "hi");
        assert_eq!(crate::mam::origin_id(&el).as_deref(), Some("o1"));
        assert_eq!(when_ms(&t[0]), 1500);
    }

    /// `bob@localhost`'s account file from the dev Prosody 13 container
    /// (dev/prosody/setup.sh, password `bob-pass`), byte for byte: the hub
    /// derives the same SCRAM-SHA-1 keys, so imported passwords keep working.
    #[test]
    fn prosody_scram_sha1_keys_verify_as_they_are() {
        let file = "return {\n\t[\"stored_key\"] = \"183db897afa0c6f028b3291f7d3951914e3b03af\";\n\
            \t[\"updated\"] = 1794326327;\n\t[\"salt\"] = \"f764096f-f998-4c2b-b11b-b3a31101a006\";\n\
            \t[\"server_key\"] = \"8cbd9a7f4beaac137f12d1cc21aafd29bb84068d\";\n\t[\"created\"] = 1794326327;\n\
            \t[\"iteration_count\"] = 10000;\n};\n";
        let Value::Table(t) = lua::parse_dat(file).unwrap() else {
            panic!()
        };
        let (c, sha1_only, _) = credentials(&t, 4096).unwrap();
        assert!(sha1_only && c.sha256.is_none());
        assert_eq!(c.iterations, 10_000);
        assert_eq!(c.sha1, Hash::Sha1.keys("bob-pass", &c.salt, c.iterations));
        assert_ne!(c.sha1, Hash::Sha1.keys("bob-pass2", &c.salt, c.iterations));
    }
}
