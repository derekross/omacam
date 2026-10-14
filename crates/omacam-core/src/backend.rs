//! The interface between omacam and a camera library.
//!
//! A backend runs on its own thread. The app sends it [`Command`]s through
//! [`Backend::send`] and receives [`Event`]s through the sink it gave the
//! backend when it started it. Every stream start carries a generation
//! number; the backend tags the stream's frames and events with it, so the
//! controller can drop what arrives late from a stream it already left.

use std::fmt;
use std::os::fd::RawFd;

/// Width and height in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Size { width, height }
    }

    pub fn pixels(self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// Where a camera points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Front,
    Back,
    External,
}

/// A camera as the backend found it.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraInfo {
    /// The library's stable id (for libcamera, the sensor's DT path).
    pub id: String,
    pub location: Location,
    /// libcamera's `Rotation` property: how far the image is turned on the
    /// sensor, which a viewer corrects by turning it this many degrees
    /// counter-clockwise (property_ids_core.yaml, "Example two - Phone
    /// camera"). See [`crate::lens::upright_clockwise`].
    pub rotation: u32,
    pub model: String,
}

/// Byte layout of a single-plane RGB frame, named by DRM fourcc as
/// libcamera names them (so `Xrgb8888` is B, G, R, X in memory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelLayout {
    Xrgb8888,
    Xbgr8888,
    Rgb888,
    Bgr888,
}

impl PixelLayout {
    pub fn bytes_per_pixel(self) -> u32 {
        match self {
            PixelLayout::Xrgb8888 | PixelLayout::Xbgr8888 => 4,
            PixelLayout::Rgb888 | PixelLayout::Bgr888 => 3,
        }
    }

    /// The DRM fourcc code.
    pub fn fourcc(self) -> u32 {
        let code = match self {
            PixelLayout::Xrgb8888 => b"XR24",
            PixelLayout::Xbgr8888 => b"XB24",
            PixelLayout::Rgb888 => b"RG24",
            PixelLayout::Bgr888 => b"BG24",
        };
        u32::from_le_bytes(*code)
    }

    pub fn from_fourcc(fourcc: u32) -> Option<Self> {
        [
            PixelLayout::Xrgb8888,
            PixelLayout::Xbgr8888,
            PixelLayout::Rgb888,
            PixelLayout::Bgr888,
        ]
        .into_iter()
        .find(|l| l.fourcc() == fourcc)
    }
}

/// What a stream is for. The software ISP gives one processed stream, so a
/// full-resolution still is a separate configuration of the same camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Preview,
    Still,
}

/// What to ask of the camera when a stream starts.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRequest {
    pub purpose: Purpose,
    /// None lets the library choose.
    pub size: Option<Size>,
    /// Set explicitly: libcamera's default of four full-size buffers once
    /// failed to allocate from CMA.
    pub buffers: u32,
    /// Smaller sizes to settle for, in order, when `size` can't start (for
    /// a still, usually lack of camera memory). The backend says so with an
    /// [`Event::Notice`].
    pub fallbacks: Vec<Size>,
}

/// Autofocus states, as libcamera reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfState {
    Idle,
    Scanning,
    Focused,
    Failed,
}

/// Controls to apply. `None` leaves a control to the camera's algorithms.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Controls {
    pub exposure_us: Option<i32>,
    pub analogue_gain: Option<f32>,
    pub colour_gains: Option<[f32; 2]>,
    pub lens_position: Option<f32>,
    /// Ask for continuous autofocus where the camera has it.
    pub af_continuous: bool,
}

/// Per-frame metadata, where the camera reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameMeta {
    pub sequence: u32,
    pub timestamp_ns: u64,
    pub exposure_us: Option<i32>,
    pub analogue_gain: Option<f32>,
    pub digital_gain: Option<f32>,
    pub colour_gains: Option<[f32; 2]>,
    pub af_state: Option<AfState>,
    pub lens_position: Option<f32>,
}

/// Returns a borrowed buffer to its camera when dropped.
pub struct ReleaseToken(Option<Box<dyn FnOnce() + Send>>);

impl ReleaseToken {
    pub fn new(release: impl FnOnce() + Send + 'static) -> Self {
        ReleaseToken(Some(Box::new(release)))
    }
}

impl Drop for ReleaseToken {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            release();
        }
    }
}

impl fmt::Debug for ReleaseToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReleaseToken")
    }
}

/// The pixels of a frame.
#[derive(Debug)]
pub enum FrameData {
    /// A copy the frame owns. The camera's buffer is already back in use.
    Memory(Vec<u8>),
    /// The camera's own dmabuf, lent until the token is dropped. The fd is
    /// valid for as long as the token lives and must not be closed.
    Dmabuf {
        fd: RawFd,
        offset: u32,
        release: ReleaseToken,
    },
}

/// One processed frame.
#[derive(Debug)]
pub struct Frame {
    pub generation: u64,
    pub camera: String,
    pub size: Size,
    pub stride: u32,
    pub layout: PixelLayout,
    pub data: FrameData,
    pub meta: FrameMeta,
}

/// Commands from the app to a backend.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Stop whatever streams and start `camera`.
    Start {
        generation: u64,
        camera: String,
        stream: StreamRequest,
        controls: Controls,
    },
    /// Stop the stream of this generation (a no-op for an older one).
    Stop { generation: u64 },
    /// Apply controls to the running stream's next requests.
    SetControls { generation: u64, controls: Controls },
    /// Release every camera and the camera library, until the next Start.
    Release,
}

/// Events from a backend to the app.
#[derive(Debug)]
pub enum Event {
    /// The cameras present. Sent once at startup.
    Cameras(Vec<CameraInfo>),
    Started {
        generation: u64,
        camera: String,
        size: Size,
        stride: u32,
        layout: PixelLayout,
    },
    Frame(Frame),
    /// Something the user should know that isn't a failure (a still taken
    /// at a smaller size than asked).
    Notice {
        message: String,
    },
    Stopped {
        generation: u64,
    },
    /// A start failed or a running stream broke. `generation` is None when
    /// the failure isn't tied to a stream (the library didn't start).
    Error {
        generation: Option<u64>,
        message: String,
    },
}

/// A running backend.
pub trait Backend {
    fn send(&self, command: Command);
}

/// Where a backend delivers its events. Called on the backend's thread.
pub type EventSink = Box<dyn Fn(Event) + Send + Sync>;
