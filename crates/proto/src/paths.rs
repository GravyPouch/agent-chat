//! Per-profile file locations.
//!
//! Layout: `<base>/profiles/<name>/{config.toml, agent-chat.db, daemon.log, daemon.lock, agent-chat.sock}`
//! where `<base>` is `$AGENT_CHAT_HOME` or the platform data directory
//! (`~/Library/Application Support/agent-chat` on macOS).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const DEFAULT_PROFILE: &str = "default";
pub const HOME_ENV: &str = "AGENT_CHAT_HOME";
pub const PROFILE_ENV: &str = "AGENT_CHAT_PROFILE";

/// Unix socket paths are limited to ~104 bytes on macOS.
const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub dir: PathBuf,
}

impl Profile {
    /// Resolve a profile by name (`None` = `$AGENT_CHAT_PROFILE` or `default`).
    pub fn resolve(name: Option<&str>) -> Result<Self> {
        let name = match name {
            Some(n) => n.to_owned(),
            None => std::env::var(PROFILE_ENV).unwrap_or_else(|_| DEFAULT_PROFILE.to_owned()),
        };
        validate_name(&name)?;
        let base = base_dir()?;
        Ok(Self {
            dir: base.join("profiles").join(&name),
            name,
        })
    }

    /// Create the profile directory (mode 0700).
    pub fn ensure_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.join("agent-chat.db")
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("daemon.log")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("daemon.lock")
    }

    pub fn socket_path(&self) -> PathBuf {
        let p = self.dir.join("agent-chat.sock");
        if p.as_os_str().len() <= MAX_SOCKET_PATH {
            return p;
        }
        // Fall back to a short per-user temp path derived from the profile dir.
        std::env::temp_dir().join(format!("agent-chat-{:016x}.sock", fnv1a(&self.dir)))
    }
}

fn base_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os(HOME_ENV) {
        return Ok(PathBuf::from(home));
    }
    let dirs = directories::ProjectDirs::from("", "", "agent-chat")
        .context("could not determine a home directory; set AGENT_CHAT_HOME")?;
    Ok(dirs.data_dir().to_path_buf())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("invalid profile name {name:?}: use letters, digits, '-' or '_'");
    }
    Ok(())
}

fn fnv1a(path: &Path) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in path.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_names() {
        assert!(validate_name("ok-name_1").is_ok());
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("").is_err());
        assert!(validate_name("a b").is_err());
    }

    #[test]
    fn long_socket_paths_fall_back() {
        let p = Profile {
            name: "x".into(),
            dir: PathBuf::from("/".to_owned() + &"d".repeat(200)),
        };
        assert!(p.socket_path().as_os_str().len() <= MAX_SOCKET_PATH + 40);
        assert!(p.socket_path().starts_with(std::env::temp_dir()));
    }
}
