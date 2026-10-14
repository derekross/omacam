//! The window: a full-screen viewfinder with the controls over its bottom.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use gtk4 as gtk;
use libadwaita as adw;

use omacam_core::backend::{Backend, Command, Event, Frame, FrameData, PixelLayout};
use omacam_core::controller::{Options, Output, Picture};
use omacam_core::zoom::ZoomSource;
use omacam_core::{capture, Controller};

use crate::photo;
use crate::viewfinder::{Placement, Viewfinder};
use crate::{CaptureMode, Cli};

const CSS: &str = r#"
window.omacam, window.omacam viewfinder { background: black; }
.omacam-controls { padding: 10px 24px 18px 24px; background: black; }
.zoom-pills { background: rgba(0,0,0,0.45); border-radius: 999px; padding: 4px; }
.zoom-pill {
  min-width: 44px; min-height: 44px; padding: 0; border-radius: 999px;
  background: transparent; color: white; font-weight: bold; font-size: 13px;
  box-shadow: none;
}
.zoom-pill.active {
  background: rgba(255,255,255,0.18);
  color: var(--accent-bg-color);
  font-size: 15px;
}
.shutter {
  min-width: 72px; min-height: 72px; padding: 0; margin: 0; border-radius: 999px;
  background: white;
  box-shadow: 0 0 0 5px rgba(255,255,255,0.35);
}
.shutter:active { background: #d8d8d8; }
.shutter.busy { background: var(--accent-bg-color); }
.round-button {
  min-width: 56px; min-height: 56px; padding: 0; margin: 0; border-radius: 999px;
  background: rgba(255,255,255,0.16); color: white;
}

.zoom-indicator, .status {
  color: white; font-weight: bold; background: rgba(0,0,0,0.5);
  border-radius: 999px; padding: 6px 14px;
}
.zoom-indicator { font-size: 18px; }
.lens-label { color: rgba(255,255,255,0.85); font-size: 12px; }
.debug {
  font-family: monospace; font-size: 11px; color: #9f9;
  background: rgba(0,0,0,0.55); padding: 4px 6px; border-radius: 6px;
}
scale.zoom-slider trough { min-height: 4px; }
scale.zoom-slider { color: white; }
"#;

/// How long the slider and the zoom readout stay after the last change.
const HIDE_AFTER: Duration = Duration::from_millis(1500);
/// How long the window may lose focus before the cameras are let go.
const UNFOCUSED_RELEASE: Duration = Duration::from_secs(10);

enum Msg {
    Event(Event),
    /// The newest frame is in the frame slot.
    FrameReady,
    Saved(Result<(PathBuf, photo::Bgrx), String>),
}

struct Fps {
    window_start: Instant,
    frames: u32,
    value: f64,
}

impl Fps {
    fn tick(&mut self) {
        self.frames += 1;
        let dt = self.window_start.elapsed();
        if dt >= Duration::from_secs(1) {
            self.value = self.frames as f64 / dt.as_secs_f64();
            self.frames = 0;
            self.window_start = Instant::now();
        }
    }
}

struct App {
    cli: Cli,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    viewfinder: Viewfinder,
    pills: gtk::Box,
    pill_buttons: RefCell<Vec<(f64, gtk::Button)>>,
    slider: gtk::Scale,
    slider_revealer: gtk::Revealer,
    zoom_indicator: gtk::Label,
    lens_label: gtk::Label,
    status: gtk::Label,
    shutter: gtk::Button,
    flip: gtk::Button,
    thumb: gtk::Button,
    thumb_picture: gtk::Image,
    debug: gtk::Label,

    backend: RefCell<Option<Box<dyn Backend>>>,
    controller: RefCell<Option<Controller>>,
    frame_slot: Arc<Mutex<Option<Frame>>>,
    tx: async_channel::Sender<Msg>,

    shown_camera: RefCell<Option<String>>,
    shown_size: Cell<(u32, u32)>,
    /// Mean brightness (0-255) and the AE values of the last shown frame,
    /// for the state log.
    shown_luma: Cell<Option<f64>>,
    shown_ae: Cell<(Option<i32>, Option<f32>)>,
    fps: RefCell<Fps>,
    pinch_base: Cell<f64>,
    drag_base: Cell<f64>,
    drag_claimed: Cell<bool>,
    setting_slider: Cell<bool>,
    hide_source: RefCell<Option<glib::SourceId>>,
    unfocus_source: RefCell<Option<glib::SourceId>>,
    suspended: Cell<bool>,
    last_photo: RefCell<Option<PathBuf>>,
    last_error: RefCell<Option<(String, Instant)>>,
    built_presets: RefCell<Vec<f64>>,
}

pub fn run(cli: Cli) -> glib::ExitCode {
    let app = adw::Application::builder()
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| build(app, cli.clone()));
    // Our options are parsed already; GTK gets none.
    app.run_with_args::<&str>(&[])
}

fn round_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::from_icon_name(icon);
    b.add_css_class("round-button");
    b.set_tooltip_text(Some(tooltip));
    b.set_valign(gtk::Align::Center);
    b
}

fn build(gtk_app: &adw::Application, cli: Cli) {
    let display = gdk::Display::default().expect("a display");
    let monitor = crate::theme::follow_omarchy_theme();
    let css = gtk::CssProvider::new();
    css.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &display,
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );

    let window = adw::ApplicationWindow::builder()
        .application(gtk_app)
        .title("Camera")
        .default_width(480)
        .default_height(1040)
        .build();
    window.add_css_class("omacam");
    // Keep the theme watcher alive with the window.
    if let Some(m) = monitor {
        unsafe_keep(&window, m);
    }

    let viewfinder = Viewfinder::new();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&viewfinder));

    // Top: lens name and the debug readout.
    let lens_label = gtk::Label::new(None);
    lens_label.add_css_class("lens-label");
    let debug = gtk::Label::new(None);
    debug.add_css_class("debug");
    debug.set_xalign(0.0);
    debug.set_visible(cli.debug);
    let top = gtk::Box::new(gtk::Orientation::Vertical, 4);
    top.set_valign(gtk::Align::Start);
    top.set_halign(gtk::Align::Start);
    top.set_margin_top(12);
    top.set_margin_start(12);
    top.append(&debug);
    overlay.add_overlay(&top);

    // Centre: zoom readout while zooming, and status text.
    let zoom_indicator = gtk::Label::new(None);
    zoom_indicator.add_css_class("zoom-indicator");
    zoom_indicator.set_halign(gtk::Align::Center);
    zoom_indicator.set_valign(gtk::Align::Center);
    zoom_indicator.set_visible(false);
    zoom_indicator.set_can_target(false);
    overlay.add_overlay(&zoom_indicator);
    let status = gtk::Label::new(Some("Starting the camera…"));
    status.add_css_class("status");
    status.set_halign(gtk::Align::Center);
    status.set_valign(gtk::Align::Center);
    status.set_can_target(false);
    overlay.add_overlay(&status);

    // Bottom: slider, zoom pills, then thumbnail / shutter / flip.
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
    slider.set_draw_value(false);
    slider.add_css_class("zoom-slider");
    slider.set_size_request(320, -1);
    let slider_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .child(&slider)
        .halign(gtk::Align::Center)
        .build();
    let pills = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    pills.add_css_class("zoom-pills");
    pills.set_halign(gtk::Align::Center);

    let thumb_picture = gtk::Image::from_icon_name("image-x-generic-symbolic");
    thumb_picture.set_pixel_size(22);
    let thumb = gtk::Button::new();
    thumb.set_child(Some(&thumb_picture));
    thumb.add_css_class("round-button");
    thumb.add_css_class("thumb");
    thumb.set_overflow(gtk::Overflow::Hidden);
    thumb.set_tooltip_text(Some("Last photo"));
    thumb.set_sensitive(false);
    let shutter = gtk::Button::new();
    shutter.add_css_class("shutter");
    shutter.set_tooltip_text(Some("Take a photo"));
    let flip = round_button("camera-switch-symbolic", "Switch camera");
    // Fixed-size circles: never stretched by the row, whatever the window.
    for (b, size) in [(&thumb, 56), (&shutter, 72), (&flip, 56)] {
        b.set_size_request(size, size);
        b.set_halign(gtk::Align::Center);
        b.set_valign(gtk::Align::Center);
        b.set_hexpand(false);
        b.set_vexpand(false);
    }
    let row = gtk::CenterBox::new();
    row.set_start_widget(Some(&thumb));
    row.set_center_widget(Some(&shutter));
    row.set_end_widget(Some(&flip));
    row.set_vexpand(false);

    // Over the bottom of the picture: the lens name and the slider, which
    // come and go without moving anything.
    let picture_bottom = gtk::Box::new(gtk::Orientation::Vertical, 6);
    picture_bottom.set_valign(gtk::Align::End);
    picture_bottom.set_halign(gtk::Align::Center);
    picture_bottom.set_margin_bottom(8);
    lens_label.set_halign(gtk::Align::Center);
    picture_bottom.append(&slider_revealer);
    picture_bottom.append(&lens_label);
    overlay.add_overlay(&picture_bottom);

    // Below the picture: the zoom buttons and the shutter row, in a bar of
    // their own, so the picture is letterboxed above them.
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 12);
    controls.add_css_class("omacam-controls");
    controls.set_vexpand(false);
    controls.append(&pills);
    controls.append(&row);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    overlay.set_vexpand(true);
    page.append(&overlay);
    page.append(&controls);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));
    window.set_content(Some(&toasts));

    let (tx, rx) = async_channel::unbounded::<Msg>();
    let app = Rc::new(App {
        cli,
        window: window.clone(),
        toasts,
        viewfinder,
        pills,
        pill_buttons: RefCell::new(Vec::new()),
        slider,
        slider_revealer,
        zoom_indicator,
        lens_label,
        status,
        shutter,
        flip,
        thumb,
        thumb_picture,
        debug,
        backend: RefCell::new(None),
        controller: RefCell::new(None),
        frame_slot: Arc::new(Mutex::new(None)),
        tx,
        shown_camera: RefCell::new(None),
        shown_size: Cell::new((0, 0)),
        shown_luma: Cell::new(None),
        shown_ae: Cell::new((None, None)),
        fps: RefCell::new(Fps {
            window_start: Instant::now(),
            frames: 0,
            value: 0.0,
        }),
        pinch_base: Cell::new(1.0),
        drag_base: Cell::new(0.0),
        drag_claimed: Cell::new(false),
        setting_slider: Cell::new(false),
        hide_source: RefCell::new(None),
        unfocus_source: RefCell::new(None),
        suspended: Cell::new(false),
        last_photo: RefCell::new(None),
        last_error: RefCell::new(None),
        built_presets: RefCell::new(Vec::new()),
    });

    app.start_backend();
    app.connect_input();

    let a = app.clone();
    glib::spawn_future_local(async move {
        while let Ok(msg) = rx.recv().await {
            a.handle(msg);
        }
    });
    let a = Rc::downgrade(&app);
    glib::timeout_add_local(Duration::from_millis(30), move || {
        let Some(a) = a.upgrade() else {
            return glib::ControlFlow::Break;
        };
        a.tick();
        glib::ControlFlow::Continue
    });

    let a = app.clone();
    window.connect_close_request(move |_| {
        a.shutdown();
        glib::Propagation::Proceed
    });
    window.present();

    if let Some(text) = app.cli.script.clone() {
        let steps = crate::script::parse(&text).expect("checked in main");
        glib::spawn_future_local(run_script(app.clone(), steps));
    }
}

async fn run_script(app: Rc<App>, steps: Vec<crate::script::Step>) {
    use crate::script::Step;
    for step in steps {
        tracing::info!(?step, "script");
        match step {
            Step::Wait(d) => glib::timeout_future(d).await,
            Step::Zoom(z) => app.zoom_to(z, ZoomSource::Preset),
            Step::Pinch(z) => {
                app.zoom_to(z, ZoomSource::Gesture);
                app.zoom_released();
            }
            Step::Flip => app.flip.emit_clicked(),
            Step::Shutter => app.shutter.emit_clicked(),
            Step::Shot(path) => match app.shot_after_paint(path.clone()).await {
                Ok(()) => tracing::info!(path = %path.display(), "window saved"),
                Err(e) => tracing::warn!(error = %e, "window not saved"),
            },
            Step::Quit => {
                app.window.close();
                return;
            }
        }
        app.log_state();
    }
}

/// Tie an object's lifetime to the window's.
fn unsafe_keep(window: &adw::ApplicationWindow, monitor: gio::FileMonitor) {
    let cell = RefCell::new(Some(monitor));
    window.connect_destroy(move |_| {
        cell.borrow_mut().take();
    });
}

fn memory_format(layout: PixelLayout) -> gdk::MemoryFormat {
    match layout {
        PixelLayout::Xrgb8888 => gdk::MemoryFormat::B8g8r8x8,
        PixelLayout::Xbgr8888 => gdk::MemoryFormat::R8g8b8x8,
        PixelLayout::Rgb888 => gdk::MemoryFormat::B8g8r8,
        PixelLayout::Bgr888 => gdk::MemoryFormat::R8g8b8,
    }
}

fn texture(display: &gdk::Display, frame: Frame) -> Option<gdk::Texture> {
    let (w, h) = (frame.size.width as i32, frame.size.height as i32);
    match frame.data {
        FrameData::Memory(v) => Some(
            gdk::MemoryTexture::new(
                w,
                h,
                memory_format(frame.layout),
                &glib::Bytes::from_owned(v),
                frame.stride as usize,
            )
            .upcast(),
        ),
        FrameData::Dmabuf {
            fd,
            offset,
            release,
        } => {
            // SAFETY: the fd belongs to the camera's buffer, which the
            // backend keeps alive and unqueued until `release` is dropped;
            // GTK calls the release function when it no longer reads it.
            let built = unsafe {
                gdk::DmabufTextureBuilder::new()
                    .set_display(display)
                    .set_width(w as u32)
                    .set_height(h as u32)
                    .set_fourcc(frame.layout.fourcc())
                    .set_modifier(0) // DRM_FORMAT_MOD_LINEAR
                    .set_n_planes(1)
                    .set_fd(0, fd)
                    .set_offset(0, offset)
                    .set_stride(0, frame.stride)
                    .build_with_release_func(move || drop(release))
            };
            match built {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!(error = %e, "dmabuf import failed");
                    None
                }
            }
        }
    }
}

/// Mean brightness (0-255) of a memory frame, from a sparse sample.
fn mean_luma(frame: &Frame) -> Option<f64> {
    let FrameData::Memory(data) = &frame.data else {
        return None;
    };
    let bpp = frame.layout.bytes_per_pixel() as usize;
    let (w, h) = (frame.size.width as usize, frame.size.height as usize);
    let (mut sum, mut n) = (0.0, 0u32);
    for y in (0..h).step_by(16) {
        for x in (0..w).step_by(16) {
            let i = y * frame.stride as usize + x * bpp;
            let Some(p) = data.get(i..i + 3) else {
                continue;
            };
            // B, G, R for the XRGB/RGB layouts; close enough for the others.
            sum += 0.114 * p[0] as f64 + 0.587 * p[1] as f64 + 0.299 * p[2] as f64;
            n += 1;
        }
    }
    (n > 0).then(|| sum / n as f64)
}

fn format_zoom(z: f64) -> String {
    let r = (z * 10.0).round() / 10.0;
    if r < 1.0 {
        format!("{r:.1}×").trim_start_matches('0').to_string()
    } else if (r - r.round()).abs() < 0.05 {
        format!("{}×", r.round() as i64)
    } else {
        format!("{r:.1}×")
    }
}

fn preset_label(p: f64) -> String {
    if p < 1.0 {
        format!("{p}").trim_start_matches('0').to_string()
    } else {
        format!("{}", p.round() as i64)
    }
}

impl App {
    fn start_backend(self: &Rc<Self>) {
        let tx = self.tx.clone();
        let slot = self.frame_slot.clone();
        let sink = Box::new(move |e: Event| match e {
            Event::Frame(f) => {
                let was_empty = {
                    let mut s = slot.lock().unwrap();
                    let empty = s.is_none();
                    // An unshown older frame is dropped here (its buffer
                    // goes back to the camera).
                    *s = Some(f);
                    empty
                };
                if was_empty {
                    let _ = tx.send_blocking(Msg::FrameReady);
                }
            }
            other => {
                let _ = tx.send_blocking(Msg::Event(other));
            }
        });
        match crate::spawn_backend(&self.cli, sink) {
            Ok(b) => *self.backend.borrow_mut() = Some(b),
            Err(e) => self.status.set_label(&format!("{e}")),
        }
    }

    fn apply(self: &Rc<Self>, outputs: Vec<Output>) {
        for o in outputs {
            match o {
                Output::Backend(cmd) => {
                    if matches!(
                        cmd,
                        Command::Start { .. } | Command::Stop { .. } | Command::Release
                    ) {
                        self.freeze_in_memory();
                    }
                    if let Some(b) = self.backend.borrow().as_ref() {
                        b.send(cmd);
                    }
                }
                Output::Show(frame) => self.show(frame),
                Output::Picture(p) => self.save(p),
                Output::Error(msg) => self.error(&msg),
            }
        }
        self.refresh();
    }

    fn with_controller<R>(&self, f: impl FnOnce(&mut Controller) -> R) -> Option<R> {
        self.controller.borrow_mut().as_mut().map(f)
    }

    fn handle(self: &Rc<Self>, msg: Msg) {
        let now = Instant::now();
        match msg {
            Msg::Event(Event::Cameras(cameras)) => {
                let opts = Options {
                    capture_mode: match self.cli.capture {
                        CaptureMode::Full => capture::Mode::Full,
                        CaptureMode::Quick => capture::Mode::Quick,
                    },
                    transfer_exposure: self.cli.exposure_hints,
                    ..Options::default()
                };
                let mut c = Controller::new(&self.cli.lens_table(), &cameras, opts);
                if !c.has_cameras() {
                    self.status.set_label("No cameras found");
                    *self.controller.borrow_mut() = Some(c);
                    return;
                }
                c.set_facing(self.cli.start_facing());
                if let Some(z) = self.cli.zoom {
                    c.set_zoom(z, ZoomSource::Preset, now);
                }
                let out = c.resume(now);
                *self.controller.borrow_mut() = Some(c);
                self.apply(out);
            }
            Msg::Event(e) => {
                let out = self.with_controller(|c| c.on_event(e, now));
                if let Some(out) = out {
                    self.apply(out);
                }
            }
            Msg::FrameReady => {
                let frame = self.frame_slot.lock().unwrap().take();
                if let Some(f) = frame {
                    let out = self.with_controller(|c| c.on_event(Event::Frame(f), now));
                    if let Some(out) = out {
                        self.apply(out);
                    }
                }
            }
            Msg::Saved(Ok((path, thumb))) => {
                let tex = gdk::MemoryTexture::new(
                    thumb.width as i32,
                    thumb.height as i32,
                    gdk::MemoryFormat::B8g8r8x8,
                    &glib::Bytes::from_owned(thumb.data),
                    thumb.width as usize * 4,
                );
                self.thumb_picture.set_paintable(Some(&tex));
                self.thumb_picture.set_pixel_size(56);
                self.thumb.set_sensitive(true);
                tracing::info!(path = %path.display(), "photo saved");
                *self.last_photo.borrow_mut() = Some(path);
            }
            Msg::Saved(Err(e)) => self.error(&format!("Photo not saved: {e}")),
        }
    }

    fn show(self: &Rc<Self>, frame: Frame) {
        let placement = self.placement_for(&frame.camera);
        let camera = frame.camera.clone();
        let size = (frame.size.width, frame.size.height);
        if self.cli.debug || self.cli.script.is_some() {
            self.shown_luma.set(mean_luma(&frame));
            self.shown_ae
                .set((frame.meta.exposure_us, frame.meta.analogue_gain));
        }
        if let Some(tex) = texture(&WidgetExt::display(&self.window), frame) {
            self.viewfinder.set_frame(tex, placement);
            self.status.set_visible(false);
            *self.shown_camera.borrow_mut() = Some(camera);
            self.shown_size.set(size);
            self.fps.borrow_mut().tick();
        }
    }

    /// Before a stream stops: keep the frame on screen as a memory copy,
    /// so the camera's dmabuf goes back before its buffers are freed.
    fn freeze_in_memory(&self) {
        let Some(tex) = self.viewfinder.texture() else {
            return;
        };
        if !tex.is::<gdk::DmabufTexture>() {
            return;
        }
        let mut downloader = gdk::TextureDownloader::new(&tex);
        downloader.set_format(gdk::MemoryFormat::B8g8r8a8);
        let (bytes, stride) = downloader.download_bytes();
        let copy = gdk::MemoryTexture::new(
            tex.width(),
            tex.height(),
            gdk::MemoryFormat::B8g8r8a8,
            &bytes,
            stride,
        );
        self.viewfinder
            .set_frame(copy.upcast(), self.viewfinder.placement());
    }

    fn placement_for(&self, camera: &str) -> Placement {
        self.controller
            .borrow()
            .as_ref()
            .and_then(|c| c.transform(camera))
            .map(|t| Placement {
                scale: t.scale,
                rotation: t.rotation,
                mirror: t.mirror,
            })
            .unwrap_or_default()
    }

    fn save(self: &Rc<Self>, p: Picture) {
        // A quick flash, so the press visibly did something.
        self.viewfinder.set_opacity(0.35);
        let vf = self.viewfinder.clone();
        glib::timeout_add_local_once(Duration::from_millis(120), move || vf.set_opacity(1.0));

        let dir = self.cli.output_dir.clone().unwrap_or_else(|| {
            glib::user_special_dir(glib::UserDirectory::Pictures)
                .unwrap_or_else(|| glib::home_dir().join("Pictures"))
                .join("Camera")
        });
        let stamp = glib::DateTime::now_local()
            .and_then(|d| d.format("%Y%m%d_%H%M%S"))
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "photo".into());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = encode_and_write(&p, &dir, &stamp).map_err(|e| format!("{e:#}"));
            let _ = tx.send_blocking(Msg::Saved(result));
        });
    }

    fn error(&self, msg: &str) {
        tracing::warn!("{msg}");
        let mut last = self.last_error.borrow_mut();
        let repeat = last
            .as_ref()
            .is_some_and(|(m, t)| m == msg && t.elapsed() < Duration::from_secs(10));
        if !repeat {
            self.toasts.add_toast(adw::Toast::new(msg));
            *last = Some((msg.to_string(), Instant::now()));
        }
    }

    /// Bring the controls in line with the controller.
    fn refresh(self: &Rc<Self>) {
        let Some(view) = self.controller.borrow().as_ref().map(|c| c.view()) else {
            return;
        };
        if *self.built_presets.borrow() != view.presets {
            self.build_pills(&view.presets);
        }
        // The active pill: the highest preset at or below the zoom; it
        // shows the exact zoom.
        let active = view
            .presets
            .iter()
            .rposition(|&p| view.zoom + 1e-6 >= p)
            .unwrap_or(0);
        for (i, (p, b)) in self.pill_buttons.borrow().iter().enumerate() {
            if i == active {
                b.add_css_class("active");
                b.set_label(&format_zoom(view.zoom));
            } else {
                b.remove_css_class("active");
                b.set_label(&preset_label(*p));
            }
        }
        self.lens_label.set_label(&view.lens_name);
        self.flip.set_visible(view.can_flip);
        self.flip.set_sensitive(!view.capturing);
        if view.capturing {
            self.shutter.add_css_class("busy");
        } else {
            self.shutter.remove_css_class("busy");
        }
        self.shutter
            .set_sensitive(!view.capturing && !view.switching);
        if !self.setting_slider.get() {
            let pos = self
                .controller
                .borrow()
                .as_ref()
                .map(|c| c.slider_position())
                .unwrap_or(0.0);
            self.setting_slider.set(true);
            self.slider.set_value(pos);
            self.setting_slider.set(false);
        }
        // Zoom a frozen frame with the fingers while a lens starts.
        if let Some(cam) = self.shown_camera.borrow().as_ref() {
            self.viewfinder.set_placement(self.placement_for(cam));
        }
        if self.cli.debug {
            let fps = self.fps.borrow().value;
            let (w, h) = self.shown_size.get();
            let switch = view
                .last_switch
                .map(|d| format!("{} ms", d.as_millis()))
                .unwrap_or_else(|| "-".into());
            self.debug.set_label(&format!(
                "{} {}x{}  {:.1} fps\nzoom {:.2}  switch {}{}",
                view.lens_name,
                w,
                h,
                fps,
                view.zoom,
                switch,
                if view.switching { "  (switching)" } else { "" }
            ));
        }
    }

    fn build_pills(self: &Rc<Self>, presets: &[f64]) {
        while let Some(c) = self.pills.first_child() {
            self.pills.remove(&c);
        }
        let mut buttons = Vec::new();
        for &p in presets {
            let b = gtk::Button::with_label(&preset_label(p));
            b.add_css_class("zoom-pill");
            b.set_tooltip_text(Some(&format_zoom(p)));
            let a = Rc::downgrade(self);
            b.connect_clicked(move |_| {
                if let Some(a) = a.upgrade() {
                    a.zoom_to(p, ZoomSource::Preset);
                }
            });
            self.pills.append(&b);
            buttons.push((p, b));
        }
        *self.pill_buttons.borrow_mut() = buttons;
        *self.built_presets.borrow_mut() = presets.to_vec();
        // Slider marks at the presets.
        self.slider.clear_marks();
        if let Some(c) = self.controller.borrow().as_ref() {
            for &p in presets {
                self.slider
                    .add_mark(c.slider_for(p), gtk::PositionType::Bottom, None);
            }
        }
    }

    fn zoom_to(self: &Rc<Self>, zoom: f64, source: ZoomSource) {
        let now = Instant::now();
        if let Some(out) = self.with_controller(|c| c.set_zoom(zoom, source, now)) {
            self.apply(out);
        }
        self.flash_zoom();
    }

    fn zoom_released(self: &Rc<Self>) {
        if let Some(out) = self.with_controller(|c| c.zoom_released(Instant::now())) {
            self.apply(out);
        }
    }

    /// Show the zoom readout and the slider for a moment.
    fn flash_zoom(self: &Rc<Self>) {
        let zoom = self
            .controller
            .borrow()
            .as_ref()
            .map(|c| c.view().zoom)
            .unwrap_or(1.0);
        self.zoom_indicator.set_label(&format_zoom(zoom));
        self.zoom_indicator.set_visible(true);
        self.slider_revealer.set_reveal_child(true);
        if let Some(id) = self.hide_source.borrow_mut().take() {
            id.remove();
        }
        let a = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(HIDE_AFTER, move || {
            if let Some(a) = a.upgrade() {
                a.hide_source.borrow_mut().take();
                a.zoom_indicator.set_visible(false);
                a.slider_revealer.set_reveal_child(false);
            }
        });
        *self.hide_source.borrow_mut() = Some(id);
    }

    fn connect_input(self: &Rc<Self>) {
        // Pinch anywhere on the viewfinder.
        let pinch = gtk::GestureZoom::new();
        let a = Rc::downgrade(self);
        pinch.connect_begin(move |_, _| {
            if let Some(a) = a.upgrade() {
                let z = a
                    .controller
                    .borrow()
                    .as_ref()
                    .map(|c| c.view().zoom)
                    .unwrap_or(1.0);
                a.pinch_base.set(z);
            }
        });
        let a = Rc::downgrade(self);
        pinch.connect_scale_changed(move |_, s| {
            if let Some(a) = a.upgrade() {
                a.zoom_to(a.pinch_base.get() * s, ZoomSource::Gesture);
            }
        });
        let a = Rc::downgrade(self);
        pinch.connect_end(move |_, _| {
            if let Some(a) = a.upgrade() {
                a.zoom_released();
            }
        });
        self.viewfinder.add_controller(pinch);

        // Drag sideways across the pills to zoom smoothly, like the slider.
        let drag = gtk::GestureDrag::new();
        drag.set_propagation_phase(gtk::PropagationPhase::Capture);
        let a = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, _, _| {
            if let Some(a) = a.upgrade() {
                let pos = a
                    .controller
                    .borrow()
                    .as_ref()
                    .map(|c| c.slider_position())
                    .unwrap_or(0.0);
                a.drag_base.set(pos);
                a.drag_claimed.set(false);
            }
        });
        let a = Rc::downgrade(self);
        drag.connect_drag_update(move |g, dx, _| {
            let Some(a) = a.upgrade() else { return };
            if !a.drag_claimed.get() {
                if dx.abs() < 12.0 {
                    return;
                }
                g.set_state(gtk::EventSequenceState::Claimed);
                a.drag_claimed.set(true);
            }
            let pos = (a.drag_base.get() + dx / 360.0).clamp(0.0, 1.0);
            let zoom = a
                .controller
                .borrow()
                .as_ref()
                .map(|c| c.zoom_for_slider(pos));
            if let Some(z) = zoom {
                a.zoom_to(z, ZoomSource::Gesture);
            }
        });
        let a = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(a) = a.upgrade() {
                if a.drag_claimed.replace(false) {
                    a.zoom_released();
                }
            }
        });
        self.pills.add_controller(drag);

        let a = Rc::downgrade(self);
        self.slider.connect_value_changed(move |s| {
            let Some(a) = a.upgrade() else { return };
            if a.setting_slider.get() {
                return;
            }
            let zoom = a
                .controller
                .borrow()
                .as_ref()
                .map(|c| c.zoom_for_slider(s.value()));
            if let Some(z) = zoom {
                a.setting_slider.set(true);
                a.zoom_to(z, ZoomSource::Gesture);
                a.setting_slider.set(false);
            }
        });

        let a = Rc::downgrade(self);
        self.shutter.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                if let Some(out) = a.with_controller(|c| c.shutter(Instant::now())) {
                    a.apply(out);
                }
            }
        });
        let a = Rc::downgrade(self);
        self.flip.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                if let Some(out) = a.with_controller(|c| c.flip(Instant::now())) {
                    a.apply(out);
                }
            }
        });
        let a = Rc::downgrade(self);
        self.thumb.connect_clicked(move |_| {
            let Some(a) = a.upgrade() else { return };
            let Some(path) = a.last_photo.borrow().clone() else {
                return;
            };
            let uri = gio::File::for_path(&path).uri();
            if let Err(e) =
                gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>)
            {
                a.error(&format!("Can't open the photo: {e}"));
            }
        });

        // Let other apps have the cameras while hidden.
        let a = Rc::downgrade(self);
        self.window.connect_suspended_notify(move |w| {
            if let Some(a) = a.upgrade() {
                tracing::info!(suspended = w.is_suspended(), "window state");
                a.set_suspended(w.is_suspended());
            }
        });
        let a = Rc::downgrade(self);
        self.window.connect_is_active_notify(move |w| {
            let Some(a) = a.upgrade() else { return };
            if let Some(id) = a.unfocus_source.borrow_mut().take() {
                id.remove();
            }
            if w.is_active() {
                if !w.is_suspended() {
                    a.set_suspended(false);
                }
            } else {
                let weak = Rc::downgrade(&a);
                let id = glib::timeout_add_local_once(UNFOCUSED_RELEASE, move || {
                    if let Some(a) = weak.upgrade() {
                        a.unfocus_source.borrow_mut().take();
                        tracing::info!("unfocused for a while: releasing the cameras");
                        a.set_suspended(true);
                    }
                });
                *a.unfocus_source.borrow_mut() = Some(id);
            }
        });
    }

    fn set_suspended(self: &Rc<Self>, suspended: bool) {
        if self.suspended.replace(suspended) == suspended {
            return;
        }
        let now = Instant::now();
        let out = self.with_controller(|c| {
            if suspended {
                c.suspend(now)
            } else {
                c.resume(now)
            }
        });
        if let Some(out) = out {
            self.apply(out);
        }
    }

    fn tick(self: &Rc<Self>) {
        if let Some(out) = self.with_controller(|c| c.tick(Instant::now())) {
            if !out.is_empty() {
                self.apply(out);
            } else if self.cli.debug {
                self.refresh();
            }
        }
    }

    /// Wait until the window has painted a frame (at most 500 ms), so its
    /// render nodes are fresh for a snapshot.
    async fn shot_after_paint(self: &Rc<Self>, path: PathBuf) -> anyhow::Result<()> {
        let Some(clock) = self.window.frame_clock() else {
            return self.render_png(&path);
        };
        let (tx, rx) = async_channel::bounded::<anyhow::Result<()>>(2);
        let done = Rc::new(Cell::new(false));
        let (app, p, t, d) = (Rc::downgrade(self), path.clone(), tx.clone(), done.clone());
        // Snapshot inside the paint, while the render nodes are fresh.
        let id = clock.connect_after_paint(move |_| {
            if d.replace(true) {
                return;
            }
            if let Some(a) = app.upgrade() {
                let _ = t.try_send(a.render_png(&p));
            }
        });
        // No paint (the panel is off): render what there is.
        let app = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            if done.replace(true) {
                return;
            }
            if let Some(a) = app.upgrade() {
                let _ = tx.try_send(a.render_png(&path));
            }
        });
        let result = rx
            .recv()
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("no result")));
        clock.disconnect(id);
        result
    }

    /// Render the window's content to a PNG, the way it is drawn on screen.
    /// With the panel off the compositor sends no frames and GTK lays
    /// nothing out; then only the viewfinder is drawn, at the default
    /// window size.
    fn render_png(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let content = self
            .window
            .content()
            .ok_or_else(|| anyhow::anyhow!("no content"))?;
        let (mut w, mut h) = (content.width() as f32, content.height() as f32);
        let mut node = None;
        if w > 0.0 && h > 0.0 && content.is_mapped() {
            let snapshot = gtk::Snapshot::new();
            gtk::WidgetPaintable::new(Some(&content)).snapshot(&snapshot, w as f64, h as f64);
            node = snapshot.to_node();
        }
        if node.is_none() {
            tracing::info!(
                mapped = content.is_mapped(),
                "window not drawn (panel off?): drawing the viewfinder alone"
            );
            if w <= 0.0 || h <= 0.0 {
                let (dw, dh) = self.window.default_size();
                (w, h) = (dw as f32, dh as f32);
            }
            let snapshot = gtk::Snapshot::new();
            crate::viewfinder::draw(
                &snapshot,
                (w, h),
                self.viewfinder.texture().as_ref(),
                self.viewfinder.placement(),
                0.0,
            );
            node = snapshot.to_node();
        }
        let node = node.ok_or_else(|| anyhow::anyhow!("nothing drawn"))?;
        let renderer = match self.window.renderer() {
            Some(r) => r,
            None => {
                let r = gtk::gsk::CairoRenderer::new();
                r.realize_for_display(&WidgetExt::display(&self.window))?;
                r.upcast()
            }
        };
        let bounds = gtk::graphene::Rect::new(0.0, 0.0, w, h);
        let texture = renderer.render_texture(node, Some(&bounds));
        texture.save_to_png(path)?;
        Ok(())
    }

    /// One log line with what the debug overlay shows.
    fn log_state(&self) {
        let Some(view) = self.controller.borrow().as_ref().map(|c| c.view()) else {
            return;
        };
        let (w, h) = self.shown_size.get();
        tracing::info!(
            lens = %view.lens_name,
            zoom = view.zoom,
            size = %format!("{w}x{h}"),
            fps = format!("{:.1}", self.fps.borrow().value),
            switch_ms = view.last_switch.map(|d| d.as_millis() as u64),
            switching = view.switching,
            capturing = view.capturing,
            luma = self.shown_luma.get().map(|l| format!("{l:.0}")),
            exposure_us = self.shown_ae.get().0,
            gain = self.shown_ae.get().1,
            "state"
        );
    }

    fn shutdown(self: &Rc<Self>) {
        self.suspended.set(true);
        self.freeze_in_memory();
        self.frame_slot.lock().unwrap().take();
        if let Some(out) = self.with_controller(|c| c.suspend(Instant::now())) {
            for o in out {
                if let Output::Backend(cmd) = o {
                    if let Some(b) = self.backend.borrow().as_ref() {
                        b.send(cmd);
                    }
                }
            }
        }
        // Dropping the backend joins its thread after the stop.
        self.backend.borrow_mut().take();
    }
}

fn encode_and_write(
    p: &Picture,
    dir: &std::path::Path,
    stamp: &str,
) -> anyhow::Result<(PathBuf, photo::Bgrx)> {
    let FrameData::Memory(data) = &p.frame.data else {
        anyhow::bail!("the still frame wasn't copied to memory");
    };
    let started = Instant::now();
    let raw = photo::Raw {
        data,
        width: p.frame.size.width,
        height: p.frame.size.height,
        stride: p.frame.stride,
        layout: p.frame.layout,
    };
    let img = photo::crop_and_rotate(&raw, p.crop, p.rotation)?;
    let jpeg = photo::encode_jpeg(&img)?;
    std::fs::create_dir_all(dir)?;
    let path = photo::next_path(dir, stamp);
    photo::write_atomically(&path, &jpeg)?;
    tracing::info!(
        lens = %p.lens.name,
        zoom = p.zoom,
        size = %format!("{}x{}", img.width, img.height),
        bytes = jpeg.len(),
        ms = started.elapsed().as_millis() as u64,
        "photo encoded"
    );
    Ok((path, photo::thumbnail(&img)))
}
