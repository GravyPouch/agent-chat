//! Hub file locations (§5.2).
//!
//! Layout: `<base>/hub/{hub.toml, hub.db, tls/, hub.sock, hub.lock, hub.json, hub.log}`
//! where `<base>` is `$AGENT_CHAT_HOME` or the platform data directory, the
//! same base the client profiles use (`agent-chat-proto::paths`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const HOME_ENV: &str = "AGENT_CHAT_HOME";
pub const CONFIG_ENV: &str = "AGENT_CHAT_HUB_CONFIG";

/// Unix socket paths are limited to ~104 bytes on macOS.
const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubPaths {
    pub dir: PathBuf,
    config: PathBuf,
}

impl HubPaths {
    /// `<base>/hub`, with the config path overridable by `$AGENT_CHAT_HUB_CONFIG`.
    pub fn resolve() -> Result<Self> {
        let mut paths = Self::at(base_dir()?.join("hub"));
        if let Some(cfg) = std::env::var_os(CONFIG_ENV) {
            paths.config = PathBuf::from(cfg);
        }
        Ok(paths)
    }

    /// Paths for a hub living in `dir`.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            config: dir.join("hub.toml"),
            dir,
        }
    }

    /// Create the hub directory (mode 0700).
    pub fn ensure_dir(&self) -> Result<()> {
        create_private_dir(&self.dir)?;
        create_private_dir(&self.tls_dir())
    }

    pub fn config(&self) -> &Path {
        &self.config
    }

    pub fn db(&self) -> PathBuf {
        self.dir.join("hub.db")
    }

    pub fn tls_dir(&self) -> PathBuf {
        self.dir.join("tls")
    }

    pub fn lock(&self) -> PathBuf {
        self.dir.join("hub.lock")
    }

    /// Runtime information for clients (`hub.json`).
    pub fn runtime(&self) -> PathBuf {
        self.dir.join("hub.json")
    }

    pub fn log(&self) -> PathBuf {
        self.dir.join("hub.log")
    }

    /// Resolve a path from the config relative to the hub directory.
    pub fn resolve_file(&self, p: &Path) -> PathBuf {
        if p.is_absolute() { p.to_owned() } else { self.dir.join(p) }
    }

    /// The admin socket.
    pub fn socket(&self) -> PathBuf {
        let p = self.dir.join("hub.sock");
        if p.as_os_str().len() <= MAX_SOCKET_PATH {
            return p;
        }
        // Fall back to a short per-user temp path derived from the hub dir.
        std::env::temp_dir().join(format!("agent-chat-hub-{:016x}.sock", fnv1a(&self.dir)))
    }
}

pub fn base_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os(HOME_ENV) {
        return Ok(PathBuf::from(home));
    }
    let dirs = directories::ProjectDirs::from("", "", "agent-chat")
        .context("could not determine a home directory; set AGENT_CHAT_HOME")?;
    Ok(dirs.data_dir().to_path_buf())
}

fn create_private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write a file atomically with mode 0600.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(contents)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
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
    fn layout() {
        let p = HubPaths::at("/x/hub");
        assert_eq!(p.config(), Path::new("/x/hub/hub.toml"));
        assert_eq!(p.db(), PathBuf::from("/x/hub/hub.db"));
        assert_eq!(p.resolve_file(Path::new("tls/cert.pem")), PathBuf::from("/x/hub/tls/cert.pem"));
        assert_eq!(p.resolve_file(Path::new("/etc/c.pem")), PathBuf::from("/etc/c.pem"));
    }

    #[test]
    fn long_socket_paths_fall_back() {
        let p = HubPaths::at("/".to_owned() + &"d".repeat(200));
        assert!(p.socket().starts_with(std::env::temp_dir()));
    }

    #[test]
    fn private_write() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.json");
        write_private(&f, b"{}").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"{}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
