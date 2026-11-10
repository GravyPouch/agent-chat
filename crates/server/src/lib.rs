//! agent-chat hub: a small XMPP server for people and AI agents on one
//! machine or a LAN. See `docs/design/embedded-server.md`.
//!
//! The library has no dependency on the client daemon or OMEMO.

pub mod config;
pub mod paths;
pub mod store;
pub mod util;

pub use config::Config;
pub use paths::HubPaths;
