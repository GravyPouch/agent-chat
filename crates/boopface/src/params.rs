// The looks follow BoopFace's flat colour and skin series (MIT, Copyright (c)
// 2026 CX ART Lab; src/skins/catalog.js, src/skins/native.js). The JID mapping
// and the palettes derived from the XEP-0392 hue are agent-chat's own.
// See third_party/boopface/LICENSE.

//! From a JID to a face.
//!
//! BoopFace has no seed: every `<boop-face>` is the same robot until the page
//! sets its colour, skin or head roundness. This mapping is agent-chat's own.
//! It only uses integers taken from one SHA-1 digest, so the same JID gives
//! the same parameters on every run and platform:
//!
//! - bytes 0–1: the XEP-0392 hue (the same colour Dino and Gajim show);
//! - byte 2: the look, one of [`Look::ALL`];
//! - byte 3: the head roundness, 20..=90 on BoopFace's 0..=100 scale.
//!
//! Changing this mapping changes everyone's face, so treat it as stable.

use sha1::{Digest, Sha1};

use crate::color::{Rgb, hsluv};
use crate::xep0392;

/// A paint style, after one of BoopFace's flat colour or skin painters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Look {
    /// The plain BoopFace look: one flat colour, white eyes (`color` attribute).
    Flat,
    /// A deep radial gradient with a glossy highlight, after the
    /// "Gems and ceramics" skins (series 10).
    Gem,
    /// A light diagonal gradient with dark eyes, after the "Gradient" skins (series 7).
    Gradient,
    /// Round ball shading with a hot spot, after the "Balls" skins (series 12),
    /// without their printed patterns.
    Ball,
}

impl Look {
    pub const ALL: [Look; 4] = [Look::Flat, Look::Gem, Look::Gradient, Look::Ball];

    pub fn name(self) -> &'static str {
        match self {
            Look::Flat => "flat",
            Look::Gem => "gem",
            Look::Gradient => "gradient",
            Look::Ball => "ball",
        }
    }
}

/// Everything that makes one face differ from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FaceParams {
    /// The XEP-0392 16-bit hue value (degrees = raw / 65536 × 360).
    pub hue_raw: u16,
    pub look: Look,
    /// BoopFace head roundness, `0..=100`; 50 is BoopFace's default.
    pub roundness: u8,
}

/// The lowest and highest roundness a JID can get: neither a square nor a circle.
const ROUNDNESS_MIN: u8 = 20;
const ROUNDNESS_MAX: u8 = 90;

/// The bare JID, lower-cased, as the identifier for hashing. JIDs compare
/// case-insensitively in their local and domain parts; the resource is dropped.
pub fn normalize_jid(jid: &str) -> String {
    let bare = jid.split('/').next().unwrap_or(jid).trim();
    bare.to_lowercase()
}

impl FaceParams {
    /// The face for a JID (any resource is ignored).
    pub fn from_jid(jid: &str) -> FaceParams {
        FaceParams::from_identifier(&normalize_jid(jid))
    }

    /// The face for an arbitrary identifier, hashed as given.
    pub fn from_identifier(identifier: &str) -> FaceParams {
        let digest = Sha1::digest(identifier.as_bytes());
        let range = u32::from(ROUNDNESS_MAX - ROUNDNESS_MIN);
        FaceParams {
            hue_raw: u16::from_le_bytes([digest[0], digest[1]]),
            look: Look::ALL[usize::from(digest[2]) % Look::ALL.len()],
            roundness: ROUNDNESS_MIN + (u32::from(digest[3]) * range / 255) as u8,
        }
    }

    /// The XEP-0392 hue in degrees.
    pub fn hue(&self) -> f64 {
        xep0392::raw_to_degrees(self.hue_raw)
    }

    /// A 64-bit seed for the motion engine's idle quirks.
    pub fn seed(&self) -> u64 {
        u64::from(self.hue_raw) << 16 | (self.look as u64) << 8 | u64::from(self.roundness)
    }

    /// The XEP-0392 colour (saturation 100, lightness 50, as in the spec's
    /// test vectors): the flat look's head colour, and a colour to use for
    /// this JID elsewhere.
    pub fn accent(&self) -> Rgb {
        hsluv(self.hue(), xep0392::DEFAULT_SATURATION, xep0392::DEFAULT_LIGHTNESS)
    }

    /// The colours to paint this face with.
    pub fn palette(&self) -> Palette {
        let h = self.hue();
        let c = |dh: f64, s: f64, l: f64| hsluv(h + dh, s, l);
        match self.look {
            Look::Flat => {
                let head = self.accent();
                Palette {
                    base: [head; 4],
                    eye: Rgb::WHITE,
                    rim: head,
                    antenna: [head; 3],
                    shine: 0.0,
                    shade: 0.0,
                }
            }
            // Gems: a pale top, a saturated middle and a deep shadow; light eyes.
            Look::Gem => Palette {
                base: [
                    c(0.0, 85.0, 78.0),
                    c(0.0, 100.0, 48.0),
                    c(0.0, 100.0, 28.0),
                    c(0.0, 100.0, 12.0),
                ],
                eye: c(0.0, 60.0, 97.0),
                rim: c(0.0, 85.0, 85.0),
                antenna: [c(0.0, 80.0, 90.0), c(0.0, 100.0, 48.0), c(0.0, 100.0, 12.0)],
                shine: 0.7,
                shade: 0.55,
            },
            // Gradients run through neighbouring hues, like Ocean's cyan to indigo.
            Look::Gradient => Palette {
                base: [
                    c(-25.0, 75.0, 90.0),
                    c(0.0, 85.0, 76.0),
                    c(20.0, 85.0, 58.0),
                    c(40.0, 80.0, 40.0),
                ],
                eye: c(40.0, 70.0, 15.0),
                rim: c(0.0, 40.0, 97.0),
                antenna: [c(-25.0, 40.0, 97.0), c(0.0, 85.0, 76.0), c(40.0, 80.0, 40.0)],
                shine: 0.8,
                shade: 0.3,
            },
            Look::Ball => Palette {
                base: [
                    c(0.0, 90.0, 72.0),
                    c(0.0, 100.0, 58.0),
                    c(0.0, 100.0, 45.0),
                    c(0.0, 100.0, 25.0),
                ],
                eye: c(0.0, 50.0, 97.0),
                rim: Rgb::WHITE,
                antenna: [c(0.0, 90.0, 72.0), c(0.0, 100.0, 58.0), c(0.0, 100.0, 25.0)],
                shine: 0.55,
                shade: 0.45,
            },
        }
    }
}

/// Colours for one face, light to dark, in the shape of a BoopFace skin
/// (`base`, `eye`, `rim`, `shine`, `shade`, `antenna`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub base: [Rgb; 4],
    pub eye: Rgb,
    pub rim: Rgb,
    pub antenna: [Rgb; 3],
    /// Strength of the white highlight (0 = none).
    pub shine: f64,
    /// Strength of the darkening toward the edge (0 = none).
    pub shade: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_jid_same_face() {
        let a = FaceParams::from_jid("alice@localhost");
        assert_eq!(a, FaceParams::from_jid("alice@localhost"));
        assert_eq!(
            a,
            FaceParams::from_jid("Alice@LocalHost/phone"),
            "bare JID, case-insensitive"
        );
        assert_ne!(a, FaceParams::from_jid("bob@localhost"));
    }

    /// Golden values: if this fails, every user's faces changed.
    #[test]
    fn mapping_is_stable() {
        let cases = [
            ("juliet@capulet.lit", FaceParams::from_identifier("juliet@capulet.lit")),
            ("alice@localhost", FaceParams::from_jid("alice@localhost")),
            ("bob@localhost", FaceParams::from_jid("bob@localhost")),
            ("agent@localhost", FaceParams::from_jid("agent@localhost")),
        ];
        let got: Vec<String> = cases
            .iter()
            .map(|(jid, p)| format!("{jid} {} {} {}", p.hue_raw, p.look.name(), p.roundness))
            .collect();
        assert_eq!(got, GOLDEN, "\n{got:#?}");
        // The hue is exactly XEP-0392's.
        assert!((cases[0].1.hue() - 209.410400).abs() < 1e-6);
    }

    const GOLDEN: [&str; 4] = [
        "juliet@capulet.lit 38122 flat 34",
        "alice@localhost 14230 gradient 53",
        "bob@localhost 44540 ball 65",
        "agent@localhost 45776 gradient 86",
    ];

    #[test]
    fn roundness_stays_in_range_and_looks_all_occur() {
        let mut seen = std::collections::HashSet::new();
        for i in 0..400 {
            let p = FaceParams::from_jid(&format!("user{i}@example.org"));
            assert!((ROUNDNESS_MIN..=ROUNDNESS_MAX).contains(&p.roundness));
            seen.insert(p.look);
        }
        assert_eq!(seen.len(), Look::ALL.len());
    }

    #[test]
    fn eyes_contrast_with_the_head() {
        for i in 0..200 {
            let p = FaceParams::from_jid(&format!("user{i}@example.org")).palette();
            let (a, b) = (p.eye.luminance(), p.base[1].luminance());
            let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(ratio > 1.9, "user{i}: contrast {ratio:.2}");
        }
    }
}
