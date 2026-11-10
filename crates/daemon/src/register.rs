//! Account self-registration (XEP-0077), with the proof-of-work extension
//! `urn:xmpp:ibr-pow:0` (docs/design/pow-registration.md).
//!
//! Runs without an account, so it opens its own unauthenticated stream
//! instead of going through the daemon.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use futures::{SinkExt, StreamExt};
use sha2::{Digest, Sha256};
use tokio_xmpp::connect::{DnsConfig, ServerConnector, StartTlsServerConnector, TcpServerConnector};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::xmlstream::{Timeouts, XmlStream, XmppStreamElement};
use xmpp_parsers::iq::Iq;
use xmpp_parsers::jid::{BareJid, Jid};
use xmpp_parsers::stanza::Stanza;

use crate::config::is_loopback_host;
use crate::omemo::xml::escape;

pub const NS_POW: &str = "urn:xmpp:ibr-pow:0";
const NS_REGISTER: &str = "jabber:iq:register";
const NS_DATA: &str = "jabber:x:data";

fn pow_field(name: &str) -> String {
    format!("{{{NS_POW}}}{name}")
}

#[derive(Debug, Clone)]
pub struct RegisterOptions {
    /// Server domain (the part after `@`).
    pub domain: String,
    /// `None`: generate `agent-xxxxxxxxxx`.
    pub username: Option<String>,
    pub password: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub insecure_plaintext: bool,
    pub threads: usize,
    /// Budget for the whole operation.
    pub timeout: Duration,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PowStats {
    pub algorithm: String,
    pub bits: u32,
    pub count: u32,
    pub hashes: u64,
    pub elapsed_ms: u64,
    pub threads: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Registered {
    pub jid: String,
    /// `None` when the server didn't ask for a proof of work.
    pub pow: Option<PowStats>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Progress {
    PowChallenge {
        server: String,
        username: String,
        algorithm: String,
        bits: u32,
        count: u32,
        expected_hashes: u64,
        threads: usize,
        estimated_seconds: u64,
    },
    PowProgress {
        solved: u32,
        count: u32,
        hashes: u64,
        elapsed_ms: u64,
    },
    Retrying {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("the server doesn't offer in-band registration")]
    NotOffered,
    #[error("the registration form needs fields this client can't fill: {0}")]
    UnsupportedForm(String),
    #[error("username {0} is already taken")]
    Taken(String),
    #[error("refused by the server: {0}")]
    Policy(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("{0:#}")]
    Other(#[from] anyhow::Error),
}

impl RegisterError {
    /// CLI exit code (see `agent-chat account register --help`).
    pub fn exit_code(&self) -> u8 {
        match self {
            RegisterError::Timeout(_) => 2,
            RegisterError::NotOffered | RegisterError::Policy(_) => 5,
            RegisterError::Taken(_) => 6,
            RegisterError::UnsupportedForm(_) | RegisterError::Other(_) => 1,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            RegisterError::NotOffered => "registration_not_offered",
            RegisterError::UnsupportedForm(_) => "unsupported_form",
            RegisterError::Taken(_) => "username_taken",
            RegisterError::Policy(_) => "refused",
            RegisterError::Timeout(_) => "timeout",
            RegisterError::Other(_) => "error",
        }
    }
}

// ------------------------------------------------------------------ solver

/// SHA-256 puzzle from §5.5: `H_i = SHA-256(prefix || i || N_i)` must start
/// with `bits` zero bits, where `prefix = NS 0 token 0 domain 0 username 0`.
pub fn puzzle_prefix(token: &str, domain: &str, username: &str) -> Vec<u8> {
    let mut p = Vec::new();
    for part in [NS_POW, token, domain, username] {
        p.extend_from_slice(part.as_bytes());
        p.push(0);
    }
    p
}

pub fn leading_zero_bits(digest: &[u8]) -> u32 {
    let mut n = 0;
    for b in digest {
        if *b == 0 {
            n += 8;
        } else {
            return n + b.leading_zeros();
        }
    }
    n
}

pub fn check_nonce(prefix: &[u8], index: u8, nonce: u64, bits: u32) -> bool {
    let mut h = Sha256::new();
    h.update(prefix);
    h.update([index]);
    h.update(nonce.to_be_bytes());
    leading_zero_bits(&h.finalize()) >= bits
}

/// Find one nonce for sub-puzzle `index` using `threads` threads.
fn solve_one(
    prefix: &[u8],
    index: u8,
    bits: u32,
    threads: usize,
    stop: &AtomicBool,
    hashes: &AtomicU64,
) -> Option<u64> {
    let mut base = Sha256::new();
    base.update(prefix);
    base.update([index]);
    let found = AtomicU64::new(u64::MAX);
    std::thread::scope(|s| {
        for t in 0..threads as u64 {
            let (base, found) = (base.clone(), &found);
            s.spawn(move || {
                // Random start so restarts don't redo the same work.
                let mut n = u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap())
                    .wrapping_mul(threads as u64)
                    .wrapping_add(t);
                while found.load(Ordering::Relaxed) == u64::MAX && !stop.load(Ordering::Relaxed) {
                    for _ in 0..4096 {
                        let mut h = base.clone();
                        h.update(n.to_be_bytes());
                        if leading_zero_bits(&h.finalize()) >= bits {
                            let _ = found.compare_exchange(u64::MAX, n, Ordering::SeqCst, Ordering::SeqCst);
                            break;
                        }
                        n = n.wrapping_add(threads as u64);
                    }
                    hashes.fetch_add(4096, Ordering::Relaxed);
                }
            });
        }
    });
    let n = found.into_inner();
    (n != u64::MAX).then_some(n)
}

/// Single-thread hash rate measured over ~`dur`.
fn measure_rate(dur: Duration) -> f64 {
    let prefix = puzzle_prefix("benchmark", "example", "user");
    let mut base = Sha256::new();
    base.update(&prefix);
    let start = Instant::now();
    let mut n: u64 = 0;
    while start.elapsed() < dur {
        for _ in 0..4096 {
            let mut h = base.clone();
            h.update(n.to_be_bytes());
            std::hint::black_box(h.finalize());
            n += 1;
        }
    }
    n as f64 / start.elapsed().as_secs_f64()
}

// --------------------------------------------------------------- protocol

#[derive(Debug, Clone)]
struct PowChallenge {
    token: String,
    bits: u32,
    count: u32,
}

#[derive(Debug)]
struct Form {
    pow: Option<PowChallenge>,
}

type Stream = XmlStream<Box<dyn tokio_xmpp::connect::AsyncReadAndWrite + Send + 'static>, XmppStreamElement>;

async fn connect(opts: &RegisterOptions) -> Result<Stream, RegisterError> {
    let domain = BareJid::new(&opts.domain).map_err(|e| anyhow!("invalid server domain: {e}"))?;
    if domain.node().is_some() {
        return Err(anyhow!("give the server domain (example.org), not a JID").into());
    }
    let jid: Jid = domain.clone().into();
    let dns = match &opts.host {
        Some(h) => DnsConfig::no_srv(h, opts.port.unwrap_or(5222)),
        None => DnsConfig::srv_default_client(domain.domain().as_str()),
    };
    let timeouts = Timeouts::default();
    let (features, stream) = if opts.insecure_plaintext {
        let host = opts.host.clone().unwrap_or_else(|| opts.domain.clone());
        if !is_loopback_host(&host) {
            return Err(anyhow!("--insecure-plaintext is only allowed for localhost servers").into());
        }
        let (pending, _) = TcpServerConnector(dns)
            .connect(&jid, xmpp_parsers::ns::JABBER_CLIENT, timeouts)
            .await
            .map_err(|e| anyhow!("connecting to {}: {e}", opts.domain))?;
        finish(pending).await?
    } else {
        let (pending, _) = StartTlsServerConnector(dns)
            .connect(&jid, xmpp_parsers::ns::JABBER_CLIENT, timeouts)
            .await
            .map_err(|e| anyhow!("connecting to {}: {e}", opts.domain))?;
        finish(pending).await?
    };
    if !features.in_band_registration {
        return Err(RegisterError::NotOffered);
    }
    Ok(stream)
}

async fn finish<Io>(
    pending: tokio_xmpp::xmlstream::PendingFeaturesRecv<Io>,
) -> Result<(xmpp_parsers::stream_features::StreamFeatures, Stream), RegisterError>
where
    Io: tokio_xmpp::connect::AsyncReadAndWrite + Send + 'static,
{
    let (features, stream) = pending
        .recv_features::<XmppStreamElement>()
        .await
        .map_err(|e| anyhow!("reading stream features: {e}"))?;
    Ok((features, stream.box_stream()))
}

/// Send an IQ and wait for the response with the same id.
async fn iq(stream: &mut Stream, iq: Iq) -> Result<Iq, RegisterError> {
    let id = iq.id().to_owned();
    stream
        .send(&XmppStreamElement::Stanza(Stanza::Iq(iq)))
        .await
        .map_err(|e| anyhow!("sending: {e}"))?;
    // A server that never answers (e.g. a crashing module) must not hang us.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let next = tokio::time::timeout_at(deadline, stream.next())
            .await
            .map_err(|_| RegisterError::Timeout("the server didn't answer within 60 s".into()))?;
        match next {
            Some(Ok(XmppStreamElement::Stanza(Stanza::Iq(resp)))) if resp.id() == id => return Ok(resp),
            Some(Ok(_)) => continue,
            Some(Err(tokio_xmpp::xmlstream::ReadError::SoftTimeout)) => continue,
            Some(Err(e)) => return Err(anyhow!("connection error: {e}").into()),
            None => return Err(anyhow!("the server closed the connection").into()),
        }
    }
}

fn field_values(form: &Element) -> Vec<(String, Option<String>, bool, String)> {
    form.children()
        .filter(|c| c.name() == "field")
        .map(|f| {
            (
                f.attr("var").unwrap_or_default().to_owned(),
                f.get_child("value", NS_DATA).map(|v| v.text()),
                f.get_child("required", NS_DATA).is_some(),
                f.attr("type").unwrap_or("text-single").to_owned(),
            )
        })
        .collect()
}

async fn fetch_form(stream: &mut Stream, domain: &Jid) -> Result<Form, RegisterError> {
    let query = Element::builder("query", NS_REGISTER).build();
    let get = Iq::Get {
        from: None,
        to: Some(domain.clone()),
        id: uuid::Uuid::new_v4().to_string(),
        payload: query,
    };
    let resp = iq(stream, get).await?;
    let payload = match resp {
        Iq::Result { payload: Some(p), .. } => p,
        Iq::Error { error, .. } => {
            return Err(RegisterError::Policy(
                error
                    .texts
                    .values()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| format!("{:?}", error.defined_condition)),
            ));
        }
        _ => return Err(anyhow!("empty registration form").into()),
    };
    let Some(form) = payload.get_child("x", NS_DATA) else {
        // Legacy-only form: we can fill username/password.
        let unknown: Vec<String> = payload
            .children()
            .map(|c| c.name().to_owned())
            .filter(|n| !["instructions", "username", "password", "registered"].contains(&n.as_str()))
            .collect();
        if !unknown.is_empty() {
            return Err(RegisterError::UnsupportedForm(unknown.join(", ")));
        }
        return Ok(Form { pow: None });
    };
    let fields = field_values(form);
    let get = |var: &str| fields.iter().find(|f| f.0 == var).and_then(|f| f.1.clone());
    let known =
        |var: &str| matches!(var, "FORM_TYPE" | "username" | "password") || var.starts_with(&format!("{{{NS_POW}}}"));
    let unknown: Vec<String> = fields
        .iter()
        .filter(|(var, _, required, _)| *required && !known(var))
        .map(|f| f.0.clone())
        .collect();
    if !unknown.is_empty() {
        return Err(RegisterError::UnsupportedForm(unknown.join(", ")));
    }
    let pow = match get(&pow_field("challenge")) {
        None => None,
        Some(token) => {
            let algorithm = get(&pow_field("algorithm")).unwrap_or_default();
            if algorithm != "sha256" {
                return Err(RegisterError::Policy(format!(
                    "unsupported proof-of-work algorithm {algorithm:?}"
                )));
            }
            let bits = get(&pow_field("bits"))
                .and_then(|b| b.parse().ok())
                .context("bad bits")?;
            let count = get(&pow_field("count"))
                .and_then(|b| b.parse().ok())
                .context("bad count")?;
            if bits > 48 || count == 0 || count > 64 {
                return Err(RegisterError::Policy(format!(
                    "unreasonable challenge: {count} × {bits} bits"
                )));
            }
            Some(PowChallenge { token, bits, count })
        }
    };
    Ok(Form { pow })
}

/// What the server said about a submission.
enum Submit {
    Ok,
    /// Fetch a new challenge and try again.
    Retry(String),
    Failed(RegisterError),
}

async fn submit(
    stream: &mut Stream,
    domain: &Jid,
    username: &str,
    password: &str,
    pow: Option<(&PowChallenge, &[u64])>,
) -> Result<Submit, RegisterError> {
    let mut fields = format!(
        "<field var='FORM_TYPE'><value>{NS_REGISTER}</value></field>\
         <field var='username'><value>{}</value></field>\
         <field var='password'><value>{}</value></field>",
        escape(username),
        escape(password)
    );
    if let Some((c, nonces)) = pow {
        let solution: Vec<String> = nonces.iter().map(|n| format!("{n:016x}")).collect();
        fields.push_str(&format!(
            "<field var='{}'><value>{}</value></field><field var='{}'><value>{}</value></field>",
            escape(&pow_field("challenge")),
            escape(&c.token),
            escape(&pow_field("solution")),
            solution.join(",")
        ));
    }
    let query: Element =
        format!("<query xmlns='{NS_REGISTER}'><x xmlns='{NS_DATA}' type='submit'>{fields}</x></query>")
            .parse()
            .map_err(|e| anyhow!("building form: {e}"))?;
    let set = Iq::Set {
        from: None,
        to: Some(domain.clone()),
        id: uuid::Uuid::new_v4().to_string(),
        payload: query,
    };
    let resp = iq(stream, set).await?;
    let error = match resp {
        Iq::Result { .. } => return Ok(Submit::Ok),
        Iq::Error { error, .. } => error,
        _ => return Err(anyhow!("unexpected response").into()),
    };
    let text = error.texts.values().next().cloned().unwrap_or_default();
    let app = error
        .other
        .as_ref()
        .filter(|el| el.ns() == NS_POW)
        .map(|el| el.name().to_owned());
    use xmpp_parsers::stanza_error::DefinedCondition as C;
    Ok(match (app.as_deref(), &error.defined_condition) {
        (Some("challenge-expired" | "challenge-invalid" | "challenge-outdated" | "challenge-used"), _) => {
            Submit::Retry(app.unwrap())
        }
        (Some("solution-invalid"), _) => Submit::Failed(anyhow!("server rejected the proof of work: {text}").into()),
        (Some(other), _) => Submit::Failed(RegisterError::Policy(format!("{other}: {text}"))),
        (None, C::Conflict) => Submit::Failed(RegisterError::Taken(username.to_owned())),
        (None, c) => Submit::Failed(RegisterError::Policy(if text.is_empty() {
            format!("{c:?}")
        } else {
            text
        })),
    })
}

// ------------------------------------------------------------------ driver

const BASE32: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";

pub fn generate_username() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let tail: String = bytes[..10].iter().map(|b| BASE32[(*b as usize) % 32] as char).collect();
    format!("agent-{tail}")
}

pub fn generate_password() -> String {
    use base64::Engine;
    let mut b = uuid::Uuid::new_v4().into_bytes().to_vec();
    b.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Usernames we hash exactly as the server's nodeprep will see them.
pub fn valid_username(u: &str) -> bool {
    let b = u.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
}

async fn solve(
    c: &PowChallenge,
    domain: &str,
    username: &str,
    threads: usize,
    deadline: Instant,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<(Vec<u64>, PowStats), RegisterError> {
    let prefix = Arc::new(puzzle_prefix(&c.token, domain, username));
    let stop = Arc::new(AtomicBool::new(false));
    let hashes = Arc::new(AtomicU64::new(0));
    let solved = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    let (bits, count) = (c.bits, c.count);
    let worker = {
        let (prefix, stop, hashes, solved) = (prefix.clone(), stop.clone(), hashes.clone(), solved.clone());
        tokio::task::spawn_blocking(move || {
            let mut nonces = Vec::with_capacity(count as usize);
            for i in 0..count {
                match solve_one(&prefix, i as u8, bits, threads, &stop, &hashes) {
                    Some(n) => {
                        nonces.push(n);
                        solved.fetch_add(1, Ordering::Relaxed);
                    }
                    None => return None,
                }
            }
            Some(nonces)
        })
    };
    tokio::pin!(worker);
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    ticker.tick().await;
    loop {
        tokio::select! {
            r = &mut worker => {
                let nonces = r.map_err(|e| anyhow!("solver crashed: {e}"))?
                    .ok_or_else(|| RegisterError::Timeout("proof of work not finished in time".into()))?;
                let stats = PowStats {
                    algorithm: "sha256".into(),
                    bits,
                    count,
                    hashes: hashes.load(Ordering::Relaxed),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                    threads,
                };
                return Ok((nonces, stats));
            }
            _ = tokio::time::sleep_until(deadline.into()) => {
                stop.store(true, Ordering::Relaxed);
            }
            _ = ticker.tick() => {
                progress(Progress::PowProgress {
                    solved: solved.load(Ordering::Relaxed) as u32,
                    count,
                    hashes: hashes.load(Ordering::Relaxed),
                    elapsed_ms: start.elapsed().as_millis() as u64,
                });
            }
        }
    }
}

/// Register an account. `progress` receives events for stderr.
pub async fn register(
    opts: &RegisterOptions,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<Registered, RegisterError> {
    let deadline = Instant::now() + opts.timeout;
    let domain_jid: Jid = BareJid::new(&opts.domain)
        .map_err(|e| anyhow!("invalid server domain: {e}"))?
        .into();
    let explicit = opts.username.is_some();
    let mut username = match &opts.username {
        Some(u) => {
            let u = u.to_lowercase();
            if !valid_username(&u) {
                return Err(anyhow!("usernames may use a-z, 0-9, '.', '_' and '-' (got {u:?})").into());
            }
            u
        }
        None => generate_username(),
    };
    let threads = opts.threads.max(1);
    let mut stream = connect(opts).await?;
    let mut retried_challenge = false;
    let mut retried_name = false;

    loop {
        let form = fetch_form(&mut stream, &domain_jid).await?;
        let (nonces, stats) = match &form.pow {
            None => (vec![], None),
            Some(c) => {
                let expected = (c.count as u64) << c.bits;
                let rate = measure_rate(Duration::from_millis(300)) * threads as f64;
                let estimate = (expected as f64 / rate).ceil() as u64;
                progress(Progress::PowChallenge {
                    server: opts.domain.clone(),
                    username: username.clone(),
                    algorithm: "sha256".into(),
                    bits: c.bits,
                    count: c.count,
                    expected_hashes: expected,
                    threads,
                    estimated_seconds: estimate,
                });
                let remaining = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
                if estimate as f64 * 1.6 > remaining {
                    return Err(RegisterError::Timeout(format!(
                        "estimated {estimate}s of work but only {remaining:.0}s left; raise --timeout or --threads"
                    )));
                }
                let (n, s) = solve(c, &opts.domain, &username, threads, deadline, progress).await?;
                (n, Some(s))
            }
        };
        // The server may have dropped an idle unauthenticated stream while we
        // solved; the challenge is stateless, so just reconnect.
        let pow = form.pow.as_ref().map(|c| (c, nonces.as_slice()));
        let outcome = match submit(&mut stream, &domain_jid, &username, &opts.password, pow).await {
            Ok(o) => o,
            Err(RegisterError::Other(_)) => {
                stream = connect(opts).await?;
                submit(&mut stream, &domain_jid, &username, &opts.password, pow).await?
            }
            Err(e) => return Err(e),
        };
        match outcome {
            Submit::Ok => {
                let _ = SinkExt::<&XmppStreamElement>::close(&mut stream).await;
                return Ok(Registered {
                    jid: format!("{username}@{}", opts.domain),
                    pow: stats,
                });
            }
            Submit::Retry(why) if !retried_challenge => {
                retried_challenge = true;
                progress(Progress::Retrying { reason: why });
            }
            Submit::Failed(RegisterError::Taken(_)) if !explicit && !retried_name => {
                retried_name = true;
                username = generate_username();
                progress(Progress::Retrying {
                    reason: "generated username was taken".into(),
                });
            }
            Submit::Retry(why) => return Err(RegisterError::Policy(why)),
            Submit::Failed(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/design/pow-registration.md §5.7, also checked by the Lua module.
    #[test]
    fn design_test_vector_verifies() {
        let token =
            "1:sha256:25:8:1794326400:1794330000:9f2c4e7a1b3d5f60718293a4b5c6d7e8:6b8ae478ff3592e715741262396837c1";
        let nonces = [
            0x0000000001dee49fu64,
            0x000000000212fd53,
            0x00000000006093ad,
            0x000000000016321f,
            0x00000000002aa9f1,
            0x0000000003f1a352,
            0x00000000022ca4ce,
            0x000000000014a0b1,
        ];
        let prefix = puzzle_prefix(token, "example.org", "agent-7k2m9qx4td");
        for (i, n) in nonces.iter().enumerate() {
            assert!(check_nonce(&prefix, i as u8, *n, 25), "nonce {i}");
        }
        // Bound to the username.
        let other = puzzle_prefix(token, "example.org", "agent-7k2m9qx4tX");
        assert!(!check_nonce(&other, 0, nonces[0], 25));
    }

    #[test]
    fn solver_finds_valid_nonces() {
        let prefix = puzzle_prefix("t", "d", "u");
        let stop = AtomicBool::new(false);
        let hashes = AtomicU64::new(0);
        for i in 0..4u8 {
            let n = solve_one(&prefix, i, 12, 2, &stop, &hashes).unwrap();
            assert!(check_nonce(&prefix, i, n, 12));
        }
        assert!(hashes.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn zero_bit_counting() {
        assert_eq!(leading_zero_bits(&[0, 0, 0b0001_0000]), 19);
        assert_eq!(leading_zero_bits(&[0x80]), 0);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
    }

    #[test]
    fn usernames() {
        assert!(valid_username(&generate_username()));
        assert!(valid_username("agent.7_k-2"));
        assert!(!valid_username("Agent"));
        assert!(!valid_username("-x"));
        assert!(!valid_username("a b"));
        assert!(generate_password().len() >= 40);
    }
}
