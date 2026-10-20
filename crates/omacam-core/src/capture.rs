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
//! Burst mode (Night and HDR, merged by omacam-merge) settles the same way,
//! then locks one exposure for the whole burst: the settled one, the mode's
//! stops under it, with the shutter capped for a handheld phone and the
//! analog gain making up the rest. It drops frames until the camera reports
//! that exposure, keeps [`BurstSpec::frames`] of them, and goes back to the
//! preview at the settled exposure.
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
/// How close a burst frame's exposure time and gain must be to the burst's.
pub const BURST_WITHIN: f64 = 0.06;
/// Frames to wait for the camera to apply the burst's exposure.
pub const BURST_APPLY_MAX: u32 = 8;

/// A burst: how many frames, how far under the settled exposure, and the
/// longest shutter a frame may have.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurstSpec {
    pub frames: u32,
    /// Stops under the settled exposure (0 keeps it).
    pub under_stops: f32,
    /// The longest exposure time per frame (hand shake); gain makes up the
    /// rest.
    pub max_exposure_us: i32,
}

impl BurstSpec {
    /// Night: many frames at the settled brightness, each at most 1/15 s.
    pub const NIGHT: BurstSpec = BurstSpec {
        frames: 12,
        under_stops: 0.0,
        max_exposure_us: 66_000,
    };
    /// HDR: fewer frames, 1.5 stops under so the highlights don't clip.
    pub const HDR: BurstSpec = BurstSpec {
        frames: 8,
        under_stops: 1.5,
        max_exposure_us: 33_000,
    };

    /// The burst's exposure time and analog gain, from the settled ones.
    pub fn exposure(&self, settled_us: i32, settled_gain: f32) -> (i32, f32) {
        let mut t = settled_us as f64 * 2f64.powf(-self.under_stops as f64);
        let mut g = settled_gain.max(1.0) as f64;
        if t > self.max_exposure_us as f64 {
            g *= t / self.max_exposure_us as f64;
            t = self.max_exposure_us as f64;
        }
        (t.round().max(1.0) as i32, g as f32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// A preview frame, at preview resolution.
    Quick,
    /// A frame from the still mode, at full resolution.
    Full,
    /// Several frames from the still mode at one locked exposure.
    Burst(BurstSpec),
}

/// What the controller should do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Nothing,
    /// Switch the current lens to its still mode with these hints.
    StartStill(Controls),
    /// Keep the frame just given as the picture.
    Keep,
    /// Hold this exposure and color on the still stream (a burst's lock).
    Apply(Controls),
    /// Keep this frame as one of a burst; more follow.
    KeepBurst,
    /// Keep this frame and go back to the preview with these hints.
    KeepAndRestore(Controls),
    /// Drop this frame.
    Discard,
    /// Give up and go back to the preview.
    Fail(String),
    /// The preview is back after a burst: put the exposure settings back.
    /// The burst's lock made them manual, and the controls a stream starts
    /// with don't reach the camera's exposure loop, so the preview would
    /// keep the burst's exposure.
    Unlock,
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
    /// Waiting for the camera to apply the burst's exposure.
    BurstApply {
        spec: BurstSpec,
        target: (i32, f32),
        waited: u32,
        deadline: Instant,
        first: Instant,
        seen: u32,
    },
    BurstKeep {
        spec: BurstSpec,
        target: (i32, f32),
        kept: u32,
        deadline: Instant,
        first: Instant,
        seen: u32,
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
    /// The burst this capture takes, if it is one.
    burst: Option<BurstSpec>,
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
            burst: None,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.state != State::Idle
    }

    /// True while frames belong to the still stream (the preview should
    /// stay frozen).
    pub fn wants_still_stream(&self) -> bool {
        matches!(
            self.state,
            State::Still { .. } | State::BurstApply { .. } | State::BurstKeep { .. }
        )
    }

    pub fn locked(&self) -> &Controls {
        &self.locked
    }

    /// How the last still was chosen.
    pub fn last_still(&self) -> Option<&StillReport> {
        self.last_still.as_ref()
    }

    /// The burst the current (or last) capture takes, if it is one.
    pub fn burst(&self) -> Option<BurstSpec> {
        self.burst
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
        self.burst = match mode {
            Mode::Burst(spec) => Some(spec),
            _ => None,
        };
        match mode {
            Mode::Quick => {
                self.state = State::QuickWait;
                Step::Nothing
            }
            Mode::Full | Mode::Burst(_) => {
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
                if self.burst.is_some() {
                    Step::Unlock
                } else {
                    Step::Nothing
                }
            }
            _ => Step::Nothing,
        }
    }

    /// A frame from the still stream, arriving at `now`.
    pub fn on_still_frame(&mut self, meta: &FrameMeta, now: Instant) -> Step {
        if matches!(
            self.state,
            State::BurstApply { .. } | State::BurstKeep { .. }
        ) {
            return self.on_burst_frame(meta, now);
        }
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
        if !(settled || out_of_time) {
            return Step::Discard;
        }
        self.last_still = Some(StillReport {
            frames: seen,
            elapsed,
            settled,
            meta: meta.clone(),
        });
        if let Some(spec) = self.burst {
            // Lock the burst's exposure on the settled frame's values; the
            // preview comes back at the settled ones (`locked`).
            let first = now - elapsed;
            let settled_controls = lock_from(Some(meta));
            let (t, g) = spec.exposure(
                meta.exposure_us
                    .or(self.locked.exposure_us)
                    .unwrap_or(10_000),
                meta.analog_gain.or(self.locked.analog_gain).unwrap_or(1.0),
            );
            self.locked = Controls {
                lens_position: settled_controls.lens_position.or(self.locked.lens_position),
                ..settled_controls
            };
            self.state = State::BurstApply {
                spec,
                target: (t, g),
                waited: 0,
                deadline: now + STILL_TIMEOUT,
                first,
                seen,
            };
            return Step::Apply(Controls {
                exposure_us: Some(t),
                analog_gain: Some(g),
                color_gains: meta.color_gains.or(self.locked.color_gains),
                lens_position: self.locked.lens_position,
                ..Controls::default()
            });
        }
        self.state = State::Restoring;
        Step::KeepAndRestore(self.locked.clone())
    }

    /// A still-stream frame while a burst locks or keeps its frames.
    fn on_burst_frame(&mut self, meta: &FrameMeta, now: Instant) -> Step {
        let near = |got: Option<f64>, want: f64| {
            got.is_some_and(|g| want > 0.0 && ((g - want) / want).abs() <= BURST_WITHIN)
        };
        let applied = |target: (i32, f32)| {
            near(meta.exposure_us.map(f64::from), target.0 as f64)
                && near(meta.analog_gain.map(f64::from), target.1 as f64)
        };
        match &mut self.state {
            State::BurstApply {
                spec,
                target,
                waited,
                deadline,
                first,
                seen,
            } => {
                *seen += 1;
                if !applied(*target) {
                    *waited += 1;
                    if *waited > BURST_APPLY_MAX {
                        self.state = State::Restoring;
                        return Step::Fail("the camera didn't take the burst's exposure".into());
                    }
                    return Step::Discard;
                }
                let next = State::BurstKeep {
                    spec: *spec,
                    target: *target,
                    kept: 0,
                    deadline: *deadline,
                    first: *first,
                    seen: *seen,
                };
                self.state = next;
                self.keep_burst_frame(meta, now)
            }
            State::BurstKeep { target, seen, .. } => {
                *seen += 1;
                // A frame off the burst's exposure (rare: a control
                // re-applied late) would merge badly: leave it out.
                if !applied(*target) {
                    return Step::Discard;
                }
                self.keep_burst_frame(meta, now)
            }
            _ => Step::Discard,
        }
    }

    fn keep_burst_frame(&mut self, meta: &FrameMeta, now: Instant) -> Step {
        let State::BurstKeep {
            spec,
            kept,
            first,
            seen,
            ..
        } = &mut self.state
        else {
            return Step::Discard;
        };
        *kept += 1;
        if *kept < spec.frames {
            return Step::KeepBurst;
        }
        self.last_still = Some(StillReport {
            frames: *seen,
            elapsed: now.saturating_duration_since(*first),
            settled: true,
            meta: meta.clone(),
        });
        self.state = State::Restoring;
        Step::KeepAndRestore(self.locked.clone())
    }

    /// Time passes: deadlines.
    pub fn tick(&mut self, last: Option<&FrameMeta>, now: Instant) -> Step {
        match self.state {
            State::WaitFocus { deadline } if now >= deadline => self.go_still(last, now),
            State::Still { deadline, .. } if now >= deadline => {
                self.state = State::Restoring;
                Step::Fail("the still frame didn't arrive".into())
            }
            State::BurstApply { deadline, .. } | State::BurstKeep { deadline, .. }
                if now >= deadline =>
            {
                self.state = State::Restoring;
                Step::Fail("the burst didn't finish".into())
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
    fn settled(exposure: i32, gain: f32) -> FrameMeta {
        FrameMeta {
            exposure_us: Some(exposure),
            analog_gain: Some(gain),
            color_gains: Some([1.8, 1.5]),
            ae_state: Some(AeState::Converged),
            ..FrameMeta::default()
        }
    }

    #[test]
    fn a_burst_caps_the_shutter_and_makes_it_up_with_gain() {
        // Night in a dim room: the AGC wants 1/5 s at gain 2.
        let (t, g) = BurstSpec::NIGHT.exposure(200_000, 2.0);
        assert_eq!(t, 66_000);
        assert!((g - 2.0 * 200.0 / 66.0).abs() < 1e-3, "{g}");
        // HDR outdoors: 1.5 stops under 1/50 s, gain unchanged.
        let (t, g) = BurstSpec::HDR.exposure(20_000, 1.0);
        assert_eq!(t, 7071);
        assert_eq!(g, 1.0);
    }

    #[test]
    fn a_burst_locks_its_exposure_then_keeps_its_frames() {
        let spec = BurstSpec {
            frames: 3,
            under_stops: 1.0,
            max_exposure_us: 66_000,
        };
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut c = CaptureController::new();
        let preview = settled(20_000, 2.0);
        assert!(matches!(
            c.begin(Mode::Burst(spec), false, Some(&preview), t0),
            Step::StartStill(_)
        ));
        // The still mode settles at 20 ms, gain 2 (after the first drops).
        for i in 0..MIN_DISCARD as u64 {
            assert_eq!(
                c.on_still_frame(&settled(20_000, 2.0), ms(100 * i)),
                Step::Discard
            );
        }
        let lock = match c.on_still_frame(&settled(20_000, 2.0), ms(300)) {
            Step::Apply(controls) => controls,
            other => panic!("{other:?}"),
        };
        assert_eq!(lock.exposure_us, Some(10_000));
        assert_eq!(lock.analog_gain, Some(2.0));
        assert_eq!(lock.color_gains, Some([1.8, 1.5]));
        assert!(c.wants_still_stream());
        // A frame still at the old exposure is dropped.
        assert_eq!(
            c.on_still_frame(&settled(20_000, 2.0), ms(400)),
            Step::Discard
        );
        // Three at the burst's exposure: two kept, the third ends the burst
        // and the preview comes back at the settled exposure.
        assert_eq!(
            c.on_still_frame(&settled(10_000, 2.0), ms(500)),
            Step::KeepBurst
        );
        assert_eq!(
            c.on_still_frame(&settled(10_050, 2.0), ms(600)),
            Step::KeepBurst
        );
        match c.on_still_frame(&settled(10_000, 2.0), ms(700)) {
            Step::KeepAndRestore(restore) => {
                assert_eq!(restore.exposure_us, Some(20_000));
                assert_eq!(restore.analog_gain, Some(2.0));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(c.burst(), Some(spec));
        assert!(!c.wants_still_stream());
        // The preview's first frame puts the exposure settings back.
        assert_eq!(
            c.on_preview_frame(&settled(20_000, 2.0), ms(800)),
            Step::Unlock
        );
        assert!(!c.is_busy());
    }

    #[test]
    fn a_burst_gives_up_when_the_camera_ignores_its_exposure() {
        let t0 = Instant::now();
        let mut c = CaptureController::new();
        let preview = settled(20_000, 2.0);
        c.begin(Mode::Burst(BurstSpec::HDR), false, Some(&preview), t0);
        let mut step = Step::Nothing;
        for i in 0..(MIN_DISCARD + 1 + BURST_APPLY_MAX + 1) {
            step = c.on_still_frame(
                &settled(20_000, 2.0),
                t0 + Duration::from_millis(100 * i as u64),
            );
            if matches!(step, Step::Fail(_)) {
                break;
            }
        }
        assert!(matches!(step, Step::Fail(_)), "{step:?}");
    }

    #[test]
    fn full_mode_still_keeps_one_frame() {
        let t0 = Instant::now();
        let mut c = CaptureController::new();
        let preview = settled(20_000, 2.0);
        c.begin(Mode::Full, false, Some(&preview), t0);
        let mut step = Step::Nothing;
        for i in 0..=MIN_DISCARD {
            step = c.on_still_frame(
                &settled(20_000, 2.0),
                t0 + Duration::from_millis(100 * i as u64),
            );
        }
        assert!(matches!(step, Step::KeepAndRestore(_)), "{step:?}");
        assert_eq!(c.burst(), None);
    }
}
