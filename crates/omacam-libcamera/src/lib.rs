//! omacam's libcamera backend, through libcamera-rs.
//!
//! libcamera-rs's `ActiveCamera` isn't `Send` (libcamera-rs #86), so the
//! camera manager, the camera and its buffers all live on one thread of
//! ours. Commands, completed requests and buffer returns reach that thread
//! through one channel, so it handles them in order.
//!
//! Display modes:
//! - [`DisplayMode::Memory`] copies each frame out of the camera's buffer
//!   and requeues the buffer at once. One copy of about 9 MB a frame at
//!   2016x1136, but it doesn't touch the GPU import path (the default
//!   until Mesa's panthor export fix lands).
//! - [`DisplayMode::Dmabuf`] lends the camera's dmabuf to the UI and
//!   requeues the request only when the UI drops the frame's
//!   [`ReleaseToken`].

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libcamera::camera::ActiveCamera;
use libcamera::camera_manager::CameraManager;
use libcamera::control::{ControlEntry, ControlInfoMap, ControlList};
use libcamera::control_value::ControlValue;
use libcamera::framebuffer::{AsFrameBuffer, FrameMetadataStatus};
use libcamera::framebuffer_allocator::{FrameBuffer, FrameBufferAllocator};
use libcamera::framebuffer_map::MemoryMappedFrameBuffer;
use libcamera::geometry::{Rectangle, Size as LcSize};
use libcamera::pixel_format::PixelFormat;
use libcamera::request::{Request, RequestStatus, ReuseFlag};
use libcamera::stream::{Stream, StreamRole};
use libcamera::{controls, properties};

use omacam_core::backend::{
    AeState, AfState, Awb, AwbPreset, Backend, CameraInfo, Caps, Command, Controls, Event,
    EventSink, ExposureSettings, Frame, FrameData, FrameMeta, Location, PixelLayout, Purpose, Rect,
    ReleaseToken, Setting, Size, StreamRequest,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DisplayMode {
    #[default]
    Memory,
    Dmabuf,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub display: DisplayMode,
    /// Pass exposure and gain hints to a starting stream. Off by default:
    /// whether the simple IPA takes ExposureTime as a starting point or as
    /// manual exposure needs checking on the phone first (CAMERA-APP-PLAN
    /// L3).
    pub exposure_hints: bool,
}

/// XRGB8888, the format asked of the software ISP: 32-bit linear, the
/// safe EGL import target on panfrost.
const XRGB8888: PixelFormat = PixelFormat::new(u32::from_le_bytes(*b"XR24"), 0);
const EINVAL: i32 = 22;
const ENOMEM: i32 = 12;

/// A failed start, and the step that failed.
#[derive(Debug)]
struct StartError {
    step: &'static str,
    error: io::Error,
}

impl StartError {
    fn is_out_of_memory(&self) -> bool {
        self.error.raw_os_error() == Some(ENOMEM)
    }
}

/// How often to look for cameras while there are none.
const POLL_CAMERAS: Duration = Duration::from_secs(2);
/// How long a stop waits for the UI to give back lent buffers.
const LENT_WAIT: Duration = Duration::from_millis(300);

enum Msg {
    Cmd(Command),
    /// libcamera added or removed a camera (`removed`: its id).
    CamerasChanged {
        removed: Option<String>,
    },
    Completed(Request),
    Release {
        generation: u64,
        cookie: u64,
    },
    Quit,
}

pub struct LibcameraBackend {
    tx: mpsc::Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl LibcameraBackend {
    pub fn spawn(sink: EventSink, opts: Options) -> io::Result<LibcameraBackend> {
        let (tx, rx) = mpsc::channel();
        let me = tx.clone();
        let thread = std::thread::Builder::new()
            .name("omacam-libcamera".into())
            .spawn(move || Worker::new(me, sink, opts).run(rx))?;
        Ok(LibcameraBackend {
            tx,
            thread: Some(thread),
        })
    }
}

impl Backend for LibcameraBackend {
    fn send(&self, command: Command) {
        let _ = self.tx.send(Msg::Cmd(command));
    }
}

impl Drop for LibcameraBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type MappedBuffer = MemoryMappedFrameBuffer<FrameBuffer>;

/// A configured, running camera. Field order is drop order: the camera
/// stops and releases before its buffers go.
struct Active {
    cam: ActiveCamera<'static>,
    generation: u64,
    camera_id: String,
    stream: Stream,
    size: Size,
    stride: u32,
    layout: PixelLayout,
    /// Copy frames out (memory display, and always for stills, which the
    /// encoder reads on the CPU).
    copy: bool,
    /// Completed requests lent to the UI (dmabuf mode), by cookie.
    lent: HashMap<u64, Request>,
    /// Requests that couldn't be queued.
    idle: Vec<Request>,
    /// Controls to put on the next queued request.
    pending_controls: Option<Controls>,
    _alloc: FrameBufferAllocator,
}

struct Worker {
    me: mpsc::Sender<Msg>,
    sink: EventSink,
    opts: Options,
    active: Option<Active>,
    manager: Option<CameraManager>,
    /// Messages that arrived while a stop drained the channel.
    deferred: VecDeque<Msg>,
    /// The camera ids last announced, and whether any list went out.
    known: Vec<String>,
    announced: bool,
}

impl Worker {
    fn new(me: mpsc::Sender<Msg>, sink: EventSink, opts: Options) -> Worker {
        Worker {
            me,
            sink,
            opts,
            active: None,
            manager: None,
            deferred: VecDeque::new(),
            known: Vec::new(),
            announced: false,
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        if let Err(e) = self.open_manager() {
            tracing::warn!(error = %e, "libcamera didn't start; waiting for cameras");
        }
        self.announce_cameras();
        loop {
            let msg = match self.deferred.pop_front() {
                Some(m) => m,
                // No cameras yet (the drivers load late in boot): look
                // again every couple of seconds.
                None if self.known.is_empty() && self.active.is_none() => {
                    match rx.recv_timeout(POLL_CAMERAS) {
                        Ok(m) => m,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            self.manager = None;
                            let _ = self.open_manager();
                            self.announce_cameras();
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                None => match rx.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                },
            };
            match msg {
                Msg::Quit => break,
                Msg::CamerasChanged { removed } => {
                    if let Some(id) = removed {
                        tracing::warn!(camera = id, "camera removed");
                        if self.active.as_ref().is_some_and(|a| a.camera_id == id) {
                            self.stop(&rx);
                        }
                    }
                    self.announce_cameras();
                }
                Msg::Cmd(cmd) => self.command(cmd, &rx),
                Msg::Completed(req) => self.completed(req),
                Msg::Release { generation, cookie } => self.release(generation, cookie),
            }
        }
        self.stop(&rx);
        self.manager = None;
    }

    fn open_manager(&mut self) -> io::Result<()> {
        if self.manager.is_none() {
            let mut mgr = CameraManager::new()?;
            let me = self.me.clone();
            mgr.on_camera_added(move |_| {
                let _ = me.send(Msg::CamerasChanged { removed: None });
            });
            let me = self.me.clone();
            mgr.on_camera_removed(move |cam| {
                let _ = me.send(Msg::CamerasChanged {
                    removed: Some(cam.id().to_string()),
                });
            });
            self.manager = Some(mgr);
        }
        Ok(())
    }

    /// Send the camera list when it differs from the last one sent.
    fn announce_cameras(&mut self) {
        let cameras = self.list_cameras();
        let ids: Vec<String> = cameras.iter().map(|c| c.id.clone()).collect();
        if ids == self.known && self.announced {
            return;
        }
        tracing::info!(count = cameras.len(), "cameras found");
        self.known = ids;
        self.announced = true;
        (self.sink)(Event::Cameras(cameras));
    }

    fn list_cameras(&self) -> Vec<CameraInfo> {
        let Some(mgr) = &self.manager else {
            return Vec::new();
        };
        let list = mgr.cameras();
        list.iter()
            .map(|cam| {
                let props = cam.properties();
                let location = match props.get::<properties::Location>() {
                    Ok(properties::Location::CameraFront) => Location::Front,
                    Ok(properties::Location::CameraBack) => Location::Back,
                    _ => Location::External,
                };
                let rotation = props
                    .get::<properties::Rotation>()
                    .map(|r| r.0.rem_euclid(360) as u32)
                    .unwrap_or(0);
                let model = props
                    .get::<properties::Model>()
                    .map(|m| m.0)
                    .unwrap_or_default();
                let ctrls = cam.controls();
                let has = |id: u32| ctrls.find(id).is_ok();
                // The coordinates AfWindows use: ScalerCropMaximum where the
                // camera reports it, else (libcamera 4.10's simple pipeline)
                // the sensor's active area, from its origin.
                let crop_maximum = props
                    .get::<properties::ScalerCropMaximum>()
                    .ok()
                    .map(|r| Rect {
                        x: r.0.x,
                        y: r.0.y,
                        width: r.0.width,
                        height: r.0.height,
                    })
                    .or_else(|| {
                        let areas = props.get::<properties::PixelArrayActiveAreas>().ok()?;
                        let a = areas.0.first()?;
                        Some(Rect {
                            x: 0,
                            y: 0,
                            width: a.width,
                            height: a.height,
                        })
                    });
                let caps = caps(ctrls);
                tracing::info!(camera = cam.id(), ?caps, "camera controls");
                CameraInfo {
                    id: cam.id().to_string(),
                    location,
                    rotation,
                    model,
                    autofocus: has(<controls::AfMode as ControlEntry>::ID),
                    af_windows: has(<controls::AfWindows as ControlEntry>::ID),
                    crop_maximum,
                    caps,
                }
            })
            .collect()
    }

    fn command(&mut self, cmd: Command, rx: &mpsc::Receiver<Msg>) {
        match cmd {
            Command::Start {
                generation,
                camera,
                stream,
                controls,
            } => {
                self.stop(rx);
                let began = Instant::now();
                match self.start_with_fallbacks(generation, &camera, &stream, &controls) {
                    Ok(note) => {
                        let a = self.active.as_ref().expect("started");
                        tracing::info!(
                            generation,
                            camera,
                            size = %a.size,
                            stride = a.stride,
                            ms = began.elapsed().as_millis() as u64,
                            "stream started"
                        );
                        (self.sink)(Event::Started {
                            generation,
                            camera,
                            size: a.size,
                            stride: a.stride,
                            layout: a.layout,
                        });
                        if let Some(message) = note {
                            (self.sink)(Event::Notice { message });
                        }
                    }
                    Err(e) => {
                        tracing::error!(generation, camera, step = e.step, error = %e.error, "camera didn't start");
                        (self.sink)(Event::Error {
                            generation: Some(generation),
                            message: format!("Camera didn't start ({}): {}", e.step, e.error),
                        });
                    }
                }
            }
            Command::Stop { generation } => {
                if self
                    .active
                    .as_ref()
                    .is_some_and(|a| a.generation == generation)
                {
                    self.stop(rx);
                } else {
                    // Already gone (a camera that went away): say so, so
                    // the app doesn't wait for it.
                    (self.sink)(Event::Stopped { generation });
                }
            }
            Command::SetControls {
                generation,
                controls,
            } => {
                if let Some(a) = self.active.as_mut().filter(|a| a.generation == generation) {
                    a.pending_controls = Some(controls);
                }
            }
            Command::Release => {
                self.stop(rx);
                if self.manager.take().is_some() {
                    tracing::info!("camera manager released");
                }
            }
        }
    }

    /// Start `req`, trying again when memory runs short: once more after
    /// releasing the camera manager, then with one buffer, then at each
    /// fallback size. A still stream also falls back on other start
    /// errors. Returns a note for the user when it had to settle for less.
    fn start_with_fallbacks(
        &mut self,
        generation: u64,
        camera: &str,
        req: &StreamRequest,
        controls: &Controls,
    ) -> Result<Option<String>, StartError> {
        let mut attempts: Vec<(Option<Size>, u32)> = vec![(req.size, req.buffers)];
        attempts.push((req.size, req.buffers));
        if req.buffers > 1 {
            attempts.push((req.size, 1));
        }
        for &f in &req.fallbacks {
            attempts.push((Some(f), req.buffers.min(2)));
        }
        let mut last = None;
        for (i, &(size, buffers)) in attempts.iter().enumerate() {
            let fallback = i > 0;
            if fallback {
                tracing::warn!(
                    generation,
                    camera,
                    size = %size.map(|s| s.to_string()).unwrap_or_else(|| "default".into()),
                    buffers,
                    "retrying the start"
                );
            }
            match self.start(generation, camera, size, buffers, req, controls) {
                Ok(()) => {
                    let got = self.active.as_ref().map(|a| a.size);
                    let note = match (size == req.size, got) {
                        (false, Some(g)) => Some(format!(
                            "Not enough camera memory for the full mode: used {g} instead"
                        )),
                        _ => None,
                    };
                    return Ok(note);
                }
                Err(e) => {
                    tracing::warn!(
                        generation,
                        camera,
                        step = e.step,
                        error = %e.error,
                        errno = e.error.raw_os_error(),
                        attempt = i + 1,
                        "start failed"
                    );
                    let retry = e.is_out_of_memory() || req.purpose == Purpose::Still;
                    if i == 0 && e.is_out_of_memory() {
                        // Let go of everything libcamera holds, then retry.
                        self.manager = None;
                    }
                    last = Some(e);
                    if !retry {
                        break;
                    }
                }
            }
        }
        Err(last.expect("at least one attempt"))
    }

    fn start(
        &mut self,
        generation: u64,
        camera: &str,
        size_wanted: Option<Size>,
        buffers: u32,
        req: &StreamRequest,
        controls: &Controls,
    ) -> Result<(), StartError> {
        let at = |step: &'static str| move |error: io::Error| StartError { step, error };
        self.open_manager().map_err(at("camera manager"))?;
        let mgr = self.manager.as_ref().expect("opened");
        let cam = mgr.get(camera).ok_or_else(|| {
            at("find camera")(io::Error::new(io::ErrorKind::NotFound, "no such camera"))
        })?;
        let mut cam: ActiveCamera<'static> = cam.acquire().map_err(at("acquire"))?;

        let mut cfgs = cam
            .generate_configuration(&[StreamRole::ViewFinder])
            .ok_or_else(|| at("generate configuration")(io::Error::other("no configuration")))?;
        {
            let mut sc = cfgs.get_mut(0).expect("one stream");
            sc.set_pixel_format(XRGB8888);
            if let Some(s) = size_wanted {
                sc.set_size(LcSize::new(s.width, s.height));
            }
            sc.set_buffer_count(buffers.max(1));
        }
        let status = cfgs.validate();
        tracing::info!(
            generation,
            camera,
            purpose = ?req.purpose,
            asked = %size_wanted.map(|s| s.to_string()).unwrap_or_else(|| "default".into()),
            buffers,
            status = ?status,
            config = %cfgs.to_string_repr(),
            "configuration validated"
        );
        if status.is_invalid() {
            return Err(at("validate")(io::Error::from_raw_os_error(EINVAL)));
        }
        let (layout, size, stride, frame_size, count) = {
            let sc = cfgs.get(0).expect("one stream");
            let fmt = sc.get_pixel_format();
            let layout = PixelLayout::from_fourcc(fmt.fourcc())
                .filter(|_| fmt.modifier() == 0)
                .ok_or_else(|| {
                    at("format")(io::Error::other(format!(
                        "unsupported pixel format {:#x}",
                        fmt.fourcc()
                    )))
                })?;
            let s = sc.get_size();
            (
                layout,
                Size::new(s.width, s.height),
                sc.get_stride(),
                sc.get_frame_size(),
                sc.get_buffer_count(),
            )
        };
        cam.configure(&mut cfgs).map_err(at("configure"))?;
        let stream = cfgs
            .get(0)
            .and_then(|sc| sc.stream())
            .ok_or_else(|| at("stream")(io::Error::other("no stream after configure")))?;

        let mut alloc = FrameBufferAllocator::new(&cam);
        let fbs = alloc.alloc(&stream).map_err(at("allocate buffers"))?;
        tracing::info!(
            generation,
            count = fbs.len(),
            asked = count,
            frame_bytes = frame_size,
            total_mib = format!(
                "{:.1}",
                fbs.len() as f64 * frame_size as f64 / (1024.0 * 1024.0)
            ),
            "buffers allocated"
        );
        let mut requests = Vec::with_capacity(fbs.len());
        for (i, fb) in fbs.into_iter().enumerate() {
            let mapped = MappedBuffer::new(fb).map_err(|e| {
                at("map buffer")(io::Error::other(format!("mapping a buffer: {e:?}")))
            })?;
            let mut r = cam
                .create_request(Some(i as u64))
                .ok_or_else(|| at("create request")(io::Error::other("create_request failed")))?;
            r.add_buffer(&stream, mapped).map_err(at("add buffer"))?;
            requests.push(r);
        }

        let me = self.me.clone();
        cam.on_request_completed(move |req| {
            let _ = me.send(Msg::Completed(req));
        });

        let mut start_controls = ControlList::from_info_map(cam.controls()).unwrap_or_default();
        set_controls(&mut start_controls, controls, self.opts.exposure_hints);
        cam.start(Some(&start_controls)).map_err(at("start"))?;

        let mut active = Active {
            cam,
            generation,
            camera_id: camera.to_string(),
            stream,
            size,
            stride,
            layout,
            copy: self.opts.display == DisplayMode::Memory || req.purpose == Purpose::Still,
            lent: HashMap::new(),
            idle: Vec::new(),
            pending_controls: None,
            _alloc: alloc,
        };
        for r in requests {
            if let Err((r, e)) = active.cam.queue_request(r) {
                tracing::warn!(generation, error = %e, "queueing a request failed");
                active.idle.push(r);
            }
        }
        if !active.idle.is_empty() && active.idle.len() == count as usize {
            return Err(at("queue requests")(io::Error::other(
                "no request could be queued",
            )));
        }
        self.active = Some(active);
        Ok(())
    }

    fn requeue(&mut self, mut req: Request) {
        let hints = self.opts.exposure_hints;
        let Some(a) = self.active.as_mut() else {
            return;
        };
        req.reuse(ReuseFlag::REUSE_BUFFERS);
        if let Some(c) = a.pending_controls.take() {
            set_controls(req.controls_mut(), &c, hints);
        }
        if let Err((req, e)) = a.cam.queue_request(req) {
            tracing::warn!(error = %e, "requeue failed");
            a.idle.push(req);
        }
    }

    fn completed(&mut self, req: Request) {
        let Some(a) = self.active.as_mut() else {
            return; // a stopped stream's leftovers
        };
        if req.status() != RequestStatus::Complete {
            return; // canceled by a stop
        }
        let Some(fb) = req.buffer::<MappedBuffer>(&a.stream) else {
            return;
        };
        let ok = fb
            .metadata()
            .is_some_and(|m| m.status() == FrameMetadataStatus::Success);
        if !ok {
            self.requeue(req);
            return;
        }
        let meta = frame_meta(&req);
        let (generation, size, stride, layout) = (a.generation, a.size, a.stride, a.layout);
        let camera = a.camera_id.clone();
        let data = match a.copy {
            true => {
                let copy = {
                    let planes = fb.data();
                    planes.first().map(|plane| {
                        let len = (stride as usize * size.height as usize).min(plane.len());
                        plane[..len].to_vec()
                    })
                };
                self.requeue(req);
                match copy {
                    Some(c) => FrameData::Memory(c),
                    None => return,
                }
            }
            false => {
                let plane = {
                    let planes = fb.planes();
                    planes
                        .get(0)
                        .map(|p| (p.fd(), p.offset().unwrap_or(0) as u32))
                };
                let Some((fd, offset)) = plane else {
                    self.requeue(req);
                    return;
                };
                let cookie = req.cookie();
                let me = self.me.clone();
                let release = ReleaseToken::new(move || {
                    let _ = me.send(Msg::Release { generation, cookie });
                });
                a.lent.insert(cookie, req);
                FrameData::Dmabuf {
                    fd,
                    offset,
                    release,
                }
            }
        };
        (self.sink)(Event::Frame(Frame {
            generation,
            camera,
            size,
            stride,
            layout,
            data,
            meta,
        }));
    }

    fn release(&mut self, generation: u64, cookie: u64) {
        let Some(a) = self.active.as_mut().filter(|a| a.generation == generation) else {
            return;
        };
        if let Some(req) = a.lent.remove(&cookie) {
            self.requeue(req);
        }
    }

    /// Stop and drop the active camera, if any, and say so.
    fn stop(&mut self, rx: &mpsc::Receiver<Msg>) {
        let Some(mut a) = self.active.take() else {
            return;
        };
        // Give the UI a moment to hand back lent buffers: their fds close
        // with the allocator.
        let deadline = Instant::now() + LENT_WAIT;
        while !a.lent.is_empty() && Instant::now() < deadline {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Msg::Release { generation, cookie }) if generation == a.generation => {
                    a.lent.remove(&cookie);
                }
                Ok(Msg::Completed(_)) | Ok(Msg::Release { .. }) => {}
                Ok(other) => self.deferred.push_back(other),
                Err(_) => break,
            }
        }
        if !a.lent.is_empty() {
            tracing::warn!(count = a.lent.len(), "stopping with buffers still lent");
        }
        if let Err(e) = a.cam.stop() {
            tracing::warn!(error = %e, "stop failed");
        }
        // stop() completed every queued request as canceled: collect them
        // so they're dropped before the camera and its allocator.
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Completed(_) | Msg::Release { .. } => {}
                other => self.deferred.push_back(other),
            }
        }
        let generation = a.generation;
        drop(a);
        (self.sink)(Event::Stopped { generation });
    }
}

/// ColourGains is an array control: its range is per element.
fn color_gains_range(map: &ControlInfoMap) -> Option<(f32, f32)> {
    let info = map.find(<controls::ColourGains as ControlEntry>::ID).ok()?;
    let first = |v: ControlValue| match v {
        ControlValue::Float(x) => x.first().copied(),
        _ => None,
    };
    let (lo, hi) = (first(info.min())?, first(info.max())?);
    (hi >= lo).then_some((lo, hi))
}

fn number(v: ControlValue) -> Option<f64> {
    match v {
        ControlValue::Float(x) => x.first().map(|v| *v as f64),
        ControlValue::Int32(x) => x.first().map(|v| *v as f64),
        ControlValue::Int64(x) => x.first().map(|v| *v as f64),
        ControlValue::Uint32(x) => x.first().map(|v| *v as f64),
        _ => None,
    }
}

/// A control's range, when the map lists it with numeric bounds.
fn range(map: &ControlInfoMap, id: u32) -> Option<(f64, f64)> {
    let info = map.find(id).ok()?;
    let (lo, hi) = (number(info.min())?, number(info.max())?);
    (hi >= lo).then_some((lo, hi))
}

/// What the camera offers for manual exposure, white balance and focus.
/// Everything is looked up by control id, so a libcamera without one of
/// them just leaves that control disabled in the app.
fn caps(map: &ControlInfoMap) -> Caps {
    let has = |id: u32| map.find(id).is_ok();
    let id = |c: u32| c;
    Caps {
        ev: range(map, id(<controls::ExposureValue as ControlEntry>::ID))
            .map(|(a, b)| (a as f32, b as f32)),
        exposure_time: range(map, <controls::ExposureTime as ControlEntry>::ID)
            .map(|(a, b)| (a as i32, b as i32)),
        gain: range(map, <controls::AnalogueGain as ControlEntry>::ID)
            .map(|(a, b)| (a as f32, b as f32)),
        manual_exposure: has(<controls::ExposureTimeMode as ControlEntry>::ID)
            || has(<controls::AeEnable as ControlEntry>::ID),
        awb_modes: has(<controls::AwbMode as ControlEntry>::ID),
        color_temperature: range(map, <controls::ColourTemperature as ControlEntry>::ID)
            .filter(|_| has(<controls::AwbEnable as ControlEntry>::ID))
            .map(|(a, b)| (a as i32, b as i32)),
        lens_position: range(map, <controls::LensPosition as ControlEntry>::ID)
            .filter(|_| has(<controls::AfMode as ControlEntry>::ID))
            .map(|(a, b)| (a as f32, b as f32)),
        ae_enable: has(<controls::AeEnable as ControlEntry>::ID),
        ae_metering: has(<controls::AeMeteringMode as ControlEntry>::ID)
            && has(<controls::AfWindows as ControlEntry>::ID),
        af_pause: has(<controls::AfPause as ControlEntry>::ID),
        color_gains: color_gains_range(map)
            .filter(|_| has(<controls::AwbEnable as ControlEntry>::ID)),
    }
}

/// Exposure, white balance and manual focus on a control list. Modes go
/// with values: a manual exposure time is ExposureTimeMode Manual plus
/// ExposureTime (or AeEnable off on a libcamera without the modes), and
/// Auto puts the mode back.
fn set_settings(list: &mut ControlList, s: &ExposureSettings) {
    let _ = list.set(controls::ExposureValue(s.ev));
    if s.ae_hold {
        // AE lock: the camera holds what it has. Mode controls in the same
        // request would win over it, so none are sent.
        let _ = list.set(controls::AeEnable(false));
    } else {
        match s.shutter {
            Setting::Manual(t) => {
                let _ = list.set(controls::ExposureTimeMode::Manual);
                let _ = list.set(controls::ExposureTime(t));
            }
            Setting::Auto => {
                let _ = list.set(controls::ExposureTimeMode::Auto);
            }
        }
        match s.gain {
            Setting::Manual(g) => {
                let _ = list.set(controls::AnalogueGainMode::Manual);
                let _ = list.set(controls::AnalogueGain(g));
            }
            Setting::Auto => {
                let _ = list.set(controls::AnalogueGainMode::Auto);
            }
        }
        // The older switch, for a libcamera without the mode controls.
        let _ = list.set(controls::AeEnable(s.shutter.is_auto() && s.gain.is_auto()));
    }
    let _ = list.set(if s.spot {
        controls::AeMeteringMode::MeteringSpot
    } else {
        controls::AeMeteringMode::MeteringMatrix
    });
    match s.white_balance {
        Setting::Auto => {
            let _ = list.set(controls::AwbEnable(true));
            let _ = list.set(controls::AwbMode::AwbAuto);
        }
        Setting::Manual(Awb::Preset(p)) => {
            let _ = list.set(controls::AwbEnable(true));
            let _ = list.set(match p {
                AwbPreset::Daylight => controls::AwbMode::AwbDaylight,
                AwbPreset::Cloudy => controls::AwbMode::AwbCloudy,
                AwbPreset::Incandescent => controls::AwbMode::AwbIncandescent,
                AwbPreset::Fluorescent => controls::AwbMode::AwbFluorescent,
            });
        }
        Setting::Manual(Awb::Kelvin(k)) => {
            let _ = list.set(controls::AwbEnable(false));
            let _ = list.set(controls::ColourTemperature(k as i32));
        }
        Setting::Manual(Awb::Gains(g)) => {
            let _ = list.set(controls::AwbEnable(false));
            let _ = list.set(controls::ColourGains(g));
        }
    }
    match s.lens {
        Setting::Manual(p) => {
            let _ = list.set(controls::AfMode::Manual);
            let _ = list.set(controls::LensPosition(p));
        }
        Setting::Auto => {
            let _ = list.set(if s.af_hold {
                controls::AfPause::Immediate
            } else {
                controls::AfPause::Resume
            });
        }
    }
}

fn set_window(list: &mut ControlList, w: &Rect) {
    let _ = list.set(controls::AfMetering::Windows);
    let _ = list.set(controls::AfWindows(vec![Rectangle {
        x: w.x,
        y: w.y,
        width: w.width,
        height: w.height,
    }]));
}

/// Put `c` on a control list. A control the camera doesn't have fails to
/// set; that's fine.
fn set_controls(list: &mut ControlList, c: &Controls, exposure_hints: bool) {
    if c.af_continuous {
        let _ = list.set(controls::AfMode::Continuous);
    }
    if let Some(s) = &c.settings {
        set_settings(list, s);
    }
    if let Some(w) = &c.metering_window {
        set_window(list, w);
    }
    if let Some(focus) = c.focus {
        // One scan: auto mode, the tapped window when the camera takes
        // windows, then the trigger, all in the same request.
        let _ = list.set(controls::AfMode::Auto);
        if let Some(w) = focus.window {
            set_window(list, &w);
        }
        let _ = list.set(controls::AfTrigger::Start);
    }
    if exposure_hints {
        if let Some(t) = c.exposure_us {
            let _ = list.set(controls::ExposureTime(t));
        }
        if let Some(g) = c.analog_gain {
            let _ = list.set(controls::AnalogueGain(g));
        }
    }
}

fn frame_meta(req: &Request) -> FrameMeta {
    let md = req.metadata();
    FrameMeta {
        sequence: req.sequence(),
        timestamp_ns: md
            .get::<controls::SensorTimestamp>()
            .map(|t| t.0 as u64)
            .unwrap_or(0),
        exposure_us: md.get::<controls::ExposureTime>().ok().map(|v| v.0),
        analog_gain: md.get::<controls::AnalogueGain>().ok().map(|v| v.0),
        digital_gain: md.get::<controls::DigitalGain>().ok().map(|v| v.0),
        color_gains: md.get::<controls::ColourGains>().ok().map(|v| v.0),
        af_state: md.get::<controls::AfState>().ok().map(|s| match s {
            controls::AfState::Idle => AfState::Idle,
            controls::AfState::Scanning => AfState::Scanning,
            controls::AfState::Focused => AfState::Focused,
            controls::AfState::Failed => AfState::Failed,
        }),
        lens_position: md.get::<controls::LensPosition>().ok().map(|v| v.0),
        ae_state: md.get::<controls::AeState>().ok().map(|s| match s {
            controls::AeState::Idle => AeState::Idle,
            controls::AeState::Searching => AeState::Searching,
            controls::AeState::Converged => AeState::Converged,
        }),
    }
}
