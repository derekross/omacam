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

impl FrameData {
    /// The pixels, when they're in memory.
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            FrameData::Memory(v) => Some(v),
            FrameData::Shared(s) => Some(s.as_ref()),
            FrameData::Dmabuf { .. } => None,
        }
    }
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

/// A rectangle in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Where a camera points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Location {
    Front,
    #[default]
    Back,
    External,
}

/// A camera as the backend found it.
#[derive(Debug, Clone, PartialEq, Default)]
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
    /// The camera has autofocus (an `AfMode` control).
    pub autofocus: bool,
    /// Its autofocus takes `AfWindows` (focus where the user taps).
    pub af_windows: bool,
    /// `ScalerCropMaximum`: the sensor area the processed frames show,
    /// in the coordinates `AfWindows` uses.
    pub crop_maximum: Option<Rect>,
    /// The exposure, white balance and focus controls it offers.
    pub caps: Caps,
}

/// What a camera's control map offers for manual exposure, white balance
/// and focus. A control the map doesn't list stays disabled in the UI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Caps {
    /// ExposureValue, EV range.
    pub ev: Option<(f32, f32)>,
    /// ExposureTime, microseconds.
    pub exposure_time: Option<(i32, i32)>,
    /// AnalogueGain.
    pub gain: Option<(f32, f32)>,
    /// ExposureTimeMode/AnalogueGainMode (or the older AeEnable): manual
    /// exposure is possible at all.
    pub manual_exposure: bool,
    /// AwbMode presets.
    pub awb_modes: bool,
    /// ColourTemperature (with AwbEnable off), kelvin.
    pub color_temperature: Option<(i32, i32)>,
    /// LensPosition with AfMode Manual: manual focus.
    pub lens_position: Option<(f32, f32)>,
    /// AeEnable: false holds the current exposure (AE lock).
    pub ae_enable: bool,
    /// AeMeteringMode, so a tap can meter its window (Spot).
    pub ae_metering: bool,
    /// AfPause, to hold continuous focus (AF lock).
    pub af_pause: bool,
    /// ColourGains [red, blue] range (with AwbEnable off): white balance by
    /// gains where there's no ColourTemperature.
    pub color_gains: Option<(f32, f32)>,
}

impl Caps {
    pub fn manual_shutter(&self) -> bool {
        self.manual_exposure && self.exposure_time.is_some()
    }

    pub fn manual_gain(&self) -> bool {
        self.manual_exposure && self.gain.is_some()
    }

    pub fn white_balance(&self) -> bool {
        self.awb_modes || self.color_temperature.is_some() || self.color_gains.is_some()
    }

    /// Can AE/AF lock hold the exposure: AeEnable, or manual exposure.
    pub fn can_lock(&self) -> bool {
        self.ae_enable || (self.manual_shutter() && self.manual_gain())
    }
}

/// A control left to the camera, or set by hand.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Setting<T> {
    #[default]
    Auto,
    Manual(T),
}

impl<T: Copy> Setting<T> {
    pub fn manual(self) -> Option<T> {
        match self {
            Setting::Auto => None,
            Setting::Manual(v) => Some(v),
        }
    }

    pub fn is_auto(&self) -> bool {
        matches!(self, Setting::Auto)
    }
}

/// White balance by preset or color temperature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Awb {
    Preset(AwbPreset),
    Kelvin(u32),
    /// Red and blue gains (a preset on a camera without presets or a
    /// temperature).
    Gains([f32; 2]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwbPreset {
    Daylight,
    Cloudy,
    Incandescent,
    Fluorescent,
}

impl AwbPreset {
    pub const ALL: [AwbPreset; 4] = [
        AwbPreset::Daylight,
        AwbPreset::Cloudy,
        AwbPreset::Incandescent,
        AwbPreset::Fluorescent,
    ];

    /// The color temperature a preset stands for, for cameras that take
    /// a temperature but no presets.
    pub fn kelvin(self) -> u32 {
        match self {
            AwbPreset::Daylight => 5500,
            AwbPreset::Cloudy => 6500,
            AwbPreset::Incandescent => 2850,
            AwbPreset::Fluorescent => 4000,
        }
    }

    /// Red and blue gains for a preset on a camera that takes only gains:
    /// typical values for a phone sensor's white balance under each light,
    /// to calibrate against the AWB's own gains measured under it.
    pub fn gains(self) -> [f32; 2] {
        match self {
            AwbPreset::Daylight => [1.9, 1.6],
            AwbPreset::Cloudy => [2.1, 1.45],
            AwbPreset::Incandescent => [1.25, 2.6],
            AwbPreset::Fluorescent => [1.6, 2.1],
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AwbPreset::Daylight => "Daylight",
            AwbPreset::Cloudy => "Cloudy",
            AwbPreset::Incandescent => "Incandescent",
            AwbPreset::Fluorescent => "Fluorescent",
        }
    }
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
    /// Capture each frame's raw Bayer picture too, where the camera gives
    /// it at the processed frame's size ([`Frame::raw`]): a Night or HDR
    /// burst merges the raw frames.
    pub raw: bool,
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
    pub analog_gain: Option<f32>,
    pub color_gains: Option<[f32; 2]>,
    pub lens_position: Option<f32>,
    /// Ask for continuous autofocus where the camera has it.
    pub af_continuous: bool,
    /// Focus once (AfMode Auto, AfTrigger Start), on `window` when given
    /// and the camera takes AfWindows, else wherever its AF looks.
    pub focus: Option<FocusRequest>,
    /// The exposure, white balance and focus settings to apply (None:
    /// leave as they are). Unlike the hints above, these hold.
    pub settings: Option<ExposureSettings>,
    /// The tapped window, for spot metering (AfMetering Windows +
    /// AfWindows; AeMeteringMode Spot meters it), even with manual focus.
    pub metering_window: Option<Rect>,
    /// Continuous autofocus on this window (AfMode Continuous, AfMetering
    /// Windows, AfWindows): a tap while recording video.
    pub af_window: Option<Rect>,
    /// FrameDurationLimits, both ends: a fixed frame rate (16 667 us is
    /// 60 fps). None leaves the camera's.
    pub frame_duration_us: Option<i64>,
}

impl Controls {
    /// `newer` on top of `self`: what it sets wins, what it leaves alone
    /// stays (two changes before the camera's next request).
    pub fn merged(self, newer: Controls) -> Controls {
        Controls {
            exposure_us: newer.exposure_us.or(self.exposure_us),
            analog_gain: newer.analog_gain.or(self.analog_gain),
            color_gains: newer.color_gains.or(self.color_gains),
            lens_position: newer.lens_position.or(self.lens_position),
            af_continuous: newer.af_continuous || (self.af_continuous && newer.focus.is_none()),
            focus: newer
                .focus
                .or(if newer.af_continuous || newer.af_window.is_some() {
                    None
                } else {
                    self.focus
                }),
            settings: newer.settings.or(self.settings),
            metering_window: newer.metering_window.or(self.metering_window),
            af_window: newer.af_window.or(if newer.focus.is_some() {
                None
            } else {
                self.af_window
            }),
            frame_duration_us: newer.frame_duration_us.or(self.frame_duration_us),
        }
    }
}

/// What the user set: everything Auto by default.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ExposureSettings {
    /// ExposureValue, EV.
    pub ev: f32,
    /// ExposureTime, microseconds.
    pub shutter: Setting<i32>,
    /// AnalogueGain.
    pub gain: Setting<f32>,
    pub white_balance: Setting<Awb>,
    /// LensPosition (Auto: the autofocus decides).
    pub lens: Setting<f32>,
    /// AE lock through AeEnable=false (the camera holds what it has).
    pub ae_hold: bool,
    /// AF lock through AfPause (continuous focus holds where it is).
    pub af_hold: bool,
    /// Meter the focus window (AeMeteringMode Spot) instead of the frame.
    pub spot: bool,
}

/// A one-shot focus scan.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FocusRequest {
    pub window: Option<Rect>,
}

/// Per-frame metadata, where the camera reports it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameMeta {
    pub sequence: u32,
    pub timestamp_ns: u64,
    pub exposure_us: Option<i32>,
    pub analog_gain: Option<f32>,
    pub digital_gain: Option<f32>,
    pub color_gains: Option<[f32; 2]>,
    pub af_state: Option<AfState>,
    pub lens_position: Option<f32>,
    pub ae_state: Option<AeState>,
}

/// Auto-exposure states, as libcamera reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AeState {
    Idle,
    Searching,
    Converged,
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

/// Pixels shared between the viewfinder, the video encoder and a photo,
/// without copies.
#[derive(Debug, Clone)]
pub struct SharedPixels(pub std::sync::Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedPixels {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// The pixels of a frame.
#[derive(Debug)]
pub enum FrameData {
    /// A copy the frame owns. The camera's buffer is already back in use.
    Memory(Vec<u8>),
    /// The same copy, shared (the app shares it with its encoder).
    Shared(SharedPixels),
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
    /// The raw picture the frame was processed from, when the stream asked
    /// for it ([`StreamRequest::raw`]).
    pub raw: Option<Box<RawFrame>>,
}

/// A raw Bayer frame: MIPI CSI-2 packed 10-bit samples, at the processed
/// frame's size.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub size: Size,
    /// Bytes per row.
    pub stride: u32,
    /// Where the red sample of the 2x2 pattern sits: (column, row).
    pub first_red: (u8, u8),
    pub data: Vec<u8>,
    pub isp: IspMeta,
}

/// What the software ISP used for a frame, as far as the metadata says:
/// the raw merge finishes with the same (the camera's tuning file has the
/// rest).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IspMeta {
    /// The sensor's model (`s5kgn1`): it names the tuning file.
    pub sensor_model: Option<String>,
    /// Black level on a 16-bit scale (libcamera's SensorBlackLevels).
    pub black_level: Option<f32>,
    pub color_matrix: Option<[[f32; 3]; 3]>,
    pub saturation: Option<f32>,
    pub contrast: Option<f32>,
    pub sharpness: Option<f32>,
    pub gamma: Option<f32>,
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
        /// The highest frame rate the configured mode allows, from its
        /// FrameDurationLimits minimum; None when the camera doesn't
        /// report frame durations (it runs at its own rate, 30 fps).
        max_fps: Option<f64>,
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
