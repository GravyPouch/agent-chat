//! Colours: HSLuv → sRGB and a few helpers.
//!
//! HSLuv follows the reference algorithm published at <https://www.hsluv.org>
//! (MIT). XEP-0392 names HSLuv as its colour space.

/// An sRGB colour with components in `0.0..=1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgb {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

impl Rgb {
    pub const WHITE: Rgb = Rgb { r: 1.0, g: 1.0, b: 1.0 };
    pub const BLACK: Rgb = Rgb { r: 0.0, g: 0.0, b: 0.0 };

    pub fn from_hex(hex: &str) -> Option<Rgb> {
        let hex = hex.strip_prefix('#')?;
        if hex.len() != 6 {
            return None;
        }
        let channel = |i: usize| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .ok()
                .map(|v| f64::from(v) / 255.0)
        };
        Some(Rgb {
            r: channel(0)?,
            g: channel(2)?,
            b: channel(4)?,
        })
    }

    /// 8-bit channels, rounded.
    pub fn to_u8(self) -> [u8; 3] {
        let q = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        [q(self.r), q(self.g), q(self.b)]
    }

    /// `#rrggbb`.
    pub fn to_hex(self) -> String {
        let [r, g, b] = self.to_u8();
        format!("#{r:02x}{g:02x}{b:02x}")
    }

    /// Mixes two colours in sRGB, like BoopFace's `mixHex`; `t = 0` keeps `self`.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        Rgb {
            r: self.r + (other.r - self.r) * t,
            g: self.g + (other.g - self.g) * t,
            b: self.b + (other.b - self.b) * t,
        }
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f64 {
        let lin = |c: f64| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }
}

// sRGB D65 matrix (XYZ → linear RGB) and CIELUV constants from the reference.
const M: [[f64; 3]; 3] = [
    [3.240969941904521, -1.537383177570093, -0.498610760293],
    [-0.96924363628087, 1.87596750150772, 0.041555057407175],
    [0.055630079696993, -0.20397695888897, 1.056971514242878],
];
const REF_U: f64 = 0.19783000664283;
const REF_V: f64 = 0.46831999493879;
const KAPPA: f64 = 903.2962962;
const EPSILON: f64 = 0.0088564516;

/// The six lines that bound the sRGB gamut at lightness `l` in the LCh(uv) plane.
fn bounds(l: f64) -> [(f64, f64); 6] {
    let sub1 = (l + 16.0).powi(3) / 1_560_896.0;
    let sub2 = if sub1 > EPSILON { sub1 } else { l / KAPPA };
    let mut out = [(0.0, 0.0); 6];
    for (c, row) in M.iter().enumerate() {
        let [m1, m2, m3] = *row;
        for t in 0..2 {
            let t = f64::from(t);
            let top1 = (284_517.0 * m1 - 94_839.0 * m3) * sub2;
            let top2 = (838_422.0 * m3 + 769_860.0 * m2 + 731_718.0 * m1) * l * sub2 - 769_860.0 * t * l;
            let bottom = (632_260.0 * m3 - 126_452.0 * m2) * sub2 + 126_452.0 * t;
            out[c * 2 + t as usize] = (top1 / bottom, top2 / bottom);
        }
    }
    out
}

fn max_chroma(l: f64, h: f64) -> f64 {
    let hrad = h.to_radians();
    bounds(l)
        .iter()
        .map(|&(slope, intercept)| intercept / (hrad.sin() - slope * hrad.cos()))
        .filter(|len| *len >= 0.0)
        .fold(f64::INFINITY, f64::min)
}

fn y_from_l(l: f64) -> f64 {
    if l <= 8.0 {
        l / KAPPA
    } else {
        ((l + 16.0) / 116.0).powi(3)
    }
}

fn from_linear(c: f64) -> f64 {
    if c <= 0.003_130_8 {
        12.92 * c
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

/// HSLuv (hue in degrees, saturation and lightness in `0..=100`) to sRGB.
pub fn hsluv(h: f64, s: f64, l: f64) -> Rgb {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 100.0);
    let l = l.clamp(0.0, 100.0);
    // HSLuv → LCh(uv)
    let c = if l > 99.999_999_9 || l < 0.000_000_01 {
        0.0
    } else {
        max_chroma(l, h) / 100.0 * s
    };
    if l <= 0.0 {
        return Rgb::BLACK;
    }
    // LCh → Luv → XYZ
    let hrad = h.to_radians();
    let (u, v) = (hrad.cos() * c, hrad.sin() * c);
    let var_u = u / (13.0 * l) + REF_U;
    let var_v = v / (13.0 * l) + REF_V;
    let y = y_from_l(l);
    let x = -(9.0 * y * var_u) / ((var_u - 4.0) * var_v - var_u * var_v);
    let z = (9.0 * y - 15.0 * var_v * y - var_v * x) / (3.0 * var_v);
    // XYZ → sRGB
    let dot = |row: [f64; 3]| row[0] * x + row[1] * y + row[2] * z;
    Rgb {
        r: from_linear(dot(M[0])).clamp(0.0, 1.0),
        g: from_linear(dot(M[1])).clamp(0.0, 1.0),
        b: from_linear(dot(M[2])).clamp(0.0, 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsluv_reference_points() {
        // Known values from the HSLuv reference snapshot.
        assert_eq!(hsluv(0.0, 0.0, 100.0).to_hex(), "#ffffff");
        assert_eq!(hsluv(0.0, 0.0, 0.0).to_hex(), "#000000");
        assert_eq!(
            hsluv(12.177_050_630_061_776, 100.0, 53.237_115_595_429_54).to_hex(),
            "#ff0000"
        );
        assert_eq!(
            hsluv(127.715_012_949_240_96, 100.0, 87.735_550_352_036_2).to_hex(),
            "#00ff00"
        );
        assert_eq!(
            hsluv(265.874_528_754_118_5, 100.0, 32.300_740_312_419_1).to_hex(),
            "#0000ff"
        );
    }

    #[test]
    fn hex_round_trip_and_mix() {
        let c = Rgb::from_hex("#3cc3e0").unwrap();
        assert_eq!(c.to_hex(), "#3cc3e0");
        assert_eq!(Rgb::WHITE.mix(Rgb::BLACK, 0.5).to_hex(), "#808080");
        assert!(Rgb::from_hex("3cc3e0").is_none());
    }
}
