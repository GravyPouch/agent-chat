//! Small helpers shared by the server modules.

use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch (the storage convention, §5.8).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Fill a buffer from the OS random number generator.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the OS random number generator failed");
    b
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// 128 random bits as hex: stream ids and similar.
pub fn random_id() -> String {
    hex(&random_bytes::<16>())
}

/// Loopback check that also covers IPv4-mapped IPv6 addresses.
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// The address prefix used in logs and per-network throttles:
/// /24 for IPv4, /56 for IPv6 (§5.10).
pub fn ip_prefix(ip: IpAddr) -> String {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}00::/56", s[0], s[1], s[2], s[3] >> 8)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert_eq!(ip_prefix("192.168.1.20".parse().unwrap()), "192.168.1.0/24");
        assert_eq!(ip_prefix("2001:db8:1:2345::1".parse().unwrap()), "2001:db8:1:2300::/56");
        assert_eq!(ip_prefix("::ffff:10.1.2.3".parse().unwrap()), "10.1.2.0/24");
    }

    #[test]
    fn loopback() {
        assert!(is_loopback("127.0.0.1".parse().unwrap()));
        assert!(is_loopback("::1".parse().unwrap()));
        assert!(is_loopback("::ffff:127.0.0.1".parse().unwrap()));
        assert!(!is_loopback("10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn hex_encoding() {
        assert_eq!(hex(&[0, 0xab, 0x10]), "00ab10");
        assert_eq!(random_id().len(), 32);
    }
}
