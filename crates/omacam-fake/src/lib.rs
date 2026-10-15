//! A fake camera backend: synthetic frames from a thread, with a scripted
//! start delay and cameras that can be made to fail. It lets the tests
//! drive the controller through the same command and event path as the
//! real backend, and lets the UI run on a machine without cameras.

use std::collections::HashSet;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use omacam_core::backend::{
    Backend, CameraInfo, Command, Controls, Event, EventSink, Frame, FrameData, FrameMeta,
    Location, PixelLayout, Size,
};

/// The manual controls a camera might offer: all of them.
pub fn full_caps(movable_lens: bool) -> omacam_core::backend::Caps {
    omacam_core::backend::Caps {
        ev: Some((-8.0, 8.0)),
        exposure_time: Some((100, 66_666)),
        gain: Some((1.0, 16.0)),
        manual_exposure: true,
        awb_modes: true,
        color_temperature: Some((2500, 8000)),
        lens_position: movable_lens.then_some((0.0, 1.0)),
        ae_enable: true,
        ae_metering: true,
        af_pause: true,
        color_gains: Some((0.25, 4.0)),
    }
}

/// The Pixel 7 Pro's four cameras, as libcamera names them.
pub fn cheetah_cameras() -> Vec<CameraInfo> {
    // Main and ultrawide focus (and, in the fake, take focus windows);
    // the telephoto's lens doesn't answer and the front is fixed-focus.
    let cam = |id: &str, location, rotation, model: &str| {
        let af = matches!(model, "s5kgn1" | "imx386");
        CameraInfo {
            id: id.into(),
            location,
            rotation,
            model: model.into(),
            autofocus: af,
            af_windows: af,
            crop_maximum: Some(omacam_core::backend::Rect {
                x: 0,
                y: 0,
                width: 4080,
                height: 3072,
            }),
            // Everything libcamera can offer, so the Pro panel can be
            // tried here; the phone's libcamera may offer less.
            caps: full_caps(af),
        }
    };
    vec![
        cam(
            "/base/soc@0/usi@109100c0/i2c@10910000/camera-sensor@10",
            Location::Front,
            90,
            "s5k3j1",
        ),
        cam(
            "/base/soc@0/usi@109000c0/i2c@10900000/camera-sensor@3d",
            Location::Back,
            90,
            "s5kgn1",
        ),
        cam(
            "/base/soc@0/usi@109200c0/i2c@10920000/camera-sensor@1a",
            Location::Back,
            270,
            "imx386",
        ),
        cam(
            "/base/soc@0/usi@109300c0/i2c@10930000/camera-sensor@2d",
            Location::Back,
            270,
            "s5kgm5",
        ),
    ]
}

#[derive(Debug, Clone)]
pub struct Options {
    pub cameras: Vec<CameraInfo>,
    /// Time from Start to Started (sensor power-up and stream start).
    pub start_delay: Duration,
    pub frame_interval: Duration,
    /// Size used when a start asks for none.
    pub default_size: Size,
    /// Largest frame the fake renders; bigger requests are scaled down so
    /// tests stay fast. The reported size is the rendered one.
    pub max_size: Size,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            cameras: cheetah_cameras(),
            start_delay: Duration::from_millis(200),
            frame_interval: Duration::from_millis(33),
            default_size: Size::new(640, 480),
            max_size: Size::new(1008, 760),
        }
    }
}

/// Shared switches a test can flip while the backend runs.
#[derive(Debug, Default)]
pub struct Faults {
    /// Cameras whose start fails with "busy".
    pub busy: HashSet<String>,
    /// Streams bigger than this many pixels fail for lack of memory, as
    /// the CMA pool does on the phone; the request's fallbacks are tried.
    pub memory_limit_pixels: Option<u64>,
}

enum Msg {
    Cmd(Command),
    /// The cameras change, as when drivers load late or one goes away.
    SetCameras(Vec<CameraInfo>),
    Quit,
}

pub struct FakeBackend {
    tx: mpsc::Sender<Msg>,
    thread: Option<JoinHandle<()>>,
    pub faults: Arc<Mutex<Faults>>,
}

impl FakeBackend {
    pub fn spawn(sink: EventSink, opts: Options) -> FakeBackend {
        let (tx, rx) = mpsc::channel();
        let faults = Arc::new(Mutex::new(Faults::default()));
        let f = faults.clone();
        let thread = std::thread::Builder::new()
            .name("omacam-fake".into())
            .spawn(move || run(rx, sink, opts, f))
            .expect("spawn fake backend thread");
        FakeBackend {
            tx,
            thread: Some(thread),
            faults,
        }
    }
}

impl FakeBackend {
    /// Replace the cameras present: a stream on a camera that's gone stops,
    /// and the new list goes out as an [`Event::Cameras`].
    pub fn set_cameras(&self, cameras: Vec<CameraInfo>) {
        let _ = self.tx.send(Msg::SetCameras(cameras));
    }
}

impl Backend for FakeBackend {
    fn send(&self, command: Command) {
        let _ = self.tx.send(Msg::Cmd(command));
    }
}

impl Drop for FakeBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Stream {
    generation: u64,
    camera: String,
    size: Size,
    started_at: Instant,
    started: bool,
    notice: Option<String>,
    sequence: u32,
    next_frame: Instant,
    controls: Controls,
    /// The exposure settings in force (kept across partial updates).
    settings: omacam_core::backend::ExposureSettings,
    tint: [u8; 3],
}

fn tint(id: &str) -> [u8; 3] {
    let h = id
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    [
        80 + (h & 0x7f) as u8,
        80 + ((h >> 8) & 0x7f) as u8,
        80 + ((h >> 16) & 0x7f) as u8,
    ]
}

fn fit(size: Size, max: Size) -> Size {
    if size.width <= max.width && size.height <= max.height {
        return size;
    }
    let s = f64::min(
        max.width as f64 / size.width as f64,
        max.height as f64 / size.height as f64,
    );
    Size::new(
        ((size.width as f64 * s) as u32).max(2) & !1,
        ((size.height as f64 * s) as u32).max(2) & !1,
    )
}

/// A test card: a tinted gradient, a center cross (to check crop and
/// rotation centring), a block in the top-left corner (to check
/// orientation) and a bar that moves with the frame count.
fn render(stream: &Stream) -> Vec<u8> {
    let Size { width, height } = stream.size;
    let (w, h) = (width as usize, height as usize);
    let stride = w * 4;
    let mut px = vec![0u8; stride * h];
    let bar = (stream.sequence as usize * 8) % w;
    // What the settings would do to a real picture: EV and a manual
    // exposure brighten or darken it, white balance shifts its color.
    use omacam_core::backend::{Awb, AwbPreset, Setting};
    let set = &stream.settings;
    let mut light = 2f32.powf(set.ev);
    if let Setting::Manual(t) = set.shutter {
        light *= t as f32 / 10_000.0;
    }
    if let Setting::Manual(g) = set.gain {
        light *= g;
    }
    let (rw, bw) = match set.white_balance {
        Setting::Manual(Awb::Preset(AwbPreset::Cloudy)) => (1.15, 0.9),
        Setting::Manual(Awb::Preset(AwbPreset::Incandescent)) => (0.75, 1.3),
        Setting::Manual(Awb::Preset(AwbPreset::Fluorescent)) => (0.9, 1.1),
        Setting::Manual(Awb::Kelvin(k)) => (k as f32 / 5500.0, 5500.0 / k as f32),
        Setting::Manual(Awb::Gains([r, b])) => (r / 1.9, b / 1.6),
        _ => (1.0, 1.0),
    };
    let tone = |v: f32, w: f32| (v * light * w).clamp(0.0, 255.0) as u8;
    for y in 0..h {
        for x in 0..w {
            let i = y * stride + x * 4;
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let mut rgb = [
                tone(stream.tint[0] as f32 * (0.4 + 0.6 * fx), rw),
                tone(stream.tint[1] as f32 * (0.4 + 0.6 * fy), 1.0),
                tone(stream.tint[2] as f32, bw),
            ];
            let cross = x.abs_diff(w / 2) < 3 || y.abs_diff(h / 2) < 3;
            let corner = x < w / 8 && y < h / 8;
            if cross || corner {
                rgb = [255, 255, 255];
            }
            if x.abs_diff(bar) < 4 {
                rgb = [20, 20, 20];
            }
            // XRGB8888: B, G, R, X in memory.
            px[i] = rgb[2];
            px[i + 1] = rgb[1];
            px[i + 2] = rgb[0];
            px[i + 3] = 255;
        }
    }
    px
}

fn stop(stream: &mut Option<Stream>, sink: &EventSink) {
    if let Some(old) = stream.take() {
        sink(Event::Stopped {
            generation: old.generation,
        });
    }
}

fn handle(
    cmd: Command,
    stream: &mut Option<Stream>,
    sink: &EventSink,
    opts: &Options,
    cameras: &[CameraInfo],
    faults: &Mutex<Faults>,
) {
    match cmd {
        Command::Start {
            generation,
            camera,
            stream: req,
            controls,
        } => {
            stop(stream, sink);
            let limit = faults.lock().unwrap().memory_limit_pixels;
            let fits = |s: &Size| limit.is_none_or(|l| fit(*s, opts.max_size).pixels() <= l);
            let wanted = req.size.unwrap_or(opts.default_size);
            let (size, notice) = if fits(&wanted) {
                (Some(wanted), None)
            } else {
                match req.fallbacks.iter().find(|s| fits(s)) {
                    Some(s) => (
                        Some(*s),
                        Some(format!(
                            "Not enough camera memory for the full mode: used {s} instead"
                        )),
                    ),
                    None => (None, None),
                }
            };
            let failure = if !cameras.iter().any(|c| c.id == camera) {
                Some(format!("no camera {camera}"))
            } else if faults.lock().unwrap().busy.contains(&camera) {
                Some(format!("{camera} is busy"))
            } else if size.is_none() {
                Some("Cannot allocate memory (os error 12)".into())
            } else {
                None
            };
            if let Some(message) = failure {
                sink(Event::Error {
                    generation: Some(generation),
                    message,
                });
                return;
            }
            let now = Instant::now();
            *stream = Some(Stream {
                generation,
                tint: tint(&camera),
                camera,
                size: fit(size.expect("checked"), opts.max_size),
                started_at: now + opts.start_delay,
                started: false,
                notice,
                sequence: 0,
                next_frame: now,
                settings: controls.settings.unwrap_or_default(),
                controls,
            });
        }
        Command::Stop { generation } => {
            if stream.as_ref().is_some_and(|s| s.generation == generation) {
                stop(stream, sink);
            } else {
                // Already gone: say so, so nobody waits for it.
                sink(Event::Stopped { generation });
            }
        }
        Command::SetControls {
            generation,
            controls,
        } => {
            if let Some(s) = stream.as_mut().filter(|s| s.generation == generation) {
                if let Some(settings) = controls.settings {
                    s.settings = settings;
                }
                s.controls = controls;
            }
        }
        Command::Release => stop(stream, sink),
    }
}

fn step(s: &mut Stream, sink: &EventSink, opts: &Options, epoch: Instant) {
    let now = Instant::now();
    if !s.started {
        if now >= s.started_at {
            s.started = true;
            s.next_frame = now;
            sink(Event::Started {
                generation: s.generation,
                camera: s.camera.clone(),
                size: s.size,
                stride: s.size.width * 4,
                layout: PixelLayout::Xrgb8888,
            });
            if let Some(message) = s.notice.take() {
                sink(Event::Notice { message });
            }
        }
        return;
    }
    if now < s.next_frame {
        return;
    }
    // An auto-exposure that walks from the start hint towards 10 ms over a
    // few frames, like the soft ISP's AGC.
    let start = s.controls.exposure_us.unwrap_or(10_000) as f64;
    let k = 0.5f64.powi(s.sequence.min(30) as i32);
    let exposure = (10_000.0 + (start - 10_000.0) * k) as i32;
    sink(Event::Frame(Frame {
        generation: s.generation,
        camera: s.camera.clone(),
        size: s.size,
        stride: s.size.width * 4,
        layout: PixelLayout::Xrgb8888,
        data: FrameData::Memory(render(s)),
        meta: FrameMeta {
            sequence: s.sequence,
            timestamp_ns: now.duration_since(epoch).as_nanos() as u64,
            exposure_us: Some(s.settings.shutter.manual().unwrap_or(exposure)),
            analog_gain: Some(s.settings.gain.manual().unwrap_or(1.0)),
            lens_position: Some(s.settings.lens.manual().unwrap_or(0.5)),
            ..FrameMeta::default()
        },
    }));
    s.sequence += 1;
    s.next_frame += opts.frame_interval;
}

fn run(rx: mpsc::Receiver<Msg>, sink: EventSink, opts: Options, faults: Arc<Mutex<Faults>>) {
    let epoch = Instant::now();
    let mut cameras = opts.cameras.clone();
    sink(Event::Cameras(cameras.clone()));
    let mut stream: Option<Stream> = None;
    loop {
        let now = Instant::now();
        let wake = match &stream {
            Some(s) if !s.started => s.started_at,
            Some(s) => s.next_frame,
            None => now + Duration::from_secs(3600),
        };
        match rx.recv_timeout(wake.saturating_duration_since(now)) {
            Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Msg::Cmd(cmd)) => handle(cmd, &mut stream, &sink, &opts, &cameras, &faults),
            Ok(Msg::SetCameras(list)) => {
                if stream
                    .as_ref()
                    .is_some_and(|s| !list.iter().any(|c| c.id == s.camera))
                {
                    stop(&mut stream, &sink);
                }
                cameras = list;
                sink(Event::Cameras(cameras.clone()));
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(s) = stream.as_mut() {
                    step(s, &sink, &opts, epoch);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_small_sizes_and_scales_large_ones() {
        let max = Size::new(1008, 760);
        assert_eq!(fit(Size::new(640, 480), max), Size::new(640, 480));
        let s = fit(Size::new(4080, 3072), max);
        assert!(s.width <= 1008 && s.height <= 760);
        assert_eq!(s.width % 2, 0);
    }

    #[test]
    fn render_marks_the_center_and_the_top_left() {
        let s = Stream {
            generation: 1,
            camera: "x".into(),
            size: Size::new(64, 32),
            started_at: Instant::now(),
            started: true,
            notice: None,
            sequence: 0,
            next_frame: Instant::now(),
            controls: Controls::default(),
            settings: Default::default(),
            tint: [100, 100, 100],
        };
        let px = render(&s);
        let at = |x: usize, y: usize| px[(y * 64 + x) * 4..(y * 64 + x) * 4 + 3].to_vec();
        assert_eq!(at(32, 16), [255, 255, 255]);
        assert_eq!(at(5, 2), [255, 255, 255]);
        assert_ne!(at(40, 28), [255, 255, 255]);
    }
}
