//! Counters behind `hub status` (§5.10). A Prometheus endpoint (M7) will
//! export the same values.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default)]
pub struct Stats {
    pub connections: AtomicU64,
    pub connections_total: AtomicU64,
    pub connections_refused: AtomicU64,
    pub sessions: AtomicU64,
    /// Resumable sessions waiting for their client (XEP-0198).
    pub hibernating: AtomicU64,
    pub sm_resumes: AtomicU64,
    pub stanzas_in: AtomicU64,
    pub stanzas_out: AtomicU64,
    pub limit_hits: AtomicU64,
    auth: Mutex<BTreeMap<(String, String), u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCount {
    pub mechanism: String,
    pub result: String,
    pub count: u64,
}

impl Stats {
    pub fn inc(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec(c: &AtomicU64) {
        c.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    /// `auth_total{mechanism, result}`.
    pub fn auth(&self, mechanism: &str, result: &str) {
        *self
            .auth
            .lock()
            .unwrap()
            .entry((mechanism.to_owned(), result.to_owned()))
            .or_default() += 1;
    }

    pub fn auth_counts(&self) -> Vec<AuthCount> {
        self.auth
            .lock()
            .unwrap()
            .iter()
            .map(|((m, r), n)| AuthCount {
                mechanism: m.clone(),
                result: r.clone(),
                count: *n,
            })
            .collect()
    }
}
