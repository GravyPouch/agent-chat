//! XEP-0313 MAM with XEP-0059 RSM: query parsing, result messages and the
//! `<fin/>`, shared by user archives (the account's actor) and room
//! archives (the room's actor). Archiving policy lives with the archive
//! owner (`user::message::archivable`, the room).

use tokio_xmpp::minidom::Element;
use xmpp_parsers::ns;

use crate::stanza::{self, Condition, NS_CLIENT};
use crate::store::{ArchivePage, ArchiveQuery, ArchiveRow};
use crate::user::message::{NS_FORWARD, delay};

pub const NS_MAM: &str = "urn:xmpp:mam:2";
pub const NS_MAM_EXTENDED: &str = "urn:xmpp:mam:2#extended";
pub const NS_RSM: &str = "http://jabber.org/protocol/rsm";
pub const NS_DATA: &str = "jabber:x:data";
pub const NS_SID: &str = ns::SID;

/// Page size when the client gives no `max`.
pub const DEFAULT_PAGE: usize = 50;

/// A parsed `<query xmlns='urn:xmpp:mam:2'>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub query: ArchiveQuery,
    pub queryid: Option<String>,
    pub flip: bool,
}

/// XEP-0082 date-time → milliseconds.
pub fn parse_datetime(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

fn field_values(form: &Element, var: &str) -> Vec<String> {
    form.children()
        .filter(|f| f.is("field", NS_DATA) && f.attr("var") == Some(var))
        .flat_map(|f| f.children().filter(|v| v.is("value", NS_DATA)).map(|v| v.text()))
        .filter(|v| !v.is_empty())
        .collect()
}

/// Parse a query (§4 of XEP-0313). `max_page` caps RSM `max`.
pub fn parse(query: &Element, archive_jid: &str, max_page: usize) -> Result<Request, Condition> {
    let mut q = ArchiveQuery {
        archive_jid: archive_jid.to_owned(),
        max: DEFAULT_PAGE.min(max_page),
        ..Default::default()
    };
    if let Some(form) = query.get_child("x", NS_DATA) {
        let one = |var: &str| field_values(form, var).into_iter().next();
        if let Some(t) = field_values(form, "FORM_TYPE").first()
            && t != NS_MAM
        {
            return Err(Condition::BadRequest);
        }
        if let Some(w) = one("with") {
            let j = xmpp_parsers::jid::Jid::new(&w).map_err(|_| Condition::JidMalformed)?;
            q.with = Some(j.to_bare().to_string());
        }
        if let Some(s) = one("start") {
            q.start_ms = Some(parse_datetime(&s).ok_or(Condition::BadRequest)?);
        }
        if let Some(e) = one("end") {
            q.end_ms = Some(parse_datetime(&e).ok_or(Condition::BadRequest)?);
        }
        q.after_id = one("after-id");
        q.before_id = one("before-id");
        q.ids = field_values(form, "ids");
    }
    if let Some(set) = query.get_child("set", NS_RSM) {
        if set.has_child("index", NS_RSM) {
            return Err(Condition::FeatureNotImplemented);
        }
        if let Some(m) = set.get_child("max", NS_RSM) {
            let max: usize = m.text().trim().parse().map_err(|_| Condition::BadRequest)?;
            q.max = max.min(max_page);
        }
        if let Some(a) = set.get_child("after", NS_RSM) {
            let t = a.text();
            if t.is_empty() {
                return Err(Condition::BadRequest);
            }
            q.rsm_after = Some(t);
        }
        if let Some(b) = set.get_child("before", NS_RSM) {
            q.rsm_before = Some(b.text());
        }
    }
    if !q.ids.is_empty() {
        // Explicit ids: as many as asked for, in archive order.
        q.max = q.ids.len().min(max_page);
    }
    Ok(Request {
        query: q,
        queryid: query.attr("queryid").map(str::to_owned),
        flip: query.has_child("flip-page", NS_MAM),
    })
}

/// `<message from=archive to=requester><result queryid id><forwarded>`.
pub fn result_message(to: &str, from: Option<&str>, queryid: Option<&str>, row: &ArchiveRow) -> Option<Element> {
    let original: Element = row.stanza.parse().ok()?;
    let mut msg = stanza::element("message", NS_CLIENT, &[("to", to)]);
    if let Some(f) = from {
        stanza::set_attr(&mut msg, "from", f);
    }
    let mut result = stanza::element("result", NS_MAM, &[("id", &row.id)]);
    if let Some(q) = queryid {
        stanza::set_attr(&mut result, "queryid", q);
    }
    let mut fwd = Element::bare("forwarded", NS_FORWARD);
    let mut d = delay("", row.ts_ms);
    stanza::remove_attr(&mut d, "from");
    fwd.append_child(d);
    fwd.append_child(original);
    result.append_child(fwd);
    msg.append_child(result);
    Some(msg)
}

/// The `<fin/>` for a page, with RSM first/last/count.
pub fn fin(page: &ArchivePage, rows_in_order: &[&ArchiveRow]) -> Element {
    let mut fin = Element::bare("fin", NS_MAM);
    if page.complete {
        stanza::set_attr(&mut fin, "complete", "true");
    }
    let mut set = Element::bare("set", NS_RSM);
    if let (Some(first), Some(last)) = (rows_in_order.first(), rows_in_order.last()) {
        let mut f = Element::bare("first", NS_RSM);
        f.append_text(first.id.as_str());
        set.append_child(f);
        let mut l = Element::bare("last", NS_RSM);
        l.append_text(last.id.as_str());
        set.append_child(l);
    }
    let mut c = Element::bare("count", NS_RSM);
    c.append_text(page.count.to_string());
    set.append_child(c);
    fin.append_child(set);
    fin
}

/// The query form (XEP-0313 §6.1), for `<iq type='get'><query/>`.
pub fn form() -> Element {
    let mut q = Element::bare("query", NS_MAM);
    let mut x = stanza::element("x", NS_DATA, &[("type", "form")]);
    let mut ft = stanza::element("field", NS_DATA, &[("var", "FORM_TYPE"), ("type", "hidden")]);
    let mut v = Element::bare("value", NS_DATA);
    v.append_text(NS_MAM);
    ft.append_child(v);
    x.append_child(ft);
    for (var, ty) in [
        ("with", "jid-single"),
        ("start", "text-single"),
        ("end", "text-single"),
        ("before-id", "text-single"),
        ("after-id", "text-single"),
        ("ids", "list-multi"),
    ] {
        x.append_child(stanza::element("field", NS_DATA, &[("var", var), ("type", ty)]));
    }
    q.append_child(x);
    q
}

/// XEP-0313 §7 `<prefs/>`: the hub archives everything, so the answer is
/// always `default='always'` (a set is accepted and answered the same way).
pub fn prefs() -> Element {
    let mut p = stanza::element("prefs", NS_MAM, &[("default", "always")]);
    p.append_child(Element::bare("always", NS_MAM));
    p.append_child(Element::bare("never", NS_MAM));
    p
}

/// XEP-0313 §6.3 archive metadata from the first and last rows.
pub fn metadata(first: Option<&ArchiveRow>, last: Option<&ArchiveRow>) -> Element {
    let mut m = Element::bare("metadata", NS_MAM);
    for (name, row) in [("start", first), ("end", last)] {
        if let Some(r) = row {
            let ts = chrono::DateTime::from_timestamp_millis(r.ts_ms)
                .unwrap_or_default()
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string();
            m.append_child(stanza::element(name, NS_MAM, &[("id", &r.id), ("timestamp", &ts)]));
        }
    }
    m
}

/// The `<stanza-id/>` (XEP-0359) an archive stamps.
pub fn stanza_id(by: &str, id: &str) -> Element {
    stanza::element("stanza-id", NS_SID, &[("by", by), ("id", id)])
}

/// A fresh archive id: UUIDv7, time-ordered and unguessable (§5.7).
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The `origin-id` of a message, if any.
pub fn origin_id(el: &Element) -> Option<String> {
    el.get_child("origin-id", NS_SID)
        .and_then(|o| o.attr("id"))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(xml: &str) -> Result<Request, Condition> {
        parse(&xml.parse().unwrap(), "a@h", 250)
    }

    #[test]
    fn parses_forms_and_rsm() {
        let r = q("<query xmlns='urn:xmpp:mam:2' queryid='f1'>\
            <x xmlns='jabber:x:data' type='submit'>\
              <field var='FORM_TYPE' type='hidden'><value>urn:xmpp:mam:2</value></field>\
              <field var='with'><value>B@H/res</value></field>\
              <field var='start'><value>2026-11-10T10:00:00Z</value></field>\
            </x>\
            <set xmlns='http://jabber.org/protocol/rsm'><max>1000</max><before/></set>\
            <flip-page/></query>")
        .unwrap();
        assert_eq!(r.queryid.as_deref(), Some("f1"));
        assert_eq!(r.query.with.as_deref(), Some("b@h"));
        assert_eq!(r.query.max, 250, "capped");
        assert_eq!(r.query.rsm_before.as_deref(), Some(""));
        assert!(r.flip);
        assert!(r.query.start_ms.is_some());
        let plain = q("<query xmlns='urn:xmpp:mam:2'/>").unwrap();
        assert_eq!(plain.query.max, DEFAULT_PAGE);
        assert!(plain.query.rsm_before.is_none() && plain.query.rsm_after.is_none());
        assert_eq!(
            q(
                "<query xmlns='urn:xmpp:mam:2'><set xmlns='http://jabber.org/protocol/rsm'><index>3</index></set></query>"
            ),
            Err(Condition::FeatureNotImplemented)
        );
        assert_eq!(
            q("<query xmlns='urn:xmpp:mam:2'><set xmlns='http://jabber.org/protocol/rsm'><max>x</max></set></query>"),
            Err(Condition::BadRequest)
        );
        let max0 =
            q("<query xmlns='urn:xmpp:mam:2'><set xmlns='http://jabber.org/protocol/rsm'><max>0</max></set></query>")
                .unwrap();
        assert_eq!(max0.query.max, 0);
    }

    #[test]
    fn results_and_fin() {
        let row = ArchiveRow {
            id: "id1".into(),
            archive_jid: "a@h".into(),
            ts_ms: 0,
            with_jid: "b@h".into(),
            direction: crate::store::Direction::In,
            origin_id: None,
            from_jid: "b@h/r".into(),
            stanza: "<message xmlns='jabber:client' from='b@h/r'><body>x</body></message>".into(),
        };
        let m = result_message("a@h/r", Some("a@h"), Some("q"), &row).unwrap();
        let parsed = xmpp_parsers::message::Message::try_from(m).unwrap();
        assert!(parsed.payloads.iter().any(|p| p.is("result", NS_MAM)));
        let page = ArchivePage {
            rows: vec![row.clone()],
            complete: true,
            count: 1,
            id_not_found: false,
        };
        let f = fin(&page, &[&row]);
        let parsed = xmpp_parsers::mam::Fin::try_from(f).unwrap();
        assert!(parsed.complete);
        assert_eq!(parsed.set.last.as_deref(), Some("id1"));
        assert_eq!(parsed.set.count, Some(1));
    }
}
