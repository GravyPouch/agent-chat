//! Migration from Prosody: a synthetic `data_path` in Prosody 13's internal
//! storage format (bob's account file is the real one from the dev
//! Prosody, password `bob-pass`) is imported into a stopped hub, which then
//! serves bob with his old password, roster, PEP, archive and rooms.

mod common;

use std::path::Path;

use agent_chat_server::admin::{AdminRequest, UserInfo};
use agent_chat_server::import::{ImportOptions, import_prosody};
use agent_chat_server::{HubPaths, InitOptions, RunningHub};
use common::{TestHub, next_el, online, st};

fn write(base: &Path, rel: &str, text: &str) {
    let p = base.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// Prosody's layout for host `hubtest.local` (encoded `hubtest%2elocal`).
fn prosody_data(dir: &Path) {
    let h = "hubtest%2elocal";
    write(
        dir,
        &format!("{h}/accounts/bob.dat"),
        "return {\n\t[\"stored_key\"] = \"183db897afa0c6f028b3291f7d3951914e3b03af\";\n\t[\"updated\"] = 1794326327;\n\
         \t[\"salt\"] = \"f764096f-f998-4c2b-b11b-b3a31101a006\";\n\
         \t[\"server_key\"] = \"8cbd9a7f4beaac137f12d1cc21aafd29bb84068d\";\n\t[\"created\"] = 1794326327;\n\
         \t[\"iteration_count\"] = 10000;\n};\n",
    );
    write(
        dir,
        &format!("{h}/roster/bob.dat"),
        "return {\n\t[false] = {\n\t\t[\"pending\"] = { [\"carol@hubtest.local\"] = true; };\n\t\t[\"version\"] = 6;\n\t};\n\
         \t[\"alice@hubtest.local\"] = {\n\t\t[\"subscription\"] = \"both\";\n\t\t[\"groups\"] = { [\"Friends\"] = true; };\n\
         \t\t[\"name\"] = \"Alice\";\n\t};\n};\n",
    );
    write(
        dir,
        &format!("{h}/pep/bob.dat"),
        "return {\n\t[\"urn:xmpp:omemo:2:devices\"] = {\n\t\t[\"subscribers\"] = {};\n\t\t[\"affiliations\"] = {};\n\
         \t\t[\"name\"] = \"urn:xmpp:omemo:2:devices\";\n\t\t[\"config\"] = {\n\t\t\t[\"access_model\"] = \"open\";\n\t\t};\n\t};\n};\n",
    );
    write(
        dir,
        &format!("{h}/pep_urn%3axmpp%3aomemo%3a2%3adevices/bob.list"),
        "item({\n\t{\n\t\t[\"name\"] = \"device\";\n\t\t[\"attr\"] = { [\"id\"] = \"4242\"; };\n\t};\n\
         \t[\"key\"] = \"current\";\n\t[\"when\"] = 1794327332;\n\t[\"with\"] = \"bob@hubtest.local\";\n\
         \t[\"attr\"] = { [\"xmlns\"] = \"urn:xmpp:omemo:2\"; };\n\t[\"name\"] = \"devices\";\n});\n",
    );
    write(
        dir,
        &format!("{h}/archive/bob.list"),
        "item({\n\t{\n\t\t\"hello from alice\";\n\t\t[\"name\"] = \"body\";\n\t\t[\"attr\"] = {};\n\t};\n\
         \t[\"key\"] = \"01a1c62f-3f33-7075-8253-bff3785833dd\";\n\t[\"when\"] = 1794326347.57114005;\n\
         \t[\"with\"] = \"alice@hubtest.local\";\n\t[\"attr\"] = {\n\t\t[\"to\"] = \"bob@hubtest.local\";\n\
         \t\t[\"type\"] = \"chat\";\n\t\t[\"xml:lang\"] = \"en\";\n\t\t[\"from\"] = \"alice@hubtest.local/x\";\n\
         \t\t[\"stamp\"] = \"2026-11-10T15:59:07.571140Z\";\n\t};\n\t[\"name\"] = \"message\";\n});\n",
    );
    write(
        dir,
        "conference%2ehubtest%2elocal/config/team.dat",
        "return {\n\t[\"_jid\"] = \"team@conference.hubtest.local\";\n\t[\"_data\"] = {\n\t\t[\"whois\"] = \"anyone\";\n\
         \t\t[\"members_only\"] = true;\n\t\t[\"persistent\"] = true;\n\t\t[\"hidden\"] = true;\n\t};\n\
         \t[\"_affiliation_data\"] = {};\n\t[\"bob@hubtest.local\"] = \"owner\";\n\t[\"alice@hubtest.local\"] = \"member\";\n};\n",
    );
}

#[tokio::test]
async fn prosody_data_imports_and_old_passwords_work() {
    common::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let paths = HubPaths::at(dir.path().join("hub"));
    agent_chat_server::init(
        &paths,
        InitOptions {
            domain: Some(common::DOMAIN.into()),
            listen: vec!["127.0.0.1:0".into()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let data = dir.path().join("prosody");
    prosody_data(&data);
    let imported = import_prosody(
        &paths,
        ImportOptions {
            data_path: data,
            host: None,
            archives: true,
        },
    )
    .await
    .unwrap();
    let r = &imported.report;
    assert_eq!((r.accounts, r.sha1_only, r.roster_items), (1, 1, 2), "{r:?}");
    assert_eq!((r.pep_nodes, r.pep_items, r.archived_messages), (1, 1, 1));
    assert_eq!((r.rooms, r.room_affiliations), (1, 2));
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    assert!(imported.mechanisms_changed);
    assert!(!imported.mechanisms.iter().any(|m| m.starts_with("SCRAM-SHA-256")));

    let hub = RunningHub::start(paths.clone()).await.unwrap();
    let addr = hub.listeners()[0];
    let pin = hub.ctx.tls.current().pin.clone();
    let th = TestHub {
        hub: Some(hub),
        dir,
        paths,
        addr,
        pin,
    };
    // bob logs in with his Prosody password (SCRAM-SHA-1, keys as imported).
    let mut b = th.plaintext_client("bob@hubtest.local/new", "bob-pass");
    online(&mut b).await;
    let st_ = th.status().await;
    assert_eq!(TestHub::auth_count(&st_, "SCRAM-SHA-1", "success"), 1, "{:?}", st_.auth);

    // Roster: alice (both, named, grouped); carol's request is pending, not listed.
    b.send(st(
        "<iq xmlns='jabber:client' type='get' id='r'><query xmlns='jabber:iq:roster'/></iq>",
    ))
    .await;
    let roster = next_el(&mut b, |e| e.attr("id") == Some("r")).await;
    let q = roster.get_child("query", "jabber:iq:roster").unwrap();
    let items: Vec<_> = q
        .children()
        .map(|i| (i.attr("jid"), i.attr("subscription"), i.attr("name")))
        .collect();
    assert_eq!(items, [(Some("alice@hubtest.local"), Some("both"), Some("Alice"))]);
    b.send(st("<presence xmlns='jabber:client'/>")).await;
    let p = next_el(&mut b, |e| {
        e.name() == "presence" && e.attr("type") == Some("subscribe")
    })
    .await;
    assert_eq!(p.attr("from"), Some("carol@hubtest.local"));

    // PEP: the imported OMEMO:2 device list, readable by anyone (open).
    b.send(st(
        "<iq xmlns='jabber:client' type='get' id='p'><pubsub xmlns='http://jabber.org/protocol/pubsub'>\
        <items node='urn:xmpp:omemo:2:devices'/></pubsub></iq>",
    ))
    .await;
    let p = next_el(&mut b, |e| e.attr("id") == Some("p")).await;
    assert!(String::from(&p).contains("4242"), "{p:?}");

    // MAM: the archived message keeps Prosody's id (clients' last-synced id stays valid).
    b.send(st(
        "<iq xmlns='jabber:client' type='set' id='m'><query xmlns='urn:xmpp:mam:2' queryid='q'>\
        <set xmlns='http://jabber.org/protocol/rsm'><max>10</max><before/></set></query></iq>",
    ))
    .await;
    let res = next_el(&mut b, |e| {
        e.name() == "message" && e.has_child("result", "urn:xmpp:mam:2")
    })
    .await;
    let r = res.get_child("result", "urn:xmpp:mam:2").unwrap();
    assert_eq!(r.attr("id"), Some("01a1c62f-3f33-7075-8253-bff3785833dd"));
    assert!(String::from(r).contains("hello from alice"));

    // The room: members-only, bob is the owner, alice a member.
    b.send(st(
        "<iq xmlns='jabber:client' type='get' id='a' to='team@conference.hubtest.local'>\
        <query xmlns='http://jabber.org/protocol/muc#admin'><item affiliation='member'/></query></iq>",
    ))
    .await;
    let a = next_el(&mut b, |e| e.attr("id") == Some("a")).await;
    assert!(String::from(&a).contains("alice@hubtest.local"), "{a:?}");
    b.close().await;

    // A PLAIN login (over TLS) adds SCRAM-SHA-256 keys to the imported account.
    let users: Vec<UserInfo> = serde_json::from_value(th.admin(AdminRequest::UserList).await).unwrap();
    assert!(!users[0].scram_sha256);
    th.stop_and_keep().await;
}

impl TestHub {
    async fn stop_and_keep(mut self) {
        if let Some(h) = self.hub.take() {
            h.shutdown().await;
        }
        // Restart with PLAIN as the only usable mechanism over TLS (tokio-xmpp
        // falls back to PLAIN when -PLUS isn't offered, plan §3.1).
        let mut cfg = agent_chat_server::Config::load(self.paths.config()).unwrap();
        cfg.auth.mechanisms = vec!["SCRAM-SHA-1".into(), "PLAIN".into()];
        std::fs::write(self.paths.config(), cfg.to_toml().unwrap()).unwrap();
        let hub = RunningHub::start(self.paths.clone()).await.unwrap();
        self.addr = hub.listeners()[0];
        self.pin = hub.ctx.tls.current().pin.clone();
        self.hub = Some(hub);
        let mut b = self.tls_client("bob@hubtest.local/tls", "bob-pass");
        online(&mut b).await;
        let st_ = self.status().await;
        assert_eq!(TestHub::auth_count(&st_, "PLAIN", "success"), 1, "{:?}", st_.auth);
        b.close().await;
        let users: Vec<UserInfo> = serde_json::from_value(self.admin(AdminRequest::UserList).await).unwrap();
        assert!(users[0].scram_sha256, "upgraded after PLAIN");
        self.stop().await;
    }
}
