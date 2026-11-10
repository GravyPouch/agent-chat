//! Safe wrapper around a libomemo-c context for one OMEMO version.

use std::ffi::{c_char, c_int, c_void};
use std::ptr;

use omemo_sys as sys;

use crate::store::{OmemoStore, StoreCell, install};
use crate::{Error, Version, check};

/// Owned reference to a libomemo-c ref-counted object.
struct Ref<T>(*mut T);

impl<T> Drop for Ref<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sys::signal_type_unref(self.0.cast()) };
        }
    }
}

/// Owned `signal_buffer`.
struct Buf(*mut sys::signal_buffer);

impl Buf {
    fn to_vec(&self) -> Vec<u8> {
        if self.0.is_null() {
            return Vec::new();
        }
        unsafe {
            let len = sys::signal_buffer_len(self.0);
            let data = sys::signal_buffer_data(self.0);
            if len == 0 || data.is_null() {
                Vec::new()
            } else {
                std::slice::from_raw_parts(data, len).to_vec()
            }
        }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { sys::signal_buffer_bzero_free(self.0) };
        }
    }
}

/// A remote device's published bundle, in that version's wire encoding
/// (already base64-decoded).
#[derive(Debug, Clone)]
pub struct RemoteBundle {
    pub identity_key: Vec<u8>,
    pub signed_pre_key_id: u32,
    pub signed_pre_key: Vec<u8>,
    pub signed_pre_key_signature: Vec<u8>,
    pub pre_key_id: u32,
    pub pre_key: Vec<u8>,
}

/// Our own bundle, in the version's wire encoding (to be base64-encoded).
#[derive(Debug, Clone)]
pub struct OwnBundle {
    pub identity_key: Vec<u8>,
    pub signed_pre_key_id: u32,
    pub signed_pre_key: Vec<u8>,
    pub signed_pre_key_signature: Vec<u8>,
    pub pre_keys: Vec<(u32, Vec<u8>)>,
}

#[derive(Debug, Clone)]
pub struct Decrypted {
    pub plaintext: Vec<u8>,
    /// Set when this was a key exchange that consumed one of our pre keys.
    pub used_pre_key: Option<u32>,
}

unsafe extern "C" fn log_fn(level: c_int, message: *const c_char, len: usize, _: *mut c_void) {
    if level <= sys::SG_LOG_WARNING as c_int && !message.is_null() {
        let bytes = unsafe { std::slice::from_raw_parts(message.cast::<u8>(), len) };
        eprintln!("libomemo-c: {}", String::from_utf8_lossy(bytes));
    }
}

/// One libomemo-c context with its stores. Not thread-safe: wrap in a mutex.
pub struct Context {
    global: *mut sys::signal_context,
    store: *mut sys::signal_protocol_store_context,
    version: Version,
}

// The raw pointers are only used through `&mut self`/`&self` methods on one
// thread at a time (callers serialise access).
unsafe impl Send for Context {}

struct Address<'a> {
    raw: sys::signal_protocol_address,
    _name: &'a str,
}

impl<'a> Address<'a> {
    fn new(name: &'a str, device_id: u32) -> Self {
        Self {
            raw: sys::signal_protocol_address {
                name: name.as_ptr().cast(),
                name_len: name.len(),
                device_id: device_id as i32,
            },
            _name: name,
        }
    }
}

impl Context {
    pub fn new(version: Version, store: Box<dyn OmemoStore>) -> Result<Self, Error> {
        let mut global = ptr::null_mut();
        check(unsafe { sys::signal_context_create(&mut global, ptr::null_mut()) })?;
        let mut ctx = Self {
            global,
            store: ptr::null_mut(),
            version,
        };
        let provider = crate::crypto::provider();
        check(unsafe { sys::signal_context_set_crypto_provider(global, &provider) })?;
        check(unsafe { sys::signal_context_set_log_function(global, Some(log_fn)) })?;
        let mut store_ctx = ptr::null_mut();
        check(unsafe { sys::signal_protocol_store_context_create(&mut store_ctx, global) })?;
        ctx.store = store_ctx;
        check(unsafe { install(store_ctx, Box::new(StoreCell { inner: store })) })?;
        Ok(ctx)
    }

    pub fn version(&self) -> Version {
        self.version
    }

    fn lib_version(&self) -> u32 {
        match self.version {
            Version::Legacy => sys::CIPHERTEXT_CURRENT_VERSION,
            Version::Omemo2 => sys::CIPHERTEXT_OMEMO_VERSION,
        }
    }

    /// Serialize a public key the way this version puts DH keys on the wire.
    fn wire_dh_key(&self, key: *const sys::ec_public_key) -> Result<Vec<u8>, Error> {
        let mut buf = ptr::null_mut();
        match self.version {
            Version::Legacy => check(unsafe { sys::ec_public_key_serialize(&mut buf, key) })?,
            Version::Omemo2 => check(unsafe { sys::ec_public_key_serialize_omemo(&mut buf, key) })?,
        };
        Ok(Buf(buf).to_vec())
    }

    fn decode_dh_key(&self, data: &[u8]) -> Result<Ref<sys::ec_public_key>, Error> {
        let mut key = ptr::null_mut();
        let rc = match self.version {
            Version::Legacy => unsafe { sys::curve_decode_point(&mut key, data.as_ptr(), data.len(), self.global) },
            Version::Omemo2 => unsafe { sys::curve_decode_point_mont(&mut key, data.as_ptr(), data.len(), self.global) },
        };
        check(rc)?;
        Ok(Ref(key))
    }

    fn decode_identity_key(&self, data: &[u8]) -> Result<Ref<sys::ec_public_key>, Error> {
        let mut key = ptr::null_mut();
        let rc = match self.version {
            // 33 bytes: 0x05 || Curve25519
            Version::Legacy => unsafe { sys::curve_decode_point(&mut key, data.as_ptr(), data.len(), self.global) },
            // 32 bytes: Ed25519
            Version::Omemo2 => unsafe { sys::curve_decode_point_ed(&mut key, data.as_ptr(), data.len(), self.global) },
        };
        check(rc)?;
        Ok(Ref(key))
    }

    /// Generate a fresh identity key pair, serialized for [`OmemoStore::identity_key_pair`].
    pub fn generate_identity_key_pair(&self) -> Result<(Vec<u8>, Vec<u8>), Error> {
        let mut kp = ptr::null_mut();
        check(unsafe { sys::signal_protocol_key_helper_generate_identity_key_pair(&mut kp, self.global) })?;
        let kp = Ref(kp);
        let (mut pub_buf, mut priv_buf) = (ptr::null_mut(), ptr::null_mut());
        unsafe {
            check(sys::ec_public_key_serialize(&mut pub_buf, sys::ratchet_identity_key_pair_get_public(kp.0)))?;
            let pub_buf = Buf(pub_buf);
            check(sys::ec_private_key_serialize(&mut priv_buf, sys::ratchet_identity_key_pair_get_private(kp.0)))?;
            let priv_buf = Buf(priv_buf);
            Ok((pub_buf.to_vec(), priv_buf.to_vec()))
        }
    }

    /// Generate pre keys `start..start+count`, returning serialized records.
    pub fn generate_pre_keys(&self, start: u32, count: u32) -> Result<Vec<(u32, Vec<u8>)>, Error> {
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            // Ids wrap within 1..=0xFFFFFE like libsignal's medium value range.
            let id = (start.wrapping_sub(1).wrapping_add(i) % 0xFF_FFFE) + 1;
            let mut pair = ptr::null_mut();
            check(unsafe { sys::curve_generate_key_pair(self.global, &mut pair) })?;
            let pair = Ref(pair);
            let mut pk = ptr::null_mut();
            check(unsafe { sys::session_pre_key_create(&mut pk, id, pair.0) })?;
            let pk = Ref(pk);
            let mut buf = ptr::null_mut();
            check(unsafe { sys::session_pre_key_serialize(&mut buf, pk.0) })?;
            out.push((id, Buf(buf).to_vec()));
        }
        Ok(out)
    }

    /// Generate a signed pre key with the stored identity. Returns the serialized record.
    pub fn generate_signed_pre_key(&self, id: u32, timestamp_ms: u64) -> Result<Vec<u8>, Error> {
        let mut identity = ptr::null_mut();
        check(unsafe { sys::signal_protocol_identity_get_key_pair(self.store, &mut identity) })?;
        let identity = Ref(identity);
        let mut spk = ptr::null_mut();
        check(unsafe {
            sys::signal_protocol_key_helper_generate_signed_pre_key(&mut spk, identity.0, id, timestamp_ms, self.global)
        })?;
        let spk = Ref(spk);
        let mut buf = ptr::null_mut();
        check(unsafe { sys::session_signed_pre_key_serialize(&mut buf, spk.0) })?;
        Ok(Buf(buf).to_vec())
    }

    /// Our identity public key in this version's wire format.
    pub fn identity_public(&self) -> Result<Vec<u8>, Error> {
        let mut identity = ptr::null_mut();
        check(unsafe { sys::signal_protocol_identity_get_key_pair(self.store, &mut identity) })?;
        let identity = Ref(identity);
        let public = unsafe { sys::ratchet_identity_key_pair_get_public(identity.0) };
        match self.version {
            Version::Legacy => {
                let mut buf = ptr::null_mut();
                check(unsafe { sys::ec_public_key_serialize(&mut buf, public) })?;
                Ok(Buf(buf).to_vec())
            }
            Version::Omemo2 => Ok(Buf(unsafe { sys::ec_public_key_get_ed(public) }).to_vec()),
        }
    }

    /// Build our publishable bundle from stored records.
    pub fn own_bundle(&self, signed_pre_key_record: &[u8], pre_key_records: &[(u32, Vec<u8>)]) -> Result<OwnBundle, Error> {
        let mut spk = ptr::null_mut();
        check(unsafe {
            sys::session_signed_pre_key_deserialize(
                &mut spk,
                signed_pre_key_record.as_ptr(),
                signed_pre_key_record.len(),
                self.global,
            )
        })?;
        let spk = Ref(spk);
        let (spk_id, spk_public, signature) = unsafe {
            let pair = sys::session_signed_pre_key_get_key_pair(spk.0);
            let public = self.wire_dh_key(sys::ec_key_pair_get_public(pair))?;
            let (sig, len) = match self.version {
                Version::Legacy => (
                    sys::session_signed_pre_key_get_signature(spk.0),
                    sys::session_signed_pre_key_get_signature_len(spk.0),
                ),
                Version::Omemo2 => (
                    sys::session_signed_pre_key_get_signature_omemo(spk.0),
                    sys::session_signed_pre_key_get_signature_omemo_len(spk.0),
                ),
            };
            if sig.is_null() || len == 0 {
                return Err(Error::Invalid("signed pre key has no signature for this version"));
            }
            (
                sys::session_signed_pre_key_get_id(spk.0),
                public,
                std::slice::from_raw_parts(sig, len).to_vec(),
            )
        };
        let mut pre_keys = Vec::with_capacity(pre_key_records.len());
        for (id, record) in pre_key_records {
            let mut pk = ptr::null_mut();
            check(unsafe { sys::session_pre_key_deserialize(&mut pk, record.as_ptr(), record.len(), self.global) })?;
            let pk = Ref(pk);
            let public = unsafe {
                let pair = sys::session_pre_key_get_key_pair(pk.0);
                self.wire_dh_key(sys::ec_key_pair_get_public(pair))?
            };
            pre_keys.push((*id, public));
        }
        Ok(OwnBundle {
            identity_key: self.identity_public()?,
            signed_pre_key_id: spk_id,
            signed_pre_key: spk_public,
            signed_pre_key_signature: signature,
            pre_keys,
        })
    }

    pub fn has_session(&self, name: &str, device_id: u32) -> bool {
        let addr = Address::new(name, device_id);
        unsafe { sys::signal_protocol_session_contains_session(self.store, &addr.raw) == 1 }
    }

    /// Start a session from a remote bundle (verifies the signed pre key signature).
    pub fn process_bundle(&self, name: &str, device_id: u32, b: &RemoteBundle) -> Result<(), Error> {
        let ik = self.decode_identity_key(&b.identity_key)?;
        let spk = self.decode_dh_key(&b.signed_pre_key)?;
        let pk = self.decode_dh_key(&b.pre_key)?;
        let mut bundle = ptr::null_mut();
        check(unsafe {
            sys::session_pre_key_bundle_create(
                &mut bundle,
                0,
                device_id as c_int,
                b.pre_key_id,
                pk.0,
                b.signed_pre_key_id,
                spk.0,
                b.signed_pre_key_signature.as_ptr(),
                b.signed_pre_key_signature.len(),
                ik.0,
            )
        })?;
        let bundle = Ref(bundle);
        let addr = Address::new(name, device_id);
        let mut builder = ptr::null_mut();
        check(unsafe { sys::session_builder_create(&mut builder, self.store, &addr.raw, self.global) })?;
        unsafe { sys::session_builder_set_version(builder, self.lib_version()) };
        let rc = unsafe { sys::session_builder_process_pre_key_bundle(builder, bundle.0) };
        unsafe { sys::session_builder_free(builder) };
        check(rc)?;
        Ok(())
    }

    fn with_cipher<T>(
        &self,
        name: &str,
        device_id: u32,
        f: impl FnOnce(*mut sys::session_cipher) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let addr = Address::new(name, device_id);
        let mut cipher = ptr::null_mut();
        check(unsafe { sys::session_cipher_create(&mut cipher, self.store, &addr.raw, self.global) })?;
        unsafe { sys::session_cipher_set_version(cipher, self.lib_version()) };
        let result = f(cipher);
        unsafe { sys::session_cipher_free(cipher) };
        result
    }

    /// Encrypt `plaintext` (OMEMO key material) for a device with an existing
    /// session. Returns the serialized message and whether it's a key exchange.
    pub fn encrypt(&self, name: &str, device_id: u32, plaintext: &[u8]) -> Result<(Vec<u8>, bool), Error> {
        if !self.has_session(name, device_id) {
            return Err(Error::NoSession);
        }
        self.with_cipher(name, device_id, |cipher| {
            let mut msg = ptr::null_mut();
            check(unsafe { sys::session_cipher_encrypt(cipher, plaintext.as_ptr(), plaintext.len(), &mut msg) })?;
            let msg = Ref(msg);
            let kex = unsafe { sys::ciphertext_message_get_type(msg.0) } == sys::CIPHERTEXT_PREKEY_TYPE as c_int;
            // Borrowed buffer owned by the message: copy, don't free.
            let data = unsafe { sys::ciphertext_message_get_serialized(msg.0) };
            if data.is_null() {
                return Err(Error::Invalid("empty ciphertext"));
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(sys::signal_buffer_data(data), sys::signal_buffer_len(data)).to_vec()
            };
            Ok((bytes, kex))
        })
    }

    /// Decrypt a message from `name`/`device_id`. `kex` = the `<key>` was
    /// marked as a key exchange (`kex='true'` / `prekey='true'`).
    pub fn decrypt(&self, name: &str, device_id: u32, data: &[u8], kex: bool) -> Result<Decrypted, Error> {
        self.with_cipher(name, device_id, |cipher| unsafe {
            let mut out = ptr::null_mut();
            let mut used_pre_key = None;
            if kex {
                let mut m = ptr::null_mut();
                let rc = match self.version {
                    Version::Legacy => sys::pre_key_signal_message_deserialize(&mut m, data.as_ptr(), data.len(), self.global),
                    Version::Omemo2 => sys::pre_key_signal_message_deserialize_omemo(
                        &mut m,
                        data.as_ptr(),
                        data.len(),
                        device_id,
                        self.global,
                    ),
                };
                check(rc)?;
                let m = Ref(m);
                if sys::pre_key_signal_message_has_pre_key_id(m.0) != 0 {
                    used_pre_key = Some(sys::pre_key_signal_message_get_pre_key_id(m.0));
                }
                check(sys::session_cipher_decrypt_pre_key_signal_message(cipher, m.0, ptr::null_mut(), &mut out))?;
            } else {
                let mut m = ptr::null_mut();
                let rc = match self.version {
                    Version::Legacy => sys::signal_message_deserialize(&mut m, data.as_ptr(), data.len(), self.global),
                    Version::Omemo2 => sys::signal_message_deserialize_omemo(&mut m, data.as_ptr(), data.len(), self.global),
                };
                check(rc)?;
                let m = Ref(m);
                check(sys::session_cipher_decrypt_signal_message(cipher, m.0, ptr::null_mut(), &mut out))?;
            }
            let out = Buf(out);
            Ok(Decrypted {
                plaintext: out.to_vec(),
                used_pre_key,
            })
        })
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            if !self.store.is_null() {
                sys::signal_protocol_store_context_destroy(self.store);
            }
            if !self.global.is_null() {
                sys::signal_context_destroy(self.global);
            }
        }
    }
}
