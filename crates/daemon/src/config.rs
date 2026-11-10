//! Profile configuration (`config.toml`) and password storage.

use std::path::Path;

use agent_chat_proto::ipc::PasswordStorage;
use agent_chat_proto::model::AccountKind;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

pub const PASSWORD_ENV: &str = "AGENT_CHAT_PASSWORD";
const KEYRING_SERVICE: &str = "agent-chat";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AccountConfig>,
    #[serde(default)]
    pub settings: Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub jid: String,
    /// Resource requested at bind time; generated once and kept stable.
    pub resource: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default)]
    pub insecure_plaintext: bool,
    /// Only set when the user chose `PasswordStorage::ConfigFile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Accept every incoming presence subscription request.
    #[serde(default)]
    pub auto_accept_subscriptions: bool,
    /// Nickname used when joining rooms without `--nick`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_nick: Option<String>,
    /// How many archived messages to fetch per archive on first sync.
    #[serde(default = "default_backfill")]
    pub mam_backfill: u32,
    /// Local setup page on 127.0.0.1 (first free port from here; 0 = off).
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// When an agent reads a 1:1 message (`wait`, `inbox`, MCP), tell the
    /// sender it is typing until it replies or the timeout passes.
    #[serde(default = "yes")]
    pub agent_typing: bool,
    /// Seconds after which an agent's "typing" ends by itself.
    #[serde(default = "default_typing_timeout")]
    pub agent_typing_timeout_secs: u64,
    /// `person` or `agent`. Unset: a person's, until an agent consumer
    /// (`wait`, `listen`, `inbox`, MCP) uses the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_kind: Option<AccountKind>,
    /// Agents: seconds without any consumer before presence becomes away
    /// ("Not listening; messages will wait").
    #[serde(default = "default_away_after")]
    pub agent_away_after_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_accept_subscriptions: false,
            default_nick: None,
            mam_backfill: default_backfill(),
            http_port: default_http_port(),
            agent_typing: true,
            agent_typing_timeout_secs: default_typing_timeout(),
            account_kind: None,
            agent_away_after_secs: default_away_after(),
        }
    }
}

fn default_away_after() -> u64 {
    300
}

fn yes() -> bool {
    true
}

fn default_typing_timeout() -> u64 {
    120
}

fn default_backfill() -> u32 {
    50
}

fn default_http_port() -> u16 {
    7400
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write atomically with mode 0600 (it may contain a password).
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

impl AccountConfig {
    /// `insecure_plaintext` is refused unless we connect to the local machine.
    pub fn validate(&self) -> Result<()> {
        if self.insecure_plaintext {
            let host = self
                .host
                .clone()
                .unwrap_or_else(|| self.jid.rsplit('@').next().unwrap_or("").to_owned());
            if !is_loopback_host(&host) {
                bail!(
                    "insecure_plaintext is only allowed for localhost servers (got {host:?}); \
                     use TLS for remote servers"
                );
            }
        }
        Ok(())
    }
}

pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Find the password: `$AGENT_CHAT_PASSWORD`, then the config file, then the keychain.
pub fn resolve_password(acct: &AccountConfig) -> Result<String> {
    if let Ok(p) = std::env::var(PASSWORD_ENV)
        && !p.is_empty()
    {
        return Ok(p);
    }
    if let Some(p) = &acct.password {
        return Ok(p.clone());
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, &acct.jid).map_err(|e| anyhow!("keychain unavailable: {e}"))?;
    entry.get_password().map_err(|e| {
        anyhow!(
            "no password for {} in the keychain ({e}); run `agent-chat account add` \
             or set {PASSWORD_ENV}",
            acct.jid
        )
    })
}

/// Persist the password according to the chosen storage. Returns the value
/// to put in `AccountConfig::password`.
pub fn store_password(jid: &str, password: &str, storage: PasswordStorage) -> Result<Option<String>> {
    match storage {
        PasswordStorage::ConfigFile => Ok(Some(password.to_owned())),
        PasswordStorage::Keychain => {
            let entry = keyring::Entry::new(KEYRING_SERVICE, jid).map_err(|e| anyhow!("keychain unavailable: {e}"))?;
            entry
                .set_password(password)
                .map_err(|e| anyhow!("could not save the password to the keychain: {e}"))?;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(jid: &str, host: Option<&str>, insecure: bool) -> AccountConfig {
        AccountConfig {
            jid: jid.into(),
            resource: "r".into(),
            host: host.map(Into::into),
            port: None,
            insecure_plaintext: insecure,
            password: None,
        }
    }

    #[test]
    fn plaintext_only_for_loopback() {
        assert!(acct("a@localhost", None, true).validate().is_ok());
        assert!(acct("a@example.org", Some("127.0.0.1"), true).validate().is_ok());
        assert!(acct("a@example.org", Some("::1"), true).validate().is_ok());
        assert!(acct("a@example.org", None, true).validate().is_err());
        assert!(acct("a@example.org", Some("10.0.0.1"), true).validate().is_err());
        assert!(acct("a@example.org", None, false).validate().is_ok());
    }

    #[test]
    fn older_configs_get_the_new_defaults() {
        let cfg: Config = toml::from_str("[settings]\nmam_backfill = 10\n").unwrap();
        assert!(cfg.settings.agent_typing);
        assert_eq!(cfg.settings.agent_typing_timeout_secs, 120);
        assert_eq!(cfg.settings.account_kind, None);
        assert_eq!(cfg.settings.agent_away_after_secs, 300);
        let cfg: Config = toml::from_str("[settings]\nagent_typing = false\naccount_kind = \"agent\"\n").unwrap();
        assert!(!cfg.settings.agent_typing);
        assert_eq!(cfg.settings.account_kind, Some(AccountKind::Agent));
        let text = toml::to_string_pretty(&cfg).unwrap();
        assert!(text.contains("account_kind = \"agent\""), "{text}");
    }

    #[test]
    fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(Config::load(&path).unwrap().account.is_none());
        let cfg = Config {
            account: Some(acct("a@localhost", None, true)),
            settings: Settings::default(),
        };
        cfg.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(back.account.unwrap().jid, "a@localhost");
        assert_eq!(back.settings.mam_backfill, 50);
        assert!(back.settings.agent_typing);
        assert_eq!(back.settings.agent_typing_timeout_secs, 120);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
