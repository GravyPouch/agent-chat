//! Small desktop integrations: the window size kept per profile, the Dock
//! badge, and finding links in message text.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const WINDOW_FILE: &str = "gui-window.json";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct WindowSize {
    width: f32,
    height: f32,
}

/// The window size last used with this profile, if it's a sane one.
pub fn load_window_size(profile_dir: &Path) -> Option<(f32, f32)> {
    let text = std::fs::read_to_string(profile_dir.join(WINDOW_FILE)).ok()?;
    let s: WindowSize = serde_json::from_str(&text).ok()?;
    let sane = (640.0..=10_000.0).contains(&s.width) && (420.0..=10_000.0).contains(&s.height);
    sane.then_some((s.width, s.height))
}

pub fn save_window_size(profile_dir: &Path, width: f32, height: f32) {
    let path: PathBuf = profile_dir.join(WINDOW_FILE);
    if let Ok(text) = serde_json::to_string(&WindowSize { width, height }) {
        let _ = std::fs::write(path, text);
    }
}

/// The number on the Dock icon (macOS); none for zero.
#[cfg(target_os = "macos")]
pub fn set_dock_badge(count: u64) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    let label = (count > 0).then(|| {
        NSString::from_str(&if count > 99 {
            "99+".to_owned()
        } else {
            count.to_string()
        })
    });
    let label: *const NSString = label.as_deref().map_or(std::ptr::null(), |l| l as *const NSString);
    // SAFETY: called on the main (UI) thread, where GPUI runs us. NSApp and
    // its dock tile live as long as the process; a nil label clears the badge.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        let tile: *mut AnyObject = msg_send![app, dockTile];
        if tile.is_null() {
            return;
        }
        let _: () = msg_send![tile, setBadgeLabel: label];
    }
}

#[cfg(not(target_os = "macos"))]
pub fn set_dock_badge(_count: u64) {}

/// http(s) links in a message, in order, without duplicates or trailing
/// punctuation.
pub fn find_links(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"')) {
        let Some(start) = word.find("https://").or_else(|| word.find("http://")) else {
            continue;
        };
        let mut url = &word[start..];
        while let Some(last) = url.chars().last()
            && (matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '_')
                || (last == ')' && url.matches('(').count() < url.matches(')').count())
                || matches!(last, ']' | '}'))
        {
            url = &url[..url.len() - last.len_utf8()];
        }
        let host = url.split("://").nth(1).unwrap_or("");
        if host.is_empty() || host.starts_with('/') {
            continue;
        }
        if !out.iter().any(|u| u == url) {
            out.push(url.to_owned());
        }
    }
    out
}

/// A link shortened for a button: no scheme, at most `max` characters.
pub fn short_link(url: &str, max: usize) -> String {
    let s = url.split_once("://").map_or(url, |(_, rest)| rest);
    let s = s.strip_prefix("www.").unwrap_or(s);
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    } else {
        s.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::{find_links, load_window_size, save_window_size, short_link};

    #[test]
    fn links_in_text() {
        assert_eq!(
            find_links("Logs: https://example.org/build/1234. See (https://x.org/a_(b)) and http://y.net, ok?"),
            vec!["https://example.org/build/1234", "https://x.org/a_(b)", "http://y.net"]
        );
        assert_eq!(
            find_links("twice https://a.io https://a.io <https://b.io/p?q=1>"),
            vec!["https://a.io", "https://b.io/p?q=1"]
        );
        assert!(find_links("no links, just https:// and ftp://x").is_empty());
    }

    #[test]
    fn short_links() {
        assert_eq!(short_link("https://www.example.org/a", 40), "example.org/a");
        assert_eq!(short_link("https://example.org/very/long/path", 12), "example.org…");
    }

    #[test]
    fn window_size_round_trip() {
        let dir = std::env::temp_dir().join(format!("agent-chat-gui-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_window_size(&dir), None);
        save_window_size(&dir, 1300.0, 900.0);
        assert_eq!(load_window_size(&dir), Some((1300.0, 900.0)));
        save_window_size(&dir, 10.0, 10.0);
        assert_eq!(load_window_size(&dir), None, "nonsense sizes are ignored");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
