//! The viewfinder: draws the latest frame, turned upright, mirrored for
//! the front camera, and cropped for the zoom, all in the render
//! transform (no pixel copies).
//!
//! The frame is fitted inside the widget (the whole frame shows, as the
//! picture will) and centred in the part above the controls when it fits
//! there.

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use gtk4 as gtk;

/// How to draw the current texture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Magnification about the centre (the zoom crop).
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

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Viewfinder {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub placement: Cell<Placement>,
        pub inset: Cell<f32>,
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
                self.inset.get(),
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
    inset: f32,
) {
    let (w, h) = size;
    snapshot.append_color(&gdk::RGBA::BLACK, &graphene::Rect::new(0.0, 0.0, w, h));
    let Some(tex) = texture else {
        return;
    };
    let (tw, th) = (tex.width() as f32, tex.height() as f32);
    let quarter = p.rotation % 180 == 90;
    let upright = if quarter { (th, tw) } else { (tw, th) };
    let rect = fit(upright, (w, h), inset);
    let s = rect.width() / upright.0;
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
    snapshot.pop();
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

    /// Height of the controls drawn over the bottom.
    pub fn set_inset(&self, inset: f32) {
        self.imp().inset.set(inset);
        self.queue_draw();
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
    fn portrait_4_3_frame_centres_above_the_controls() {
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

    #[test]
    fn empty_sizes_give_an_empty_rect() {
        assert_eq!(fit((0.0, 10.0), (480.0, 1040.0), 0.0).width(), 0.0);
    }
}
