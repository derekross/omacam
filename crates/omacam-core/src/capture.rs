//! The still-capture sequence (CAMERA-APP-PLAN §3.3).
//!
//! The software ISP gives one processed stream, so a full-resolution still
//! is a short reconfiguration into the camera's large mode:
//!
//! 1. Shutter: if autofocus is still scanning, wait for it (at most
//!    [`FOCUS_WAIT`]); then read the last exposure, gain and color gains.
//! 2. Reconfigure to the still mode, with those values as start hints.
//! 3. Drop frames until the auto-exposure says Converged (libcamera
//!    4.10's AeState) and the white balance is back near the preview's,
//!    or, on a libcamera without AeState, [`FIXED_DISCARD`] frames and
//!    [`SETTLE_MIN`]; at least [`MIN_DISCARD`] frames. If that doesn't
//!    come, keep a frame anyway once both [`MAX_DISCARD`] frames and
//!    [`SETTLE_MIN`] have passed, or at [`SETTLE_MAX`]. The wait is by
//!    time as well as frames because the full-resolution modes run at
//!    5-13 fps: ten frames could end before the AGC had converged, and the
//!    telephoto's stills came out dark (mean luma 27-32 at minimum gain).
//!    A freshly started mode also begins with the AWB's default gains; a
//!    still kept on its first frames came out yellow on the telephoto
//!    (blue gain 1.56 needed). Times count from the still stream's first
//!    frame.
//! 4. Reconfigure back to the preview mode with the same hints.
//!
//! Quick mode keeps the next preview frame instead, with no reconfiguring.
//!
//! The machine is pure: the controller feeds it frames and time, and acts
//! on the [`Step`]s it returns.

use std::time::{Duration, Instant};

use crate::backend::{AeState, AfState, Controls, FrameMeta};

/// Longest wait for autofocus to finish before taking the picture anyway.
pub const FOCUS_WAIT: Duration = Duration::from_millis(500);
/// Frames to drop at least after the still mode starts (the sensor applies
/// controls two frames late).
pub const MIN_DISCARD: u32 = 2;
/// Frames to see before keeping one that hasn't settled (with
/// [`SETTLE_MIN`]).
pub const MAX_DISCARD: u32 = 10;
/// The least time the still stream runs before a frame that hasn't
/// settled (or, without AeState, any frame) is kept.
pub const SETTLE_MIN: Duration = Duration::from_millis(1500);
/// The longest wait for the exposure to settle: past it the next frame is
/// kept whatever it says.
pub const SETTLE_MAX: Duration = Duration::from_secs(3);
/// On a libcamera that reports no AeState: the frames to drop before
/// keeping one, a fixed wait for exposure and white balance to settle.
pub const FIXED_DISCARD: u32 = 8;
/// Longest wait for the still frame before giving up.
pub const STILL_TIMEOUT: Duration = Duration::from_secs(6);
/// How close each color gain must be to the preview's.
pub const GAINS_WITHIN: f64 = 0.10;
/// Without the preview's gains to compare: how little they may change
/// from one frame to the next to count as settled.
pub const GAINS_STEADY: f64 = 0.03;

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
    WaitFocus {
        deadline: Instant,
    },
    Still {
        seen: u32,
        deadline: Instant,
        last_gains: Option<[f32; 2]>,
        /// When the still stream's first frame arrived.
        first: Option<Instant>,
    },
    Restoring,
}

/// How a still was chosen, for the log.
#[derive(Debug, Clone, PartialEq)]
pub struct StillReport {
    /// Still frames seen, the kept one included.
    pub frames: u32,
    /// From the still stream's first frame to the kept one.
    pub elapsed: Duration,
    /// The exposure settled (Converged, or the fixed wait), rather than
    /// the wait running out.
    pub settled: bool,
    pub meta: FrameMeta,
}

#[derive(Debug, Clone)]
pub struct CaptureController {
    state: State,
    locked: Controls,
    last_still: Option<StillReport>,
}

impl Default for CaptureController {
    fn default() -> Self {
        Self::new()
    }
}

/// The controls that hold the exposure and color of `meta`.
pub fn lock_from(meta: Option<&FrameMeta>) -> Controls {
    let Some(m) = meta else {
        return Controls::default();
    };
    Controls {
        exposure_us: m.exposure_us,
        analog_gain: m.analog_gain,
        color_gains: m.color_gains,
        lens_position: m.lens_position,
        ..Controls::default()
    }
}

impl CaptureController {
    pub fn new() -> CaptureController {
        CaptureController {
            state: State::Idle,
            locked: Controls::default(),
            last_still: None,
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

    /// How the last still was chosen.
    pub fn last_still(&self) -> Option<&StillReport> {
        self.last_still.as_ref()
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
            last_gains: None,
            first: None,
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

    /// A frame from the still stream, arriving at `now`.
    pub fn on_still_frame(&mut self, meta: &FrameMeta, now: Instant) -> Step {
        let State::Still {
            seen,
            last_gains,
            first,
            ..
        } = &mut self.state
        else {
            return Step::Discard;
        };
        *seen += 1;
        let seen = *seen;
        let elapsed = now.saturating_duration_since(*first.get_or_insert(now));
        let previous = std::mem::replace(last_gains, meta.color_gains);
        if seen <= MIN_DISCARD {
            return Step::Discard;
        }
        let close = |a: [f32; 2], b: [f32; 2], within: f64| {
            a.iter()
                .zip(b)
                .all(|(x, y)| y > 0.0 && ((x - y) / y).abs() as f64 <= within)
        };
        let color = match (meta.color_gains, self.locked.color_gains, previous) {
            (Some(got), Some(want), _) => close(got, want, GAINS_WITHIN),
            (Some(got), None, Some(prev)) => close(got, prev, GAINS_STEADY),
            (Some(_), None, None) => false,
            // The camera reports no gains: nothing to wait for.
            (None, _, _) => true,
        };
        let settled = match meta.ae_state {
            // libcamera 4.10 says when its AE has converged (also when it
            // sits at a limit).
            Some(state) => state == AeState::Converged && color,
            // Older: a fixed wait, by frames and by time.
            None => seen > FIXED_DISCARD && elapsed >= SETTLE_MIN,
        };
        let out_of_time = (seen > MAX_DISCARD && elapsed >= SETTLE_MIN) || elapsed >= SETTLE_MAX;
        if settled || out_of_time {
            self.last_still = Some(StillReport {
                frames: seen,
                elapsed,
                settled,
                meta: meta.clone(),
            });
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
            analog_gain: Some(gain),
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

    fn converged(exposure: i32, gains: Option<[f32; 2]>, state: AeState) -> FrameMeta {
        FrameMeta {
            color_gains: gains,
            ae_state: Some(state),
            ..meta(exposure, 1.0, None)
        }
    }

    #[test]
    fn full_mode_waits_for_ae_converged_then_keeps_and_restores() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        let last = meta(10_000, 2.0, Some(AfState::Focused));
        let locked = match c.begin(Mode::Full, true, Some(&last), now) {
            Step::StartStill(ctl) => ctl,
            other => panic!("{other:?}"),
        };
        assert_eq!(locked.exposure_us, Some(10_000));
        assert!(c.wants_still_stream());
        // Converged in the first two frames still waits the minimum.
        assert_eq!(
            c.on_still_frame(&converged(9_000, None, AeState::Converged), now),
            Step::Discard
        );
        assert_eq!(
            c.on_still_frame(&converged(9_000, None, AeState::Converged), now),
            Step::Discard
        );
        assert_eq!(
            c.on_still_frame(&converged(9_000, None, AeState::Searching), now),
            Step::Discard
        );
        assert_eq!(
            c.on_still_frame(&converged(9_000, None, AeState::Converged), now),
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
    fn without_ae_state_it_waits_a_fixed_number_of_frames() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, Some(&meta(10_000, 1.0, None)), now);
        // A full-resolution mode at 5 fps: the frames take 1.6 s.
        let steps: Vec<Step> = (0..FIXED_DISCARD + 1)
            .map(|i| c.on_still_frame(&meta(10_000, 1.0, None), at(now, i, 200)))
            .collect();
        assert!(steps[..FIXED_DISCARD as usize]
            .iter()
            .all(|s| *s == Step::Discard));
        assert!(matches!(
            steps[FIXED_DISCARD as usize],
            Step::KeepAndRestore(_)
        ));
    }

    #[test]
    fn the_still_waits_for_white_balance_to_return() {
        // The telephoto's preview had R 1.09 / B 1.56; the fresh still mode
        // starts at the AWB's defaults and walks back, while AE already
        // says Converged.
        let now = Instant::now();
        let mut c = CaptureController::new();
        let last = FrameMeta {
            color_gains: Some([1.09, 1.56]),
            ..meta(20_000, 1.0, None)
        };
        c.begin(Mode::Full, false, Some(&last), now);
        assert_eq!(c.locked().color_gains, Some([1.09, 1.56]));
        let frames = [
            [1.0, 1.0],
            [1.0, 1.0],
            [1.05, 1.2],
            [1.08, 1.38],
            [1.09, 1.55],
        ];
        let steps: Vec<Step> = frames
            .iter()
            .map(|g| c.on_still_frame(&converged(20_000, Some(*g), AeState::Converged), now))
            .collect();
        assert!(steps[..4].iter().all(|s| *s == Step::Discard), "{steps:?}");
        assert!(matches!(steps[4], Step::KeepAndRestore(_)));
    }

    /// The `i`th frame of a stream whose frames come every `ms`.
    fn at(start: Instant, i: u32, ms: u64) -> Instant {
        start + Duration::from_millis(ms * i as u64)
    }

    /// Feed still frames every `ms` until one is kept; returns its index.
    fn kept_at(c: &mut CaptureController, now: Instant, ms: u64, m: &FrameMeta) -> u32 {
        for i in 0..200 {
            match c.on_still_frame(m, at(now, i, ms)) {
                Step::Discard => {}
                Step::KeepAndRestore(_) => return i,
                other => panic!("{other:?}"),
            }
        }
        panic!("never kept");
    }

    #[test]
    fn exposure_that_never_converges_is_kept_after_frames_and_time() {
        let searching = converged(90_000, None, AeState::Searching);
        // At 30 fps ten frames pass in a third of a second: the wait goes
        // on to 1.5 s.
        let now = Instant::now();
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, Some(&meta(10_000, 1.0, None)), now);
        let i = kept_at(&mut c, now, 33, &searching);
        assert!(at(now, i, 33) - now >= SETTLE_MIN, "frame {i}");
        assert!(at(now, i - 1, 33) - now < SETTLE_MIN);
        let r = c.last_still().unwrap();
        assert!(!r.settled);
        assert_eq!(r.frames, i + 1);
        // At 5 fps the frames take longer: ten of them, 2 s.
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, None, now);
        assert_eq!(kept_at(&mut c, now, 200, &searching), MAX_DISCARD);
        // At 2 fps the wait stops at 3 s, before ten frames.
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, None, now);
        let i = kept_at(&mut c, now, 500, &searching);
        assert_eq!(at(now, i, 500) - now, SETTLE_MAX);
        assert!(i < MAX_DISCARD);
    }

    #[test]
    fn a_slow_mode_keeps_waiting_for_the_agc_past_ten_frames() {
        // The telephoto's full mode at about 7.7 fps: the AGC converges on
        // the 12th frame, after ten. Up to 0.3.0 the 11th, still dark, was
        // kept.
        let now = Instant::now();
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, None, now);
        for i in 0..11 {
            let m = converged(33_000, None, AeState::Searching);
            assert_eq!(
                c.on_still_frame(&m, at(now, i, 130)),
                Step::Discard,
                "frame {i}"
            );
        }
        let done = converged(33_000, None, AeState::Converged);
        assert!(matches!(
            c.on_still_frame(&done, at(now, 11, 130)),
            Step::KeepAndRestore(_)
        ));
        let r = c.last_still().unwrap();
        assert!(r.settled);
        assert_eq!(r.frames, 12);
        assert_eq!(r.elapsed, Duration::from_millis(1430));
        assert_eq!(r.meta.ae_state, Some(AeState::Converged));
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
    fn no_metadata_means_no_lock_and_the_fixed_wait() {
        let now = Instant::now();
        let mut c = CaptureController::new();
        assert_eq!(
            c.begin(Mode::Full, false, None, now),
            Step::StartStill(Controls::default())
        );
        let none = FrameMeta::default();
        // At 5 fps, the fixed wait's frames and time end together.
        for i in 0..FIXED_DISCARD {
            assert_eq!(c.on_still_frame(&none, at(now, i, 200)), Step::Discard);
        }
        assert!(matches!(
            c.on_still_frame(&none, at(now, FIXED_DISCARD, 200)),
            Step::KeepAndRestore(_)
        ));
        // At 30 fps the time decides: 1.5 s, not 9 frames.
        let mut c = CaptureController::new();
        c.begin(Mode::Full, false, None, now);
        let i = kept_at(&mut c, now, 33, &none);
        assert!(at(now, i, 33) - now >= SETTLE_MIN && i > FIXED_DISCARD);
    }
}
