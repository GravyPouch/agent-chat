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
    pub muc: Muc,
    #[serde(default)]
    pub registration: Registration,
    #[serde(default)]
    pub http: Http,
    #[serde(default)]
    pub upload: Upload,
    #[serde(default)]
    pub log: Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationMode {
    /// Anyone who can reach the hub (localhost default).
    Open,
    /// An XEP-0445 invitation is required (LAN default).
    Invite,
    /// `urn:xmpp:ibr-pow:0` proof of work, or an invitation (public default).
    Pow,
    /// Only the admin CLI creates accounts.
    Closed,
}

/// In-band (XEP-0077) and web registration. The PoW parameters have the
/// same names and defaults as `mod_register_pow`'s `register_pow_*`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Registration {
    /// Unset: `open` for a loopback-only hub, `invite` when it serves the network.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<RegistrationMode>,
    /// Registration from non-loopback peers needs TLS.
    pub require_tls: bool,
    /// Loopback clients skip the proof of work and the per-IP limit.
    pub exempt_loopback: bool,
    pub pow_bits: u32,
    pub pow_count: u32,
    pub pow_max_bits: u32,
    pub pow_ttl: HumanDuration,
    /// Bind challenges to the client's address prefix.
    pub pow_bind_ip: bool,
    pub ipv4_prefix: u8,
    pub ipv6_prefix: u8,
    /// Each success from a prefix within the window adds a bit (up to the cap).
    pub prefix_window: HumanDuration,
    pub prefix_escalation_cap: u32,
    /// Registrations per hour (all prefixes) above which difficulty rises.
    pub load_threshold: u32,
    pub load_escalation_cap: u32,
    /// Prosody's `registration_throttle_max` / `registration_throttle_period`.
    pub max_per_ip: u32,
    pub per_ip_period: HumanDuration,
    pub min_password_length: usize,
    /// Offer sign-up on the HTTP page (the browser solves the puzzle).
    pub web: bool,
    /// Browser difficulty: JavaScript hashes far slower than native code.
    pub web_pow_bits: u32,
    pub web_pow_count: u32,
    /// SHA-256 (hex) of a provisioning token. A sign-up that presents this
    /// token as its invitation creates an account in any registration mode,
    /// without a proof of work or the per-IP limit, and never runs out: for
    /// a service that creates accounts for its own agents. The environment
    /// variable `AGENT_CHAT_PROVISION_TOKEN_SHA256` overrides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provision_token_sha256: Option<String>,
}

/// Overrides `registration.provision_token_sha256`.
pub const PROVISION_TOKEN_ENV: &str = "AGENT_CHAT_PROVISION_TOKEN_SHA256";

impl Default for Registration {
    fn default() -> Self {
        Self {
            mode: None,
            require_tls: true,
            exempt_loopback: true,
            pow_bits: 25,
            pow_count: 8,
            pow_max_bits: 31,
            pow_ttl: HumanDuration(Duration::from_secs(3600)),
            pow_bind_ip: true,
            ipv4_prefix: 32,
            ipv6_prefix: 56,
            prefix_window: HumanDuration(Duration::from_secs(86_400)),
            prefix_escalation_cap: 6,
            load_threshold: 30,
            load_escalation_cap: 4,
            max_per_ip: 5,
            per_ip_period: HumanDuration(Duration::from_secs(3600)),
            min_password_length: 8,
            web: false,
            web_pow_bits: 22,
            web_pow_count: 8,
            provision_token_sha256: None,
        }
    }
}

/// A `trusted_proxies` entry: an address (`10.0.0.5`) or a CIDR range
/// (`10.0.0.0/8`), as the network address and prefix length.
pub fn parse_proxy(spec: &str) -> Option<(std::net::IpAddr, u8)> {
    let spec = spec.trim();
    let (addr, prefix) = match spec.split_once('/') {
        Some((a, p)) => (a.parse::<std::net::IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
        None => (spec.parse::<std::net::IpAddr>().ok()?, None),
    };
    let max = if addr.is_ipv4() { 32 } else { 128 };
    let prefix = prefix.unwrap_or(max);
    (prefix <= max).then_some((addr, prefix))
}

/// The onboarding page and web sign-up (HTTP, no TLS: put it behind the
/// operator's HTTPS proxy when public).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Http {
    /// e.g. `["127.0.0.1:5280"]`; empty disables the page.
    pub listen: Vec<String>,
    /// Use `X-Forwarded-For` from these proxies (e.g. the HTTPS front end) as
    /// the client address: addresses, or CIDR ranges such as `10.0.0.0/8`.
    pub trusted_proxies: Vec<String>,
    /// The public URL of the page, for links (optional).
    pub public_url: Option<String>,
}

/// XEP-0363 file upload on `<host>.<domain>`: photos and files, served by
/// the web listener (without one, the hub listens on `127.0.0.1:5280` for it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Upload {
    pub enabled: bool,
    /// The subdomain: `upload` → `upload.<domain>`.
    pub host: String,
    pub max_file_size: ByteSize,
    /// Files are deleted this long after the upload.
    pub expire_after: HumanDuration,
}

impl Default for Upload {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "upload".into(),
            max_file_size: ByteSize(100_000_000),
            expire_after: HumanDuration(Duration::from_secs(30 * 86_400)),
        }
    }
}

/// XEP-0045 group chat on `<host>.<domain>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Muc {
    pub enabled: bool,
    /// The subdomain: `conference` → `conference.<domain>`.
    pub host: String,
    /// `"local-users"` (any account of this hub) or `"admins"`.
    pub room_creation: String,
    /// New rooms are persistent unless configured otherwise.
    pub default_persistent: bool,
    /// Keep an archive of every room (room MAM), like Prosody's `muc_log_all_rooms`.
    pub archive: bool,
    /// Messages of history a joining client gets when it doesn't say.
    pub default_history: usize,
    pub max_occupants: usize,
}

impl Default for Muc {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "conference".into(),
            room_creation: "local-users".into(),
            default_persistent: true,
            archive: true,
            default_history: 20,
            max_occupants: 200,
        }
    }
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
    /// Files mode: how often to check the files for a renewed certificate.
    pub reload_interval: HumanDuration,
}

impl Default for Tls {
    fn default() -> Self {
        Self {
            mode: TlsMode::SelfSigned,
            cert: PathBuf::from("tls/cert.pem"),
            key: PathBuf::from("tls/key.pem"),
            extra_names: Vec::new(),
            reload_interval: HumanDuration(Duration::from_secs(30)),
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
    /// All client connections together (loopback is exempt).
    pub max_connections: u64,
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
            max_connections: 10_000,
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
            muc: Muc::default(),
            registration: Registration::default(),
            http: Http::default(),
            upload: Upload::default(),
            log: Log::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.domain = normalize_domain(&cfg.domain).with_context(|| format!("in {}", path.display()))?;
        if let Ok(hash) = std::env::var(PROVISION_TOKEN_ENV)
            && !hash.trim().is_empty()
        {
            cfg.registration.provision_token_sha256 = Some(hash.trim().to_owned());
        }
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
        let r = &self.registration;
        if r.pow_bits == 0 || r.pow_bits > 48 || r.pow_max_bits < r.pow_bits || r.pow_count == 0 || r.pow_count > 64 {
            bail!("registration: need 1 <= pow_bits <= pow_max_bits, pow_bits <= 48, 1 <= pow_count <= 64");
        }
        if r.web_pow_bits == 0 || r.web_pow_bits > 48 || r.web_pow_count == 0 || r.web_pow_count > 64 {
            bail!("registration: need 1 <= web_pow_bits <= 48 and 1 <= web_pow_count <= 64");
        }
        if r.ipv4_prefix > 32 || r.ipv6_prefix > 128 {
            bail!("registration: ipv4_prefix <= 32 and ipv6_prefix <= 128");
        }
        if r.min_password_length == 0 {
            bail!("registration.min_password_length must be at least 1");
        }
        if let Some(h) = &r.provision_token_sha256
            && (h.len() != 64 || !h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            bail!("registration.provision_token_sha256 must be 64 lowercase hex digits (a SHA-256)");
        }
        for p in &self.http.trusted_proxies {
            if parse_proxy(p).is_none() {
                bail!("http.trusted_proxies: {p:?} is not an IP address or CIDR range");
            }
        }
        self.http_addrs()?;
        if !matches!(self.muc.room_creation.as_str(), "local-users" | "admins") {
            bail!("muc.room_creation must be \"local-users\" or \"admins\"");
        }
        if self.muc.host.is_empty() || self.muc.host.contains('.') && normalize_domain(&self.muc.host).is_err() {
            bail!("muc.host must be a subdomain label such as \"conference\"");
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

    /// The registration mode in effect (§ decisions: open on localhost,
    /// invite-only on a LAN, PoW for public hubs).
    pub fn registration_mode(&self) -> RegistrationMode {
        self.registration.mode.unwrap_or(if self.is_lan() {
            RegistrationMode::Invite
        } else {
            RegistrationMode::Open
        })
    }

    pub fn http_addrs(&self) -> Result<Vec<SocketAddr>> {
        self.http.listen.iter().map(|a| parse_listen_addr(a)).collect()
    }

    /// The MUC service's JID, if enabled.
    pub fn upload_host(&self) -> Option<String> {
        self.upload
            .enabled
            .then(|| format!("{}.{}", self.upload.host, self.domain))
    }

    /// Where uploaded files are served: `http.public_url`, else the first web
    /// listener (`addr`: the bound one).
    pub fn upload_base_url(&self, addr: Option<SocketAddr>) -> Option<String> {
        if let Some(u) = &self.http.public_url {
            return Some(u.trim_end_matches('/').to_owned());
        }
        let addr = addr?;
        Some(if addr.ip().is_unspecified() {
            format!("http://{}:{}", self.domain, addr.port())
        } else {
            format!("http://{addr}")
        })
    }

    pub fn muc_host(&self) -> Option<String> {
        self.muc.enabled.then(|| format!("{}.{}", self.muc.host, self.domain))
    }

    /// Names for the self-signed certificate.
    pub fn certificate_names(&self) -> Vec<String> {
        let mut names = vec![self.domain.clone(), format!("{}.{}", self.muc.host, self.domain)];
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
        "w" => Duration::from_secs(n * 7 * 86_400),
        u => bail!("unknown duration unit {u:?} in {s:?}; use ms, s, m, h, d or w"),
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

    /// server/deploy-hub/hub.toml stays loadable and matches production's
    /// Prosody settings where they map.
    #[test]
    fn deployment_template_is_valid() {
        let cfg: Config = toml::from_str(include_str!("../../../server/deploy-hub/hub.toml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.registration_mode(), RegistrationMode::Pow);
        assert_eq!((cfg.registration.pow_bits, cfg.registration.pow_count), (25, 8));
        assert_eq!(
            cfg.archive.expire_after,
            Retention(Some(Duration::from_secs(7 * 86_400)))
        );
        assert_eq!(cfg.archive.max_page, 250);
        assert_eq!(cfg.muc_host().as_deref(), Some("conference.agentchat.isagentproof.com"));
        assert_eq!(cfg.tls.mode, TlsMode::Files);
        assert!(cfg.is_lan());
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
