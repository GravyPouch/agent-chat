//! OMEMO (XEP-0384) for agent-chat, on top of libomemo-c.
//!
//! * [`Context`]: the Double Ratchet session layer for one OMEMO version
//!   (key generation, bundles, per-device encryption of key material).
//! * [`payload`]: the message payload encryption, which differs per version
//!   (legacy: AES-128-GCM; OMEMO:2: HKDF → AES-256-CBC + HMAC-SHA-256).
//! * [`OmemoStore`]: persistence the application provides.
//!
//! XML framing and PEP publishing live in the daemon.

mod context;
mod crypto;
pub mod fingerprint;
pub mod memory;
pub mod payload;
mod store;

pub use context::{Context, Decrypted, OwnBundle, RemoteBundle};
pub use store::{OmemoStore, StoreResult};

use omemo_sys as sys;

/// Which OMEMO version a context speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Version {
    /// XEP-0384 0.3, `eu.siacs.conversations.axolotl`.
    Legacy,
    /// XEP-0384 0.4+, `urn:xmpp:omemo:2`.
    Omemo2,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("message was already decrypted (duplicate)")]
    DuplicateMessage,
    #[error("invalid key")]
    InvalidKey,
    #[error("unknown pre key id (already used?)")]
    InvalidKeyId,
    #[error("message authentication failed")]
    InvalidMac,
    #[error("invalid message")]
    InvalidMessage,
    #[error("unsupported message version")]
    InvalidVersion,
    #[error("no session with this device")]
    NoSession,
    #[error("stale key exchange")]
    StaleKeyExchange,
    #[error("signature verification failed")]
    BadSignature,
    #[error("malformed protobuf")]
    InvalidProtobuf,
    #[error("out of memory")]
    NoMemory,
    #[error("libomemo-c error {0}")]
    Other(i32),
    #[error("{0}")]
    Invalid(&'static str),
}

pub(crate) fn check(rc: std::ffi::c_int) -> Result<std::ffi::c_int, Error> {
    if rc >= 0 {
        return Ok(rc);
    }
    Err(match rc {
        sys::SG_ERR_DUPLICATE_MESSAGE => Error::DuplicateMessage,
        sys::SG_ERR_INVALID_KEY => Error::InvalidKey,
        sys::SG_ERR_INVALID_KEY_ID => Error::InvalidKeyId,
        sys::SG_ERR_INVALID_MAC => Error::InvalidMac,
        sys::SG_ERR_INVALID_MESSAGE => Error::InvalidMessage,
        sys::SG_ERR_INVALID_VERSION | sys::SG_ERR_LEGACY_MESSAGE => Error::InvalidVersion,
        sys::SG_ERR_NO_SESSION => Error::NoSession,
        sys::SG_ERR_STALE_KEY_EXCHANGE => Error::StaleKeyExchange,
        sys::SG_ERR_VRF_SIG_VERIF_FAILED => Error::BadSignature,
        sys::SG_ERR_INVALID_PROTO_BUF => Error::InvalidProtobuf,
        sys::SG_ERR_NOMEM => Error::NoMemory,
        other => Error::Other(other),
    })
}
