//! Zoom: which lens shows a zoom value, and how much of it is cropped.
//!
//! Zoom is measured against the main camera's field of view. A lens can
//! show any zoom at or above its own factor by cropping its frame; it can't
//! show less. So switching down to a wider lens happens as soon as the zoom
//! drops below the current lens's factor, and the hysteresis that keeps a
//! pinch from flapping between two cameras sits above the threshold:
//! switching up waits until the zoom is [`HYSTERESIS`] past the next lens's
//! factor. (CAMERA-APP-PLAN §3.4 puts the band below the threshold, which a
//! lens can't show; this is the same 6 % on the side it can.)
//!
//! A pinch or a slider drag switches only after the zoom has stayed still
//! for [`SETTLE`]; a preset button switches at once and without hysteresis.

use std::time::{Duration, Instant};

/// How far past the next lens's factor the zoom must go before a pinch or
/// a drag switches up to it.
pub const HYSTERESIS: f64 = 0.06;
/// How long a pinch or a drag must rest before the lens switches.
pub const SETTLE: Duration = Duration::from_millis(150);

/// The lenses of one side as zoom steps, from widest to longest.
#[derive(Debug, Clone, PartialEq)]
pub struct Ladder {
    /// Each lens's zoom factor, ascending.
    factors: Vec<f64>,
    max: f64,
}

impl Ladder {
    /// `factors` must be ascending and non-empty.
    pub fn new(factors: Vec<f64>, max: f64) -> Ladder {
        assert!(!factors.is_empty(), "a ladder needs a lens");
        assert!(
            factors.windows(2).all(|w| w[0] <= w[1]),
            "ladder factors must ascend"
        );
        let max = max.max(factors[factors.len() - 1]);
        Ladder { factors, max }
    }

    pub fn len(&self) -> usize {
        self.factors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.factors.is_empty()
    }

    pub fn factor(&self, step: usize) -> f64 {
        self.factors[step]
    }

    pub fn min(&self) -> f64 {
        self.factors[0]
    }

    pub fn max(&self) -> f64 {
        self.max
    }

    pub fn clamp(&self, zoom: f64) -> f64 {
        if zoom.is_nan() {
            return 1.0_f64.clamp(self.min(), self.max);
        }
        zoom.clamp(self.min(), self.max)
    }

    /// The longest lens that can show `zoom`: the plain thresholds.
    pub fn natural(&self, zoom: f64) -> usize {
        self.factors.iter().rposition(|&f| zoom >= f).unwrap_or(0)
    }

    /// The lens to show `zoom` when `current` is showing now, with the
    /// hysteresis band above each switch-up threshold.
    pub fn sticky(&self, zoom: f64, current: usize) -> usize {
        let natural = self.natural(zoom);
        if natural <= current {
            // Down (or staying): the current lens can't show less than its
            // own factor.
            return natural;
        }
        // Up: only to lenses whose band the zoom has cleared.
        (current + 1..self.len())
            .rev()
            .find(|&s| zoom >= self.factors[s] * (1.0 + HYSTERESIS))
            .unwrap_or(current)
    }

    /// How much of `step`'s frame to crop away for `zoom`: the scale to
    /// apply to it. Below 1 when a frame is shown wider than its lens can
    /// (a frozen frame while the wider lens starts).
    pub fn scale(&self, zoom: f64, step: usize) -> f64 {
        zoom / self.factors[step]
    }

    /// Position on a logarithmic slider, 0 at the widest lens, 1 at max.
    pub fn to_slider(&self, zoom: f64) -> f64 {
        let (lo, hi) = (self.min(), self.max);
        if hi <= lo {
            return 0.0;
        }
        ((self.clamp(zoom) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
    }

    pub fn from_slider(&self, pos: f64) -> f64 {
        let (lo, hi) = (self.min(), self.max);
        lo * (hi / lo).powf(pos.clamp(0.0, 1.0))
    }
}

/// Where a zoom change came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomSource {
    /// A preset button: switch now, no hysteresis.
    Preset,
    /// A pinch or a slider drag in progress: switch after it settles.
    Gesture,
}

/// The zoom value and the lens that should be streaming for it.
#[derive(Debug, Clone)]
pub struct ZoomPlanner {
    ladder: Ladder,
    zoom: f64,
    /// The ladder step chosen for the current zoom.
    target: usize,
    /// When a gesture last moved the zoom, while its lens choice waits.
    pending_since: Option<Instant>,
}

impl ZoomPlanner {
    pub fn new(ladder: Ladder, zoom: f64) -> ZoomPlanner {
        let zoom = ladder.clamp(zoom);
        let target = ladder.natural(zoom);
        ZoomPlanner {
            ladder,
            zoom,
            target,
            pending_since: None,
        }
    }

    pub fn ladder(&self) -> &Ladder {
        &self.ladder
    }

    pub fn zoom(&self) -> f64 {
        self.zoom
    }

    /// The step that should stream now.
    pub fn target(&self) -> usize {
        self.target
    }

    pub fn is_settling(&self) -> bool {
        self.pending_since.is_some()
    }

    /// Set the zoom. Returns the new target step when it changes now.
    pub fn set(&mut self, zoom: f64, source: ZoomSource, now: Instant) -> Option<usize> {
        self.zoom = self.ladder.clamp(zoom);
        match source {
            ZoomSource::Preset => {
                self.pending_since = None;
                self.retarget(self.ladder.natural(self.zoom))
            }
            ZoomSource::Gesture => {
                self.pending_since = Some(now);
                None
            }
        }
    }

    /// The gesture ended: decide at the next tick without waiting longer.
    pub fn release(&mut self, now: Instant) -> Option<usize> {
        if self.pending_since.is_some() {
            self.pending_since = Some(now - SETTLE);
        }
        self.tick(now)
    }

    /// Call often (every frame or every few tens of ms). Returns the new
    /// target step once a gesture has settled on another lens.
    pub fn tick(&mut self, now: Instant) -> Option<usize> {
        let since = self.pending_since?;
        if now.duration_since(since) < SETTLE {
            return None;
        }
        self.pending_since = None;
        self.retarget(self.ladder.sticky(self.zoom, self.target))
    }

    /// Time until a settling gesture needs a [`ZoomPlanner::tick`].
    pub fn next_tick(&self, now: Instant) -> Option<Duration> {
        self.pending_since
            .map(|since| SETTLE.saturating_sub(now.duration_since(since)))
    }

    fn retarget(&mut self, step: usize) -> Option<usize> {
        if step == self.target {
            None
        } else {
            self.target = step;
            Some(step)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Pixel 7 Pro's rear ladder: 0.45x ultrawide, 1x main, 4.78x tele.
    fn cheetah() -> Ladder {
        Ladder::new(vec![0.45, 1.0, 4.78], 15.0)
    }

    const UW: usize = 0;
    const MAIN: usize = 1;
    const TELE: usize = 2;

    #[test]
    fn natural_thresholds_follow_the_lens_factors() {
        let l = cheetah();
        assert_eq!(l.natural(0.45), UW);
        assert_eq!(l.natural(0.5), UW);
        assert_eq!(l.natural(0.99), UW);
        assert_eq!(l.natural(1.0), MAIN);
        assert_eq!(l.natural(2.0), MAIN);
        assert_eq!(l.natural(4.77), MAIN);
        assert_eq!(l.natural(4.78), TELE);
        assert_eq!(l.natural(5.0), TELE);
        assert_eq!(l.natural(15.0), TELE);
    }

    #[test]
    fn presets_land_on_the_plan_lenses_and_crops() {
        let l = cheetah();
        // 0.5x is a 10 % crop of the ultrawide
        assert!((l.scale(0.5, l.natural(0.5)) - 1.111).abs() < 0.001);
        assert_eq!(l.scale(1.0, l.natural(1.0)), 1.0);
        // 2x is a center crop of the main
        assert_eq!(l.scale(2.0, l.natural(2.0)), 2.0);
        // 5x is the tele cropped by 5/4.78
        assert!((l.scale(5.0, l.natural(5.0)) - 1.046).abs() < 0.001);
        // 15x is a 3.1x crop of the tele
        assert!((l.scale(15.0, TELE) - 3.138).abs() < 0.001);
    }

    #[test]
    fn switching_up_waits_for_the_hysteresis_band() {
        let l = cheetah();
        // From the ultrawide, 1.0x..1.06x stays on it.
        assert_eq!(l.sticky(1.0, UW), UW);
        assert_eq!(l.sticky(1.05, UW), UW);
        assert_eq!(l.sticky(1.061, UW), MAIN);
        // From the main, up to 4.78 * 1.06 = 5.067 stays on it.
        assert_eq!(l.sticky(5.0, MAIN), MAIN);
        assert_eq!(l.sticky(5.07, MAIN), TELE);
        // A jump past two thresholds goes straight to the longest lens.
        assert_eq!(l.sticky(8.0, UW), TELE);
    }

    #[test]
    fn switching_down_happens_at_the_lens_factor() {
        let l = cheetah();
        assert_eq!(l.sticky(4.78, TELE), TELE);
        assert_eq!(l.sticky(4.77, TELE), MAIN);
        assert_eq!(l.sticky(1.0, MAIN), MAIN);
        assert_eq!(l.sticky(0.99, MAIN), UW);
        assert_eq!(l.sticky(0.6, TELE), UW);
    }

    #[test]
    fn a_pinch_back_and_forth_across_1x_does_not_flap() {
        let l = cheetah();
        let mut step = MAIN;
        let mut switches = 0;
        // Wobble around 1.0 .. 1.05 while on the main: never switches up or
        // down except when it drops below 1.0, once.
        for z in [1.02, 1.04, 1.01, 1.03, 1.05, 1.02] {
            let next = l.sticky(z, step);
            if next != step {
                switches += 1;
                step = next;
            }
        }
        assert_eq!(switches, 0);
        for z in [0.98, 1.02, 1.04, 1.01, 1.05] {
            let next = l.sticky(z, step);
            if next != step {
                switches += 1;
                step = next;
            }
        }
        assert_eq!(switches, 1, "down at 0.98, then held in the band");
        assert_eq!(step, UW);
    }

    #[test]
    fn slider_is_logarithmic_and_round_trips() {
        let l = cheetah();
        assert_eq!(l.to_slider(0.45), 0.0);
        assert!((l.to_slider(15.0) - 1.0).abs() < 1e-12);
        let mid = l.from_slider(0.5);
        assert!((mid - (0.45f64 * 15.0).sqrt()).abs() < 1e-9);
        for z in [0.45, 0.7, 1.0, 2.0, 4.78, 9.0, 15.0] {
            assert!((l.from_slider(l.to_slider(z)) - z).abs() < 1e-9);
        }
        assert_eq!(l.clamp(100.0), 15.0);
        assert_eq!(l.clamp(0.1), 0.45);
        assert_eq!(l.clamp(f64::NAN), 1.0);
    }

    #[test]
    fn gesture_switches_only_after_settling() {
        let t0 = Instant::now();
        let mut p = ZoomPlanner::new(cheetah(), 1.0);
        assert_eq!(p.target(), MAIN);
        assert_eq!(p.set(6.0, ZoomSource::Gesture, t0), None);
        assert!(p.is_settling());
        assert_eq!(p.tick(t0 + Duration::from_millis(100)), None);
        // Still moving: the settle timer restarts.
        p.set(6.5, ZoomSource::Gesture, t0 + Duration::from_millis(120));
        assert_eq!(p.tick(t0 + Duration::from_millis(200)), None);
        assert_eq!(p.tick(t0 + Duration::from_millis(271)), Some(TELE));
        assert!(!p.is_settling());
        assert_eq!(p.tick(t0 + Duration::from_millis(400)), None);
    }

    #[test]
    fn releasing_a_gesture_decides_at_once() {
        let t0 = Instant::now();
        let mut p = ZoomPlanner::new(cheetah(), 1.0);
        p.set(0.6, ZoomSource::Gesture, t0);
        assert_eq!(p.release(t0 + Duration::from_millis(10)), Some(UW));
    }

    #[test]
    fn presets_switch_now_without_hysteresis() {
        let t0 = Instant::now();
        let mut p = ZoomPlanner::new(cheetah(), 1.0);
        // 5x is inside main's hysteresis band, but a preset takes the tele.
        assert_eq!(p.set(5.0, ZoomSource::Preset, t0), Some(TELE));
        assert_eq!(p.set(2.0, ZoomSource::Preset, t0), Some(MAIN));
        assert_eq!(p.set(2.5, ZoomSource::Preset, t0), None);
        assert_eq!(p.set(0.5, ZoomSource::Preset, t0), Some(UW));
    }

    #[test]
    fn next_tick_counts_down_the_settle_time() {
        let t0 = Instant::now();
        let mut p = ZoomPlanner::new(cheetah(), 1.0);
        assert_eq!(p.next_tick(t0), None);
        p.set(3.0, ZoomSource::Gesture, t0);
        assert_eq!(
            p.next_tick(t0 + Duration::from_millis(50)),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn a_single_lens_ladder_zooms_digitally() {
        let l = Ladder::new(vec![1.0], 4.0);
        assert_eq!(l.natural(0.5), 0);
        assert_eq!(l.sticky(3.0, 0), 0);
        assert_eq!(l.clamp(0.5), 1.0);
        assert_eq!(l.scale(3.0, 0), 3.0);
    }
}
