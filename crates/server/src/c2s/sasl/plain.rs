//! SASL PLAIN (RFC 4616). Offered only under TLS. Verified by deriving the
//! SCRAM-SHA-256 StoredKey from the supplied password (§6.3).

use zeroize::Zeroizing;

/// `[authzid] NUL authcid NUL passwd`
pub struct PlainMessage {
    pub authzid: Option<String>,
    pub username: String,
    pub password: Zeroizing<String>,
}

pub fn parse(msg: &[u8]) -> Option<PlainMessage> {
    let mut parts = msg.split(|b| *b == 0);
    let authzid = parts.next()?;
    let authcid = parts.next()?;
    let passwd = parts.next()?;
    if parts.next().is_some() || authcid.is_empty() || passwd.is_empty() {
        return None;
    }
    Some(PlainMessage {
        authzid: (!authzid.is_empty())
            .then(|| String::from_utf8(authzid.to_vec()).ok())
            .flatten(),
        username: String::from_utf8(authcid.to_vec()).ok()?,
        password: Zeroizing::new(String::from_utf8(passwd.to_vec()).ok()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let m = parse(b"\0user\0pencil").unwrap();
        assert_eq!(
            (m.authzid, m.username.as_str(), m.password.as_str()),
            (None, "user", "pencil")
        );
        let m = parse(b"admin@x\0user\0pw").unwrap();
        assert_eq!(m.authzid.as_deref(), Some("admin@x"));
        assert!(parse(b"user\0pencil").is_none());
        assert!(parse(b"\0\0pencil").is_none());
        assert!(parse(b"\0user\0").is_none());
        assert!(parse(b"\0user\0pw\0x").is_none());
    }
}
