//! libomemo-c crypto provider implemented with RustCrypto.

use std::ffi::{c_int, c_void};
use std::ptr;

use aes::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, StreamCipher, block_padding::Pkcs7};
use hmac::{Hmac, Mac};
use omemo_sys as sys;
use sha2::{Digest, Sha256, Sha512};

type HmacSha256 = Hmac<Sha256>;

const OK: c_int = sys::SG_SUCCESS as c_int;
const ERR_UNKNOWN: c_int = sys::SG_ERR_UNKNOWN;
const ERR_NOMEM: c_int = sys::SG_ERR_NOMEM;
const ERR_INVAL: c_int = sys::SG_ERR_INVAL;

pub(crate) fn provider() -> sys::signal_crypto_provider {
    sys::signal_crypto_provider {
        random_func: Some(random),
        hmac_sha256_init_func: Some(hmac_init),
        hmac_sha256_update_func: Some(hmac_update),
        hmac_sha256_final_func: Some(hmac_final),
        hmac_sha256_cleanup_func: Some(hmac_cleanup),
        sha512_digest_init_func: Some(sha512_init),
        sha512_digest_update_func: Some(sha512_update),
        sha512_digest_final_func: Some(sha512_final),
        sha512_digest_cleanup_func: Some(sha512_cleanup),
        encrypt_func: Some(encrypt),
        decrypt_func: Some(decrypt),
        user_data: ptr::null_mut(),
    }
}

/// # Safety
/// `data` must be null with `len == 0`, or valid for `len` bytes.
unsafe fn bytes<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if len == 0 || data.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }
    }
}

/// Hand a byte vector to libomemo-c as a freshly allocated `signal_buffer`.
pub(crate) unsafe fn to_signal_buffer(data: &[u8], out: *mut *mut sys::signal_buffer) -> c_int {
    let buf = unsafe { sys::signal_buffer_create(data.as_ptr(), data.len()) };
    if buf.is_null() {
        return ERR_NOMEM;
    }
    unsafe { *out = buf };
    OK
}

unsafe extern "C" fn random(data: *mut u8, len: usize, _: *mut c_void) -> c_int {
    if len == 0 {
        return OK;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(data, len) };
    match getrandom::fill(buf) {
        Ok(()) => OK,
        Err(_) => ERR_UNKNOWN,
    }
}

unsafe extern "C" fn hmac_init(ctx: *mut *mut c_void, key: *const u8, key_len: usize, _: *mut c_void) -> c_int {
    let Ok(mac) = HmacSha256::new_from_slice(unsafe { bytes(key, key_len) }) else {
        return ERR_INVAL;
    };
    unsafe { *ctx = Box::into_raw(Box::new(mac)).cast() };
    OK
}

unsafe extern "C" fn hmac_update(ctx: *mut c_void, data: *const u8, len: usize, _: *mut c_void) -> c_int {
    let mac = unsafe { &mut *ctx.cast::<HmacSha256>() };
    mac.update(unsafe { bytes(data, len) });
    OK
}

unsafe extern "C" fn hmac_final(ctx: *mut c_void, out: *mut *mut sys::signal_buffer, _: *mut c_void) -> c_int {
    let mac = unsafe { &mut *ctx.cast::<HmacSha256>() };
    let tag = mac.clone().finalize().into_bytes();
    unsafe { to_signal_buffer(&tag, out) }
}

unsafe extern "C" fn hmac_cleanup(ctx: *mut c_void, _: *mut c_void) {
    if !ctx.is_null() {
        drop(unsafe { Box::from_raw(ctx.cast::<HmacSha256>()) });
    }
}

unsafe extern "C" fn sha512_init(ctx: *mut *mut c_void, _: *mut c_void) -> c_int {
    unsafe { *ctx = Box::into_raw(Box::new(Sha512::new())).cast() };
    OK
}

unsafe extern "C" fn sha512_update(ctx: *mut c_void, data: *const u8, len: usize, _: *mut c_void) -> c_int {
    let h = unsafe { &mut *ctx.cast::<Sha512>() };
    h.update(unsafe { bytes(data, len) });
    OK
}

unsafe extern "C" fn sha512_final(ctx: *mut c_void, out: *mut *mut sys::signal_buffer, _: *mut c_void) -> c_int {
    let h = unsafe { &mut *ctx.cast::<Sha512>() };
    let digest = h.finalize_reset();
    unsafe { to_signal_buffer(&digest, out) }
}

unsafe extern "C" fn sha512_cleanup(ctx: *mut c_void, _: *mut c_void) {
    if !ctx.is_null() {
        drop(unsafe { Box::from_raw(ctx.cast::<Sha512>()) });
    }
}

fn cbc_encrypt(key: &[u8], iv: &[u8], pt: &[u8]) -> Option<Vec<u8>> {
    Some(match key.len() {
        16 => cbc::Encryptor::<aes::Aes128>::new_from_slices(key, iv)
            .ok()?
            .encrypt_padded_vec_mut::<Pkcs7>(pt),
        24 => cbc::Encryptor::<aes::Aes192>::new_from_slices(key, iv)
            .ok()?
            .encrypt_padded_vec_mut::<Pkcs7>(pt),
        32 => cbc::Encryptor::<aes::Aes256>::new_from_slices(key, iv)
            .ok()?
            .encrypt_padded_vec_mut::<Pkcs7>(pt),
        _ => return None,
    })
}

fn cbc_decrypt(key: &[u8], iv: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    match key.len() {
        16 => cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv)
            .ok()?
            .decrypt_padded_vec_mut::<Pkcs7>(ct)
            .ok(),
        24 => cbc::Decryptor::<aes::Aes192>::new_from_slices(key, iv)
            .ok()?
            .decrypt_padded_vec_mut::<Pkcs7>(ct)
            .ok(),
        32 => cbc::Decryptor::<aes::Aes256>::new_from_slices(key, iv)
            .ok()?
            .decrypt_padded_vec_mut::<Pkcs7>(ct)
            .ok(),
        _ => None,
    }
}

fn ctr_apply(key: &[u8], iv: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let mut buf = data.to_vec();
    match key.len() {
        16 => ctr::Ctr128BE::<aes::Aes128>::new_from_slices(key, iv)
            .ok()?
            .apply_keystream(&mut buf),
        24 => ctr::Ctr128BE::<aes::Aes192>::new_from_slices(key, iv)
            .ok()?
            .apply_keystream(&mut buf),
        32 => ctr::Ctr128BE::<aes::Aes256>::new_from_slices(key, iv)
            .ok()?
            .apply_keystream(&mut buf),
        _ => return None,
    }
    Some(buf)
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn encrypt(
    out: *mut *mut sys::signal_buffer,
    cipher: c_int,
    key: *const u8,
    key_len: usize,
    iv: *const u8,
    iv_len: usize,
    pt: *const u8,
    pt_len: usize,
    _: *mut c_void,
) -> c_int {
    let (key, iv, pt) = unsafe { (bytes(key, key_len), bytes(iv, iv_len), bytes(pt, pt_len)) };
    let result = match cipher as u32 {
        sys::SG_CIPHER_AES_CBC_PKCS5 => cbc_encrypt(key, iv, pt),
        sys::SG_CIPHER_AES_CTR_NOPADDING => ctr_apply(key, iv, pt),
        _ => None,
    };
    match result {
        Some(ct) => unsafe { to_signal_buffer(&ct, out) },
        None => ERR_UNKNOWN,
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn decrypt(
    out: *mut *mut sys::signal_buffer,
    cipher: c_int,
    key: *const u8,
    key_len: usize,
    iv: *const u8,
    iv_len: usize,
    ct: *const u8,
    ct_len: usize,
    _: *mut c_void,
) -> c_int {
    let (key, iv, ct) = unsafe { (bytes(key, key_len), bytes(iv, iv_len), bytes(ct, ct_len)) };
    let result = match cipher as u32 {
        sys::SG_CIPHER_AES_CBC_PKCS5 => cbc_decrypt(key, iv, ct),
        sys::SG_CIPHER_AES_CTR_NOPADDING => ctr_apply(key, iv, ct),
        _ => None,
    };
    match result {
        Some(pt) => unsafe { to_signal_buffer(&pt, out) },
        None => ERR_UNKNOWN,
    }
}
