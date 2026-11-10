//! SASL mechanisms, what to offer on a channel, fake salts and the
//! authentication-failure throttle (§5.4 step 6, §6.3).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use xmpp_parsers::sasl::DefinedCondition;

pub mod plain;
pub mod scram;

use scram::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mechanism {
    ScramSha256Plus,
    ScramSha1Plus,
    ScramSha256,
    ScramSha1,
    Plain,
}

impl Mechanism {
    pub const ALL: [Mechanism; 5] = [
        Mechanism::ScramSha256Plus,
        Mechanism::ScramSha1Plus,
        Mechanism::ScramSha256,
        Mechanism::ScramSha1,
        Mechanism::Plain,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Mechanism::ScramSha256Plus => "SCRAM-SHA-256-PLUS",
            Mechanism::ScramSha1Plus => "SCRAM-SHA-1-PLUS",
            Mechanism::ScramSha256 => "SCRAM-SHA-256",
            Mechanism::ScramSha1 => "SCRAM-SHA-1",
            Mechanism::Plain => "PLAIN",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }

    /// The SCRAM hash, or `None` for PLAIN.
    pub fn hash(self) -> Option<Hash> {
        match self {
            Mechanism::ScramSha256Plus | Mechanism::ScramSha256 => Some(Hash::Sha256),
            Mechanism::ScramSha1Plus | Mechanism::ScramSha1 => Some(Hash::Sha1),
            Mechanism::Plain => None,
        }
    }

    pub fn is_plus(self) -> bool {
        matches!(self, Mechanism::ScramSha256Plus | Mechanism::ScramSha1Plus)
    }
}

/// What the channel can carry.
#[derive(Debug, Clone, Copy)]
pub struct Channel {
    /// TLS is active.
    pub tls: bool,
    /// We have `tls-exporter` data (TLS 1.3).
    pub channel_binding: bool,
    /// Plaintext SCRAM is allowed (loopback peer and `allow_plaintext_loopback`).
    pub plaintext_scram: bool,
}

/// Mechanisms to offer, in the configured order, filtered by the channel:
/// `-PLUS` only with channel binding, plain SCRAM under TLS or on allowed
/// loopback plaintext, PLAIN only under TLS.
pub fn offered(configured: &[String], ch: Channel) -> Vec<Mechanism> {
    configured
        .iter()
        .filter_map(|n| Mechanism::from_name(n))
        .filter(|m| match m {
            m if m.is_plus() => ch.tls && ch.channel_binding,
            Mechanism::Plain => ch.tls,
            _ => ch.tls || ch.plaintext_scram,
        })
        .collect()
}

/// SASL failure conditions we send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Aborted,
    AccountDisabled,
    EncryptionRequired,
    IncorrectEncoding,
    InvalidAuthzid,
    InvalidMechanism,
    MalformedRequest,
    NotAuthorized,
    TemporaryAuthFailure,
}

impl Failure {
    pub fn condition(self) -> DefinedCondition {
        match self {
            Failure::Aborted => DefinedCondition::Aborted,
            Failure::AccountDisabled => DefinedCondition::AccountDisabled,
            Failure::EncryptionRequired => DefinedCondition::EncryptionRequired,
            Failure::IncorrectEncoding => DefinedCondition::IncorrectEncoding,
            Failure::InvalidAuthzid => DefinedCondition::InvalidAuthzid,
            Failure::InvalidMechanism => DefinedCondition::InvalidMechanism,
            Failure::MalformedRequest => DefinedCondition::MalformedRequest,
            Failure::NotAuthorized => DefinedCondition::NotAuthorized,
            Failure::TemporaryAuthFailure => DefinedCondition::TemporaryAuthFailure,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Failure::Aborted => "aborted",
            Failure::AccountDisabled => "account-disabled",
            Failure::EncryptionRequired => "encryption-required",
            Failure::IncorrectEncoding => "incorrect-encoding",
            Failure::InvalidAuthzid => "invalid-authzid",
            Failure::InvalidMechanism => "invalid-mechanism",
            Failure::MalformedRequest => "malformed-request",
            Failure::NotAuthorized => "not-authorized",
            Failure::TemporaryAuthFailure => "temporary-auth-failure",
        }
    }

    /// Whether this counts as a failed credential check for the throttle.
    pub fn counts_as_guess(self) -> bool {
        matches!(self, Failure::NotAuthorized)
    }
}

/// Salt shown for unknown users: HMAC-SHA-256(mock key, domain ‖ 0 ‖ user)[..16].
/// Deterministic, so repeated probes see a stable salt like a real account.
pub fn fake_salt(mock_key: &[u8], domain: &str, username: &str) -> Vec<u8> {
    let mut data = domain.as_bytes().to_vec();
    data.push(0);
    data.extend_from_slice(username.as_bytes());
    Hash::Sha256.hmac(mock_key, &data)[..16].to_vec()
}

/// Per-key failure counter with exponential backoff (§6.3): after `max`
/// failures in 10 minutes, attempts are refused for 1 s, 2 s, 4 s, …, up
/// to 15 minutes.
pub struct AuthThrottle {
    max: u32,
    state: Mutex<HashMap<String, ThrottleEntry>>,
}

#[derive(Default)]
struct ThrottleEntry {
    failures: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

const WINDOW: Duration = Duration::from_secs(600);
const MAX_BACKOFF: Duration = Duration::from_secs(900);

impl AuthThrottle {
    pub fn new(max_failures_per_10min: u32) -> Self {
        Self {
            max: max_failures_per_10min.max(1),
            state: Mutex::default(),
        }
    }

    /// `false` while any of the keys is in backoff.
    pub fn allowed(&self, keys: &[String]) -> bool {
        let now = Instant::now();
        let map = self.state.lock().unwrap();
        keys.iter()
            .all(|k| map.get(k).and_then(|e| e.blocked_until).is_none_or(|t| t <= now))
    }

    pub fn failure(&self, keys: &[String]) {
        let now = Instant::now();
        let mut map = self.state.lock().unwrap();
        if map.len() > 16_384 {
            map.retain(|_, e| e.failures.back().is_some_and(|t| now.duration_since(*t) < WINDOW));
        }
        for k in keys {
            let e = map.entry(k.clone()).or_default();
            while e.failures.front().is_some_and(|t| now.duration_since(*t) >= WINDOW) {
                e.failures.pop_front();
            }
            e.failures.push_back(now);
            let n = e.failures.len() as u32;
            if n >= self.max {
                let exp = (n - self.max).min(10);
                let backoff = Duration::from_secs(1u64 << exp).min(MAX_BACKOFF);
                e.blocked_until = Some(now + backoff);
            }
        }
    }

    pub fn success(&self, keys: &[String]) {
        let mut map = self.state.lock().unwrap();
        for k in keys {
            map.remove(k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: Vec<Mechanism>) -> Vec<&'static str> {
        v.into_iter().map(|m| m.name()).collect()
    }

    #[test]
    fn offers_depend_on_the_channel() {
        let all: Vec<String> = crate::config::KNOWN_MECHANISMS.iter().map(|s| s.to_string()).collect();
        let tls13 = Channel {
            tls: true,
            channel_binding: true,
            plaintext_scram: false,
        };
        assert_eq!(
            names(offered(&all, tls13)),
            [
                "SCRAM-SHA-256-PLUS",
                "SCRAM-SHA-1-PLUS",
                "SCRAM-SHA-256",
                "SCRAM-SHA-1",
                "PLAIN"
            ]
        );
        let tls12 = Channel {
            tls: true,
            channel_binding: false,
            plaintext_scram: false,
        };
        assert_eq!(names(offered(&all, tls12)), ["SCRAM-SHA-256", "SCRAM-SHA-1", "PLAIN"]);
        let loopback = Channel {
            tls: false,
            channel_binding: false,
            plaintext_scram: true,
        };
        assert_eq!(names(offered(&all, loopback)), ["SCRAM-SHA-256", "SCRAM-SHA-1"]);
        let lan_plain = Channel {
            tls: false,
            channel_binding: false,
            plaintext_scram: false,
        };
        assert!(offered(&all, lan_plain).is_empty());
    }

    #[test]
    fn fake_salts_are_stable_and_distinct() {
        let a = fake_salt(b"key", "hub.local", "mallory");
        assert_eq!(a.len(), 16);
        assert_eq!(a, fake_salt(b"key", "hub.local", "mallory"));
        assert_ne!(a, fake_salt(b"key", "hub.local", "trudy"));
        assert_ne!(a, fake_salt(b"other", "hub.local", "mallory"));
    }

    #[test]
    fn throttle_backs_off_after_max_failures() {
        let t = AuthThrottle::new(3);
        let keys = vec!["acct:alice".to_owned()];
        t.failure(&keys);
        t.failure(&keys);
        assert!(t.allowed(&keys));
        t.failure(&keys);
        assert!(!t.allowed(&keys), "third failure starts the backoff");
        assert!(t.allowed(&["acct:bob".to_owned()]));
        t.success(&keys);
        assert!(t.allowed(&keys));
    }
}
