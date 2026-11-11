//! XEP-0363 HTTP File Upload on `upload.<domain>`.
//!
//! A local account asks the service for a slot (file name, size, type); the
//! answer has a PUT and a GET URL on the hub's web listener. The PUT must
//! come within a few minutes, with exactly the announced size; the file is
//! then served until it expires. Clients that encrypt (XEP-0454) upload
//! ciphertext, so the hub never sees the content or the key.
//!
//! Files live in `<hub dir>/uploads/<token>/` with a `meta.json`; the token
//! (128 random bits) is the only thing that grants access.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio_xmpp::minidom::Element;
use xmpp_parsers::jid::Jid;
use xmpp_parsers::ns;

use crate::router::{Component, Router};
use crate::stanza::{self, Condition, Kind};

pub const NS_UPLOAD: &str = "urn:xmpp:http:upload:0";
/// How long a slot waits for its PUT.
const SLOT_TTL: Duration = Duration::from_secs(300);
/// Unused slots one account may hold at once.
const MAX_PENDING_PER_ACCOUNT: usize = 20;
const META: &str = "meta.json";

#[derive(Debug, Clone)]
struct Slot {
    filename: String,
    size: u64,
    content_type: String,
    owner: String,
    created: Instant,
}

/// What `meta.json` records about an uploaded file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMeta {
    pub filename: String,
    pub size: u64,
    pub content_type: String,
    pub owner: String,
    /// Milliseconds since the Unix epoch.
    pub uploaded: i64,
}

pub struct UploadService {
    host: String,
    domain: String,
    router: Weak<Router>,
    dir: PathBuf,
    /// `https://example.org` (no trailing slash); URLs are `<base>/upload/<token>/<name>`.
    base_url: String,
    max_size: u64,
    pending: Mutex<HashMap<String, Slot>>,
}

/// A file name safe to put in a URL path and on disk: the base name,
/// percent-encoded (clients decode it to show the original name).
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim_start_matches('.');
    let mut out = String::new();
    for b in base.bytes() {
        if out.len() >= 180 {
            break;
        }
        if b.is_ascii_alphanumeric() || b"._-~".contains(&b) {
            out.push(b as char);
        } else if b >= 0x20 && b != 0x7f {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.is_empty() { "file".into() } else { out }
}

fn valid_token(t: &str) -> bool {
    t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit())
}

impl UploadService {
    pub fn new(
        host: &str,
        domain: &str,
        router: &Arc<Router>,
        dir: PathBuf,
        base_url: &str,
        max_size: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            host: host.to_owned(),
            domain: domain.to_owned(),
            router: Arc::downgrade(router),
            dir,
            base_url: base_url.trim_end_matches('/').to_owned(),
            max_size,
            pending: Mutex::default(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn reply(&self, el: Element) {
        if let Some(r) = self.router.upgrade() {
            r.deliver_reply(el);
        }
    }

    fn error(&self, request: &Element, cond: Condition, text: Option<&str>, app: Option<Element>) {
        if let Some(e) = stanza::error_reply_with(request, cond, Some(&self.host), text, app) {
            self.reply(e);
        }
    }

    fn disco_info(&self) -> Element {
        let mut q = Element::bare("query", ns::DISCO_INFO);
        q.append_child(stanza::element(
            "identity",
            ns::DISCO_INFO,
            &[("category", "store"), ("type", "file"), ("name", "HTTP File Upload")],
        ));
        for f in [NS_UPLOAD, ns::DISCO_INFO, ns::PING] {
            q.append_child(stanza::element("feature", ns::DISCO_INFO, &[("var", f)]));
        }
        // XEP-0363 §2: the size limit as an extended disco form.
        let mut x = stanza::element("x", "jabber:x:data", &[("type", "result")]);
        for (var, value) in [
            ("FORM_TYPE", NS_UPLOAD.to_owned()),
            ("max-file-size", self.max_size.to_string()),
        ] {
            let mut field = stanza::element("field", "jabber:x:data", &[("var", var)]);
            if var == "FORM_TYPE" {
                stanza::set_attr(&mut field, "type", "hidden");
            }
            let mut v = Element::bare("value", "jabber:x:data");
            v.append_text(&value);
            field.append_child(v);
            x.append_child(field);
        }
        q.append_child(x);
        q
    }

    /// A slot request: answer with the PUT and GET URLs.
    fn request_slot(&self, iq: &Element, req: &Element, owner: &str) {
        let filename = req.attr("filename").unwrap_or("").trim();
        let size: Option<u64> = req.attr("size").and_then(|s| s.parse().ok());
        let (Some(size), false) = (size, filename.is_empty()) else {
            self.error(iq, Condition::BadRequest, Some("filename and size are required"), None);
            return;
        };
        if size == 0 || size > self.max_size {
            let mut app = Element::bare("file-too-large", NS_UPLOAD);
            let mut max = Element::bare("max-file-size", NS_UPLOAD);
            max.append_text(self.max_size.to_string());
            app.append_child(max);
            let text = format!("files can be up to {} bytes", self.max_size);
            self.error(iq, Condition::NotAcceptable, Some(&text), Some(app));
            return;
        }
        let content_type = req
            .attr("content-type")
            .filter(|t| t.len() < 128 && t.contains('/') && !t.contains(['\r', '\n']))
            .unwrap_or("application/octet-stream")
            .to_owned();
        let token = crate::util::random_id();
        let name = sanitize_filename(filename);
        {
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|_, s| s.created.elapsed() < SLOT_TTL);
            if pending.values().filter(|s| s.owner == owner).count() >= MAX_PENDING_PER_ACCOUNT {
                drop(pending);
                self.error(
                    iq,
                    Condition::ResourceConstraint,
                    Some("too many unused upload slots"),
                    None,
                );
                return;
            }
            pending.insert(
                token.clone(),
                Slot {
                    filename: name.clone(),
                    size,
                    content_type,
                    owner: owner.to_owned(),
                    created: Instant::now(),
                },
            );
        }
        let url = format!("{}/upload/{token}/{name}", self.base_url);
        let mut slot = Element::bare("slot", NS_UPLOAD);
        slot.append_child(stanza::element("put", NS_UPLOAD, &[("url", &url)]));
        slot.append_child(stanza::element("get", NS_UPLOAD, &[("url", &url)]));
        self.reply(stanza::iq_result(iq, Some(&self.host), Some(slot)));
    }

    /// The slot for an upload to `/upload/<token>/<name>`, if it's pending
    /// (it is taken: a slot is good for one PUT).
    pub fn take_slot(&self, token: &str, name: &str) -> Option<(u64, String, String)> {
        let mut pending = self.pending.lock().unwrap();
        let ok = pending
            .get(token)
            .is_some_and(|s| s.filename == name && s.created.elapsed() < SLOT_TTL);
        if !ok {
            return None;
        }
        pending.remove(token).map(|s| (s.size, s.content_type, s.owner))
    }

    /// Where the file for `token` lives, if the token is well-formed.
    pub fn file_dir(&self, token: &str) -> Option<PathBuf> {
        valid_token(token).then(|| self.dir.join(token))
    }

    pub fn read_meta(&self, token: &str) -> Option<FileMeta> {
        let dir = self.file_dir(token)?;
        serde_json::from_slice(&std::fs::read(dir.join(META)).ok()?).ok()
    }

    pub fn write_meta(&self, token: &str, meta: &FileMeta) -> std::io::Result<()> {
        let dir = self.file_dir(token).ok_or(std::io::ErrorKind::InvalidInput)?;
        std::fs::write(dir.join(META), serde_json::to_vec(meta)?)
    }

    /// Delete files uploaded longer than `keep` ago; returns how many.
    pub fn expire(&self, keep: Duration) -> usize {
        let cutoff = crate::util::now_ms() - keep.as_millis() as i64;
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut n = 0;
        for e in entries.flatten() {
            let token = e.file_name().to_string_lossy().into_owned();
            let stale = match self.read_meta(&token) {
                Some(m) => m.uploaded < cutoff,
                // An upload that never finished.
                None => e
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().unwrap_or_default() > SLOT_TTL * 2),
            };
            if stale && valid_token(&token) && std::fs::remove_dir_all(e.path()).is_ok() {
                n += 1;
            }
        }
        n
    }
}

impl Component for UploadService {
    fn deliver(self: Arc<Self>, el: Element) {
        let Some(to) = el.attr("to").and_then(|t| Jid::new(t).ok()) else {
            return;
        };
        if Kind::of(&el) != Some(Kind::Iq) || !matches!(stanza::stanza_type(&el), "get" | "set") {
            return;
        }
        let from = el.attr("from").and_then(|f| Jid::new(f).ok());
        let owner = match &from {
            Some(f) if f.domain().as_str() == self.domain && f.node().is_some() => f.to_bare().to_string(),
            _ => {
                self.error(
                    &el,
                    Condition::NotAllowed,
                    Some("only accounts on this hub can upload"),
                    None,
                );
                return;
            }
        };
        if to.node().is_some() || to.domain().as_str() != self.host {
            self.error(&el, Condition::ServiceUnavailable, None, None);
            return;
        }
        let payload = el.children().next().cloned();
        match (stanza::stanza_type(&el), payload) {
            ("get", Some(p)) if p.is("ping", ns::PING) => self.reply(stanza::iq_result(&el, Some(&self.host), None)),
            ("get", Some(p)) if p.is("query", ns::DISCO_INFO) && p.attr("node").is_none() => {
                self.reply(stanza::iq_result(&el, Some(&self.host), Some(self.disco_info())))
            }
            ("get", Some(p)) if p.is("query", ns::DISCO_ITEMS) => {
                let q = Element::bare("query", ns::DISCO_ITEMS);
                self.reply(stanza::iq_result(&el, Some(&self.host), Some(q)))
            }
            ("get", Some(p)) if p.is("request", NS_UPLOAD) => self.request_slot(&el, &p, &owner),
            _ => self.error(&el, Condition::ServiceUnavailable, None, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_are_made_safe() {
        assert_eq!(sanitize_filename("photo 1.jpg"), "photo%201.jpg");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("..."), "file");
        assert_eq!(sanitize_filename("C:\\x\\ü.png"), "%C3%BC.png");
        assert_eq!(sanitize_filename("a\nb"), "ab");
        assert!(valid_token(&crate::util::random_id()));
        assert!(!valid_token("../x"));
    }
}
