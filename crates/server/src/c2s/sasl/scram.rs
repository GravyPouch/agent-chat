//! SCRAM server (RFC 5802, RFC 7677) with `tls-exporter` channel binding
//! (RFC 9266). Our own implementation, because the `sasl` crate's server
//! needs the SaltedPassword (password-equivalent) and compares proofs in
//! variable time (§3.2).
//!
//! The server keeps only salt, iteration count, StoredKey and ServerKey.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::store::ScramKeys;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
}

impl Hash {
    pub fn output_len(self) -> usize {
        match self {
            Hash::Sha1 => 20,
            Hash::Sha256 => 32,
        }
    }

    pub fn hmac(self, key: &[u8], data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => {
                let mut m = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
            Hash::Sha256 => {
                let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
                m.update(data);
                m.finalize().into_bytes().to_vec()
            }
        }
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => Sha1::digest(data).to_vec(),
            Hash::Sha256 => Sha256::digest(data).to_vec(),
        }
    }

    /// Hi() from RFC 5802: PBKDF2 with HMAC-H.
    pub fn salted_password(self, password: &[u8], salt: &[u8], iterations: u32) -> Zeroizing<Vec<u8>> {
        let mut out = Zeroizing::new(vec![0u8; self.output_len()]);
        match self {
            Hash::Sha1 => pbkdf2::pbkdf2::<Hmac<Sha1>>(password, salt, iterations, &mut out),
            Hash::Sha256 => pbkdf2::pbkdf2::<Hmac<Sha256>>(password, salt, iterations, &mut out),
        }
        .expect("PBKDF2 output length is valid");
        out
    }

    /// StoredKey and ServerKey for an already SASLprep'd password.
    pub fn keys(self, password: &str, salt: &[u8], iterations: u32) -> ScramKeys {
        let salted = self.salted_password(password.as_bytes(), salt, iterations);
        let client_key = Zeroizing::new(self.hmac(&salted, b"Client Key"));
        ScramKeys {
            stored_key: self.digest(&client_key),
            server_key: self.hmac(&salted, b"Server Key"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScramError {
    /// Syntax error in a client message.
    Malformed(&'static str),
    /// The authzid isn't the authenticating account.
    InvalidAuthzid,
    /// Wrong proof, wrong channel binding data, or wrong nonce.
    NotAuthorized,
}

/// The gs2 channel-binding flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CbFlag {
    /// `n`: the client doesn't support channel binding.
    None,
    /// `y`: the client supports it but thinks the server doesn't.
    ClientOnly,
    /// `p=<type>`: the client uses channel binding.
    Used(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFirst {
    pub cb: CbFlag,
    pub authzid: Option<String>,
    /// Decoded `n=` value (not yet normalized).
    pub username: String,
    pub nonce: String,
    /// The gs2 header, verbatim (`n,,`).
    gs2_header: String,
    /// client-first-message-bare, verbatim.
    bare: String,
}

pub fn parse_client_first(msg: &[u8]) -> Result<ClientFirst, ScramError> {
    let s = std::str::from_utf8(msg).map_err(|_| ScramError::Malformed("not UTF-8"))?;
    let (flag, rest) = s.split_once(',').ok_or(ScramError::Malformed("missing gs2 header"))?;
    let cb = match flag {
        "n" => CbFlag::None,
        "y" => CbFlag::ClientOnly,
        f => match f.strip_prefix("p=") {
            Some(name)
                if !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') =>
            {
                CbFlag::Used(name.to_owned())
            }
            _ => return Err(ScramError::Malformed("bad channel-binding flag")),
        },
    };
    let (authz, bare) = rest
        .split_once(',')
        .ok_or(ScramError::Malformed("missing gs2 header"))?;
    let authzid = match authz {
        "" => None,
        a => Some(decode_saslname(
            a.strip_prefix("a=").ok_or(ScramError::Malformed("bad authzid"))?,
        )?),
    };
    let gs2_header = s[..flag.len() + 1 + authz.len() + 1].to_owned();

    let mut attrs = bare.split(',');
    let first = attrs.next().unwrap_or("");
    if first.starts_with("m=") {
        return Err(ScramError::Malformed("unsupported mandatory extension"));
    }
    let username = decode_saslname(
        first
            .strip_prefix("n=")
            .ok_or(ScramError::Malformed("missing username"))?,
    )?;
    if username.is_empty() {
        return Err(ScramError::Malformed("empty username"));
    }
    let nonce = attrs
        .next()
        .and_then(|a| a.strip_prefix("r="))
        .ok_or(ScramError::Malformed("missing nonce"))?;
    if !valid_nonce(nonce) {
        return Err(ScramError::Malformed("bad nonce"));
    }
    Ok(ClientFirst {
        cb,
        authzid,
        username,
        nonce: nonce.to_owned(),
        gs2_header,
        bare: bare.to_owned(),
    })
}

fn valid_nonce(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b',')
}

/// `saslname`: `=2C` is `,` and `=3D` is `=`; any other `=` is an error.
fn decode_saslname(s: &str) -> Result<String, ScramError> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('=') {
        out.push_str(&rest[..i]);
        match rest.get(i..i + 3) {
            Some("=2C") => out.push(','),
            Some("=3D") => out.push('='),
            _ => return Err(ScramError::Malformed("bad escape in name")),
        }
        rest = &rest[i + 3..];
    }
    out.push_str(rest);
    Ok(out)
}

/// One SCRAM exchange after the server-first message was produced.
pub struct Exchange {
    hash: Hash,
    first: ClientFirst,
    server_first: String,
    combined_nonce: String,
    keys: ScramKeys,
    /// Channel-binding data appended to the gs2 header in `c=`.
    cb_data: Vec<u8>,
}

impl Exchange {
    /// Build the server-first message. For unknown users the caller passes
    /// a fake salt and random keys, so the exchange looks the same.
    pub fn start(
        hash: Hash,
        first: ClientFirst,
        salt: &[u8],
        iterations: u32,
        keys: ScramKeys,
        server_nonce: &str,
        cb_data: Vec<u8>,
    ) -> (Self, String) {
        let combined_nonce = format!("{}{}", first.nonce, server_nonce);
        let server_first = format!("r={combined_nonce},s={},i={iterations}", B64.encode(salt));
        (
            Self {
                hash,
                first,
                server_first: server_first.clone(),
                combined_nonce,
                keys,
                cb_data,
            },
            server_first,
        )
    }

    /// Verify the client-final message; on success return server-final (`v=…`).
    pub fn finish(&self, msg: &[u8]) -> Result<String, ScramError> {
        let s = std::str::from_utf8(msg).map_err(|_| ScramError::Malformed("not UTF-8"))?;
        let (without_proof, proof) = s.rsplit_once(",p=").ok_or(ScramError::Malformed("missing proof"))?;
        if proof.contains(',') {
            return Err(ScramError::Malformed("proof must be last"));
        }
        let mut attrs = without_proof.split(',');
        let c = attrs
            .next()
            .and_then(|a| a.strip_prefix("c="))
            .ok_or(ScramError::Malformed("missing channel binding"))?;
        let r = attrs
            .next()
            .and_then(|a| a.strip_prefix("r="))
            .ok_or(ScramError::Malformed("missing nonce"))?;
        let c = B64
            .decode(c)
            .map_err(|_| ScramError::Malformed("bad channel-binding encoding"))?;
        let proof = B64
            .decode(proof)
            .map_err(|_| ScramError::Malformed("bad proof encoding"))?;

        let mut expected_c = self.first.gs2_header.as_bytes().to_vec();
        expected_c.extend_from_slice(&self.cb_data);
        let cb_ok = c.ct_eq(&expected_c);
        let nonce_ok = r.as_bytes().ct_eq(self.combined_nonce.as_bytes());

        let auth_message = format!("{},{},{}", self.first.bare, self.server_first, without_proof);
        let client_signature = self.hash.hmac(&self.keys.stored_key, auth_message.as_bytes());
        let proof_ok = if proof.len() == client_signature.len() {
            let client_key = Zeroizing::new(
                proof
                    .iter()
                    .zip(&client_signature)
                    .map(|(a, b)| a ^ b)
                    .collect::<Vec<u8>>(),
            );
            self.hash.digest(&client_key).ct_eq(&self.keys.stored_key)
        } else {
            subtle::Choice::from(0)
        };
        if bool::from(cb_ok & nonce_ok & proof_ok) {
            let server_signature = self.hash.hmac(&self.keys.server_key, auth_message.as_bytes());
            Ok(format!("v={}", B64.encode(server_signature)))
        } else {
            Err(ScramError::NotAuthorized)
        }
    }

    pub fn client_first(&self) -> &ClientFirst {
        &self.first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kat(
        hash: Hash,
        client_first: &str,
        salt_b64: &str,
        server_nonce: &str,
        client_final: &str,
        server_final: &str,
        server_first: &str,
    ) {
        let salt = B64.decode(salt_b64).unwrap();
        let keys = hash.keys("pencil", &salt, 4096);
        let first = parse_client_first(client_first.as_bytes()).unwrap();
        assert_eq!(first.username, "user");
        let (ex, sf) = Exchange::start(hash, first, &salt, 4096, keys, server_nonce, vec![]);
        assert_eq!(sf, server_first);
        assert_eq!(ex.finish(client_final.as_bytes()).unwrap(), server_final);
    }

    #[test]
    fn rfc5802_sha1_example() {
        kat(
            Hash::Sha1,
            "n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL",
            "QSXCR+Q6sek8bf92",
            "3rfcNHYJY1ZVvWVs7j",
            "c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts=",
            "v=rmF9pqV8S7suAoZWja4dJRkFsKQ=",
            "r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,s=QSXCR+Q6sek8bf92,i=4096",
        );
    }

    #[test]
    fn rfc7677_sha256_example() {
        kat(
            Hash::Sha256,
            "n,,n=user,r=rOprNGfwEbeRWgbNEkqO",
            "W22ZaJ0SNY7soEsUEjb6gQ==",
            "%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0",
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=",
            "v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=",
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096",
        );
    }

    #[test]
    fn wrong_proof_and_tampering_fail() {
        let salt = B64.decode("QSXCR+Q6sek8bf92").unwrap();
        let first = parse_client_first(b"n,,n=user,r=fyko+d2lbbFgONRv9qkxdawL").unwrap();
        let (ex, _) = Exchange::start(
            Hash::Sha1,
            first,
            &salt,
            4096,
            Hash::Sha1.keys("pencil", &salt, 4096),
            "3rfcNHYJY1ZVvWVs7j",
            vec![],
        );
        // Wrong proof.
        assert_eq!(
            ex.finish(b"c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=w0X8v3Bz2T0CJGbJQyF0X+HI4Ts="),
            Err(ScramError::NotAuthorized)
        );
        // Changed nonce (the proof covers it, so it fails too).
        assert_eq!(
            ex.finish(b"c=biws,r=fyko+d2lbbFgONRv9qkxdawLXXXX,p=v0X8v3Bz2T0CJGbJQyF0X+HI4Ts="),
            Err(ScramError::NotAuthorized)
        );
        // Short proof.
        assert_eq!(
            ex.finish(b"c=biws,r=fyko+d2lbbFgONRv9qkxdawL3rfcNHYJY1ZVvWVs7j,p=AAAA"),
            Err(ScramError::NotAuthorized)
        );
        assert!(matches!(ex.finish(b"c=biws,p=AAAA"), Err(ScramError::Malformed(_))));
    }

    /// Round trip against the `sasl` crate's client, including -PLUS with a
    /// fixed tls-exporter value.
    #[test]
    fn interoperates_with_the_sasl_client_including_plus() {
        use sasl::client::Mechanism;
        use sasl::client::mechanisms::Scram;
        use sasl::common::ChannelBinding;
        use sasl::common::scram::{Sha1 as CSha1, Sha256 as CSha256};

        let exporter = vec![7u8; 32];
        for plus in [false, true] {
            let cb = if plus {
                ChannelBinding::TlsExporter(exporter.clone())
            } else {
                ChannelBinding::None
            };
            let mut client = Scram::<CSha256>::new("alice", "correct horse battery", cb).unwrap();
            assert_eq!(client.name(), if plus { "SCRAM-SHA-256-PLUS" } else { "SCRAM-SHA-256" });
            let salt = [3u8; 16];
            let first = parse_client_first(&client.initial()).unwrap();
            assert_eq!(
                first.cb,
                if plus {
                    CbFlag::Used("tls-exporter".into())
                } else {
                    CbFlag::None
                }
            );
            let keys = Hash::Sha256.keys("correct horse battery", &salt, 4096);
            let cb_data = if plus { exporter.clone() } else { vec![] };
            let (ex, sf) = Exchange::start(Hash::Sha256, first, &salt, 4096, keys, "servernonce", cb_data);
            let fin = client.response(sf.as_bytes()).unwrap();
            let v = ex.finish(&fin).unwrap();
            client.success(v.as_bytes()).unwrap();
        }

        // A different exporter value on the server (a MITM terminating TLS) fails.
        let mut client = Scram::<CSha1>::new("alice", "pw", ChannelBinding::TlsExporter(exporter)).unwrap();
        let first = parse_client_first(&client.initial()).unwrap();
        let salt = [1u8; 16];
        let (ex, sf) = Exchange::start(
            Hash::Sha1,
            first,
            &salt,
            4096,
            Hash::Sha1.keys("pw", &salt, 4096),
            "n",
            vec![9u8; 32],
        );
        let fin = client.response(sf.as_bytes()).unwrap();
        assert_eq!(ex.finish(&fin), Err(ScramError::NotAuthorized));
    }

    #[test]
    fn parses_gs2_variants() {
        let f = parse_client_first(b"y,a=bob,n=al=2Cice=3D,r=abc").unwrap();
        assert_eq!(f.cb, CbFlag::ClientOnly);
        assert_eq!(f.authzid.as_deref(), Some("bob"));
        assert_eq!(f.username, "al,ice=");
        for bad in [
            &b"x,,n=a,r=b"[..],
            b"n,,n=,r=b",
            b"n,,n=a",
            b"n,,m=ext,n=a,r=b",
            b"n,,n=a=2X,r=b",
            b"p=,,n=a,r=b",
            b"n,,n=a,r=has space",
            b"\xff,,n=a,r=b",
        ] {
            assert!(parse_client_first(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }
}
