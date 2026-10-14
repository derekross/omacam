//! The still-capture sequence (CAMERA-APP-PLAN §3.3).
//!
//! The software ISP gives one processed stream, so a full-resolution still
//! is a short reconfiguration into the camera's large mode:
//!
//! 1. Shutter: if autofocus is still scanning, wait for it (at most
//!    [`FOCUS_WAIT`]); then read the last exposure, gain and colour gains.
//! 2. Reconfigure to the still mode, with those values as start hints.
//! 3. Drop frames until the exposure is back near the locked values (at
//!    least [`MIN_DISCARD`], at most [`MAX_DISCARD`]), then keep one.
//! 4. Reconfigure back to the preview mode with the same hints.
//!
//! Quick mode keeps the next preview frame instead, with no reconfiguring.
//!
//! The machine is pure: the controller feeds it frames and time, and acts
//! on the [`Step`]s it returns.

use std::time::{Duration, Instant};

use crate::backend::{AfState, Controls, FrameMeta};

/// Longest wait for autofocus to finish before taking the picture anyway.
pub const FOCUS_WAIT: Duration = Duration::from_millis(500);
/// Frames to drop at least after the still mode starts (the sensor applies
/// controls two frames late).
pub const MIN_DISCARD: u32 = 2;
/// Frames to drop at most while waiting for the exposure to settle.
pub const MAX_DISCARD: u32 = 6;
/// Longest wait for the still frame before giving up.
pub const STILL_TIMEOUT: Duration = Duration::from_secs(6);
/// How close (relative) exposure × gain must be to the locked value.
pub const SETTLED_WITHIN: f64 = 0.15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A preview frame, at preview resolution.
    Quick,
    /// A frame from the still mode, at full resolution.
    Full,
}

/// What the controller should do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Nothing,
    /// Switch the current lens to its still mode with these hints.
    StartStill(Controls),
    /// Keep the frame just given as the picture.
    Keep,
    /// Keep this frame and go back to the preview with these hints.
    KeepAndRestore(Controls),
    /// Drop this frame.
    Discard,
    /// Give up and go back to the preview.
    Fail(String),
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Idle,
    QuickWait,
    WaitFocus { deadline: Instant },
    Still { seen: u32, deadline: Instant },
    Restoring,
}

#[derive(Debug, Clone)]
pub struct CaptureController {
    state: State,
    locked: Controls,
}

impl Default for CaptureController {
    fn default() -> Self {
        Self::new()
    }
}

/// The controls that hold the exposure and colour of `meta`.
pub fn lock_from(meta: Option<&FrameMeta>) -> Controls {
    let Some(m) = meta else {
        return Controls::default();
    };
    Controls {
        exposure_us: m.exposure_us,
        analogue_gain: m.analogue_gain,
        colour_gains: m.colour_gains,
        lens_position: m.lens_position,
        af_continuous: false,
        focus: None,
    }
}

fn product(c: &Controls) -> Option<f64> {
    Some(c.exposure_us? as f64 * c.analogue_gain.unwrap_or(1.0) as f64)
}

impl CaptureController {
    pub fn new() -> CaptureController {
        CaptureController {
            state: State::Idle,
            locked: Controls::default(),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.state != State::Idle
    }

    /// True while frames belong to the still stream (the preview should
    /// stay frozen).
    pub fn wants_still_stream(&self) -> bool {
        matches!(self.state, State::Still { .. })
    }

    pub fn locked(&self) -> &Controls {
        &self.locked
    }

    /// The shutter was pressed.
    pub fn begin(
        &mut self,
        mode: Mode,
        autofocus: bool,
        last: Option<&FrameMeta>,
        now: Instant,
    ) -> Step {
        if self.is_busy() {
            return Step::Nothing;
        }
        match mode {
            Mode::Quick => {
                self.state = State::QuickWait;
                Step::Nothing
            }
            Mode::Full => {
                let scanning = last.and_then(|m| m.af_state) == Some(AfState::Scanning);
                if autofocus && scanning {
                    self.state = State::WaitFocus {
                        deadline: now + FOCUS_WAIT,
                    };
                    Step::Nothing
                } else {
                    self.go_still(last, now)
                }
            }
        }
    }

    fn go_still(&mut self, last: Option<&FrameMeta>, now: Instant) -> Step {
        self.locked = lock_from(last);
        self.state = State::Still {
            seen: 0,
            deadline: now + STILL_TIMEOUT,
        };
        Step::StartStill(self.locked.clone())
    }

    /// A frame from the preview stream.
    pub fn on_preview_frame(&mut self, meta: &FrameMeta, now: Instant) -> Step {
        match self.state {
            State::QuickWait => {
                self.state = State::Idle;
                Step::Keep
            }
            State::WaitFocus { deadline } => {
                let done = matches!(meta.af_state, Some(AfState::Focused | AfState::Failed));
                if done || now >= deadline {
                    self.go_still(Some(meta), now)
                } else {
                    Step::Nothing
                }
            }
            State::Restoring => {
                self.state = State::Idle;
                Step::Nothing
            }
            _ => Step::Nothing,
        }
    }

    /// A frame from the still stream.
    pub fn on_still_frame(&mut self, meta: &FrameMeta) -> Step {
        let State::Still { seen, .. } = &mut self.state else {
            return Step::Discard;
        };
        *seen += 1;
        let seen = *seen;
        if seen <= MIN_DISCARD {
            return Step::Discard;
        }
        let settled = match (product(&self.locked), product(&lock_from(Some(meta)))) {
            (Some(want), Some(got)) if want > 0.0 => ((got - want) / want).abs() <= SETTLED_WITHIN,
            // Nothing to compare: the minimum wait has to do.
            _ => true,
        };
        if settled || seen > MAX_DISCARD {
            self.state = State::Restoring;
            Step::KeepAndRestore(self.locked.clone())
        } else {
            Step::Discard
        }
    }

    /// Time passes: deadlines.
    pub fn tick(&mut self, last: Option<&FrameMeta>, now: Instant) -> Step {
        match self.state {
            State::WaitFocus { deadline } if now >= deadline => self.go_still(last, now),
            State::Still { deadline, .. } if now >= deadline => {
                self.state = State::Restoring;
                Step::Fail("the still frame didn't arrive".into())
            }
            _ => Step::Nothing,
        }
    }

    /// The stream broke or the app stopped: forget the capture.
    pub fn abort(&mut self) {
        self.state = State::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(exposure: i32, gain: f32, af: Option<AfState>) -> FrameMeta {
        FrameMeta {
            exposure_us: Some(exposure),
            analogue_gain: Some(gain),
            af_state: af,
            ..FrameMeta::default()
        }
    }

    #[test]
    fn quick_mode_keeps_the_next_preview_frame() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        assert_eq!(c.begin(Mode::Quick, true, None, now), Step::Nothing);
        assert!(c.is_busy());
        assert_eq!(c.on_preview_frame(&FrameMeta::default(), now), Step::Keep);
        assert!(!c.is_busy());
    }

    #[test]
    fn full_mode_locks_exposure_discards_then_keeps_and_restores() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        let last = meta(10_000, 2.0, Some(AfState::Focused));
        let step = c.begin(Mode::Full, true, Some(&last), now);
        let locked = match step {
            Step::StartStill(ctl) => ctl,
            other => panic!("{other:?}"),
        };
        assert_eq!(locked.exposure_us, Some(10_000));
        assert_eq!(locked.analogue_gain, Some(2.0));
        assert!(c.wants_still_stream());
        // Two frames always go, then a wrong exposure goes, then a match.
        assert_eq!(c.on_still_frame(&meta(10_000, 2.0, None)), Step::Discard);
        assert_eq!(c.on_still_frame(&meta(10_000, 2.0, None)), Step::Discard);
        assert_eq!(c.on_still_frame(&meta(30_000, 4.0, None)), Step::Discard);
        assert_eq!(
            c.on_still_frame(&meta(11_000, 2.0, None)),
            Step::KeepAndRestore(locked)
        );
        assert!(c.is_busy(), "until the preview is back");
        assert_eq!(
            c.on_preview_frame(&FrameMeta::default(), now),
            Step::Nothing
        );
        assert!(!c.is_busy());
    }

    #[test]
    fn exposure_that_never_settles_is_kept_after_the_maximum() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, Some(&meta(10_000, 1.0, None)), now);
        let mut steps = Vec::new();
        for _ in 0..MAX_DISCARD + 1 {
            steps.push(c.on_still_frame(&meta(90_000, 8.0, None)));
        }
        assert!(steps[..MAX_DISCARD as usize]
            .iter()
            .all(|s| *s == Step::Discard));
        assert!(matches!(
            steps[MAX_DISCARD as usize],
            Step::KeepAndRestore(_)
        ));
    }

    #[test]
    fn waits_for_autofocus_but_not_forever() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        let scanning = meta(10_000, 1.0, Some(AfState::Scanning));
        assert_eq!(
            c.begin(Mode::Full, true, Some(&scanning), now),
            Step::Nothing
        );
        assert_eq!(c.on_preview_frame(&scanning, now), Step::Nothing);
        let focused = meta(12_000, 1.0, Some(AfState::Focused));
        match c.on_preview_frame(&focused, now) {
            Step::StartStill(ctl) => assert_eq!(ctl.exposure_us, Some(12_000)),
            other => panic!("{other:?}"),
        }

        let mut c = CaptureController::new();
        c.begin(Mode::Full, true, Some(&scanning), now);
        assert_eq!(c.tick(Some(&scanning), now), Step::Nothing);
        assert!(matches!(
            c.tick(Some(&scanning), now + FOCUS_WAIT),
            Step::StartStill(_)
        ));
    }

    #[test]
    fn a_lens_without_autofocus_does_not_wait() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        let scanning = meta(10_000, 1.0, Some(AfState::Scanning));
        assert!(matches!(
            c.begin(Mode::Full, false, Some(&scanning), now),
            Step::StartStill(_)
        ));
    }

    #[test]
    fn a_missing_still_frame_times_out() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, None, now);
        assert_eq!(c.tick(None, now + Duration::from_secs(1)), Step::Nothing);
        assert!(matches!(c.tick(None, now + STILL_TIMEOUT), Step::Fail(_)));
        assert!(c.is_busy(), "restoring the preview");
    }

    #[test]
    fn a_second_shutter_press_while_busy_is_ignored() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, None, now);
        assert_eq!(c.begin(Mode::Full, false, None, now), Step::Nothing);
        assert!(c.wants_still_stream());
    }

    #[test]
    fn no_metadata_means_no_lock_and_the_minimum_wait() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        assert_eq!(
            c.begin(Mode::Full, false, None, now),
            Step::StartStill(Controls::default())
        );
        let none = FrameMeta::default();
        assert_eq!(c.on_still_frame(&none), Step::Discard);
        assert_eq!(c.on_still_frame(&none), Step::Discard);
        assert!(matches!(c.on_still_frame(&none), Step::KeepAndRestore(_)));
    }
}
