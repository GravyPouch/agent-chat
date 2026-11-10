//! Conversation status → face action. This part is agent-chat's own, not
//! BoopFace's: it decides which BoopFace action fits what is happening in a
//! one-to-one chat.
//!
//! | Signal | Face |
//! |---|---|
//! | the contact is typing | `input` |
//! | we sent a message and wait for the reply | `waiting` |
//! | the message was accepted | a short `success`, then back to waiting |
//! | their reply arrived | `success`, then `idle` |
//! | sending failed, or a message could not be decrypted | `failure` |
//! | the contact went offline / came back | `sleep` / `wake` |
//!
//! Waiting and typing time out, so a reply that never comes (or a lost
//! "paused") doesn't keep the face busy forever.

use std::time::Duration;

use crate::engine::{Action, Engine};

/// Something that happened in the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// The contact is typing (XEP-0085 `composing`).
    Typing,
    /// The contact stopped typing (`paused`, `active`, `inactive` or `gone`).
    StoppedTyping,
    /// We pressed send.
    Sending,
    /// The daemon accepted our message.
    Sent,
    /// A reply from the contact arrived.
    Reply,
    /// Sending failed, or an incoming message could not be decrypted.
    Failed,
    /// The contact's presence changed: available (`true`) or not.
    Online(bool),
}

/// How long the success after sending shows before the face goes back to waiting.
const SENT_SUCCESS_MS: f64 = 1000.0;
/// Give up waiting for a reply after this long.
const WAIT_TIMEOUT_MS: f64 = 120_000.0;
/// Forget "typing" if no other chat state follows within this time.
const TYPING_TIMEOUT_MS: f64 = 45_000.0;

/// Keeps the conversation state and steers an [`Engine`].
#[derive(Debug, Clone)]
pub struct Director {
    online: bool,
    typing_since: Option<f64>,
    awaiting_since: Option<f64>,
    /// A one-shot cut short at this engine time.
    transient_until: Option<f64>,
}

impl Director {
    /// Starts the engine in the resting state for this presence.
    pub fn new(online: bool, engine: &mut Engine) -> Director {
        let director = Director {
            online,
            typing_since: None,
            awaiting_since: None,
            transient_until: None,
        };
        engine.set_action(director.resting());
        director
    }

    /// What the face does when nothing new is happening.
    pub fn resting(&self) -> Action {
        if self.typing_since.is_some() {
            Action::Input
        } else if !self.online {
            Action::Sleep
        } else if self.awaiting_since.is_some() {
            Action::WaitingSpin
        } else {
            Action::Idle
        }
    }

    pub fn is_online(&self) -> bool {
        self.online
    }

    pub fn signal(&mut self, signal: Signal, engine: &mut Engine) {
        let now = engine.now_ms();
        match signal {
            Signal::Typing => self.typing_since = Some(now),
            Signal::StoppedTyping => self.typing_since = None,
            Signal::Sending => self.awaiting_since = Some(now),
            Signal::Sent => {
                self.awaiting_since.get_or_insert(now);
                self.play(engine, Action::Success, Some(SENT_SUCCESS_MS));
                return;
            }
            Signal::Reply => {
                self.typing_since = None;
                self.awaiting_since = None;
                self.play(engine, Action::Success, None);
                return;
            }
            Signal::Failed => {
                self.typing_since = None;
                self.awaiting_since = None;
                self.play(engine, Action::Failure, None);
                return;
            }
            Signal::Online(online) => {
                let was = self.online;
                self.online = online;
                if online && !was && engine.action() == Action::Sleep && self.typing_since.is_none() {
                    self.play(engine, Action::Wake, None);
                    return;
                }
            }
        }
        if self.transient_until.is_none() && !engine.is_playing_one_shot() {
            engine.set_action(self.resting());
        }
    }

    fn play(&mut self, engine: &mut Engine, action: Action, cut_after_ms: Option<f64>) {
        engine.set_action(action);
        self.transient_until = cut_after_ms.map(|ms| engine.now_ms() + ms);
    }

    /// Call after every [`Engine::tick`]: applies timeouts and returns the face
    /// to its resting state once a one-shot is over.
    pub fn update(&mut self, engine: &mut Engine) {
        let now = engine.now_ms();
        if self.typing_since.is_some_and(|t| now - t >= TYPING_TIMEOUT_MS) {
            self.typing_since = None;
        }
        if self.awaiting_since.is_some_and(|t| now - t >= WAIT_TIMEOUT_MS) {
            self.awaiting_since = None;
        }
        if let Some(until) = self.transient_until {
            if now < until {
                return;
            }
            self.transient_until = None;
        } else if engine.is_playing_one_shot() {
            return;
        }
        let resting = self.resting();
        if engine.action() != resting {
            engine.set_action(resting);
        }
    }

    /// Fast-forward to the pose the face comes to rest in, for a host that
    /// isn't animating (reduced motion, or a window in the background): runs
    /// one-shots and transitions to their end, but at most `max` of engine time
    /// (looping actions such as waiting never come to rest).
    pub fn settle(&mut self, engine: &mut Engine, max: Duration) {
        const STEP: Duration = Duration::from_millis(33);
        let mut left = max;
        while engine.next_change() == Some(Duration::ZERO) && left >= STEP {
            engine.tick(STEP);
            self.update(engine);
            left -= STEP;
        }
        // Don't come to rest on the off phase of a blinking text cursor.
        if engine.action() == Action::Input
            && !engine.frame().eyes[0].is_visible()
            && let Some(wait) = engine.next_change()
        {
            engine.tick(wait);
        }
    }

    /// The time until [`Director::update`] has something to do on its own.
    pub fn next_deadline(&self, engine: &Engine) -> Option<Duration> {
        let now = engine.now_ms();
        [
            self.transient_until,
            self.typing_since.map(|t| t + TYPING_TIMEOUT_MS),
            self.awaiting_since.map(|t| t + WAIT_TIMEOUT_MS),
        ]
        .into_iter()
        .flatten()
        .map(|at| Duration::from_secs_f64((at - now).max(0.0) / 1000.0))
        .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineConfig;

    fn setup(online: bool) -> (Engine, Director) {
        let mut engine = Engine::new(EngineConfig::default(), 42);
        let director = Director::new(online, &mut engine);
        (engine, director)
    }

    fn advance(engine: &mut Engine, director: &mut Director, ms: u64) {
        for _ in 0..ms / 50 {
            engine.tick(Duration::from_millis(50));
            director.update(engine);
        }
    }

    #[test]
    fn send_wait_reply_cycle() {
        let (mut e, mut d) = setup(true);
        assert_eq!(e.action(), Action::Idle);
        d.signal(Signal::Sending, &mut e);
        assert_eq!(e.action(), Action::WaitingSpin);
        d.signal(Signal::Sent, &mut e);
        assert_eq!(e.action(), Action::Success);
        advance(&mut e, &mut d, 1100);
        assert_eq!(e.action(), Action::WaitingSpin, "back to waiting for the reply");
        d.signal(Signal::Typing, &mut e);
        assert_eq!(e.action(), Action::Input);
        d.signal(Signal::StoppedTyping, &mut e);
        assert_eq!(e.action(), Action::WaitingSpin);
        d.signal(Signal::Reply, &mut e);
        assert_eq!(e.action(), Action::Success);
        advance(&mut e, &mut d, 2100);
        assert_eq!(e.action(), Action::Idle);
    }

    #[test]
    fn failure_then_idle() {
        let (mut e, mut d) = setup(true);
        d.signal(Signal::Sending, &mut e);
        d.signal(Signal::Failed, &mut e);
        assert_eq!(e.action(), Action::Failure);
        advance(&mut e, &mut d, 3000);
        assert_eq!(e.action(), Action::Idle, "a failed send isn't awaited");
    }

    #[test]
    fn presence_sleeps_and_wakes() {
        let (mut e, mut d) = setup(false);
        assert_eq!(e.action(), Action::Sleep);
        d.signal(Signal::Online(true), &mut e);
        assert_eq!(e.action(), Action::Wake);
        advance(&mut e, &mut d, 1000);
        assert_eq!(e.action(), Action::Idle);
        d.signal(Signal::Online(false), &mut e);
        assert_eq!(e.action(), Action::Sleep);
        // A one-shot while offline returns to sleep.
        d.signal(Signal::Reply, &mut e);
        advance(&mut e, &mut d, 2200);
        assert_eq!(e.action(), Action::Sleep);
    }

    #[test]
    fn waiting_and_typing_time_out() {
        let (mut e, mut d) = setup(true);
        d.signal(Signal::Typing, &mut e);
        assert!(d.next_deadline(&e).unwrap() <= Duration::from_millis(45_000));
        advance(&mut e, &mut d, 46_000);
        assert_eq!(e.action(), Action::Idle);
        d.signal(Signal::Sending, &mut e);
        advance(&mut e, &mut d, 121_000);
        assert_eq!(e.action(), Action::Idle);
        assert_eq!(d.next_deadline(&e), None);
    }

    #[test]
    fn settle_lands_on_the_resting_pose() {
        let (mut e, mut d) = setup(true);
        d.signal(Signal::Typing, &mut e);
        d.settle(&mut e, Duration::from_secs(5));
        assert_eq!(e.action(), Action::Input);
        let cursor = e.frame().eyes[0];
        assert!(cursor.corner.is_some() && cursor.is_visible());

        d.signal(Signal::Reply, &mut e);
        d.settle(&mut e, Duration::from_secs(5));
        assert_eq!(e.action(), Action::Idle, "the success played to its end");

        d.signal(Signal::Online(false), &mut e);
        d.settle(&mut e, Duration::from_secs(5));
        assert_eq!(e.next_change(), None, "asleep, eyes closed");
    }

    #[test]
    fn deterministic_transitions() {
        let script = || {
            let (mut e, mut d) = setup(true);
            let mut trace = vec![];
            for (i, s) in [
                Signal::Sending,
                Signal::Sent,
                Signal::Typing,
                Signal::Reply,
                Signal::Online(false),
            ]
            .into_iter()
            .enumerate()
            {
                d.signal(s, &mut e);
                advance(&mut e, &mut d, 300 * (i as u64 + 1));
                trace.push((e.action(), e.frame()));
            }
            trace
        };
        assert_eq!(script(), script());
    }
}
