//! XEP-0392: Consistent Color Generation, version 1.0.1 (2025-10-03).
//!
//! <https://xmpp.org/extensions/xep-0392.html>
//!
//! - §5.1: hash the UTF-8 identifier with SHA-1, read the first two bytes as a
//!   little-endian 16-bit number, divide by 65536 and multiply by 360.
//! - §5.2: convert the hue angle to RGB with HSLuv. Saturation and lightness
//!   are up to the implementation; the test vectors use 100 and 50.
//!
//! The spec has no colour-vision-deficiency correction any more (removed in
//! 0.8.0), so none is applied here.

use sha1::{Digest, Sha1};

use crate::color::{Rgb, hsluv};

/// Saturation used by the spec's test vectors.
pub const DEFAULT_SATURATION: f64 = 100.0;
/// Lightness used by the spec's test vectors.
pub const DEFAULT_LIGHTNESS: f64 = 50.0;

/// The 16-bit hue value of §5.1, before scaling to degrees.
pub fn hue_raw(identifier: &str) -> u16 {
    let digest = Sha1::digest(identifier.as_bytes());
    u16::from_le_bytes([digest[0], digest[1]])
}

/// Converts a raw 16-bit hue value to degrees in `0.0..360.0`.
pub fn raw_to_degrees(raw: u16) -> f64 {
    f64::from(raw) / 65536.0 * 360.0
}

/// The hue angle of §5.1, in degrees.
pub fn hue_angle(identifier: &str) -> f64 {
    raw_to_degrees(hue_raw(identifier))
}

/// The colour of §5.2 with the given HSLuv saturation and lightness.
pub fn color_with(identifier: &str, saturation: f64, lightness: f64) -> Rgb {
    hsluv(hue_angle(identifier), saturation, lightness)
}

/// The colour of §5.2 with the test-vector saturation (100) and lightness (50).
pub fn color(identifier: &str) -> Rgb {
    color_with(identifier, DEFAULT_SATURATION, DEFAULT_LIGHTNESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §13.1, "full range": text, angle, r, g, b (saturation 100, lightness 50).
    const VECTORS: &[(&str, f64, f64, f64, f64)] = &[
        ("Romeo", 327.255249, 0.865, 0.000, 0.686),
        ("juliet@capulet.lit", 209.410400, 0.000, 0.515, 0.573),
        ("\u{1F63A}", 331.199341, 0.872, 0.000, 0.659),
        ("council", 359.994507, 0.918, 0.000, 0.394),
        ("Board", 171.430664, 0.000, 0.527, 0.457),
    ];

    #[test]
    fn spec_test_vectors() {
        for &(text, angle, r, g, b) in VECTORS {
            let got = hue_angle(text);
            assert!((got - angle).abs() < 5e-7, "{text}: angle {got} != {angle}");
            let c = color(text);
            for (name, got, want) in [("r", c.r, r), ("g", c.g, g), ("b", c.b, b)] {
                assert!((got - want).abs() < 5e-4, "{text}: {name} {got:.4} != {want}");
            }
        }
    }

    #[test]
    fn emoji_input_is_utf8() {
        // The vector table lists the UTF-8 octets f0 9f 98 ba for the cat.
        assert_eq!("\u{1F63A}".as_bytes(), [0xf0, 0x9f, 0x98, 0xba]);
    }
}
