// Ported from BoopFace Embedded (MIT, Copyright (c) 2026 CX ART Lab):
//   embedded/lib/BoopFace/src/BoopFace.h, BoopFace.cpp  the clip player (Action,
//     webPose, poseAt, mix, edge squash, crossfade, waiting loops, cursor blink)
//   embedded/lib/BoopFace/src/BoopFaceWebMotion.h       the sampled clips (web_motion.rs)
// and from the BoopFace Web component (src/boopface-core.js): the idle gaze
// wander (`_updateLook`) and the randomised blink (`_updateBlink`), which here
// draw from a seeded generator instead of `Math.random()`.
// See third_party/boopface/LICENSE.
//
// Not ported: the 1-bit rasteriser, display layouts, the gaze tracker and the
// v0.1 waiting variant's legacy geometry beyond what `WaitingWave` needs.

//! The BoopFace motion engine: actions in, eye geometry out.

use std::f32::consts::PI;
use std::time::Duration;

use crate::web_motion::CLIPS;

/// Where the C++ engine's 160 × 100 face area sits in the 240-unit face.
pub const FACE_AREA_X: f32 = 40.0;
pub const FACE_AREA_Y: f32 = 76.0;

/// Something for the face to do. Numbering and names follow BoopFace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Idle,
    Bored,
    WaitingSpin,
    WaitingWrap,
    Input,
    Affirm,
    Success,
    Failure,
    Wary,
    Inspect,
    Angry,
    Refuse,
    Surprise,
    Love,
    Random,
    Sleep,
    Wake,
    WaitingWave,
}

impl Action {
    pub const ALL: [Action; 18] = [
        Action::Idle,
        Action::Bored,
        Action::WaitingSpin,
        Action::WaitingWrap,
        Action::Input,
        Action::Affirm,
        Action::Success,
        Action::Failure,
        Action::Wary,
        Action::Inspect,
        Action::Angry,
        Action::Refuse,
        Action::Surprise,
        Action::Love,
        Action::Random,
        Action::Sleep,
        Action::Wake,
        Action::WaitingWave,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Action::Idle => "idle",
            Action::Bored => "bored",
            Action::WaitingSpin => "waiting-spin",
            Action::WaitingWrap => "waiting-wrap",
            Action::Input => "input",
            Action::Affirm => "affirm",
            Action::Success => "success",
            Action::Failure => "failure",
            Action::Wary => "wary",
            Action::Inspect => "inspect",
            Action::Angry => "angry",
            Action::Refuse => "refuse",
            Action::Surprise => "surprise",
            Action::Love => "love",
            Action::Random => "random",
            Action::Sleep => "sleep",
            Action::Wake => "wake",
            Action::WaitingWave => "waiting-wave",
        }
    }

    /// Parses a BoopFace action name, including `waiting` and the deprecated
    /// aliases BoopFace still accepts.
    pub fn parse(name: &str) -> Option<Action> {
        if let Some(a) = Action::ALL.iter().find(|a| a.name() == name) {
            return Some(*a);
        }
        Some(match name {
            "waiting" | "wait" => Action::WaitingSpin,
            "send" => Action::Affirm,
            "failed" | "fail" => Action::Failure,
            "warning" => Action::Wary,
            "verify" | "review" => Action::Inspect,
            "blocked" | "policy-blocked" => Action::Angry,
            "error" | "system-error" | "connection-error" => Action::Refuse,
            "slot" => Action::Random,
            _ => return None,
        })
    }

    /// How long a one-shot action plays before the face returns to idle, in
    /// milliseconds; 0 for actions that hold until replaced.
    pub fn duration_ms(self) -> u32 {
        match self {
            Action::Bored => 4000,
            Action::Affirm => 1360,
            Action::Success => 2000,
            Action::Failure => 2720,
            Action::Wary => 3850,
            Action::Inspect => 6870,
            Action::Angry => 1980,
            Action::Refuse => 1840,
            Action::Surprise => 1770,
            Action::Love => 3190,
            Action::Random => 2260,
            Action::Wake => 750,
            _ => 0,
        }
    }

    pub fn is_waiting(self) -> bool {
        matches!(self, Action::WaitingSpin | Action::WaitingWrap | Action::WaitingWave)
    }

    pub fn is_one_shot(self) -> bool {
        self.duration_ms() > 0
    }

    fn clip_index(self) -> usize {
        self as usize
    }
}

/// One eye, in the face's 240-unit coordinates.
///
/// The eye is an ellipse (or, when `corner` is set, a rounded rectangle) with
/// radii `rx`/`ry`, rotated by `angle` degrees about its centre. The lids cut
/// it along two lines in the eye's own rotated frame: points with
/// `y < top + top_slope * x` or `y > bottom + bottom_slope * x` are hidden.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Eye {
    pub cx: f32,
    pub cy: f32,
    pub rx: f32,
    pub ry: f32,
    pub angle: f32,
    pub top: f32,
    pub bottom: f32,
    pub top_slope: f32,
    pub bottom_slope: f32,
    /// Corner radii of the rounded-rectangle (text cursor) shape.
    pub corner: Option<(f32, f32)>,
}

impl Eye {
    pub fn is_visible(&self) -> bool {
        self.rx >= 0.05 && self.ry >= 0.05
    }

    /// True if the lids don't touch the shape (`noClip` in BoopFace.cpp).
    pub fn lids_clear(&self) -> bool {
        self.top <= -self.ry - self.top_slope.abs() * self.rx
            && self.bottom >= self.ry + self.bottom_slope.abs() * self.rx
    }

    /// The visible outline as a convex polygon in face coordinates: the shape
    /// sampled with `segments` points, cut by the lids, rotated and placed.
    /// Empty if nothing is visible.
    pub fn outline(&self, segments: usize) -> Vec<(f32, f32)> {
        if !self.is_visible() {
            return Vec::new();
        }
        let segments = segments.max(8);
        let mut local: Vec<(f32, f32)> = Vec::with_capacity(segments + 4);
        match self.corner {
            None => {
                for i in 0..segments {
                    let t = i as f32 / segments as f32 * 2.0 * PI;
                    local.push((self.rx * t.cos(), self.ry * t.sin()));
                }
            }
            Some((kx, ky)) => {
                let (kx, ky) = (kx.min(self.rx).max(0.0), ky.min(self.ry).max(0.0));
                let per_corner = (segments / 4).max(2);
                // Corner centres, counter-clockwise from the lower right in y-down.
                let corners = [
                    (self.rx - kx, self.ry - ky, 0.0),
                    (-(self.rx - kx), self.ry - ky, 0.5 * PI),
                    (-(self.rx - kx), -(self.ry - ky), PI),
                    (self.rx - kx, -(self.ry - ky), 1.5 * PI),
                ];
                for (ox, oy, start) in corners {
                    for i in 0..=per_corner {
                        let t = start + i as f32 / per_corner as f32 * 0.5 * PI;
                        local.push((ox + kx * t.cos(), oy + ky * t.sin()));
                    }
                }
            }
        }
        if !self.lids_clear() {
            // Keep y >= top + top_slope * x, then y <= bottom + bottom_slope * x.
            local = clip_half_plane(&local, |(x, y)| y - (self.top + self.top_slope * x));
            local = clip_half_plane(&local, |(x, y)| (self.bottom + self.bottom_slope * x) - y);
        }
        let (s, c) = self.angle.to_radians().sin_cos();
        local
            .into_iter()
            .map(|(x, y)| (self.cx + c * x - s * y, self.cy + s * x + c * y))
            .collect()
    }
}

/// Sutherland–Hodgman against one half-plane: keeps points where `inside >= 0`.
fn clip_half_plane(poly: &[(f32, f32)], inside: impl Fn((f32, f32)) -> f32) -> Vec<(f32, f32)> {
    let mut out = Vec::with_capacity(poly.len() + 2);
    for i in 0..poly.len() {
        let a = poly[i];
        let b = poly[(i + 1) % poly.len()];
        let (da, db) = (inside(a), inside(b));
        if da >= 0.0 {
            out.push(a);
        }
        if (da >= 0.0) != (db >= 0.0) {
            let t = da / (da - db);
            out.push((a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t));
        }
    }
    out
}

/// What to draw: both eyes. The head and antenna don't move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub eyes: [Eye; 2],
}

impl Frame {
    /// The neutral idle pose (first sample of the idle clip).
    pub fn idle() -> Frame {
        web_pose(Action::Idle, 0.0).to_frame()
    }
}

/// Engine settings (`EyesConfig` in BoopFace Embedded, minus the display).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    /// Crossfade between actions, in ms (BoopFace Embedded default: 240).
    pub transition_ms: u16,
    /// Blink now and then while idle.
    pub auto_blink: bool,
    /// Let the eyes glance around while idle, like the Web component.
    pub wander: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig { transition_ms: 240, auto_blink: true, wander: true }
    }
}

// --------------------------------------------------------------------- poses

/// One eye in the C++ engine's face-area units (`Eye` in BoopFace.h).
#[derive(Debug, Clone, Copy, PartialEq)]
struct RawEye {
    x: f32,
    y: f32,
    rx: f32,
    ry: f32,
    angle: f32,
    top: f32,
    bottom: f32,
    top_slope: f32,
    bottom_slope: f32,
    /// Negative: an ellipse.
    corner: f32,
    corner_y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Pose {
    left: RawEye,
    right: RawEye,
}

impl RawEye {
    fn to_eye(self) -> Eye {
        Eye {
            cx: self.x + FACE_AREA_X,
            cy: self.y + FACE_AREA_Y,
            rx: self.rx.max(0.0),
            ry: self.ry.max(0.0),
            angle: self.angle,
            top: self.top,
            bottom: self.bottom,
            top_slope: self.top_slope,
            bottom_slope: self.bottom_slope,
            corner: (self.corner >= 0.0).then_some((self.corner, self.corner_y)),
        }
    }
}

impl Pose {
    fn to_frame(self) -> Frame {
        Frame { eyes: [self.left.to_eye(), self.right.to_eye()] }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn smooth(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `webPose`: sample a clip at `elapsed` ms, interpolating between the 20 ms samples.
fn web_pose(action: Action, elapsed: f64) -> Pose {
    let clip = &CLIPS[action.clip_index()];
    let index = (elapsed.max(0.0) / 20.0).floor() as usize;
    let a = index.min(clip.count - 1);
    let b = (a + 1).min(clip.count - 1);
    let t = ((elapsed.max(0.0) - index as f64 * 20.0) / 20.0) as f32;
    let eye = |side: usize| {
        let mut v = [0f32; 10];
        for (j, slot) in v.iter_mut().enumerate() {
            let scale = if j == 7 || j == 8 { 4096.0 } else { 100.0 };
            let first = f32::from(clip.values[a * 20 + side * 10 + j]) / scale;
            let second = f32::from(clip.values[b * 20 + side * 10 + j]) / scale;
            *slot = if j == 9 { first } else { lerp(first, second, t) };
        }
        let corner = if v[9] < 0.0 { -1.0 } else { v[9].min(v[2].min(v[3])) };
        RawEye {
            x: v[0],
            y: v[1],
            rx: v[2],
            ry: v[3],
            angle: v[4],
            top: v[5],
            bottom: v[6],
            top_slope: v[7],
            bottom_slope: v[8],
            corner,
            corner_y: corner,
        }
    };
    Pose { left: eye(0), right: eye(1) }
}

/// `mixEye`: interpolate two eyes, morphing ellipses and rounded rectangles.
fn mix_eye(a: &RawEye, b: &RawEye, t: f32) -> RawEye {
    if t <= 0.0 {
        return *a;
    }
    if t >= 1.0 {
        return *b;
    }
    let ellipses = a.corner < 0.0 && b.corner < 0.0;
    let ax = if a.corner < 0.0 { a.rx } else { a.corner.min(a.rx) };
    let ay = if a.corner < 0.0 { a.ry } else { a.corner_y.min(a.ry) };
    let bx = if b.corner < 0.0 { b.rx } else { b.corner.min(b.rx) };
    let by = if b.corner < 0.0 { b.ry } else { b.corner_y.min(b.ry) };
    RawEye {
        x: lerp(a.x, b.x, t),
        y: lerp(a.y, b.y, t),
        rx: lerp(a.rx, b.rx, t),
        ry: lerp(a.ry, b.ry, t),
        angle: lerp(a.angle, b.angle, t),
        top: lerp(a.top, b.top, t),
        bottom: lerp(a.bottom, b.bottom, t),
        top_slope: lerp(a.top_slope, b.top_slope, t),
        bottom_slope: lerp(a.bottom_slope, b.bottom_slope, t),
        corner: if ellipses { -1.0 } else { lerp(ax, bx, t) },
        corner_y: if ellipses { -1.0 } else { lerp(ay, by, t) },
    }
}

fn mix(a: &Pose, b: &Pose, t: f32) -> Pose {
    Pose { left: mix_eye(&a.left, &b.left, t), right: mix_eye(&a.right, &b.right, t) }
}

/// Within 58 units of the head edge an eye narrows (BoopFace.cpp `edgeFactor`).
fn edge_factor(distance: f32) -> f32 {
    (1.0 - distance / 58.0).clamp(0.0, 1.0)
}

// -------------------------------------------------------------------- random

/// SplitMix64: small, fast and the same on every platform.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`, like `Math.random()`.
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

// -------------------------------------------------------------------- engine

#[derive(Debug, Clone, Copy)]
struct Blink {
    start: f64,
    duration: f64,
}

/// Plays BoopFace actions and produces one [`Frame`] per tick.
///
/// Time only moves through [`Engine::tick`], and all randomness comes from
/// the seed, so the same seed and the same ticks give the same frames.
#[derive(Debug, Clone)]
pub struct Engine {
    config: EngineConfig,
    rng: Rng,
    /// Engine clock, in ms.
    now: f64,
    action: Action,
    started: f64,
    from: Pose,
    crossfade: bool,
    // Idle gaze, in face units (the Web component's `_look` and `_wanderTarget`).
    look: (f32, f32),
    look_target: (f32, f32),
    next_wander_at: f64,
    blink: Option<Blink>,
    next_blink_at: f64,
}

/// How long the sleep clip takes to close the eyes (`sleepRendered_` in BoopFace.cpp).
const SLEEP_SETTLE_MS: f64 = 3400.0;
/// The input clip's length; after it only the cursor blink changes.
const INPUT_CLIP_MS: f64 = 800.0;
/// Text cursor blink half-period.
const CURSOR_MS: f64 = 265.0;

impl Engine {
    pub fn new(config: EngineConfig, seed: u64) -> Engine {
        let neutral = web_pose(Action::Idle, 0.0);
        let mut engine = Engine {
            config,
            rng: Rng(seed),
            now: 0.0,
            action: Action::Idle,
            started: 0.0,
            from: neutral,
            crossfade: false,
            look: (0.0, 0.0),
            look_target: (0.0, 0.0),
            next_wander_at: 500.0,
            blink: None,
            next_blink_at: 0.0,
        };
        engine.next_blink_at = 1800.0 + f64::from(engine.rng.next_f32()) * 1800.0;
        engine
    }

    pub fn config(&self) -> EngineConfig {
        self.config
    }

    /// The action now playing. One-shot actions report `Idle` once they end.
    pub fn action(&self) -> Action {
        if self.action.is_one_shot() && self.elapsed() >= f64::from(self.action.duration_ms()) {
            Action::Idle
        } else {
            self.action
        }
    }

    /// True while a one-shot action is still playing.
    pub fn is_playing_one_shot(&self) -> bool {
        self.action.is_one_shot() && self.elapsed() < f64::from(self.action.duration_ms())
    }

    /// Engine time in milliseconds since creation.
    pub fn now_ms(&self) -> f64 {
        self.now
    }

    fn elapsed(&self) -> f64 {
        self.now - self.started
    }

    /// Start an action now (`Eyes::play`). Starting the action that is already
    /// holding (idle, waiting, input or sleep) changes nothing.
    pub fn set_action(&mut self, action: Action) {
        let current = self.action();
        if action == current
            && (action == Action::Idle || action.is_waiting() || action == Action::Sleep || action == Action::Input)
        {
            return;
        }
        self.from = self.pose_at(self.now);
        let cursor_shape = self.from.left.corner >= 0.0 || self.from.right.corner >= 0.0;
        self.crossfade = (current != Action::Idle || action == Action::WaitingWave || cursor_shape)
            && (action != Action::Wake || cursor_shape);
        self.action = action;
        self.started = self.now;
        self.blink = None;
        if action == Action::Idle {
            self.enter_idle();
        } else {
            // Expressions look straight ahead; the crossfade hides the jump.
            self.look = (0.0, 0.0);
            self.look_target = (0.0, 0.0);
        }
    }

    fn enter_idle(&mut self) {
        self.look_target = (0.0, 0.0);
        self.next_wander_at = self.now + 1150.0 + f64::from(self.rng.next_f32()) * 1900.0;
        self.next_blink_at = self.now + 1900.0 + f64::from(self.rng.next_f32()) * 2600.0;
    }

    /// Advance the clock by `dt` and return the frame to draw.
    pub fn tick(&mut self, dt: Duration) -> Frame {
        let dt_ms = dt.as_secs_f64() * 1000.0;
        self.now += dt_ms;
        let total = f64::from(self.action.duration_ms());
        if total > 0.0 && self.elapsed() >= total {
            // Idle starts here with nothing to fade from.
            self.crossfade = false;
            self.action = Action::Idle;
            self.started = self.now;
            self.blink = None;
            self.look = (0.0, 0.0);
            self.enter_idle();
        }
        if self.action == Action::Idle {
            self.update_idle(dt_ms);
        }
        self.frame()
    }

    /// The frame at the current time, without advancing it.
    pub fn frame(&self) -> Frame {
        self.pose_at(self.now).to_frame()
    }

    /// `_updateLook` and `_updateBlink` from the Web component, seeded.
    fn update_idle(&mut self, dt_ms: f64) {
        let now = self.now;
        if self.config.wander && now >= self.next_wander_at {
            let amp_x = 12.0 + self.rng.next_f32() * 13.0;
            let amp_y = 5.0 + self.rng.next_f32() * 9.0;
            let mut x = (self.rng.next_f32() * 2.0 - 1.0) * amp_x;
            let mut y = (self.rng.next_f32() * 2.0 - 1.0) * amp_y;
            // The eyes reach 48 sideways and 34 up or down, but not both at once.
            let reach = ((x / 48.0).powi(2) + (y / 34.0).powi(2)).sqrt();
            if reach > 1.0 {
                x /= reach;
                y /= reach;
            }
            self.look_target = (x, y);
            self.next_wander_at = now + 1150.0 + f64::from(self.rng.next_f32()) * 1900.0;
        }
        let k = 1.0 - (1.0 - 0.16f32).powf((dt_ms / 16.67) as f32);
        self.look.0 = lerp(self.look.0, self.look_target.0, k);
        self.look.1 = lerp(self.look.1, self.look_target.1, k);
        if (self.look.0 - self.look_target.0).abs() < 0.05 && (self.look.1 - self.look_target.1).abs() < 0.05 {
            self.look = self.look_target;
        }

        if let Some(b) = self.blink {
            if now - b.start >= b.duration {
                self.blink = None;
                self.next_blink_at = now + 1900.0 + f64::from(self.rng.next_f32()) * 2600.0;
            }
        } else if self.config.auto_blink && now >= self.next_blink_at {
            self.blink = Some(Blink { start: now, duration: 150.0 + f64::from(self.rng.next_f32()) * 70.0 });
        }
    }

    /// `poseAt`, with the Web component's gaze and blink in place of the
    /// embedded engine's fixed ones.
    fn pose_at(&self, now: f64) -> Pose {
        let mut elapsed = now - self.started;
        let total = f64::from(self.action.duration_ms());
        let selected = if total > 0.0 && elapsed >= total { Action::Idle } else { self.action };
        if selected == Action::WaitingSpin && elapsed >= 120.0 {
            elapsed = 120.0 + (elapsed - 120.0) % 3200.0;
        }
        if selected == Action::WaitingWrap && elapsed >= 120.0 {
            elapsed = 120.0 + (elapsed - 120.0) % 2400.0;
        }
        let mut p = if selected == Action::WaitingWave {
            // The v0.1 waiting motion, kept by BoopFace Embedded as a variant.
            let phase = ((elapsed % 1400.0) as f32) * 2.0 * PI / 1400.0;
            let eye = |x: f32, y: f32| RawEye {
                x,
                y,
                rx: 27.0,
                ry: 29.0,
                angle: 0.0,
                top: -100.0,
                bottom: 100.0,
                top_slope: 0.0,
                bottom_slope: 0.0,
                corner: -1.0,
                corner_y: -1.0,
            };
            Pose { left: eye(46.0, 50.0 + phase.sin() * 10.0), right: eye(114.0, 50.0 + (phase + 0.7).sin() * 10.0) }
        } else {
            web_pose(selected, elapsed)
        };
        if selected == Action::Input && p.left.corner >= 0.0 && ((now / CURSOR_MS) as u64) % 2 == 1 {
            p.left.rx = 0.0;
            p.right.rx = 0.0;
        }
        if selected == Action::Idle && self.action == Action::Idle {
            let (dx, dy) = self.look;
            for (side, e) in [&mut p.left, &mut p.right].into_iter().enumerate() {
                e.y += dy;
                if dx == 0.0 {
                    continue;
                }
                let outward = if side == 1 { 1.0 } else { -1.0 };
                let edge = if side == 1 { 168.0 } else { -8.0 };
                // Apply only the change from the resting edge squash.
                let f0 = edge_factor(outward * (edge - e.x));
                let f = edge_factor(outward * (edge - e.x - dx));
                let w0 = 1.0 - 0.22 * f0 * f0;
                let w = 1.0 - 0.22 * f * f;
                let rx = e.rx / w0 * w;
                e.x += dx - outward * 2.0 * (e.rx - rx) * 0.58;
                e.rx = rx;
                e.ry = e.ry / (1.0 - 0.08 * f0 * f0) * (1.0 - 0.08 * f * f);
            }
            if let Some(b) = self.blink {
                let phase = (((now - b.start) / b.duration) as f32).clamp(0.0, 1.0);
                let open = if phase < 0.5 { lerp(1.0, 0.08, phase * 2.0) } else { lerp(0.08, 1.0, (phase - 0.5) * 2.0) };
                for e in [&mut p.left, &mut p.right] {
                    e.ry = (e.ry * open).max(1.3);
                    e.y += (58.0 - 2.0 * e.ry) * 0.02;
                }
            }
        }
        let command_elapsed = now - self.started;
        let transition = f64::from(self.config.transition_ms);
        if self.crossfade && transition > 0.0 && command_elapsed < transition {
            p = mix(&self.from, &p, smooth((command_elapsed / transition) as f32));
        }
        p
    }

    /// When the picture next changes without a new action: `Some(ZERO)` while
    /// something is moving, the wait until the next blink, glance or cursor
    /// flip, or `None` if it holds still until [`Engine::set_action`].
    /// Lets a host stop its frame timer while the face is at rest.
    pub fn next_change(&self) -> Option<Duration> {
        let elapsed = self.elapsed();
        let ms = |v: f64| Some(Duration::from_secs_f64(v.max(0.0) / 1000.0));
        if self.crossfade && elapsed < f64::from(self.config.transition_ms) {
            return ms(0.0);
        }
        match self.action() {
            Action::Idle if self.action.is_one_shot() => ms(0.0), // ends on the next tick
            Action::Idle => {
                if self.blink.is_some() || self.look != self.look_target {
                    return ms(0.0);
                }
                let mut next = f64::INFINITY;
                if self.config.auto_blink {
                    next = next.min(self.next_blink_at);
                }
                if self.config.wander {
                    next = next.min(self.next_wander_at);
                }
                next.is_finite().then(|| ms(next - self.now)).flatten()
            }
            Action::Input if elapsed >= INPUT_CLIP_MS => ms(CURSOR_MS - self.now % CURSOR_MS),
            Action::Sleep if elapsed >= SLEEP_SETTLE_MS => None,
            _ => ms(0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(33);

    fn run(engine: &mut Engine, ms: u64) -> Vec<Frame> {
        (0..ms / 33).map(|_| engine.tick(FRAME)).collect()
    }

    #[test]
    fn names_round_trip_and_aliases() {
        for a in Action::ALL {
            assert_eq!(Action::parse(a.name()), Some(a));
        }
        assert_eq!(Action::parse("waiting"), Some(Action::WaitingSpin));
        assert_eq!(Action::parse("blocked"), Some(Action::Angry));
        assert_eq!(Action::parse("nope"), None);
        assert_eq!(CLIPS.len(), 17, "one clip per action before WaitingWave");
    }

    #[test]
    fn neutral_pose_matches_the_web_face() {
        let f = Frame::idle();
        let [l, r] = f.eyes;
        // BoopFace's idle eyes: centres (86, 126) and (154, 126), rx 27, ry 29.
        assert!((l.cx - 86.0).abs() < 0.05 && (r.cx - 154.0).abs() < 0.05);
        assert!((l.cy - 126.0).abs() < 0.05 && (l.rx - 27.0).abs() < 0.05 && (l.ry - 29.0).abs() < 0.05);
        assert!(l.lids_clear() && l.corner.is_none());
    }

    #[test]
    fn same_seed_same_frames() {
        let script = |seed| {
            let mut e = Engine::new(EngineConfig::default(), seed);
            let mut frames = run(&mut e, 5000);
            e.set_action(Action::Success);
            frames.extend(run(&mut e, 2500));
            e.set_action(Action::WaitingSpin);
            frames.extend(run(&mut e, 4000));
            frames
        };
        assert_eq!(script(7), script(7));
        assert_ne!(script(7), script(8), "the seed drives idle glances and blinks");
    }

    #[test]
    fn one_shots_return_to_idle() {
        let mut e = Engine::new(EngineConfig::default(), 1);
        e.set_action(Action::Success);
        assert_eq!(e.action(), Action::Success);
        assert!(e.is_playing_one_shot());
        run(&mut e, 1900);
        assert_eq!(e.action(), Action::Success);
        run(&mut e, 200);
        assert_eq!(e.action(), Action::Idle);
        assert!(!e.is_playing_one_shot());
    }

    #[test]
    fn holding_actions_hold_and_ignore_repeats() {
        let mut e = Engine::new(EngineConfig::default(), 1);
        e.set_action(Action::WaitingSpin);
        let a = run(&mut e, 1000);
        e.set_action(Action::WaitingSpin); // no restart
        let b = run(&mut e, 10_000);
        assert_eq!(e.action(), Action::WaitingSpin);
        assert!(a.iter().chain(&b).any(|f| f.eyes[0].cx != a[0].eyes[0].cx), "the eyes circle");
        assert_eq!(e.next_change(), Some(Duration::ZERO));

        e.set_action(Action::Input);
        run(&mut e, 1000);
        assert_eq!(e.action(), Action::Input);
        let cursor = e.frame().eyes[0];
        assert!(cursor.corner.is_some(), "input turns the eyes into text cursors");
        assert!((cursor.cx - 120.0).abs() < 1.0, "both cursors sit in the middle");
        let wait = e.next_change().unwrap();
        assert!(wait > Duration::ZERO && wait <= Duration::from_millis(265));
    }

    #[test]
    fn sleep_settles_and_wake_opens() {
        let mut e = Engine::new(EngineConfig::default(), 3);
        e.set_action(Action::Sleep);
        run(&mut e, 3000);
        assert_eq!(e.next_change(), Some(Duration::ZERO));
        run(&mut e, 600);
        assert_eq!(e.next_change(), None, "a sleeping face needs no frames");
        let asleep = e.frame().eyes[0];
        assert!(!asleep.lids_clear(), "the lids are closed");
        e.set_action(Action::Wake);
        run(&mut e, 1000);
        assert_eq!(e.action(), Action::Idle);
        let awake = e.frame().eyes[0];
        assert!(awake.ry > 20.0);
    }

    #[test]
    fn idle_rests_between_glances() {
        let mut e = Engine::new(EngineConfig::default(), 11);
        let mut rested = false;
        for _ in 0..400 {
            e.tick(FRAME);
            if let Some(wait) = e.next_change()
                && wait > Duration::ZERO
            {
                rested = true;
                assert!(wait < Duration::from_secs(5));
            }
        }
        assert!(rested, "idle has quiet stretches with no frames needed");
        let still = Engine::new(EngineConfig { auto_blink: false, wander: false, ..Default::default() }, 1);
        assert_eq!(still.next_change(), None);
    }

    #[test]
    fn crossfade_starts_from_the_current_pose() {
        let mut e = Engine::new(EngineConfig::default(), 5);
        e.set_action(Action::Failure);
        let first = e.tick(Duration::from_millis(1));
        let neutral = Frame::idle();
        assert!((first.eyes[0].cx - neutral.eyes[0].cx).abs() < 1.0);
    }

    #[test]
    fn lids_cut_the_outline() {
        let mut eye = Frame::idle().eyes[0];
        let full = eye.outline(64);
        assert_eq!(full.len(), 64);
        eye.top = 0.0; // upper half closed
        let half = eye.outline(64);
        assert!(half.iter().all(|&(_, y)| y >= eye.cy - 0.01), "{half:?}");
        eye.rx = 0.0;
        assert!(eye.outline(64).is_empty());
    }
}
