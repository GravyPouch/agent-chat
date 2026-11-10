//! agent-chat daemon: XMPP session, message store and local IPC server.

pub mod agent;
pub mod config;
pub mod core;
pub mod daemon;
pub mod omemo;
pub mod register;
pub mod server;
pub mod store;
pub mod web;
pub mod xmpp;

pub use daemon::run;
