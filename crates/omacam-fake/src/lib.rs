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

/// The Pixel 7 Pro's four cameras, as libcamera names them.
pub fn cheetah_cameras() -> Vec<CameraInfo> {
    let cam = |id: &str, location, rotation, model: &str| CameraInfo {
        id: id.into(),
        location,
        rotation,
        model: model.into(),
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
}

enum Msg {
    Cmd(Command),
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
    sequence: u32,
    next_frame: Instant,
    controls: Controls,
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

/// A test card: a tinted gradient, a centre cross (to check crop and
/// rotation centring), a block in the top-left corner (to check
/// orientation) and a bar that moves with the frame count.
fn render(stream: &Stream) -> Vec<u8> {
    let Size { width, height } = stream.size;
    let (w, h) = (width as usize, height as usize);
    let stride = w * 4;
    let mut px = vec![0u8; stride * h];
    let bar = (stream.sequence as usize * 8) % w;
    for y in 0..h {
        for x in 0..w {
            let i = y * stride + x * 4;
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let mut rgb = [
                (stream.tint[0] as f32 * (0.4 + 0.6 * fx)) as u8,
                (stream.tint[1] as f32 * (0.4 + 0.6 * fy)) as u8,
                stream.tint[2],
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
            let failure = if !opts.cameras.iter().any(|c| c.id == camera) {
                Some(format!("no camera {camera}"))
            } else if faults.lock().unwrap().busy.contains(&camera) {
                Some(format!("{camera} is busy"))
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
                size: fit(req.size.unwrap_or(opts.default_size), opts.max_size),
                started_at: now + opts.start_delay,
                started: false,
                sequence: 0,
                next_frame: now,
                controls,
            });
        }
        Command::Stop { generation } => {
            if stream.as_ref().is_some_and(|s| s.generation == generation) {
                stop(stream, sink);
            }
        }
        Command::SetControls {
            generation,
            controls,
        } => {
            if let Some(s) = stream.as_mut().filter(|s| s.generation == generation) {
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
            exposure_us: Some(exposure),
            analogue_gain: Some(1.0),
            ..FrameMeta::default()
        },
    }));
    s.sequence += 1;
    s.next_frame += opts.frame_interval;
}

fn run(rx: mpsc::Receiver<Msg>, sink: EventSink, opts: Options, faults: Arc<Mutex<Faults>>) {
    let epoch = Instant::now();
    sink(Event::Cameras(opts.cameras.clone()));
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
            Ok(Msg::Cmd(cmd)) => handle(cmd, &mut stream, &sink, &opts, &faults),
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
    fn render_marks_the_centre_and_the_top_left() {
        let s = Stream {
            generation: 1,
            camera: "x".into(),
            size: Size::new(64, 32),
            started_at: Instant::now(),
            started: true,
            sequence: 0,
            next_frame: Instant::now(),
            controls: Controls::default(),
            tint: [100, 100, 100],
        };
        let px = render(&s);
        let at = |x: usize, y: usize| px[(y * 64 + x) * 4..(y * 64 + x) * 4 + 3].to_vec();
        assert_eq!(at(32, 16), [255, 255, 255]);
        assert_eq!(at(5, 2), [255, 255, 255]);
        assert_ne!(at(40, 28), [255, 255, 255]);
    }
}
