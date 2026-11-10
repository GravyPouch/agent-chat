//! Account creation and password changes: the one code path that the admin
//! socket uses today and that XEP-0077 registration (M6) and a future HTTP
//! registration endpoint will call too.

use std::sync::Arc;

use thiserror::Error;
use zeroize::Zeroizing;

use crate::c2s::sasl::scram::Hash;
use crate::store::{Account, CreatedVia, Credentials, NewAccount, Role, Store};
use crate::util::random_bytes;

/// Admin-chosen passwords must be at least this long (§6.3).
pub const MIN_PASSWORD_LEN: usize = 12;
/// Length of generated passwords.
pub const GENERATED_PASSWORD_LEN: usize = 24;

#[derive(Debug, Error)]
pub enum AccountError {
    #[error("invalid username {0:?}: {1}")]
    InvalidUsername(String, String),
    #[error("the password must be at least {MIN_PASSWORD_LEN} characters")]
    WeakPassword,
    #[error("the password contains characters SASLprep rejects")]
    InvalidPassword,
    #[error("account {0} already exists")]
    Exists(String),
    #[error("no such account: {0}")]
    NotFound(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// What a caller asks for.
pub struct CreateRequest {
    pub username: String,
    /// `None`: generate one and return it.
    pub password: Option<String>,
    pub role: Role,
    pub created_via: CreatedVia,
}

pub struct Created {
    pub account: Account,
    /// Set when the password was generated (shown once, never stored).
    pub generated_password: Option<String>,
}

/// Normalize a username to the JID localpart (nodeprep, as the jid crate does).
pub fn normalize_username(name: &str) -> Result<String, AccountError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(AccountError::InvalidUsername(name.into(), "it is empty".into()));
    }
    if trimmed.contains('@') || trimmed.contains('/') {
        return Err(AccountError::InvalidUsername(
            name.into(),
            "give the name without @domain".into(),
        ));
    }
    xmpp_parsers::jid::NodePart::new(trimmed)
        .map(|n| n.into_owned().to_string())
        .map_err(|e| AccountError::InvalidUsername(name.into(), e.to_string()))
}

/// A random password from an unambiguous alphabet.
pub fn generate_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let bytes = random_bytes::<64>();
    let mut out = String::with_capacity(GENERATED_PASSWORD_LEN);
    for b in bytes {
        // Rejection sampling keeps the distribution uniform.
        let limit = 256 - (256 % ALPHABET.len());
        if (b as usize) < limit {
            out.push(ALPHABET[b as usize % ALPHABET.len()] as char);
            if out.len() == GENERATED_PASSWORD_LEN {
                return out;
            }
        }
    }
    // 64 bytes nearly always suffice; draw again otherwise.
    let need = GENERATED_PASSWORD_LEN - out.len();
    out + &generate_password()[..need]
}

/// SASLprep, then derive the stored SCRAM keys for both hashes. CPU-heavy
/// (PBKDF2), so it runs on the blocking pool.
pub async fn derive_credentials(password: &str, iterations: u32) -> Result<Credentials, AccountError> {
    let prepped = Zeroizing::new(
        stringprep::saslprep(password)
            .map_err(|_| AccountError::InvalidPassword)?
            .into_owned(),
    );
    if prepped.is_empty() {
        return Err(AccountError::InvalidPassword);
    }
    let salt = random_bytes::<16>().to_vec();
    tokio::task::spawn_blocking(move || Credentials {
        sha1: Hash::Sha1.keys(&prepped, &salt, iterations),
        sha256: Hash::Sha256.keys(&prepped, &salt, iterations),
        salt,
        iterations,
    })
    .await
    .map_err(|e| AccountError::Other(anyhow::anyhow!("key derivation failed: {e}")))
}

fn check_password(password: Option<String>) -> Result<(String, bool), AccountError> {
    match password {
        Some(p) if p.chars().count() < MIN_PASSWORD_LEN => Err(AccountError::WeakPassword),
        Some(p) => Ok((p, false)),
        None => Ok((generate_password(), true)),
    }
}

/// Create an account on `domain`.
pub async fn create(
    store: &Arc<dyn Store>,
    domain: &str,
    iterations: u32,
    req: CreateRequest,
) -> Result<Created, AccountError> {
    let localpart = normalize_username(&req.username)?;
    let (password, generated) = check_password(req.password)?;
    let password = Zeroizing::new(password);
    let credentials = derive_credentials(&password, iterations).await?;
    let account = store
        .create_account(NewAccount {
            domain: domain.to_owned(),
            localpart: localpart.clone(),
            role: req.role,
            created_via: req.created_via,
            credentials,
        })
        .await?
        .ok_or_else(|| AccountError::Exists(format!("{localpart}@{domain}")))?;
    Ok(Created {
        account,
        generated_password: generated.then(|| password.to_string()),
    })
}

/// Change a password. Returns the generated password if none was given.
pub async fn set_password(
    store: &Arc<dyn Store>,
    domain: &str,
    iterations: u32,
    username: &str,
    password: Option<String>,
) -> Result<Option<String>, AccountError> {
    let localpart = normalize_username(username)?;
    let (password, generated) = check_password(password)?;
    let password = Zeroizing::new(password);
    let credentials = derive_credentials(&password, iterations).await?;
    if !store.set_credentials(domain, &localpart, credentials).await? {
        return Err(AccountError::NotFound(format!("{localpart}@{domain}")));
    }
    Ok(generated.then(|| password.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    #[test]
    fn usernames() {
        assert_eq!(normalize_username("Alice").unwrap(), "alice");
        assert_eq!(normalize_username(" bob ").unwrap(), "bob");
        assert!(normalize_username("").is_err());
        assert!(normalize_username("a@b").is_err());
        assert!(normalize_username("a b").is_err());
    }

    #[test]
    fn generated_passwords() {
        let a = generate_password();
        assert_eq!(a.len(), GENERATED_PASSWORD_LEN);
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(a, generate_password());
    }

    #[tokio::test]
    async fn create_and_change_password() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&dir.path().join("hub.db")).unwrap());
        store.ensure_host("hub.local").await.unwrap();
        let req = |name: &str, pw: Option<&str>| CreateRequest {
            username: name.into(),
            password: pw.map(Into::into),
            role: Role::User,
            created_via: CreatedVia::Admin,
        };
        let c = create(&store, "hub.local", 4096, req("Alice", None)).await.unwrap();
        assert_eq!(c.account.jid(), "alice@hub.local");
        let pw = c.generated_password.unwrap();
        // The stored keys match the password, and nothing else is stored.
        let salt = &c.account.credentials.salt;
        assert_eq!(c.account.credentials.sha256, Hash::Sha256.keys(&pw, salt, 4096));
        assert!(matches!(
            create(&store, "hub.local", 4096, req("alice", Some("another password"))).await,
            Err(AccountError::Exists(_))
        ));
        assert!(matches!(
            create(&store, "hub.local", 4096, req("bob", Some("short"))).await,
            Err(AccountError::WeakPassword)
        ));
        assert_eq!(
            set_password(&store, "hub.local", 4096, "alice", Some("a new long password".into()))
                .await
                .unwrap(),
            None
        );
        let a = store.account("hub.local", "alice").await.unwrap().unwrap();
        assert_eq!(
            a.credentials.sha1,
            Hash::Sha1.keys("a new long password", &a.credentials.salt, 4096)
        );
        assert!(matches!(
            set_password(&store, "hub.local", 4096, "nobody", None).await,
            Err(AccountError::NotFound(_))
        ));
    }
}
