//! Hub lifecycle: `init`, start (lock, config, TLS, store, listeners, admin
//! socket, `hub.json`), and graceful shutdown (§5.2, §5.3 rule 6).

use std::fs::File;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::c2s::sasl::AuthThrottle;
use crate::config::{self, Config, TlsMode};
use crate::net::listener::{self, Admission};
use crate::net::tls;
use crate::paths::{HubPaths, write_private};
use crate::router::Router;
use crate::stats::Stats;
use crate::store::{SqliteStore, Store};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared, read-mostly state of a running hub.
pub struct HubCtx {
    pub config: Config,
    pub paths: HubPaths,
    pub domain: String,
    pub store: Arc<dyn Store>,
    pub tls: tls::Reloadable,
    pub router: Arc<Router>,
    pub stats: Stats,
    pub throttle: AuthThrottle,
    pub admission: Arc<Admission>,
    /// Resumable sessions (XEP-0198).
    pub sm: crate::c2s::sm::Registry,
    /// In-band and web registration.
    pub registrar: Arc<crate::register::Registrar>,
    /// Key for fake SCRAM salts (`meta.scram_mock_key`).
    pub mock_key: Vec<u8>,
    pub shutdown: CancellationToken,
    pub tracker: TaskTracker,
    pub started_at: DateTime<Utc>,
    pub started: Instant,
    pub listeners: Vec<SocketAddr>,
    next_conn: AtomicU64,
}

impl HubCtx {
    pub fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }
}

/// `hub.json`: what clients need to find and trust the hub (§7.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub pid: u32,
    pub version: String,
    pub domain: String,
    /// Bound client listeners (STARTTLS).
    pub c2s: Vec<SocketAddr>,
    pub tls_mode: TlsMode,
    /// `sha256/<base64>` of the certificate's public key.
    pub pin: Option<String>,
    pub admin_socket: String,
    /// The onboarding page's listeners (HTTP).
    #[serde(default)]
    pub http: Vec<SocketAddr>,
    pub started_at: DateTime<Utc>,
}

impl RuntimeInfo {
    pub fn read(paths: &HubPaths) -> Result<Option<Self>> {
        match std::fs::read_to_string(paths.runtime()) {
            Ok(s) => Ok(Some(serde_json::from_str(&s).context("parsing hub.json")?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading hub.json"),
        }
    }

    /// A loopback listener a local client can use.
    pub fn loopback_c2s(&self) -> Option<SocketAddr> {
        self.c2s
            .iter()
            .find(|a| a.ip().is_loopback() && a.is_ipv4())
            .or_else(|| self.c2s.iter().find(|a| a.ip().is_loopback()))
            .copied()
            .or_else(|| {
                // 0.0.0.0 also answers on 127.0.0.1.
                self.c2s
                    .iter()
                    .find(|a| a.ip().is_unspecified() && a.is_ipv4())
                    .map(|a| SocketAddr::from(([127, 0, 0, 1], a.port())))
            })
    }
}

// ------------------------------------------------------------------ init

#[derive(Debug, Clone, Default)]
pub struct InitOptions {
    /// `None`: `localhost`, or `<hostname>.local` with `lan`.
    pub domain: Option<String>,
    /// LAN listener (e.g. `0.0.0.0:5222`); also listens on loopback.
    pub lan: Option<String>,
    /// Explicit listeners; overrides the defaults and `lan`.
    pub listen: Vec<String>,
    pub tls_mode: Option<TlsMode>,
    /// A public hub: all interfaces, certificate files, PoW registration.
    pub public: bool,
    /// Certificate and key files (`tls.mode = "files"`).
    pub cert: Option<std::path::PathBuf>,
    pub key: Option<std::path::PathBuf>,
    /// The onboarding page's address (e.g. `0.0.0.0:5280`).
    pub http: Option<String>,
    /// Offer sign-up with in-browser proof of work on that page.
    pub web_signup: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InitReport {
    pub dir: String,
    pub config: String,
    pub domain: String,
    pub listen: Vec<String>,
    pub tls_mode: TlsMode,
    pub pin: Option<String>,
}

/// Write `hub.toml`, create TLS material and `hub.db`. Refuses if the hub
/// is already initialized, and never changes an existing hub's domain.
pub async fn init(paths: &HubPaths, opts: InitOptions) -> Result<InitReport> {
    if paths.config().exists() {
        let existing = Config::load(paths.config())?;
        if let Some(d) = &opts.domain
            && config::normalize_domain(d)? != existing.domain
        {
            bail!(
                "this hub already serves {} and its domain is permanent; \
                 use another AGENT_CHAT_HOME for a hub with another domain",
                existing.domain
            );
        }
        bail!(
            "the hub is already initialized ({}, domain {})",
            paths.config().display(),
            existing.domain
        );
    }
    let domain = match (&opts.domain, &opts.lan) {
        (Some(d), _) => config::normalize_domain(d)?,
        (None, Some(_)) => config::lan_domain(),
        (None, None) => config::LOCAL_DOMAIN.to_owned(),
    };
    let mut cfg = Config::new(&domain);
    if !opts.listen.is_empty() {
        cfg.listen.c2s = opts.listen.clone();
    } else if let Some(lan) = &opts.lan {
        let addr = config::parse_listen_addr(lan)?;
        let mut c2s = vec![addr.to_string()];
        let port = addr.port();
        if !(addr.ip().is_unspecified() && addr.is_ipv4()) {
            c2s.push(format!("127.0.0.1:{port}"));
        }
        if !(addr.ip().is_unspecified() && addr.is_ipv6()) {
            c2s.push(format!("[::1]:{port}"));
        }
        cfg.listen.c2s = c2s;
    }
    if domain == config::LOCAL_DOMAIN && cfg.is_lan() {
        bail!(
            "`localhost` only works on this machine; give the LAN hub a name with --domain NAME.local (or omit --domain)"
        );
    }
    if let Some(m) = opts.tls_mode {
        cfg.tls.mode = m;
    }
    if opts.public {
        if opts.domain.is_none() || domain == config::LOCAL_DOMAIN || domain.ends_with(".local") {
            bail!("a public hub needs its real domain: hub init --public --domain chat.example.org");
        }
        if opts.listen.is_empty() {
            cfg.listen.c2s = vec![
                format!("0.0.0.0:{}", config::DEFAULT_PORT),
                format!("[::]:{}", config::DEFAULT_PORT),
            ];
        }
        cfg.tls.mode = TlsMode::Files;
        cfg.registration.mode = Some(config::RegistrationMode::Pow);
        if opts.http.is_none() {
            cfg.http.listen = vec!["0.0.0.0:5280".into()];
        }
    }
    if let Some(c) = opts.cert {
        cfg.tls.cert = c;
    }
    if let Some(k) = opts.key {
        cfg.tls.key = k;
    }
    if let Some(h) = &opts.http {
        cfg.http.listen = vec![h.clone()];
    }
    cfg.registration.web = opts.web_signup;
    cfg.validate()?;

    paths.ensure_dir()?;
    // Fails if an existing database belongs to another domain.
    let store = SqliteStore::open(&paths.db())?;
    store.ensure_host(&domain).await?;
    drop(store);

    let (cert, key) = (paths.resolve_file(&cfg.tls.cert), paths.resolve_file(&cfg.tls.key));
    let pin = match cfg.tls.mode {
        TlsMode::SelfSigned => {
            tls::ensure_self_signed(&cert, &key, &cfg.certificate_names())?;
            Some(tls::load(&cert, &key)?.pin)
        }
        TlsMode::Files => tls::load(&cert, &key).ok().map(|m| m.pin),
        TlsMode::LocalCa => None,
    };
    write_private(paths.config(), cfg.to_toml()?.as_bytes())?;
    Ok(InitReport {
        dir: paths.dir.display().to_string(),
        config: paths.config().display().to_string(),
        domain,
        listen: cfg.listen.c2s.clone(),
        tls_mode: cfg.tls.mode,
        pin,
    })
}

// --------------------------------------------------------------- running

/// A started hub. Call [`RunningHub::wait`] (or `shutdown`) to finish it.
pub struct RunningHub {
    pub ctx: Arc<HubCtx>,
    lock: Option<File>,
}

impl RunningHub {
    /// Start the hub in `paths.dir` (it must be initialized).
    pub async fn start(paths: HubPaths) -> Result<Self> {
        paths.ensure_dir()?;
        let lock = File::create(paths.lock()).with_context(|| format!("creating {}", paths.lock().display()))?;
        if lock.try_lock().is_err() {
            bail!("a hub is already running in {}", paths.dir.display());
        }
        if !paths.config().exists() {
            bail!("the hub is not initialized; run `agent-chat hub init` first");
        }
        let config = Config::load(paths.config())?;
        let domain = config.domain.clone();

        let (cert, key) = (
            paths.resolve_file(&config.tls.cert),
            paths.resolve_file(&config.tls.key),
        );
        let names = config.certificate_names();
        let tls = match config.tls.mode {
            TlsMode::SelfSigned => {
                tls::ensure_self_signed(&cert, &key, &names)?;
                tls::Reloadable::open(&cert, &key, None)
            }
            // Operator files (e.g. a shared /certs volume): until they exist,
            // a temporary self-signed certificate, like server/deploy does.
            _ => tls::Reloadable::open(
                &cert,
                &key,
                Some((
                    &paths.dir.join("tls/fallback-cert.pem"),
                    &paths.dir.join("tls/fallback-key.pem"),
                    &names,
                )),
            ),
        }
        .context("loading the TLS certificate")?;

        let store: Arc<dyn Store> = Arc::new(SqliteStore::open(&paths.db())?);
        store.ensure_host(&domain).await?;
        let mock_key = store.secret("scram_mock_key", 32).await?;
        // Persisted, so challenges survive a restart (§5.8 `meta`).
        let pow_secret = store.secret("pow_secret", 32).await?;
        let registrar = Arc::new(crate::register::Registrar::new(
            &domain,
            store.clone(),
            config.registration.clone(),
            config.registration_mode(),
            config.auth.scram_iterations,
            pow_secret,
        ));

        let mut bound = Vec::new();
        for addr in config.c2s_addrs()? {
            match listener::bind(addr) {
                Ok(l) => bound.push(l),
                // IPv6 loopback may be missing (containers): skip it.
                Err(e) if addr.is_ipv6() && addr.ip().is_loopback() => {
                    tracing::warn!("not listening on {addr}: {e:#}");
                }
                Err(e) => return Err(e),
            }
        }
        if bound.is_empty() {
            bail!("no listener could be bound");
        }
        let listeners: Vec<SocketAddr> = bound.iter().filter_map(|l| l.local_addr().ok()).collect();

        let socket_path = paths.socket();
        if socket_path.exists() {
            if UnixStream::connect(&socket_path).await.is_ok() {
                bail!("a hub is already listening on {}", socket_path.display());
            }
            std::fs::remove_file(&socket_path).ok();
        }
        let admin = UnixListener::bind(&socket_path).with_context(|| format!("binding {}", socket_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        }

        let limits = &config.limits;
        let ctx = Arc::new(HubCtx {
            router: Router::new(
                &domain,
                store.clone(),
                crate::router::Settings {
                    max_resources: limits.max_resources_per_account,
                    offline_cap: config.archive.max_offline_messages,
                    archive_enabled: config.archive.enabled,
                    mam_max_page: config.archive.max_page,
                    scram_iterations: config.auth.scram_iterations,
                    min_password: config.registration.min_password_length,
                },
            ),
            throttle: AuthThrottle::new(config.auth.max_failures_per_10min),
            admission: Admission::new(limits.max_connections_per_ip, limits.max_new_connections_per_ip_per_min),
            stats: Stats::default(),
            sm: Default::default(),
            registrar,
            mock_key,
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
            started_at: Utc::now(),
            started: Instant::now(),
            listeners: listeners.clone(),
            next_conn: AtomicU64::new(1),
            tls,
            store,
            domain,
            paths: paths.clone(),
            config,
        });

        if let Some(host) = ctx.config.muc_host() {
            let m = &ctx.config.muc;
            let muc = crate::muc::MucService::new(
                &host,
                &ctx.router,
                crate::muc::Settings {
                    admins_only: m.room_creation == "admins",
                    default_persistent: m.default_persistent,
                    archive: m.archive,
                    default_history: m.default_history,
                    max_occupants: m.max_occupants,
                    mam_max_page: ctx.config.archive.max_page,
                },
            );
            ctx.router.set_component(&host, muc);
        }
        for l in bound {
            let addr = l.local_addr()?;
            tracing::info!(%addr, domain = %ctx.domain, "listening for clients");
            ctx.tracker.spawn(accept_loop(ctx.clone(), l));
        }
        let mut http = Vec::new();
        for addr in ctx.config.http_addrs()? {
            let l = listener::bind(addr)?;
            let bound = l.local_addr()?;
            tracing::info!(addr = %bound, "serving the web page");
            http.push(bound);
            ctx.tracker.spawn(crate::http::serve(ctx.clone(), l));
        }
        ctx.tracker.spawn(watch_certificate(ctx.clone()));
        if let (true, Some(keep)) = (ctx.config.archive.enabled, ctx.config.archive.expire_after.0) {
            ctx.tracker.spawn(expire_archives(ctx.clone(), keep));
        }

        let info = RuntimeInfo {
            pid: std::process::id(),
            version: VERSION.into(),
            domain: ctx.domain.clone(),
            c2s: listeners,
            tls_mode: ctx.config.tls.mode,
            pin: Some(ctx.tls.current().pin.clone()),
            admin_socket: socket_path.display().to_string(),
            http,
            started_at: ctx.started_at,
        };
        write_private(&paths.runtime(), serde_json::to_string_pretty(&info)?.as_bytes())?;
        // Last: `hub start` and `account add --hub` treat an answering admin
        // socket as "running" and then read hub.json, so it must exist first.
        ctx.tracker.spawn(crate::admin::serve(ctx.clone(), admin));
        Ok(Self { ctx, lock: Some(lock) })
    }

    pub fn listeners(&self) -> &[SocketAddr] {
        &self.ctx.listeners
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.ctx.shutdown.clone()
    }

    /// Wait until shutdown is requested, then stop: listeners close,
    /// sessions get `<system-shutdown/>`, `hub.json` and the socket go away.
    pub async fn wait(mut self) {
        self.ctx.shutdown.cancelled().await;
        tracing::info!("shutting down");
        self.ctx.tracker.close();
        if tokio::time::timeout(Duration::from_secs(5), self.ctx.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("some connections did not close in time");
        }
        std::fs::remove_file(self.ctx.paths.runtime()).ok();
        std::fs::remove_file(self.ctx.paths.socket()).ok();
        drop(self.lock.take());
        tracing::info!("hub stopped");
    }

    pub async fn shutdown(self) {
        self.ctx.shutdown.cancel();
        self.wait().await;
    }
}

/// Pick up renewed certificate files: poll every `tls.reload_interval`
/// (files mode) and reload on SIGHUP.
async fn watch_certificate(ctx: Arc<HubCtx>) {
    let interval = ctx.config.tls.reload_interval.0;
    let poll = ctx.config.tls.mode == TlsMode::Files;
    #[cfg(unix)]
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    loop {
        #[cfg(unix)]
        let sighup = async {
            match hup.as_mut() {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending().await,
            }
        };
        #[cfg(not(unix))]
        let sighup = std::future::pending::<()>();
        tokio::select! {
            _ = tokio::time::sleep(interval), if poll => {}
            _ = sighup => tracing::info!("SIGHUP: checking the certificate"),
            _ = ctx.shutdown.cancelled() => return,
        }
        if let Err(e) = ctx.tls.reload_if_changed() {
            tracing::warn!("the new certificate files can't be used yet: {e:#}");
        }
    }
}

/// Delete archived and offline messages older than `keep`, at start and
/// then hourly (§5.7 "Preferences and retention").
async fn expire_archives(ctx: Arc<HubCtx>, keep: Duration) {
    loop {
        let before = crate::util::now_ms() - keep.as_millis() as i64;
        match ctx.store.archive_expire(before).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(deleted = n, "expired archived messages"),
            Err(e) => tracing::warn!("expiring archives failed: {e:#}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
            _ = ctx.shutdown.cancelled() => return,
        }
    }
}

async fn accept_loop(ctx: Arc<HubCtx>, l: tokio::net::TcpListener) {
    loop {
        let accepted = tokio::select! {
            r = l.accept() => r,
            _ = ctx.shutdown.cancelled() => return,
        };
        match accepted {
            Ok((_, peer))
                if Stats::get(&ctx.stats.connections) >= ctx.config.limits.max_connections
                    && !crate::util::is_loopback(peer.ip()) =>
            {
                Stats::inc(&ctx.stats.connections_refused);
                Stats::inc(&ctx.stats.limit_hits);
                tracing::warn!("connection refused: the hub is at limits.max_connections");
            }
            Ok((tcp, peer)) => match ctx.admission.admit(peer.ip()) {
                Ok(guard) => {
                    ctx.tracker.spawn(crate::c2s::serve(ctx.clone(), tcp, peer, guard));
                }
                Err(refusal) => {
                    Stats::inc(&ctx.stats.connections_refused);
                    Stats::inc(&ctx.stats.limit_hits);
                    tracing::info!(
                        peer = %crate::util::ip_prefix(peer.ip()),
                        reason = refusal.as_str(),
                        "connection refused"
                    );
                }
            },
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// `hub run`: start, then stop on Ctrl-C, SIGTERM or an admin `shutdown`.
pub async fn run(paths: HubPaths) -> Result<()> {
    let hub = RunningHub::start(paths).await?;
    let token = hub.shutdown_token();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => return,
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
                _ = term.recv() => tracing::info!("terminated"),
                _ = token.cancelled() => return,
            }
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("interrupted"),
            _ = token.cancelled() => return,
        }
        token.cancel();
    });
    hub.wait().await;
    Ok(())
}
