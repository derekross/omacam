//! Framing: which part of a lens's frame the picture shows at a zoom, so
//! that every lens frames the same scene the same way.
//!
//! Three coordinate spaces meet here:
//!
//! - the **frame**: the pixels a camera delivers, in its sensor's axes;
//! - a lens's **picture**: its whole field of view (the still mode's),
//!   turned upright and, for the selfie camera, mirrored. A preview mode
//!   may show only part of it ([`crate::lens::Lens::coverage`]): the main
//!   camera's 16:9 mode crops the height of its 4:3 sensor;
//! - the **view**: what the screen (and the photo, and the video) shows:
//!   a window of the picture `1 / scale` of its size about a center.
//!
//! Zoom is measured on the picture, against the main camera's, so a
//! frame from any lens at the same zoom shows the same part of the scene,
//! whatever its mode crops. The view's center follows the lenses' optical
//! axes ([`view_center`]): it slides from one lens's axis to the next
//! lens's as the zoom goes from the one's factor to the other's, so at a
//! switch the new lens's view is centered where the old one's was.

use crate::backend::Size;

/// How to draw a frame of one lens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// Magnification of the picture about [`Transform::center`] (the zoom
    /// crop), never below 1.
    pub scale: f64,
    /// Clockwise degrees that turn the frame upright.
    pub rotation: u32,
    /// Mirror (left-right) after turning upright: the selfie preview.
    pub mirror: bool,
    /// The view's center on the picture (upright, mirrored as shown,
    /// 0..1).
    pub center: (f64, f64),
    /// The share of the picture the frame covers, upright (width,
    /// height): below 1 where the frame's mode crops the field of view.
    pub coverage: (f64, f64),
}

impl Default for Transform {
    fn default() -> Self {
        Transform {
            scale: 1.0,
            rotation: 0,
            mirror: false,
            center: (0.5, 0.5),
            coverage: (1.0, 1.0),
        }
    }
}

/// A frame-axes pair (width, height) turned upright by `rotation`.
pub fn upright_pair(pair: (f64, f64), rotation: u32) -> (f64, f64) {
    if rotation % 180 == 90 {
        (pair.1, pair.0)
    } else {
        pair
    }
}

/// The view's center on the main camera's upright picture at `zoom`, for
/// the lenses of one side as (zoom factor, axis), ascending: from each
/// lens's axis at its own factor to the next lens's axis at the next
/// factor, along the logarithm of the zoom. Past the longest lens it
/// stays on that lens's axis.
pub fn view_center(zoom: f64, lenses: &[(f64, (f64, f64))]) -> (f64, f64) {
    let Some(&(first_zoom, first_axis)) = lenses.first() else {
        return (0.5, 0.5);
    };
    if !zoom.is_finite() || zoom <= first_zoom {
        return first_axis;
    }
    for w in lenses.windows(2) {
        let ((za, aa), (zb, ab)) = (w[0], w[1]);
        if zoom < zb && zb > za {
            let t = ((zoom / za).ln() / (zb / za).ln()).clamp(0.0, 1.0);
            return (aa.0 + t * (ab.0 - aa.0), aa.1 + t * (ab.1 - aa.1));
        }
    }
    lenses[lenses.len() - 1].1
}

/// The view's center on one lens's own picture: `main_center` (on the
/// main camera's picture) moved into the lens's picture, which spans
/// `1 / lens_zoom` of the main one's about `lens_axis`; then held where
/// the view, `1 / scale` of the picture, stays inside it.
pub fn lens_center(
    main_center: (f64, f64),
    lens_zoom: f64,
    lens_axis: (f64, f64),
    scale: f64,
) -> (f64, f64) {
    let half = 0.5 / scale.max(1.0);
    let one = |c: f64, a: f64| (0.5 + (c - a) * lens_zoom).clamp(half, 1.0 - half);
    (
        one(main_center.0, lens_axis.0),
        one(main_center.1, lens_axis.1),
    )
}

/// A point on the view (0..1) to the same point on the upright,
/// mirrored-as-shown frame (0..1; outside it where the frame's mode
/// doesn't cover the picture).
pub fn view_to_upright(u: f64, v: f64, t: &Transform) -> (f64, f64) {
    let s = t.scale.max(1.0);
    let p = (t.center.0 + (u - 0.5) / s, t.center.1 + (v - 0.5) / s);
    (
        0.5 + (p.0 - 0.5) / t.coverage.0,
        0.5 + (p.1 - 0.5) / t.coverage.1,
    )
}

/// The inverse of [`view_to_upright`].
pub fn upright_to_view(x: f64, y: f64, t: &Transform) -> (f64, f64) {
    let s = t.scale.max(1.0);
    let p = (
        0.5 + (x - 0.5) * t.coverage.0,
        0.5 + (y - 0.5) * t.coverage.1,
    );
    (0.5 + (p.0 - t.center.0) * s, 0.5 + (p.1 - t.center.1) * s)
}

/// A point of the upright, mirrored-as-shown picture (0..1) on the
/// frame's own axes (0..1).
pub fn upright_to_frame_point(p: (f64, f64), rotation: u32, mirror: bool) -> (f64, f64) {
    let u = if mirror { 1.0 - p.0 } else { p.0 };
    let v = p.1;
    match rotation % 360 {
        90 => (v, 1.0 - u),
        180 => (1.0 - u, 1.0 - v),
        270 => (1.0 - v, u),
        _ => (u, v),
    }
}

/// A move on the upright picture (shares of its sides) as a move on the
/// frame's own axes.
pub fn upright_to_frame_vector(d: (f64, f64), rotation: u32, mirror: bool) -> (f64, f64) {
    let a = upright_to_frame_point((0.5, 0.5), rotation, mirror);
    let b = upright_to_frame_point((0.5 + d.0, 0.5 + d.1), rotation, mirror);
    (b.0 - a.0, b.1 - a.1)
}

/// A rectangle in pixels, (x, y, width, height), fractional.
pub type RectF = (f64, f64, f64, f64);

/// The part of a frame of `size` that the view shows, in frame pixels:
/// the view's window of the picture (shaped `aspect`, width over height
/// upright, or the picture's own shape for None), inside the frame. Where
/// the frame doesn't cover the whole window (the main camera's 16:9 mode
/// in a 4:3 view, near 1x) the rectangle is the part it does cover.
pub fn frame_window(t: &Transform, size: Size, aspect: Option<f64>) -> RectF {
    let (fw, fh) = (size.width as f64, size.height as f64);
    if fw <= 0.0 || fh <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    let quarter = t.rotation % 180 == 90;
    // The upright frame, and the picture it covers part of.
    let (uw, uh) = if quarter { (fh, fw) } else { (fw, fh) };
    let (pw, ph) = (uw / t.coverage.0, uh / t.coverage.1);
    let (aw, ah) = match aspect.filter(|a| a.is_finite() && *a > 0.0) {
        Some(a) if pw / ph > a => (ph * a, ph),
        Some(a) => (pw, pw / a),
        None => (pw, ph),
    };
    let s = t.scale.max(1.0);
    let (w, h) = (aw / s, ah / s);
    // The window on the upright frame, then inside it.
    let x0 = t.center.0 * pw - w / 2.0 - (pw - uw) / 2.0;
    let y0 = t.center.1 * ph - h / 2.0 - (ph - uh) / 2.0;
    let (x1, y1) = ((x0 + w).min(uw), (y0 + h).min(uh));
    let (x0, y0) = (x0.max(0.0), y0.max(0.0));
    let (mut x, y, w, h) = (x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0));
    if t.mirror {
        x = uw - x - w;
    }
    // Back from upright to the frame: the inverse of a clockwise turn.
    match t.rotation % 360 {
        90 => (y, uw - x - w, h, w),
        180 => (uw - x - w, uh - y - h, w, h),
        270 => (uh - y - h, x, h, w),
        _ => (x, y, w, h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f64, f64), b: (f64, f64)) -> bool {
        (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9
    }

    const CHEETAH: [(f64, (f64, f64)); 3] =
        [(0.45, (0.47, 0.5)), (1.0, (0.5, 0.5)), (4.78, (0.56, 0.48))];

    #[test]
    fn the_center_slides_from_axis_to_axis_with_the_zoom() {
        assert_eq!(view_center(0.45, &CHEETAH), (0.47, 0.5));
        assert_eq!(view_center(1.0, &CHEETAH), (0.5, 0.5));
        assert_eq!(view_center(4.78, &CHEETAH), (0.56, 0.48));
        assert_eq!(view_center(15.0, &CHEETAH), (0.56, 0.48));
        // Halfway in log zoom between 1x and 4.78x: halfway between axes.
        let mid = view_center((4.78f64).sqrt(), &CHEETAH);
        assert!(close(mid, (0.53, 0.49)), "{mid:?}");
        // Continuous just under a lens's factor.
        let below = view_center(4.7799, &CHEETAH);
        assert!((below.0 - 0.56).abs() < 1e-4);
        assert_eq!(view_center(2.0, &[]), (0.5, 0.5));
    }

    #[test]
    fn at_a_switch_both_lenses_center_on_the_same_point() {
        // Just under 4.78x the main crops about the tele's axis; the tele
        // at 4.78x is at its own center.
        let c = view_center(4.78, &CHEETAH);
        let main = lens_center(c, 1.0, (0.5, 0.5), 4.78);
        assert!(close(main, (0.56, 0.48)), "{main:?}");
        let tele = lens_center(c, 4.78, (0.56, 0.48), 1.0);
        assert!(close(tele, (0.5, 0.5)));
        // The ultrawide at 0.99x looks at the main's axis: 0.03 right of
        // its own, which in its wider picture is 0.03 x 0.45.
        let uw = lens_center(
            view_center(0.9999, &CHEETAH),
            0.45,
            (0.47, 0.5),
            0.9999 / 0.45,
        );
        assert!((uw.0 - (0.5 + 0.03 * 0.45)).abs() < 1e-3, "{uw:?}");
    }

    #[test]
    fn the_view_stays_inside_the_picture() {
        // A far-off axis at 1x can't move the center: the view is the
        // whole picture.
        assert_eq!(lens_center((0.9, 0.1), 1.0, (0.5, 0.5), 1.0), (0.5, 0.5));
        // At 2x the center may go a quarter either way, no further.
        assert_eq!(lens_center((0.9, 0.1), 1.0, (0.5, 0.5), 2.0), (0.75, 0.25));
    }

    #[test]
    fn view_points_map_to_the_frame_and_back() {
        let t = Transform {
            scale: 2.0,
            center: (0.6, 0.45),
            coverage: (0.74, 1.0),
            ..Transform::default()
        };
        let (x, y) = view_to_upright(0.5, 0.5, &t);
        assert!(close((x, y), (0.5 + 0.1 / 0.74, 0.45)));
        let back = upright_to_view(x, y, &t);
        assert!(close(back, (0.5, 0.5)));
        // At 1x a 74 %-wide frame leaves the view's edges uncovered.
        let one = Transform {
            coverage: (0.74, 1.0),
            ..Transform::default()
        };
        assert!(view_to_upright(0.0, 0.5, &one).0 < 0.0);
    }

    #[test]
    fn frame_windows_follow_zoom_center_aspect_and_coverage() {
        let size = Size::new(4000, 3000);
        // The whole frame at 1x.
        let whole = frame_window(&Transform::default(), size, None);
        assert_eq!(whole, (0.0, 0.0, 4000.0, 3000.0));
        // 2x, centered: the middle quarter.
        let t2 = Transform {
            scale: 2.0,
            ..Transform::default()
        };
        assert_eq!(
            frame_window(&t2, size, None),
            (1000.0, 750.0, 2000.0, 1500.0)
        );
        // Turned 90 clockwise, a 3:4 upright window of a 4:3 frame is all
        // of it; a square one is the middle of the frame's width.
        let r90 = Transform {
            rotation: 90,
            ..Transform::default()
        };
        assert_eq!(
            frame_window(&r90, size, Some(0.75)),
            (0.0, 0.0, 4000.0, 3000.0)
        );
        let sq = frame_window(&r90, size, Some(1.0));
        assert_eq!(sq, (500.0, 0.0, 3000.0, 3000.0));
        // A center right of the middle, upright, is toward the frame's
        // top for a frame turned 90 clockwise (its top edge goes right).
        let off = Transform {
            rotation: 90,
            scale: 2.0,
            center: (0.6, 0.5),
            ..Transform::default()
        };
        let (_, y, _, h) = frame_window(&off, size, None);
        let mid = y + h / 2.0;
        assert!((mid - 0.4 * 3000.0).abs() < 1e-6, "{mid}");
        // The main camera's 16:9 preview at 1x in a 3:4 view covers 74 %
        // of the width: the window is the whole frame.
        let main = Transform {
            rotation: 270,
            coverage: (2272.0 / 3072.0, 4032.0 / 4080.0),
            ..Transform::default()
        };
        let w = frame_window(&main, Size::new(2016, 1136), Some(0.75));
        assert!(w.0.abs() < 1e-6 && w.1.abs() < 1e-6, "{w:?}");
        assert!((w.2 - 2016.0).abs() < 1e-6 && (w.3 - 1136.0).abs() < 1e-6);
        // At 2x the window is inside the strip and 3:4 upright.
        let main2 = Transform { scale: 2.0, ..main };
        let (x, y, w, h) = frame_window(&main2, Size::new(2016, 1136), Some(0.75));
        assert!(x > 0.0 && y > 0.0 && x + w <= 2016.0 && y + h <= 1136.0);
        assert!((h / w - 0.75).abs() < 1e-6, "{w}x{h}");
    }

    #[test]
    fn upright_points_and_moves_on_the_frame() {
        // Turned 90 clockwise, the upright picture's right is the frame's
        // top.
        assert!(close(
            upright_to_frame_point((1.0, 0.5), 90, false),
            (0.5, 0.0)
        ));
        assert!(close(
            upright_to_frame_point((1.0, 0.5), 270, false),
            (0.5, 1.0)
        ));
        assert!(close(
            upright_to_frame_point((0.2, 0.3), 0, true),
            (0.8, 0.3)
        ));
        assert!(close(
            upright_to_frame_vector((0.1, 0.0), 90, false),
            (0.0, -0.1)
        ));
        assert!(close(
            upright_to_frame_vector((0.0, 0.1), 270, false),
            (-0.1, 0.0)
        ));
        assert!(close(
            upright_to_frame_vector((0.1, 0.2), 180, false),
            (-0.1, -0.2)
        ));
    }

    #[test]
    fn mirrored_windows_flip_left_and_right() {
        let t = Transform {
            scale: 2.0,
            mirror: true,
            center: (0.3, 0.5),
            ..Transform::default()
        };
        let (x, _, w, _) = frame_window(&t, Size::new(1000, 500), None);
        // Shown at 0.3 from the left, so 0.7 from the left of the frame.
        assert!((x + w / 2.0 - 700.0).abs() < 1e-6);
    }
}
