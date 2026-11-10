// Ported from BoopFace (MIT, Copyright (c) 2026 CX ART Lab):
//   src/boopface-core.js            the head outline (`#headShape`) and its transform
//   src/boopface-geometry.js        flattenHeadPoints
//   src/boopface-head-roundness.js  setHeadRoundness (superellipse mix)
// See third_party/boopface/LICENSE.

//! The BoopFace head outline.

/// The 160-point head outline from BoopFace's `#headShape` path, in its raw
/// coordinates (a 200 × 200 rounded square at 20..220).
#[rustfmt::skip]
const HEAD_POINTS: [(f64, f64); 160] = [
    (120.00, 20.00), (132.80, 20.05), (139.87, 20.20), (145.68, 20.44), (150.79, 20.78), (155.43, 21.22),
    (159.71, 21.76), (163.69, 22.40), (167.44, 23.14), (170.99, 23.97), (174.34, 24.90), (177.53, 25.93),
    (180.57, 27.07), (183.46, 28.30), (186.22, 29.63), (188.85, 31.06), (191.36, 32.59), (193.75, 34.22),
    (196.03, 35.96), (198.19, 37.80), (200.25, 39.75), (202.20, 41.81), (204.04, 43.97), (205.78, 46.25),
    (207.41, 48.64), (208.94, 51.15), (210.37, 53.78), (211.70, 56.54), (212.93, 59.43), (214.07, 62.47),
    (215.10, 65.66), (216.03, 69.01), (216.86, 72.56), (217.60, 76.31), (218.24, 80.29), (218.78, 84.57),
    (219.22, 89.21), (219.56, 94.32), (219.80, 100.13), (219.95, 107.20), (220.00, 120.00), (219.95, 132.80),
    (219.80, 139.87), (219.56, 145.68), (219.22, 150.79), (218.78, 155.43), (218.24, 159.71), (217.60, 163.69),
    (216.86, 167.44), (216.03, 170.99), (215.10, 174.34), (214.07, 177.53), (212.93, 180.57), (211.70, 183.46),
    (210.37, 186.22), (208.94, 188.85), (207.41, 191.36), (205.78, 193.75), (204.04, 196.03), (202.20, 198.19),
    (200.25, 200.25), (198.19, 202.20), (196.03, 204.04), (193.75, 205.78), (191.36, 207.41), (188.85, 208.94),
    (186.22, 210.37), (183.46, 211.70), (180.57, 212.93), (177.53, 214.07), (174.34, 215.10), (170.99, 216.03),
    (167.44, 216.86), (163.69, 217.60), (159.71, 218.24), (155.43, 218.78), (150.79, 219.22), (145.68, 219.56),
    (139.87, 219.80), (132.80, 219.95), (120.00, 220.00), (107.20, 219.95), (100.13, 219.80), (94.32, 219.56),
    (89.21, 219.22), (84.57, 218.78), (80.29, 218.24), (76.31, 217.60), (72.56, 216.86), (69.01, 216.03),
    (65.66, 215.10), (62.47, 214.07), (59.43, 212.93), (56.54, 211.70), (53.78, 210.37), (51.15, 208.94),
    (48.64, 207.41), (46.25, 205.78), (43.97, 204.04), (41.81, 202.20), (39.75, 200.25), (37.80, 198.19),
    (35.96, 196.03), (34.22, 193.75), (32.59, 191.36), (31.06, 188.85), (29.63, 186.22), (28.30, 183.46),
    (27.07, 180.57), (25.93, 177.53), (24.90, 174.34), (23.97, 170.99), (23.14, 167.44), (22.40, 163.69),
    (21.76, 159.71), (21.22, 155.43), (20.78, 150.79), (20.44, 145.68), (20.20, 139.87), (20.05, 132.80),
    (20.00, 120.00), (20.05, 107.20), (20.20, 100.13), (20.44, 94.32), (20.78, 89.21), (21.22, 84.57),
    (21.76, 80.29), (22.40, 76.31), (23.14, 72.56), (23.97, 69.01), (24.90, 65.66), (25.93, 62.47),
    (27.07, 59.43), (28.30, 56.54), (29.63, 53.78), (31.06, 51.15), (32.59, 48.64), (34.22, 46.25),
    (35.96, 43.97), (37.80, 41.81), (39.75, 39.75), (41.81, 37.80), (43.97, 35.96), (46.25, 34.22),
    (48.64, 32.59), (51.15, 31.06), (53.78, 29.63), (56.54, 28.30), (59.43, 27.07), (62.47, 25.93),
    (65.66, 24.90), (69.01, 23.97), (72.56, 23.14), (76.31, 22.40), (80.29, 21.76), (84.57, 21.22),
    (89.21, 20.78), (94.32, 20.44), (100.13, 20.20), (107.20, 20.05),
];

/// BoopFace draws the head with `translate(7.2 7.2) scale(.94)`.
const HEAD_OFFSET: f64 = 7.2;
const HEAD_SCALE: f64 = 0.94;
/// `DEFAULT_HEAD_HEIGHT_SCALE` in boopface-geometry.js.
const HEAD_HEIGHT_SCALE: f64 = 0.9;
/// `DEFAULT_ROUNDNESS` in boopface-head-roundness.js.
pub const DEFAULT_ROUNDNESS: u8 = 50;

fn smooth01(v: f64) -> f64 {
    let t = v.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn bounds(points: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    points.iter().fold(
        (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY),
        |(x0, y0, x1, y1), &(x, y)| (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
    )
}

/// `flattenHeadPoints`: squash the head to `height_scale` of its height while
/// keeping the top and bottom 40 % straight, so the face reads as a wide screen.
fn flatten(points: &[(f64, f64)], height_scale: f64) -> Vec<(f64, f64)> {
    let (_, min_y, _, max_y) = bounds(points);
    let height = max_y - min_y;
    if height <= 0.0 {
        return points.to_vec();
    }
    let scale = height_scale.clamp(0.1, 1.0);
    let center_y = (min_y + max_y) / 2.0;
    let inset = height * (1.0 - scale) / 2.0;
    let upper_lock = min_y + height * 0.40;
    let lower_lock = max_y - height * 0.40;
    points
        .iter()
        .map(|&(x, y)| {
            let shift = if y <= upper_lock {
                inset
            } else if y < center_y {
                inset * smooth01((center_y - y) / (center_y - upper_lock).max(0.001))
            } else if y >= lower_lock {
                -inset
            } else if y > center_y {
                -inset * smooth01((y - center_y) / (lower_lock - center_y).max(0.001))
            } else {
                0.0
            };
            (x, y + shift)
        })
        .collect()
}

fn superellipse_point(angle: f64, cx: f64, cy: f64, half_w: f64, half_h: f64, exponent: f64) -> (f64, f64) {
    let (sin, cos) = angle.sin_cos();
    let x_term = (cos.abs() / half_w.max(0.001)).powf(exponent);
    let y_term = (sin.abs() / half_h.max(0.001)).powf(exponent);
    let denom = (x_term + y_term).powf(1.0 / exponent);
    let radius = if denom > 0.0 { 1.0 / denom } else { 0.0 };
    (cx + cos * radius, cy + sin * radius)
}

/// `targetShape`: the superellipse with the outline's bounds, sampled at each
/// point's angle from the centre.
fn target_shape(points: &[(f64, f64)], exponent: f64) -> Vec<(f64, f64)> {
    let (x0, y0, x1, y1) = bounds(points);
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let (half_w, half_h) = ((x1 - x0) / 2.0, (y1 - y0) / 2.0);
    points
        .iter()
        .map(|&(x, y)| superellipse_point((y - cy).atan2(x - cx), cx, cy, half_w, half_h, exponent))
        .collect()
}

fn mix_points(from: &[(f64, f64)], to: &[(f64, f64)], amount: f64) -> Vec<(f64, f64)> {
    let t = amount.clamp(0.0, 1.0);
    from.iter()
        .zip(to)
        .map(|(&(ax, ay), &(bx, by))| (ax + (bx - ax) * t, ay + (by - ay) * t))
        .collect()
}

/// The head outline for a roundness in `0..=100` (50 is BoopFace's default;
/// lower is squarer, higher is rounder), in the face's 240-unit coordinates.
pub fn head_outline(roundness: u8) -> Vec<(f64, f64)> {
    let baseline = flatten(&HEAD_POINTS, HEAD_HEIGHT_SCALE);
    let roundness = f64::from(roundness.min(100));
    let default = f64::from(DEFAULT_ROUNDNESS);
    let points = if roundness < default {
        mix_points(
            &baseline,
            &target_shape(&baseline, 10.0),
            (default - roundness) / default,
        )
    } else if roundness > default {
        mix_points(
            &baseline,
            &target_shape(&baseline, 2.2),
            (roundness - default) / default,
        )
    } else {
        baseline
    };
    points
        .into_iter()
        .map(|(x, y)| (HEAD_OFFSET + x * HEAD_SCALE, HEAD_OFFSET + y * HEAD_SCALE))
        .collect()
}

/// SVG path data for a closed outline, two decimals like BoopFace's `_pointsToPath`.
pub fn path_data(points: &[(f64, f64)]) -> String {
    let mut d = String::with_capacity(points.len() * 16);
    for (i, (x, y)) in points.iter().enumerate() {
        d.push_str(if i == 0 { "M" } else { " L" });
        d.push_str(&format!(" {x:.2} {y:.2}"));
    }
    d.push_str(" Z");
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_head_is_flattened_and_transformed() {
        let head = head_outline(DEFAULT_ROUNDNESS);
        assert_eq!(head.len(), 160);
        let (x0, y0, x1, y1) = bounds(&head);
        // 20..220 scaled by .94 around 7.2; height flattened to 90 %.
        assert!((x0 - 26.0).abs() < 0.01 && (x1 - 214.0).abs() < 0.01, "{x0} {x1}");
        assert!((y0 - 35.4).abs() < 0.01 && (y1 - 204.6).abs() < 0.01, "{y0} {y1}");
    }

    #[test]
    fn roundness_changes_the_corners_only() {
        let square = head_outline(0);
        let round = head_outline(100);
        // The point at 45° (a corner) moves; the bounds stay.
        let corner = |h: &[(f64, f64)]| h[20];
        assert!(corner(&square).0 > corner(&round).0 + 5.0);
        let (sx0, _, sx1, _) = bounds(&square);
        let (rx0, _, rx1, _) = bounds(&round);
        assert!((sx0 - rx0).abs() < 0.5 && (sx1 - rx1).abs() < 0.5);
    }
}
