//! Electronic video stabilization from the gyroscope.
//!
//! The recording crops a margin off the view ([`MARGIN`]) and moves that
//! crop with the shake: the gyroscope's rates, integrated, say how far
//! the phone has turned since the last frame, and a turn of θ moves the
//! scene by about f·θ on the sensor (f the focal length in pixels). The
//! camera's intended path is the turn smoothed over [`Config::smoothing`];
//! what's left over is shake, and the crop follows it so the scene holds
//! still. When the shake is bigger than the margin, the crop stops at the
//! edge and the smoothed path is pulled along, so a deliberate pan goes
//! through with a short lag instead of sticking.
//!
//! Frames are matched to the gyroscope by time, both on CLOCK_MONOTONIC
//! on the phone ([`Stabilizer::set_clock_offset`] bridges two clocks).
//! gs201's camera receiver stamps libcamera's SensorTimestamp at the end
//! of the frame's DMA, so the middle of the exposure is about that, less
//! half the readout ([`Config::readout_us`]) and half the exposure. The
//! gyroscope's small bias (about -0.004 rad/s at rest on the phone) adds
//! up to a slow turn, which the smoothed path follows like a pan, so it
//! leaves no lasting offset. Roll (turning about the lens's axis) would
//! need a rotation, not a shift, and isn't corrected.
//!
//! Axes are Android's, which the phone's sensor proxy reports: x to the
//! right and y up the phone held in portrait, z out of the screen. A back
//! camera looks along -z, so a turn about +y moves the scene right in
//! the upright picture and a turn about +x moves it down.

use std::collections::VecDeque;

/// The share of the view kept as margin for the shake: the recording
/// shows 1 / (1 + MARGIN) of what the view would.
pub const MARGIN: f64 = 0.10;

/// One gyroscope record: when, and the turn rates about x, y and z in
/// radians per second.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GyroSample {
    pub t_ns: i64,
    pub rate: [f32; 3],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// Time constant of the intended camera path, seconds.
    pub smoothing: f64,
    /// How long gyro records are kept, nanoseconds.
    pub keep_ns: i64,
    /// How long the sensor takes to read a frame out, microseconds (frame
    /// lines times line time): an estimate for the 30 fps modes until
    /// measured per mode.
    pub readout_us: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            smoothing: 0.5,
            keep_ns: 2_000_000_000,
            readout_us: 20_000,
        }
    }
}

/// The focal length over the upright picture's width and height: how far
/// (as a share of each side) a turn of one radian moves the scene.
pub fn focal_over_sides(focal_length_35mm: f64, picture_aspect: f64) -> (f64, f64) {
    // 35 mm equivalents are by the diagonal: 43.27 mm.
    let a = picture_aspect.max(1e-6);
    let diagonal_over_width = (1.0 + 1.0 / (a * a)).sqrt();
    let fw = focal_length_35mm / 43.27 * diagonal_over_width;
    (fw, fw * a)
}

#[derive(Debug, Clone)]
pub struct Stabilizer {
    config: Config,
    /// Records with the integrated turn about x and y up to each.
    turns: VecDeque<(i64, [f64; 2])>,
    last_rate: Option<GyroSample>,
    /// Intended path (smoothed turn) and when it was last updated.
    smooth: Option<([f64; 2], i64)>,
    clock_offset_ns: i64,
}

impl Default for Stabilizer {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl Stabilizer {
    pub fn new(config: Config) -> Stabilizer {
        Stabilizer {
            config,
            turns: VecDeque::new(),
            last_rate: None,
            smooth: None,
            clock_offset_ns: 0,
        }
    }

    /// Nanoseconds to add to a frame's timestamp to put it on the
    /// gyroscope's clock.
    pub fn set_clock_offset(&mut self, ns: i64) {
        self.clock_offset_ns = ns;
    }

    /// A new gyroscope record (in time order; older ones are ignored).
    pub fn push(&mut self, s: GyroSample) {
        let turn = match (self.last_rate, self.turns.back()) {
            (Some(prev), Some(&(t, turn))) => {
                if s.t_ns <= t {
                    return;
                }
                let dt = (s.t_ns - t) as f64 * 1e-9;
                // Trapezoids: the mean of the two rates over the gap.
                let r = |i: usize| (prev.rate[i] as f64 + s.rate[i] as f64) / 2.0;
                [turn[0] + r(0) * dt, turn[1] + r(1) * dt]
            }
            _ => [0.0, 0.0],
        };
        self.turns.push_back((s.t_ns, turn));
        self.last_rate = Some(s);
        while let (Some(&(first, _)), Some(&(last, _))) = (self.turns.front(), self.turns.back()) {
            if last - first <= self.config.keep_ns || self.turns.len() <= 2 {
                break;
            }
            self.turns.pop_front();
        }
    }

    /// The turn about x and y at time `t` (gyro clock), interpolated;
    /// None outside the records.
    fn turn_at(&self, t: i64) -> Option<[f64; 2]> {
        let i = self.turns.iter().position(|&(ts, _)| ts >= t)?;
        let (tb, b) = self.turns[i];
        if i == 0 {
            return (tb == t).then_some(b);
        }
        let (ta, a) = self.turns[i - 1];
        let f = (t - ta) as f64 / (tb - ta).max(1) as f64;
        Some([a[0] + f * (b[0] - a[0]), a[1] + f * (b[1] - a[1])])
    }

    /// Where to move the view for a frame stamped `frame_ns` (the end of
    /// its readout, on the frame's clock) whose exposure lasted
    /// `exposure_us`, as a
    /// share of the upright picture (x right, y down), for a picture whose
    /// focal length over its sides is `focal` and a view `scale` times
    /// smaller than the picture (the zoom). (0, 0) when the records don't
    /// cover the frame (no gyroscope, or it lags).
    pub fn correction(
        &mut self,
        frame_ns: i64,
        exposure_us: Option<i32>,
        focal: (f64, f64),
        scale: f64,
    ) -> (f64, f64) {
        let mid = frame_ns + self.clock_offset_ns
            - self.config.readout_us * 500
            - exposure_us.map(|e| e as i64 * 500).unwrap_or(0);
        let Some(turn) = self.turn_at(mid) else {
            return (0.0, 0.0);
        };
        let (mut path, last) = self.smooth.unwrap_or((turn, mid));
        let dt = ((mid - last) as f64 * 1e-9).max(0.0);
        let alpha = 1.0 - (-dt / self.config.smoothing).exp();
        for i in 0..2 {
            path[i] += alpha * (turn[i] - path[i]);
        }
        // The shake, in picture shares; the crop can move as far as the
        // margin leaves room for.
        let room = (1.0 - 1.0 / (1.0 + MARGIN)) / 2.0 / scale.max(1.0);
        // A turn about y moves the scene along x, about x along y.
        let mut shift = [(turn[1] - path[1]) * focal.0, (turn[0] - path[0]) * focal.1];
        for (i, f) in [(0, focal.0), (1, focal.1)] {
            if shift[i].abs() > room {
                shift[i] = room.copysign(shift[i]);
                // Pull the path along so a pan isn't held back.
                let axis = 1 - i;
                path[axis] = turn[axis] - shift[i] / f;
            }
        }
        self.smooth = Some((path, mid));
        (shift[0], shift[1])
    }

    /// Start a new path (a new recording or another lens).
    pub fn reset_path(&mut self) {
        self.smooth = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    /// Gyro records every 5 ms (200 Hz) with rates from `rate(t)`.
    fn feed(s: &mut Stabilizer, from_ms: i64, to_ms: i64, rate: impl Fn(f64) -> [f32; 3]) {
        let mut t = from_ms * MS;
        while t <= to_ms * MS {
            s.push(GyroSample {
                t_ns: t,
                rate: rate(t as f64 * 1e-9),
            });
            t += 5 * MS;
        }
    }

    #[test]
    fn the_main_cameras_focal_length_in_picture_shares() {
        // 25 mm equivalent, upright 3:4: about 0.96 of the width.
        let (fw, fh) = focal_over_sides(25.0, 0.75);
        assert!((fw - 25.0 / 43.27 * 5.0 / 3.0).abs() < 1e-9);
        assert!((fh - fw * 0.75).abs() < 1e-9);
    }

    #[test]
    fn a_still_phone_needs_no_correction() {
        let mut s = Stabilizer::default();
        feed(&mut s, 0, 1000, |_| [0.0; 3]);
        for f in 0..30 {
            let c = s.correction(f * 33 * MS, Some(10_000), (1.0, 0.75), 1.0);
            assert_eq!(c, (0.0, 0.0));
        }
    }

    #[test]
    fn shake_is_followed_and_a_slow_pan_is_not() {
        // A 6 Hz shake about y, 0.2° each way: the correction follows it.
        let mut s = Stabilizer::default();
        let amp = 0.2f64.to_radians();
        let w = 2.0 * std::f64::consts::PI * 6.0;
        feed(&mut s, 0, 2000, |t| {
            [0.0, (amp * w * (w * t).cos()) as f32, 0.0]
        });
        let mut worst = 0.0f64;
        for f in 0..60 {
            let t = f * 33 * MS;
            let c = s.correction(t, None, (1.0, 0.75), 1.0);
            // The scene moved by f * turn (the turn's own mean is about 0),
            // once the path has settled from its start.
            let turn = amp * (w * t as f64 * 1e-9).sin();
            if f >= 30 {
                worst = worst.max((c.0 - turn).abs());
            }
            assert_eq!(c.1, 0.0, "nothing about x");
        }
        assert!(worst < 0.4 * amp, "worst error {worst} of {amp}");
        // A steady pan at 10°/s: after it settles the correction is a
        // small constant lag, not the whole turn.
        let mut s = Stabilizer::default();
        let rate = 10f64.to_radians();
        feed(&mut s, 0, 4000, |_| [0.0, rate as f32, 0.0]);
        let mut last = (0.0, 0.0);
        for f in 0..120 {
            last = s.correction(f * 33 * MS, None, (1.0, 0.75), 1.0);
        }
        let lag = rate * 0.5; // the path's time constant behind
        assert!(last.0 > 0.0 && last.0 <= lag + 1e-6, "{last:?}");
    }

    #[test]
    fn corrections_stay_inside_the_margin() {
        let mut s = Stabilizer::default();
        // A sudden 5° turn about x.
        feed(&mut s, 0, 1000, |t| {
            if (0.5..0.55).contains(&t) {
                [100f32.to_radians(), 0.0, 0.0]
            } else {
                [0.0; 3]
            }
        });
        let room = (1.0 - 1.0 / 1.1) / 2.0;
        for f in 0..30 {
            let c = s.correction(f * 33 * MS, None, (1.0, 0.75), 1.0);
            assert!(c.1.abs() <= room + 1e-12, "{c:?}");
        }
        // Zoomed in 2x, half the room.
        let mut s2 = Stabilizer::default();
        feed(&mut s2, 0, 1000, |_| [1.0, 0.0, 0.0]);
        for f in 0..30 {
            let c = s2.correction(f * 33 * MS, None, (1.0, 0.75), 2.0);
            assert!(c.1.abs() <= room / 2.0 + 1e-12);
        }
    }

    #[test]
    fn frames_outside_the_records_get_nothing_and_clocks_line_up() {
        let mut s = Stabilizer::default();
        feed(&mut s, 1000, 2000, |_| [0.0, 0.5, 0.0]);
        assert_eq!(s.correction(100 * MS, None, (1.0, 1.0), 1.0), (0.0, 0.0));
        assert_eq!(s.correction(3000 * MS, None, (1.0, 1.0), 1.0), (0.0, 0.0));
        // Frames on a clock 900 ms behind the gyro's.
        s.set_clock_offset(900 * MS);
        s.correction(200 * MS, None, (1.0, 1.0), 1.0);
        let c = s.correction(300 * MS, None, (1.0, 1.0), 1.0);
        assert!(c.0 > 0.0, "{c:?}");
    }

    #[test]
    fn frames_are_matched_at_the_middle_of_their_exposure() {
        // Still until 1 s, then turning about y. A frame stamped at 1.015 s
        // (end of readout) with a 10 ms exposure was exposed around 1.0 s:
        // no turn yet. Its successor, 33 ms later, sees the turn.
        let mut s = Stabilizer::default();
        feed(&mut s, 0, 2000, |t| {
            if t < 1.0 {
                [0.0; 3]
            } else {
                [0.0, 1.0, 0.0]
            }
        });
        s.correction(982 * MS, Some(10_000), (1.0, 1.0), 1.0);
        let c = s.correction(1015 * MS, Some(10_000), (1.0, 1.0), 1.0);
        assert!(c.0.abs() < 3e-3, "{c:?}");
        let c = s.correction(1048 * MS, Some(10_000), (1.0, 1.0), 1.0);
        assert!(c.0 > 0.02, "{c:?}");
    }

    #[test]
    fn old_records_are_dropped() {
        let mut s = Stabilizer::default();
        feed(&mut s, 0, 10_000, |_| [0.0; 3]);
        assert!(s.turns.len() <= 2000 / 5 + 2);
        // Out of order records are ignored.
        let n = s.turns.len();
        s.push(GyroSample {
            t_ns: 0,
            rate: [1.0; 3],
        });
        assert_eq!(s.turns.len(), n);
    }
}
