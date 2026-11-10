//! Identity key fingerprints.
//!
//! XEP-0384 §8: the fingerprint SHOULD be the identity key's Curve25519 form
//! (32 bytes, RFC 7748 encoding), shown as lowercase hex in 8 groups of 8.
//! Legacy bundles carry that form with a 0x05 prefix; OMEMO:2 bundles carry
//! the Ed25519 form, which we convert.

use curve25519_dalek::edwards::CompressedEdwardsY;

use crate::Version;

/// Curve25519 bytes of an identity key in a version's wire encoding.
pub fn curve25519_bytes(version: Version, identity_key: &[u8]) -> Option<[u8; 32]> {
    match version {
        Version::Legacy => match identity_key.len() {
            33 if identity_key[0] == 0x05 => identity_key[1..].try_into().ok(),
            32 => identity_key.try_into().ok(),
            _ => None,
        },
        Version::Omemo2 => {
            let ed: [u8; 32] = identity_key.try_into().ok()?;
            Some(CompressedEdwardsY(ed).decompress()?.to_montgomery().to_bytes())
        }
    }
}

/// Canonical fingerprint string (lowercase hex, no separators).
pub fn fingerprint(version: Version, identity_key: &[u8]) -> Option<String> {
    let bytes = curve25519_bytes(version, identity_key)?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Fingerprint split into 8 groups of 8 for display.
pub fn display(fingerprint: &str) -> String {
    fingerprint
        .as_bytes()
        .chunks(8)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalise user input (spaces, colons, case) for comparison.
pub fn normalize(input: &str) -> String {
    input
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_strips_prefix_and_display_groups() {
        let mut key = vec![0x05];
        key.extend(0u8..32);
        let fp = fingerprint(Version::Legacy, &key).unwrap();
        assert_eq!(fp.len(), 64);
        assert!(fp.starts_with("00010203"));
        assert_eq!(display(&fp).split(' ').count(), 8);
        assert_eq!(normalize("0001 0203:ABCD"), "00010203abcd");
    }

    #[test]
    fn ed25519_basepoint_maps_to_curve25519_basepoint() {
        // Ed25519 base point y = 4/5 ↔ Curve25519 u = 9.
        let ed = curve25519_dalek::constants::ED25519_BASEPOINT_COMPRESSED.to_bytes();
        let mont = curve25519_bytes(Version::Omemo2, &ed).unwrap();
        let mut nine = [0u8; 32];
        nine[0] = 9;
        assert_eq!(mont, nine);
    }
}
