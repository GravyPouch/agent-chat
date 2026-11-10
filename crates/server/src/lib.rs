//! agent-chat hub: a small XMPP server for people and AI agents on one
//! machine or a LAN. See `docs/design/embedded-server.md`.
//!
//! M1 (this code): streams, STARTTLS with a self-signed certificate,
//! SCRAM-SHA-1/256 with `-PLUS` (`tls-exporter`), PLAIN under TLS, resource
//! binding, 1:1 routing, ping and disco on the domain, the admin socket.
//!
//! The library has no dependency on the client daemon or OMEMO.

pub mod accounts;
pub mod admin;
pub mod c2s;
pub mod config;
pub mod disco;
pub mod forms;
pub mod http;
pub mod hub;
pub mod import;
pub mod mam;
pub mod muc;
pub mod net;
pub mod paths;
pub mod register;
pub mod router;
pub mod stanza;
pub mod stats;
pub mod store;
pub mod user;
pub mod util;

pub use config::Config;
pub use hub::{InitOptions, InitReport, RunningHub, RuntimeInfo, init, run};
pub use paths::HubPaths;

/// Set up `tracing` for `hub run`: stderr, the configured level unless
/// `RUST_LOG` is set, text or JSON.
pub fn init_logging(cfg: &config::Log) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("{},tokio_xmpp=warn", cfg.level)));
    use std::io::IsTerminal;
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        // No colour codes in hub.log when detached.
        .with_ansi(std::io::stderr().is_terminal());
    let _ = match cfg.format {
        config::LogFormat::Json => builder.json().try_init(),
        config::LogFormat::Text => builder.try_init(),
    };
}
