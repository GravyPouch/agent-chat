//! Client-to-server connections: negotiation (stages A–C) and the session
//! loop after resource binding (§5.4).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::SinkExt;
use tokio::net::TcpStream;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::xmlstream::{Timeouts, XmlStream};
use tracing::Instrument;
use xmpp_parsers::stream_error::{DefinedCondition as StreamCondition, StreamError};

use crate::hub::HubCtx;
use crate::net::io::{ConnIo, Transport};
use crate::net::listener::AdmissionGuard;
use crate::stats::Stats;

pub mod negotiate;
pub mod sasl;
pub mod session;

pub type Io = ConnIo<Transport<TcpStream>>;
pub type Stream = XmlStream<Io, Element>;

/// Soft read timeout (we then ping the client) and the grace after it.
pub fn timeouts() -> Timeouts {
    Timeouts {
        read_timeout: Duration::from_secs(120),
        response_timeout: Duration::from_secs(60),
    }
}

/// Serve one TCP connection until it closes.
pub async fn serve(ctx: Arc<HubCtx>, tcp: TcpStream, peer: SocketAddr, guard: AdmissionGuard) {
    let conn_id = ctx.next_conn_id();
    let span = tracing::info_span!("conn", id = conn_id, peer = %guard.prefix(), jid = tracing::field::Empty);
    async move {
        Stats::inc(&ctx.stats.connections);
        Stats::inc(&ctx.stats.connections_total);
        let _ = tcp.set_nodelay(true);
        let deadline = ctx.config.limits.negotiation_timeout.0;
        let negotiated = tokio::select! {
            r = tokio::time::timeout(deadline, negotiate::negotiate(&ctx, tcp, peer.ip(), conn_id)) => match r {
                Ok(Ok(bound)) => bound,
                Ok(Err(e)) => {
                    tracing::debug!("connection ended during negotiation: {e:#}");
                    None
                }
                Err(_) => {
                    tracing::info!("negotiation timed out");
                    Stats::inc(&ctx.stats.limit_hits);
                    None
                }
            },
            _ = ctx.shutdown.cancelled() => None,
        };
        if let Some(bound) = negotiated {
            tracing::Span::current().record("jid", tracing::field::display(&bound.jid));
            session::run(&ctx, bound).await;
        }
        Stats::dec(&ctx.stats.connections);
        drop(guard);
    }
    .instrument(span)
    .await
}

/// Send a stream error and close the stream, giving up after a few seconds.
pub async fn close_with_error(stream: &mut Stream, condition: StreamCondition, text: &str) {
    let err = StreamError::new(condition, "en", text);
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        if stream.send(&err).await.is_ok() {
            let _ = SinkExt::<&Element>::close(stream).await;
        }
    })
    .await;
}

/// Close the stream cleanly (our `</stream:stream>`).
pub async fn close(stream: &mut Stream) {
    let _ = tokio::time::timeout(Duration::from_secs(5), SinkExt::<&Element>::close(stream)).await;
}
