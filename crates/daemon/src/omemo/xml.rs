//! OMEMO XML: device lists, bundles, `<encrypted/>` and the SCE envelope,
//! for both versions.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use omemo::{OwnBundle, Version};
use tokio_xmpp::minidom::Element;

pub const NS_PUBSUB: &str = "http://jabber.org/protocol/pubsub";
pub const NS_PUBSUB_EVENT: &str = "http://jabber.org/protocol/pubsub#event";
pub const NS_SCE: &str = "urn:xmpp:sce:1";
pub const NS_CLIENT: &str = "jabber:client";

pub fn ns(v: Version) -> &'static str {
    match v {
        Version::Omemo2 => super::NS_OMEMO2,
        Version::Legacy => super::NS_LEGACY,
    }
}

pub fn devices_node(v: Version) -> &'static str {
    match v {
        Version::Omemo2 => "urn:xmpp:omemo:2:devices",
        Version::Legacy => "eu.siacs.conversations.axolotl.devicelist",
    }
}

pub fn bundles_node(v: Version, device_id: u32) -> String {
    match v {
        Version::Omemo2 => "urn:xmpp:omemo:2:bundles".to_owned(),
        Version::Legacy => format!("eu.siacs.conversations.axolotl.bundles:{device_id}"),
    }
}

pub fn bundle_item_id(v: Version, device_id: u32) -> String {
    match v {
        Version::Omemo2 => device_id.to_string(),
        Version::Legacy => "current".to_owned(),
    }
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

fn parse(xml: &str) -> Element {
    xml.parse().expect("internally generated XML is well-formed")
}

fn b64(data: &[u8]) -> String {
    B64.encode(data)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    B64.decode(cleaned).ok()
}

fn id_attr(el: &Element, name: &str) -> Option<u32> {
    el.attr(name)?.trim().parse().ok().filter(|v: &u32| *v > 0)
}

// --------------------------------------------------------------- device list

pub fn device_list(v: Version, ids: &[u32]) -> Element {
    let devices: String = ids.iter().map(|id| format!("<device id='{id}'/>")).collect();
    match v {
        Version::Omemo2 => parse(&format!("<devices xmlns='{}'>{devices}</devices>", super::NS_OMEMO2)),
        Version::Legacy => parse(&format!("<list xmlns='{}'>{devices}</list>", super::NS_LEGACY)),
    }
}

/// Device ids from a device list payload. Labels are ignored: XEP-0384
/// requires a verified `labelsig`, which we don't check yet.
pub fn parse_device_list(v: Version, el: &Element) -> Option<Vec<(u32, Option<String>)>> {
    let root = match v {
        Version::Omemo2 if el.is("devices", super::NS_OMEMO2) => el,
        Version::Legacy if el.is("list", super::NS_LEGACY) => el,
        _ => return None,
    };
    Some(
        root.children()
            .filter(|c| c.name() == "device")
            .filter_map(|c| id_attr(c, "id"))
            .map(|id| (id, None))
            .collect(),
    )
}

// -------------------------------------------------------------------- bundle

pub fn bundle(v: Version, b: &OwnBundle) -> Element {
    match v {
        Version::Omemo2 => {
            let pks: String = b
                .pre_keys
                .iter()
                .map(|(id, k)| format!("<pk id='{id}'>{}</pk>", b64(k)))
                .collect();
            parse(&format!(
                "<bundle xmlns='{}'><spk id='{}'>{}</spk><spks>{}</spks><ik>{}</ik><prekeys>{pks}</prekeys></bundle>",
                super::NS_OMEMO2,
                b.signed_pre_key_id,
                b64(&b.signed_pre_key),
                b64(&b.signed_pre_key_signature),
                b64(&b.identity_key),
            ))
        }
        Version::Legacy => {
            let pks: String = b
                .pre_keys
                .iter()
                .map(|(id, k)| format!("<preKeyPublic preKeyId='{id}'>{}</preKeyPublic>", b64(k)))
                .collect();
            parse(&format!(
                "<bundle xmlns='{}'><signedPreKeyPublic signedPreKeyId='{}'>{}</signedPreKeyPublic>\
                 <signedPreKeySignature>{}</signedPreKeySignature><identityKey>{}</identityKey>\
                 <prekeys>{pks}</prekeys></bundle>",
                super::NS_LEGACY,
                b.signed_pre_key_id,
                b64(&b.signed_pre_key),
                b64(&b.signed_pre_key_signature),
                b64(&b.identity_key),
            ))
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedBundle {
    pub identity_key: Vec<u8>,
    pub signed_pre_key_id: u32,
    pub signed_pre_key: Vec<u8>,
    pub signature: Vec<u8>,
    pub pre_keys: Vec<(u32, Vec<u8>)>,
}

pub fn parse_bundle(v: Version, el: &Element) -> Option<ParsedBundle> {
    let ns = ns(v);
    if !el.is("bundle", ns) {
        return None;
    }
    let (spk_name, spk_id, sig_name, ik_name, pk_name, pk_id) = match v {
        Version::Omemo2 => ("spk", "id", "spks", "ik", "pk", "id"),
        Version::Legacy => (
            "signedPreKeyPublic",
            "signedPreKeyId",
            "signedPreKeySignature",
            "identityKey",
            "preKeyPublic",
            "preKeyId",
        ),
    };
    let spk = el.get_child(spk_name, ns)?;
    let pre_keys = el
        .get_child("prekeys", ns)?
        .children()
        .filter(|c| c.name() == pk_name)
        .filter_map(|c| Some((id_attr(c, pk_id)?, unb64(&c.text())?)))
        .collect::<Vec<_>>();
    Some(ParsedBundle {
        identity_key: unb64(&el.get_child(ik_name, ns)?.text())?,
        signed_pre_key_id: id_attr(spk, spk_id)?,
        signed_pre_key: unb64(&spk.text())?,
        signature: unb64(&el.get_child(sig_name, ns)?.text())?,
        pre_keys,
    })
}

// -------------------------------------------------------------------- pubsub

/// `<pubsub><publish node><item id>payload</item></publish><publish-options/></pubsub>`
pub fn publish(node: &str, item_id: &str, payload: Element, options: &[(&str, &str)]) -> Element {
    let fields: String = options
        .iter()
        .map(|(var, val)| format!("<field var='{var}'><value>{val}</value></field>"))
        .collect();
    let mut root = parse(&format!(
        "<pubsub xmlns='{NS_PUBSUB}'><publish node='{}'><item id='{}'/></publish>\
         <publish-options><x xmlns='jabber:x:data' type='submit'>\
         <field var='FORM_TYPE' type='hidden'><value>http://jabber.org/protocol/pubsub#publish-options</value></field>\
         {fields}</x></publish-options></pubsub>",
        escape(node),
        escape(item_id),
    ));
    if let Some(item) = root
        .get_child_mut("publish", NS_PUBSUB)
        .and_then(|p| p.get_child_mut("item", NS_PUBSUB))
    {
        item.append_child(payload);
    }
    root
}

pub fn items_query(node: &str, item_id: Option<&str>) -> Element {
    let item = item_id
        .map(|id| format!("<item id='{}'/>", escape(id)))
        .unwrap_or_default();
    parse(&format!(
        "<pubsub xmlns='{NS_PUBSUB}'><items node='{}'>{item}</items></pubsub>",
        escape(node)
    ))
}

/// Payloads of the `<item>`s in a pubsub items result.
pub fn result_payloads(pubsub: &Element) -> Vec<Element> {
    pubsub
        .get_child("items", NS_PUBSUB)
        .map(|items| {
            items
                .children()
                .filter(|c| c.name() == "item")
                .filter_map(|item| item.children().next().cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// (node, payloads) from a PEP `<event/>` notification.
pub fn event_payloads(event: &Element) -> Option<(String, Vec<Element>)> {
    let items = event.get_child("items", NS_PUBSUB_EVENT)?;
    let node = items.attr("node")?.to_owned();
    let payloads = items
        .children()
        .filter(|c| c.name() == "item")
        .filter_map(|item| item.children().next().cloned())
        .collect();
    Some((node, payloads))
}

// ----------------------------------------------------------------- encrypted

#[derive(Debug, Clone, PartialEq)]
pub struct KeyEntry {
    /// Recipient bare JID (OMEMO:2 groups keys by JID; empty for legacy).
    pub jid: String,
    pub rid: u32,
    pub kex: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Encrypted {
    pub sid: u32,
    pub keys: Vec<KeyEntry>,
    /// Legacy only.
    pub iv: Option<Vec<u8>>,
    pub payload: Option<Vec<u8>>,
}

pub fn build_encrypted(v: Version, e: &Encrypted) -> Element {
    let payload = e
        .payload
        .as_ref()
        .map(|p| format!("<payload>{}</payload>", b64(p)))
        .unwrap_or_default();
    match v {
        Version::Omemo2 => {
            let mut jids: Vec<&str> = e.keys.iter().map(|k| k.jid.as_str()).collect();
            jids.dedup();
            jids.sort();
            jids.dedup();
            let groups: String = jids
                .iter()
                .map(|jid| {
                    let keys: String = e
                        .keys
                        .iter()
                        .filter(|k| k.jid == *jid)
                        .map(|k| {
                            format!(
                                "<key rid='{}'{}>{}</key>",
                                k.rid,
                                if k.kex { " kex='true'" } else { "" },
                                b64(&k.data)
                            )
                        })
                        .collect();
                    format!("<keys jid='{}'>{keys}</keys>", escape(jid))
                })
                .collect();
            parse(&format!(
                "<encrypted xmlns='{}'><header sid='{}'>{groups}</header>{payload}</encrypted>",
                super::NS_OMEMO2,
                e.sid
            ))
        }
        Version::Legacy => {
            let keys: String = e
                .keys
                .iter()
                .map(|k| {
                    format!(
                        "<key rid='{}'{}>{}</key>",
                        k.rid,
                        if k.kex { " prekey='true'" } else { "" },
                        b64(&k.data)
                    )
                })
                .collect();
            let iv =
                e.iv.as_ref()
                    .map(|iv| format!("<iv>{}</iv>", b64(iv)))
                    .unwrap_or_default();
            parse(&format!(
                "<encrypted xmlns='{}'><header sid='{}'>{keys}{iv}</header>{payload}</encrypted>",
                super::NS_LEGACY,
                e.sid
            ))
        }
    }
}

fn truthy(v: Option<&str>) -> bool {
    matches!(v, Some("true") | Some("1"))
}

pub fn parse_encrypted(v: Version, el: &Element) -> Option<Encrypted> {
    let ns = ns(v);
    if !el.is("encrypted", ns) {
        return None;
    }
    let header = el.get_child("header", ns)?;
    let sid = id_attr(header, "sid")?;
    let mut keys = Vec::new();
    match v {
        Version::Omemo2 => {
            for group in header.children().filter(|c| c.name() == "keys") {
                let jid = group.attr("jid").unwrap_or_default().to_owned();
                for k in group.children().filter(|c| c.name() == "key") {
                    if let (Some(rid), Some(data)) = (id_attr(k, "rid"), unb64(&k.text())) {
                        keys.push(KeyEntry {
                            jid: jid.clone(),
                            rid,
                            kex: truthy(k.attr("kex")),
                            data,
                        });
                    }
                }
            }
        }
        Version::Legacy => {
            for k in header.children().filter(|c| c.name() == "key") {
                if let (Some(rid), Some(data)) = (id_attr(k, "rid"), unb64(&k.text())) {
                    keys.push(KeyEntry {
                        jid: String::new(),
                        rid,
                        kex: truthy(k.attr("prekey")),
                        data,
                    });
                }
            }
        }
    }
    Some(Encrypted {
        sid,
        keys,
        iv: header.get_child("iv", ns).and_then(|iv| unb64(&iv.text())),
        payload: el.get_child("payload", ns).and_then(|p| unb64(&p.text())),
    })
}

/// XEP-0380 hint so non-OMEMO clients can explain the message.
pub fn eme(v: Version) -> Element {
    parse(&format!(
        "<encryption xmlns='urn:xmpp:eme:0' namespace='{}' name='OMEMO'/>",
        ns(v)
    ))
}

// ---------------------------------------------------------------------- SCE

fn rpad() -> String {
    // 0..=199 random printable characters, from two UUIDs' worth of entropy.
    let a = uuid::Uuid::new_v4();
    let len = (a.as_bytes()[0] as usize * 200) / 256;
    let mut s = String::new();
    while s.len() < len {
        s.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    s.truncate(len);
    s
}

/// XEP-0420 envelope per XEP-0384 §5.5.1 (`rpad` required, `from`
/// recommended, `to` required in group chats).
pub fn envelope(body: &str, from: &str, to: Option<&str>) -> Vec<u8> {
    let to = to.map(|t| format!("<to jid='{}'/>", escape(t))).unwrap_or_default();
    format!(
        "<envelope xmlns='{NS_SCE}'><content><body xmlns='{NS_CLIENT}'>{}</body></content>\
         <rpad>{}</rpad><from jid='{}'/>{to}</envelope>",
        escape(body),
        rpad(),
        escape(from),
    )
    .into_bytes()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub body: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

pub fn parse_envelope(data: &[u8]) -> Option<Envelope> {
    let el: Element = std::str::from_utf8(data).ok()?.parse().ok()?;
    if !el.is("envelope", NS_SCE) {
        return None;
    }
    let content = el.get_child("content", NS_SCE)?;
    Some(Envelope {
        body: content.get_child("body", NS_CLIENT).map(|b| b.text()),
        from: el
            .get_child("from", NS_SCE)
            .and_then(|f| f.attr("jid"))
            .map(str::to_owned),
        to: el
            .get_child("to", NS_SCE)
            .and_then(|f| f.attr("jid"))
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_round_trip_both_versions() {
        for v in [Version::Omemo2, Version::Legacy] {
            let jid = if v == Version::Omemo2 { "bob@x" } else { "" };
            let e = Encrypted {
                sid: 27183,
                keys: vec![
                    KeyEntry {
                        jid: jid.into(),
                        rid: 31415,
                        kex: true,
                        data: vec![1, 2, 3],
                    },
                    KeyEntry {
                        jid: jid.into(),
                        rid: 1337,
                        kex: false,
                        data: vec![4],
                    },
                ],
                iv: (v == Version::Legacy).then(|| vec![9; 12]),
                payload: Some(vec![7; 20]),
            };
            let el = build_encrypted(v, &e);
            assert_eq!(parse_encrypted(v, &el).unwrap(), e, "{v:?}");
            // The other version's parser must not accept it.
            let other = if v == Version::Omemo2 {
                Version::Legacy
            } else {
                Version::Omemo2
            };
            assert!(parse_encrypted(other, &el).is_none());
        }
    }

    #[test]
    fn parses_spec_example_shape() {
        let xml = "<encrypted xmlns='urn:xmpp:omemo:2'><header sid='27183'>\
                   <keys jid='juliet@capulet.lit'><key rid='31415'>AQID</key></keys>\
                   <keys jid='romeo@montague.lit'><key rid='1337'>BA==</key><key kex='true' rid='12321'>BQ==</key></keys>\
                   </header><payload>Bwc=</payload></encrypted>";
        let e = parse_encrypted(Version::Omemo2, &xml.parse().unwrap()).unwrap();
        assert_eq!(e.sid, 27183);
        assert_eq!(e.keys.len(), 3);
        assert!(e.keys[2].kex && e.keys[2].jid == "romeo@montague.lit");
        assert_eq!(e.payload, Some(vec![7, 7]));
    }

    #[test]
    fn envelope_round_trip_escapes_markup() {
        let data = envelope("hi <b>&</b> 'x'", "alice@x", None);
        let env = parse_envelope(&data).unwrap();
        assert_eq!(env.body.as_deref(), Some("hi <b>&</b> 'x'"));
        assert_eq!(env.from.as_deref(), Some("alice@x"));
        assert_eq!(env.to, None);
    }

    #[test]
    fn bundle_and_device_list_round_trip() {
        for v in [Version::Omemo2, Version::Legacy] {
            let own = OwnBundle {
                identity_key: vec![1; 32],
                signed_pre_key_id: 3,
                signed_pre_key: vec![2; 32],
                signed_pre_key_signature: vec![3; 64],
                pre_keys: vec![(1, vec![4; 32]), (2, vec![5; 32])],
            };
            let p = parse_bundle(v, &bundle(v, &own)).unwrap();
            assert_eq!(p.identity_key, own.identity_key);
            assert_eq!(p.signed_pre_key_id, 3);
            assert_eq!(p.pre_keys, own.pre_keys);
            let list = parse_device_list(v, &device_list(v, &[5, 6])).unwrap();
            assert_eq!(list.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [5, 6]);
        }
    }

    #[test]
    fn publish_puts_payload_in_item() {
        let el = publish(
            "node",
            "current",
            device_list(Version::Omemo2, &[1]),
            &[("pubsub#access_model", "open")],
        );
        let item = el
            .get_child("publish", NS_PUBSUB)
            .unwrap()
            .get_child("item", NS_PUBSUB)
            .unwrap();
        assert!(item.get_child("devices", super::super::NS_OMEMO2).is_some());
    }
}
