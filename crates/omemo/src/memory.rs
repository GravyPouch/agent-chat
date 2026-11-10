//! In-memory [`OmemoStore`], for tests and throwaway sessions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::store::{OmemoStore, StoreResult};

#[derive(Default)]
struct Inner {
    identity: Option<(Vec<u8>, Vec<u8>)>,
    registration_id: u32,
    identities: HashMap<(String, i32), Vec<u8>>,
    sessions: HashMap<(String, i32), Vec<u8>>,
    pre_keys: HashMap<u32, Vec<u8>>,
    signed_pre_keys: HashMap<u32, Vec<u8>>,
}

/// Cloneable handle: clones share the same state.
#[derive(Clone, Default)]
pub struct MemoryStore(Arc<Mutex<Inner>>);

impl MemoryStore {
    pub fn new(registration_id: u32) -> Self {
        let s = Self::default();
        s.0.lock().unwrap().registration_id = registration_id;
        s
    }

    pub fn set_identity(&self, public: Vec<u8>, private: Vec<u8>) {
        self.0.lock().unwrap().identity = Some((public, private));
    }

    pub fn pre_key_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.0.lock().unwrap().pre_keys.keys().copied().collect();
        ids.sort();
        ids
    }

    pub fn remote_identity(&self, name: &str, device_id: i32) -> Option<Vec<u8>> {
        self.0.lock().unwrap().identities.get(&(name.to_owned(), device_id)).cloned()
    }
}

impl OmemoStore for MemoryStore {
    fn identity_key_pair(&mut self) -> StoreResult<(Vec<u8>, Vec<u8>)> {
        self.0.lock().unwrap().identity.clone().ok_or_else(|| "no identity".into())
    }
    fn local_registration_id(&mut self) -> StoreResult<u32> {
        Ok(self.0.lock().unwrap().registration_id)
    }
    fn save_identity(&mut self, name: &str, device_id: i32, key: &[u8]) -> StoreResult<()> {
        self.0.lock().unwrap().identities.insert((name.into(), device_id), key.to_vec());
        Ok(())
    }
    fn load_session(&mut self, name: &str, device_id: i32) -> StoreResult<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().sessions.get(&(name.into(), device_id)).cloned())
    }
    fn store_session(&mut self, name: &str, device_id: i32, record: &[u8]) -> StoreResult<()> {
        self.0.lock().unwrap().sessions.insert((name.into(), device_id), record.to_vec());
        Ok(())
    }
    fn contains_session(&mut self, name: &str, device_id: i32) -> StoreResult<bool> {
        Ok(self.0.lock().unwrap().sessions.contains_key(&(name.into(), device_id)))
    }
    fn delete_session(&mut self, name: &str, device_id: i32) -> StoreResult<()> {
        self.0.lock().unwrap().sessions.remove(&(name.into(), device_id));
        Ok(())
    }
    fn delete_all_sessions(&mut self, name: &str) -> StoreResult<usize> {
        let mut g = self.0.lock().unwrap();
        let before = g.sessions.len();
        g.sessions.retain(|(n, _), _| n != name);
        Ok(before - g.sessions.len())
    }
    fn sub_device_sessions(&mut self, name: &str) -> StoreResult<Vec<i32>> {
        Ok(self.0.lock().unwrap().sessions.keys().filter(|(n, _)| n == name).map(|(_, d)| *d).collect())
    }
    fn load_pre_key(&mut self, id: u32) -> StoreResult<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().pre_keys.get(&id).cloned())
    }
    fn store_pre_key(&mut self, id: u32, record: &[u8]) -> StoreResult<()> {
        self.0.lock().unwrap().pre_keys.insert(id, record.to_vec());
        Ok(())
    }
    fn contains_pre_key(&mut self, id: u32) -> StoreResult<bool> {
        Ok(self.0.lock().unwrap().pre_keys.contains_key(&id))
    }
    fn remove_pre_key(&mut self, id: u32) -> StoreResult<()> {
        self.0.lock().unwrap().pre_keys.remove(&id);
        Ok(())
    }
    fn load_signed_pre_key(&mut self, id: u32) -> StoreResult<Option<Vec<u8>>> {
        Ok(self.0.lock().unwrap().signed_pre_keys.get(&id).cloned())
    }
    fn store_signed_pre_key(&mut self, id: u32, record: &[u8]) -> StoreResult<()> {
        self.0.lock().unwrap().signed_pre_keys.insert(id, record.to_vec());
        Ok(())
    }
    fn contains_signed_pre_key(&mut self, id: u32) -> StoreResult<bool> {
        Ok(self.0.lock().unwrap().signed_pre_keys.contains_key(&id))
    }
    fn remove_signed_pre_key(&mut self, id: u32) -> StoreResult<()> {
        self.0.lock().unwrap().signed_pre_keys.remove(&id);
        Ok(())
    }
}
