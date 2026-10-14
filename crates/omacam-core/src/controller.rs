//! The controller: user input and backend events in, backend commands and
//! things to show out. The UI owns one and does what it returns.

use std::time::{Duration, Instant};

use crate::backend::{CameraInfo, Command, Event, Frame, FrameMeta, Purpose, StreamRequest};
use crate::capture::{self, CaptureController, Step};
use crate::exposure;
use crate::focus::{self, FocusController};
use crate::lens::{Facing, Lens, LensTable, Role, TableSpec};
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
}

impl Default for Options {
    fn default() -> Self {
        Options {
            capture_mode: capture::Mode::Full,
            preview_buffers: 4,
            still_buffers: 2,
            transfer_exposure: true,
            tap_to_focus: true,
        }
    }
}

/// A captured frame to encode and save.
#[derive(Debug)]
pub struct Picture {
    pub frame: Frame,
    /// Centre crop: keep 1/crop of each side (1 = the whole frame).
    pub crop: f64,
    /// Clockwise degrees that turn the frame upright.
    pub rotation: u32,
    pub lens: Lens,
    pub zoom: f64,
}

/// What the UI should do.
#[derive(Debug)]
pub enum Output {
    Backend(Command),
    /// A new frame for the viewfinder.
    Show(Frame),
    Picture(Picture),
    /// Something to tell the user.
    Error(String),
}

/// How to draw a frame of one lens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// Magnification of the frame about its centre (the zoom crop), never
    /// below 1.
    pub scale: f64,
    /// Clockwise degrees that turn the frame upright.
    pub rotation: u32,
    /// Mirror (left-right) after turning upright: the selfie preview.
    pub mirror: bool,
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
    /// The focus ring, after a tap.
    pub focus: Option<focus::Indicator>,
}

struct Side {
    planner: ZoomPlanner,
    /// Ladder step to lens-table index.
    lenses: Vec<usize>,
}

impl Side {
    fn new(table: &LensTable, facing: Facing) -> Option<Side> {
        let lenses = table.ladder(facing);
        if lenses.is_empty() {
            return None;
        }
        let factors = lenses.iter().map(|&i| table.get(i).zoom).collect();
        let ladder = Ladder::new(factors, table.max_zoom(facing));
        // Open at 1x, or the widest lens when nothing reaches 1x.
        Some(Side {
            planner: ZoomPlanner::new(ladder, 1.0),
            lenses,
        })
    }

    fn lens(&self) -> usize {
        self.lenses[self.planner.target()]
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
    last_meta: Option<(usize, FrameMeta)>,
    suspended: bool,
    retry_at: Option<Instant>,
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
            last_meta: None,
            suspended: true,
            retry_at: None,
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
            focus: self.focus.indicator(),
        }
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

    /// How to draw a frame from `camera` at the current zoom.
    pub fn transform(&self, camera: &str) -> Option<Transform> {
        let i = self.table.index_of(camera)?;
        let lens = self.table.get(i);
        let zoom = self.side().map(|s| s.planner.zoom()).unwrap_or(1.0);
        // A frame from the other side (frozen across a flip) keeps 1x. A
        // frame is never drawn below 1x: a lens can't show more than its
        // own field of view, so a frozen frame from a narrower lens holds
        // at its full view until the wider lens's first frame.
        let scale = if lens.role.faces_front() == (self.facing == Facing::Front) {
            (zoom / lens.zoom).max(1.0)
        } else {
            1.0
        };
        Some(Transform {
            scale,
            rotation: lens.upright_clockwise(),
            mirror: lens.mirror,
        })
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
        target.controls.af_continuous = lens.autofocus;
        if self.opts.transfer_exposure {
            if let Some((from, meta)) = &self.last_meta {
                let from_lens = self.table.get(*from);
                if let (Some(t), Some(f_old), Some(f_new)) =
                    (meta.exposure_us, from_lens.f_number, lens.f_number)
                {
                    let e = exposure::transfer(
                        exposure::Exposure {
                            time_us: t as f64,
                            gain: meta.analogue_gain.unwrap_or(1.0) as f64,
                        },
                        f_old,
                        f_new,
                        1.0,
                        exposure::Limits::default(),
                    );
                    target.controls.exposure_us = Some(e.time_us.round() as i32);
                    target.controls.analogue_gain = Some(e.gain as f32);
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

    /// A tap on the drawn picture at (u, v), 0..1 from its top left.
    pub fn tap(&mut self, u: f64, v: f64, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if !self.opts.tap_to_focus || self.capture.is_busy() || self.switch.is_switching() {
            return out;
        }
        let Some((generation, target)) = self.switch.streaming() else {
            return out;
        };
        if target.purpose != Purpose::Preview {
            return out;
        }
        let Some(lens) = self.table.lenses().get(target.lens).cloned() else {
            return out;
        };
        let Some(t) = self.transform(&lens.id) else {
            return out;
        };
        let request = self.focus.tap(
            u,
            v,
            &t,
            focus::Capability {
                autofocus: lens.autofocus,
                af_windows: lens.af_windows,
                area: lens.crop_maximum,
            },
            now,
        );
        if let Some(focus) = request {
            out.push(Output::Backend(Command::SetControls {
                generation,
                controls: crate::backend::Controls {
                    focus: Some(focus),
                    ..Default::default()
                },
            }));
        }
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
        if self.capture.is_busy() || self.back.is_none() || self.front.is_none() {
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
        if self.switch.streaming().is_none() || self.switch.is_switching() {
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
                    target.controls = controls;
                    target.controls.af_continuous = false;
                    self.request(Some(target), now, out);
                }
            }
            Step::Fail(msg) => {
                out.push(Output::Error(format!("Photo failed: {msg}")));
                self.restore_preview(now, out);
            }
            Step::Nothing | Step::Keep | Step::KeepAndRestore(_) | Step::Discard => {}
        }
    }

    fn restore_preview(&mut self, now: Instant, out: &mut Vec<Output>) {
        if let Some(mut target) = self.target(Purpose::Preview) {
            let locked = self.capture.locked().clone();
            target.controls.exposure_us = locked.exposure_us;
            target.controls.analogue_gain = locked.analogue_gain;
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
                if af && target.purpose == Purpose::Preview {
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

    fn picture(&self, frame: Frame, lens_idx: usize) -> Output {
        let lens = self.table.get(lens_idx).clone();
        let zoom = self.side().map(|s| s.planner.zoom()).unwrap_or(1.0);
        Output::Picture(Picture {
            crop: (zoom / lens.zoom).max(1.0),
            rotation: lens.upright_clockwise(),
            zoom,
            lens,
            frame,
        })
    }

    pub fn on_event(&mut self, event: Event, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        match event {
            Event::Cameras(_) => {}
            Event::Notice { message } => out.push(Output::Error(message)),
            Event::Started { generation, .. } => {
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
                self.last_meta = Some((lens_idx, frame.meta.clone()));
                match self.capture.on_preview_frame(&frame.meta, now) {
                    Step::Keep => out.push(self.picture(frame, lens_idx)),
                    step => {
                        self.capture_step(step, now, out);
                        out.push(Output::Show(frame));
                    }
                }
            }
            Purpose::Still => match self.capture.on_still_frame(&frame.meta) {
                Step::KeepAndRestore(_) => {
                    out.push(self.picture(frame, lens_idx));
                    self.restore_preview(now, out);
                }
                Step::Discard => {}
                step => self.capture_step(step, now, out),
            },
        }
    }
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
                analogue_gain: Some(1.0),
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
                assert!(controls.analogue_gain.unwrap() > 1.0);
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
        match commands(&c.tap(0.25, 0.25, now)).as_slice() {
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
    fn taps_on_a_fixed_focus_lens_or_when_off_send_nothing() {
        let now = Instant::now();
        let mut c = Controller::new(&spec(), &cameras(), Options::default());
        open(&mut c, now);
        c.set_zoom(5.0, ZoomSource::Preset, now);
        let (g, _, _) = (2, "tele", Purpose::Preview);
        c.on_event(started(g, "tele"), now);
        c.on_event(frame(g, "tele", 10_000), now);
        assert!(commands(&c.tap(0.5, 0.5, now)).is_empty());
        assert_eq!(c.view().focus.unwrap().status, focus::Status::FixedFocus);
        c.set_tap_to_focus(false);
        c.set_zoom(1.0, ZoomSource::Preset, now);
        assert!(c.tap(0.5, 0.5, now).is_empty());
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
        // Two discarded, then the third matches the lock.
        for _ in 0..2 {
            assert!(c.on_event(frame(gs, "main", 10_000), now).is_empty());
        }
        let out = c.on_event(frame(gs, "main", 10_000), now);
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
