//! Carrying exposure from one camera to the next at a lens switch, so the
//! new camera's first frames start near the right brightness instead of
//! at its algorithm's default (CAMERA-APP-PLAN §3.4 step 3).
//!
//! The light a pixel collects scales with exposure time and with the
//! aperture's area, 1/N². So to keep the image as bright through a lens
//! with f-number N_new, exposure × gain must grow by (N_new/N_old)². Time
//! is kept under the frame budget, and the rest goes to gain.

/// Exposure as time and analog gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exposure {
    pub time_us: f64,
    pub gain: f64,
}

/// Limits of the camera the exposure moves to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// The longest exposure that keeps the frame rate (33 333 µs at 30 fps).
    pub max_time_us: f64,
    pub min_time_us: f64,
    pub max_gain: f64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_time_us: 33_333.0,
            min_time_us: 100.0,
            max_gain: 16.0,
        }
    }
}

/// The exposure for the new camera. `sensitivity` is any further measured
/// ratio of the new sensor's response to the old one's (1 when unknown).
pub fn transfer(
    old: Exposure,
    f_old: f64,
    f_new: f64,
    sensitivity: f64,
    limits: Limits,
) -> Exposure {
    let ratio = (f_new / f_old).powi(2) / sensitivity.max(f64::EPSILON);
    let total = old.time_us * old.gain.max(1.0) * ratio;
    let time_us = total.clamp(limits.min_time_us, limits.max_time_us);
    let gain = (total / time_us).clamp(1.0, limits.max_gain);
    Exposure { time_us, gain }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6 * b.abs().max(1.0)
    }

    #[test]
    fn main_to_tele_needs_3_6x_the_light() {
        // f/1.85 main to f/3.5 tele: (3.5/1.85)^2 = 3.579
        let old = Exposure {
            time_us: 5_000.0,
            gain: 2.0,
        };
        let new = transfer(old, 1.85, 3.5, 1.0, Limits::default());
        assert!(close(
            new.time_us * new.gain,
            10_000.0 * 3.5f64.powi(2) / 1.85f64.powi(2)
        ));
        assert!(close(new.time_us, 33_333.0), "time fills the frame first");
    }

    #[test]
    fn time_stays_under_the_frame_budget_and_the_rest_is_gain() {
        let old = Exposure {
            time_us: 30_000.0,
            gain: 4.0,
        };
        let new = transfer(old, 1.85, 2.2, 1.0, Limits::default());
        assert!(close(new.time_us, 33_333.0));
        assert!(new.gain > 4.0 && new.gain < 16.0);
    }

    #[test]
    fn gain_is_clamped_at_the_ceiling() {
        let old = Exposure {
            time_us: 33_000.0,
            gain: 15.0,
        };
        let new = transfer(old, 1.85, 3.5, 1.0, Limits::default());
        assert_eq!(new.gain, 16.0);
    }

    #[test]
    fn a_brighter_lens_shortens_the_exposure_before_the_gain_drops_to_1() {
        let old = Exposure {
            time_us: 10_000.0,
            gain: 1.0,
        };
        let new = transfer(old, 3.5, 1.85, 1.0, Limits::default());
        assert_eq!(new.gain, 1.0);
        assert!(close(new.time_us, 10_000.0 * (1.85f64 / 3.5).powi(2)));
    }

    #[test]
    fn sensitivity_divides_the_needed_light() {
        let old = Exposure {
            time_us: 1_000.0,
            gain: 1.0,
        };
        let new = transfer(old, 2.0, 2.0, 2.0, Limits::default());
        assert!(close(new.time_us, 500.0));
    }
}
