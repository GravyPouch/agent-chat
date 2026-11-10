//! `hub.toml`: schema, defaults and validation (§5.9).
//!
//! Every key is optional except `domain`. Sections that later milestones add
//! (`[registration]`, `[sm]`, `[archive]`, `[muc]`, `[http]`, `[metrics]`) are
//! simply new fields here; unknown top-level sections are ignored so a newer
//! `hub.toml` still loads, while unknown keys inside a known section are
//! errors (they are usually typos).

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const DEFAULT_PORT: u16 = 5222;
pub const LOCAL_DOMAIN: &str = "localhost";

/// The SASL mechanisms the hub implements, strongest first.
pub const KNOWN_MECHANISMS: [&str; 5] = [
    "SCRAM-SHA-256-PLUS",
    "SCRAM-SHA-1-PLUS",
    "SCRAM-SHA-256",
    "SCRAM-SHA-1",
    "PLAIN",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Permanent: stored in the `hosts` table; the hub refuses to start if it changes.
    pub domain: String,
    #[serde(default)]
    pub listen: Listen,
    #[serde(default)]
    pub tls: Tls,
    #[serde(default)]
    pub auth: Auth,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub sm: Sm,
    #[serde(default)]
    pub archive: Archive,
    #[serde(default)]
    pub log: Log,
}

/// XEP-0198 stream management.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sm {
    /// How long a dropped session can be resumed.
    pub resume_timeout: HumanDuration,
    /// Unacknowledged stanzas kept per session; past that the session closes.
    pub max_queue: usize,
}

impl Default for Sm {
    fn default() -> Self {
        Self {
            resume_timeout: HumanDuration(Duration::from_secs(600)),
            max_queue: 1000,
        }
    }
}

/// XEP-0313 archives and offline storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Archive {
    pub enabled: bool,
    /// Messages older than this are deleted; `"never"` keeps them.
    pub expire_after: Retention,
    /// Largest page a MAM query returns (RSM `max` is capped to it).
    pub max_page: usize,
    /// Offline messages kept per account; the oldest are dropped.
    pub max_offline_messages: usize,
}

impl Default for Archive {
    fn default() -> Self {
        Self {
            enabled: true,
            expire_after: Retention(Some(Duration::from_secs(7 * 86_400))),
            max_page: 250,
            max_offline_messages: 1000,
        }
    }
}

/// A retention period, or `"never"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention(pub Option<Duration>);

impl<'de> Deserialize<'de> for Retention {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match IntOrString::deserialize(d)? {
            IntOrString::Int(n) => Ok(Retention(Some(Duration::from_secs(n)))),
            IntOrString::Str(s) if s.trim() == "never" => Ok(Retention(None)),
            IntOrString::Str(s) => parse_duration(&s)
                .map(|d| Retention(Some(d)))
                .map_err(serde::de::Error::custom),
        }
    }
}

impl Serialize for Retention {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Some(d) => s.serialize_str(&format_duration(d)),
            None => s.serialize_str("never"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Listen {
    /// Client-to-server listeners (STARTTLS on the standard port).
    pub c2s: Vec<String>,
    /// Allow SCRAM without TLS from loopback peers only.
    pub allow_plaintext_loopback: bool,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            c2s: vec![format!("127.0.0.1:{DEFAULT_PORT}"), format!("[::1]:{DEFAULT_PORT}")],
            allow_plaintext_loopback: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    /// rcgen-generated certificate; our client pins it (`hub.json`).
    SelfSigned,
    /// A local CA issues the leaf (M6).
    LocalCa,
    /// Operator-provided PEM files.
    Files,
}

impl TlsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            TlsMode::SelfSigned => "self-signed",
            TlsMode::LocalCa => "local-ca",
            TlsMode::Files => "files",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Tls {
    pub mode: TlsMode,
    pub cert: PathBuf,
    pub key: PathBuf,
    /// Extra subject alternative names, e.g. `["hub.lan", "192.168.1.20"]`.
    pub extra_names: Vec<String>,
}

impl Default for Tls {
    fn default() -> Self {
        Self {
            mode: TlsMode::SelfSigned,
            cert: PathBuf::from("tls/cert.pem"),
            key: PathBuf::from("tls/key.pem"),
            extra_names: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Auth {
    pub mechanisms: Vec<String>,
    pub scram_iterations: u32,
    /// Per account and per network prefix; then exponential backoff.
    pub max_failures_per_10min: u32,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            mechanisms: KNOWN_MECHANISMS.iter().map(|s| s.to_string()).collect(),
            scram_iterations: 10_000,
            max_failures_per_10min: 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    pub max_stanza_bytes: ByteSize,
    pub unauth_stanza_bytes: ByteSize,
    pub session_rate: ByteRate,
    pub session_burst: ByteSize,
    /// Loopback peers are exempt.
    pub max_connections_per_ip: u32,
    pub max_new_connections_per_ip_per_min: u32,
    pub max_resources_per_account: usize,
    pub negotiation_timeout: HumanDuration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_stanza_bytes: ByteSize(256 * 1024),
            unauth_stanza_bytes: ByteSize(10_000),
            session_rate: ByteRate(20_000),
            session_burst: ByteSize(200_000),
            max_connections_per_ip: 20,
            max_new_connections_per_ip_per_min: 30,
            max_resources_per_account: 10,
            negotiation_timeout: HumanDuration(Duration::from_secs(60)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Log {
    pub level: String,
    pub format: LogFormat,
    /// Used when the hub runs detached (`hub start`).
    pub file: PathBuf,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::Text,
            file: PathBuf::from("hub.log"),
        }
    }
}

impl Config {
    /// A configuration with defaults for `domain`.
    pub fn new(domain: &str) -> Self {
        Self {
            domain: domain.to_owned(),
            listen: Listen::default(),
            tls: Tls::default(),
            auth: Auth::default(),
            limits: Limits::default(),
            sm: Sm::default(),
            archive: Archive::default(),
            log: Log::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.domain = normalize_domain(&cfg.domain).with_context(|| format!("in {}", path.display()))?;
        cfg.validate().with_context(|| format!("in {}", path.display()))?;
        Ok(cfg)
    }

    pub fn to_toml(&self) -> Result<String> {
        let body = toml::to_string_pretty(self)?;
        Ok(format!(
            "# agent-chat hub configuration. See docs/design/embedded-server.md §5.9.\n\
             # `domain` is permanent: accounts can never move to another domain.\n\n{body}"
        ))
    }

    /// Startup checks (§5.9).
    pub fn validate(&self) -> Result<()> {
        if normalize_domain(&self.domain)? != self.domain {
            bail!("domain {:?} is not normalized", self.domain);
        }
        let addrs = self.c2s_addrs()?;
        if addrs.is_empty() {
            bail!("listen.c2s is empty: the hub needs at least one listener");
        }
        match self.tls.mode {
            TlsMode::SelfSigned | TlsMode::Files => {}
            TlsMode::LocalCa => bail!("tls.mode = \"local-ca\" is not implemented yet (milestone M6)"),
        }
        for m in &self.auth.mechanisms {
            if !KNOWN_MECHANISMS.contains(&m.as_str()) {
                bail!("unknown SASL mechanism {m:?} in auth.mechanisms");
            }
        }
        if !self.auth.mechanisms.iter().any(|m| m.starts_with("SCRAM-")) {
            bail!("auth.mechanisms must include at least one SCRAM mechanism");
        }
        if self.auth.scram_iterations < 4096 {
            bail!("auth.scram_iterations must be at least 4096 (RFC 5802 §5.1)");
        }
        let l = &self.limits;
        if l.unauth_stanza_bytes.0 < 1024 || l.max_stanza_bytes.0 < l.unauth_stanza_bytes.0 {
            bail!("limits: need 1024 <= unauth_stanza_bytes <= max_stanza_bytes");
        }
        if l.max_resources_per_account == 0 {
            bail!("limits.max_resources_per_account must be at least 1");
        }
        if l.negotiation_timeout.0 < Duration::from_secs(1) {
            bail!("limits.negotiation_timeout is too short");
        }
        if l.session_rate.0 == 0 || l.session_burst.0 == 0 {
            bail!("limits.session_rate and session_burst must be positive");
        }
        Ok(())
    }

    pub fn c2s_addrs(&self) -> Result<Vec<SocketAddr>> {
        self.listen.c2s.iter().map(|a| parse_listen_addr(a)).collect()
    }

    /// Whether any listener accepts non-loopback peers.
    pub fn is_lan(&self) -> bool {
        self.c2s_addrs()
            .map(|a| a.iter().any(|a| !a.ip().is_loopback()))
            .unwrap_or(false)
    }

    /// Names for the self-signed certificate.
    pub fn certificate_names(&self) -> Vec<String> {
        let mut names = vec![self.domain.clone(), format!("conference.{}", self.domain)];
        for n in &self.tls.extra_names {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        names
    }
}

pub fn parse_listen_addr(a: &str) -> Result<SocketAddr> {
    if let Ok(addr) = a.parse::<SocketAddr>() {
        return Ok(addr);
    }
    // A bare address means the standard port.
    let ip: IpAddr = a
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| anyhow!("invalid listen address {a:?}; use IP:PORT, e.g. 0.0.0.0:5222"))?;
    Ok(SocketAddr::new(ip, DEFAULT_PORT))
}

/// Validate and normalize the hub's domain: `localhost` or a DNS name
/// (`NAME.local` for LAN hubs, any domain you own for the hosted path).
pub fn normalize_domain(d: &str) -> Result<String> {
    let d = d.trim().trim_end_matches('.');
    if d.is_empty() {
        bail!("the domain is empty");
    }
    if d.parse::<IpAddr>().is_ok() || d.starts_with('[') {
        bail!("the domain must be a DNS name, not an IP address ({d:?})");
    }
    let lower = d.to_ascii_lowercase();
    if !lower.is_ascii() {
        bail!("domain {d:?}: use the ASCII (xn--) form of internationalized names");
    }
    if lower.len() > 253 {
        bail!("domain {d:?} is longer than 253 characters");
    }
    let labels: Vec<&str> = lower.split('.').collect();
    for label in &labels {
        let ok = !label.is_empty()
            && label.len() <= 63
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            bail!("domain {d:?} is not a valid DNS name (bad label {label:?})");
        }
    }
    if labels.len() > 1 && labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit())) {
        bail!("domain {d:?} looks like an IP address; use a DNS name");
    }
    // Same normalization (nameprep) as the jid crate, so JIDs compare equal.
    let normalized = xmpp_parsers::jid::DomainPart::new(&lower)
        .map_err(|e| anyhow!("domain {d:?} is not a valid XMPP domain: {e}"))?
        .into_owned()
        .to_string();
    Ok(normalized)
}

/// `<hostname>.local`: the name macOS and Avahi advertise over mDNS.
pub fn lan_domain() -> String {
    format!("{}.local", sanitize_host_label(&local_hostname()))
}

fn local_hostname() -> String {
    // macOS: the Bonjour name is the LocalHostName, which can differ from
    // gethostname() (that one may be a DHCP-provided name).
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("scutil")
        .args(["--get", "LocalHostName"])
        .output()
        && out.status.success()
    {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !name.is_empty() {
            return name;
        }
    }
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; gethostname NUL-terminates
    // on success (we also cap at the buffer length).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// First label of a host name, lowercased, invalid characters replaced by `-`.
pub fn sanitize_host_label(host: &str) -> String {
    let first = host.split('.').next().unwrap_or("");
    let mut out = String::new();
    for c in first.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    let mut out: String = out.chars().take(63).collect();
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "agent-chat-hub".to_owned()
    } else {
        out
    }
}

// ------------------------------------------------------------ value types

/// A byte count: an integer, or a string like `"200KB"` (KB = 1000, KiB = 1024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

/// Bytes per second, e.g. `"20KB/s"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRate(pub u64);

/// A duration like `"60s"`, `"10m"`, `"1h"`, `"7d"`, `"500ms"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HumanDuration(pub Duration);

fn parse_bytes(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| anyhow!("invalid size {s:?}"))?;
    let mult = match unit.trim() {
        "" | "B" => 1,
        "KB" | "kB" | "K" | "k" => 1000,
        "KiB" => 1024,
        "MB" | "M" => 1_000_000,
        "MiB" => 1024 * 1024,
        "GB" | "G" => 1_000_000_000,
        "GiB" => 1024 * 1024 * 1024,
        u => bail!("unknown size unit {u:?} in {s:?}"),
    };
    n.checked_mul(mult).ok_or_else(|| anyhow!("size {s:?} is too large"))
}

fn format_bytes(n: u64) -> String {
    if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{}MB", n / 1_000_000)
    } else if n >= 1000 && n.is_multiple_of(1000) {
        format!("{}KB", n / 1000)
    } else {
        format!("{n}B")
    }
}

pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().map_err(|_| anyhow!("invalid duration {s:?}"))?;
    Ok(match unit.trim() {
        "ms" => Duration::from_millis(n),
        "" | "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        "d" => Duration::from_secs(n * 86_400),
        u => bail!("unknown duration unit {u:?} in {s:?}; use ms, s, m, h or d"),
    })
}

fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if !ms.is_multiple_of(1000) {
        return format!("{ms}ms");
    }
    let s = d.as_secs();
    if s != 0 && s.is_multiple_of(86_400) {
        format!("{}d", s / 86_400)
    } else if s != 0 && s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s != 0 && s.is_multiple_of(60) {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum IntOrString {
    Int(u64),
    Str(String),
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match IntOrString::deserialize(d)? {
            IntOrString::Int(n) => Ok(ByteSize(n)),
            IntOrString::Str(s) => parse_bytes(&s).map(ByteSize).map_err(serde::de::Error::custom),
        }
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for ByteRate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match IntOrString::deserialize(d)? {
            IntOrString::Int(n) => Ok(ByteRate(n)),
            IntOrString::Str(s) => {
                let base = s.trim().strip_suffix("/s").unwrap_or(s.trim());
                parse_bytes(base).map(ByteRate).map_err(serde::de::Error::custom)
            }
        }
    }
}

impl Serialize for ByteRate {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}/s", format_bytes(self.0)))
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match IntOrString::deserialize(d)? {
            IntOrString::Int(n) => Ok(HumanDuration(Duration::from_secs(n))),
            IntOrString::Str(s) => parse_duration(&s).map(HumanDuration).map_err(serde::de::Error::custom),
        }
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format_duration(self.0))
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_duration(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domains() {
        assert_eq!(normalize_domain("localhost").unwrap(), "localhost");
        assert_eq!(normalize_domain("MyMac.local").unwrap(), "mymac.local");
        assert_eq!(normalize_domain("hub.example.org.").unwrap(), "hub.example.org");
        for bad in [
            "",
            "192.168.1.2",
            "::1",
            "a..b",
            "-x.local",
            "x-.local",
            "a b",
            "1.2.3",
            "hé.local",
        ] {
            assert!(normalize_domain(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn host_labels() {
        assert_eq!(sanitize_host_label("Gravys-MacBook-Pro"), "gravys-macbook-pro");
        assert_eq!(sanitize_host_label("my host.example.com"), "my-host");
        assert_eq!(sanitize_host_label("Büro PC"), "b-ro-pc");
        assert_eq!(sanitize_host_label("---"), "agent-chat-hub");
        assert!(lan_domain().ends_with(".local"));
        normalize_domain(&lan_domain()).unwrap();
    }

    #[test]
    fn sizes_and_durations() {
        assert_eq!(parse_bytes("200KB").unwrap(), 200_000);
        assert_eq!(parse_bytes("256KiB").unwrap(), 262_144);
        assert_eq!(parse_bytes("10000").unwrap(), 10_000);
        assert!(parse_bytes("10XB").is_err());
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(7 * 86_400));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(format_bytes(20_000), "20KB");
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let cfg = Config::new("localhost");
        cfg.validate().unwrap();
        let text = cfg.to_toml().unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, cfg);
        assert!(text.contains("session_rate = \"20KB/s\""));
        assert!(text.contains("negotiation_timeout = \"1m\""));
    }

    #[test]
    fn minimal_file_and_unknown_sections() {
        let cfg: Config = toml::from_str(
            "domain = \"hub.local\"\n[limits]\nsession_rate = \"5KB/s\"\n[http]\nlisten = [\"0.0.0.0:8080\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.limits.session_rate, ByteRate(5000));
        assert_eq!(cfg.listen, Listen::default());
        assert!(toml::from_str::<Config>("domain = \"x\"\n[limits]\nmax_stanza = 1\n").is_err());
    }

    #[test]
    fn validation() {
        let mut cfg = Config::new("localhost");
        cfg.listen.c2s = vec![];
        assert!(cfg.validate().is_err());
        let mut cfg = Config::new("localhost");
        cfg.auth.mechanisms = vec!["PLAIN".into()];
        assert!(cfg.validate().is_err());
        let mut cfg = Config::new("localhost");
        cfg.tls.mode = TlsMode::LocalCa;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::new("hub.local");
        cfg.listen.c2s = vec!["0.0.0.0".into()];
        assert_eq!(cfg.c2s_addrs().unwrap()[0].port(), DEFAULT_PORT);
        assert!(cfg.is_lan());
        assert!(!Config::new("localhost").is_lan());
    }
}
