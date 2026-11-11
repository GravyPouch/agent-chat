//! Async IPC client used by the CLI and GUI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::ipc::{ClientFrame, Event, IpcError, Request, ServerFrame};
use crate::paths::Profile;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, IpcError>>>>>;

#[derive(Debug)]
pub enum CallError {
    /// The daemon answered with an error.
    Remote(IpcError),
    /// The connection to the daemon was lost.
    Disconnected,
    /// The daemon's answer didn't have the expected shape.
    Decode(serde_json::Error),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Remote(e) => write!(f, "{e}"),
            CallError::Disconnected => write!(f, "lost connection to the agent-chat daemon"),
            CallError::Decode(e) => write!(f, "unexpected response from daemon: {e}"),
        }
    }
}

impl std::error::Error for CallError {}

/// A connection to the daemon. Cheap to share behind an `Arc`; calls may be
/// issued concurrently.
pub struct IpcClient {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    events: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
}

impl IpcClient {
    pub async fn connect(socket: &Path) -> std::io::Result<Self> {
        let stream = UnixStream::connect(socket).await?;
        let (read_half, mut write_half) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();

        tokio::spawn(async move {
            while let Some(mut line) = rx.recv().await {
                line.push('\n');
                if write_half.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(read_half).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(frame) = serde_json::from_str::<ServerFrame>(&line) else {
                    continue;
                };
                match frame {
                    ServerFrame::Response { id, result, error } => {
                        if let Some(waiter) = reader_pending.lock().await.remove(&id) {
                            let outcome = match error {
                                Some(e) => Err(e),
                                None => Ok(result.unwrap_or(Value::Null)),
                            };
                            let _ = waiter.send(outcome);
                        }
                    }
                    ServerFrame::Event { event } => {
                        let _ = events_tx.send(*event);
                    }
                }
            }
            // Connection closed: fail every outstanding call.
            reader_pending.lock().await.clear();
        });

        Ok(Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            events: std::sync::Mutex::new(Some(events_rx)),
        })
    }

    /// Connect to the profile's daemon, starting it first if it isn't running.
    pub async fn connect_or_spawn(profile: &Profile, launcher: &DaemonLauncher) -> Result<Self> {
        let socket = profile.socket_path();
        if let Ok(c) = Self::connect(&socket).await {
            return Ok(c);
        }
        launcher.spawn(profile)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            match Self::connect(&socket).await {
                Ok(c) => return Ok(c),
                Err(e) if tokio::time::Instant::now() >= deadline => {
                    return Err(e)
                        .with_context(|| format!("daemon did not start; see {}", profile.log_path().display()));
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    pub async fn call_value(&self, request: Request) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let line = serde_json::to_string(&ClientFrame { id, request }).map_err(CallError::Decode)?;
        if self.tx.send(line).is_err() {
            self.pending.lock().await.remove(&id);
            return Err(CallError::Disconnected);
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(CallError::Remote(e)),
            Err(_) => Err(CallError::Disconnected),
        }
    }

    pub async fn call<T: DeserializeOwned>(&self, request: Request) -> Result<T, CallError> {
        let v = self.call_value(request).await?;
        serde_json::from_value(v).map_err(CallError::Decode)
    }

    /// Subscribe to daemon events. Can only be called once per connection.
    pub async fn subscribe(&self) -> Result<mpsc::UnboundedReceiver<Event>, CallError> {
        let rx = self.events.lock().unwrap().take().ok_or(CallError::Disconnected)?;
        self.call_value(Request::Subscribe).await?;
        Ok(rx)
    }
}

/// How to start the daemon when it isn't running.
#[derive(Debug, Clone)]
pub struct DaemonLauncher {
    /// Path to the `agent-chat` CLI binary (which hosts `daemon run`).
    pub program: PathBuf,
}

impl DaemonLauncher {
    /// Use the given `agent-chat` binary.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// Find `agent-chat` next to the current executable, then on `PATH`.
    pub fn locate() -> Result<Self> {
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            let candidate = dir.join("agent-chat");
            if candidate.is_file() {
                return Ok(Self::new(candidate));
            }
        }
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let candidate = dir.join("agent-chat");
                if candidate.is_file() {
                    return Ok(Self::new(candidate));
                }
            }
        }
        bail!("could not find the `agent-chat` binary next to this program or on PATH")
    }

    /// Start `agent-chat --profile NAME daemon run` detached from this process.
    pub fn spawn(&self, profile: &Profile) -> Result<()> {
        profile.ensure_dir()?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(profile.log_path())
            .with_context(|| format!("opening {}", profile.log_path().display()))?;
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(["--profile", &profile.name, "daemon", "run"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group: survives the terminal/agent shell that started it.
            cmd.process_group(0);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", self.program.display()))?;
        // Reap the child if it exits while we're still running.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}
