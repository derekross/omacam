//! The viewfinder: draws the latest frame, turned upright, mirrored for
//! the front camera, and cropped for the zoom, all in the render
//! transform (no pixel copies).
//!
//! The frame is fitted inside the widget (the whole frame shows, as the
//! picture will) and centered in the part above the controls when it fits
//! there.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use gtk4 as gtk;

/// How to draw the current texture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Magnification about the center (the zoom crop).
    pub scale: f64,
    /// Clockwise degrees to turn the frame upright.
    pub rotation: u32,
    pub mirror: bool,
}

impl Default for Placement {
    fn default() -> Self {
        Placement {
            scale: 1.0,
            rotation: 0,
            mirror: false,
        }
    }
}

/// Where a frame of `frame` size (after rotation) lands in a widget of
/// `widget` size with `inset` pixels of controls at the bottom.
pub fn fit(frame: (f32, f32), widget: (f32, f32), inset: f32) -> graphene::Rect {
    let (fw, fh) = frame;
    let (w, h) = widget;
    if fw <= 0.0 || fh <= 0.0 || w <= 0.0 || h <= 0.0 {
        return graphene::Rect::new(0.0, 0.0, 0.0, 0.0);
    }
    let s = (w / fw).min(h / fh);
    let (dw, dh) = (fw * s, fh * s);
    let x = (w - dw) / 2.0;
    let above = (h - inset).max(0.0);
    // Too tall for the space above the controls: start at the top, and
    // let the controls cover its bottom.
    let y = if dh <= above { (above - dh) / 2.0 } else { 0.0 };
    graphene::Rect::new(x, y, dw, dh)
}

/// The part of an upright frame of `frame` size that shows at `scale`:
/// a centered rectangle (x, y, w, h) in frame pixels. Drawing clips the
/// scaled frame to its fitted rectangle, so this is what fills it.
#[cfg(test)]
fn visible_source(frame: (f32, f32), scale: f64) -> (f32, f32, f32, f32) {
    let s = scale.max(1.0) as f32;
    let (w, h) = (frame.0 / s, frame.1 / s);
    ((frame.0 - w) / 2.0, (frame.1 - h) / 2.0, w, h)
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Viewfinder {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub placement: Cell<Placement>,
        pub decor: RefCell<Decor>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Viewfinder {
        const NAME: &'static str = "OmacamViewfinder";
        type Type = super::Viewfinder;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("viewfinder");
        }
    }

    impl ObjectImpl for Viewfinder {}

    impl WidgetImpl for Viewfinder {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            draw(
                snapshot,
                (widget.width() as f32, widget.height() as f32),
                self.texture.borrow().as_ref(),
                self.placement.get(),
                &self.decor.borrow(),
            );
        }
    }
}

/// Draw `texture` placed as `p` in a `size` area with `inset` pixels of
/// controls at the bottom.
pub fn draw(
    snapshot: &gtk::Snapshot,
    size: (f32, f32),
    texture: Option<&gdk::Texture>,
    p: Placement,
    decor: &Decor,
) {
    let (w, h) = size;
    snapshot.append_color(&gdk::RGBA::BLACK, &graphene::Rect::new(0.0, 0.0, w, h));
    let Some(tex) = texture else {
        return;
    };
    let (tw, th) = (tex.width() as f32, tex.height() as f32);
    let quarter = p.rotation % 180 == 90;
    let upright = if quarter { (th, tw) } else { (tw, th) };
    let shown = crop_to_aspect(upright, decor.aspect);
    let rect = fit(shown, (w, h), 0.0);
    // Pixels of screen per pixel of frame.
    let s = rect.width() / shown.0;
    snapshot.push_clip(&rect);
    snapshot.save();
    snapshot.translate(&graphene::Point::new(
        rect.x() + rect.width() / 2.0,
        rect.y() + rect.height() / 2.0,
    ));
    let zoom = p.scale as f32;
    snapshot.scale(zoom, zoom);
    if p.mirror {
        snapshot.scale(-1.0, 1.0);
    }
    snapshot.rotate(p.rotation as f32);
    let (dw, dh) = (tw * s, th * s);
    snapshot.append_scaled_texture(
        tex,
        gsk::ScalingFilter::Linear,
        &graphene::Rect::new(-dw / 2.0, -dh / 2.0, dw, dh),
    );
    snapshot.restore();
    if decor.grid > 1 {
        let line = gdk::RGBA::new(1.0, 1.0, 1.0, 0.35);
        for i in 1..decor.grid {
            let f = i as f32 / decor.grid as f32;
            snapshot.append_color(
                &line,
                &graphene::Rect::new(rect.x() + rect.width() * f, rect.y(), 1.0, rect.height()),
            );
            snapshot.append_color(
                &line,
                &graphene::Rect::new(rect.x(), rect.y() + rect.height() * f, rect.width(), 1.0),
            );
        }
    }
    snapshot.pop();
    if let Some(bins) = decor.histogram.as_ref().filter(|b| !b.is_empty()) {
        let (hw, hh) = (120.0f32, 48.0f32);
        let (x0, y0) = (rect.x() + 10.0, rect.y() + 40.0);
        snapshot.append_color(
            &gdk::RGBA::new(0.0, 0.0, 0.0, 0.45),
            &graphene::Rect::new(x0, y0, hw, hh),
        );
        let bw = hw / bins.len() as f32;
        let bar = gdk::RGBA::new(1.0, 1.0, 1.0, 0.8);
        for (i, b) in bins.iter().enumerate() {
            let h = (b.clamp(0.0, 1.0) * (hh - 4.0)).max(0.5);
            snapshot.append_color(
                &bar,
                &graphene::Rect::new(x0 + i as f32 * bw, y0 + hh - 2.0 - h, bw.max(1.0), h),
            );
        }
    }
    if let Some(ring) = decor.ring {
        let (ur, vr) = picture_to_shown(ring.u, ring.v, upright, shown);
        let (cx, cy) = (
            rect.x() + rect.width() * ur as f32,
            rect.y() + rect.height() * vr as f32,
        );
        let r = 34.0;
        let bounds = graphene::Rect::new(cx - r, cy - r, 2.0 * r, 2.0 * r);
        let rounded = gsk::RoundedRect::from_rect(bounds, r);
        snapshot.append_border(&rounded, &[2.5; 4], &[ring.color; 4]);
    }
}

/// What the viewfinder draws over the picture.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Decor {
    /// Width over height of the photo, upright; None shows the whole frame.
    pub aspect: Option<f64>,
    /// Grid lines: 0 for none, 3 for thirds, 4 for quarters.
    pub grid: u32,
    pub ring: Option<Ring>,
    /// A luma histogram (bins, each 0..1) to draw over the top left.
    pub histogram: Option<Vec<f32>>,
}

/// A focus ring at a point of the whole upright picture (0..1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ring {
    pub u: f64,
    pub v: f64,
    pub color: gdk::RGBA,
}

/// The largest centered part of an upright frame with the given aspect
/// (width over height); the whole frame for None.
pub fn crop_to_aspect(upright: (f32, f32), aspect: Option<f64>) -> (f32, f32) {
    let Some(a) = aspect.filter(|a| a.is_finite() && *a > 0.0) else {
        return upright;
    };
    let (w, h) = upright;
    let a = a as f32;
    if w / h > a {
        (h * a, h)
    } else {
        (w, w / a)
    }
}

/// A point of the whole upright picture to the shown (aspect-cropped) one.
fn picture_to_shown(u: f64, v: f64, upright: (f32, f32), shown: (f32, f32)) -> (f64, f64) {
    (
        0.5 + (u - 0.5) * (upright.0 / shown.0) as f64,
        0.5 + (v - 0.5) * (upright.1 / shown.1) as f64,
    )
}

/// A widget point to the whole upright picture (0..1), if it's on the
/// shown picture.
pub fn widget_to_picture(
    x: f32,
    y: f32,
    widget: (f32, f32),
    upright: (f32, f32),
    aspect: Option<f64>,
) -> Option<(f64, f64)> {
    let shown = crop_to_aspect(upright, aspect);
    let rect = fit(shown, widget, 0.0);
    if rect.width() <= 0.0
        || x < rect.x()
        || y < rect.y()
        || x > rect.x() + rect.width()
        || y > rect.y() + rect.height()
    {
        return None;
    }
    let (ur, vr) = (
        ((x - rect.x()) / rect.width()) as f64,
        ((y - rect.y()) / rect.height()) as f64,
    );
    Some((
        0.5 + (ur - 0.5) * (shown.0 / upright.0) as f64,
        0.5 + (vr - 0.5) * (shown.1 / upright.1) as f64,
    ))
}

glib::wrapper! {
    pub struct Viewfinder(ObjectSubclass<imp::Viewfinder>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Viewfinder {
    fn default() -> Self {
        Self::new()
    }
}

impl Viewfinder {
    pub fn new() -> Viewfinder {
        glib::Object::builder()
            .property("hexpand", true)
            .property("vexpand", true)
            .build()
    }

    /// Show a new frame.
    pub fn set_frame(&self, texture: gdk::Texture, placement: Placement) {
        self.imp().texture.replace(Some(texture));
        self.imp().placement.set(placement);
        self.queue_draw();
    }

    /// Change how the current frame is drawn (zoom on a frozen frame).
    pub fn set_placement(&self, placement: Placement) {
        if self.imp().placement.replace(placement) != placement {
            self.queue_draw();
        }
    }

    pub fn texture(&self) -> Option<gdk::Texture> {
        self.imp().texture.borrow().clone()
    }

    pub fn placement(&self) -> Placement {
        self.imp().placement.get()
    }

    pub fn decor(&self) -> Decor {
        self.imp().decor.borrow().clone()
    }

    pub fn set_decor(&self, decor: Decor) {
        if *self.imp().decor.borrow() != decor {
            self.imp().decor.replace(decor);
            self.queue_draw();
        }
    }

    /// Where a point of the whole upright picture (0..1) is drawn, in
    /// widget coordinates.
    pub fn widget_point(&self, u: f64, v: f64) -> Option<(f32, f32)> {
        let tex = self.texture()?;
        let p = self.placement();
        let (tw, th) = (tex.width() as f32, tex.height() as f32);
        let upright = if p.rotation % 180 == 90 {
            (th, tw)
        } else {
            (tw, th)
        };
        let shown = crop_to_aspect(upright, self.decor().aspect);
        let rect = fit(shown, (self.width() as f32, self.height() as f32), 0.0);
        let (su, sv) = picture_to_shown(u, v, upright, shown);
        Some((
            rect.x() + rect.width() * su as f32,
            rect.y() + rect.height() * sv as f32,
        ))
    }

    /// A widget point to the whole upright picture (0..1), if on it.
    pub fn picture_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let tex = self.texture()?;
        let p = self.placement();
        let (tw, th) = (tex.width() as f32, tex.height() as f32);
        let upright = if p.rotation % 180 == 90 {
            (th, tw)
        } else {
            (tw, th)
        };
        widget_to_picture(
            x as f32,
            y as f32,
            (self.width() as f32, self.height() as f32),
            upright,
            self.decor().aspect,
        )
    }

    pub fn clear(&self) {
        self.imp().texture.replace(None);
        self.queue_draw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portrait_16_9_frame_fills_the_width_and_tops_out() {
        // The main camera's 2016x1136 preview turned upright, on a 480x1040
        // phone with 220 px of controls.
        let r = fit((1136.0, 2016.0), (480.0, 1040.0), 220.0);
        assert!((r.width() - 480.0).abs() < 0.01);
        assert!((r.height() - 851.83).abs() < 0.1);
        assert_eq!(r.y(), 0.0, "taller than the space above the controls");
    }

    #[test]
    fn portrait_4_3_frame_centers_above_the_controls() {
        let r = fit((1512.0, 2016.0), (480.0, 1040.0), 220.0);
        assert!((r.height() - 640.0).abs() < 0.01);
        assert!((r.y() - 90.0).abs() < 0.01);
    }

    #[test]
    fn landscape_window_letterboxes_sideways() {
        let r = fit((1136.0, 2016.0), (1920.0, 1080.0), 0.0);
        assert!((r.height() - 1080.0).abs() < 0.01);
        assert!(r.x() > 0.0);
    }

    /// Every zoom from 0.45x to 10x on the Pixel 7 Pro, in the window the
    /// phone gives the app (456 wide, about 730 above the control bar):
    /// the picture fills the viewfinder's width or height, and what shows
    /// is a rectangle inside the frame.
    #[test]
    fn every_zoom_fills_the_viewfinder_with_a_crop_inside_the_frame() {
        use omacam_core::controller::Options;
        use omacam_core::zoom::ZoomSource;
        use omacam_core::{Controller, LensTable};
        let cams = omacam_fake::cheetah_cameras();
        let spec = omacam_core::lens::TableSpec::cheetah();
        let table: LensTable = spec.resolve(&cams);
        let mut c = Controller::new(&spec, &cams, Options::default());
        let now = std::time::Instant::now();
        let widget = (456.0f32, 730.0f32);
        let mut z = 0.45;
        while z <= 10.0 {
            c.set_zoom(z, ZoomSource::Preset, now);
            for lens in table.lenses().iter().filter(|l| !l.mirror) {
                let t = c.transform(&lens.id).unwrap();
                let (pw, ph) = (
                    lens.preview.unwrap().width as f32,
                    lens.preview.unwrap().height as f32,
                );
                let upright = if t.rotation % 180 == 90 {
                    (ph, pw)
                } else {
                    (pw, ph)
                };
                let r = fit(upright, widget, 0.0);
                let fills =
                    (r.width() - widget.0).abs() < 0.01 || (r.height() - widget.1).abs() < 0.01;
                assert!(fills, "{} at {z}: {:?}", lens.name, r);
                let (x, y, w, h) = visible_source(upright, t.scale);
                assert!(
                    x >= 0.0 && y >= 0.0 && x + w <= upright.0 + 0.01 && y + h <= upright.1 + 0.01
                );
                assert!(w <= upright.0 && h <= upright.1 && w > 0.0 && h > 0.0);
                // The visible part keeps the frame's aspect, so it fills
                // the fitted rectangle exactly.
                assert!((w / h - upright.0 / upright.1).abs() < 1e-3);
            }
            z += 0.05;
        }
    }

    #[test]
    fn aspect_crops_keep_the_center_and_the_requested_shape() {
        // Main camera upright: 1136x2008.
        let up = (1136.0, 2008.0);
        assert_eq!(crop_to_aspect(up, None), up);
        let (w, h) = crop_to_aspect(up, Some(3.0 / 4.0));
        assert!((w / h - 0.75).abs() < 1e-4 && w <= 1136.0 && h <= 2008.0);
        let (w, h) = crop_to_aspect(up, Some(1.0));
        assert_eq!((w, h), (1136.0, 1136.0));
    }

    #[test]
    fn taps_on_the_shown_picture_map_to_the_whole_picture() {
        let up = (1136.0, 2008.0);
        let widget = (456.0, 730.0);
        // The center is the center.
        let shown = crop_to_aspect(up, Some(1.0));
        let r = fit(shown, widget, 0.0);
        let c = widget_to_picture(
            r.x() + r.width() / 2.0,
            r.y() + r.height() / 2.0,
            widget,
            up,
            Some(1.0),
        )
        .unwrap();
        assert!((c.0 - 0.5).abs() < 1e-6 && (c.1 - 0.5).abs() < 1e-6);
        // The top edge of a square crop is well inside the whole picture.
        let top = widget_to_picture(r.x() + 1.0, r.y(), widget, up, Some(1.0)).unwrap();
        assert!(top.1 > 0.2 && top.1 < 0.25, "{top:?}");
        // Off the picture: nothing.
        assert!(
            widget_to_picture(1.0, r.y() - 5.0, widget, up, Some(1.0)).is_none() || r.y() < 5.0
        );
        // And back.
        let (u, v) = picture_to_shown(top.0, top.1, up, shown);
        assert!((v - 0.0).abs() < 1e-4 && u > 0.0);
    }

    #[test]
    fn empty_sizes_give_an_empty_rect() {
        assert_eq!(fit((0.0, 10.0), (480.0, 1040.0), 0.0).width(), 0.0);
    }
}
