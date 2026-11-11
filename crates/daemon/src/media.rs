//! Photos and files: XEP-0363 HTTP upload, XEP-0454 encrypted media
//! (`aesgcm://` links with the key in the fragment) and XEP-0066 links.
//!
//! Sending encrypts the file with a fresh AES-256-GCM key unless the
//! message is sent as plaintext, uploads it to the server's upload service,
//! and sends its link as the message body (inside OMEMO when the chat is
//! encrypted). Receiving parses the link into an attachment; `download`
//! fetches and decrypts it into the profile's `files/` directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::aes::Aes256;
use aes_gcm::{Aes256Gcm, AesGcm, Nonce};
use agent_chat_proto::ipc::{EncryptionMode, ErrorCode, IpcError};
use agent_chat_proto::model::{Attachment, Message, mime_for};
use futures::StreamExt;
use xmpp_parsers::disco::{DiscoInfoQuery, DiscoInfoResult, DiscoItemsQuery, DiscoItemsResult};
use xmpp_parsers::jid::Jid;
use xmpp_parsers::minidom::Element;

use crate::core::Core;
use crate::xmpp::session::SessionHandle;

pub const NS_UPLOAD: &str = "urn:xmpp:http:upload:0";
/// Largest file we send or download.
pub const MAX_FILE: u64 = 100_000_000;

fn err(code: ErrorCode, msg: impl Into<String>) -> IpcError {
    IpcError::new(code, msg.into())
}

fn bad(msg: impl Into<String>) -> IpcError {
    err(ErrorCode::BadRequest, msg)
}

fn internal(e: impl std::fmt::Display) -> IpcError {
    err(ErrorCode::Internal, e.to_string())
}

/// The server's upload service and its size limit.
#[derive(Debug, Clone, PartialEq)]
pub struct UploadService {
    pub jid: String,
    pub max_size: Option<u64>,
}

fn upload_service_of(info: &DiscoInfoResult) -> Option<Option<u64>> {
    if !info.features.iter().any(|f| f == NS_UPLOAD) {
        return None;
    }
    let max = info
        .extensions
        .iter()
        .find(|f| f.form_type() == Some(NS_UPLOAD))
        .and_then(|f| f.fields.iter().find(|x| x.var.as_deref() == Some("max-file-size")))
        .and_then(|x| x.values.first())
        .and_then(|v| v.parse().ok());
    Some(max)
}

/// Find the upload service among the server's disco items. Remembered for
/// the session.
pub async fn upload_service(core: &Core, handle: &SessionHandle) -> Result<UploadService, IpcError> {
    if let Some(s) = core.state().upload_service.clone() {
        return Ok(s);
    }
    let domain = handle.own.domain().to_string();
    let server: Option<Jid> = Jid::new(&domain).ok();
    if let Ok(Some(el)) = handle.iq_get(server, DiscoItemsQuery { node: None, rsm: None }).await
        && let Ok(items) = DiscoItemsResult::try_from(el)
    {
        for item in items.items.iter().filter(|i| i.node.is_none()).take(20) {
            let info = handle
                .iq_get(Some(item.jid.clone()), DiscoInfoQuery { node: None })
                .await
                .ok()
                .flatten()
                .and_then(|el| DiscoInfoResult::try_from(el).ok());
            if let Some(max_size) = info.as_ref().and_then(upload_service_of) {
                let s = UploadService {
                    jid: item.jid.to_string(),
                    max_size,
                };
                core.state().upload_service = Some(s.clone());
                return Ok(s);
            }
        }
    }
    Err(err(
        ErrorCode::NotFound,
        format!("{domain} has no file upload service (XEP-0363)"),
    ))
}

// ---------------------------------------------------------------- crypto

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random number generator failed");
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
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

/// AES-256-GCM with a fresh key and 12-byte IV; the tag is appended.
pub fn encrypt(data: &[u8]) -> (Vec<u8>, [u8; 12], [u8; 32]) {
    let (iv, key) = (random::<12>(), random::<32>());
    let cipher = Aes256Gcm::new_from_slice(&key).expect("32-byte key");
    let out = cipher
        .encrypt(Nonce::from_slice(&iv), data)
        .expect("AES-GCM encryption can't fail for in-memory data");
    (out, iv, key)
}

/// Decrypt with a 12-byte or (older clients) 16-byte IV.
pub fn decrypt(data: &[u8], iv: &[u8], key: &[u8]) -> Option<Vec<u8>> {
    match iv.len() {
        12 => Aes256Gcm::new_from_slice(key)
            .ok()?
            .decrypt(Nonce::from_slice(iv), data)
            .ok(),
        16 => AesGcm::<Aes256, aes_gcm::aead::consts::U16>::new_from_slice(key)
            .ok()?
            .decrypt(aes_gcm::aead::generic_array::GenericArray::from_slice(iv), data)
            .ok(),
        _ => None,
    }
}

/// `https://host/path` + IV + key → `aesgcm://host/path#<iv><key>`.
pub fn aesgcm_url(get_url: &str, iv: &[u8], key: &[u8]) -> String {
    let rest = get_url
        .strip_prefix("https://")
        .or_else(|| get_url.strip_prefix("http://"))
        .unwrap_or(get_url);
    format!("aesgcm://{rest}#{}{}", hex(iv), hex(key))
}

/// An `aesgcm://` link → (the URL to fetch, IV, key). `plain_http`: the
/// upload host serves plain HTTP (a hub on this machine).
fn parse_aesgcm(url: &str, plain_http: bool) -> Option<(String, Vec<u8>, Vec<u8>)> {
    let rest = url.strip_prefix("aesgcm://")?;
    let (path, frag) = rest.split_once('#')?;
    let bytes = unhex(frag)?;
    let iv_len = match bytes.len() {
        44 => 12,
        48 => 16,
        _ => return None,
    };
    let scheme = if plain_http { "http" } else { "https" };
    Some((
        format!("{scheme}://{path}"),
        bytes[..iv_len].to_vec(),
        bytes[iv_len..].to_vec(),
    ))
}

/// Whether a host only serves plain HTTP: loopback addresses (a local hub).
fn local_host(path: &str) -> bool {
    let host = path.split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

// --------------------------------------------------------------- sending

fn http() -> Result<reqwest::Client, IpcError> {
    reqwest::Client::builder()
        .user_agent(concat!("agent-chat/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(internal)
}

struct Slot {
    put: String,
    headers: Vec<(String, String)>,
    get: String,
}

async fn request_slot(
    handle: &SessionHandle,
    service: &UploadService,
    filename: &str,
    size: u64,
    content_type: &str,
) -> Result<Slot, IpcError> {
    let attr = |n: &str| xmpp_parsers::minidom::rxml::NcName::try_from(n).expect("valid attribute name");
    let req = Element::builder("request", NS_UPLOAD)
        .attr(attr("filename"), filename)
        .attr(attr("size"), size.to_string())
        .attr(attr("content-type"), content_type)
        .build();
    let to = Jid::new(&service.jid).map_err(internal)?;
    let el = handle
        .iq_get(Some(to), req)
        .await
        .map_err(|e| err(ErrorCode::Internal, format!("the upload service refused: {e:#}")))?
        .ok_or_else(|| internal("empty upload slot"))?;
    let put = el
        .get_child("put", NS_UPLOAD)
        .ok_or_else(|| internal("upload slot without a PUT URL"))?;
    let get = el
        .get_child("get", NS_UPLOAD)
        .ok_or_else(|| internal("upload slot without a GET URL"))?;
    // XEP-0363 §4: only these headers may be passed on.
    let headers = put
        .children()
        .filter(|h| h.is("header", NS_UPLOAD))
        .filter_map(|h| Some((h.attr("name")?.to_owned(), h.text())))
        .filter(|(n, _)| ["authorization", "cookie", "expires"].contains(&n.to_ascii_lowercase().as_str()))
        .collect();
    Ok(Slot {
        put: put.attr("url").unwrap_or_default().to_owned(),
        headers,
        get: get.attr("url").unwrap_or_default().to_owned(),
    })
}

/// Where this profile keeps sent and downloaded files.
fn files_dir(core: &Core) -> PathBuf {
    core.profile.dir.join("files")
}

fn safe_name(name: &str) -> String {
    let n: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " ._-()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    let n = n.trim_start_matches('.').trim().to_owned();
    if n.is_empty() { "file".into() } else { n }
}

/// Copy `data` into the files directory as `<prefix>-<name>`.
async fn keep_file(core: &Core, prefix: &str, name: &str, data: &[u8]) -> Result<PathBuf, IpcError> {
    let dir = files_dir(core);
    tokio::fs::create_dir_all(&dir).await.map_err(internal)?;
    let path = dir.join(format!("{prefix}-{}", safe_name(name)));
    tokio::fs::write(&path, data).await.map_err(internal)?;
    Ok(path)
}

/// Upload `path` and send it to `to`.
pub async fn send_file(
    core: &Arc<Core>,
    handle: &SessionHandle,
    to: &str,
    path: &Path,
    room: Option<bool>,
    mode: EncryptionMode,
) -> Result<Message, IpcError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| bad(format!("can't read {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(bad(format!("{} is not a file", path.display())));
    }
    let service = upload_service(core, handle).await?;
    let limit = service.max_size.unwrap_or(MAX_FILE).min(MAX_FILE);
    if meta.len() + 16 > limit {
        return Err(bad(format!(
            "{} is {} bytes; the limit is {limit}",
            path.display(),
            meta.len()
        )));
    }
    let data = tokio::fs::read(path).await.map_err(internal)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let mime = mime_for(&name);
    let encrypt_file = mode != EncryptionMode::Plaintext;
    let (payload, key) = if encrypt_file {
        let (c, iv, key) = encrypt(&data);
        (c, Some((iv, key)))
    } else {
        (data.clone(), None)
    };
    let content_type = if encrypt_file {
        "application/octet-stream"
    } else {
        mime.unwrap_or("application/octet-stream")
    };
    let slot = request_slot(handle, &service, &name, payload.len() as u64, content_type).await?;
    let mut put = http()?
        .put(&slot.put)
        .header("Content-Type", content_type)
        .timeout(std::time::Duration::from_secs(600))
        .body(payload);
    for (k, v) in &slot.headers {
        put = put.header(k, v);
    }
    let resp = put.send().await.map_err(|e| internal(format!("upload failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(internal(format!("upload failed: HTTP {}", resp.status())));
    }
    let url = match key {
        Some((iv, key)) => aesgcm_url(&slot.get, &iv, &key),
        None => slot.get.clone(),
    };
    let local = keep_file(core, &uuid::Uuid::new_v4().simple().to_string()[..8], &name, &data).await?;
    let attachment = Attachment {
        url: url.clone(),
        name,
        mime: mime.map(str::to_owned),
        size: Some(data.len() as u64),
        encrypted: encrypt_file,
        path: Some(local.to_string_lossy().into_owned()),
    };
    let oob = (!encrypt_file).then(|| url.clone());
    crate::xmpp::actions::send_message_with(core, handle, to, url, room, mode, oob, Some(attachment)).await
}

// ------------------------------------------------------------- receiving

/// Fetch (and decrypt) a message's attachment into the files directory;
/// returns the message with `attachment.path` set. Already downloaded
/// files aren't fetched again.
pub async fn download(core: &Arc<Core>, id: i64) -> Result<Message, IpcError> {
    let msg = core
        .store()
        .get_message(id)
        .map_err(internal)?
        .ok_or_else(|| err(ErrorCode::NotFound, format!("no message {id}")))?;
    let Some(att) = msg.attachment.clone() else {
        return Err(bad(format!("message {id} has no attachment")));
    };
    if att.path.as_deref().is_some_and(|p| Path::new(p).is_file()) {
        return Ok(msg);
    }
    let (url, key) = if att.encrypted {
        let rest = att.url.strip_prefix("aesgcm://").unwrap_or_default();
        let (u, iv, key) = parse_aesgcm(&att.url, local_host(rest)).ok_or_else(|| bad("malformed aesgcm:// link"))?;
        (u, Some((iv, key)))
    } else {
        (att.url.clone(), None)
    };
    let resp = http()?
        .get(&url)
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await
        .map_err(|e| internal(format!("download failed: {e}")))?;
    if !resp.status().is_success() {
        return Err(internal(format!("download failed: HTTP {}", resp.status())));
    }
    if resp.content_length().is_some_and(|n| n > MAX_FILE) {
        return Err(bad(format!("the file is larger than {MAX_FILE} bytes")));
    }
    let mut data = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        data.extend_from_slice(&chunk.map_err(|e| internal(format!("download failed: {e}")))?);
        if data.len() as u64 > MAX_FILE {
            return Err(bad(format!("the file is larger than {MAX_FILE} bytes")));
        }
    }
    if let Some((iv, key)) = key {
        data = decrypt(&data, &iv, &key).ok_or_else(|| internal("the file didn't decrypt (wrong key or damaged)"))?;
    }
    let path = keep_file(core, &id.to_string(), &att.name, &data).await?;
    core.store()
        .set_attachment_file(id, &path.to_string_lossy(), data.len() as u64)
        .map_err(internal)?;
    core.store()
        .get_message(id)
        .map_err(internal)?
        .ok_or_else(|| internal("message vanished"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_link_and_decrypt_round_trip() {
        let (c, iv, key) = encrypt(b"hello photo");
        let url = aesgcm_url("https://up.example.org/upload/t/p.jpg", &iv, &key);
        assert!(url.starts_with("aesgcm://up.example.org/upload/t/p.jpg#"));
        let (get, iv2, key2) = parse_aesgcm(&url, false).unwrap();
        assert_eq!(get, "https://up.example.org/upload/t/p.jpg");
        assert_eq!(decrypt(&c, &iv2, &key2).unwrap(), b"hello photo");
        assert!(decrypt(&c, &iv2, &[0u8; 32]).is_none());
        let local = aesgcm_url("http://127.0.0.1:5280/upload/t/p.jpg", &iv, &key);
        let rest = local.strip_prefix("aesgcm://").unwrap();
        assert!(local_host(rest));
        assert!(
            parse_aesgcm(&local, true)
                .unwrap()
                .0
                .starts_with("http://127.0.0.1:5280/")
        );
        assert!(!local_host("up.example.org/x"));
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_name("../../.ssh/id_rsa"), "_.._.ssh_id_rsa");
        assert_eq!(safe_name("My Photo (1).jpg"), "My Photo (1).jpg");
    }
}
