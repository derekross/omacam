//! Low-light lens fallback: in dim light the telephoto (f/3.5, about 3.6
//! times less light than the main camera's f/1.85) gives way to a crop of
//! the main camera at the same zoom, as stock does, and comes back when
//! the light allows.
//!
//! The light is judged from what auto exposure chose: exposure time times
//! gain (analog and digital) is what the scene asks of a lens, and scaled
//! by the square of the f-number ratio it says what the tele would need.
//! So any back lens's frames judge the light, and zooming to 5x in the
//! dark goes straight to the main camera. A tele that has hit its limits
//! reports its limits, which is at or past the threshold too.
//!
//! The scene counts as dim once the tele would need more than
//! [`Config::dim_gain`] at a full frame time, and as bright again only
//! below half of that (a stop of hysteresis). Either has to hold for its
//! dwell time before anything switches, so a passing shadow or the
//! exposure settling after a switch doesn't flap the lenses. Frames whose
//! exposure is still searching are not judged.
//!
//! The ultrawide has no fallback: the main camera can't show a wider view
//! than its own, so falling back would change the framing, and a user who
//! chose 0.5x wants that view (the ultrawide's f/2.2 is close to the
//! main's anyway).

use std::time::{Duration, Instant};

use crate::backend::{AeState, FrameMeta};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// Frame time at which a lens runs out of exposure time, microseconds
    /// (30 fps).
    pub frame_time_us: f64,
    /// Gain past which the tele counts as struggling (8: ISO 400 at the
    /// sensors' base ISO 50).
    pub dim_gain: f64,
    /// How far below the dim threshold the light must come back, as a
    /// factor (2: one stop).
    pub hysteresis: f64,
    /// How long dim light must last before falling back.
    pub dim_dwell: Duration,
    /// How long good light must last before going back to the tele.
    pub bright_dwell: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            frame_time_us: 33_333.0,
            dim_gain: 8.0,
            hysteresis: 2.0,
            dim_dwell: Duration::from_millis(1500),
            bright_dwell: Duration::from_secs(3),
        }
    }
}

impl Config {
    /// The demand (microseconds x gain at the tele's aperture) at which
    /// the scene is dim. Capped below what the tele can reach, so a tele
    /// whose gain tops out under `dim_gain` still falls back at its limit.
    pub fn dim_threshold(&self, tele_max_gain: Option<f64>) -> f64 {
        let gain = match tele_max_gain {
            Some(g) if g.is_finite() && g > 1.0 => self.dim_gain.min(0.9 * g),
            _ => self.dim_gain,
        };
        self.frame_time_us * gain
    }
}

/// What a frame's exposure says the tele would need: exposure time times
/// gain, scaled from the frame's lens aperture to the tele's. None when
/// the frame doesn't say, or while auto exposure is still searching.
pub fn tele_demand(meta: &FrameMeta, f_number: f64, tele_f_number: f64) -> Option<f64> {
    if meta.ae_state == Some(AeState::Searching) {
        return None;
    }
    let t = meta.exposure_us? as f64;
    let gain = meta.analog_gain? as f64 * meta.digital_gain.unwrap_or(1.0) as f64;
    if !(t > 0.0 && gain > 0.0 && f_number > 0.0 && tele_f_number > 0.0) {
        return None;
    }
    Some(t * gain * (tele_f_number / f_number).powi(2))
}

/// Whether the scene is too dim for the tele, with hysteresis and dwell.
#[derive(Debug, Clone)]
pub struct LowLight {
    config: Config,
    dim: bool,
    /// Since when the frames have said the other state.
    other_since: Option<Instant>,
}

impl Default for LowLight {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl LowLight {
    pub fn new(config: Config) -> LowLight {
        LowLight {
            config,
            dim: false,
            other_since: None,
        }
    }

    pub fn is_dim(&self) -> bool {
        self.dim
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Forget what was seen (another side, the app suspended).
    pub fn reset(&mut self) {
        self.dim = false;
        self.other_since = None;
    }

    /// One frame's demand (None: nothing to judge). Returns the new state
    /// when it changes.
    pub fn update(
        &mut self,
        demand: Option<f64>,
        tele_max_gain: Option<f64>,
        now: Instant,
    ) -> Option<bool> {
        let demand = demand?;
        let dim_at = self.config.dim_threshold(tele_max_gain);
        let says_other = if self.dim {
            demand <= dim_at / self.config.hysteresis
        } else {
            demand >= dim_at
        };
        if !says_other {
            self.other_since = None;
            return None;
        }
        let since = *self.other_since.get_or_insert(now);
        let dwell = if self.dim {
            self.config.bright_dwell
        } else {
            self.config.dim_dwell
        };
        if now.duration_since(since) < dwell {
            return None;
        }
        self.dim = !self.dim;
        self.other_since = None;
        Some(self.dim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(us: i32, gain: f32) -> FrameMeta {
        FrameMeta {
            exposure_us: Some(us),
            analog_gain: Some(gain),
            ..FrameMeta::default()
        }
    }

    const MAIN_F: f64 = 1.85;
    const TELE_F: f64 = 3.5;

    #[test]
    fn demand_scales_by_the_aperture_ratio() {
        let d = tele_demand(&meta(10_000, 2.0), MAIN_F, TELE_F).unwrap();
        assert!((d - 20_000.0 * (3.5f64 / 1.85).powi(2)).abs() < 1e-6);
        // Digital gain counts; a searching AE doesn't.
        let mut m = meta(33_000, 16.0);
        m.digital_gain = Some(2.0);
        assert_eq!(tele_demand(&m, TELE_F, TELE_F), Some(33_000.0 * 32.0));
        m.ae_state = Some(AeState::Searching);
        assert_eq!(tele_demand(&m, TELE_F, TELE_F), None);
        assert_eq!(tele_demand(&FrameMeta::default(), MAIN_F, TELE_F), None);
    }

    #[test]
    fn dim_light_falls_back_after_the_dwell_and_comes_back_later() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut l = LowLight::default();
        // A tele at 1/30 s and gain 12: dim.
        let dim = tele_demand(&meta(33_333, 12.0), TELE_F, TELE_F);
        assert_eq!(l.update(dim, Some(16.0), ms(0)), None);
        assert_eq!(l.update(dim, Some(16.0), ms(1000)), None);
        assert_eq!(l.update(dim, Some(16.0), ms(1500)), Some(true));
        assert!(l.is_dim());
        // On the main: at gain 6 and 1/30 s it would be dim on the tele
        // (6 x 3.58 = 21); at 1/30 s and gain 1.5 (5.4 on the tele) it is
        // inside the hysteresis band and nothing changes.
        let band = tele_demand(&meta(33_333, 1.5), MAIN_F, TELE_F);
        assert_eq!(l.update(band, Some(16.0), ms(2000)), None);
        assert_eq!(l.update(band, Some(16.0), ms(9000)), None);
        // Bright: 1/100 s at gain 1 on the main (about 1.1 on the tele).
        let bright = tele_demand(&meta(10_000, 1.0), MAIN_F, TELE_F);
        assert_eq!(l.update(bright, Some(16.0), ms(10_000)), None);
        assert_eq!(l.update(bright, Some(16.0), ms(12_999)), None);
        assert_eq!(l.update(bright, Some(16.0), ms(13_000)), Some(false));
    }

    #[test]
    fn a_short_dip_does_not_switch() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut l = LowLight::default();
        let dim = tele_demand(&meta(33_333, 12.0), TELE_F, TELE_F);
        let ok = tele_demand(&meta(10_000, 2.0), TELE_F, TELE_F);
        assert_eq!(l.update(dim, None, ms(0)), None);
        assert_eq!(l.update(ok, None, ms(1000)), None);
        // The dwell starts over.
        assert_eq!(l.update(dim, None, ms(1100)), None);
        assert_eq!(l.update(dim, None, ms(2500)), None);
        assert_eq!(l.update(dim, None, ms(2600)), Some(true));
        // No metadata: nothing judged, nothing reset.
        assert_eq!(l.update(None, None, ms(9000)), None);
        assert!(l.is_dim());
    }

    #[test]
    fn a_tele_whose_gain_tops_out_low_still_falls_back_at_its_limit() {
        let c = Config::default();
        assert_eq!(c.dim_threshold(Some(16.0)), 33_333.0 * 8.0);
        assert!((c.dim_threshold(Some(4.0)) - 33_333.0 * 3.6).abs() < 1e-6);
        assert_eq!(c.dim_threshold(None), 33_333.0 * 8.0);
        let t0 = Instant::now();
        let mut l = LowLight::default();
        let at_limit = tele_demand(&meta(33_333, 4.0), TELE_F, TELE_F);
        l.update(at_limit, Some(4.0), t0);
        assert_eq!(
            l.update(at_limit, Some(4.0), t0 + Duration::from_secs(2)),
            Some(true)
        );
    }
}
