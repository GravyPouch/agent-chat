//! TLS material: the self-signed certificate (rcgen), the rustls acceptor,
//! the SPKI pin clients use, and the `tls-exporter` channel binding (§6.2).

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::rustls::server::ServerConnection;
use tokio_rustls::rustls::{self, ProtocolVersion, ServerConfig};

use crate::paths::write_private;

/// RFC 9266: label, empty context, 32 bytes.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-Channel-Binding";

pub struct TlsMaterial {
    pub acceptor: TlsAcceptor,
    /// `sha256/<base64 of the SPKI SHA-256>`, for `hub.json` and invites.
    pub pin: String,
}

/// Generate a self-signed ECDSA P-256 certificate for `names` unless the
/// files already exist. Returns whether new files were written.
pub fn ensure_self_signed(cert_path: &Path, key_path: &Path, names: &[String]) -> Result<bool> {
    if cert_path.exists() && key_path.exists() {
        return Ok(false);
    }
    let key = rcgen::KeyPair::generate().context("generating the TLS key")?;
    let mut params = rcgen::CertificateParams::new(names.to_vec()).context("certificate names")?;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, names.first().cloned().unwrap_or_default());
    dn.push(rcgen::DnType::OrganizationName, "agent-chat hub");
    params.distinguished_name = dn;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + time::Duration::days(3650);
    let cert = params.self_signed(&key).context("signing the certificate")?;
    if let Some(dir) = key_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_private(key_path, key.serialize_pem().as_bytes())?;
    write_private(cert_path, cert.pem().as_bytes())?;
    Ok(true)
}

/// Load the certificate chain and key and build the STARTTLS acceptor.
pub fn load(cert_path: &Path, key_path: &Path) -> Result<TlsMaterial> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("reading {}", cert_path.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing {}", cert_path.display()))?;
    let Some(leaf) = certs.first() else {
        bail!("{} contains no certificate", cert_path.display());
    };
    let pin = spki_pin(leaf.as_ref()).context("computing the certificate pin")?;
    let key = PrivateKeyDer::from_pem_file(key_path).with_context(|| format!("reading {}", key_path.display()))?;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the TLS key does not match the certificate")?;
    Ok(TlsMaterial {
        acceptor: TlsAcceptor::from(Arc::new(config)),
        pin,
    })
}

/// The `tls-exporter` channel binding (RFC 9266). Only TLS 1.3: rustls has
/// no `tls-unique`, so `-PLUS` is offered on TLS 1.3 only.
pub fn channel_binding(conn: &ServerConnection) -> Option<Vec<u8>> {
    if conn.protocol_version() != Some(ProtocolVersion::TLSv1_3) {
        return None;
    }
    conn.export_keying_material(vec![0u8; 32], EXPORTER_LABEL, None).ok()
}

/// `sha256/` + base64(SHA-256(SubjectPublicKeyInfo)) of a DER certificate.
pub fn spki_pin(cert_der: &[u8]) -> Result<String> {
    let spki = subject_public_key_info(cert_der)?;
    Ok(format!("sha256/{}", B64.encode(Sha256::digest(spki))))
}

/// Extract the DER SubjectPublicKeyInfo from an X.509 certificate with a
/// minimal DER walk (Certificate → tbsCertificate → 7th field, or 6th
/// without the optional `[0] version`).
pub fn subject_public_key_info(cert_der: &[u8]) -> Result<&[u8]> {
    let (cert, _) = der_tlv(cert_der, 0x30).context("certificate is not a SEQUENCE")?;
    let (tbs, _) = der_tlv(cert.content, 0x30).context("tbsCertificate is not a SEQUENCE")?;
    let mut rest = tbs.content;
    // Optional explicit version [0].
    if rest.first() == Some(&0xa0) {
        let (_, r) = der_tlv(rest, 0xa0)?;
        rest = r;
    }
    // serialNumber, signature, issuer, validity, subject.
    for (tag, what) in [(0x02, "serial"), (0x30, "signature"), (0x30, "issuer"), (0x30, "validity"), (0x30, "subject")]
    {
        let (_, r) = der_tlv(rest, tag).with_context(|| format!("malformed {what}"))?;
        rest = r;
    }
    let (spki, _) = der_tlv(rest, 0x30).context("malformed subjectPublicKeyInfo")?;
    Ok(spki.whole)
}

struct Tlv<'a> {
    whole: &'a [u8],
    content: &'a [u8],
}

fn der_tlv(input: &[u8], tag: u8) -> Result<(Tlv<'_>, &[u8])> {
    let (&t, rest) = input.split_first().ok_or_else(|| anyhow!("truncated DER"))?;
    if t != tag {
        bail!("unexpected DER tag {t:#04x}, wanted {tag:#04x}");
    }
    let (&l, rest) = rest.split_first().ok_or_else(|| anyhow!("truncated DER"))?;
    let (len, rest) = if l < 0x80 {
        (l as usize, rest)
    } else {
        let n = (l & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            bail!("bad DER length");
        }
        let len = rest[..n].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        (len, &rest[n..])
    };
    if rest.len() < len {
        bail!("truncated DER");
    }
    let header = input.len() - rest.len();
    Ok((
        Tlv {
            whole: &input[..header + len],
            content: &rest[..len],
        },
        &rest[len..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::PublicKeyData;

    #[test]
    fn generates_loads_and_pins() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = (dir.path().join("tls/cert.pem"), dir.path().join("tls/key.pem"));
        let names = vec!["hub.local".to_owned(), "conference.hub.local".to_owned(), "192.168.1.20".to_owned()];
        assert!(ensure_self_signed(&cert, &key, &names).unwrap());
        assert!(!ensure_self_signed(&cert, &key, &names).unwrap(), "existing files are kept");
        let material = load(&cert, &key).unwrap();

        // The pin from the certificate equals the pin from the key's SPKI.
        let kp = rcgen::KeyPair::from_pem(&std::fs::read_to_string(&key).unwrap()).unwrap();
        let expected = format!("sha256/{}", B64.encode(Sha256::digest(kp.subject_public_key_info())));
        assert_eq!(material.pin, expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(spki_pin(b"\x30\x03\x02\x01").is_err());
        assert!(spki_pin(b"").is_err());
    }
}
