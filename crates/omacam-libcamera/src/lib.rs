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

use libcamera::camera::{ActiveCamera, CameraConfigurationStatus};
use libcamera::camera_manager::CameraManager;
use libcamera::control::ControlList;
use libcamera::framebuffer::{AsFrameBuffer, FrameMetadataStatus};
use libcamera::framebuffer_allocator::{FrameBuffer, FrameBufferAllocator};
use libcamera::framebuffer_map::MemoryMappedFrameBuffer;
use libcamera::geometry::Size as LcSize;
use libcamera::pixel_format::PixelFormat;
use libcamera::request::{Request, RequestStatus, ReuseFlag};
use libcamera::stream::{Stream, StreamRole};
use libcamera::{controls, properties};

use omacam_core::backend::{
    AfState, Backend, CameraInfo, Command, Controls, Event, EventSink, Frame, FrameData, FrameMeta,
    Location, PixelLayout, Purpose, ReleaseToken, Size, StreamRequest,
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
                CameraInfo {
                    id: cam.id().to_string(),
                    location,
                    rotation,
                    model,
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
                match self.start(generation, &camera, &stream, &controls) {
                    Ok(()) => {
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
                    }
                    Err(e) => {
                        tracing::warn!(generation, camera, error = %e, "start failed");
                        (self.sink)(Event::Error {
                            generation: Some(generation),
                            message: format!("Camera didn't start: {e}"),
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

    fn start(
        &mut self,
        generation: u64,
        camera: &str,
        req: &StreamRequest,
        controls: &Controls,
    ) -> io::Result<()> {
        self.open_manager()?;
        let mgr = self.manager.as_ref().expect("opened");
        let cam = mgr
            .get(camera)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such camera"))?;
        let mut cam: ActiveCamera<'static> = cam.acquire()?;

        let mut cfgs = cam
            .generate_configuration(&[StreamRole::ViewFinder])
            .ok_or_else(|| io::Error::other("no configuration for the viewfinder role"))?;
        {
            let mut sc = cfgs.get_mut(0).expect("one stream");
            sc.set_pixel_format(XRGB8888);
            if let Some(s) = req.size {
                sc.set_size(LcSize::new(s.width, s.height));
            }
            sc.set_buffer_count(req.buffers.max(1));
        }
        match cfgs.validate() {
            CameraConfigurationStatus::Invalid => {
                return Err(io::Error::other("configuration invalid"))
            }
            CameraConfigurationStatus::Adjusted => {
                tracing::info!(config = %cfgs.to_string_repr(), "configuration adjusted")
            }
            CameraConfigurationStatus::Valid => {}
        }
        let (layout, size, stride) = {
            let sc = cfgs.get(0).expect("one stream");
            let fmt = sc.get_pixel_format();
            let layout = PixelLayout::from_fourcc(fmt.fourcc())
                .filter(|_| fmt.modifier() == 0)
                .ok_or_else(|| {
                    io::Error::other(format!("unsupported pixel format {:#x}", fmt.fourcc()))
                })?;
            let s = sc.get_size();
            (layout, Size::new(s.width, s.height), sc.get_stride())
        };
        cam.configure(&mut cfgs)?;
        let stream = cfgs
            .get(0)
            .and_then(|sc| sc.stream())
            .ok_or_else(|| io::Error::other("no stream after configure"))?;

        let mut alloc = FrameBufferAllocator::new(&cam);
        let fbs = alloc.alloc(&stream)?;
        tracing::debug!(asked = req.buffers, got = fbs.len(), "buffers allocated");
        let mut requests = Vec::with_capacity(fbs.len());
        for (i, fb) in fbs.into_iter().enumerate() {
            let mapped = MappedBuffer::new(fb)
                .map_err(|e| io::Error::other(format!("mapping a buffer: {e:?}")))?;
            let mut r = cam
                .create_request(Some(i as u64))
                .ok_or_else(|| io::Error::other("create_request failed"))?;
            r.add_buffer(&stream, mapped)?;
            requests.push(r);
        }

        let me = self.me.clone();
        cam.on_request_completed(move |req| {
            let _ = me.send(Msg::Completed(req));
        });

        let mut start_controls = ControlList::from_info_map(cam.controls()).unwrap_or_default();
        set_controls(&mut start_controls, controls, self.opts.exposure_hints);
        cam.start(Some(&start_controls))?;

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
                tracing::warn!(error = %e, "queueing a request failed");
                active.idle.push(r);
            }
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
