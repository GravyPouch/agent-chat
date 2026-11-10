//! Persistent state for one OMEMO version, and the C trampolines that expose
//! it to libomemo-c.
//!
//! Trust is *not* enforced through libomemo-c's `is_trusted_identity`: like
//! Dino, we accept every identity at the ratchet layer and apply trust when
//! choosing recipients and when labelling received messages. The store only
//! records which identity key each device uses.

use std::ffi::{c_char, c_int, c_void};

use omemo_sys as sys;

use crate::crypto::to_signal_buffer;

pub type StoreResult<T> = Result<T, String>;

/// Storage for a single OMEMO version (sessions and keys must not be shared
/// between versions).
pub trait OmemoStore: Send {
    /// Serialized (public, private) identity key pair, as produced by
    /// libomemo-c (`ec_public_key_serialize` / `ec_private_key_serialize`).
    fn identity_key_pair(&mut self) -> StoreResult<(Vec<u8>, Vec<u8>)>;
    fn local_registration_id(&mut self) -> StoreResult<u32>;
    /// Remember the identity key a remote device uses.
    fn save_identity(&mut self, name: &str, device_id: i32, key: &[u8]) -> StoreResult<()>;

    fn load_session(&mut self, name: &str, device_id: i32) -> StoreResult<Option<Vec<u8>>>;
    fn store_session(&mut self, name: &str, device_id: i32, record: &[u8]) -> StoreResult<()>;
    fn contains_session(&mut self, name: &str, device_id: i32) -> StoreResult<bool>;
    fn delete_session(&mut self, name: &str, device_id: i32) -> StoreResult<()>;
    fn delete_all_sessions(&mut self, name: &str) -> StoreResult<usize>;
    fn sub_device_sessions(&mut self, name: &str) -> StoreResult<Vec<i32>>;

    fn load_pre_key(&mut self, id: u32) -> StoreResult<Option<Vec<u8>>>;
    fn store_pre_key(&mut self, id: u32, record: &[u8]) -> StoreResult<()>;
    fn contains_pre_key(&mut self, id: u32) -> StoreResult<bool>;
    fn remove_pre_key(&mut self, id: u32) -> StoreResult<()>;

    fn load_signed_pre_key(&mut self, id: u32) -> StoreResult<Option<Vec<u8>>>;
    fn store_signed_pre_key(&mut self, id: u32, record: &[u8]) -> StoreResult<()>;
    fn contains_signed_pre_key(&mut self, id: u32) -> StoreResult<bool>;
    fn remove_signed_pre_key(&mut self, id: u32) -> StoreResult<()>;
}

/// Heap cell passed to libomemo-c as `user_data` for all four stores.
pub(crate) struct StoreCell {
    pub(crate) inner: Box<dyn OmemoStore>,
}

const OK: c_int = 0;
const ERR: c_int = sys::SG_ERR_UNKNOWN;

unsafe fn cell<'a>(user_data: *mut c_void) -> &'a mut StoreCell {
    unsafe { &mut *user_data.cast::<StoreCell>() }
}

unsafe fn name_of(name: *const c_char, len: usize) -> String {
    let bytes = unsafe { std::slice::from_raw_parts(name.cast::<u8>(), len) };
    String::from_utf8_lossy(bytes).into_owned()
}

unsafe fn address(a: *const sys::signal_protocol_address) -> (String, i32) {
    let a = unsafe { &*a };
    (unsafe { name_of(a.name, a.name_len) }, a.device_id)
}

unsafe fn slice<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if data.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(data, len) }
    }
}

fn log_err(what: &str, e: &str) -> c_int {
    eprintln!("omemo store: {what}: {e}");
    ERR
}

// ------------------------------------------------------------------ sessions

unsafe extern "C" fn load_session(
    record: *mut *mut sys::signal_buffer,
    user_record: *mut *mut sys::signal_buffer,
    addr: *const sys::signal_protocol_address,
    ud: *mut c_void,
) -> c_int {
    let (name, dev) = unsafe { address(addr) };
    if !user_record.is_null() {
        unsafe { *user_record = std::ptr::null_mut() };
    }
    match unsafe { cell(ud) }.inner.load_session(&name, dev) {
        Ok(Some(r)) => {
            let rc = unsafe { to_signal_buffer(&r, record) };
            if rc < 0 { rc } else { 1 }
        }
        Ok(None) => 0,
        Err(e) => log_err("load_session", &e),
    }
}

unsafe extern "C" fn sub_device_sessions(
    sessions: *mut *mut sys::signal_int_list,
    name: *const c_char,
    name_len: usize,
    ud: *mut c_void,
) -> c_int {
    let name = unsafe { name_of(name, name_len) };
    match unsafe { cell(ud) }.inner.sub_device_sessions(&name) {
        Ok(ids) => {
            let list = unsafe { sys::signal_int_list_alloc() };
            if list.is_null() {
                return sys::SG_ERR_NOMEM;
            }
            for id in &ids {
                unsafe { sys::signal_int_list_push_back(list, *id) };
            }
            unsafe { *sessions = list };
            ids.len() as c_int
        }
        Err(e) => log_err("sub_device_sessions", &e),
    }
}

unsafe extern "C" fn store_session(
    addr: *const sys::signal_protocol_address,
    record: *mut u8,
    record_len: usize,
    _user_record: *mut u8,
    _user_record_len: usize,
    ud: *mut c_void,
) -> c_int {
    let (name, dev) = unsafe { address(addr) };
    match unsafe { cell(ud) }
        .inner
        .store_session(&name, dev, unsafe { slice(record, record_len) })
    {
        Ok(()) => OK,
        Err(e) => log_err("store_session", &e),
    }
}

unsafe extern "C" fn contains_session(addr: *const sys::signal_protocol_address, ud: *mut c_void) -> c_int {
    let (name, dev) = unsafe { address(addr) };
    match unsafe { cell(ud) }.inner.contains_session(&name, dev) {
        Ok(b) => b as c_int,
        Err(e) => log_err("contains_session", &e),
    }
}

unsafe extern "C" fn delete_session(addr: *const sys::signal_protocol_address, ud: *mut c_void) -> c_int {
    let (name, dev) = unsafe { address(addr) };
    match unsafe { cell(ud) }.inner.delete_session(&name, dev) {
        Ok(()) => 1,
        Err(e) => log_err("delete_session", &e),
    }
}

unsafe extern "C" fn delete_all_sessions(name: *const c_char, name_len: usize, ud: *mut c_void) -> c_int {
    let name = unsafe { name_of(name, name_len) };
    match unsafe { cell(ud) }.inner.delete_all_sessions(&name) {
        Ok(n) => n as c_int,
        Err(e) => log_err("delete_all_sessions", &e),
    }
}

unsafe extern "C" fn destroy(ud: *mut c_void) {
    if !ud.is_null() {
        drop(unsafe { Box::from_raw(ud.cast::<StoreCell>()) });
    }
}

// ------------------------------------------------------------------ pre keys

unsafe extern "C" fn load_pre_key(record: *mut *mut sys::signal_buffer, id: u32, ud: *mut c_void) -> c_int {
    match unsafe { cell(ud) }.inner.load_pre_key(id) {
        Ok(Some(r)) => unsafe { to_signal_buffer(&r, record) },
        Ok(None) => sys::SG_ERR_INVALID_KEY_ID,
        Err(e) => log_err("load_pre_key", &e),
    }
}

unsafe extern "C" fn store_pre_key(id: u32, record: *mut u8, len: usize, ud: *mut c_void) -> c_int {
    match unsafe { cell(ud) }
        .inner
        .store_pre_key(id, unsafe { slice(record, len) })
    {
        Ok(()) => OK,
        Err(e) => log_err("store_pre_key", &e),
    }
}

unsafe extern "C" fn contains_pre_key(id: u32, ud: *mut c_void) -> c_int {
    match unsafe { cell(ud) }.inner.contains_pre_key(id) {
        Ok(b) => b as c_int,
        Err(e) => log_err("contains_pre_key", &e),
    }
}

unsafe extern "C" fn remove_pre_key(id: u32, ud: *mut c_void) -> c_int {
    match unsafe { cell(ud) }.inner.remove_pre_key(id) {
        Ok(()) => OK,
        Err(e) => log_err("remove_pre_key", &e),
    }
}

unsafe extern "C" fn load_signed_pre_key(record: *mut *mut sys::signal_buffer, id: u32, ud: *mut c_void) -> c_int {
    match unsafe { cell(ud) }.inner.load_signed_pre_key(id) {
        Ok(Some(r)) => unsafe { to_signal_buffer(&r, record) },
        Ok(None) => sys::SG_ERR_INVALID_KEY_ID,
        Err(e) => log_err("load_signed_pre_key", &e),
    }
}

unsafe extern "C" fn store_signed_pre_key(id: u32, record: *mut u8, len: usize, ud: *mut c_void) -> c_int {
    match unsafe { cell(ud) }
        .inner
        .store_signed_pre_key(id, unsafe { slice(record, len) })
    {
        Ok(()) => OK,
        Err(e) => log_err("store_signed_pre_key", &e),
    }
}

unsafe extern "C" fn contains_signed_pre_key(id: u32, ud: *mut c_void) -> c_int {
    match unsafe { cell(ud) }.inner.contains_signed_pre_key(id) {
        Ok(b) => b as c_int,
        Err(e) => log_err("contains_signed_pre_key", &e),
    }
}

unsafe extern "C" fn remove_signed_pre_key(id: u32, ud: *mut c_void) -> c_int {
    match unsafe { cell(ud) }.inner.remove_signed_pre_key(id) {
        Ok(()) => OK,
        Err(e) => log_err("remove_signed_pre_key", &e),
    }
}

// ------------------------------------------------------------------ identity

unsafe extern "C" fn get_identity_key_pair(
    public_data: *mut *mut sys::signal_buffer,
    private_data: *mut *mut sys::signal_buffer,
    ud: *mut c_void,
) -> c_int {
    match unsafe { cell(ud) }.inner.identity_key_pair() {
        Ok((public, private)) => {
            let rc = unsafe { to_signal_buffer(&public, public_data) };
            if rc < 0 {
                return rc;
            }
            unsafe { to_signal_buffer(&private, private_data) }
        }
        Err(e) => log_err("identity_key_pair", &e),
    }
}

unsafe extern "C" fn get_local_registration_id(ud: *mut c_void, id: *mut u32) -> c_int {
    match unsafe { cell(ud) }.inner.local_registration_id() {
        Ok(v) => {
            unsafe { *id = v };
            OK
        }
        Err(e) => log_err("local_registration_id", &e),
    }
}

unsafe extern "C" fn save_identity(
    addr: *const sys::signal_protocol_address,
    key: *mut u8,
    key_len: usize,
    ud: *mut c_void,
) -> c_int {
    let (name, dev) = unsafe { address(addr) };
    match unsafe { cell(ud) }
        .inner
        .save_identity(&name, dev, unsafe { slice(key, key_len) })
    {
        Ok(()) => OK,
        Err(e) => log_err("save_identity", &e),
    }
}

unsafe extern "C" fn is_trusted_identity(
    _addr: *const sys::signal_protocol_address,
    _key: *mut u8,
    _key_len: usize,
    _ud: *mut c_void,
) -> c_int {
    // Trust is applied by the application (see module docs).
    1
}

/// Register the four stores on a libomemo-c store context. Ownership of
/// `cell` passes to the context (freed by the session store's destroy hook).
pub(crate) unsafe fn install(ctx: *mut sys::signal_protocol_store_context, cell: Box<StoreCell>) -> c_int {
    let ud: *mut c_void = Box::into_raw(cell).cast();
    let session = sys::signal_protocol_session_store {
        load_session_func: Some(load_session),
        get_sub_device_sessions_func: Some(sub_device_sessions),
        store_session_func: Some(store_session),
        contains_session_func: Some(contains_session),
        delete_session_func: Some(delete_session),
        delete_all_sessions_func: Some(delete_all_sessions),
        destroy_func: Some(destroy),
        user_data: ud,
    };
    let pre_key = sys::signal_protocol_pre_key_store {
        load_pre_key: Some(load_pre_key),
        store_pre_key: Some(store_pre_key),
        contains_pre_key: Some(contains_pre_key),
        remove_pre_key: Some(remove_pre_key),
        destroy_func: None,
        user_data: ud,
    };
    let signed = sys::signal_protocol_signed_pre_key_store {
        load_signed_pre_key: Some(load_signed_pre_key),
        store_signed_pre_key: Some(store_signed_pre_key),
        contains_signed_pre_key: Some(contains_signed_pre_key),
        remove_signed_pre_key: Some(remove_signed_pre_key),
        destroy_func: None,
        user_data: ud,
    };
    let identity = sys::signal_protocol_identity_key_store {
        get_identity_key_pair: Some(get_identity_key_pair),
        get_local_registration_id: Some(get_local_registration_id),
        save_identity: Some(save_identity),
        is_trusted_identity: Some(is_trusted_identity),
        destroy_func: None,
        user_data: ud,
    };
    unsafe {
        let mut rc = sys::signal_protocol_store_context_set_session_store(ctx, &session);
        if rc == 0 {
            rc = sys::signal_protocol_store_context_set_pre_key_store(ctx, &pre_key);
        }
        if rc == 0 {
            rc = sys::signal_protocol_store_context_set_signed_pre_key_store(ctx, &signed);
        }
        if rc == 0 {
            rc = sys::signal_protocol_store_context_set_identity_key_store(ctx, &identity);
        }
        if rc != 0 {
            // Context never took ownership.
            destroy(ud);
        }
        rc
    }
}
