//! Connection I/O: our own read buffer with a per-element byte budget and an
//! inbound byte-rate limit (§5.4 step 1).
//!
//! tokio-xmpp's XML reader has no size limit, so the stream is fed from a
//! [`ConnIo`]: the session resets the shared [`Budget`] each time it gets a
//! complete top-level element. If an element grows past the budget, reads
//! fail with `InvalidData` and the session sends `<policy-violation/>`.
//! `fill_buf` never hands out more than the remaining budget, so the limit
//! is exact: an element of `limit` bytes passes, `limit + 1` fails.
//!
//! The rate limit is a token bucket. When it is empty, reads are delayed,
//! not failed, which throttles a flood without disconnecting.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep};

const READ_BUFFER: usize = 8 * 1024;

/// Shared between the [`ConnIo`] (which counts) and the session (which resets).
#[derive(Debug)]
pub struct Budget {
    used: AtomicUsize,
    limit: AtomicUsize,
    exceeded: AtomicBool,
}

impl Budget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            used: AtomicUsize::new(0),
            limit: AtomicUsize::new(limit),
            exceeded: AtomicBool::new(false),
        })
    }

    /// Start counting a new top-level element.
    pub fn reset(&self) {
        self.used.store(0, Ordering::Relaxed);
    }

    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Whether a read failed because an element was too large.
    pub fn exceeded(&self) -> bool {
        self.exceeded.load(Ordering::Relaxed)
    }
}

/// The error carried by the `io::Error` when the budget is exceeded.
#[derive(Debug)]
pub struct BudgetExceeded;

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stanza size limit exceeded")
    }
}

impl std::error::Error for BudgetExceeded {}

/// Token bucket over inbound bytes.
#[derive(Debug)]
pub struct RateLimit {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimit {
    pub fn new(bytes_per_sec: u64, burst: u64) -> Self {
        Self {
            rate: bytes_per_sec.max(1) as f64,
            burst: burst.max(1) as f64,
            tokens: burst.max(1) as f64,
            last: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
    }

    /// How long until at least one byte may be read; `None` if now.
    fn wait(&mut self) -> Option<Duration> {
        self.refill();
        if self.tokens >= 1.0 {
            None
        } else {
            Some(Duration::from_secs_f64((1.0 - self.tokens) / self.rate))
        }
    }

    fn take(&mut self, n: usize) {
        self.tokens -= n as f64;
    }
}

/// The transport under the XML stream: plain TCP, or TLS after STARTTLS.
pub enum Transport<S> {
    Plain(S),
    Tls(Box<tokio_rustls::server::TlsStream<S>>),
}

impl<S> Transport<S> {
    pub fn is_tls(&self) -> bool {
        matches!(self, Transport::Tls(_))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for Transport<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for Transport<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Transport::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(s) => Pin::new(s).poll_flush(cx),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Transport::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Transport::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Buffered, budgeted and rate-limited I/O for one connection.
pub struct ConnIo<S> {
    inner: S,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
    budget: Arc<Budget>,
    rate: Option<RateLimit>,
    delay: Option<Pin<Box<Sleep>>>,
}

impl<S> ConnIo<S> {
    pub fn new(inner: S, budget: Arc<Budget>, rate: Option<RateLimit>) -> Self {
        Self {
            inner,
            buf: vec![0u8; READ_BUFFER].into_boxed_slice(),
            pos: 0,
            cap: 0,
            budget,
            rate,
            delay: None,
        }
    }

    pub fn budget(&self) -> &Arc<Budget> {
        &self.budget
    }

    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Bytes read from the socket but not yet consumed by the parser.
    pub fn buffered(&self) -> usize {
        self.cap - self.pos
    }

    /// Give back the transport plus the rate limiter, for the STARTTLS upgrade.
    pub fn into_parts(self) -> (S, Option<RateLimit>) {
        (self.inner, self.rate)
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ConnIo<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let mut this = self;
        let avail = ready!(this.as_mut().poll_fill_buf(cx))?;
        let n = avail.len().min(out.remaining());
        out.put_slice(&avail[..n]);
        this.consume(n);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + Unpin> AsyncBufRead for ConnIo<S> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        let used = this.budget.used.load(Ordering::Relaxed);
        let limit = this.budget.limit();
        if used >= limit {
            this.budget.exceeded.store(true, Ordering::Relaxed);
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidData, BudgetExceeded)));
        }
        if this.pos >= this.cap {
            if let Some(rate) = this.rate.as_mut() {
                loop {
                    if let Some(d) = this.delay.as_mut() {
                        ready!(d.as_mut().poll(cx));
                        this.delay = None;
                    }
                    match rate.wait() {
                        None => break,
                        Some(wait) => this.delay = Some(Box::pin(tokio::time::sleep(wait))),
                    }
                }
            }
            let mut rb = ReadBuf::new(&mut this.buf);
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut rb))?;
            let n = rb.filled().len();
            this.pos = 0;
            this.cap = n;
            if let Some(rate) = this.rate.as_mut() {
                rate.take(n);
            }
        }
        let n = (this.cap - this.pos).min(limit - used);
        Poll::Ready(Ok(&this.buf[this.pos..this.pos + n]))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        let amt = amt.min(this.cap - this.pos);
        this.pos += amt;
        this.budget.used.fetch_add(amt, Ordering::Relaxed);
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ConnIo<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;
    use tokio_xmpp::minidom::Element;
    use tokio_xmpp::xmlstream::{ReadError, StreamHeader, Timeouts, accept_stream};

    const HEADER: &str = "<?xml version='1.0'?><stream:stream xmlns='jabber:client' \
        xmlns:stream='http://etherx.jabber.org/streams' to='hub.local' version='1.0'>";

    /// A message stanza of exactly `len` bytes.
    fn stanza(len: usize) -> String {
        let head = "<message><body>";
        let tail = "</body></message>";
        format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
    }

    async fn read_one(limit: usize, payload: String) -> Result<Element, ReadError> {
        let (client, server) = tokio::io::duplex(1 << 20);
        let budget = Budget::new(limit);
        let io = ConnIo::new(server, budget.clone(), None);
        let mut client = client;
        client.write_all(HEADER.as_bytes()).await.unwrap();
        client.write_all(payload.as_bytes()).await.unwrap();
        let accepted = accept_stream(io, "jabber:client", Timeouts::tight()).await.unwrap();
        budget.reset();
        let pending = accepted.send_header(StreamHeader::default()).await.unwrap();
        let mut stream = pending
            .send_features::<Element>(&Default::default())
            .await
            .unwrap();
        let r = stream.next().await.unwrap();
        if r.is_err() {
            assert!(budget.exceeded());
        }
        r
    }

    #[tokio::test]
    async fn budget_is_exact() {
        // Exactly at the limit: accepted.
        assert!(read_one(10_000, stanza(10_000)).await.is_ok());
        // One byte over: refused.
        match read_one(10_000, stanza(10_001)).await {
            Err(ReadError::HardError(e)) => assert!(e.get_ref().unwrap().is::<BudgetExceeded>()),
            other => panic!("expected a budget error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn budget_resets_per_element() {
        let (client, server) = tokio::io::duplex(1 << 20);
        let budget = Budget::new(2000);
        let io = ConnIo::new(server, budget.clone(), None);
        let mut client = client;
        client.write_all(HEADER.as_bytes()).await.unwrap();
        for _ in 0..5 {
            client.write_all(stanza(1500).as_bytes()).await.unwrap();
        }
        let accepted = accept_stream(io, "jabber:client", Timeouts::tight()).await.unwrap();
        budget.reset();
        let pending = accepted.send_header(StreamHeader::default()).await.unwrap();
        let mut stream = pending.send_features::<Element>(&Default::default()).await.unwrap();
        for _ in 0..5 {
            stream.next().await.unwrap().unwrap();
            budget.reset();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limit_delays_reads() {
        let (mut client, server) = tokio::io::duplex(1 << 16);
        let mut io = ConnIo::new(server, Budget::new(1 << 20), Some(RateLimit::new(1000, 1000)));
        client.write_all(&[b'a'; 3000]).await.unwrap();
        let start = Instant::now();
        let mut got = 0;
        while got < 3000 {
            let n = {
                let buf = std::future::poll_fn(|cx| Pin::new(&mut io).poll_fill_buf(cx).map(|r| r.map(|b| b.len())))
                    .await
                    .unwrap();
                buf
            };
            Pin::new(&mut io).consume(n);
            got += n;
        }
        // The first read takes the whole burst plus what the 8 KiB buffer
        // holds; later reads wait for tokens. 3000 bytes in one read here,
        // so test the delay on the next read instead.
        client.write_all(&[b'b'; 10]).await.unwrap();
        let n = std::future::poll_fn(|cx| Pin::new(&mut io).poll_fill_buf(cx).map(|r| r.map(|b| b.len())))
            .await
            .unwrap();
        assert_eq!(n, 10);
        assert!(start.elapsed() >= Duration::from_secs(2), "reads were not delayed: {:?}", start.elapsed());
    }
}
