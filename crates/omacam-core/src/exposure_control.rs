//! What the user does to exposure, white balance and focus: tap to meter,
//! the EV slider, AE/AF lock, and the Pro panel's manual values. Pure: the
//! controller turns [`Exposure::effective`] into controls for the camera.
//!
//! - **Tap to meter.** libcamera has no standard control for an exposure
//!   region, so a tap meters by brightness: the mean luma of the tapped
//!   area is brought to mid-grey through `ExposureValue`
//!   (CAMERA-APP-PLAN §3.5). A camera without ExposureValue only focuses.
//! - **The EV slider** adds to that, -2..+2 EV, and resets at the next tap
//!   or lens switch.
//! - **AE/AF lock** (long press) holds the exposure time, gain and lens
//!   position the last frame reported, as manual values; a tap releases it.
//! - **Pro values** (shutter, ISO, white balance, manual focus) hold until
//!   set back to Auto, across lens switches; each applies only where the
//!   lens's camera offers the control.

use crate::backend::{Awb, Caps, ExposureSettings, FrameMeta, Setting};

/// The brightness a metered area is brought to: 18 % grey in sRGB.
pub const MID_GREY: f64 = 118.0;
/// The EV range of the tap metering and the slider.
pub const EV_LIMIT: f32 = 2.0;

/// A held exposure (AE/AF lock).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Lock {
    /// AeEnable=false: the camera holds its exposure (libcamera 4.10).
    ae_hold: bool,
    /// Otherwise the reported exposure, held as manual values.
    manual: Option<(i32, f32)>,
    /// The reported lens position, held as manual focus...
    lens: Option<f32>,
    /// ...or continuous focus paused (AfPause).
    af_pause: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Exposure {
    /// The Pro values. Their `ev` is unused: EV is metering + slider.
    pro: ExposureSettings,
    ev_meter: f32,
    ev_user: f32,
    lock: Option<Lock>,
    /// The last tap metered its window (AeMeteringMode Spot).
    spot: bool,
}

fn clamp_ev(ev: f32, caps: &Caps) -> f32 {
    let (lo, hi) = caps.ev.unwrap_or((0.0, 0.0));
    ev.clamp(lo.max(-EV_LIMIT), hi.min(EV_LIMIT))
}

impl Exposure {
    pub fn new() -> Exposure {
        Exposure::default()
    }

    pub fn pro(&self) -> &ExposureSettings {
        &self.pro
    }

    pub fn ev_user(&self) -> f32 {
        self.ev_user
    }

    pub fn is_locked(&self) -> bool {
        self.lock.is_some()
    }

    /// The settings to send to a camera with `caps`: what it can't do stays
    /// Auto, and values are kept inside its ranges.
    pub fn effective(&self, caps: &Caps) -> ExposureSettings {
        let ev = if caps.ev.is_some() {
            clamp_ev(self.ev_meter + self.ev_user, caps)
        } else {
            0.0
        };
        let (shutter, gain) = match self.lock.and_then(|l| l.manual) {
            Some((t, g)) => (Setting::Manual(t), Setting::Manual(g)),
            None => (self.pro.shutter, self.pro.gain),
        };
        let shutter = match (shutter, caps.manual_shutter(), caps.exposure_time) {
            (Setting::Manual(t), true, Some((lo, hi))) => Setting::Manual(t.clamp(lo, hi.max(lo))),
            _ => Setting::Auto,
        };
        let gain = match (gain, caps.manual_gain(), caps.gain) {
            (Setting::Manual(g), true, Some((lo, hi))) => Setting::Manual(g.clamp(lo, hi.max(lo))),
            _ => Setting::Auto,
        };
        let white_balance = match self.pro.white_balance {
            Setting::Manual(Awb::Preset(p)) if caps.awb_modes => Setting::Manual(Awb::Preset(p)),
            // No presets: the preset's temperature, or its gains.
            Setting::Manual(Awb::Preset(p)) => match (caps.colour_temperature, caps.colour_gains) {
                (Some((lo, hi)), _) => {
                    Setting::Manual(Awb::Kelvin((p.kelvin() as i32).clamp(lo, hi) as u32))
                }
                (None, Some(range)) => Setting::Manual(Awb::Gains(clamp_gains(p.gains(), range))),
                _ => Setting::Auto,
            },
            Setting::Manual(Awb::Kelvin(k)) => match caps.colour_temperature {
                Some((lo, hi)) => Setting::Manual(Awb::Kelvin((k as i32).clamp(lo, hi) as u32)),
                None => Setting::Auto,
            },
            Setting::Manual(Awb::Gains(g)) => match caps.colour_gains {
                Some(range) => Setting::Manual(Awb::Gains(clamp_gains(g, range))),
                None => Setting::Auto,
            },
            Setting::Auto => Setting::Auto,
        };
        let lens = match (
            self.lock.and_then(|l| l.lens),
            self.pro.lens,
            caps.lens_position,
        ) {
            (Some(p), _, Some((lo, hi))) | (None, Setting::Manual(p), Some((lo, hi))) => {
                Setting::Manual(p.clamp(lo, hi))
            }
            _ => Setting::Auto,
        };
        let ae_hold = self.lock.is_some_and(|l| l.ae_hold) && caps.ae_enable;
        ExposureSettings {
            ev,
            shutter,
            gain,
            white_balance,
            lens,
            ae_hold,
            af_hold: self.lock.is_some_and(|l| l.af_pause) && caps.af_pause && lens.is_auto(),
            spot: self.spot && caps.ae_metering,
        }
    }

    /// A tap: release a lock, reset the slider, and meter the tapped area
    /// (its mean luma, 0-255) when the camera has ExposureValue. Returns
    /// whether anything changed.
    pub fn tap(&mut self, luma: Option<f64>, caps: &Caps) -> bool {
        let before = self.clone();
        self.lock = None;
        self.ev_user = 0.0;
        // Real spot metering where the camera offers it; else bring the
        // area's brightness to mid-grey through EV.
        self.spot = caps.ae_metering;
        self.ev_meter = match (luma, caps.ev.is_some(), self.spot) {
            (Some(l), true, false) if l > 0.5 => clamp_ev((MID_GREY / l).log2() as f32, caps),
            _ => 0.0,
        };
        *self != before
    }

    /// The EV slider.
    pub fn set_ev(&mut self, ev: f32, caps: &Caps) {
        self.ev_user = ev.clamp(-EV_LIMIT, EV_LIMIT);
        // Keep metering + slider inside the camera's range.
        let total = clamp_ev(self.ev_meter + self.ev_user, caps);
        self.ev_user = total - self.ev_meter;
    }

    /// AE/AF lock. With AeEnable the camera holds its own exposure; else
    /// the exposure time and gain the last frame reported are held as
    /// manual values (which needs them in the metadata and manual
    /// exposure). Focus holds at the reported lens position, or through
    /// AfPause.
    pub fn lock(&mut self, meta: Option<&FrameMeta>, caps: &Caps) -> bool {
        let manual = meta
            .and_then(|m| m.exposure_us.zip(m.analogue_gain))
            .filter(|_| caps.manual_shutter() && caps.manual_gain());
        if !caps.ae_enable && manual.is_none() {
            return false;
        }
        let lens = meta
            .and_then(|m| m.lens_position)
            .filter(|_| caps.lens_position.is_some());
        self.lock = Some(Lock {
            ae_hold: caps.ae_enable,
            manual: if caps.ae_enable { None } else { manual },
            lens,
            af_pause: lens.is_none() && caps.af_pause,
        });
        true
    }

    pub fn unlock(&mut self) {
        self.lock = None;
    }

    /// Another lens: metering, the slider and a lock start over; Pro values
    /// stay.
    pub fn lens_switched(&mut self) {
        self.ev_meter = 0.0;
        self.ev_user = 0.0;
        self.lock = None;
        self.spot = false;
    }

    pub fn set_shutter(&mut self, s: Setting<i32>) {
        self.pro.shutter = s;
    }

    pub fn set_gain(&mut self, s: Setting<f32>) {
        self.pro.gain = s;
    }

    pub fn set_white_balance(&mut self, s: Setting<Awb>) {
        self.pro.white_balance = s;
    }

    pub fn set_lens(&mut self, s: Setting<f32>) {
        self.pro.lens = s;
    }

    /// Back to fully automatic.
    pub fn reset(&mut self) {
        *self = Exposure::default();
    }
}

fn clamp_gains(g: [f32; 2], (lo, hi): (f32, f32)) -> [f32; 2] {
    [g[0].clamp(lo, hi), g[1].clamp(lo, hi)]
}

/// Gain to ISO for a sensor whose ISO at gain 1 is `base`.
pub fn iso(gain: f64, base: f64) -> u32 {
    (gain * base).round().max(1.0) as u32
}

pub fn gain_for_iso(iso: u32, base: f64) -> f32 {
    (iso as f64 / base.max(1.0)) as f32
}

/// Shutter speeds a dial offers, in microseconds, inside `range`: whole
/// stops from 1/8000 s to 1 s.
pub fn shutter_stops(range: (i32, i32)) -> Vec<i32> {
    const STOPS: [i32; 14] = [
        125, 250, 500, 1_000, 2_000, 4_000, 8_000, 16_667, 33_333, 66_667, 125_000, 250_000,
        500_000, 1_000_000,
    ];
    STOPS
        .into_iter()
        .filter(|t| *t >= range.0 && *t <= range.1)
        .collect()
}

/// ISO values a dial offers: whole stops from the base up to the most the
/// gain range allows.
pub fn iso_stops(base: f64, gain: (f32, f32)) -> Vec<u32> {
    let lo = (base * gain.0.max(1.0) as f64).round() as u32;
    let hi = (base * gain.1 as f64).round() as u32;
    let mut out = Vec::new();
    let mut v = lo.max(1);
    while v <= hi {
        out.push(v);
        v *= 2;
    }
    if out.last().is_some_and(|l| *l != hi) {
        out.push(hi);
    }
    out
}

/// A shutter speed as photographers write it: "1/120", "1/4", "1 s".
pub fn format_shutter(us: i32) -> String {
    if us >= 1_000_000 {
        let s = us as f64 / 1e6;
        if (s - s.round()).abs() < 0.05 {
            format!("{} s", s.round() as i64)
        } else {
            format!("{s:.1} s")
        }
    } else if us <= 0 {
        "-".into()
    } else {
        format!("1/{}", (1e6 / us as f64).round() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::AwbPreset;

    fn full() -> Caps {
        Caps {
            ev: Some((-8.0, 8.0)),
            exposure_time: Some((100, 66_000)),
            gain: Some((1.0, 16.0)),
            manual_exposure: true,
            awb_modes: true,
            colour_temperature: Some((2500, 7500)),
            lens_position: Some((0.0, 1.0)),
            ae_enable: false,
            ae_metering: false,
            af_pause: false,
            colour_gains: None,
        }
    }

    /// The table libcamera 4.10 offers on the main camera: no presets or
    /// temperature, gains instead; AeEnable for the lock; spot metering.
    fn libcamera_4_10_main() -> Caps {
        Caps {
            ev: Some((-2.0, 2.0)),
            exposure_time: Some((10, 33_000)),
            gain: Some((1.0, 16.0)),
            manual_exposure: true,
            awb_modes: false,
            colour_temperature: None,
            lens_position: Some((0.0, 1.0)),
            ae_enable: true,
            ae_metering: true,
            af_pause: true,
            colour_gains: Some((0.25, 4.0)),
        }
    }

    #[test]
    fn on_libcamera_4_10_a_tap_spot_meters_and_the_lock_uses_ae_enable() {
        let caps = libcamera_4_10_main();
        let mut e = Exposure::new();
        e.tap(Some(29.5), &caps);
        let s = e.effective(&caps);
        assert!(s.spot, "real spot metering");
        assert_eq!(s.ev, 0.0, "no brightness EV when the camera meters");
        // Lock without metadata: AeEnable holds; focus pauses.
        assert!(e.lock(None, &caps));
        let s = e.effective(&caps);
        assert!(s.ae_hold && s.af_hold);
        assert_eq!(s.shutter, Setting::Auto);
        // With a reported lens position: held as manual focus instead.
        let meta = FrameMeta {
            lens_position: Some(0.42),
            ..FrameMeta::default()
        };
        e.lock(Some(&meta), &caps);
        let s = e.effective(&caps);
        assert!(s.ae_hold && !s.af_hold);
        assert_eq!(s.lens, Setting::Manual(0.42));
        e.tap(None, &caps);
        assert!(!e.effective(&caps).ae_hold);
    }

    #[test]
    fn presets_become_gains_where_there_is_no_temperature() {
        let caps = libcamera_4_10_main();
        let mut e = Exposure::new();
        e.set_white_balance(Setting::Manual(Awb::Preset(AwbPreset::Incandescent)));
        assert_eq!(
            e.effective(&caps).white_balance,
            Setting::Manual(Awb::Gains(AwbPreset::Incandescent.gains()))
        );
        // The ultrawide offers a temperature: the preset's kelvin.
        let uw = Caps {
            colour_temperature: Some((2890, 7900)),
            ..caps
        };
        assert_eq!(
            e.effective(&uw).white_balance,
            Setting::Manual(Awb::Kelvin(2890)),
            "2850 K clamped into 2890..7900"
        );
    }

    #[test]
    fn auto_by_default_and_on_todays_libcamera() {
        let e = Exposure::new();
        assert_eq!(e.effective(&full()), ExposureSettings::default());
        // libcamera 4.8: none of the manual controls offered.
        assert_eq!(e.effective(&Caps::default()), ExposureSettings::default());
    }

    #[test]
    fn a_tap_meters_the_area_to_mid_grey() {
        let mut e = Exposure::new();
        // A dark area (29.5, two stops under) brightens by 2 EV.
        assert!(e.tap(Some(29.5), &full()));
        assert!((e.effective(&full()).ev - 2.0).abs() < 1e-3);
        // A bright area darkens, but not past -2 EV.
        e.tap(Some(255.0), &full());
        let ev = e.effective(&full()).ev;
        assert!((ev + 1.11).abs() < 0.01, "{ev}");
        e.tap(Some(1000.0), &full());
        assert_eq!(e.effective(&full()).ev, -2.0);
        // Without ExposureValue: no metering.
        e.tap(Some(29.5), &Caps::default());
        assert_eq!(e.effective(&Caps::default()).ev, 0.0);
    }

    #[test]
    fn the_slider_adds_to_the_metering_and_a_tap_resets_it() {
        let mut e = Exposure::new();
        e.tap(Some(MID_GREY), &full());
        e.set_ev(1.3, &full());
        assert!((e.effective(&full()).ev - 1.3).abs() < 1e-6);
        e.set_ev(9.0, &full());
        assert_eq!(e.ev_user(), 2.0);
        e.tap(Some(MID_GREY), &full());
        assert_eq!(e.ev_user(), 0.0);
        // A narrow camera range caps the sum.
        let narrow = Caps {
            ev: Some((-1.0, 1.0)),
            ..full()
        };
        e.set_ev(2.0, &narrow);
        assert_eq!(e.effective(&narrow).ev, 1.0);
    }

    #[test]
    fn a_lock_holds_the_reported_exposure_and_a_tap_releases_it() {
        let mut e = Exposure::new();
        let meta = FrameMeta {
            exposure_us: Some(10_000),
            analogue_gain: Some(2.5),
            lens_position: Some(0.4),
            ..FrameMeta::default()
        };
        assert!(e.lock(Some(&meta), &full()));
        let s = e.effective(&full());
        assert_eq!(s.shutter, Setting::Manual(10_000));
        assert_eq!(s.gain, Setting::Manual(2.5));
        assert_eq!(s.lens, Setting::Manual(0.4));
        assert!(e.is_locked());
        e.tap(None, &full());
        assert!(!e.is_locked());
        assert_eq!(e.effective(&full()).shutter, Setting::Auto);
        // No manual exposure, or no metadata: no lock.
        assert!(!e.lock(Some(&meta), &Caps::default()));
        assert!(!e.lock(Some(&FrameMeta::default()), &full()));
    }

    #[test]
    fn pro_values_apply_only_where_offered_and_inside_ranges() {
        let mut e = Exposure::new();
        e.set_shutter(Setting::Manual(1_000_000));
        e.set_gain(Setting::Manual(0.5));
        e.set_lens(Setting::Manual(3.0));
        e.set_white_balance(Setting::Manual(Awb::Preset(AwbPreset::Cloudy)));
        let s = e.effective(&full());
        assert_eq!(s.shutter, Setting::Manual(66_000));
        assert_eq!(s.gain, Setting::Manual(1.0));
        assert_eq!(s.lens, Setting::Manual(1.0));
        assert_eq!(
            s.white_balance,
            Setting::Manual(Awb::Preset(AwbPreset::Cloudy))
        );
        // A fixed-focus camera with a temperature but no presets.
        let caps = Caps {
            lens_position: None,
            awb_modes: false,
            ..full()
        };
        let s = e.effective(&caps);
        assert_eq!(s.lens, Setting::Auto);
        assert_eq!(s.white_balance, Setting::Manual(Awb::Kelvin(6500)));
        // Today's libcamera: all of it stays automatic.
        assert_eq!(e.effective(&Caps::default()), ExposureSettings::default());
    }

    #[test]
    fn a_lens_switch_resets_ev_and_lock_but_keeps_pro_values() {
        let mut e = Exposure::new();
        e.set_shutter(Setting::Manual(4_000));
        e.tap(Some(29.5), &full());
        e.set_ev(-1.0, &full());
        e.lens_switched();
        let s = e.effective(&full());
        assert_eq!(s.ev, 0.0);
        assert_eq!(s.shutter, Setting::Manual(4_000));
    }

    #[test]
    fn iso_and_shutter_dials() {
        assert_eq!(iso(2.0, 50.0), 100);
        assert!((gain_for_iso(400, 50.0) - 8.0).abs() < 1e-6);
        assert_eq!(iso_stops(50.0, (1.0, 16.0)), vec![50, 100, 200, 400, 800]);
        assert_eq!(iso_stops(50.0, (1.0, 12.0)), vec![50, 100, 200, 400, 600]);
        let s = shutter_stops((100, 33_333));
        assert_eq!(s.first(), Some(&125));
        assert_eq!(s.last(), Some(&33_333));
        assert_eq!(format_shutter(8_333), "1/120");
        assert_eq!(format_shutter(33_333), "1/30");
        assert_eq!(format_shutter(1_000_000), "1 s");
        assert_eq!(format_shutter(500_000), "1/2");
    }
}
