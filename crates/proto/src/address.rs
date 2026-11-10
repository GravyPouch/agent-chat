//! Understanding what people type or paste to start a chat: a bare
//! username on their own server, an address, or an `xmpp:` link (RFC 5122,
//! XEP-0147).

/// Who or what to open a chat with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatTarget {
    /// A contact's bare JID.
    Contact(String),
    /// A group chat's bare JID (an `xmpp:…?join` link).
    Room(String),
}

/// `bob`, `bob@example.org`, `xmpp:bob@example.org?message` or
/// `xmpp:team@conference.example.org?join`. A bare name is put on
/// `own_domain`. `None` if it doesn't look like an address.
pub fn parse_chat_target(input: &str, own_domain: Option<&str>) -> Option<ChatTarget> {
    let mut s = input.trim();
    let is_uri = s.len() >= 5 && s[..5].eq_ignore_ascii_case("xmpp:");
    if is_uri {
        s = s[5..].trim_start_matches('/');
    }
    let (address, query) = match s.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (s, None),
    };
    let address = percent_decode(address);
    // A bare JID: drop any resource.
    let address = address.split('/').next().unwrap_or_default().trim().to_owned();
    if address.is_empty() || address.chars().any(char::is_whitespace) {
        return None;
    }
    let jid = match address.split_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() && !domain.contains('@') => address,
        Some(_) => return None,
        None => format!("{address}@{}", own_domain.filter(|d| !d.is_empty())?),
    };
    let join = query.is_some_and(|q| q.split(';').next() == Some("join"));
    Some(if join {
        ChatTarget::Room(jid)
    } else {
        ChatTarget::Contact(jid)
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contact(s: &str) -> Option<ChatTarget> {
        Some(ChatTarget::Contact(s.to_owned()))
    }

    #[test]
    fn names_addresses_and_links() {
        let own = Some("example.org");
        assert_eq!(parse_chat_target("bob", own), contact("bob@example.org"));
        assert_eq!(parse_chat_target("  bob@other.net ", own), contact("bob@other.net"));
        assert_eq!(parse_chat_target("bob@other.net/phone", own), contact("bob@other.net"));
        assert_eq!(parse_chat_target("xmpp:bob@other.net", own), contact("bob@other.net"));
        assert_eq!(
            parse_chat_target("XMPP:bob@other.net?message;body=hi", own),
            contact("bob@other.net")
        );
        assert_eq!(
            parse_chat_target("xmpp:bob@other.net?roster;name=Bob", own),
            contact("bob@other.net")
        );
        // OMEMO fingerprints in the link (Conversations' QR codes) are ignored.
        assert_eq!(
            parse_chat_target("xmpp:bob@other.net?omemo-sid-123=abcdef", own),
            contact("bob@other.net")
        );
        assert_eq!(parse_chat_target("xmpp:bob%40other.net", own), contact("bob@other.net"));
        assert_eq!(
            parse_chat_target("xmpp:team@conference.other.net?join", own),
            Some(ChatTarget::Room("team@conference.other.net".into()))
        );
    }

    #[test]
    fn rejects_non_addresses() {
        assert_eq!(parse_chat_target("", Some("example.org")), None);
        assert_eq!(parse_chat_target("bob smith", Some("example.org")), None);
        assert_eq!(parse_chat_target("@example.org", Some("example.org")), None);
        assert_eq!(parse_chat_target("bob@", Some("example.org")), None);
        assert_eq!(parse_chat_target("a@b@c", Some("example.org")), None);
        // A bare name needs a server to put it on.
        assert_eq!(parse_chat_target("bob", None), None);
    }
}
