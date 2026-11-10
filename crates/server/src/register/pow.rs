//! `urn:xmpp:ibr-pow:0`, server side: the same token format, HMAC binding,
//! puzzle, escalation and replay cache as `server/prosody/mod_register_pow`
//! (docs/design/pow-registration.md §5). Pure functions plus the in-memory
//! escalation state; checked against the same §5.7 test vector as the Lua
//! module and the client.

use std::collections::HashMap;
use std::net::IpAddr;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::util::hex;

pub const NS: &str = "urn:xmpp:ibr-pow:0";

/// `{urn:xmpp:ibr-pow:0}name`, the form field names.
pub fn field(name: &str) -> String {
    format!("{{{NS}}}{name}")
}

/// `tag = hex(HMAC-SHA256(K, NS 0x00 domain 0x00 ipbind 0x00 payload))[..32]`
pub fn sign(secret: &[u8], domain: &str, ipbind: &str, payload: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC takes any key length");
    for (i, part) in [NS, domain, ipbind, payload].iter().enumerate() {
        if i > 0 {
            mac.update(&[0]);
        }
        mac.update(part.as_bytes());
    }
    hex(&mac.finalize().into_bytes())[..32].to_owned()
}

/// A parsed `1:alg:bits:count:iat:exp:rnd:tag` token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub version: String,
    pub algorithm: String,
    pub bits: u32,
    pub count: u32,
    pub iat: i64,
    pub exp: i64,
    pub tag: String,
    /// Everything before `:tag`.
    pub payload: String,
}

pub fn parse_token(token: &str) -> Option<Token> {
    let parts: Vec<&str> = token.split(':').collect();
    let [version, alg, bits, count, iat, exp, rnd, tag] = parts.as_slice() else {
        return None;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let hexy = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    if !(digits(version) && digits(bits) && digits(count) && digits(iat) && digits(exp))
        || !alg.bytes().all(|b| b.is_ascii_alphanumeric())
        || alg.is_empty()
        || !hexy(rnd)
        || !hexy(tag)
    {
        return None;
    }
    Some(Token {
        version: (*version).to_owned(),
        algorithm: (*alg).to_owned(),
        bits: bits.parse().ok()?,
        count: count.parse().ok()?,
        iat: iat.parse().ok()?,
        exp: exp.parse().ok()?,
        tag: (*tag).to_owned(),
        payload: token[..token.len() - tag.len() - 1].to_owned(),
    })
}

pub fn make_payload(bits: u32, count: u32, iat: i64, exp: i64, rnd_hex: &str) -> String {
    format!("1:sha256:{bits}:{count}:{iat}:{exp}:{rnd_hex}")
}

/// `prefix = NS 0x00 token 0x00 domain 0x00 username 0x00`
pub fn puzzle_prefix(token: &str, domain: &str, username: &str) -> Vec<u8> {
    let mut p = Vec::new();
    for part in [NS, token, domain, username] {
        p.extend_from_slice(part.as_bytes());
        p.push(0);
    }
    p
}

/// Does the digest start with at least `bits` zero bits?
pub fn leading_zero_bits_ok(h: &[u8], bits: u32) -> bool {
    let (full, rem) = ((bits / 8) as usize, bits % 8);
    if h.len() < full + usize::from(rem > 0) {
        return false;
    }
    h[..full].iter().all(|b| *b == 0) && (rem == 0 || h[full] < (1u8 << (8 - rem)))
}

/// `H_i = SHA-256(prefix || BYTE(i) || N_i)`, `N_i` = 8 bytes (16 hex).
pub fn verify_solution(
    token: &str,
    domain: &str,
    username: &str,
    solution: &str,
    bits: u32,
    count: u32,
) -> Result<(), String> {
    let nonces: Vec<&str> = solution.split(',').filter(|n| !n.is_empty()).collect();
    if nonces.len() != count as usize {
        return Err(format!("expected {count} nonces"));
    }
    let prefix = puzzle_prefix(token, domain, username);
    for (i, n) in nonces.iter().enumerate() {
        if n.len() != 16 {
            return Err(format!("nonce {i} must be 16 hex characters"));
        }
        let Ok(v) = u64::from_str_radix(n, 16) else {
            return Err(format!("nonce {i} is not hex"));
        };
        let mut h = Sha256::new();
        h.update(&prefix);
        h.update([i as u8]);
        h.update(v.to_be_bytes());
        if !leading_zero_bits_ok(&h.finalize(), bits) {
            return Err(format!("nonce {i} does not satisfy the difficulty"));
        }
    }
    Ok(())
}

/// `203.0.113.7/32`, `2001:db8:1200::/56` (IPv4-mapped IPv6 counts as IPv4).
pub fn ip_prefix(ip: IpAddr, v4_len: u8, v6_len: u8) -> String {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let len = v4_len.min(32);
            let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
            let net = std::net::Ipv4Addr::from(u32::from(v4) & mask);
            format!("{net}/{len}")
        }
        IpAddr::V6(v6) => {
            let len = v6_len.min(128);
            let mask = if len == 0 { 0 } else { u128::MAX << (128 - len) };
            let net = std::net::Ipv6Addr::from(u128::from(v6) & mask);
            format!("{net}/{len}")
        }
    }
}

/// Recent successful PoW registrations, for difficulty escalation
/// (`register_pow_prefix_*` and `register_pow_load_*` in the Lua module).
#[derive(Debug, Default)]
pub struct Escalation {
    by_prefix: HashMap<String, Vec<i64>>,
    recent_all: Vec<i64>,
}

impl Escalation {
    /// Extra bits for this prefix: one per success in the window, capped.
    pub fn prefix_extra(&mut self, prefix: &str, now: i64, window_secs: i64, cap: u32) -> u32 {
        let list = self.by_prefix.entry(prefix.to_owned()).or_default();
        list.retain(|t| *t >= now - window_secs);
        let n = list.len() as u32;
        if list.is_empty() {
            self.by_prefix.remove(prefix);
        }
        n.min(cap)
    }

    /// Extra bits for global load: `floor(log2(rate / threshold)) + 1` once
    /// the last hour's rate reaches the threshold, capped.
    pub fn load_extra(&mut self, now: i64, threshold: u32, cap: u32) -> u32 {
        self.recent_all.retain(|t| *t >= now - 3600);
        let rate = self.recent_all.len() as f64;
        if rate < threshold as f64 {
            return 0;
        }
        (((rate / threshold as f64).log2().floor() as u32) + 1).min(cap)
    }

    pub fn record(&mut self, prefix: &str, now: i64) {
        self.by_prefix.entry(prefix.to_owned()).or_default().push(now);
        self.recent_all.push(now);
        if self.by_prefix.len() > 100_000 {
            self.by_prefix.retain(|_, v| v.iter().any(|t| *t >= now - 86_400));
        }
    }
}

/// Tokens already used to create an account (replay cache).
#[derive(Debug, Default)]
pub struct Spent {
    tags: HashMap<String, i64>,
}

impl Spent {
    pub fn contains(&self, tag: &str) -> bool {
        self.tags.contains_key(tag)
    }

    pub fn insert(&mut self, tag: &str, exp: i64, now: i64) {
        if self.tags.len() > 10_000 {
            self.tags.retain(|_, e| *e > now);
        }
        self.tags.insert(tag.to_owned(), exp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: &[u8] = b"example-secret-do-not-use";
    const TOKEN: &str =
        "1:sha256:25:8:1794326400:1794330000:9f2c4e7a1b3d5f60718293a4b5c6d7e8:6b8ae478ff3592e715741262396837c1";
    const SOLUTION: &str = "0000000001dee49f,000000000212fd53,00000000006093ad,000000000016321f,\
        00000000002aa9f1,0000000003f1a352,00000000022ca4ce,000000000014a0b1";

    /// docs/design/pow-registration.md §5.7, as in test_vector.lua.
    #[test]
    fn design_test_vector() {
        let t = parse_token(TOKEN).unwrap();
        assert_eq!((t.bits, t.count), (25, 8));
        assert_eq!(sign(K, "example.org", "203.0.113.7/32", &t.payload), t.tag);
        assert_ne!(sign(K, "example.org", "198.51.100.1/32", &t.payload), t.tag);
        let ok = |d: &str, u: &str, s: &str| verify_solution(TOKEN, d, u, s, t.bits, t.count).is_ok();
        assert!(ok("example.org", "agent-7k2m9qx4td", SOLUTION));
        assert!(
            !ok("example.org", "agent-7k2m9qx4tX", SOLUTION),
            "bound to the username"
        );
        assert!(!ok("example.net", "agent-7k2m9qx4td", SOLUTION), "bound to the domain");
        assert!(
            !ok("example.org", "agent-7k2m9qx4td", &SOLUTION[17..]),
            "needs all nonces"
        );
        assert!(leading_zero_bits_ok(&[0, 0, 1], 23) && !leading_zero_bits_ok(&[0, 0, 1], 24));
    }

    #[test]
    fn tokens_and_prefixes() {
        assert!(parse_token("1:sha256:25:8:1:2:zz:aa").is_none());
        assert!(parse_token("1:sha256:25:8:1:2:aa").is_none());
        let p = make_payload(20, 8, 10, 20, "ab");
        let tag = sign(K, "d", "web:1.2.3.4/32", &p);
        let t = parse_token(&format!("{p}:{tag}")).unwrap();
        assert_eq!((t.bits, t.iat, t.exp, t.payload), (20, 10, 20, p));
        assert_eq!(ip_prefix("203.0.113.7".parse().unwrap(), 32, 56), "203.0.113.7/32");
        assert_eq!(ip_prefix("203.0.113.7".parse().unwrap(), 24, 56), "203.0.113.0/24");
        assert_eq!(
            ip_prefix("2001:db8:1234:56ff::1".parse().unwrap(), 32, 56),
            "2001:db8:1234:5600::/56"
        );
        assert_eq!(ip_prefix("::ffff:10.1.2.3".parse().unwrap(), 32, 56), "10.1.2.3/32");
    }

    #[test]
    fn escalation_arithmetic() {
        let mut e = Escalation::default();
        assert_eq!(e.prefix_extra("p", 1000, 86_400, 6), 0);
        for _ in 0..10 {
            e.record("p", 1000);
        }
        assert_eq!(e.prefix_extra("p", 1000, 86_400, 6), 6, "capped");
        assert_eq!(e.prefix_extra("p", 1000 + 86_401, 86_400, 6), 0, "window passed");
        // 10 in the last hour, threshold 5: floor(log2(2)) + 1 = 2.
        assert_eq!(e.load_extra(1000, 5, 4), 2);
        assert_eq!(e.load_extra(1000, 30, 4), 0);
    }
}
