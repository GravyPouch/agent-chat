// Ported from BoopFace (MIT, Copyright (c) 2026 CX ART Lab):
//   src/boopface-core.js     the SVG shell: head, eyes, eyelids, head clip (`_renderShell`, `_draw`)
//   src/boopface-antenna.js  the antenna dot (120, 12), radius 15, in the head colour
//   src/skins/native.js      the layered-gradient skin painter (`build`, `paintSkin`,
//                            `applyLight` at the resting light), for the gem, gradient
//                            and ball looks
//   src/skins/eyelids.js     lids cut the eyes instead of painting over them
// See third_party/boopface/LICENSE.

//! SVG output for GPUI's `img()` (resvg/usvg).

use std::fmt::Write as _;

use crate::color::Rgb;
use crate::engine::{Eye, Frame};
use crate::geometry::{head_outline, path_data};
use crate::params::{FaceParams, Look};

/// The face's viewBox: BoopFace's 240 units, moved up so the antenna dot
/// (which pokes out above the 240 box in BoopFace) is inside and the whole
/// robot is centred.
pub const VIEW_BOX: [f32; 4] = [0.0, -19.0, 240.0, 240.0];

/// The antenna dot (`ANTENNA_DEFAULTS` in boopface-antenna.js).
const ANTENNA: (f32, f32, f32) = (120.0, 12.0, 15.0);

/// Segments for an eye outline cut by the lids.
const EYE_SEGMENTS: usize = 48;

/// The static idle face, `size` × `size` CSS pixels.
pub fn render_svg(params: &FaceParams, size: u32) -> String {
    render_frame_svg(params, Some(&Frame::idle()), size)
}

/// The face without eyes, for drawing animated eyes on top.
pub fn render_head_svg(params: &FaceParams, size: u32) -> String {
    render_frame_svg(params, None, size)
}

/// The face with the eyes of `frame` (none if `None`).
pub fn render_frame_svg(params: &FaceParams, frame: Option<&Frame>, size: u32) -> String {
    let palette = params.palette();
    let head = path_data(&head_outline(params.roundness));
    let [vx, vy, vw, vh] = VIEW_BOX;
    let mut s = String::with_capacity(12 * 1024);
    let _ = write!(
        s,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="{vx} {vy} {vw} {vh}">"#
    );

    let flat = params.look == Look::Flat;
    s.push_str("<defs>");
    let _ = write!(s, r#"<clipPath id="headClip"><path d="{head}"/></clipPath>"#);
    if !flat {
        skin_defs(&mut s, params.look, &palette);
    }
    s.push_str("</defs>");

    if flat {
        let _ = write!(s, r#"<path d="{head}" fill="{}"/>"#, palette.base[0].to_hex());
    } else {
        // Base paint, then shine, volume, edge reflection and rim, as in `build`.
        for fill in ["url(#skinBase)", "url(#skinShine)", "url(#skinVolume)", "url(#skinEdge)"] {
            let _ = write!(s, r#"<path d="{head}" fill="{fill}"/>"#);
        }
        let _ = write!(s, r#"<path d="{head}" fill="none" stroke="url(#skinRim)" stroke-width="1.25"/>"#);
        if params.look == Look::Ball {
            for fill in ["url(#skinBallShade)", "url(#skinBallHot)"] {
                let _ = write!(s, r#"<path d="{head}" fill="{fill}"/>"#);
            }
        }
    }

    if let Some(frame) = frame {
        s.push_str(r#"<g clip-path="url(#headClip)">"#);
        for eye in &frame.eyes {
            eye_svg(&mut s, eye, palette.eye);
        }
        s.push_str("</g>");
    }

    let (ax, ay, ar) = ANTENNA;
    if flat {
        let _ = write!(s, r#"<circle cx="{ax}" cy="{ay}" r="{ar}" fill="{}"/>"#, palette.antenna[0].to_hex());
    } else {
        let _ = write!(
            s,
            r#"<circle cx="{ax}" cy="{ay}" r="{ar}" fill="url(#skinAntenna)" stroke="{}" stroke-width="0.7"/>"#,
            palette.rim.to_hex()
        );
    }
    s.push_str("</svg>");
    s
}

fn eye_svg(s: &mut String, eye: &Eye, color: Rgb) {
    if !eye.is_visible() {
        return;
    }
    let fill = color.to_hex();
    if eye.lids_clear() {
        let rotate = if eye.angle.abs() > 0.001 {
            format!(r#" transform="rotate({:.2} {:.2} {:.2})""#, eye.angle, eye.cx, eye.cy)
        } else {
            String::new()
        };
        match eye.corner {
            None => {
                let _ = write!(
                    s,
                    r#"<ellipse cx="{:.2}" cy="{:.2}" rx="{:.2}" ry="{:.2}" fill="{fill}"{rotate}/>"#,
                    eye.cx, eye.cy, eye.rx, eye.ry
                );
            }
            Some((kx, ky)) => {
                let _ = write!(
                    s,
                    r#"<rect x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" rx="{:.2}" ry="{:.2}" fill="{fill}"{rotate}/>"#,
                    eye.cx - eye.rx,
                    eye.cy - eye.ry,
                    eye.rx * 2.0,
                    eye.ry * 2.0,
                    kx.min(eye.rx),
                    ky.min(eye.ry)
                );
            }
        }
        return;
    }
    let outline = eye.outline(EYE_SEGMENTS);
    if outline.len() < 3 {
        return;
    }
    let points: Vec<(f64, f64)> = outline.iter().map(|&(x, y)| (f64::from(x), f64::from(y))).collect();
    let _ = write!(s, r#"<path d="{}" fill="{fill}"/>"#, path_data(&points));
}

/// One `<stop>`; opacity 1 is left out.
fn stop(s: &mut String, offset: f64, color: Rgb, opacity: f64) {
    let _ = write!(s, r#"<stop offset="{offset:.3}" stop-color="{}""#, color.to_hex());
    if opacity < 1.0 {
        let _ = write!(s, r#" stop-opacity="{:.3}""#, opacity.max(0.0));
    }
    s.push_str("/>");
}

fn radial(s: &mut String, id: &str, cx: f64, cy: f64, r: f64, stops: &[(f64, Rgb, f64)]) {
    let _ = write!(s, r#"<radialGradient id="{id}" cx="{cx:.3}" cy="{cy:.3}" r="{r:.3}">"#);
    for &(o, c, a) in stops {
        stop(s, o, c, a);
    }
    s.push_str("</radialGradient>");
}

fn linear(s: &mut String, id: &str, [x1, y1, x2, y2]: [f64; 4], stops: &[(f64, Rgb, f64)]) {
    let _ = write!(s, r#"<linearGradient id="{id}" x1="{x1:.3}" y1="{y1:.3}" x2="{x2:.3}" y2="{y2:.3}">"#);
    for &(o, c, a) in stops {
        stop(s, o, c, a);
    }
    s.push_str("</linearGradient>");
}

/// The gradients of `paintSkin`, placed where `applyLight` puts them for the
/// resting light (x = y = -0.5, up and to the left).
fn skin_defs(s: &mut String, look: Look, p: &crate::params::Palette) {
    const LIGHT: (f64, f64) = (-0.5, -0.5);
    let (lx, ly) = LIGHT;
    let (cx, cy) = (0.5 + lx * 0.30, 0.30 + ly * 0.17);
    let white = Rgb::WHITE;
    let ball = look == Look::Ball;
    let dark = if ball { p.base[3] } else { Rgb::from_hex("#091126").unwrap_or(Rgb::BLACK) };
    let reflect = if ball { p.base[0] } else { p.rim };
    let base_stops: Vec<(f64, Rgb, f64)> = p.base.iter().enumerate().map(|(i, &c)| (i as f64 / 3.0, c, 1.0)).collect();

    if look == Look::Gradient {
        linear(s, "skinBase", [0.05, 0.05, 0.85, 0.98], &base_stops);
    } else {
        radial(s, "skinBase", cx, cy, 0.95, &base_stops);
    }
    radial(
        s,
        "skinShine",
        cx,
        cy - 0.10,
        0.70,
        &[(0.0, white, p.shine), (0.23, white, p.shine * 0.30), (0.65, white, 0.035), (1.0, white, 0.0)],
    );
    radial(
        s,
        "skinVolume",
        0.5 + lx * 0.12,
        0.35 + ly * 0.08,
        0.76,
        &[(0.0, dark, 0.0), (0.58, dark, 0.0), (0.82, dark, p.shade * 0.27), (1.0, dark, p.shade)],
    );
    linear(
        s,
        "skinRim",
        [0.5 + lx * 0.5, 0.0, 0.5 - lx * 0.5, 1.0],
        &[(0.0, white, 0.95), (0.28, p.rim, 0.60), (0.52, p.rim, 0.06), (0.82, p.rim, 0.13), (1.0, p.rim, 0.65)],
    );
    radial(
        s,
        "skinEdge",
        0.5 - lx * 0.42,
        0.75 - ly * 0.12,
        0.5,
        &[(0.0, reflect, 0.5), (0.45, reflect, 0.16), (1.0, reflect, 0.0)],
    );
    radial(
        s,
        "skinAntenna",
        cx,
        cy.max(0.1),
        0.85,
        &[(0.0, p.antenna[0], 1.0), (0.43, p.antenna[1], 1.0), (1.0, p.antenna[2], 1.0)],
    );
    if ball {
        radial(
            s,
            "skinBallShade",
            0.46,
            0.4,
            0.74,
            &[(0.0, dark, 0.0), (0.55, dark, 0.0), (0.84, dark, 0.2), (1.0, dark, 0.5)],
        );
        radial(
            s,
            "skinBallHot",
            cx,
            cy - 0.10,
            0.2,
            &[(0.0, white, 0.62), (0.45, white, 0.22), (1.0, white, 0.0)],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Action, Engine, EngineConfig};
    use std::time::Duration;

    fn check_svg(svg: &str) -> usvg::Tree {
        let doc = roxmltree::Document::parse(svg).unwrap_or_else(|e| panic!("not well-formed: {e}\n{svg}"));
        let root = doc.root_element();
        assert_eq!(root.tag_name().name(), "svg");
        assert_eq!(root.tag_name().namespace(), Some("http://www.w3.org/2000/svg"));
        usvg::Tree::from_str(svg, &usvg::Options::default()).unwrap_or_else(|e| panic!("usvg rejects it: {e}"))
    }

    fn count_paths(group: &usvg::Group) -> usize {
        group
            .children()
            .iter()
            .map(|node| match node {
                usvg::Node::Group(g) => count_paths(g),
                usvg::Node::Path(_) => 1,
                _ => 0,
            })
            .sum()
    }

    fn all_looks() -> Vec<FaceParams> {
        Look::ALL
            .iter()
            .enumerate()
            .map(|(i, &look)| FaceParams { hue_raw: (i as u16) * 13_000, look, roundness: 20 + i as u8 * 20 })
            .collect()
    }

    #[test]
    fn static_faces_are_well_formed_svg() {
        for p in all_looks().into_iter().chain([FaceParams::from_jid("alice@localhost")]) {
            for size in [24, 48, 96] {
                let svg = render_svg(&p, size);
                let tree = check_svg(&svg);
                assert_eq!(tree.size().width(), size as f32, "{:?}", p.look);
                // Head, two eyes and the antenna all made it into the render tree.
                assert!(count_paths(tree.root()) >= 4, "{:?}", p.look);
                assert!(svg.contains("<ellipse"), "idle eyes are plain ellipses");
            }
            check_svg(&render_head_svg(&p, 64));
        }
    }

    #[test]
    fn every_action_frame_is_well_formed_svg() {
        let p = FaceParams::from_jid("agent@localhost");
        for action in Action::ALL {
            let mut e = Engine::new(EngineConfig::default(), 1);
            e.set_action(action);
            for _ in 0..30 {
                let frame = e.tick(Duration::from_millis(100));
                check_svg(&render_frame_svg(&p, Some(&frame), 64));
            }
        }
    }

    #[test]
    fn rendering_is_deterministic() {
        let p = FaceParams::from_jid("bob@localhost");
        assert_eq!(render_svg(&p, 48), render_svg(&p, 48));
    }
}
