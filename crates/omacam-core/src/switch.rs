//! The stream state machine: which camera streams, and getting from one
//! to the next.
//!
//! One camera streams at a time on gs201 (all CSIS links share one write
//! DMA context), so a lens switch is a stop and a start. The machine keeps
//! at most one start in flight: while a camera is starting, newer requests
//! only replace the wanted target, and the machine moves on to it once the
//! start completes. A pinch through three lenses thus costs at most two
//! starts, not one per lens it passed.
//!
//! The UI freezes the last frame while [`SwitchController::is_switching`]
//! and keeps applying the zoom to it (CAMERA-APP-PLAN §3.4 step 1).

use std::time::{Duration, Instant};

use crate::backend::{Controls, Purpose};

/// What should be streaming.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// Index into the lens table.
    pub lens: usize,
    /// The camera's id: what makes two targets the same stream, since the
    /// lens table's indices change when cameras come and go.
    pub camera: String,
    pub purpose: Purpose,
    /// Start values (exposure hints, autofocus mode). Not part of identity.
    pub controls: Controls,
}

impl Target {
    pub fn new(lens: usize, purpose: Purpose) -> Target {
        Target {
            lens,
            camera: format!("lens{lens}"),
            purpose,
            controls: Controls::default(),
        }
    }

    fn same_stream(&self, other: &Target) -> bool {
        self.camera == other.camera && self.purpose == other.purpose
    }
}

/// What the machine asks of the backend.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Start `target` as stream `generation`. Starting stops any other
    /// stream first.
    Start {
        generation: u64,
        target: Target,
    },
    Stop {
        generation: u64,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Idle,
    Starting {
        generation: u64,
        target: Target,
    },
    Streaming {
        generation: u64,
        target: Target,
        got_frame: bool,
    },
    Stopping {
        generation: u64,
    },
}

/// How a frame relates to the current stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// From an older stream: drop it.
    Stale,
    /// The first frame of the current stream.
    First,
    Current,
}

#[derive(Debug, Clone)]
pub struct SwitchController {
    state: State,
    wanted: Option<Target>,
    next_generation: u64,
    /// When the stream that's starting was first wanted.
    switch_began: Option<Instant>,
    last_switch: Option<Duration>,
}

impl Default for SwitchController {
    fn default() -> Self {
        Self::new()
    }
}

impl SwitchController {
    pub fn new() -> SwitchController {
        SwitchController {
            state: State::Idle,
            wanted: None,
            next_generation: 1,
            switch_began: None,
            last_switch: None,
        }
    }

    /// The stream that delivers frames now, if any.
    pub fn streaming(&self) -> Option<(u64, &Target)> {
        match &self.state {
            State::Streaming {
                generation, target, ..
            } => Some((*generation, target)),
            _ => None,
        }
    }

    pub fn wanted(&self) -> Option<&Target> {
        self.wanted.as_ref()
    }

    /// True from a switch request until the new stream's first frame.
    pub fn is_switching(&self) -> bool {
        match (&self.state, &self.wanted) {
            (_, None) => matches!(self.state, State::Starting { .. }),
            (
                State::Streaming {
                    target, got_frame, ..
                },
                Some(w),
            ) => !*got_frame || !target.same_stream(w),
            _ => true,
        }
    }

    /// Time from the request to the first frame of the last switch.
    pub fn last_switch_time(&self) -> Option<Duration> {
        self.last_switch
    }

    /// The lens table was rebuilt: point the targets at their cameras' new
    /// indices. A target whose camera is gone keeps `usize::MAX`, which
    /// nothing indexes (its stream is ending).
    pub fn remap(&mut self, index_of: impl Fn(&str) -> Option<usize>) {
        let fix = |t: &mut Target| t.lens = index_of(&t.camera).unwrap_or(usize::MAX);
        match &mut self.state {
            State::Starting { target, .. } | State::Streaming { target, .. } => fix(target),
            _ => {}
        }
        if let Some(w) = self.wanted.as_mut() {
            fix(w);
        }
    }

    /// Ask for `target` (None: stop streaming).
    pub fn request(&mut self, target: Option<Target>, now: Instant) -> Vec<Action> {
        let changes = match (&self.wanted, &target) {
            (Some(a), Some(b)) => !a.same_stream(b),
            (None, None) => false,
            _ => true,
        };
        if changes && target.is_some() && self.switch_began.is_none() {
            self.switch_began = Some(now);
        }
        if target.is_none() {
            self.switch_began = None;
        }
        self.wanted = target;
        self.drive()
    }

    pub fn on_started(&mut self, generation: u64) -> Vec<Action> {
        if let State::Starting {
            generation: g,
            target,
        } = &self.state
        {
            if *g == generation {
                self.state = State::Streaming {
                    generation,
                    target: target.clone(),
                    got_frame: false,
                };
            }
        }
        self.drive()
    }

    pub fn on_frame(&mut self, generation: u64, now: Instant) -> FrameKind {
        match &mut self.state {
            State::Streaming {
                generation: g,
                got_frame,
                target,
            } if *g == generation => {
                if *got_frame {
                    FrameKind::Current
                } else {
                    *got_frame = true;
                    let settled = self.wanted.as_ref().is_some_and(|w| w.same_stream(target));
                    if settled {
                        if let Some(began) = self.switch_began.take() {
                            self.last_switch = Some(now.duration_since(began));
                        }
                    }
                    FrameKind::First
                }
            }
            _ => FrameKind::Stale,
        }
    }

    pub fn on_stopped(&mut self, generation: u64) -> Vec<Action> {
        let ours = match &self.state {
            State::Stopping { generation: g } => *g == generation,
            // The backend stopped it on its own (the camera went away).
            State::Streaming { generation: g, .. } | State::Starting { generation: g, .. } => {
                *g == generation
            }
            State::Idle => false,
        };
        if ours {
            self.state = State::Idle;
        }
        self.drive()
    }

    /// A start failed or the stream broke. The machine goes idle and drops
    /// its wish: retrying is the caller's choice.
    pub fn on_error(&mut self, generation: Option<u64>) -> Vec<Action> {
        let current = match &self.state {
            State::Starting { generation: g, .. }
            | State::Streaming { generation: g, .. }
            | State::Stopping { generation: g } => Some(*g),
            State::Idle => None,
        };
        if generation.is_none() || generation == current {
            let failed = match &self.state {
                State::Starting { target, .. } | State::Streaming { target, .. } => {
                    Some(target.clone())
                }
                _ => None,
            };
            self.state = State::Idle;
            // A newer wish than the failed stream still stands: start it.
            let newer = match (&self.wanted, &failed) {
                (Some(w), Some(f)) => !w.same_stream(f),
                _ => false,
            };
            if generation.is_some() && newer {
                return self.drive();
            }
            self.wanted = None;
            self.switch_began = None;
        }
        Vec::new()
    }

    fn start(&mut self, target: Target) -> Vec<Action> {
        let generation = self.next_generation;
        self.next_generation += 1;
        self.state = State::Starting {
            generation,
            target: target.clone(),
        };
        vec![Action::Start { generation, target }]
    }

    fn drive(&mut self) -> Vec<Action> {
        match (&self.state, &self.wanted) {
            (State::Idle, Some(w)) => self.start(w.clone()),
            (State::Streaming { generation, .. }, None) => {
                let generation = *generation;
                self.state = State::Stopping { generation };
                vec![Action::Stop { generation }]
            }
            (State::Streaming { target, .. }, Some(w)) if !target.same_stream(w) => {
                self.start(w.clone())
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(lens: usize) -> Option<Target> {
        Some(Target::new(lens, Purpose::Preview))
    }

    fn started(actions: &[Action]) -> u64 {
        match actions {
            [Action::Start { generation, .. }] => *generation,
            other => panic!("expected one start, got {other:?}"),
        }
    }

    #[test]
    fn start_stream_and_first_frame() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g = started(&s.request(t(1), now));
        assert!(s.is_switching());
        assert_eq!(s.on_frame(g, now), FrameKind::Stale, "not started yet");
        assert!(s.on_started(g).is_empty());
        assert!(s.is_switching(), "no frame yet");
        assert_eq!(
            s.on_frame(g, now + Duration::from_millis(300)),
            FrameKind::First
        );
        assert_eq!(s.on_frame(g, now), FrameKind::Current);
        assert!(!s.is_switching());
        assert_eq!(s.last_switch_time(), Some(Duration::from_millis(300)));
    }

    #[test]
    fn a_switch_starts_the_new_lens_and_drops_old_frames() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g1 = started(&s.request(t(1), now));
        s.on_started(g1);
        s.on_frame(g1, now);
        let g2 = started(&s.request(t(2), now));
        assert!(g2 > g1);
        assert!(s.is_switching());
        assert_eq!(s.on_frame(g1, now), FrameKind::Stale);
        s.on_started(g2);
        assert_eq!(s.on_frame(g2, now), FrameKind::First);
        assert_eq!(s.streaming().unwrap().1.lens, 2);
    }

    #[test]
    fn requests_during_a_start_coalesce_into_one_more_start() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g1 = started(&s.request(t(0), now));
        // A pinch passes through two more lenses while the first starts.
        assert!(s.request(t(1), now).is_empty());
        assert!(s.request(t(2), now).is_empty());
        let next = s.on_started(g1);
        let g2 = started(&next);
        match &next[0] {
            Action::Start { target, .. } => assert_eq!(target.lens, 2),
            _ => unreachable!(),
        }
        s.on_started(g2);
        assert_eq!(s.on_frame(g2, now), FrameKind::First);
    }

    #[test]
    fn asking_for_the_streaming_lens_again_costs_nothing() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g = started(&s.request(t(1), now));
        assert!(s.request(t(2), now).is_empty());
        // Back to 1 before 1 even started: no further start.
        assert!(s.request(t(1), now).is_empty());
        assert!(s.on_started(g).is_empty());
        assert!(s.request(t(1), now).is_empty());
    }

    #[test]
    fn stop_and_resume() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g = started(&s.request(t(1), now));
        s.on_started(g);
        assert_eq!(s.request(None, now), vec![Action::Stop { generation: g }]);
        assert!(!s.is_switching());
        // A resume while the stop is in flight waits for it.
        assert!(s.request(t(1), now).is_empty());
        let g2 = started(&s.on_stopped(g));
        assert!(g2 > g);
    }

    #[test]
    fn a_failed_start_keeps_a_newer_wish() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g = started(&s.request(t(1), now));
        assert!(s.request(t(2), now).is_empty(), "waits for the start");
        let next = s.on_error(Some(g));
        let g2 = started(&next);
        assert!(g2 > g);
        assert_eq!(s.wanted().unwrap().lens, 2);
    }

    #[test]
    fn a_stream_the_backend_stops_on_its_own_starts_again() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g = started(&s.request(t(1), now));
        s.on_started(g);
        // The camera went away and the backend stopped the stream: the
        // wish still stands, so a new start goes out.
        let g2 = started(&s.on_stopped(g));
        assert!(g2 > g);
        // An old generation's stop changes nothing.
        assert!(s.on_stopped(g).is_empty());
    }

    #[test]
    fn errors_go_idle_and_ignore_other_generations() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g = started(&s.request(t(1), now));
        assert!(s.on_error(Some(g + 100)).is_empty());
        assert!(s.wanted().is_some());
        s.on_error(Some(g));
        assert!(s.wanted().is_none());
        assert!(!s.is_switching());
        // A retry starts a fresh generation.
        assert!(started(&s.request(t(1), now)) > g);
    }

    #[test]
    fn still_and_preview_of_the_same_lens_are_different_streams() {
        let now = Instant::now();
        let mut s = SwitchController::new();
        let g = started(&s.request(t(1), now));
        s.on_started(g);
        let still = Some(Target::new(1, Purpose::Still));
        let g2 = started(&s.request(still, now));
        assert!(g2 > g);
    }
}
