//! Readable group chat names, so nobody has to type `room@conference.example.org`.

use std::hash::{BuildHasher, Hasher};

const ADJECTIVES: &[&str] = &[
    "amber", "brave", "bright", "calm", "clever", "cosy", "crisp", "curious", "eager", "gentle", "golden", "happy",
    "humble", "jolly", "keen", "kind", "lively", "lucky", "merry", "mellow", "nimble", "noble", "polite", "quick",
    "quiet", "rapid", "silver", "steady", "sunny", "swift", "tidy", "witty",
];

const ANIMALS: &[&str] = &[
    "badger", "beaver", "bison", "crane", "dolphin", "eagle", "falcon", "ferret", "finch", "fox", "gecko", "heron",
    "ibis", "koala", "lemur", "lynx", "marten", "moose", "newt", "otter", "owl", "panda", "puffin", "quokka", "raven",
    "robin", "seal", "stoat", "swan", "tapir", "walrus", "wren",
];

/// A name like `quiet-otter-42` from 64 bits of `seed`.
pub fn room_name_from(seed: u64) -> String {
    let adjective = ADJECTIVES[(seed % ADJECTIVES.len() as u64) as usize];
    let animal = ANIMALS[((seed >> 16) % ANIMALS.len() as u64) as usize];
    let number = 10 + (seed >> 32) % 90;
    format!("{adjective}-{animal}-{number}")
}

/// A fresh random name like `quiet-otter-42`.
pub fn random_room_name() -> String {
    room_name_from(random_u64())
}

/// Not for secrets: the standard library's per-process random hash keys
/// mixed with the clock.
pub fn random_u64() -> u64 {
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    h.write_u128(nanos);
    h.finish()
}

/// The local part of a room address for a name someone typed: "Release
/// planning!" becomes `release-planning`. `None` if nothing usable is left.
pub fn room_slug(name: &str) -> Option<String> {
    let mut slug = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if (c.is_whitespace() || matches!(c, '-' | '_' | '.')) && !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug.truncate(48);
    while slug.ends_with('-') {
        slug.pop();
    }
    (!slug.is_empty()).then_some(slug)
}

/// `name` as a room address: kept if it already has a service
/// (`team@conference.example.org`), otherwise put on `service`.
pub fn room_address(name: &str, service: &str) -> String {
    let name = name.trim();
    if name.contains('@') {
        name.to_owned()
    } else {
        format!("{}@{service}", name.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_readable() {
        for seed in [0, 1, 42, u64::MAX, 0xdead_beef_cafe_f00d] {
            let name = room_name_from(seed);
            let parts: Vec<&str> = name.split('-').collect();
            assert_eq!(parts.len(), 3, "{name}");
            assert!(ADJECTIVES.contains(&parts[0]), "{name}");
            assert!(ANIMALS.contains(&parts[1]), "{name}");
            let n: u32 = parts[2].parse().unwrap();
            assert!((10..100).contains(&n), "{name}");
            // Valid as a JID local part as it is.
            assert_eq!(room_slug(&name).as_deref(), Some(name.as_str()));
        }
        assert_eq!(room_name_from(42), room_name_from(42));
        let names: std::collections::HashSet<String> = (0..20).map(|_| random_room_name()).collect();
        assert!(names.len() > 1, "random names vary");
    }

    #[test]
    fn slugs() {
        assert_eq!(room_slug("Release planning!").as_deref(), Some("release-planning"));
        assert_eq!(room_slug("  Q3 -- roadmap_v2  ").as_deref(), Some("q3-roadmap-v2"));
        assert_eq!(room_slug("Café crème").as_deref(), Some("caf-crme"));
        assert_eq!(room_slug("!!!"), None);
        assert_eq!(room_slug("—"), None);
        assert_eq!(room_slug(&"a".repeat(80)).unwrap().len(), 48);
    }

    #[test]
    fn addresses() {
        assert_eq!(
            room_address("Team", "conference.example.org"),
            "team@conference.example.org"
        );
        assert_eq!(
            room_address("team@muc.other.org", "conference.example.org"),
            "team@muc.other.org"
        );
    }
}
