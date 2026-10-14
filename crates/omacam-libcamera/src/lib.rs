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
use libcamera::control::{ControlEntry, ControlList};
use libcamera::framebuffer::{AsFrameBuffer, FrameMetadataStatus};
use libcamera::framebuffer_allocator::{FrameBuffer, FrameBufferAllocator};
use libcamera::framebuffer_map::MemoryMappedFrameBuffer;
use libcamera::geometry::{Rectangle, Size as LcSize};
use libcamera::pixel_format::PixelFormat;
use libcamera::request::{Request, RequestStatus, ReuseFlag};
use libcamera::stream::{Stream, StreamRole};
use libcamera::{controls, properties};

use omacam_core::backend::{
    AfState, Backend, CameraInfo, Command, Controls, Event, EventSink, Frame, FrameData, FrameMeta,
    Location, PixelLayout, Purpose, Rect, ReleaseToken, Size, StreamRequest,
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

/// How long a stop waits for the UI to give back lent buffers.
const LENT_WAIT: Duration = Duration::from_millis(300);

enum Msg {
    Cmd(Command),
    Completed(Request),
    Release { generation: u64, cookie: u64 },
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
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Msg>) {
        match self.open_manager() {
            Ok(()) => {
                let cameras = self.list_cameras();
                tracing::info!(count = cameras.len(), "cameras found");
                (self.sink)(Event::Cameras(cameras));
            }
            Err(e) => {
                (self.sink)(Event::Error {
                    generation: None,
                    message: format!("libcamera didn't start: {e}"),
                });
                (self.sink)(Event::Cameras(Vec::new()));
            }
        }
        loop {
            let msg = match self.deferred.pop_front() {
                Some(m) => m,
                None => match rx.recv() {
                    Ok(m) => m,
                    Err(_) => break,
                },
            };
            match msg {
                Msg::Quit => break,
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
            self.manager = Some(CameraManager::new()?);
        }
        Ok(())
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
                let crop_maximum =
                    props
                        .get::<properties::ScalerCropMaximum>()
                        .ok()
                        .map(|r| Rect {
                            x: r.0.x,
                            y: r.0.y,
                            width: r.0.width,
                            height: r.0.height,
                        });
                CameraInfo {
                    id: cam.id().to_string(),
                    location,
                    rotation,
                    model,
                    autofocus: has(<controls::AfMode as ControlEntry>::ID),
                    af_windows: has(<controls::AfWindows as ControlEntry>::ID),
                    crop_maximum,
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
            return; // cancelled by a stop
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
        // stop() completed every queued request as cancelled: collect them
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

/// Put `c` on a control list. A control the camera doesn't have fails to
/// set; that's fine.
fn set_controls(list: &mut ControlList, c: &Controls, exposure_hints: bool) {
    if c.af_continuous {
        let _ = list.set(controls::AfMode::Continuous);
    }
    if let Some(focus) = c.focus {
        // One scan: auto mode, the tapped window when the camera takes
        // windows, then the trigger, all in the same request.
        let _ = list.set(controls::AfMode::Auto);
        if let Some(w) = focus.window {
            let _ = list.set(controls::AfMetering::Windows);
            let _ = list.set(controls::AfWindows(vec![Rectangle {
                x: w.x,
                y: w.y,
                width: w.width,
                height: w.height,
            }]));
        }
        let _ = list.set(controls::AfTrigger::Start);
    }
    if exposure_hints {
        if let Some(t) = c.exposure_us {
            let _ = list.set(controls::ExposureTime(t));
        }
        if let Some(g) = c.analogue_gain {
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
        analogue_gain: md.get::<controls::AnalogueGain>().ok().map(|v| v.0),
        digital_gain: md.get::<controls::DigitalGain>().ok().map(|v| v.0),
        colour_gains: md.get::<controls::ColourGains>().ok().map(|v| v.0),
        af_state: md.get::<controls::AfState>().ok().map(|s| match s {
            controls::AfState::Idle => AfState::Idle,
            controls::AfState::Scanning => AfState::Scanning,
            controls::AfState::Focused => AfState::Focused,
            controls::AfState::Failed => AfState::Failed,
        }),
        lens_position: md.get::<controls::LensPosition>().ok().map(|v| v.0),
    }
}
