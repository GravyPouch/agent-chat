//! IPC server: newline-delimited JSON over a Unix socket.

use std::sync::Arc;
use std::time::Duration;

use agent_chat_proto::ipc::{
    AccountParams, ClientFrame, ContactList, ErrorCode, Event, IpcError, Request, ServerFrame,
};
use agent_chat_proto::model::{Delivery, Filter, Message, PresenceInfo};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinSet;
use xmpp_parsers::jid::BareJid;

use crate::config::{AccountConfig, store_password};
use crate::core::Core;
use crate::xmpp::actions;

pub async fn serve(core: Arc<Core>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_connection(core.clone(), stream));
            }
            Err(e) => {
                tracing::error!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle_connection(core: Arc<Core>, stream: UnixStream) {
    let (read_half, mut write_half) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerFrame>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let Ok(mut line) = serde_json::to_string(&frame) else { continue };
            line.push('\n');
            if write_half.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Requests run concurrently (a long `wait` must not block other calls)
    // and are aborted when the client goes away.
    let mut tasks = JoinSet::new();
    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        while tasks.try_join_next().is_some() {}
        if line.trim().is_empty() {
            continue;
        }
        let frame: ClientFrame = match serde_json::from_str(&line) {
            Ok(f) => f,
            Err(e) => {
                let _ = tx.send(ServerFrame::Response {
                    id: 0,
                    result: None,
                    error: Some(IpcError::new(ErrorCode::BadRequest, format!("bad request: {e}"))),
                });
                continue;
            }
        };
        if frame.request == Request::Subscribe {
            let mut events = core.events.subscribe();
            let etx = tx.clone();
            tasks.spawn(async move {
                loop {
                    match events.recv().await {
                        Ok(event) => {
                            if etx.send(ServerFrame::Event { event }).is_err() {
                                break;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
            let _ = tx.send(ServerFrame::Response {
                id: frame.id,
                result: Some(Value::Null),
                error: None,
            });
            continue;
        }
        let core = core.clone();
        let tx = tx.clone();
        tasks.spawn(async move {
            let (result, error) = match dispatch(&core, frame.request).await {
                Ok(v) => (Some(v), None),
                Err(e) => (None, Some(e)),
            };
            let _ = tx.send(ServerFrame::Response {
                id: frame.id,
                result,
                error,
            });
        });
    }
    tasks.abort_all();
    drop(tx);
    let _ = writer.await;
}

fn internal(e: impl std::fmt::Display) -> IpcError {
    IpcError::new(ErrorCode::Internal, e.to_string())
}

fn json<T: serde::Serialize>(v: T) -> Result<Value, IpcError> {
    serde_json::to_value(v).map_err(internal)
}

async fn dispatch(core: &Arc<Core>, req: Request) -> Result<Value, IpcError> {
    match req {
        Request::Status => json(core.status()),
        Request::SetAccount(params) => json(set_account(core, params).await?),
        Request::Send {
            to,
            body,
            room,
            encryption,
        } => {
            let handle = core.session()?;
            json(actions::send_message(core, &handle, &to, body, room, encryption).await?)
        }
        Request::Inbox {
            delivery,
            filter,
            peek,
            limit,
            require_trusted,
        } => {
            let store = core.store();
            let msgs = store
                .undelivered(&delivery, &filter, require_trusted, limit)
                .map_err(internal)?;
            if !peek {
                for m in &msgs {
                    store.mark_delivered(&delivery.consumer, m.id).map_err(internal)?;
                }
            }
            json(msgs)
        }
        Request::Wait {
            delivery,
            filter,
            timeout_secs,
            require_trusted,
        } => json(wait(core, &delivery, &filter, timeout_secs, require_trusted).await?),
        Request::History {
            conversation,
            limit,
            before_id,
        } => json(
            core.store()
                .history(&conversation, limit.min(1000), before_id)
                .map_err(internal)?,
        ),
        Request::Conversations => json(core.store().conversations().map_err(internal)?),
        Request::MarkRead {
            conversation,
            up_to_id,
        } => {
            core.store().mark_read(&conversation, up_to_id).map_err(internal)?;
            Ok(Value::Null)
        }
        Request::Contacts => json(ContactList {
            contacts: core.contacts(),
            pending_requests: core
                .state()
                .pending_requests
                .iter()
                .map(|jid| agent_chat_proto::model::SubscriptionRequest { jid: jid.clone() })
                .collect(),
        }),
        Request::ContactAdd { jid, name } => {
            actions::add_contact(&core.session()?, &jid, name).await?;
            Ok(Value::Null)
        }
        Request::ContactRemove { jid } => {
            actions::remove_contact(&core.session()?, &jid).await?;
            Ok(Value::Null)
        }
        Request::ContactApprove { jid } => {
            actions::approve_subscription(core, &core.session()?, &jid)?;
            Ok(Value::Null)
        }
        Request::ContactDeny { jid } => {
            actions::deny_subscription(core, &core.session()?, &jid)?;
            Ok(Value::Null)
        }
        Request::Rooms => json(core.rooms().map_err(internal)?),
        Request::RoomJoin {
            jid,
            nick,
            password,
        } => {
            let handle = core.session()?;
            json(actions::join_room(core, &handle, &jid, nick, password).await?)
        }
        Request::RoomLeave { jid } => {
            actions::leave_room(core, &core.session()?, &jid)?;
            Ok(Value::Null)
        }
        Request::SetPresence { show, status } => {
            let info = PresenceInfo { show, status };
            match core.session() {
                Ok(handle) => actions::set_presence(core, &handle, info)?,
                // Remember it for the next login.
                Err(_) => core.state().status.presence = info,
            }
            Ok(Value::Null)
        }
        Request::OmemoDevices { jid } => json(crate::omemo::xmpp::list_devices(core, jid).await?),
        Request::OmemoSetTrust {
            jid,
            fingerprint,
            trusted,
        } => json(crate::omemo::xmpp::set_trust(core, &jid, &fingerprint, trusted).await?),
        Request::Subscribe => Ok(Value::Null),
        Request::Shutdown => {
            core.shutdown.notify_one();
            Ok(Value::Null)
        }
    }
}

/// Block until the consumer has an unread message (or the timeout passes).
async fn wait(
    core: &Arc<Core>,
    delivery: &Delivery,
    filter: &Filter,
    timeout_secs: Option<u64>,
    require_trusted: bool,
) -> Result<Option<Message>, IpcError> {
    // Subscribe before checking the store so nothing slips in between.
    let mut events = core.events.subscribe();
    let deadline = timeout_secs.map(|s| tokio::time::Instant::now() + Duration::from_secs(s));
    loop {
        loop {
            let next = core
                .store()
                .undelivered(delivery, filter, require_trusted, Some(1))
                .map_err(internal)?
                .into_iter()
                .next();
            let Some(m) = next else { break };
            // Another waiter on the same consumer may have taken it.
            if core
                .store()
                .mark_delivered(&delivery.consumer, m.id)
                .map_err(internal)?
            {
                return Ok(Some(m));
            }
        }
        loop {
            let ev = match deadline {
                Some(dl) => match tokio::time::timeout_at(dl, events.recv()).await {
                    Err(_) => return Ok(None),
                    Ok(ev) => ev,
                },
                None => events.recv().await,
            };
            match ev {
                Ok(Event::Message(_)) | Err(broadcast::error::RecvError::Lagged(_)) => break,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(internal("daemon is shutting down"));
                }
            }
        }
    }
}

async fn set_account(core: &Arc<Core>, p: AccountParams) -> Result<agent_chat_proto::model::StatusInfo, IpcError> {
    let bare = BareJid::new(&p.jid)
        .map_err(|e| IpcError::new(ErrorCode::BadRequest, format!("invalid JID {:?}: {e}", p.jid)))?;
    if bare.node().is_none() {
        return Err(IpcError::new(ErrorCode::BadRequest, "account JIDs look like name@server"));
    }
    let jid = bare.to_string();
    let mut cfg = core.config.lock().unwrap().clone();
    let previous = cfg.account.as_ref().map(|a| (a.jid.clone(), a.resource.clone()));
    if let Some((old, _)) = &previous
        && !old.eq_ignore_ascii_case(&jid)
        && core.store().max_message_id().map_err(internal)? > 0
    {
        return Err(IpcError::new(
            ErrorCode::BadRequest,
            format!(
                "profile {:?} already holds messages for {old}; use `--profile NAME` for another account",
                core.profile.name
            ),
        ));
    }
    let resource = match previous {
        Some((old, res)) if old.eq_ignore_ascii_case(&jid) => res,
        _ => format!("agent-chat-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
    };
    let password = store_password(&jid, &p.password, p.password_storage)
        .map_err(|e| IpcError::new(ErrorCode::Internal, format!("{e:#}")))?;
    let acct = AccountConfig {
        jid,
        resource,
        host: p.host,
        port: p.port,
        insecure_plaintext: p.insecure_plaintext,
        password,
    };
    acct.validate()
        .map_err(|e| IpcError::new(ErrorCode::BadRequest, format!("{e:#}")))?;
    cfg.account = Some(acct);
    cfg.save(&core.profile.config_path()).map_err(internal)?;
    *core.config.lock().unwrap() = cfg;

    let mut events = core.events.subscribe();
    core.restart_session()
        .await
        .map_err(|e| IpcError::new(ErrorCode::NotConnected, format!("{e:#}")))?;
    // Give the login a moment so the caller learns whether it worked.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while core.status().state != agent_chat_proto::model::ConnectionState::Online {
        if tokio::time::timeout_at(deadline, events.recv()).await.is_err() {
            break;
        }
    }
    Ok(core.status())
}
