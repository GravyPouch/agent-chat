//! Daemon entry point.

use std::sync::Arc;

use agent_chat_proto::Profile;
use anyhow::{Context, Result, bail};
use tokio::net::{UnixListener, UnixStream};

use crate::config::Config;
use crate::core::Core;
use crate::store::Store;

/// Run the daemon for a profile until it is told to stop.
pub async fn run(profile: Profile) -> Result<()> {
    profile.ensure_dir()?;

    // One daemon per profile.
    let lock = std::fs::File::create(profile.lock_path())
        .with_context(|| format!("creating {}", profile.lock_path().display()))?;
    if lock.try_lock().is_err() {
        bail!("a daemon is already running for profile {:?}", profile.name);
    }

    let socket = profile.socket_path();
    if socket.exists() {
        if UnixStream::connect(&socket).await.is_ok() {
            bail!("a daemon is already listening on {}", socket.display());
        }
        std::fs::remove_file(&socket).ok();
    }

    let _ = tokio_xmpp::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let config = Config::load(&profile.config_path())?;
    let store = Store::open(&profile.db_path())?;
    let core: Arc<Core> = Core::new(profile.clone(), store, config);

    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(profile = %profile.name, socket = %socket.display(), "daemon started");

    let http_port = core.config.lock().unwrap().settings.http_port;
    if let Some(url) = crate::web::start(core.clone(), http_port).await {
        tracing::info!("setup page: {url}");
        core.state().status.web_url = Some(url);
    }

    if let Err(e) = core.restart_session().await {
        tracing::warn!("not connecting: {e:#}");
    }

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = crate::server::serve(core.clone(), listener) => {}
        _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
        _ = sigterm.recv() => tracing::info!("terminated"),
        _ = core.shutdown.notified() => tracing::info!("shutdown requested"),
    }

    core.stop_session().await;
    std::fs::remove_file(&socket).ok();
    drop(lock);
    tracing::info!("daemon stopped");
    Ok(())
}
