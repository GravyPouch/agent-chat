//! Quick links and QR codes for opening agent-chat accounts in other XMPP
//! clients (phones in particular): RFC 5122 `xmpp:` URIs.
//!
//! * Sign-up: `xmpp:example.org?register` (XEP-0147 action, as used by the
//!   XEP-0401 onboarding links that Conversations, Monal and others handle).
//! * Contact: `xmpp:alice@example.org?omemo-sid-ID=FINGERPRINT;…`, the
//!   format Conversations introduced (and Monal follows) that adds the
//!   contact *and* marks the listed OMEMO keys as verified.

use qrcode::QrCode;
use qrcode::render::svg;

/// Percent-encode everything except RFC 3986 unreserved characters and `@`.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn register_uri(domain: &str) -> String {
    format!("xmpp:{}?register", encode(domain))
}

/// `fingerprints`: (OMEMO device id, 64-hex Curve25519 fingerprint).
pub fn contact_uri(jid: &str, fingerprints: &[(u32, String)]) -> String {
    let mut uri = format!("xmpp:{}", encode(jid));
    let params: Vec<String> = fingerprints
        .iter()
        .filter(|(_, fp)| fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(|(sid, fp)| format!("omemo-sid-{sid}={}", fp.to_ascii_lowercase()))
        .collect();
    if !params.is_empty() {
        uri.push('?');
        uri.push_str(&params.join(";"));
    }
    uri
}

/// A QR code that signs in to an existing account: the JSON provisioning
/// format Monal's login scanner reads (and Conversations proposed), e.g.
/// `{"protocol":"xmpp","address":"ana@example.org","password":"…"}`.
pub fn login_qr_payload(jid: &str, password: &str) -> String {
    serde_json::json!({"protocol": "xmpp", "address": jid, "password": password}).to_string()
}

/// Domains a phone can't reach (this machine only).
pub fn local_only(domain: &str) -> bool {
    let d = domain.trim_end_matches('.').to_ascii_lowercase();
    d == "localhost" || d.ends_with(".localhost") || d.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// The QR code as a standalone SVG document (black on white, with a quiet zone).
pub fn qr_svg(data: &str, size: u32) -> String {
    match QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<svg::Color<'_>>()
            .min_dimensions(size, size)
            .quiet_zone(true)
            .dark_color(svg::Color("#000000"))
            .light_color(svg::Color("#ffffff"))
            .build(),
        Err(_) => String::new(),
    }
}

/// The QR code drawn with Unicode half blocks, for terminals. Two modules
/// per character cell vertically; light-on-dark terminals scan fine too
/// because the quiet zone is included.
pub fn qr_terminal(data: &str) -> String {
    let Ok(code) = QrCode::new(data.as_bytes()) else {
        return String::new();
    };
    let width = code.width();
    let colors = code.to_colors();
    let quiet = 2usize;
    let total = width + 2 * quiet;
    let dark = |x: usize, y: usize| -> bool {
        if x < quiet || y < quiet || x >= width + quiet || y >= width + quiet {
            return false;
        }
        colors[(y - quiet) * width + (x - quiet)] == qrcode::Color::Dark
    };
    let mut out = String::new();
    let mut y = 0;
    while y < total {
        for x in 0..total {
            // Print light modules as blocks so the code reads as dark-on-light
            // even on dark terminals.
            let top = !dark(x, y);
            let bottom = y + 1 < total && !dark(x, y + 1);
            out.push(match (top, bottom) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
        y += 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_qr_is_monals_json() {
        let p = login_qr_payload("ana@example.org", "pa\"ss");
        let v: serde_json::Value = serde_json::from_str(&p).unwrap();
        assert_eq!(v["protocol"], "xmpp");
        assert_eq!(v["address"], "ana@example.org");
        assert_eq!(v["password"], "pa\"ss");
    }

    #[test]
    fn uris() {
        assert_eq!(register_uri("mac.tail1234.ts.net"), "xmpp:mac.tail1234.ts.net?register");
        let fp = "ab".repeat(32);
        assert_eq!(
            contact_uri("alice@example.org", &[(1234, fp.clone()), (99, "bad".into())]),
            format!("xmpp:alice@example.org?omemo-sid-1234={fp}")
        );
        assert_eq!(contact_uri("a b@x", &[]), "xmpp:a%20b@x");
    }

    #[test]
    fn local_only_domains() {
        assert!(local_only("localhost"));
        assert!(local_only("hub.localhost"));
        assert!(local_only("127.0.0.1"));
        assert!(!local_only("mac.tail1234.ts.net"));
        assert!(!local_only("mybox.local"));
    }

    #[test]
    fn qr_renders() {
        let svg = qr_svg("xmpp:alice@example.org", 200);
        assert!(svg.contains("<svg") && svg.contains("</svg>"));
        let t = qr_terminal("xmpp:alice@example.org");
        assert!(t.lines().count() > 10);
    }
}
