//! The admin socket (`hub.sock`, mode 0600): newline-delimited JSON with the
//! same framing as the daemon IPC, `{"id":1,"request":{...}}` →
//! `{"id":1,"result":...}` or `{"id":1,"error":{...}}` (§5.2, §6.5).
//!
//! These types are the admin API; a GUI or an HTTP provisioning API can
//! reuse them later.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use xmpp_parsers::jid::BareJid;

use crate::accounts::{self, AccountError, CreateRequest};
use crate::config::TlsMode;
use crate::hub::{HubCtx, VERSION};
use crate::stats::{AuthCount, Stats};
use crate::store::{Account, AccountState, CreatedVia, Role};
use crate::user::CloseReason;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AdminRequest {
    Status,
    UserAdd {
        username: String,
        /// `None`: generate one (returned once).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        #[serde(default)]
        admin: bool,
    },
    UserDel {
        username: String,
    },
    UserPasswd {
        username: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
    UserList,
    /// Create an XEP-0445 invitation (registration without proof of work).
    InviteCreate {
        #[serde(default = "one")]
        uses: u32,
        /// Seconds until it expires.
        #[serde(default = "week")]
        expires_secs: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    Shutdown,
}

fn one() -> u32 {
    1
}

fn week() -> u64 {
    7 * 86_400
}

/// A new invitation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InviteCreated {
    pub token: String,
    /// `xmpp:domain?register;preauth=TOKEN` (XEP-0401), for stock clients.
    pub uri: String,
    pub uses: u32,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminFrame {
    pub id: u64,
    pub request: AdminRequest,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminResponse {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AdminError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminErrorCode {
    BadRequest,
    Exists,
    NotFound,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminError {
    pub code: AdminErrorCode,
    pub message: String,
}

impl std::fmt::Display for AdminError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AdminError {}

impl From<AccountError> for AdminError {
    fn from(e: AccountError) -> Self {
        let code = match &e {
            AccountError::InvalidUsername(..) | AccountError::WeakPassword(_) | AccountError::InvalidPassword => {
                AdminErrorCode::BadRequest
            }
            AccountError::Exists(_) => AdminErrorCode::Exists,
            AccountError::NotFound(_) => AdminErrorCode::NotFound,
            AccountError::Other(_) => AdminErrorCode::Internal,
        };
        AdminError {
            code,
            message: format!("{e:#}"),
        }
    }
}

fn internal(e: impl std::fmt::Display) -> AdminError {
    AdminError {
        code: AdminErrorCode::Internal,
        message: e.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubStatus {
    pub version: String,
    pub pid: u32,
    pub domain: String,
    pub started_at: DateTime<Utc>,
    pub uptime_secs: u64,
    pub c2s: Vec<String>,
    pub tls_mode: TlsMode,
    pub pin: Option<String>,
    pub connections: u64,
    /// Bound client sessions.
    pub sessions: u64,
    /// Sessions waiting for their client to resume (XEP-0198).
    #[serde(default)]
    pub hibernating: u64,
    #[serde(default)]
    pub sm_resumes: u64,
    pub accounts: u64,
    pub user_actors: usize,
    pub connections_total: u64,
    pub connections_refused: u64,
    pub stanzas_in: u64,
    pub stanzas_out: u64,
    pub limit_hits: u64,
    pub auth: Vec<AuthCount>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserInfo {
    pub jid: String,
    pub role: Role,
    pub state: AccountState,
    pub created_via: CreatedVia,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_login_at: Option<DateTime<Utc>>,
}

impl From<&Account> for UserInfo {
    fn from(a: &Account) -> Self {
        let ts = |ms: i64| Utc.timestamp_millis_opt(ms).single().unwrap_or_default();
        UserInfo {
            jid: a.jid(),
            role: a.role,
            state: a.state,
            created_via: a.created_via,
            created_at: ts(a.created_ms),
            last_login_at: a.last_login_ms.map(ts),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserCreated {
    pub jid: String,
    pub role: Role,
    /// Only when the hub generated the password. Shown once, never stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

// ---------------------------------------------------------------- server

pub async fn serve(ctx: Arc<HubCtx>, listener: UnixListener) {
    loop {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            _ = ctx.shutdown.cancelled() => return,
        };
        match accepted {
            Ok((stream, _)) => {
                tokio::spawn(handle_connection(ctx.clone(), stream));
            }
            Err(e) => {
                tracing::warn!("admin accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(ctx: Arc<HubCtx>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    loop {
        let line = tokio::select! {
            l = lines.next_line() => l,
            _ = ctx.shutdown.cancelled() => return,
        };
        let Ok(Some(line)) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<AdminFrame>(&line) {
            Ok(frame) => {
                let id = frame.id;
                match dispatch(&ctx, frame.request).await {
                    Ok(v) => AdminResponse {
                        id,
                        result: Some(v),
                        error: None,
                    },
                    Err(e) => AdminResponse {
                        id,
                        result: None,
                        error: Some(e),
                    },
                }
            }
            Err(e) => AdminResponse {
                id: 0,
                result: None,
                error: Some(AdminError {
                    code: AdminErrorCode::BadRequest,
                    message: format!("bad request: {e}"),
                }),
            },
        };
        let Ok(mut out) = serde_json::to_string(&response) else {
            return;
        };
        out.push('\n');
        if write.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn json<T: Serialize>(v: T) -> Result<Value, AdminError> {
    serde_json::to_value(v).map_err(internal)
}

async fn dispatch(ctx: &Arc<HubCtx>, req: AdminRequest) -> Result<Value, AdminError> {
    let iterations = ctx.config.auth.scram_iterations;
    match req {
        AdminRequest::Status => json(status(ctx).await?),
        AdminRequest::UserAdd {
            username,
            password,
            admin,
        } => {
            let created = accounts::create(
                &ctx.store,
                &ctx.domain,
                iterations,
                CreateRequest {
                    username,
                    password,
                    role: if admin { Role::Admin } else { Role::User },
                    created_via: CreatedVia::Admin,
                },
            )
            .await?;
            tracing::info!(jid = %created.account.jid(), "account created by the admin");
            json(UserCreated {
                jid: created.account.jid(),
                role: created.account.role,
                password: created.generated_password,
            })
        }
        AdminRequest::UserDel { username } => {
            let localpart = accounts::normalize_username(&username)?;
            if !ctx
                .store
                .delete_account(&ctx.domain, &localpart)
                .await
                .map_err(internal)?
            {
                return Err(AccountError::NotFound(format!("{localpart}@{}", ctx.domain)).into());
            }
            if let Ok(bare) = BareJid::new(&format!("{localpart}@{}", ctx.domain)) {
                ctx.router.close_account(&bare, CloseReason::AccountDeleted);
            }
            tracing::info!(user = %localpart, "account deleted by the admin");
            Ok(Value::Null)
        }
        AdminRequest::UserPasswd { username, password } => {
            let generated = accounts::set_password(&ctx.store, &ctx.domain, iterations, &username, password).await?;
            tracing::info!(user = %username, "password changed by the admin");
            Ok(serde_json::json!({ "password": generated }))
        }
        AdminRequest::UserList => {
            let accounts = ctx.store.list_accounts(&ctx.domain).await.map_err(internal)?;
            json(accounts.iter().map(UserInfo::from).collect::<Vec<_>>())
        }
        AdminRequest::InviteCreate {
            uses,
            expires_secs,
            note,
        } => {
            if uses == 0 || expires_secs == 0 {
                return Err(AdminError {
                    code: AdminErrorCode::BadRequest,
                    message: "uses and expiry must be positive".into(),
                });
            }
            let token = crate::register::new_invite_token();
            let expires_ms = crate::util::now_ms() + (expires_secs.min(365 * 86_400) * 1000) as i64;
            ctx.store
                .invite_put(&crate::register::invite_hash(&token), expires_ms, uses, note)
                .await
                .map_err(internal)?;
            tracing::info!(uses, "invitation created");
            json(InviteCreated {
                uri: format!("xmpp:{}?register;preauth={token}", ctx.domain),
                token,
                uses,
                expires_at: Utc.timestamp_millis_opt(expires_ms).single().unwrap_or_default(),
            })
        }
        AdminRequest::Shutdown => {
            tracing::info!("shutdown requested over the admin socket");
            ctx.shutdown.cancel();
            Ok(Value::Null)
        }
    }
}

pub async fn status(ctx: &HubCtx) -> Result<HubStatus, AdminError> {
    let s = &ctx.stats;
    Ok(HubStatus {
        version: VERSION.into(),
        pid: std::process::id(),
        domain: ctx.domain.clone(),
        started_at: ctx.started_at,
        uptime_secs: ctx.started.elapsed().as_secs(),
        c2s: ctx.listeners.iter().map(|a| a.to_string()).collect(),
        tls_mode: ctx.config.tls.mode,
        pin: Some(ctx.tls.current().pin.clone()),
        connections: Stats::get(&s.connections),
        sessions: Stats::get(&s.sessions),
        hibernating: Stats::get(&s.hibernating),
        sm_resumes: Stats::get(&s.sm_resumes),
        accounts: ctx.store.count_accounts(&ctx.domain).await.map_err(internal)?,
        user_actors: ctx.router.user_actors(),
        connections_total: Stats::get(&s.connections_total),
        connections_refused: Stats::get(&s.connections_refused),
        stanzas_in: Stats::get(&s.stanzas_in),
        stanzas_out: Stats::get(&s.stanzas_out),
        limit_hits: Stats::get(&s.limit_hits),
        auth: s.auth_counts(),
    })
}

// ---------------------------------------------------------------- client

#[derive(Debug, thiserror::Error)]
pub enum AdminCallError {
    #[error("{0}")]
    Remote(AdminError),
    #[error("lost the connection to the hub")]
    Disconnected,
    #[error("unexpected response from the hub: {0}")]
    Decode(serde_json::Error),
}

/// A sequential admin client (the CLI makes one call per process).
pub struct AdminClient {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
}

impl AdminClient {
    pub async fn connect(socket: &Path) -> std::io::Result<Self> {
        let (read, write) = UnixStream::connect(socket).await?.into_split();
        Ok(Self {
            lines: BufReader::new(read).lines(),
            write,
            next_id: 1,
        })
    }

    pub async fn call_value(&mut self, request: AdminRequest) -> Result<Value, AdminCallError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&AdminFrame { id, request }).map_err(AdminCallError::Decode)?;
        line.push('\n');
        self.write
            .write_all(line.as_bytes())
            .await
            .map_err(|_| AdminCallError::Disconnected)?;
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|_| AdminCallError::Disconnected)?
                .ok_or(AdminCallError::Disconnected)?;
            let resp: AdminResponse = serde_json::from_str(&line).map_err(AdminCallError::Decode)?;
            if resp.id != id {
                continue;
            }
            return match resp.error {
                Some(e) => Err(AdminCallError::Remote(e)),
                None => Ok(resp.result.unwrap_or(Value::Null)),
            };
        }
    }

    pub async fn call<T: DeserializeOwned>(&mut self, request: AdminRequest) -> Result<T, AdminCallError> {
        let v = self.call_value(request).await?;
        serde_json::from_value(v).map_err(AdminCallError::Decode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let frame = AdminFrame {
            id: 3,
            request: AdminRequest::UserAdd {
                username: "alice".into(),
                password: None,
                admin: false,
            },
        };
        let s = serde_json::to_string(&frame).unwrap();
        assert_eq!(
            s,
            r#"{"id":3,"request":{"op":"user_add","username":"alice","admin":false}}"#
        );
        let back: AdminFrame = serde_json::from_str(r#"{"id":1,"request":{"op":"status"}}"#).unwrap();
        assert_eq!(back.request, AdminRequest::Status);
    }
}
