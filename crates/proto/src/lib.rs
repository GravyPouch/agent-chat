//! Shared types, IPC protocol and client for agent-chat.
//!
//! This crate is deliberately light (no XMPP, no SQLite) so the GUI can
//! depend on it without pulling in the daemon.

pub mod client;
pub mod instructions;
pub mod ipc;
pub mod model;
pub mod paths;
pub mod phone;
pub mod rooms;

pub use client::{CallError, DaemonLauncher, IpcClient};
pub use paths::Profile;
