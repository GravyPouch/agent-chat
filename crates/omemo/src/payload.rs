//! Payload encryption: the part of an OMEMO message that is encrypted once
//! and whose key material is then encrypted per device with the ratchet.

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes128Gcm, AesGcm, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::Error;

fn random<const N: usize>() -> Zeroizing<[u8; N]> {
    let mut b = Zeroizing::new([0u8; N]);
    getrandom::fill(b.as_mut()).expect("system RNG unavailable");
    b
}

/// Result of sealing a payload.
#[derive(Debug, Clone)]
pub struct Sealed {
    /// Encrypt this per recipient device with the ratchet.
    pub key_material: Zeroizing<Vec<u8>>,
    pub ciphertext: Vec<u8>,
    /// Legacy only: the GCM IV, sent in `<iv>`.
    pub iv: Option<Vec<u8>>,
}

/// XEP-0384 0.9 §4.4: OMEMO:2 payload encryption.
pub mod omemo2 {
    use super::*;

    /// Ratchet plaintext of an "empty OMEMO message" (§5.5.3).
    pub const EMPTY_KEY_MATERIAL: [u8; 32] = [0u8; 32];
    pub const KEY_MATERIAL_LEN: usize = 48;

    struct Keys {
        enc: Zeroizing<[u8; 32]>,
        auth: Zeroizing<[u8; 32]>,
        iv: [u8; 16],
    }

    fn derive(key: &[u8]) -> Keys {
        let hk = Hkdf::<Sha256>::new(Some(&[0u8; 32]), key);
        let mut okm = Zeroizing::new([0u8; 80]);
        hk.expand(b"OMEMO Payload", okm.as_mut())
            .expect("80 bytes is a valid HKDF-SHA-256 length");
        let mut keys = Keys {
            enc: Zeroizing::new([0; 32]),
            auth: Zeroizing::new([0; 32]),
            iv: [0; 16],
        };
        keys.enc.copy_from_slice(&okm[..32]);
        keys.auth.copy_from_slice(&okm[32..64]);
        keys.iv.copy_from_slice(&okm[64..80]);
        keys
    }

    fn mac(auth: &[u8], ciphertext: &[u8]) -> [u8; 16] {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(auth).expect("any key length");
        m.update(ciphertext);
        let full = m.finalize().into_bytes();
        let mut out = [0u8; 16];
        out.copy_from_slice(&full[..16]);
        out
    }

    /// Encrypt a serialized SCE envelope.
    pub fn seal(plaintext: &[u8]) -> Sealed {
        let key = random::<32>();
        let k = derive(key.as_ref());
        let ciphertext = cbc::Encryptor::<aes::Aes256>::new_from_slices(k.enc.as_ref(), &k.iv)
            .expect("fixed sizes")
            .encrypt_padded_vec_mut::<Pkcs7>(plaintext);
        let tag = mac(k.auth.as_ref(), &ciphertext);
        let mut km = Zeroizing::new(Vec::with_capacity(KEY_MATERIAL_LEN));
        km.extend_from_slice(key.as_ref());
        km.extend_from_slice(&tag);
        Sealed {
            key_material: km,
            ciphertext,
            iv: None,
        }
    }

    /// Verify and decrypt a payload with the decrypted key material.
    pub fn open(key_material: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        if key_material.len() != KEY_MATERIAL_LEN {
            return Err(Error::Invalid("OMEMO:2 key material must be 48 bytes"));
        }
        let (key, tag) = key_material.split_at(32);
        let k = derive(key);
        if !bool::from(mac(k.auth.as_ref(), ciphertext).ct_eq(tag)) {
            return Err(Error::InvalidMac);
        }
        cbc::Decryptor::<aes::Aes256>::new_from_slices(k.enc.as_ref(), &k.iv)
            .expect("fixed sizes")
            .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
            .map_err(|_| Error::Invalid("bad padding"))
    }
}

/// XEP-0384 0.3 (legacy): AES-128-GCM payload, key‖tag as key material.
pub mod legacy {
    use super::*;

    type Aes128Gcm16 = AesGcm<aes::Aes128, U16>;

    pub fn seal(plaintext: &[u8]) -> Sealed {
        let key = random::<16>();
        let iv = random::<12>();
        let cipher = Aes128Gcm::new_from_slice(key.as_ref()).expect("16-byte key");
        let mut ct = cipher
            .encrypt(Nonce::from_slice(iv.as_ref()), plaintext)
            .expect("GCM encryption cannot fail");
        let tag = ct.split_off(ct.len() - 16);
        let mut km = Zeroizing::new(Vec::with_capacity(32));
        km.extend_from_slice(key.as_ref());
        km.extend_from_slice(&tag);
        Sealed {
            key_material: km,
            ciphertext: ct,
            iv: Some(iv.to_vec()),
        }
    }

    /// Key material is key(16)‖tag(16); very old clients sent only the key
    /// with the tag appended to the payload. IVs may be 12 or 16 bytes.
    pub fn open(key_material: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        let (key, data) = match key_material.len() {
            32 => {
                let mut d = ciphertext.to_vec();
                d.extend_from_slice(&key_material[16..]);
                (&key_material[..16], d)
            }
            16 => (key_material, ciphertext.to_vec()),
            _ => return Err(Error::Invalid("legacy key material must be 16 or 32 bytes")),
        };
        let payload = Payload { msg: &data, aad: &[] };
        let result = match iv.len() {
            12 => Aes128Gcm::new_from_slice(key)
                .map_err(|_| Error::InvalidKey)?
                .decrypt(Nonce::from_slice(iv), payload),
            16 => Aes128Gcm16::new_from_slice(key)
                .map_err(|_| Error::InvalidKey)?
                .decrypt(aes_gcm::aead::generic_array::GenericArray::from_slice(iv), payload),
            _ => return Err(Error::Invalid("legacy IV must be 12 or 16 bytes")),
        };
        result.map_err(|_| Error::InvalidMac)
    }

    /// Random key material for a key-transport (empty) message.
    pub fn empty_key_material() -> Zeroizing<Vec<u8>> {
        Zeroizing::new(random::<32>().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omemo2_round_trip_and_tamper_detection() {
        let s = omemo2::seal(b"<envelope/>");
        assert_eq!(s.key_material.len(), 48);
        assert_eq!(omemo2::open(&s.key_material, &s.ciphertext).unwrap(), b"<envelope/>");
        let mut bad = s.ciphertext.clone();
        bad[0] ^= 1;
        assert!(matches!(omemo2::open(&s.key_material, &bad), Err(Error::InvalidMac)));
        let mut bad_km = s.key_material.to_vec();
        bad_km[40] ^= 1;
        assert!(omemo2::open(&bad_km, &s.ciphertext).is_err());
    }

    #[test]
    fn omemo2_known_derivation() {
        // HKDF-SHA-256(salt=0^32, ikm=0^32, info="OMEMO Payload") → first IV bytes
        // must be deterministic; check open() of our own seal with a fixed key.
        let key = [7u8; 32];
        let hk = Hkdf::<Sha256>::new(Some(&[0u8; 32]), &key);
        let mut okm = [0u8; 80];
        hk.expand(b"OMEMO Payload", &mut okm).unwrap();
        let ct = cbc::Encryptor::<aes::Aes256>::new_from_slices(&okm[..32], &okm[64..])
            .unwrap()
            .encrypt_padded_vec_mut::<Pkcs7>(b"hi");
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&okm[32..64]).unwrap();
        m.update(&ct);
        let mut km = key.to_vec();
        km.extend_from_slice(&m.finalize().into_bytes()[..16]);
        assert_eq!(omemo2::open(&km, &ct).unwrap(), b"hi");
    }

    #[test]
    fn legacy_round_trip_and_old_formats() {
        let s = legacy::seal(b"hello");
        assert_eq!(s.key_material.len(), 32);
        let iv = s.iv.clone().unwrap();
        assert_eq!(legacy::open(&s.key_material, &iv, &s.ciphertext).unwrap(), b"hello");
        // Old style: tag appended to the payload, 16-byte key material.
        let mut with_tag = s.ciphertext.clone();
        with_tag.extend_from_slice(&s.key_material[16..]);
        assert_eq!(legacy::open(&s.key_material[..16], &iv, &with_tag).unwrap(), b"hello");
        let mut bad = s.ciphertext.clone();
        bad[0] ^= 1;
        assert!(legacy::open(&s.key_material, &iv, &bad).is_err());
    }
}
