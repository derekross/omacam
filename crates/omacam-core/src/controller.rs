//! The controller: user input and backend events in, backend commands and
//! things to show out. The UI owns one and does what it returns.

use std::time::{Duration, Instant};

use crate::backend::{Awb, Caps, ExposureSettings, Setting};
use crate::backend::{CameraInfo, Command, Event, Frame, FrameMeta, Purpose, StreamRequest};
use crate::capture::{self, CaptureController, Step};
use crate::exposure;
use crate::exposure_control::{self, Exposure};
use crate::focus::{self, FocusController};
use crate::framing;
pub use crate::framing::Transform;
use crate::lens::{Facing, Lens, LensTable, Role, TableSpec};
use crate::lowlight::{self, LowLight};
use crate::switch::{Action, FrameKind, SwitchController, Target};
use crate::zoom::{Ladder, ZoomPlanner, ZoomSource};

/// How long to wait before trying a camera again after it failed to start
/// (another app may hold it).
pub const RETRY_AFTER: Duration = Duration::from_secs(2);

/// Preset zoom buttons, as CAMERA-APP-PLAN §3.4 has them. A preset just
/// above the widest lens (0.5 over 0.45) snaps to that lens's full view.
pub const BACK_PRESETS: [f64; 4] = [0.5, 1.0, 2.0, 5.0];
pub const FRONT_PRESETS: [f64; 2] = [1.0, 2.0];

#[derive(Debug, Clone)]
pub struct Options {
    pub capture_mode: capture::Mode,
    pub preview_buffers: u32,
    pub still_buffers: u32,
    /// Start a new lens with the old one's exposure, scaled by aperture.
    pub transfer_exposure: bool,
    /// Taps on the picture focus there.
    pub tap_to_focus: bool,
    /// In dim light, show the telephoto's zoom as a crop of the main
    /// camera ([`crate::lowlight`]).
    pub low_light: bool,
    /// Frames per second asked of a camera in video mode, where its mode
    /// allows it (30 or 60).
    pub video_fps: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            capture_mode: capture::Mode::Full,
            preview_buffers: 4,
            still_buffers: 2,
            transfer_exposure: true,
            tap_to_focus: true,
            low_light: true,
            video_fps: 30,
        }
    }
}

/// A captured frame to encode and save.
#[derive(Debug)]
pub struct Picture {
    pub frame: Frame,
    /// How the frame shows the view: the zoom crop, its center, the
    /// mode's coverage, the turn upright.
    pub transform: Transform,
    /// Center crop: keep 1/crop of each side (1 = the whole frame).
    pub crop: f64,
    /// Clockwise degrees that turn the frame upright.
    pub rotation: u32,
    pub lens: Lens,
    pub zoom: f64,
    /// The exposure settings in force (EXIF records them).
    pub settings: ExposureSettings,
}

/// What the UI should do.
#[derive(Debug)]
pub enum Output {
    Backend(Command),
    /// A new frame for the viewfinder.
    Show(Frame),
    Picture(Box<Picture>),
    /// One frame of a burst (Night, HDR) for the merge; more follow until
    /// [`Output::BurstDone`].
    BurstFrame(Box<Picture>),
    /// The burst's last frame was the one before: merge them.
    BurstDone,
    /// Something to tell the user.
    Error(String),
}

/// Photos or videos.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Photo,
    Video,
}

/// The exposure state the Pro panel and the EV slider show.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExposureView {
    pub caps: Caps,
    /// What's sent to the camera now.
    pub settings: ExposureSettings,
    /// The Pro panel's own values (manual ones stay set across lenses).
    pub pro: ExposureSettings,
    /// The EV slider's offset.
    pub ev_user: f32,
    pub locked: bool,
    /// ISO at gain 1 for this lens.
    pub base_iso: f64,
    /// What the camera reported for the last frame (auto's choice).
    pub shutter_us: Option<i32>,
    pub iso: Option<u32>,
    pub lens_position: Option<f32>,
}

/// The state the controls show.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewState {
    pub facing: Facing,
    pub zoom: f64,
    pub min_zoom: f64,
    pub max_zoom: f64,
    pub presets: Vec<f64>,
    /// The lens that is wanted for the current zoom.
    pub lens_role: Option<Role>,
    pub lens_name: String,
    pub switching: bool,
    pub capturing: bool,
    pub can_flip: bool,
    pub last_switch: Option<Duration>,
    /// Exposure, white balance and focus: what the camera offers, what's
    /// set, and what auto chose.
    pub exposure: ExposureView,
    /// The focus ring, after a tap.
    pub focus: Option<focus::Indicator>,
    pub mode: Mode,
    pub recording: bool,
    /// The telephoto's zoom is shown by the main camera because the light
    /// is too low for the tele.
    pub low_light: bool,
    /// The streaming camera can run at 60 fps in its current mode.
    pub can_60fps: bool,
    /// The frame rate video mode asks for now (30 or 60).
    pub video_fps: u32,
}

struct Side {
    planner: ZoomPlanner,
    /// Ladder step to lens-table index.
    lenses: Vec<usize>,
    roles: Vec<Role>,
    /// (zoom factor, axis) of each step, for the view's center.
    axes: Vec<(f64, (f64, f64))>,
    /// Low light: the tele's step is shown by the main camera.
    fallback: bool,
}

impl Side {
    fn new(table: &LensTable, facing: Facing) -> Option<Side> {
        let lenses = table.ladder(facing);
        if lenses.is_empty() {
            return None;
        }
        let factors = lenses.iter().map(|&i| table.get(i).zoom).collect();
        let ladder = Ladder::new(factors, table.max_zoom(facing));
        let roles = lenses.iter().map(|&i| table.get(i).role).collect();
        let axes = lenses
            .iter()
            .map(|&i| (table.get(i).zoom, table.get(i).axis))
            .collect();
        // Open at 1x, or the widest lens when nothing reaches 1x.
        Some(Side {
            planner: ZoomPlanner::new(ladder, 1.0),
            lenses,
            roles,
            axes,
            fallback: false,
        })
    }

    /// The ladder's choice for the zoom, before any low-light fallback.
    fn planned(&self) -> usize {
        self.lenses[self.planner.target()]
    }

    /// The main camera stands in for the tele: low light, the tele's step.
    fn falls_back(&self) -> bool {
        self.fallback
            && self.roles[self.planner.target()] == Role::Tele
            && self.roles.contains(&Role::Main)
    }

    /// The lens that streams for the zoom.
    fn lens(&self) -> usize {
        if self.falls_back() {
            if let Some(main) = self.roles.iter().position(|&r| r == Role::Main) {
                return self.lenses[main];
            }
        }
        self.planned()
    }
}

pub struct Controller {
    table: LensTable,
    opts: Options,
    back: Option<Side>,
    front: Option<Side>,
    facing: Facing,
    switch: SwitchController,
    capture: CaptureController,
    focus: FocusController,
    exposure: Exposure,
    last_meta: Option<(usize, FrameMeta)>,
    suspended: bool,
    retry_at: Option<Instant>,
    mode: Mode,
    recording: bool,
    low_light: LowLight,
    /// The Pro panel is open: the lens is the one the zoom picks, never a
    /// low-light stand-in.
    pro: bool,
    /// The highest frame rate each camera's current mode allows (from the
    /// stream's start), by camera id.
    max_fps: std::collections::HashMap<String, f64>,
}

impl Controller {
    pub fn new(spec: &TableSpec, cameras: &[CameraInfo], opts: Options) -> Controller {
        let table = spec.resolve(cameras);
        let back = Side::new(&table, Facing::Back);
        let front = Side::new(&table, Facing::Front);
        // The back camera, unless there's only a front one.
        let facing = if back.is_some() || front.is_none() {
            Facing::Back
        } else {
            Facing::Front
        };
        Controller {
            table,
            opts,
            back,
            front,
            facing,
            switch: SwitchController::new(),
            capture: CaptureController::new(),
            focus: FocusController::new(),
            exposure: Exposure::new(),
            last_meta: None,
            suspended: true,
            retry_at: None,
            mode: Mode::Photo,
            recording: false,
            low_light: LowLight::default(),
            pro: false,
            max_fps: std::collections::HashMap::new(),
        }
    }

    pub fn table(&self) -> &LensTable {
        &self.table
    }

    pub fn has_cameras(&self) -> bool {
        self.back.is_some() || self.front.is_some()
    }

    /// True between [`Controller::resume`] and [`Controller::suspend`].
    pub fn is_running(&self) -> bool {
        !self.suspended
    }

    /// The cameras changed (drivers loaded late, or one went away): stop
    /// what streams, take the new list, and, if running, start the
    /// default camera of the side in use (or of the other side when this
    /// one has none left). Stream generations carry on, so frames of the
    /// old stream can't pass for the new one's.
    pub fn set_cameras(
        &mut self,
        spec: &TableSpec,
        cameras: &[CameraInfo],
        now: Instant,
    ) -> Vec<Output> {
        let mut out = Vec::new();
        self.capture.abort();
        self.focus.reset();
        self.retry_at = None;
        self.last_meta = None;
        let table = spec.resolve(cameras);
        self.back = Side::new(&table, Facing::Back);
        self.front = Side::new(&table, Facing::Front);
        self.table = table;
        let table = &self.table;
        self.switch.remap(|id| table.index_of(id));
        if self.side().is_none() {
            self.facing = match self.facing {
                Facing::Back if self.front.is_some() => Facing::Front,
                Facing::Front if self.back.is_some() => Facing::Back,
                f => f,
            };
        }
        // A camera still present keeps streaming (targets are the same
        // stream by camera id); a gone one is replaced or stopped.
        if self.suspended {
            self.request(None, now, &mut out);
        } else {
            self.request_preview(now, &mut out);
            if self.side().is_none() {
                self.request(None, now, &mut out);
            }
        }
        out
    }

    fn side(&self) -> Option<&Side> {
        match self.facing {
            Facing::Back => self.back.as_ref(),
            Facing::Front => self.front.as_ref(),
        }
    }

    fn side_mut(&mut self) -> Option<&mut Side> {
        match self.facing {
            Facing::Back => self.back.as_mut(),
            Facing::Front => self.front.as_mut(),
        }
    }

    pub fn view(&self) -> ViewState {
        let side = self.side();
        let lens = side.map(|s| self.table.get(s.lens()));
        let (zoom, min_zoom, max_zoom) = side
            .map(|s| {
                let l = s.planner.ladder();
                (s.planner.zoom(), l.min(), l.max())
            })
            .unwrap_or((1.0, 1.0, 1.0));
        let presets = match self.facing {
            Facing::Back => &BACK_PRESETS[..],
            Facing::Front => &FRONT_PRESETS[..],
        };
        ViewState {
            facing: self.facing,
            zoom,
            min_zoom,
            max_zoom,
            presets: presets
                .iter()
                .copied()
                .filter(|&p| p >= min_zoom - 0.06 && p <= max_zoom)
                // The ".5" button is the widest lens's whole view (0.45x on
                // the Pixel 7 Pro), as stock's is, not a crop of it.
                .map(|p| {
                    if p < 1.0 && p - min_zoom < 0.1 {
                        min_zoom
                    } else {
                        p
                    }
                })
                .collect(),
            lens_role: lens.map(|l| l.role),
            lens_name: lens.map(|l| l.name.clone()).unwrap_or_default(),
            switching: self.switch.is_switching(),
            capturing: self.capture.is_busy(),
            can_flip: self.back.is_some() && self.front.is_some(),
            last_switch: self.switch.last_switch_time(),
            exposure: self.exposure_view(),
            focus: self.focus.indicator(),
            mode: self.mode,
            recording: self.recording,
            low_light: side.is_some_and(|s| s.falls_back()),
            can_60fps: self.can_60fps(),
            video_fps: self.opts.video_fps,
        }
    }

    /// The streaming camera's mode allows 60 fps.
    fn can_60fps(&self) -> bool {
        self.switch
            .streaming()
            .and_then(|(_, t)| self.max_fps.get(&t.camera))
            .is_some_and(|&f| f >= 59.0)
    }

    /// The frame duration to ask of a stream of `camera`, microseconds:
    /// 60 fps in video mode when asked for and its mode allows it, else
    /// nothing (the camera's default, 30 fps).
    fn frame_duration_for(&self, camera: &str, purpose: Purpose) -> Option<i64> {
        // Only on a camera whose mode reported its frame durations (it
        // takes FrameDurationLimits); elsewhere the camera keeps its own.
        let max = *self.max_fps.get(camera)?;
        let fast = self.mode == Mode::Video
            && purpose == Purpose::Preview
            && self.opts.video_fps >= 60
            && max >= 59.0;
        Some(if fast { 16_667 } else { 33_333 })
    }

    /// Slider position (0..1) for the current zoom.
    pub fn slider_position(&self) -> f64 {
        self.side()
            .map(|s| s.planner.ladder().to_slider(s.planner.zoom()))
            .unwrap_or(0.0)
    }

    /// Slider position (0..1) of a zoom value.
    pub fn slider_for(&self, zoom: f64) -> f64 {
        self.side()
            .map(|s| s.planner.ladder().to_slider(zoom))
            .unwrap_or(0.0)
    }

    pub fn zoom_for_slider(&self, pos: f64) -> f64 {
        self.side()
            .map(|s| s.planner.ladder().from_slider(pos))
            .unwrap_or(1.0)
    }

    /// How to draw a preview frame from `camera` at the current zoom.
    pub fn transform(&self, camera: &str) -> Option<Transform> {
        self.transform_for(camera, Purpose::Preview)
    }

    /// How a frame of `camera`'s preview or still mode shows the current
    /// view. Every lens of the side frames the view alike (the scale is
    /// zoom over the lens's factor, about the center of [`framing`]); a
    /// frame from the other side (frozen across a flip) keeps its whole
    /// view. A frame is never drawn below 1x: a lens can't show more than
    /// its own field of view, so a frozen frame from a narrower lens holds
    /// at its full view until the wider lens's first frame.
    pub fn transform_for(&self, camera: &str, purpose: Purpose) -> Option<Transform> {
        let i = self.table.index_of(camera)?;
        let lens = self.table.get(i);
        let rotation = lens.upright_clockwise();
        let coverage = match purpose {
            Purpose::Preview => framing::upright_pair(lens.shown_coverage(), rotation),
            Purpose::Still => (1.0, 1.0),
        };
        let mut t = Transform {
            rotation,
            mirror: lens.mirror,
            coverage,
            ..Transform::default()
        };
        let Some(side) = self.side() else {
            return Some(t);
        };
        if lens.role.faces_front() != (self.facing == Facing::Front) {
            return Some(t);
        }
        let zoom = side.planner.zoom();
        t.scale = (zoom / lens.zoom).max(1.0);
        let center = framing::view_center(zoom, &side.axes);
        t.center = framing::lens_center(center, lens.zoom, lens.axis, t.scale);
        Some(t)
    }

    /// Time until [`Controller::tick`] has something to do.
    pub fn next_tick(&self, now: Instant) -> Option<Duration> {
        let zoom = self.side().and_then(|s| s.planner.next_tick(now));
        let retry = self.retry_at.map(|t| t.saturating_duration_since(now));
        let capture = self.capture.is_busy().then_some(Duration::from_millis(100));
        let focus = self
            .focus
            .indicator()
            .is_some()
            .then_some(Duration::from_millis(100));
        [zoom, retry, capture, focus].into_iter().flatten().min()
    }

    fn target(&self, purpose: Purpose) -> Option<Target> {
        let side = self.side()?;
        let lens_idx = side.lens();
        let lens = self.table.get(lens_idx);
        let mut target = Target::new(lens_idx, purpose);
        target.camera = lens.id.clone();
        let settings = self.exposure.effective(&lens.caps);
        target.controls.af_continuous = lens.autofocus && settings.lens.is_auto();
        target.controls.settings = Some(settings);
        target.controls.frame_duration_us = self.frame_duration_for(&lens.id, purpose);
        if self.opts.transfer_exposure {
            if let Some((from, meta)) = &self.last_meta {
                let from_lens = self.table.get(*from);
                if let (Some(t), Some(f_old), Some(f_new)) =
                    (meta.exposure_us, from_lens.f_number, lens.f_number)
                {
                    let e = exposure::transfer(
                        exposure::Exposure {
                            time_us: t as f64,
                            gain: meta.analog_gain.unwrap_or(1.0) as f64,
                        },
                        f_old,
                        f_new,
                        1.0,
                        exposure::Limits::default(),
                    );
                    target.controls.exposure_us = Some(e.time_us.round() as i32);
                    target.controls.analog_gain = Some(e.gain as f32);
                }
            }
        }
        Some(target)
    }

    fn command(&self, action: Action) -> Command {
        match action {
            Action::Start { generation, target } => {
                let lens = self.table.get(target.lens);
                let stream = match target.purpose {
                    Purpose::Preview => StreamRequest {
                        purpose: Purpose::Preview,
                        size: lens.preview,
                        buffers: self.opts.preview_buffers,
                        fallbacks: Vec::new(),
                    },
                    Purpose::Still => {
                        let size = lens.still.or(lens.preview);
                        StreamRequest {
                            purpose: Purpose::Still,
                            size,
                            buffers: self.opts.still_buffers,
                            // The preview mode always fits: it just ran.
                            fallbacks: lens
                                .preview
                                .filter(|p| Some(*p) != size)
                                .into_iter()
                                .collect(),
                        }
                    }
                };
                Command::Start {
                    generation,
                    camera: lens.id.clone(),
                    stream,
                    controls: target.controls,
                }
            }
            Action::Stop { generation } => Command::Stop { generation },
        }
    }

    fn run(&self, actions: Vec<Action>, out: &mut Vec<Output>) {
        out.extend(
            actions
                .into_iter()
                .map(|a| Output::Backend(self.command(a))),
        );
    }

    fn request(&mut self, target: Option<Target>, now: Instant, out: &mut Vec<Output>) {
        let actions = self.switch.request(target, now);
        self.run(actions, out);
    }

    fn request_preview(&mut self, now: Instant, out: &mut Vec<Output>) {
        if self.suspended {
            return;
        }
        // Another camera: metering, the EV slider and a lock start over.
        let next = self.side().map(|s| self.table.get(s.lens()).id.clone());
        if let (Some(wanted), Some(next)) = (self.switch.wanted(), next) {
            if wanted.camera != next {
                self.exposure.lens_switched();
            }
        }
        let target = self.target(Purpose::Preview);
        self.request(target, now, out);
    }

    /// Start streaming (the app opened, or came back to the foreground).
    pub fn resume(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        self.suspended = false;
        self.retry_at = None;
        self.request_preview(now, &mut out);
        out
    }

    /// Stop streaming and let other apps have the cameras.
    pub fn suspend(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        self.suspended = true;
        self.recording = false;
        self.retry_at = None;
        self.capture.abort();
        self.request(None, now, &mut out);
        out.push(Output::Backend(Command::Release));
        out
    }

    pub fn set_zoom(&mut self, zoom: f64, source: ZoomSource, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if self.capture.is_busy() {
            return out;
        }
        let changed = self
            .side_mut()
            .and_then(|s| s.planner.set(zoom, source, now));
        if changed.is_some() {
            self.request_preview(now, &mut out);
        }
        out
    }

    /// A pinch or slider drag ended.
    pub fn zoom_released(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        let changed = self.side_mut().and_then(|s| s.planner.release(now));
        if changed.is_some() {
            self.request_preview(now, &mut out);
        }
        out
    }

    /// Change whether taps focus (a setting).
    pub fn set_tap_to_focus(&mut self, on: bool) {
        self.opts.tap_to_focus = on;
    }

    /// Change the photo mode (a setting).
    pub fn set_capture_mode(&mut self, mode: capture::Mode) {
        self.opts.capture_mode = mode;
    }

    /// Photo or video. Ignored while recording or taking a photo. Video
    /// mode may run the camera faster (60 fps).
    pub fn set_mode(&mut self, mode: Mode) -> Vec<Output> {
        let mut out = Vec::new();
        if !self.recording && !self.capture.is_busy() && self.mode != mode {
            self.mode = mode;
            self.push_frame_rate(&mut out);
        }
        out
    }

    /// The video frame rate setting (30 or 60). Ignored while recording.
    pub fn set_video_fps(&mut self, fps: u32) -> Vec<Output> {
        let mut out = Vec::new();
        if !self.recording && self.opts.video_fps != fps {
            self.opts.video_fps = fps;
            self.push_frame_rate(&mut out);
        }
        out
    }

    fn push_frame_rate(&self, out: &mut Vec<Output>) {
        let Some((generation, lens)) = self.preview_lens() else {
            return;
        };
        if let Some(d) = self.frame_duration_for(&lens.id, Purpose::Preview) {
            out.push(Output::Backend(Command::SetControls {
                generation,
                controls: crate::backend::Controls {
                    frame_duration_us: Some(d),
                    ..Default::default()
                },
            }));
        }
    }

    /// The Pro panel opened or closed: while it's open the zoom picks the
    /// lens strictly by the ladder (no low-light stand-in), since the user
    /// is choosing the camera's settings by hand.
    pub fn set_pro(&mut self, on: bool, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        self.pro = on;
        self.apply_low_light(now, &mut out);
        out
    }

    /// The low-light lens setting.
    pub fn set_low_light(&mut self, on: bool, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        self.opts.low_light = on;
        self.apply_low_light(now, &mut out);
        out
    }

    /// Bring the back side's fallback in line with the light, the
    /// settings and the Pro panel, and switch lenses if that changes
    /// which camera streams.
    fn apply_low_light(&mut self, now: Instant, out: &mut Vec<Output>) {
        let want = self.opts.low_light && !self.pro && self.low_light.is_dim();
        let Some(back) = self.back.as_mut() else {
            return;
        };
        if back.fallback == want {
            return;
        }
        let before = back.lens();
        back.fallback = want;
        let after = back.lens();
        if self.facing == Facing::Back && before != after && !self.capture.is_busy() {
            self.request_preview(now, out);
        }
    }

    /// The camera that a recording would take its frames from: the
    /// streaming preview's, once its frames show.
    pub fn recording_source(&self) -> Option<String> {
        let (_, target) = self.switch.streaming()?;
        if target.purpose != Purpose::Preview || self.switch.is_switching() {
            return None;
        }
        Some(target.camera.clone())
    }

    /// Recording started or stopped (the app runs the encoder). While
    /// recording the lens doesn't change: zoom stays within it, and flip
    /// and photos wait.
    pub fn set_recording(&mut self, on: bool) {
        self.recording = on && self.mode == Mode::Video;
    }

    /// The lens streaming the preview, with its stream generation.
    fn preview_lens(&self) -> Option<(u64, crate::lens::Lens)> {
        let (generation, target) = self.switch.streaming()?;
        if target.purpose != Purpose::Preview || self.switch.is_switching() {
            return None;
        }
        let lens = self.table.lenses().get(target.lens).cloned()?;
        Some((generation, lens))
    }

    /// Send the current exposure settings to the streaming preview.
    fn push_settings(&self, out: &mut Vec<Output>) {
        let Some((generation, lens)) = self.preview_lens() else {
            return;
        };
        let settings = self.exposure.effective(&lens.caps);
        out.push(Output::Backend(Command::SetControls {
            generation,
            controls: crate::backend::Controls {
                af_continuous: lens.autofocus && settings.lens.is_auto() && !self.focus.is_active(),
                settings: Some(settings),
                ..Default::default()
            },
        }));
    }

    fn exposure_view(&self) -> ExposureView {
        let lens = self.side().map(|s| self.table.get(s.lens()));
        let caps = lens.map(|l| l.caps.clone()).unwrap_or_default();
        let base_iso = lens
            .map(|l| l.base_iso)
            .unwrap_or(crate::lens::DEFAULT_BASE_ISO);
        let meta = self.last_meta.as_ref().map(|(_, m)| m);
        ExposureView {
            settings: self.exposure.effective(&caps),
            pro: *self.exposure.pro(),
            ev_user: self.exposure.ev_user(),
            locked: self.exposure.is_locked(),
            base_iso,
            shutter_us: meta.and_then(|m| m.exposure_us),
            iso: meta.and_then(|m| {
                m.analog_gain.map(|g| {
                    exposure_control::iso(g as f64 * m.digital_gain.unwrap_or(1.0) as f64, base_iso)
                })
            }),
            lens_position: meta.and_then(|m| m.lens_position),
            caps,
        }
    }

    /// A tap on the drawn picture at (u, v), 0..1 from its top left, with
    /// the mean luma (0-255) of the tapped area when the app could measure
    /// it: focus there (unless focus is manual) and meter it; releases an
    /// AE/AF lock and resets the EV slider.
    pub fn tap(&mut self, u: f64, v: f64, luma: Option<f64>, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if !self.opts.tap_to_focus || self.capture.is_busy() {
            return out;
        }
        let Some((generation, lens)) = self.preview_lens() else {
            return out;
        };
        let Some(t) = self.transform(&lens.id) else {
            return out;
        };
        self.exposure.tap(luma, &lens.caps);
        let settings = self.exposure.effective(&lens.caps);
        // Spot metering meters the AfWindows rectangle: send it even when
        // focus is manual.
        let metering_window = match (settings.spot, lens.crop_maximum) {
            (true, Some(area)) => {
                let (x, y) = focus::picture_to_frame(u, v, &t);
                Some(focus::window(x, y, area))
            }
            _ => None,
        };
        // Video on a camera whose AF takes windows: continuous AF moves to
        // the tapped window and keeps following it, instead of a one-shot
        // scan that would hunt on camera and then hold.
        let follow = self.mode == Mode::Video
            && settings.lens.is_auto()
            && lens.autofocus
            && lens.af_windows
            && lens.crop_maximum.is_some();
        if follow {
            let area = lens.crop_maximum.expect("checked");
            let (x, y) = focus::picture_to_frame(u, v, &t);
            let window = focus::window(x, y, area);
            self.focus.follow(u, v, now);
            out.push(Output::Backend(Command::SetControls {
                generation,
                controls: crate::backend::Controls {
                    af_continuous: true,
                    af_window: Some(window),
                    settings: Some(settings),
                    metering_window,
                    ..Default::default()
                },
            }));
            return out;
        }
        let request = if settings.lens.is_auto() {
            self.focus.tap(
                u,
                v,
                &t,
                focus::Capability {
                    autofocus: lens.autofocus,
                    af_windows: lens.af_windows,
                    area: lens.crop_maximum,
                },
                now,
            )
        } else {
            None
        };
        out.push(Output::Backend(Command::SetControls {
            generation,
            controls: crate::backend::Controls {
                focus: request,
                settings: Some(settings),
                metering_window,
                ..Default::default()
            },
        }));
        out
    }

    /// One autofocus scan where the AF looks, keeping the exposure as it
    /// is (as recording starts).
    pub fn refocus(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        let Some((generation, lens)) = self.preview_lens() else {
            return out;
        };
        if !self.exposure.effective(&lens.caps).lens.is_auto() {
            return out;
        }
        let Some(t) = self.transform(&lens.id) else {
            return out;
        };
        let cap = focus::Capability {
            autofocus: lens.autofocus,
            af_windows: false,
            area: None,
        };
        if let Some(request) = self.focus.tap(0.5, 0.5, &t, cap, now) {
            out.push(Output::Backend(Command::SetControls {
                generation,
                controls: crate::backend::Controls {
                    focus: Some(request),
                    ..Default::default()
                },
            }));
        }
        out
    }

    /// A long press: AE/AF lock at what the camera reports now.
    pub fn lock(&mut self, now: Instant) -> Vec<Output> {
        let _ = now;
        let mut out = Vec::new();
        let Some((_, lens)) = self.preview_lens() else {
            return out;
        };
        let meta = self.last_meta.as_ref().map(|(_, m)| m.clone());
        if self.exposure.lock(meta.as_ref(), &lens.caps) {
            self.focus.reset();
            self.push_settings(&mut out);
        } else {
            out.push(Output::Error(
                "This camera doesn't offer manual exposure yet, so it can't lock".into(),
            ));
        }
        out
    }

    pub fn unlock(&mut self) -> Vec<Output> {
        let mut out = Vec::new();
        if self.exposure.is_locked() {
            self.exposure.unlock();
            self.push_settings(&mut out);
        }
        out
    }

    /// The EV slider.
    pub fn set_ev(&mut self, ev: f32) -> Vec<Output> {
        let mut out = Vec::new();
        let caps = self.exposure_view().caps;
        self.exposure.set_ev(ev, &caps);
        self.push_settings(&mut out);
        out
    }

    pub fn set_shutter(&mut self, s: Setting<i32>) -> Vec<Output> {
        self.exposure.set_shutter(s);
        let mut out = Vec::new();
        self.push_settings(&mut out);
        out
    }

    /// ISO, as the Pro panel shows it; sent as analog gain.
    pub fn set_iso(&mut self, iso: Setting<u32>) -> Vec<Output> {
        let base = self.exposure_view().base_iso;
        self.exposure.set_gain(match iso {
            Setting::Auto => Setting::Auto,
            Setting::Manual(i) => Setting::Manual(exposure_control::gain_for_iso(i, base)),
        });
        let mut out = Vec::new();
        self.push_settings(&mut out);
        out
    }

    pub fn set_white_balance(&mut self, s: Setting<Awb>) -> Vec<Output> {
        self.exposure.set_white_balance(s);
        let mut out = Vec::new();
        self.push_settings(&mut out);
        out
    }

    pub fn set_lens_position(&mut self, s: Setting<f32>) -> Vec<Output> {
        self.exposure.set_lens(s);
        self.focus.reset();
        let mut out = Vec::new();
        self.push_settings(&mut out);
        out
    }

    /// Everything back to automatic.
    pub fn reset_exposure(&mut self) -> Vec<Output> {
        self.exposure.reset();
        let mut out = Vec::new();
        self.push_settings(&mut out);
        out
    }

    /// Choose the side to open on, before [`Controller::resume`]. Ignored
    /// when that side has no camera.
    pub fn set_facing(&mut self, facing: Facing) {
        let has = match facing {
            Facing::Back => self.back.is_some(),
            Facing::Front => self.front.is_some(),
        };
        if has {
            self.facing = facing;
        }
    }

    pub fn flip(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if self.recording || self.capture.is_busy() || self.back.is_none() || self.front.is_none() {
            return out;
        }
        self.facing = match self.facing {
            Facing::Back => Facing::Front,
            Facing::Front => Facing::Back,
        };
        // Exposure doesn't carry between the two sides.
        self.last_meta = None;
        self.request_preview(now, &mut out);
        out
    }

    pub fn shutter(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if self.recording || self.switch.streaming().is_none() || self.switch.is_switching() {
            return out;
        }
        let Some(lens) = self.side().map(|s| s.lens()) else {
            return out;
        };
        let l = self.table.get(lens);
        let autofocus = l.autofocus;
        // A still mode no bigger than the preview's gains nothing from a
        // stop and a start: take the next preview frame.
        let same_mode = l.still.is_none() || l.still == l.preview;
        let mode = if same_mode {
            capture::Mode::Quick
        } else {
            self.opts.capture_mode
        };
        let meta = self.last_meta.as_ref().map(|(_, m)| m);
        let step = self.capture.begin(mode, autofocus, meta, now);
        self.capture_step(step, now, &mut out);
        out
    }

    fn capture_step(&mut self, step: Step, now: Instant, out: &mut Vec<Output>) {
        match step {
            Step::StartStill(controls) => {
                if let Some(mut target) = self.target(Purpose::Still) {
                    // The still keeps the exposure settings; the locked
                    // values are start hints.
                    target.controls.exposure_us = controls.exposure_us;
                    target.controls.analog_gain = controls.analog_gain;
                    target.controls.color_gains = controls.color_gains;
                    target.controls.lens_position = controls.lens_position;
                    target.controls.af_continuous = false;
                    self.request(Some(target), now, out);
                }
            }
            Step::Fail(msg) => {
                out.push(Output::Error(format!("Photo failed: {msg}")));
                self.restore_preview(now, out);
            }
            Step::Apply(controls) => {
                // A burst's lock: hold its exposure, gain, color and focus
                // (manual settings hold; the values in `controls` alone are
                // only start hints the AGC would walk away from).
                if let Some((generation, target)) = self.switch.streaming() {
                    let caps = self.table.get(target.lens).caps.clone();
                    let mut settings = self.exposure.effective(&caps);
                    if let Some(t) = controls.exposure_us {
                        settings.shutter = Setting::Manual(t);
                    }
                    if let Some(g) = controls.analog_gain {
                        settings.gain = Setting::Manual(g);
                    }
                    if let Some(gains) = controls.color_gains {
                        settings.white_balance = Setting::Manual(Awb::Gains(gains));
                    }
                    if let Some(p) = controls.lens_position {
                        settings.lens = Setting::Manual(p);
                    }
                    out.push(Output::Backend(Command::SetControls {
                        generation,
                        controls: crate::backend::Controls {
                            settings: Some(settings),
                            ..controls
                        },
                    }));
                }
            }
            Step::Unlock => {
                if let Some((generation, target)) = self.switch.streaming() {
                    let caps = self.table.get(target.lens).caps.clone();
                    out.push(Output::Backend(Command::SetControls {
                        generation,
                        controls: crate::backend::Controls {
                            settings: Some(self.exposure.effective(&caps)),
                            ..Default::default()
                        },
                    }));
                }
            }
            Step::Nothing
            | Step::Keep
            | Step::KeepBurst
            | Step::KeepAndRestore(_)
            | Step::Discard => {}
        }
    }

    fn restore_preview(&mut self, now: Instant, out: &mut Vec<Output>) {
        if let Some(mut target) = self.target(Purpose::Preview) {
            let locked = self.capture.locked().clone();
            target.controls.exposure_us = locked.exposure_us;
            target.controls.analog_gain = locked.analog_gain;
            self.request(Some(target), now, out);
        }
    }

    pub fn tick(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if self.capture.is_busy() {
            let meta = self.last_meta.as_ref().map(|(_, m)| m.clone());
            let step = self.capture.tick(meta.as_ref(), now);
            self.capture_step(step, now, &mut out);
        } else if self.side_mut().and_then(|s| s.planner.tick(now)).is_some() {
            self.request_preview(now, &mut out);
        }
        if self.focus.tick(now) == focus::Action::ResumeContinuous {
            if let Some((generation, target)) = self.switch.streaming() {
                let af = self
                    .table
                    .lenses()
                    .get(target.lens)
                    .is_some_and(|l| l.autofocus);
                let auto = self
                    .table
                    .lenses()
                    .get(target.lens)
                    .is_some_and(|l| self.exposure.effective(&l.caps).lens.is_auto());
                if af && auto && target.purpose == Purpose::Preview {
                    out.push(Output::Backend(Command::SetControls {
                        generation,
                        controls: crate::backend::Controls {
                            af_continuous: true,
                            ..Default::default()
                        },
                    }));
                }
            }
        }
        if self.retry_at.is_some_and(|t| now >= t) {
            self.retry_at = None;
            self.request_preview(now, &mut out);
        }
        out
    }

    fn picture(&self, frame: Frame, lens_idx: usize, purpose: Purpose) -> Output {
        let lens = self.table.get(lens_idx).clone();
        let zoom = self.side().map(|s| s.planner.zoom()).unwrap_or(1.0);
        let transform = self.transform_for(&lens.id, purpose).unwrap_or_default();
        Output::Picture(Box::new(Picture {
            transform,
            crop: (zoom / lens.zoom).max(1.0),
            rotation: lens.upright_clockwise(),
            zoom,
            settings: self.exposure.effective(&lens.caps),
            lens,
            frame,
        }))
    }

    /// Judge the light from a back lens's preview frame. Not while
    /// recording (the lens doesn't change under a recording for the
    /// light) or taking a photo.
    fn judge_light(
        &mut self,
        lens_idx: usize,
        meta: &FrameMeta,
        now: Instant,
        out: &mut Vec<Output>,
    ) {
        if self.facing != Facing::Back
            || self.recording
            || self.capture.is_busy()
            || self.switch.is_switching()
        {
            return;
        }
        let Some(tele) = self.table.by_role(Role::Tele).map(|i| self.table.get(i)) else {
            return;
        };
        let lens = self.table.get(lens_idx);
        let (Some(f), Some(f_tele)) = (lens.f_number, tele.f_number) else {
            return;
        };
        let demand = lowlight::tele_demand(meta, f, f_tele);
        let max_gain = tele.caps.gain.map(|(_, hi)| hi as f64);
        if self.low_light.update(demand, max_gain, now).is_some() {
            tracing::info!(dim = self.low_light.is_dim(), "low light");
            self.apply_low_light(now, out);
        }
    }

    pub fn on_event(&mut self, event: Event, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        match event {
            Event::Cameras(_) => {}
            Event::Notice { message } => out.push(Output::Error(message)),
            Event::Started {
                generation,
                ref camera,
                max_fps,
                ..
            } => {
                match max_fps {
                    Some(f) => {
                        self.max_fps.insert(camera.clone(), f);
                    }
                    None => {
                        self.max_fps.remove(camera);
                    }
                }
                self.focus.reset();
                let actions = self.switch.on_started(generation);
                self.run(actions, &mut out);
            }
            Event::Stopped { generation } => {
                let actions = self.switch.on_stopped(generation);
                self.run(actions, &mut out);
            }
            Event::Error {
                generation,
                message,
            } => {
                let actions = self.switch.on_error(generation);
                self.run(actions, &mut out);
                self.capture.abort();
                out.push(Output::Error(message));
                if !self.suspended && self.switch.wanted().is_none() {
                    self.retry_at = Some(now + RETRY_AFTER);
                }
            }
            Event::Frame(frame) => self.on_frame(frame, now, &mut out),
        }
        out
    }

    fn on_frame(&mut self, frame: Frame, now: Instant, out: &mut Vec<Output>) {
        if self.switch.on_frame(frame.generation, now) == FrameKind::Stale {
            return;
        }
        let Some((_, target)) = self.switch.streaming() else {
            return;
        };
        let (lens_idx, purpose) = (target.lens, target.purpose);
        if lens_idx >= self.table.lenses().len() {
            return; // its camera is gone
        }
        match purpose {
            Purpose::Preview => {
                self.focus.on_meta(&frame.meta, now);
                self.judge_light(lens_idx, &frame.meta, now, out);
                self.last_meta = Some((lens_idx, frame.meta.clone()));
                match self.capture.on_preview_frame(&frame.meta, now) {
                    Step::Keep => out.push(self.picture(frame, lens_idx, purpose)),
                    step => {
                        self.capture_step(step, now, out);
                        out.push(Output::Show(frame));
                    }
                }
            }
            Purpose::Still => match self.capture.on_still_frame(&frame.meta, now) {
                Step::KeepAndRestore(_) => {
                    if let Some(r) = self.capture.last_still() {
                        let m = &r.meta;
                        let lens = &self.table.get(lens_idx).name;
                        tracing::info!(
                            "still from {lens}: frame {} after {} ms ({}), exposure {} us, \
                             analog gain {}, digital gain {}, AE {}",
                            r.frames,
                            r.elapsed.as_millis(),
                            if r.settled { "settled" } else { "wait ran out" },
                            opt(m.exposure_us),
                            opt(m.analog_gain.map(|g| format!("{g:.2}"))),
                            opt(m.digital_gain.map(|g| format!("{g:.2}"))),
                            opt(m.ae_state.map(|s| format!("{s:?}"))),
                        );
                    }
                    if self.capture.burst().is_some() {
                        out.push(burst_frame(self.picture(frame, lens_idx, purpose)));
                        out.push(Output::BurstDone);
                    } else {
                        out.push(self.picture(frame, lens_idx, purpose));
                    }
                    self.restore_preview(now, out);
                }
                Step::KeepBurst => out.push(burst_frame(self.picture(frame, lens_idx, purpose))),
                Step::Discard => {}
                step => self.capture_step(step, now, out),
            },
        }
    }
}

/// A picture output as one frame of a burst.
fn burst_frame(o: Output) -> Output {
    match o {
        Output::Picture(p) => Output::BurstFrame(p),
        other => other,
    }
}

/// A value for a log line, or "-".
fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{FrameData, Location, PixelLayout, Size};
    use crate::lens::TableSpec;

    fn cameras() -> Vec<CameraInfo> {
        let cam = |id: &str, location, rotation| CameraInfo {
            id: id.into(),
            location,
            rotation,
            model: "sensor".into(),
            autofocus: true,
            ..CameraInfo::default()
        };
        vec![
            cam("main", Location::Back, 90),
            cam("uw", Location::Back, 270),
            cam("tele", Location::Back, 90),
            cam("front", Location::Front, 90),
        ]
    }

    fn spec() -> TableSpec {
        TableSpec::parse(
            r#"
max_zoom = 15.0
[[lens]]
role = "main"
id = "main"
zoom = 1.0
f_number = 1.85
preview = [2016, 1136]
still = [4080, 3072]
autofocus = true
[[lens]]
role = "ultrawide"
id = "uw"
zoom = 0.45
f_number = 2.2
preview = [2016, 1508]
[[lens]]
role = "tele"
id = "tele"
zoom = 4.78
f_number = 3.5
preview = [2016, 1512]
[[lens]]
role = "front"
id = "front"
zoom = 1.0
mirror = true
"#,
        )
        .unwrap()
    }

    fn commands(out: &[Output]) -> Vec<&Command> {
        out.iter()
            .filter_map(|o| match o {
                Output::Backend(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn start_of(out: &[Output]) -> (u64, String, Purpose) {
        match commands(out).as_slice() {
            [Command::Start {
                generation,
                camera,
                stream,
                ..
            }] => (*generation, camera.clone(), stream.purpose),
            other => panic!("expected one start, got {other:?}"),
        }
    }

    fn frame(generation: u64, camera: &str, exposure: i32) -> Event {
        Event::Frame(Frame {
            generation,
            camera: camera.into(),
            size: Size::new(4, 2),
            stride: 16,
            layout: PixelLayout::Xrgb8888,
            data: FrameData::Memory(vec![0; 32]),
            meta: FrameMeta {
                exposure_us: Some(exposure),
                analog_gain: Some(1.0),
                ..FrameMeta::default()
            },
        })
    }

    fn started(generation: u64, camera: &str) -> Event {
        Event::Started {
            generation,
            camera: camera.into(),
            size: Size::new(4, 2),
            stride: 16,
            layout: PixelLayout::Xrgb8888,
            max_fps: None,
        }
    }

    /// Opens the app on the main camera and returns its generation.
    fn open(c: &mut Controller, now: Instant) -> u64 {
        let (g, cam, _) = start_of(&c.resume(now));
        assert_eq!(cam, "main");
        c.on_event(started(g, "main"), now);
        c.on_event(frame(g, "main", 10_000), now);
        g
    }

    #[test]
    fn opens_on_the_main_camera_at_1x() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        let out = c.resume(now);
        match commands(&out).as_slice() {
            [Command::Start {
                camera,
                stream,
                controls,
                ..
            }] => {
                assert_eq!(camera, "main");
                assert_eq!(stream.size, Some(Size::new(2016, 1136)));
                assert_eq!(stream.buffers, 4);
                assert!(controls.af_continuous);
            }
            other => panic!("{other:?}"),
        }
        let v = c.view();
        assert_eq!(v.zoom, 1.0);
        assert_eq!(
            v.presets,
            vec![0.45, 1.0, 2.0, 5.0],
            "the .5 button is the full ultrawide"
        );
        assert!(v.can_flip);
    }

    #[test]
    fn preset_5x_switches_to_the_tele_with_exposure_carried() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        let g = open(&mut c, now);
        let out = c.set_zoom(5.0, ZoomSource::Preset, now);
        match commands(&out).as_slice() {
            [Command::Start {
                generation,
                camera,
                controls,
                ..
            }] => {
                assert!(*generation > g);
                assert_eq!(camera, "tele");
                // 10 ms at f/1.85 needs 35.8 ms at f/3.5: time capped at
                // 33.3 ms, the rest in gain.
                assert_eq!(controls.exposure_us, Some(33_333));
                assert!(controls.analog_gain.unwrap() > 1.0);
                assert!(!controls.af_continuous, "the tele has no AF");
            }
            other => panic!("{other:?}"),
        }
        assert!(c.view().switching);
        // The main's frames still arriving are dropped.
        assert!(c.on_event(frame(g, "main", 10_000), now).is_empty());
        let t = c.transform("tele").unwrap();
        assert!((t.scale - 5.0 / 4.78).abs() < 1e-9);
        // The frozen main frame is drawn at 5x meanwhile.
        assert_eq!(c.transform("main").unwrap().scale, 5.0);
    }

    #[test]
    fn a_pinch_switches_after_it_settles() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        assert!(commands(&c.set_zoom(0.6, ZoomSource::Gesture, now)).is_empty());
        assert!(c.next_tick(now).is_some());
        assert!(commands(&c.tick(now + Duration::from_millis(50))).is_empty());
        let (_, cam, _) = start_of(&c.tick(now + Duration::from_millis(160)));
        assert_eq!(cam, "uw");
    }

    #[test]
    fn transforms_turn_each_camera_upright() {
        // main 90, uw 270, tele 90 (this test's table), front 90: turn
        // counter-clockwise by Rotation, so clockwise by 360 - Rotation.
        let c = Controller::new(&spec(), &cameras(), Options::default());
        assert_eq!(c.transform("main").unwrap().rotation, 270);
        assert_eq!(c.transform("uw").unwrap().rotation, 90);
        assert_eq!(c.transform("tele").unwrap().rotation, 270);
        let front = c.transform("front").unwrap();
        assert_eq!((front.rotation, front.mirror), (270, true));
    }

    #[test]
    fn frames_are_never_drawn_below_their_own_field_of_view() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        let mut z = 0.45;
        while z <= 10.0 {
            c.set_zoom(z, ZoomSource::Gesture, now);
            for cam in ["main", "uw", "tele"] {
                let t = c.transform(cam).unwrap();
                assert!(t.scale >= 1.0, "{cam} at {z}: scale {}", t.scale);
            }
            // The lens that should be streaming crops exactly zoom/factor.
            let lens = c.table().get(c.side().unwrap().lens());
            let t = c.transform(&lens.id).unwrap();
            assert!((t.scale - (z / lens.zoom).max(1.0)).abs() < 1e-9);
            z += 0.01;
        }
        // The frozen main frame while the ultrawide starts for 0.6x: full
        // view, not shrunk.
        c.set_zoom(0.6, ZoomSource::Preset, now);
        assert_eq!(c.transform("main").unwrap().scale, 1.0);
        assert!((c.transform("uw").unwrap().scale - 0.6 / 0.45).abs() < 1e-9);
    }

    #[test]
    fn a_still_in_the_preview_mode_reuses_the_running_stream() {
        // The ultrawide's still mode is its preview mode (the full one
        // waits for the kernel): no stop, no start, the next frame.
        let now = Instant::now();
        let spec = TableSpec::parse(
            "[[lens]]\nrole = \"main\"\nid = \"main\"\nzoom = 1.0\npreview = [2016, 1508]\nstill = [2016, 1508]\n",
        )
        .unwrap();
        let mut c = Controller::new(&spec, &cameras(), Options::default());
        let g = open(&mut c, now);
        assert!(commands(&c.shutter(now)).is_empty(), "no restart");
        match c.on_event(frame(g, "main", 10_000), now).as_slice() {
            [Output::Picture(p)] => assert_eq!(p.crop, 1.0),
            other => panic!("{other:?}"),
        }
        assert!(!c.view().capturing);
    }

    #[test]
    fn the_half_button_shows_the_whole_ultrawide() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        let half = c.view().presets[0];
        c.set_zoom(half, ZoomSource::Preset, now);
        assert_eq!(c.transform("uw").unwrap().scale, 1.0);
    }

    #[test]
    fn a_tap_focuses_the_streaming_lens_and_continuous_af_returns() {
        let now = Instant::now();
        let mut cams = cameras();
        for c in cams.iter_mut() {
            c.af_windows = true;
            c.crop_maximum = Some(crate::backend::Rect {
                x: 0,
                y: 0,
                width: 4080,
                height: 3072,
            });
        }
        let mut c = Controller::new(&spec(), &cams, Options::default());
        let g = open(&mut c, now);
        match commands(&c.tap(0.25, 0.25, None, now)).as_slice() {
            [Command::SetControls {
                generation,
                controls,
            }] => {
                assert_eq!(*generation, g);
                assert!(controls.focus.unwrap().window.is_some());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(c.view().focus.unwrap().status, focus::Status::Scanning);
        let later = now + focus::SCAN_TIMEOUT + focus::NOTICE;
        c.tick(now + focus::SCAN_TIMEOUT);
        let out = c.tick(later);
        assert!(matches!(
            commands(&out).as_slice(),
            [Command::SetControls { controls, .. }] if controls.af_continuous
        ));
    }

    #[test]
    fn taps_on_a_fixed_focus_lens_meter_only_and_off_send_nothing() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        c.set_zoom(5.0, ZoomSource::Preset, now);
        let (g, _, _) = (2, "tele", Purpose::Preview);
        c.on_event(started(g, "tele"), now);
        c.on_event(frame(g, "tele", 10_000), now);
        match commands(&c.tap(0.5, 0.5, None, now)).as_slice() {
            [Command::SetControls { controls, .. }] => assert!(controls.focus.is_none()),
            other => panic!("{other:?}"),
        }
        assert_eq!(c.view().focus.unwrap().status, focus::Status::FixedFocus);
        c.set_tap_to_focus(false);
        c.set_zoom(1.0, ZoomSource::Preset, now);
        assert!(c.tap(0.5, 0.5, None, now).is_empty());
    }

    #[test]
    fn cameras_that_appear_late_start_and_a_removed_one_stops() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &[], Options::default());
        assert!(!c.has_cameras());
        assert!(commands(&c.resume(now)).is_empty(), "nothing to start yet");
        assert!(c.is_running());
        // The drivers load: the main camera starts.
        let (g, cam, _) = start_of(&c.set_cameras(&spec(), &cameras(), now));
        assert_eq!(cam, "main");
        c.on_event(started(g, "main"), now);
        c.on_event(frame(g, "main", 10_000), now);
        // The main camera goes away: its stream stops; the ultrawide is
        // what's left at 1x on the back.
        let mut left = cameras();
        left.retain(|c| c.id != "main");
        // A start replaces the old stream; generations carry on.
        let (g2, cam, _) = start_of(&c.set_cameras(&spec(), &left, now));
        assert!(g2 > g, "generations carry on");
        assert_eq!(cam, "uw");
        // The backend reports the main stream stopped (its camera went):
        // nothing more to do.
        assert!(c.on_event(Event::Stopped { generation: g }, now).is_empty());
        // All gone: stopped, nothing to start, still running (waiting).
        c.on_event(started(g2, "uw"), now);
        let out = c.set_cameras(&spec(), &[], now);
        assert_eq!(commands(&out), vec![&Command::Stop { generation: g2 }]);
        assert!(!c.has_cameras());
        assert!(c
            .on_event(Event::Stopped { generation: g2 }, now)
            .is_empty());
    }

    #[test]
    fn a_new_camera_leaves_the_streaming_one_alone() {
        let now = Instant::now();
        let mut cams = cameras();
        let tele = cams.pop().unwrap(); // front
        let front = tele;
        let mut c = Controller::new(&spec(), &cams, Options::default());
        let g = open(&mut c, now);
        cams.push(front);
        assert!(
            commands(&c.set_cameras(&spec(), &cams, now)).is_empty(),
            "main keeps streaming"
        );
        assert_eq!(
            c.on_event(frame(g, "main", 10_000), now).len(),
            1,
            "its frames still show"
        );
    }

    #[test]
    fn late_cameras_while_suspended_wait_for_resume() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &[], Options::default());
        assert!(commands(&c.set_cameras(&spec(), &cameras(), now)).is_empty());
        let (_, cam, _) = start_of(&c.resume(now));
        assert_eq!(cam, "main");
    }

    #[test]
    fn while_recording_lenses_switch_but_flip_and_photos_wait() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        assert_eq!(c.recording_source().as_deref(), Some("main"));
        c.set_recording(true);
        assert!(!c.view().recording, "only in video mode");
        c.set_mode(Mode::Video);
        c.set_recording(true);
        assert!(c.view().recording);
        c.set_mode(Mode::Photo);
        assert_eq!(c.view().mode, Mode::Video, "no mode change mid-recording");
        // The zoom picks lenses as ever: the recorder follows the camera.
        let (g, cam, _) = start_of(&c.set_zoom(6.0, ZoomSource::Preset, now));
        assert_eq!(cam, "tele");
        c.on_event(started(g, "tele"), now);
        c.on_event(frame(g, "tele", 33_000), now);
        let (_, cam, _) = start_of(&c.set_zoom(0.5, ZoomSource::Preset, now));
        assert_eq!(cam, "uw");
        assert!(c.flip(now).is_empty());
        assert!(c.shutter(now).is_empty());
        c.set_recording(false);
    }

    /// A frame whose exposure says the scene is too dim for the tele: on
    /// the main at 1/30 s and gain 6, the tele would need gain 21.
    fn dim_frame(generation: u64, camera: &str) -> Event {
        let mut e = frame(generation, camera, 33_333);
        if let Event::Frame(f) = &mut e {
            f.meta.analog_gain = Some(6.0);
        }
        e
    }

    #[test]
    fn in_low_light_the_main_camera_stands_in_for_the_tele() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        let g = open(&mut c, t0);
        // Dim, on the main at 1x: judged, but nothing to switch.
        for n in (0..=1600).step_by(100) {
            assert!(commands(&c.on_event(dim_frame(g, "main"), ms(n))).is_empty());
        }
        assert!(!c.view().low_light);
        // 5x in the dark: the main crops 5x instead of starting the tele.
        let out = c.set_zoom(5.0, ZoomSource::Preset, ms(1700));
        assert!(commands(&out).is_empty(), "{out:?}");
        let v = c.view();
        assert!(v.low_light);
        assert_eq!(v.lens_role, Some(Role::Main));
        assert_eq!(c.transform("main").unwrap().scale, 5.0);
        // Bright again for three seconds: back to the tele.
        let mut started = None;
        for n in (1800..=5000).step_by(100) {
            let out = c.on_event(frame(g, "main", 5_000), ms(n));
            if let [Command::Start { camera, .. }] = commands(&out).as_slice() {
                started = Some((n, camera.clone()));
                break;
            }
        }
        let (n, cam) = started.expect("switched back");
        assert_eq!(cam, "tele");
        assert!(n >= 4800, "after the dwell, at {n} ms");
        assert!(!c.view().low_light);
    }

    #[test]
    fn low_light_on_the_tele_falls_back_and_pro_or_the_setting_prevent_it() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, t0);
        let (g, cam, _) = start_of(&c.set_zoom(5.0, ZoomSource::Preset, t0));
        assert_eq!(cam, "tele");
        c.on_event(started(g, "tele"), t0);
        // The tele at 1/30 s and gain 12.
        let tele_dim = |n| {
            let mut e = frame(g, "tele", 33_333);
            if let Event::Frame(f) = &mut e {
                f.meta.analog_gain = Some(12.0);
                f.meta.sequence = n;
            }
            e
        };
        let mut switched = None;
        for n in 0..30u64 {
            let out = c.on_event(tele_dim(n as u32), ms(n * 100));
            if let [Command::Start {
                camera, generation, ..
            }] = commands(&out).as_slice()
            {
                switched = Some((n * 100, camera.clone(), *generation));
                break;
            }
        }
        let (at, cam, g) = switched.expect("fell back");
        assert_eq!(cam, "main");
        assert!(at >= 1500, "{at}");
        c.on_event(started(g, "main"), ms(3500));
        let settle = |c: &mut Controller, out: Vec<Output>| {
            let (g, cam, _) = start_of(&out);
            c.on_event(started(g, &cam), ms(4000));
            cam
        };
        // The Pro panel puts the tele back, and keeps it while open.
        let out = c.set_pro(true, ms(4000));
        assert_eq!(settle(&mut c, out), "tele");
        let out = c.set_pro(false, ms(4100));
        assert_eq!(settle(&mut c, out), "main");
        // So does turning the setting off.
        let out = c.set_low_light(false, ms(4200));
        assert_eq!(settle(&mut c, out), "tele");
    }

    #[test]
    fn the_ultrawide_has_no_low_light_fallback() {
        let t0 = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        let g = open(&mut c, t0);
        for n in 0..30u64 {
            c.on_event(dim_frame(g, "main"), t0 + Duration::from_millis(n * 100));
        }
        let (_, cam, _) = start_of(&c.set_zoom(0.5, ZoomSource::Preset, t0));
        assert_eq!(cam, "uw");
        assert!(!c.view().low_light);
    }

    #[test]
    fn sixty_fps_only_where_the_mode_allows_and_only_for_video() {
        let now = Instant::now();
        let mut c = Controller::new(
            &spec(),
            &cameras(),
            Options {
                video_fps: 60,
                ..Options::default()
            },
        );
        let (g, _, _) = start_of(&c.resume(now));
        // No frame durations reported: nothing asked, 60 not offered.
        c.on_event(started(g, "main"), now);
        c.on_event(frame(g, "main", 10_000), now);
        assert!(!c.view().can_60fps);
        assert!(commands(&c.set_mode(Mode::Video)).is_empty());
        c.set_mode(Mode::Photo);
        // A mode that reaches 60 fps.
        let (g, _, _) = start_of(&c.set_zoom(0.5, ZoomSource::Preset, now));
        let mut e = started(g, "uw");
        if let Event::Started { max_fps, .. } = &mut e {
            *max_fps = Some(60.0);
        }
        c.on_event(e, now);
        c.on_event(frame(g, "uw", 10_000), now);
        assert!(c.view().can_60fps);
        let duration = |out: &[Output]| match commands(out).as_slice() {
            [Command::SetControls { controls, .. }] => controls.frame_duration_us,
            other => panic!("{other:?}"),
        };
        assert_eq!(duration(&c.set_mode(Mode::Video)), Some(16_667));
        assert_eq!(duration(&c.set_video_fps(30)), Some(33_333));
        assert_eq!(duration(&c.set_video_fps(60)), Some(16_667));
        assert_eq!(duration(&c.set_mode(Mode::Photo)), Some(33_333));
    }

    #[test]
    fn a_video_tap_moves_continuous_af_to_the_window() {
        let now = Instant::now();
        let mut cams = cameras();
        for c in cams.iter_mut() {
            c.af_windows = true;
            c.crop_maximum = Some(crate::backend::Rect {
                x: 0,
                y: 0,
                width: 4080,
                height: 3072,
            });
        }
        let mut c = Controller::new(&spec(), &cams, Options::default());
        open(&mut c, now);
        c.set_mode(Mode::Video);
        match commands(&c.tap(0.5, 0.5, None, now)).as_slice() {
            [Command::SetControls { controls, .. }] => {
                assert!(controls.af_continuous);
                assert!(controls.af_window.is_some());
                assert!(controls.focus.is_none(), "no one-shot scan");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            c.view().focus.map(|f| f.status),
            Some(focus::Status::Following)
        );
    }

    #[test]
    fn every_lens_frames_the_view_alike() {
        // With axes off center, at each switch the old and the new lens
        // look at the same point of the scene.
        let spec = TableSpec::parse(
            r#"
max_zoom = 15.0
[[lens]]
role = "main"
id = "main"
zoom = 1.0
[[lens]]
role = "ultrawide"
id = "uw"
zoom = 0.45
axis = [0.47, 0.5]
[[lens]]
role = "tele"
id = "tele"
zoom = 4.78
axis = [0.56, 0.48]
"#,
        )
        .unwrap();
        let now = Instant::now();
        let mut c = Controller::new(&spec, &cameras(), Options::default());
        // Where the view's center is in the main camera's picture, seen
        // from each lens's frame.
        let in_main = |c: &Controller, cam: &str, zoom: f64, axis: (f64, f64)| {
            let t = c.transform(cam).unwrap();
            (
                axis.0 + (t.center.0 - 0.5) / zoom,
                axis.1 + (t.center.1 - 0.5) / zoom,
            )
        };
        for z in [0.99, 1.0, 4.77, 4.78] {
            c.set_zoom(z, ZoomSource::Preset, now);
            let a = in_main(&c, "main", 1.0, (0.5, 0.5));
            let b = if z < 1.0 {
                in_main(&c, "uw", 0.45, (0.47, 0.5))
            } else {
                in_main(&c, "tele", 4.78, (0.56, 0.48))
            };
            if (1.0..4.78).contains(&z) {
                continue; // the tele can't show below its own factor
            }
            assert!(
                (a.0 - b.0).abs() < 2e-3 && (a.1 - b.1).abs() < 2e-3,
                "{z}: {a:?} {b:?}"
            );
        }
    }

    fn caps_all() -> crate::backend::Caps {
        crate::backend::Caps {
            ev: Some((-8.0, 8.0)),
            exposure_time: Some((100, 66_000)),
            gain: Some((1.0, 16.0)),
            manual_exposure: true,
            awb_modes: true,
            color_temperature: Some((2500, 7500)),
            lens_position: Some((0.0, 1.0)),
            ..Default::default()
        }
    }

    fn settings_of(out: &[Output]) -> crate::backend::ExposureSettings {
        match commands(out).as_slice() {
            [Command::SetControls { controls, .. }] => controls.settings.expect("settings"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_tap_meters_and_the_slider_adds_and_a_lens_switch_resets() {
        let now = Instant::now();
        let mut cams = cameras();
        for c in cams.iter_mut() {
            c.caps = caps_all();
        }
        let mut c = Controller::new(&spec(), &cams, Options::default());
        open(&mut c, now);
        // A dark area: +2 EV.
        let s = settings_of(&c.tap(0.5, 0.5, Some(29.5), now));
        assert!((s.ev - 2.0).abs() < 1e-3);
        let s = settings_of(&c.set_ev(-1.0));
        assert!((s.ev - 1.0).abs() < 1e-3);
        assert_eq!(c.view().exposure.ev_user, -1.0);
        // Switching to the tele: a fresh start, EV back to 0.
        match commands(&c.set_zoom(5.0, ZoomSource::Preset, now)).as_slice() {
            [Command::Start { controls, .. }] => {
                assert_eq!(controls.settings.unwrap().ev, 0.0);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(c.view().exposure.ev_user, 0.0);
    }

    #[test]
    fn lock_holds_what_the_camera_reported_and_a_tap_releases_it() {
        let now = Instant::now();
        let mut cams = cameras();
        for c in cams.iter_mut() {
            c.caps = caps_all();
        }
        let mut c = Controller::new(&spec(), &cams, Options::default());
        open(&mut c, now); // its frame reported 10 ms at gain 1
        let s = settings_of(&c.lock(now));
        assert_eq!(s.shutter, Setting::Manual(10_000));
        assert_eq!(s.gain, Setting::Manual(1.0));
        assert!(c.view().exposure.locked);
        let s = settings_of(&c.tap(0.5, 0.5, None, now));
        assert_eq!(s.shutter, Setting::Auto);
        assert!(!c.view().exposure.locked);
    }

    #[test]
    fn without_the_controls_lock_explains_and_pro_values_stay_auto() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        assert!(matches!(c.lock(now).as_slice(), [Output::Error(_)]));
        let s = settings_of(&c.set_shutter(Setting::Manual(4_000)));
        assert_eq!(
            s.shutter,
            Setting::Auto,
            "libcamera 4.8 offers no manual exposure"
        );
        assert_eq!(
            c.view().exposure.pro.shutter,
            Setting::Manual(4_000),
            "kept for when it does"
        );
    }

    #[test]
    fn pro_values_reach_stills_and_keep_across_lenses() {
        let now = Instant::now();
        let mut cams = cameras();
        for c in cams.iter_mut() {
            c.caps = caps_all();
        }
        let mut c = Controller::new(&spec(), &cams, Options::default());
        open(&mut c, now);
        c.set_shutter(Setting::Manual(4_000));
        let s = settings_of(&c.set_iso(Setting::Manual(400)));
        assert_eq!(s.gain, Setting::Manual(8.0), "ISO 400 at base 50");
        let s = settings_of(&c.set_lens_position(Setting::Manual(0.3)));
        assert_eq!(s.lens, Setting::Manual(0.3));
        match commands(&c.shutter(now)).as_slice() {
            [Command::Start {
                stream, controls, ..
            }] => {
                assert_eq!(stream.purpose, Purpose::Still);
                let s = controls.settings.unwrap();
                assert_eq!(s.shutter, Setting::Manual(4_000));
                assert_eq!(s.lens, Setting::Manual(0.3));
                assert!(!controls.af_continuous);
            }
            other => panic!("{other:?}"),
        }
        let mut c = Controller::new(&spec(), &cams, Options::default());
        open(&mut c, now);
        c.set_shutter(Setting::Manual(4_000));
        match commands(&c.set_zoom(0.5, ZoomSource::Preset, now)).as_slice() {
            [Command::Start { controls, .. }] => {
                assert_eq!(controls.settings.unwrap().shutter, Setting::Manual(4_000));
            }
            other => panic!("{other:?}"),
        }
        let s = settings_of(&{
            let g = 2;
            c.on_event(started(g, "uw"), now);
            c.on_event(frame(g, "uw", 4_000), now);
            c.reset_exposure()
        });
        assert_eq!(s, crate::backend::ExposureSettings::default());
    }

    #[test]
    fn flip_goes_to_the_front_camera_and_back() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        let (g, cam, _) = start_of(&c.flip(now));
        assert_eq!(cam, "front");
        assert_eq!(c.view().presets, vec![1.0, 2.0]);
        c.on_event(started(g, "front"), now);
        c.on_event(frame(g, "front", 5_000), now);
        let t = c.transform("front").unwrap();
        assert!(t.mirror);
        let (_, cam, _) = start_of(&c.flip(now));
        assert_eq!(cam, "main");
    }

    #[test]
    fn full_still_capture_reconfigures_and_comes_back() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        let out = c.shutter(now);
        let (gs, cam, purpose) = start_of(&out);
        assert_eq!((cam.as_str(), purpose), ("main", Purpose::Still));
        match commands(&out)[0] {
            Command::Start { stream, .. } => {
                assert_eq!(stream.size, Some(Size::new(4080, 3072)));
                assert_eq!(stream.buffers, 2);
                assert_eq!(stream.fallbacks, vec![Size::new(2016, 1136)]);
            }
            _ => unreachable!(),
        }
        assert!(c.view().capturing);
        c.on_event(started(gs, "main"), now);
        // No AeState in these frames: a fixed wait (frames and time; the
        // full mode at 5 fps), then one is kept.
        let at = |i: u32| now + Duration::from_millis(200 * i as u64);
        for i in 0..capture::FIXED_DISCARD {
            assert!(c.on_event(frame(gs, "main", 10_000), at(i)).is_empty());
        }
        let out = c.on_event(frame(gs, "main", 10_000), at(capture::FIXED_DISCARD));
        assert!(matches!(out[0], Output::Picture(_)));
        let (gp, _, purpose) = start_of(&out[1..]);
        assert_eq!(purpose, Purpose::Preview);
        c.on_event(started(gp, "main"), now);
        let out = c.on_event(frame(gp, "main", 10_000), now);
        assert!(matches!(out[0], Output::Show(_)));
        assert!(!c.view().capturing);
    }

    #[test]
    fn quick_capture_takes_the_next_preview_frame_with_the_zoom_crop() {
        let now = Instant::now();
        let opts = Options {
            capture_mode: capture::Mode::Quick,
            ..Options::default()
        };
        let mut c = Controller::new(&spec(), &cameras(), opts);
        let g = open(&mut c, now);
        c.set_zoom(2.0, ZoomSource::Preset, now);
        assert!(commands(&c.shutter(now)).is_empty());
        match c.on_event(frame(g, "main", 10_000), now).as_slice() {
            [Output::Picture(p)] => {
                assert_eq!(p.crop, 2.0);
                assert_eq!(p.rotation, 270, "main: Rotation 90 is 270 clockwise");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_busy_camera_is_retried() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        let (g, _, _) = start_of(&c.resume(now));
        let out = c.on_event(
            Event::Error {
                generation: Some(g),
                message: "busy".into(),
            },
            now,
        );
        assert!(matches!(out.last(), Some(Output::Error(_))));
        assert!(commands(&c.tick(now + Duration::from_secs(1))).is_empty());
        let (g2, cam, _) = start_of(&c.tick(now + RETRY_AFTER));
        assert!(g2 > g);
        assert_eq!(cam, "main");
    }

    #[test]
    fn suspend_stops_and_releases_and_resume_restarts() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        let g = open(&mut c, now);
        let out = c.suspend(now);
        assert_eq!(
            commands(&out),
            vec![&Command::Stop { generation: g }, &Command::Release]
        );
        // Zoom while suspended changes the zoom but starts nothing.
        assert!(commands(&c.set_zoom(5.0, ZoomSource::Preset, now)).is_empty());
        c.on_event(Event::Stopped { generation: g }, now);
        let (_, cam, _) = start_of(&c.resume(now));
        assert_eq!(cam, "tele");
    }

    #[test]
    fn a_phone_without_a_tele_zooms_the_main_digitally() {
        let now = Instant::now();
        let mut cams = cameras();
        cams.retain(|c| c.id != "tele");
        let mut c = Controller::new(&spec(), &cams, Options::default());
        open(&mut c, now);
        assert!(commands(&c.set_zoom(5.0, ZoomSource::Preset, now)).is_empty());
        assert_eq!(c.transform("main").unwrap().scale, 5.0);
    }
}
