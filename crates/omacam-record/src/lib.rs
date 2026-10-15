//! Video recording: the camera's processed frames and the microphone into
//! a file, through GStreamer.
//!
//! ```text
//! appsrc (camera frames, BGRx/RGBx/BGR/RGB) -> queue (leaky: drops under load)
//!   -> videocrop (16:9 out of the frame, then the zoom) -> videoscale
//!   -> videoconvert -> I420 at the chosen size -> encoder -> parser
//!   -> taginject (image-orientation, so a landscape frame plays upright)
//!   -> muxer -> filesink
//! audio source (PipeWire: the microphone) -> audioconvert -> audioresample
//!   -> opusenc -> muxer
//! ```
//!
//! The gs201's hardware codec (MFC) has no mainline driver, so encoding is
//! on the CPU: H.264 by x264 (gst-plugins-ugly) or OpenH264 (-bad) in MP4,
//! else VP8 (libvpx, -good) in WebM. Audio is Opus (CAMERA-APP-PLAN Q11),
//! which MP4 carries too. Frames and audio are timestamped on the
//! pipeline's clock as they arrive, which keeps them in sync to within a
//! frame.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("GStreamer: {0}")]
    Glib(#[from] gst::glib::Error),
    #[error("GStreamer: {0}")]
    Bool(#[from] gst::glib::BoolError),
    #[error("GStreamer state change failed: {0}")]
    State(#[from] gst::StateChangeError),
    #[error("no video encoder: install gst-plugins-ugly (x264enc), gst-plugins-bad (openh264enc) or gst-plugins-good (vp8enc)")]
    NoEncoder,
    #[error("recording failed: {0}")]
    Pipeline(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Byte layout of the incoming frames, as GStreamer names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    /// DRM XRGB8888: B, G, R, X in memory.
    Bgrx,
    /// DRM XBGR8888.
    Rgbx,
    /// DRM RGB888: B, G, R.
    Bgr,
    /// DRM BGR888.
    Rgb,
}

impl InputFormat {
    fn caps_name(self) -> &'static str {
        match self {
            InputFormat::Bgrx => "BGRx",
            InputFormat::Rgbx => "RGBx",
            InputFormat::Bgr => "BGR",
            InputFormat::Rgb => "RGB",
        }
    }
}

/// The encoder and container, in order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// x264enc, H.264 in MP4.
    X264,
    /// openh264enc, H.264 in MP4.
    OpenH264,
    /// vp8enc, VP8 in WebM.
    Vp8,
}

impl Codec {
    pub const PREFERENCE: [Codec; 3] = [Codec::X264, Codec::OpenH264, Codec::Vp8];

    pub fn extension(self) -> &'static str {
        match self {
            Codec::X264 | Codec::OpenH264 => "mp4",
            Codec::Vp8 => "webm",
        }
    }

    fn needed(self) -> &'static [&'static str] {
        match self {
            // x264enc gives mp4mux the avc stream format itself: no parser,
            // so no gst-plugins-bad.
            Codec::X264 => &["x264enc", "mp4mux", "videocrop", "videoflip", "taginject"],
            Codec::OpenH264 => &[
                "openh264enc",
                "h264parse",
                "mp4mux",
                "videocrop",
                "videoflip",
                "taginject",
            ],
            Codec::Vp8 => &["vp8enc", "webmmux", "videocrop", "videoflip", "taginject"],
        }
    }

    /// The encoder, parser and muxer, as a pipeline fragment.
    fn chain(self, bitrate_kbps: u32, fps: u32) -> String {
        let keyint = fps * 2;
        match self {
            Codec::X264 => format!(
                "x264enc speed-preset=ultrafast tune=zerolatency bitrate={bitrate_kbps} \
                 key-int-max={keyint} threads=0 \
                 ! video/x-h264,profile=baseline,stream-format=avc,alignment=au"
            ),
            Codec::OpenH264 => format!(
                "openh264enc bitrate={} gop-size={keyint} complexity=low ! h264parse",
                bitrate_kbps * 1000
            ),
            Codec::Vp8 => format!(
                "vp8enc deadline=1 cpu-used=8 threads=4 target-bitrate={} keyframe-max-dist={keyint}",
                bitrate_kbps * 1000
            ),
        }
    }

    fn muxer(self) -> &'static str {
        match self {
            Codec::X264 | Codec::OpenH264 => "mp4mux",
            Codec::Vp8 => "webmmux",
        }
    }
}

fn have(element: &str) -> bool {
    gst::ElementFactory::find(element).is_some()
}

/// Start GStreamer (once per process).
pub fn init() -> Result<()> {
    gst::init()?;
    Ok(())
}

/// The best codec this machine can encode with.
pub fn best_codec() -> Option<Codec> {
    Codec::PREFERENCE
        .into_iter()
        .find(|c| c.needed().iter().all(|e| have(e)))
}

/// Where the sound comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audio {
    None,
    /// The default PipeWire source (the phone's microphone).
    PipeWire,
    /// A test tone, for tests.
    Test,
}

/// The frames of one camera: their layout and how much of the lens's
/// field of view they cover.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Input {
    pub format: InputFormat,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// The share of the lens's picture the frames cover, along their
    /// width and height (the main camera's 16:9 mode crops the height).
    pub coverage: (f64, f64),
    /// Turn these frames half a turn to match the file's orientation (a
    /// lens mounted the other way up from the one recording started on).
    pub flip: bool,
}

impl Input {
    /// Rows padded past the width (the main camera's 2008 pixels in 8192
    /// bytes) are given to GStreamer as a wider frame, and the padding is
    /// cropped off with the rest: no copy and no video meta.
    fn padded_width(&self) -> u32 {
        let bpp = bytes_per_pixel(self.format) as u32;
        if self.stride.is_multiple_of(bpp) && self.stride / bpp >= self.width {
            self.stride / bpp
        } else {
            self.width
        }
    }

    fn bytes_needed(&self) -> usize {
        self.stride as usize * (self.height as usize).saturating_sub(1)
            + self.width as usize * bytes_per_pixel(self.format)
    }

    fn caps(&self) -> gst::Caps {
        gst::Caps::builder("video/x-raw")
            .field("format", self.format.caps_name())
            .field("width", self.padded_width() as i32)
            .field("height", self.height as i32)
            .field("framerate", gst::Fraction::new(0, 1))
            .build()
    }
}

/// What part of the lens's picture the file shows, in the frames' own
/// axes: `1 / scale` of the picture about `center` (0..1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub scale: f64,
    pub center: (f64, f64),
}

impl Default for View {
    fn default() -> Self {
        View {
            scale: 1.0,
            center: (0.5, 0.5),
        }
    }
}

/// Where a recording was made, for the file's location tag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    pub codec: Codec,
    /// The first camera's frames ([`Tap::set_input`] switches cameras).
    pub input: Input,
    pub out_width: u32,
    pub out_height: u32,
    pub fps: u32,
    /// Clockwise degrees that turn the picture upright; written as the
    /// file's orientation tag.
    pub rotation: u32,
    /// When recording started, ISO 8601 with the UTC offset: the file's
    /// creation time.
    pub created: Option<String>,
    /// Written as the file's location (MP4 `©xyz`) when given.
    pub location: Option<Location>,
    pub audio: Audio,
}

impl Config {
    /// Video bitrate: about 0.19 bits per pixel per frame (12 Mb/s at
    /// 1080p30, 5.3 Mb/s at 720p30).
    pub fn bitrate_kbps(&self) -> u32 {
        let bits = self.out_width as f64 * self.out_height as f64 * self.fps as f64 * 0.19;
        (bits / 1000.0).round().max(500.0) as u32
    }
}

/// The crop (left, right, top, bottom, in input pixels) that takes the
/// output's aspect out of the lens's picture, `1 / view.scale` of it about
/// `view.center`, moved by `shift` (shares of the picture), inside the
/// frame. A frame that covers only part of the picture (`coverage`) gives
/// what it has.
pub fn crop_for(
    input: &Input,
    out_w: u32,
    out_h: u32,
    view: View,
    shift: (f64, f64),
) -> (u32, u32, u32, u32) {
    let (in_w, in_h) = (input.width, input.height);
    let (iw, ih) = (in_w as f64, in_h as f64);
    let cov = |c: f64| {
        if c.is_finite() && c > 0.0 {
            c.min(1.0)
        } else {
            1.0
        }
    };
    let (pw, ph) = (iw / cov(input.coverage.0), ih / cov(input.coverage.1));
    let aspect = out_w as f64 / out_h.max(1) as f64;
    let (mut w, mut h) = if pw / ph > aspect {
        (ph * aspect, ph)
    } else {
        (pw, pw / aspect)
    };
    let s = if view.scale.is_finite() {
        view.scale.max(1.0)
    } else {
        1.0
    };
    w /= s;
    h /= s;
    let finite = |v: f64| if v.is_finite() { v } else { 0.5 };
    let cx = finite(view.center.0) + finite(shift.0) - 0.5;
    let cy = finite(view.center.1) + finite(shift.1) - 0.5;
    // The window's center in frame pixels.
    let (mx, my) = (iw / 2.0 + cx * pw, ih / 2.0 + cy * ph);
    // Even sizes and offsets for the chroma planes, inside the frame.
    let w = ((w.min(iw) as u32) & !1).clamp(2.min(in_w), in_w);
    let h = ((h.min(ih) as u32) & !1).clamp(2.min(in_h), in_h);
    let edge = |m: f64, size: u32, whole: u32| {
        let e = (m - size as f64 / 2.0).round().max(0.0) as u32;
        (e & !1).min((whole - size) & !1)
    };
    let left = edge(mx, w, in_w);
    let top = edge(my, h, in_h);
    (left, in_w - w - left, top, in_h - h - top)
}

fn orientation_tag(rotation: u32) -> &'static str {
    match rotation % 360 {
        90 => "rotate-90",
        180 => "rotate-180",
        270 => "rotate-270",
        _ => "rotate-0",
    }
}

/// Timing shared by the frame pushers, the audio probe and stop.
#[derive(Debug, Default)]
struct Timing {
    /// Running time of the first frame: the file's zero.
    first: Option<gst::ClockTime>,
    /// Running time of the last frame, once stopping: audio after it goes.
    end: Option<gst::ClockTime>,
    last: Option<gst::ClockTime>,
    frames: u64,
}

/// The cameras' frames on their way to the crop: every input seen, which
/// one frames are pushed for, the view, and each frame in flight with the
/// input and shift it was pushed with (the crop is set per frame, in the
/// streaming thread, as it reaches the crop).
#[derive(Debug)]
struct Feed {
    inputs: Vec<Input>,
    /// The input frames are pushed for now (an index into `inputs`).
    current: usize,
    /// The input whose caps appsrc has.
    caps_for: usize,
    /// Each input's view (each camera's crop of the same scene).
    views: Vec<View>,
    /// (pts, input, shift) of frames pushed and not yet cropped.
    in_flight: std::collections::VecDeque<(gst::ClockTime, usize, (f64, f64))>,
    /// What the crop and the flip are set to now.
    applied: Option<(usize, (u32, u32, u32, u32))>,
    flipped: bool,
}

struct Inner {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    crop: gst::Element,
    flip: gst::Element,
    /// The queues straight before the muxer: their pads carry the offset
    /// that puts the first frame at zero.
    video_out: gst::Pad,
    audio_out: Option<gst::Pad>,
    config: Config,
    timing: std::sync::Mutex<Timing>,
    feed: std::sync::Mutex<Feed>,
}

impl Inner {
    /// Set the crop (and the flip) for a frame of `input` shown with
    /// `shift`. Called in the streaming thread, before the crop sees it.
    fn apply(&self, feed: &mut Feed, input: usize, shift: (f64, f64)) {
        let Some(i) = feed.inputs.get(input).copied() else {
            return;
        };
        let c = &self.config;
        let view = feed.views.get(input).copied().unwrap_or_default();
        let crop = crop_for(&i, c.out_width, c.out_height, view, shift);
        if feed.applied != Some((input, crop)) {
            let (l, r, t, b) = crop;
            let padding = i.padded_width() - i.width;
            self.crop.set_property("left", l as i32);
            self.crop.set_property("right", (r + padding) as i32);
            self.crop.set_property("top", t as i32);
            self.crop.set_property("bottom", b as i32);
            feed.applied = Some((input, crop));
        }
        if feed.flipped != i.flip {
            self.flip
                .set_property_from_str("video-direction", if i.flip { "180" } else { "identity" });
            feed.flipped = i.flip;
        }
    }
}

/// A recording in progress.
pub struct Recorder {
    inner: std::sync::Arc<Inner>,
}

/// A handle that feeds frames from any thread (the camera's), so the
/// encoder gets every frame whatever the UI is doing. Cheap to clone.
#[derive(Clone)]
pub struct Tap {
    inner: std::sync::Arc<Inner>,
}

impl Recorder {
    /// Build and start the pipeline. With PipeWire audio, falls back to a
    /// silent recording when the microphone can't be opened.
    pub fn start(config: Config) -> Result<Recorder> {
        match Recorder::start_with(config.clone()) {
            Ok(r) => Ok(r),
            Err(e) if config.audio == Audio::PipeWire => {
                tracing::warn!(error = %e, "recording without sound");
                Recorder::start_with(Config {
                    audio: Audio::None,
                    ..config
                })
            }
            Err(e) => Err(e),
        }
    }

    fn start_with(config: Config) -> Result<Recorder> {
        if !config
            .input
            .stride
            .is_multiple_of(bytes_per_pixel(config.input.format) as u32)
        {
            return Err(Error::Pipeline("the stride isn't whole pixels".into()));
        }
        if !config.codec.needed().iter().all(|e| have(e)) {
            return Err(Error::NoEncoder);
        }
        let c = &config;
        let location = c.path.display().to_string().replace('"', "");
        // Generous queues before the muxer: it interleaves by time, and a
        // short audio queue that filled while it waited for video blocked
        // the microphone and lost sound.
        let big = "max-size-buffers=0 max-size-bytes=0 max-size-time=5000000000";
        let audio = match c.audio {
            Audio::None => String::new(),
            Audio::PipeWire => format!(
                "pipewiresrc ! queue {big} ! audioconvert ! audioresample \
                 ! audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=128000 \
                 ! queue name=aq {big} ! mux."
            ),
            Audio::Test => format!(
                "audiotestsrc is-live=true wave=sine ! audioconvert ! audioresample \
                 ! audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=64000 \
                 ! queue name=aq {big} ! mux."
            ),
        };
        // The camera's frames carry their own timestamps (set in push), so
        // appsrc doesn't stamp them. Conversion and scaling run on four
        // threads in one pass, and the encoder in its own thread behind a
        // short queue that drops rather than blocks the camera. The flip
        // turns a lens mounted the other way up after scaling, where the
        // frame is smallest.
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=false block=false \
             ! queue max-size-buffers=3 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! videocrop name=crop \
             ! videoconvertscale add-borders=false n-threads=4 \
             ! video/x-raw,format=I420,width={ow},height={oh},pixel-aspect-ratio=1/1 \
             ! videoflip name=flip video-direction=identity \
             ! queue max-size-buffers=3 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! {chain} ! taginject tags=\"{tags}\" \
             ! queue name=vq {big} ! {muxer} name=mux ! filesink async=false location=\"{location}\" \
             {audio}",
            ow = c.out_width,
            oh = c.out_height,
            chain = c.codec.chain(c.bitrate_kbps(), c.fps),
            tags = tags(c),
            muxer = c.codec.muxer(),
        );
        tracing::debug!(pipeline = desc, "recording pipeline");
        let pipeline = gst::parse::launch(&desc)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Pipeline("not a pipeline".into()))?;
        let by = |name: &str| {
            pipeline
                .by_name(name)
                .ok_or_else(|| Error::Pipeline(format!("no {name}")))
        };
        let src = by("src")?
            .downcast::<gst_app::AppSrc>()
            .map_err(|_| Error::Pipeline("src isn't an appsrc".into()))?;
        src.set_caps(Some(&c.input.caps()));
        let crop = by("crop")?;
        let flip = by("flip")?;
        let video_out = by("vq")?
            .static_pad("src")
            .ok_or_else(|| Error::Pipeline("no vq pad".into()))?;
        let aq = pipeline.by_name("aq");
        let audio_out = aq.as_ref().and_then(|q| q.static_pad("src"));
        let feed = Feed {
            inputs: vec![config.input],
            current: 0,
            caps_for: 0,
            views: vec![View::default()],
            in_flight: std::collections::VecDeque::new(),
            applied: None,
            flipped: false,
        };
        let inner = std::sync::Arc::new(Inner {
            pipeline,
            src,
            crop,
            flip,
            video_out,
            audio_out,
            config,
            timing: std::sync::Mutex::new(Timing::default()),
            feed: std::sync::Mutex::new(feed),
        });
        // Each frame's crop is set as it reaches the crop: a caps change
        // (another camera) first, so the crop fits the new size before
        // videocrop takes the caps, then each buffer by its timestamp.
        if let Some(sink) = inner.crop.static_pad("sink") {
            let weak = std::sync::Arc::downgrade(&inner);
            sink.add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |_, info| {
                    let Some(inner) = weak.upgrade() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let mut feed = inner.feed.lock().unwrap();
                    if let Some(buffer) = info.buffer() {
                        let pts = buffer.pts();
                        let mut found = None;
                        while let Some(&(t, input, shift)) = feed.in_flight.front() {
                            if Some(t) > pts {
                                break;
                            }
                            feed.in_flight.pop_front();
                            if Some(t) == pts {
                                found = Some((input, shift));
                            }
                        }
                        if let Some((input, shift)) = found {
                            inner.apply(&mut feed, input, shift);
                        }
                    } else if let Some(gst::EventView::Caps(caps)) = info.event().map(|e| e.view())
                    {
                        let size = caps.caps().structure(0).and_then(|s| {
                            Some((s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?))
                        });
                        // The newest input of that size.
                        let input = size.and_then(|(w, h)| {
                            feed.inputs
                                .iter()
                                .rposition(|i| i.padded_width() as i32 == w && i.height as i32 == h)
                        });
                        if let Some(input) = input {
                            let shift = feed
                                .in_flight
                                .iter()
                                .find(|f| f.1 == input)
                                .map(|f| f.2)
                                .unwrap_or((0.0, 0.0));
                            inner.apply(&mut feed, input, shift);
                        }
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        }
        // Sound before the first frame or after the last one is dropped,
        // so both tracks cover the same span.
        if let Some(sink) = aq.as_ref().and_then(|q| q.static_pad("sink")) {
            let weak = std::sync::Arc::downgrade(&inner);
            sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(inner) = weak.upgrade() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(buffer) = info.buffer() else {
                    return gst::PadProbeReturn::Ok;
                };
                let t = inner.timing.lock().unwrap();
                let keep = match (t.first, buffer.pts()) {
                    (Some(first), Some(pts)) => pts >= first && t.end.is_none_or(|end| pts <= end),
                    _ => false,
                };
                if keep {
                    gst::PadProbeReturn::Ok
                } else {
                    gst::PadProbeReturn::Drop
                }
            });
        }
        let rec = Recorder { inner };
        {
            let mut feed = rec.inner.feed.lock().unwrap();
            rec.inner.apply(&mut feed, 0, (0.0, 0.0));
        }
        rec.inner.pipeline.set_state(gst::State::Playing)?;
        // A source that can't open (no microphone) fails at once; wait a
        // moment for that, not for the whole start (frames before PLAYING
        // are dropped, and the file starts at the first one after).
        let _ = rec.inner.pipeline.state(gst::ClockTime::from_mseconds(500));
        if let Some(err) = rec.bus_error() {
            let _ = rec.inner.pipeline.set_state(gst::State::Null);
            return Err(Error::Pipeline(err));
        }
        let c = &rec.inner.config;
        tracing::info!(
            path = %c.path.display(),
            codec = ?c.codec,
            size = %format!("{}x{}", c.out_width, c.out_height),
            kbps = c.bitrate_kbps(),
            audio = ?c.audio,
            rotation = c.rotation,
            location = c.location.is_some(),
            "recording"
        );
        Ok(rec)
    }

    fn bus_error(&self) -> Option<String> {
        let bus = self.inner.pipeline.bus()?;
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(e) = msg.view() {
                return Some(format!("{} ({:?})", e.error(), e.debug()));
            }
        }
        None
    }

    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn frames(&self) -> u64 {
        self.inner.timing.lock().unwrap().frames
    }

    /// A handle for pushing frames from another thread.
    pub fn tap(&self) -> Tap {
        Tap {
            inner: self.inner.clone(),
        }
    }

    /// Zoom within the frame: `scale` 1 is the whole 16:9 view.
    pub fn set_zoom(&self, scale: f64) {
        self.tap().set_view(View {
            scale,
            ..View::default()
        });
    }

    /// Hand a frame to the encoder (see [`Tap::push`]).
    pub fn push<T: AsRef<[u8]> + Send + 'static>(&self, data: T) -> Result<()> {
        self.tap().push(data)
    }

    /// Finish the file: end of stream, wait for the muxer to write it.
    pub fn stop(self) -> Result<PathBuf> {
        let inner = &self.inner;
        {
            let mut t = inner.timing.lock().unwrap();
            t.end = t.last;
        }
        let _ = inner.src.end_of_stream();
        // The audio source needs its own end of stream.
        inner.pipeline.send_event(gst::event::Eos::new());
        let bus = inner
            .pipeline
            .bus()
            .ok_or_else(|| Error::Pipeline("no bus".into()))?;
        let mut result = Ok(());
        let deadline = gst::ClockTime::from_seconds(10);
        match bus.timed_pop_filtered(deadline, &[gst::MessageType::Eos, gst::MessageType::Error]) {
            Some(msg) => {
                if let gst::MessageView::Error(e) = msg.view() {
                    result = Err(Error::Pipeline(e.error().to_string()));
                }
            }
            None => result = Err(Error::Pipeline("the file wasn't finished in 10 s".into())),
        }
        inner.pipeline.set_state(gst::State::Null)?;
        let t = inner.timing.lock().unwrap();
        let span = match (t.first, t.last) {
            (Some(a), Some(b)) => b.saturating_sub(a).mseconds(),
            _ => 0,
        };
        let fps = if span > 0 {
            (t.frames.saturating_sub(1)) as f64 * 1000.0 / span as f64
        } else {
            0.0
        };
        if let (Ok(()), Some(l), "mp4") = (
            &result,
            inner.config.location,
            inner.config.codec.extension(),
        ) {
            if let Err(e) = add_location(&inner.config.path, l) {
                tracing::warn!(error = %e, "no location in the file");
            }
        }
        result.map(|()| {
            tracing::info!(
                path = %inner.config.path.display(),
                frames = t.frames,
                ms = span,
                fps = format!("{fps:.1}"),
                "recording saved"
            );
            inner.config.path.clone()
        })
    }
}

/// The tags taginject writes: orientation and creation time. (mp4mux
/// writes no location from tags; [`add_location`] does it after.)
fn tags(c: &Config) -> String {
    let mut t = format!("image-orientation={}", orientation_tag(c.rotation));
    if let Some(d) = &c.created {
        let safe: String = d
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || "-:+.".contains(*ch))
            .collect();
        t.push_str(&format!(",datetime=(datetime){safe}"));
    }
    t
}

impl Tap {
    /// The view of the current camera's frames (the zoom, and its center).
    pub fn set_view(&self, view: View) {
        let mut feed = self.inner.feed.lock().unwrap();
        let current = feed.current;
        if feed.views.get(current) == Some(&view) {
            return;
        }
        feed.views[current] = view;
        // Frames in flight take it as they reach the crop; with none (no
        // frame yet, or a still camera) set it now.
        if feed.in_flight.is_empty() {
            let input = feed.applied.map(|(i, _)| i).unwrap_or(feed.current);
            self.inner.apply(&mut feed, input, (0.0, 0.0));
        }
    }

    /// Frames come from another camera from now on (a lens switch while
    /// recording), shown as `view`. The file keeps its size, rate and
    /// clock: the new frames are cropped and scaled into the same picture.
    pub fn set_input(&self, input: Input, view: View) -> Result<()> {
        if !input
            .stride
            .is_multiple_of(bytes_per_pixel(input.format) as u32)
        {
            return Err(Error::Pipeline("the stride isn't whole pixels".into()));
        }
        let mut feed = self.inner.feed.lock().unwrap();
        if feed.inputs.get(feed.current) == Some(&input) {
            let current = feed.current;
            feed.views[current] = view;
            return Ok(());
        }
        feed.inputs.push(input);
        feed.views.push(view);
        feed.current = feed.inputs.len() - 1;
        Ok(())
    }

    /// Hand a frame to the encoder, shared, not copied (see
    /// [`Tap::push_shifted`]).
    pub fn push<T: AsRef<[u8]> + Send + 'static>(&self, data: T) -> Result<()> {
        self.push_shifted(data, (0.0, 0.0))
    }

    /// Hand a frame to the encoder with its view moved by `shift` (shares
    /// of the picture, in the frame's axes: stabilization). Its timestamp
    /// is the pipeline clock's running time now, the clock the
    /// microphone's sound is stamped with; the first frame becomes the
    /// file's zero. Frames before the pipeline runs, or after stop, are
    /// dropped, and so is a frame the encoder can't keep up with.
    pub fn push_shifted<T: AsRef<[u8]> + Send + 'static>(
        &self,
        data: T,
        shift: (f64, f64),
    ) -> Result<()> {
        let inner = &self.inner;
        let (input, serial, caps_change) = {
            let feed = inner.feed.lock().unwrap();
            let i = feed.inputs[feed.current];
            (i, feed.current, feed.caps_for != feed.current)
        };
        if data.as_ref().len() < input.bytes_needed() {
            return Err(Error::Pipeline("frame shorter than its size".into()));
        }
        let (Some(clock), Some(base)) = (inner.pipeline.clock(), inner.pipeline.base_time()) else {
            return Ok(()); // not running yet
        };
        let now = clock.time().saturating_sub(base);
        {
            let mut t = inner.timing.lock().unwrap();
            if t.end.is_some() {
                return Ok(());
            }
            if t.first.is_none() {
                t.first = Some(now);
                let offset = -(now.nseconds() as i64);
                inner.video_out.set_offset(offset);
                if let Some(a) = &inner.audio_out {
                    a.set_offset(offset);
                }
            }
            if t.last.is_some_and(|l| now <= l) {
                return Ok(()); // never two frames at one time
            }
            t.last = Some(now);
            t.frames += 1;
        }
        {
            let mut feed = inner.feed.lock().unwrap();
            if caps_change {
                inner.src.set_caps(Some(&input.caps()));
                feed.caps_for = serial;
            }
            feed.in_flight.push_back((now, serial, shift));
            // Frames the leaky queue dropped never reach the crop.
            while feed.in_flight.len() > 16 {
                feed.in_flight.pop_front();
            }
        }
        let mut buffer = gst::Buffer::from_slice(data);
        {
            let b = buffer.get_mut().expect("a new buffer");
            b.set_pts(now);
            b.set_dts(now);
        }
        inner
            .src
            .push_buffer(buffer)
            .map_err(|e| Error::Pipeline(format!("push: {e:?}")))?;
        Ok(())
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// ISO 6709 as phones write it in MP4 files: "+51.4779-000.0015/", with
/// the altitude before the slash when known; None for coordinates that
/// aren't real.
pub fn iso6709(l: &Location) -> Option<String> {
    if !(l.latitude.is_finite()
        && l.longitude.is_finite()
        && l.latitude.abs() <= 90.0
        && l.longitude.abs() <= 180.0)
    {
        return None;
    }
    let pad = |v: f64, int_digits: usize| {
        let sign = if v < 0.0 { '-' } else { '+' };
        format!("{sign}{:0width$.4}", v.abs(), width = int_digits + 5)
    };
    let mut s = format!("{}{}", pad(l.latitude, 2), pad(l.longitude, 3));
    if let Some(a) = l.altitude.filter(|a| a.is_finite()) {
        s.push_str(&format!(
            "{}{:.1}",
            if a < 0.0 { '-' } else { '+' },
            a.abs()
        ));
    }
    s.push('/');
    Some(s)
}

/// Write the location into a finished MP4: a `©xyz` box (as Android and
/// iOS write it) in `moov/udta`. Only where `moov` is the file's last box,
/// as mp4mux writes it, so growing it moves no media data.
pub fn add_location(path: &Path, l: Location) -> std::io::Result<()> {
    let Some(text) = iso6709(&l) else {
        return Ok(());
    };
    let mut data = std::fs::read(path)?;
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    // Top-level boxes: find moov, and check it's last.
    let mut at = 0usize;
    let mut moov = None;
    while at + 8 <= data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let size = match size {
            0 => data.len() - at,
            1 => {
                let big = data.get(at + 8..at + 16).ok_or_else(|| bad("short box"))?;
                u64::from_be_bytes(big.try_into().unwrap()) as usize
            }
            n => n,
        };
        if size < 8 || at + size > data.len() {
            return Err(bad("broken box"));
        }
        if &data[at + 4..at + 8] == b"moov" {
            moov = Some((at, size));
        }
        at += size;
    }
    let (moov_at, moov_size) = moov.ok_or_else(|| bad("no moov"))?;
    if moov_at + moov_size != data.len() {
        return Err(bad("moov isn't last"));
    }
    if u32::from_be_bytes(data[moov_at..moov_at + 4].try_into().unwrap()) == 1 {
        return Err(bad("64-bit moov"));
    }
    // The ©xyz box: length and language (0x15c7, as phones write), then
    // the text.
    let mut xyz = Vec::new();
    let body = 4 + text.len();
    xyz.extend_from_slice(&((8 + body) as u32).to_be_bytes());
    xyz.extend_from_slice(b"\xa9xyz");
    xyz.extend_from_slice(&(text.len() as u16).to_be_bytes());
    xyz.extend_from_slice(&0x15c7u16.to_be_bytes());
    xyz.extend_from_slice(text.as_bytes());
    let mut udta = Vec::new();
    udta.extend_from_slice(&((8 + xyz.len()) as u32).to_be_bytes());
    udta.extend_from_slice(b"udta");
    udta.extend_from_slice(&xyz);
    data.extend_from_slice(&udta);
    let new_size = (moov_size + udta.len()) as u32;
    data[moov_at..moov_at + 4].copy_from_slice(&new_size.to_be_bytes());
    let tmp = path.with_extension("mp4.tmp");
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, path)
}

fn bytes_per_pixel(f: InputFormat) -> usize {
    match f {
        InputFormat::Bgrx | InputFormat::Rgbx => 4,
        InputFormat::Bgr | InputFormat::Rgb => 3,
    }
}

/// `VID_YYYYMMDD_HHMMSS.<ext>` in `dir`, with `_n` added if taken.
pub fn next_path(dir: &Path, stamp: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("VID_{stamp}.{ext}"));
    if !first.exists() {
        return first;
    }
    (1..)
        .map(|n| dir.join(format!("VID_{stamp}_{n}.{ext}")))
        .find(|p| !p.exists())
        .expect("a free name")
}

/// How long a recording has run, as the UI shows it: "0:07", "12:34",
/// "1:02:03".
pub fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(w: u32, h: u32) -> Input {
        Input {
            format: InputFormat::Bgrx,
            width: w,
            height: h,
            stride: w * 4,
            coverage: (1.0, 1.0),
            flip: false,
        }
    }

    fn zoom(scale: f64) -> View {
        View {
            scale,
            ..View::default()
        }
    }

    #[test]
    fn crop_takes_16_9_from_4_3_and_zooms_about_the_center() {
        // The ultrawide's 2008x1508 to 1920x1080: full width, 1129 rows.
        let uw = input(2008, 1508);
        let (l, r, t, b) = crop_for(&uw, 1920, 1080, zoom(1.0), (0.0, 0.0));
        assert_eq!((l, r), (0, 0));
        let h = 1508 - t - b;
        assert!((h as i32 - 1129).abs() <= 1, "{h}");
        assert!(t.abs_diff(b) <= 2);
        // At 2x, half of that each way, centered.
        let (l, r, t, b) = crop_for(&uw, 1920, 1080, zoom(2.0), (0.0, 0.0));
        assert_eq!(2008 - l - r, 1004);
        assert!(l.abs_diff(r) <= 2 && t.abs_diff(b) <= 2);
        // Below 1 never widens past the frame.
        let main = input(2008, 1136);
        assert_eq!(
            crop_for(&main, 1920, 1080, zoom(0.5), (0.0, 0.0)),
            crop_for(&main, 1920, 1080, zoom(1.0), (0.0, 0.0))
        );
    }

    #[test]
    fn crops_are_even_and_inside_the_frame() {
        for &(w, h) in &[(2008, 1136), (2008, 1508), (1912, 1368), (640, 480)] {
            for s in [1.0, 1.3, 2.0, 3.7, 10.0] {
                for shift in [(0.0, 0.0), (0.04, -0.04), (-0.5, 0.5), (f64::NAN, 0.0)] {
                    let (l, r, t, b) = crop_for(&input(w, h), 1280, 720, zoom(s), shift);
                    assert!(l + r < w && t + b < h);
                    assert_eq!((w - l - r) % 2, 0);
                    assert_eq!((h - t - b) % 2, 0);
                    assert_eq!(l % 2, 0);
                    assert_eq!(t % 2, 0);
                }
            }
        }
    }

    #[test]
    fn crops_follow_the_center_the_shift_and_the_coverage() {
        let i = input(2000, 1500);
        // At 2x, a center 0.1 right of the middle moves the crop 200 px.
        let (l0, ..) = crop_for(&i, 1600, 1200, zoom(2.0), (0.0, 0.0));
        let right = View {
            scale: 2.0,
            center: (0.6, 0.5),
        };
        let (l1, ..) = crop_for(&i, 1600, 1200, right, (0.0, 0.0));
        assert_eq!(l1 - l0, 200);
        // A shift does the same.
        let (l2, ..) = crop_for(&i, 1600, 1200, zoom(2.0), (0.1, 0.0));
        assert_eq!(l2, l1);
        // A 16:9 frame that covers 74 % of a 4:3 picture's height, into
        // 16:9 at 1x: the whole frame.
        let main = Input {
            coverage: (0.988, 0.74),
            ..input(2016, 1136)
        };
        let (l, r, t, b) = crop_for(&main, 1920, 1080, zoom(1.0), (0.0, 0.0));
        assert!(l + r <= 2 && t + b <= 2, "{l} {r} {t} {b}");
        // At 2x the main crops the same share of the scene as a 4:3 lens
        // covering the whole picture would at 2x: half of the picture's
        // width, which is 0.5 / 0.988 of the frame's.
        let (l, r, ..) = crop_for(&main, 1920, 1080, zoom(2.0), (0.0, 0.0));
        let w = 2016 - l - r;
        assert!((w as f64 - 2016.0 * 0.5 / 0.988).abs() <= 2.0, "{w}");
    }

    #[test]
    fn locations_read_as_phones_write_them() {
        let g = Location {
            latitude: 51.4779,
            longitude: -0.0015,
            altitude: Some(46.0),
        };
        assert_eq!(iso6709(&g).unwrap(), "+51.4779-000.0015+46.0/");
        let zero = Location {
            latitude: 0.0,
            longitude: 0.0,
            altitude: None,
        };
        assert_eq!(iso6709(&zero).unwrap(), "+00.0000+000.0000/");
        assert!(iso6709(&Location {
            latitude: f64::NAN,
            ..zero
        })
        .is_none());
        assert!(iso6709(&Location {
            latitude: 91.0,
            ..zero
        })
        .is_none());
    }

    #[test]
    fn the_location_box_goes_into_a_last_moov() {
        let dir = std::env::temp_dir().join(format!("omacam-xyz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.mp4");
        // ftyp, mdat, then moov with one child.
        let boxed = |kind: &[u8], body: &[u8]| {
            let mut b = ((8 + body.len()) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(kind);
            b.extend_from_slice(body);
            b
        };
        let mut file = boxed(b"ftyp", b"isom");
        file.extend(boxed(b"mdat", &[7; 20]));
        file.extend(boxed(b"moov", &boxed(b"mvhd", &[0; 12])));
        std::fs::write(&path, &file).unwrap();
        let zero = Location {
            latitude: 0.0,
            longitude: 0.0,
            altitude: None,
        };
        add_location(&path, zero).unwrap();
        let out = std::fs::read(&path).unwrap();
        let moov_at = 12 + 28;
        let size = u32::from_be_bytes(out[moov_at..moov_at + 4].try_into().unwrap()) as usize;
        assert_eq!(moov_at + size, out.len(), "moov grew to the end");
        assert!(out.windows(4).any(|w| w == b"udta"));
        assert!(out.ends_with(b"+00.0000+000.0000/"));
        assert_eq!(&out[..moov_at], &file[..moov_at], "media untouched");
        // A moov before the media is left alone.
        let mut early = boxed(b"ftyp", b"isom");
        early.extend(boxed(b"moov", &boxed(b"mvhd", &[0; 12])));
        early.extend(boxed(b"mdat", &[7; 20]));
        std::fs::write(&path, &early).unwrap();
        assert!(add_location(&path, zero).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), early);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bitrates_scale_with_size() {
        let cfg = |w, h| Config {
            path: "x".into(),
            codec: Codec::X264,
            input: Input {
                stride: 8192,
                ..input(2008, 1136)
            },
            out_width: w,
            out_height: h,
            fps: 30,
            rotation: 270,
            created: None,
            location: None,
            audio: Audio::None,
        };
        assert!((11_000..13_000).contains(&cfg(1920, 1080).bitrate_kbps()));
        assert!((5_000..6_000).contains(&cfg(1280, 720).bitrate_kbps()));
    }

    #[test]
    fn elapsed_reads_like_a_clock() {
        assert_eq!(format_elapsed(Duration::from_secs(7)), "0:07");
        assert_eq!(format_elapsed(Duration::from_secs(754)), "12:34");
        assert_eq!(format_elapsed(Duration::from_secs(3723)), "1:02:03");
    }

    #[test]
    fn orientation_tags() {
        assert_eq!(orientation_tag(270), "rotate-270");
        assert_eq!(orientation_tag(90), "rotate-90");
        assert_eq!(orientation_tag(0), "rotate-0");
    }

    /// The video frames' and the sound's timestamps in an MP4, in seconds:
    /// every video pts, and the sound's first and last pts with the count
    /// of gaps over 30 ms between packets.
    fn read_back(path: &Path, with_sound: bool) -> (Vec<f64>, Option<(f64, f64, usize)>) {
        let desc = format!(
            "filesrc location=\"{}\" ! qtdemux name=d d.video_0 ! queue ! appsink name=v sync=false {}",
            path.display(),
            if with_sound { "d.audio_0 ! queue ! appsink name=a sync=false" } else { "" }
        );
        let p = gst::parse::launch(&desc)
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
        let sink = |n: &str| {
            p.by_name(n)
                .map(|e| e.downcast::<gst_app::AppSink>().unwrap())
        };
        let (v, a) = (sink("v").unwrap(), sink("a"));
        p.set_state(gst::State::Playing).unwrap();
        let pts = |s: &gst_app::AppSink| {
            let mut out = Vec::new();
            while let Ok(sample) = s.pull_sample() {
                let b = sample.buffer().unwrap();
                out.push((
                    b.pts().unwrap().nseconds() as f64 / 1e9,
                    b.duration()
                        .map(|d| d.nseconds() as f64 / 1e9)
                        .unwrap_or(0.0),
                ));
            }
            out
        };
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(a) = a {
            std::thread::spawn(move || {
                let _ = tx.send(pts(&a));
            });
        }
        let video: Vec<f64> = pts(&v).into_iter().map(|(t, _)| t).collect();
        let sound = rx.recv().ok().map(|s| {
            let gaps = s
                .windows(2)
                .filter(|w| w[1].0 - (w[0].0 + w[0].1) > 0.03)
                .count();
            (s[0].0, s[s.len() - 1].0 + s[s.len() - 1].1, gaps)
        });
        p.set_state(gst::State::Null).unwrap();
        (video, sound)
    }

    /// Encode a second of synthetic frames (with a test tone when the
    /// codec allows) and check the file. Runs where the encoder plugins
    /// are installed (scripts/record-test.sh runs it in a container that
    /// has them); skipped elsewhere.
    #[test]
    fn records_a_playable_file() {
        init().unwrap();
        let Some(codec) = best_codec() else {
            eprintln!("skipped: no video encoder plugins here");
            return;
        };
        let audio = if have("opusenc") && have("audiotestsrc") {
            Audio::Test
        } else {
            Audio::None
        };
        let dir = std::env::temp_dir().join(format!("omacam-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = next_path(&dir, "test", codec.extension());
        // Padded rows, as the main camera's are.
        let (w, h, stride) = (320u32, 240u32, 336 * 4);
        let rec = Recorder::start(Config {
            path: path.clone(),
            codec,
            input: Input {
                format: InputFormat::Bgrx,
                width: w,
                height: h,
                stride,
                coverage: (1.0, 1.0),
                flip: false,
            },
            out_width: 320,
            out_height: 180,
            fps: 30,
            rotation: 270,
            created: Some("2026-10-14T15:24:10.123-04:00".into()),
            // Neutral test coordinates: the Royal Observatory, Greenwich.
            location: Some(Location {
                latitude: 51.4779,
                longitude: -0.0015,
                altitude: None,
            }),
            audio,
        })
        .unwrap();
        let tap = rec.tap();
        let mut pushed = Vec::new();
        for i in 0..45u32 {
            if i < 25 {
                let frame: Vec<u8> = (0..stride / 4 * h)
                    .flat_map(|p| [(p % w) as u8, (i * 8) as u8, (p / w) as u8, 255])
                    .collect();
                // Stabilization moves the crop a little each frame.
                let wobble = if i % 2 == 0 { 0.02 } else { -0.02 };
                tap.push_shifted(frame, (wobble, -wobble)).unwrap();
            } else {
                // Another camera from frame 25: a 16:9 frame mounted the
                // other way up, unpadded, covering 74 % of its picture's
                // height. The file goes on at the same size and clock.
                let (w2, h2) = (400u32, 226u32);
                if i == 25 {
                    tap.set_input(
                        Input {
                            format: InputFormat::Bgrx,
                            width: w2,
                            height: h2,
                            stride: w2 * 4,
                            coverage: (1.0, 0.74),
                            flip: true,
                        },
                        View {
                            scale: 1.2,
                            center: (0.5, 0.5),
                        },
                    )
                    .unwrap();
                }
                let frame: Vec<u8> = (0..w2 * h2)
                    .flat_map(|p| [(p % w2) as u8, (i * 8) as u8, (p / w2) as u8, 255])
                    .collect();
                tap.push(frame).unwrap();
            }
            pushed.push(std::time::Instant::now());
            // Zooms whose crops round to aspects a little off 16:9: the
            // stream must not renegotiate (mp4mux refuses that).
            if i == 10 {
                rec.set_zoom(1.37);
            }
            if i == 20 {
                tap.set_view(View {
                    scale: 2.9,
                    center: (0.4, 0.55),
                });
            }
            std::thread::sleep(Duration::from_millis(33));
        }
        let saved = rec.stop().unwrap();
        let bytes = std::fs::read(&saved).unwrap();
        assert!(bytes.len() > 1000, "{} bytes", bytes.len());
        match codec.extension() {
            "mp4" => {
                assert_eq!(&bytes[4..8], b"ftyp");
                assert!(bytes.windows(4).any(|w| w == b"moov"), "has a moov");
                assert!(bytes.windows(4).any(|w| w == b"avc1"), "H.264 track");
                assert!(bytes.windows(4).any(|w| w == b"\xa9xyz"), "a location atom");
                assert!(
                    bytes.windows(8).any(|w| w == b"+51.4779"),
                    "Greenwich's latitude in it"
                );
                if audio == Audio::Test {
                    assert!(bytes.windows(4).any(|w| w == b"Opus"), "Opus track");
                }
                let (video, sound) = read_back(&saved, audio == Audio::Test);
                // 45 frames pushed about every 33 ms, across a camera
                // switch: all kept, from zero, at the rate they were pushed (a busy machine pushes
                // slower than 30; the timestamps must follow).
                assert!(video.len() >= 40, "{} video frames", video.len());
                assert!(video[0] < 0.02, "video starts at {}", video[0]);
                let span = video[video.len() - 1] - video[0];
                let fps = (video.len() - 1) as f64 / span;
                let wall = pushed[pushed.len() - 1]
                    .duration_since(pushed[0])
                    .as_secs_f64();
                let pushed_fps = (pushed.len() - 1) as f64 / wall;
                assert!(
                    (fps - pushed_fps).abs() / pushed_fps < 0.1,
                    "{fps:.1} fps in the file, {pushed_fps:.1} pushed"
                );
                if let Some((first, last, gaps)) = sound {
                    assert!((-0.001..0.1).contains(&first), "sound starts at {first}");
                    assert!(last <= span + 0.15, "sound ends at {last}, video at {span}");
                    assert!(last > span - 0.3, "sound ends early at {last}");
                    assert_eq!(gaps, 0, "gaps in the sound");
                }
            }
            _ => assert_eq!(
                &bytes[..4],
                &[0x1a, 0x45, 0xdf, 0xa3],
                "Matroska/WebM header"
            ),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
