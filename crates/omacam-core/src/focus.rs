//! Tap to focus: from a tap on the picture to a focus window on the
//! sensor, and the state of the scan it starts.
//!
//! A tap arrives as a point on the drawn picture, (0, 0) at its top left
//! and (1, 1) at its bottom right. The picture is the frame turned upright,
//! mirrored for the front camera, and cropped by the zoom about its center,
//! so the tap is mapped back through each of those to a point on the frame,
//! and from there to `ScalerCropMaximum`, the coordinates `AfWindows` uses.
//! The processed frame is taken to show all of `ScalerCropMaximum`; the
//! sensor drivers don't report their modes' crops yet (CAMERA-APP-PLAN K4),
//! so on a cropped mode (the main camera's 16:9) the window lands slightly
//! off vertically.
//!
//! Cameras whose autofocus takes no `AfWindows` (the software ISP's
//! today) still scan when tapped, on their own fixed window, and the UI
//! shows the ring there, not where the tap was. A camera without
//! autofocus shows that it's fixed-focus. A scan that reports no AF state
//! at all, as when the lens doesn't answer, ends as unavailable instead of
//! spinning forever.

use std::time::{Duration, Instant};

use crate::backend::{AfState, FocusRequest, FrameMeta, Rect};
use crate::controller::Transform;

/// Side of the focus window, as a fraction of the shorter frame side.
pub const WINDOW_FRACTION: f64 = 0.15;
/// Longest a scan may take before it counts as failed.
pub const SCAN_TIMEOUT: Duration = Duration::from_millis(2500);
/// How long a tapped focus holds before continuous AF takes over again.
pub const HOLD: Duration = Duration::from_secs(5);
/// How long the "fixed focus" or "unavailable" ring stays.
pub const NOTICE: Duration = Duration::from_millis(1500);

/// A tap on the drawn picture back to a point on the frame, both
/// normalized to 0..1.
pub fn picture_to_frame(u: f64, v: f64, t: &Transform) -> (f64, f64) {
    let scale = t.scale.max(1.0);
    let u = 0.5 + (u - 0.5) / scale;
    let v = 0.5 + (v - 0.5) / scale;
    let u = if t.mirror { 1.0 - u } else { u };
    // The picture is the frame turned clockwise by `rotation`.
    let (x, y) = match t.rotation % 360 {
        90 => (v, 1.0 - u),
        180 => (1.0 - u, 1.0 - v),
        270 => (1.0 - v, u),
        _ => (u, v),
    };
    (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))
}

/// The inverse of [`picture_to_frame`]: where a frame point is drawn.
pub fn frame_to_picture(x: f64, y: f64, t: &Transform) -> (f64, f64) {
    let (u, v) = match t.rotation % 360 {
        90 => (1.0 - y, x),
        180 => (1.0 - x, 1.0 - y),
        270 => (y, 1.0 - x),
        _ => (x, y),
    };
    let u = if t.mirror { 1.0 - u } else { u };
    let scale = t.scale.max(1.0);
    (0.5 + (u - 0.5) * scale, 0.5 + (v - 0.5) * scale)
}

/// A focus window centered on a frame point, inside `area`.
pub fn window(x: f64, y: f64, area: Rect) -> Rect {
    // A square of WINDOW_FRACTION of the shorter side, but at least 10 %
    // of each side (libcamera 4.10's minimum for AfWindows).
    let base = (area.width.min(area.height) as f64 * WINDOW_FRACTION).max(1.0);
    let w = (base.max(area.width as f64 * 0.1) as u32).min(area.width);
    let h = (base.max(area.height as f64 * 0.1) as u32).min(area.height);
    let cx = area.x as f64 + x * area.width as f64;
    let cy = area.y as f64 + y * area.height as f64;
    let max_x = area.x + (area.width - w) as i32;
    let max_y = area.y + (area.height - h) as i32;
    Rect {
        x: ((cx - w as f64 / 2.0).round() as i32).clamp(area.x, max_x),
        y: ((cy - h as f64 / 2.0).round() as i32).clamp(area.y, max_y),
        width: w,
        height: h,
    }
}

/// What the focus ring shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Scanning,
    Focused,
    Failed,
    /// The camera has no autofocus.
    FixedFocus,
    /// The scan reported nothing (the lens didn't answer).
    Unavailable,
}

/// The ring: where on the picture (0..1) and how it went.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Indicator {
    pub u: f64,
    pub v: f64,
    pub status: Status,
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Idle,
    Scanning { since: Instant, saw_state: bool },
    Holding { until: Instant },
    Notice { until: Instant },
}

/// What a lens can do about focus.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Capability {
    pub autofocus: bool,
    pub af_windows: bool,
    /// `ScalerCropMaximum`, where windows are placed.
    pub area: Option<Rect>,
}

/// What the controller should do after a tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    /// The hold is over: go back to continuous autofocus.
    ResumeContinuous,
}

#[derive(Debug, Clone)]
pub struct FocusController {
    state: State,
    indicator: Option<Indicator>,
}

impl Default for FocusController {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusController {
    pub fn new() -> FocusController {
        FocusController {
            state: State::Idle,
            indicator: None,
        }
    }

    pub fn indicator(&self) -> Option<Indicator> {
        self.indicator
    }

    /// True while a tapped focus overrides continuous AF.
    pub fn is_active(&self) -> bool {
        matches!(self.state, State::Scanning { .. } | State::Holding { .. })
    }

    /// A tap at picture point (u, v) on a lens. Returns the request to
    /// send, or None for a fixed-focus lens.
    pub fn tap(
        &mut self,
        u: f64,
        v: f64,
        transform: &Transform,
        lens: Capability,
        now: Instant,
    ) -> Option<FocusRequest> {
        if !lens.autofocus {
            self.state = State::Notice {
                until: now + NOTICE,
            };
            self.indicator = Some(Indicator {
                u,
                v,
                status: Status::FixedFocus,
            });
            return None;
        }
        let (x, y) = picture_to_frame(u, v, transform);
        let (request, (ru, rv)) = match (lens.af_windows, lens.area) {
            (true, Some(area)) => (
                FocusRequest {
                    window: Some(window(x, y, area)),
                },
                (u, v),
            ),
            // No windows: the AF scans its own center window, so show the
            // ring on the frame's center.
            _ => (
                FocusRequest { window: None },
                frame_to_picture(0.5, 0.5, transform),
            ),
        };
        self.state = State::Scanning {
            since: now,
            saw_state: false,
        };
        self.indicator = Some(Indicator {
            u: ru,
            v: rv,
            status: Status::Scanning,
        });
        Some(request)
    }

    /// Metadata of a frame from the current stream.
    pub fn on_meta(&mut self, meta: &FrameMeta, now: Instant) {
        let State::Scanning { since, saw_state } = &mut self.state else {
            return;
        };
        // The first frames may predate the trigger: wait for Scanning or a
        // result after it.
        let result = match meta.af_state {
            Some(AfState::Scanning) => {
                *saw_state = true;
                None
            }
            Some(AfState::Focused)
                if *saw_state || now.duration_since(*since) > Duration::from_millis(300) =>
            {
                Some(Status::Focused)
            }
            Some(AfState::Failed) if *saw_state => Some(Status::Failed),
            _ => None,
        };
        if let Some(status) = result {
            self.state = State::Holding { until: now + HOLD };
            if let Some(i) = self.indicator.as_mut() {
                i.status = status;
            }
        }
    }

    pub fn tick(&mut self, now: Instant) -> Action {
        match self.state.clone() {
            State::Scanning { since, saw_state } if now.duration_since(since) >= SCAN_TIMEOUT => {
                let status = if saw_state {
                    Status::Failed
                } else {
                    Status::Unavailable
                };
                if let Some(i) = self.indicator.as_mut() {
                    i.status = status;
                }
                self.state = State::Holding {
                    until: now + NOTICE,
                };
                Action::None
            }
            State::Holding { until } if now >= until => {
                self.state = State::Idle;
                self.indicator = None;
                Action::ResumeContinuous
            }
            State::Notice { until } if now >= until => {
                self.state = State::Idle;
                self.indicator = None;
                Action::None
            }
            _ => Action::None,
        }
    }

    /// The stream changed (lens switch, flip, still): its AF starts afresh.
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.indicator = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(rotation: u32, mirror: bool, scale: f64) -> Transform {
        Transform {
            scale,
            rotation,
            mirror,
        }
    }

    fn cap(autofocus: bool, af_windows: bool, area: Option<Rect>) -> Capability {
        Capability {
            autofocus,
            af_windows,
            area,
        }
    }

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    #[test]
    fn taps_map_back_through_each_rotation() {
        // The frame's top-left corner, (0, 0), turned clockwise by 90 is the
        // picture's top-right corner.
        assert!(close(
            picture_to_frame(1.0, 0.0, &t(90, false, 1.0)),
            (0.0, 0.0)
        ));
        // ... by 270 the picture's bottom-left.
        assert!(close(
            picture_to_frame(0.0, 1.0, &t(270, false, 1.0)),
            (0.0, 0.0)
        ));
        // ... by 180 the bottom-right.
        assert!(close(
            picture_to_frame(1.0, 1.0, &t(180, false, 1.0)),
            (0.0, 0.0)
        ));
        assert!(close(
            picture_to_frame(0.2, 0.3, &t(0, false, 1.0)),
            (0.2, 0.3)
        ));
        // The center stays the center whatever the transform.
        for r in [0, 90, 180, 270] {
            for m in [false, true] {
                assert!(close(picture_to_frame(0.5, 0.5, &t(r, m, 3.0)), (0.5, 0.5)));
            }
        }
    }

    #[test]
    fn mapping_round_trips_with_zoom_and_mirror() {
        for r in [0, 90, 180, 270] {
            for m in [false, true] {
                let tr = t(r, m, 2.0);
                let (x, y) = picture_to_frame(0.3, 0.8, &tr);
                assert!(close(frame_to_picture(x, y, &tr), (0.3, 0.8)), "{r} {m}");
            }
        }
        // At 2x the picture's edge is a quarter in from the frame's edge.
        assert!(close(
            picture_to_frame(0.0, 0.5, &t(0, false, 2.0)),
            (0.25, 0.5)
        ));
        // Mirrored, the left of the picture is the right of the frame.
        assert!(close(
            picture_to_frame(0.0, 0.5, &t(0, true, 1.0)),
            (1.0, 0.5)
        ));
    }

    #[test]
    fn windows_stay_inside_the_sensor_area() {
        let area = Rect {
            x: 8,
            y: 4,
            width: 4080,
            height: 3072,
        };
        let w = window(0.5, 0.5, area);
        assert_eq!(w.width, (3072.0 * WINDOW_FRACTION) as u32);
        assert!((w.x + w.width as i32 / 2 - (8 + 2040)).abs() <= 1);
        for (x, y) in [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0), (1.0, 0.0)] {
            let w = window(x, y, area);
            assert!(w.x >= area.x && w.y >= area.y);
            assert!(w.x + w.width as i32 <= area.x + area.width as i32);
            assert!(w.y + w.height as i32 <= area.y + area.height as i32);
            assert!(w.width as f64 >= area.width as f64 * 0.1);
            assert!(w.height as f64 >= area.height as f64 * 0.1);
        }
        // A wide area: at least 10 % of its width.
        let wide = Rect {
            x: 0,
            y: 0,
            width: 8000,
            height: 1000,
        };
        assert_eq!(window(0.5, 0.5, wide).width, 800);
    }

    #[test]
    fn a_tap_with_windows_scans_there_and_holds_then_resumes() {
        let now = Instant::now();
        let mut f = FocusController::new();
        let area = Rect {
            x: 0,
            y: 0,
            width: 4000,
            height: 3000,
        };
        let req = f
            .tap(
                0.2,
                0.2,
                &t(90, false, 1.0),
                cap(true, true, Some(area)),
                now,
            )
            .unwrap();
        assert!(req.window.is_some());
        assert_eq!(f.indicator().unwrap().status, Status::Scanning);
        assert!((f.indicator().unwrap().u - 0.2).abs() < 1e-9);
        let meta = |s| FrameMeta {
            af_state: Some(s),
            ..FrameMeta::default()
        };
        f.on_meta(&meta(AfState::Scanning), now);
        f.on_meta(&meta(AfState::Focused), now + Duration::from_millis(400));
        assert_eq!(f.indicator().unwrap().status, Status::Focused);
        assert!(f.is_active());
        assert_eq!(f.tick(now + Duration::from_secs(1)), Action::None);
        assert_eq!(
            f.tick(now + Duration::from_secs(6)),
            Action::ResumeContinuous
        );
        assert!(f.indicator().is_none());
    }

    #[test]
    fn without_windows_the_scan_is_central_and_shown_there() {
        let now = Instant::now();
        let mut f = FocusController::new();
        let req = f
            .tap(0.1, 0.9, &t(90, false, 2.0), cap(true, false, None), now)
            .unwrap();
        assert_eq!(req.window, None);
        let i = f.indicator().unwrap();
        assert!((i.u - 0.5).abs() < 1e-9 && (i.v - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_fixed_focus_lens_says_so_and_sends_nothing() {
        let now = Instant::now();
        let mut f = FocusController::new();
        assert!(f
            .tap(0.3, 0.3, &t(90, true, 1.0), cap(false, false, None), now)
            .is_none());
        assert_eq!(f.indicator().unwrap().status, Status::FixedFocus);
        assert!(!f.is_active());
        assert_eq!(f.tick(now + NOTICE), Action::None);
        assert!(f.indicator().is_none());
    }

    #[test]
    fn a_scan_that_reports_nothing_is_unavailable_not_endless() {
        let now = Instant::now();
        let mut f = FocusController::new();
        f.tap(0.5, 0.5, &t(0, false, 1.0), cap(true, false, None), now);
        f.on_meta(&FrameMeta::default(), now + Duration::from_millis(500));
        assert_eq!(f.tick(now + Duration::from_secs(1)), Action::None);
        assert_eq!(f.indicator().unwrap().status, Status::Scanning);
        f.tick(now + SCAN_TIMEOUT);
        assert_eq!(f.indicator().unwrap().status, Status::Unavailable);
        assert_eq!(
            f.tick(now + SCAN_TIMEOUT + NOTICE),
            Action::ResumeContinuous
        );
    }

    #[test]
    fn a_stale_focused_state_right_after_the_tap_is_ignored() {
        let now = Instant::now();
        let mut f = FocusController::new();
        f.tap(0.5, 0.5, &t(0, false, 1.0), cap(true, false, None), now);
        f.on_meta(
            &FrameMeta {
                af_state: Some(AfState::Focused),
                ..FrameMeta::default()
            },
            now + Duration::from_millis(30),
        );
        assert_eq!(f.indicator().unwrap().status, Status::Scanning);
    }
}
