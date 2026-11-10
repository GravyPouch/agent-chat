//! BoopFace avatars (via the `boopface` crate, a port of BoopFace by CX ART Lab,
//! MIT): static faces as SVG images, and the animated face of the open chat.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use boopface::{Director, Engine, EngineConfig, FaceParams, Signal, VIEW_BOX};
use gpui_kit::component::Icon;
use gpui_kit::*;

/// Static face images by (identifier, size). Generating and hashing the SVG
/// happens once per face and size; GPUI's asset cache keeps the raster.
#[derive(Default)]
struct FaceCache {
    images: HashMap<(String, u32, bool), Arc<Image>>,
}

impl Global for FaceCache {}

fn cached(key: String, size: u32, head_only: bool, params: FaceParams, cx: &mut App) -> Arc<Image> {
    cx.default_global::<FaceCache>()
        .images
        .entry((key, size, head_only))
        .or_insert_with(|| {
            let svg = if head_only {
                boopface::render_head_svg(&params, size)
            } else {
                boopface::render_svg(&params, size)
            };
            Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes()))
        })
        .clone()
}

/// The face of a JID (resource and case ignored).
pub fn jid_face(jid: &str, size: u32, cx: &mut App) -> Img {
    let key = boopface::params::normalize_jid(jid);
    let params = FaceParams::from_identifier(&key);
    face_img(cached(key, size, false, params, cx), size)
}

/// The face of a room occupant known only by nickname (Dino colours MUC
/// occupants by nickname as well).
pub fn nick_face(nick: &str, size: u32, cx: &mut App) -> Img {
    let params = FaceParams::from_identifier(nick);
    face_img(cached(format!("nick:{nick}"), size, false, params, cx), size)
}

fn face_img(image: Arc<Image>, size: u32) -> Img {
    img(image).size(px(size as f32)).flex_none()
}

/// Rooms get no face: a face stands for one person or agent, a room for a
/// place. They get a rounded tile in the room's XEP-0392 colour instead.
pub fn room_tile(room_jid: &str, size: f32, icon: impl Into<Icon>) -> Div {
    let params = FaceParams::from_jid(room_jid);
    let [r, g, b] = params.accent().to_u8();
    div()
        .size(px(size))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(size * 0.28))
        .bg(rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)))
        .text_color(white())
        .child(Icon::new(icon).size(px(size * 0.55)))
}

// ----------------------------------------------------------------- animated

/// Size of the animated face in the conversation header.
pub const ANIMATED_SIZE: u32 = 72;
/// About 30 frames per second while something moves.
const FRAME: Duration = Duration::from_millis(33);
/// How far to fast-forward when not animating.
const SETTLE_MAX: Duration = Duration::from_secs(4);

/// The animated face of the open 1:1 chat. Ticks only while the window is
/// active and the face is moving: between glances and blinks, or asleep, no
/// timer runs. With reduced motion, or while the window is in the background,
/// it jumps straight to the pose for the chat's state (typing, asleep, …).
pub struct AnimatedFace {
    jid: String,
    engine: Engine,
    director: Director,
    head: Arc<Image>,
    eye: Hsla,
    last_tick: Instant,
    window_active: bool,
    reduce_motion: bool,
    timer: Option<Task<()>>,
    _activation: Subscription,
}

impl AnimatedFace {
    pub fn new(jid: &str, online: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let key = boopface::params::normalize_jid(jid);
        let params = FaceParams::from_identifier(&key);
        let mut engine = Engine::new(EngineConfig::default(), params.seed());
        let director = Director::new(online, &mut engine);
        let [r, g, b] = params.palette().eye.to_u8();
        let activation = cx.observe_window_activation(window, |this, window, cx| {
            this.window_active = window.is_window_active();
            if !this.window_active {
                // Don't freeze mid-blink or mid-glance.
                this.advance();
                this.director.settle(&mut this.engine, SETTLE_MAX);
                cx.notify();
            }
            this.schedule(cx);
        });
        let mut face = AnimatedFace {
            jid: key.clone(),
            engine,
            director,
            head: cached(key, ANIMATED_SIZE, true, params, cx),
            eye: rgb(u32::from(r) << 16 | u32::from(g) << 8 | u32::from(b)).into(),
            last_tick: Instant::now(),
            window_active: window.is_window_active(),
            reduce_motion: cx.reduce_motion(),
            timer: None,
            _activation: activation,
        };
        // Open in the resting pose, e.g. already asleep.
        face.director.settle(&mut face.engine, SETTLE_MAX);
        face.schedule(cx);
        face
    }

    /// The bare JID (lower-cased) this face belongs to.
    pub fn jid(&self) -> &str {
        &self.jid
    }

    pub fn is_for(&self, jid: &str) -> bool {
        boopface::params::normalize_jid(jid) == self.jid
    }

    /// Something happened in the chat.
    pub fn signal(&mut self, signal: Signal, cx: &mut Context<Self>) {
        self.advance();
        self.director.signal(signal, &mut self.engine);
        if !self.animating(cx) {
            self.director.settle(&mut self.engine, SETTLE_MAX);
        }
        cx.notify();
        self.schedule(cx);
    }

    fn animating(&mut self, cx: &App) -> bool {
        self.reduce_motion = cx.reduce_motion();
        self.window_active && !self.reduce_motion
    }

    /// Bring the engine up to the wall clock.
    fn advance(&mut self) {
        let now = Instant::now();
        let dt = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        self.engine.tick(dt);
        self.director.update(&mut self.engine);
    }

    fn step(&mut self, cx: &mut Context<Self>) {
        self.timer = None;
        self.advance();
        cx.notify();
        self.schedule(cx);
    }

    /// Plan the next frame, or none if nothing will move.
    fn schedule(&mut self, cx: &mut Context<Self>) {
        if !self.animating(cx) {
            self.timer = None;
            return;
        }
        let wait = [self.engine.next_change(), self.director.next_deadline(&self.engine)]
            .into_iter()
            .flatten()
            .min();
        let Some(wait) = wait else {
            self.timer = None;
            return;
        };
        let wait = wait.max(FRAME);
        self.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this, cx| this.step(cx));
        }));
    }
}

impl Render for AnimatedFace {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let size = px(ANIMATED_SIZE as f32);
        let frame = self.engine.frame();
        let eye = self.eye;
        div()
            .relative()
            .size(size)
            .flex_none()
            .child(face_img(self.head.clone(), ANIMATED_SIZE).absolute().top_0().left_0())
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let scale = bounds.size.width / VIEW_BOX[2];
                        for e in &frame.eyes {
                            let points: Vec<Point<Pixels>> = e
                                .outline(48)
                                .into_iter()
                                .map(|(x, y)| {
                                    point(
                                        bounds.origin.x + scale * (x - VIEW_BOX[0]),
                                        bounds.origin.y + scale * (y - VIEW_BOX[1]),
                                    )
                                })
                                .collect();
                            if points.len() < 3 {
                                continue;
                            }
                            let mut path = PathBuilder::fill();
                            path.add_polygon(&points, true);
                            if let Ok(path) = path.build() {
                                window.paint_path(path, eye);
                            }
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
    }
}
