//! TCP listeners and per-IP admission (§5.4 step 1, §6.4).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpSocket};

use crate::util::{ip_prefix, is_loopback};

/// Bind a listener. IPv6 sockets are v6-only so `0.0.0.0` and `[::]` can
/// both be bound on the same port.
pub fn bind(addr: SocketAddr) -> Result<TcpListener> {
    let socket = if addr.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    if addr.is_ipv6() {
        set_v6_only(&socket);
    }
    socket.bind(addr).with_context(|| format!("binding {addr}"))?;
    socket.listen(1024).with_context(|| format!("listening on {addr}"))
}

#[cfg(unix)]
fn set_v6_only(socket: &TcpSocket) {
    use std::os::fd::AsRawFd;
    let on: libc::c_int = 1;
    // SAFETY: valid fd and option pointer/length for IPV6_V6ONLY.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            (&on as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(not(unix))]
fn set_v6_only(_socket: &TcpSocket) {}

/// Why a connection was refused at accept time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    TooManyConnections,
    TooManyNewConnections,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::TooManyConnections => "too many concurrent connections from this address",
            Refusal::TooManyNewConnections => "too many new connections from this address",
        }
    }
}

#[derive(Debug)]
struct IpState {
    active: u32,
    tokens: f64,
    last: Instant,
}

/// Per-IP concurrent connection cap and new-connection token bucket.
/// Loopback peers are exempt.
#[derive(Debug)]
pub struct Admission {
    max_concurrent: u32,
    per_minute: u32,
    state: Mutex<HashMap<IpAddr, IpState>>,
}

impl Admission {
    pub fn new(max_concurrent: u32, per_minute: u32) -> Arc<Self> {
        Arc::new(Self {
            max_concurrent,
            per_minute,
            state: Mutex::default(),
        })
    }

    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Result<AdmissionGuard, Refusal> {
        if is_loopback(ip) {
            return Ok(AdmissionGuard { admission: None, ip });
        }
        let now = Instant::now();
        let mut map = self.state.lock().unwrap();
        // Forget idle entries so the map stays small.
        if map.len() > 4096 {
            map.retain(|_, s| s.active > 0 || now.duration_since(s.last) < Duration::from_secs(120));
        }
        let burst = self.per_minute.max(1) as f64;
        let st = map.entry(ip).or_insert(IpState {
            active: 0,
            tokens: burst,
            last: now,
        });
        let elapsed = now.duration_since(st.last).as_secs_f64();
        st.last = now;
        st.tokens = (st.tokens + elapsed * burst / 60.0).min(burst);
        if st.active >= self.max_concurrent {
            return Err(Refusal::TooManyConnections);
        }
        if st.tokens < 1.0 {
            return Err(Refusal::TooManyNewConnections);
        }
        st.tokens -= 1.0;
        st.active += 1;
        Ok(AdmissionGuard {
            admission: Some(self.clone()),
            ip,
        })
    }
}

/// Releases the connection slot when the connection ends.
#[derive(Debug)]
pub struct AdmissionGuard {
    admission: Option<Arc<Admission>>,
    ip: IpAddr,
}

impl AdmissionGuard {
    pub fn prefix(&self) -> String {
        ip_prefix(self.ip)
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if let Some(a) = &self.admission
            && let Some(st) = a.state.lock().unwrap().get_mut(&self.ip)
        {
            st.active = st.active.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_concurrent_and_new_connections() {
        let adm = Admission::new(2, 3);
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        let a = adm.admit(ip).unwrap();
        let _b = adm.admit(ip).unwrap();
        assert_eq!(adm.admit(ip).unwrap_err(), Refusal::TooManyConnections);
        drop(a);
        let _c = adm.admit(ip).unwrap();
        drop(_c);
        // Three tokens per minute, all used.
        assert_eq!(adm.admit(ip).unwrap_err(), Refusal::TooManyNewConnections);
        // Another address is unaffected; loopback is exempt.
        assert!(adm.admit("192.0.2.8".parse().unwrap()).is_ok());
        let lo: Vec<_> = (0..10).map(|_| adm.admit("127.0.0.1".parse().unwrap()).unwrap()).collect();
        assert_eq!(lo.len(), 10);
    }
}
