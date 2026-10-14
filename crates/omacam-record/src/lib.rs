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
            Codec::X264 => &["x264enc", "mp4mux", "videocrop", "taginject"],
            Codec::OpenH264 => &[
                "openh264enc",
                "h264parse",
                "mp4mux",
                "videocrop",
                "taginject",
            ],
            Codec::Vp8 => &["vp8enc", "webmmux", "videocrop", "taginject"],
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

#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    pub codec: Codec,
    pub input: InputFormat,
    pub in_width: u32,
    pub in_height: u32,
    pub stride: u32,
    pub out_width: u32,
    pub out_height: u32,
    pub fps: u32,
    /// Clockwise degrees that turn the picture upright; written as the
    /// file's orientation tag.
    pub rotation: u32,
    pub audio: Audio,
}

impl Config {
    /// Rows padded past the width (the main camera's 2008 pixels in 8192
    /// bytes) are given to GStreamer as a wider frame, and the padding is
    /// cropped off with the rest: no copy and no video meta.
    fn padded_width(&self) -> u32 {
        let bpp = bytes_per_pixel(self.input) as u32;
        if self.stride.is_multiple_of(bpp) && self.stride / bpp >= self.in_width {
            self.stride / bpp
        } else {
            self.in_width
        }
    }

    /// Video bitrate: about 0.15 bits per pixel per frame (12 Mb/s at
    /// 1080p30, 5.5 Mb/s at 720p30).
    pub fn bitrate_kbps(&self) -> u32 {
        let bits = self.out_width as f64 * self.out_height as f64 * self.fps as f64 * 0.19;
        (bits / 1000.0).round().max(500.0) as u32
    }
}

/// The crop (left, right, top, bottom, in input pixels) that takes the
/// output's aspect out of the centre of the frame and then zooms by
/// `scale` (1 = no zoom).
pub fn crop_for(in_w: u32, in_h: u32, out_w: u32, out_h: u32, scale: f64) -> (u32, u32, u32, u32) {
    let (iw, ih) = (in_w as f64, in_h as f64);
    let aspect = out_w as f64 / out_h.max(1) as f64;
    let (mut w, mut h) = if iw / ih > aspect {
        (ih * aspect, ih)
    } else {
        (iw, iw / aspect)
    };
    let s = if scale.is_finite() {
        scale.max(1.0)
    } else {
        1.0
    };
    w /= s;
    h /= s;
    // Even sizes for the chroma planes.
    let w = ((w as u32) & !1).clamp(2.min(in_w), in_w);
    let h = ((h as u32) & !1).clamp(2.min(in_h), in_h);
    let left = ((in_w - w) / 2) & !1;
    let top = ((in_h - h) / 2) & !1;
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

/// A recording in progress.
pub struct Recorder {
    pipeline: gst::Pipeline,
    src: gst_app::AppSrc,
    crop: gst::Element,
    config: Config,
    frames: u64,
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
            .stride
            .is_multiple_of(bytes_per_pixel(config.input) as u32)
        {
            return Err(Error::Pipeline("the stride isn't whole pixels".into()));
        }
        if !config.codec.needed().iter().all(|e| have(e)) {
            return Err(Error::NoEncoder);
        }
        let c = &config;
        let location = c.path.display().to_string().replace('"', "");
        let audio = match c.audio {
            Audio::None => String::new(),
            Audio::PipeWire => {
                "pipewiresrc do-timestamp=true ! queue ! audioconvert ! audioresample \
                 ! audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=128000 ! queue ! mux."
                    .into()
            }
            Audio::Test => "audiotestsrc is-live=true wave=sine ! audioconvert ! audioresample \
                 ! audio/x-raw,rate=48000,channels=2 ! opusenc bitrate=64000 ! queue ! mux."
                .into(),
        };
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=true block=false \
               caps=video/x-raw,format={fmt},width={iw},height={ih},framerate=0/1 \
             ! queue max-size-buffers=4 max-size-bytes=0 max-size-time=0 leaky=downstream \
             ! videocrop name=crop ! videoscale add-borders=false ! videoconvert \
             ! video/x-raw,format=I420,width={ow},height={oh},pixel-aspect-ratio=1/1 \
             ! {chain} ! taginject tags=\"image-orientation={orient}\" \
             ! queue ! {muxer} name=mux ! filesink location=\"{location}\" \
             {audio}",
            fmt = c.input.caps_name(),
            iw = c.padded_width(),
            ih = c.in_height,
            ow = c.out_width,
            oh = c.out_height,
            chain = c.codec.chain(c.bitrate_kbps(), c.fps),
            orient = orientation_tag(c.rotation),
            muxer = c.codec.muxer(),
        );
        tracing::debug!(pipeline = desc, "recording pipeline");
        let pipeline = gst::parse::launch(&desc)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| Error::Pipeline("not a pipeline".into()))?;
        let src = pipeline
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| Error::Pipeline("no appsrc".into()))?;
        let crop = pipeline
            .by_name("crop")
            .ok_or_else(|| Error::Pipeline("no videocrop".into()))?;
        let rec = Recorder {
            pipeline,
            src,
            crop,
            frames: 0,
            config,
        };
        rec.set_zoom(1.0);
        rec.pipeline.set_state(gst::State::Playing)?;
        // A source that can't start (no microphone) fails here.
        let (res, _, _) = rec.pipeline.state(gst::ClockTime::from_seconds(3));
        if let Some(err) = rec.bus_error() {
            let _ = rec.pipeline.set_state(gst::State::Null);
            return Err(Error::Pipeline(err));
        }
        res?;
        tracing::info!(
            path = %rec.config.path.display(),
            codec = ?rec.config.codec,
            size = %format!("{}x{}", rec.config.out_width, rec.config.out_height),
            kbps = rec.config.bitrate_kbps(),
            audio = ?rec.config.audio,
            "recording"
        );
        Ok(rec)
    }

    fn bus_error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(e) = msg.view() {
                return Some(format!("{} ({:?})", e.error(), e.debug()));
            }
        }
        None
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Zoom within the frame: `scale` 1 is the whole 16:9 view.
    pub fn set_zoom(&self, scale: f64) {
        let c = &self.config;
        let (l, r, t, b) = crop_for(c.in_width, c.in_height, c.out_width, c.out_height, scale);
        let padding = c.padded_width() - c.in_width;
        self.crop.set_property("left", l as i32);
        self.crop.set_property("right", (r + padding) as i32);
        self.crop.set_property("top", t as i32);
        self.crop.set_property("bottom", b as i32);
    }

    /// Hand a frame to the encoder. `data` is shared, not copied (a
    /// `glib::Bytes` the viewfinder also shows works). A frame the encoder
    /// can't keep up with is dropped by the queue.
    pub fn push<T: AsRef<[u8]> + Send + 'static>(&mut self, data: T) -> Result<()> {
        let c = &self.config;
        let need = c.stride as usize * (c.in_height as usize - 1)
            + c.in_width as usize * bytes_per_pixel(c.input);
        if data.as_ref().len() < need {
            return Err(Error::Pipeline("frame shorter than its size".into()));
        }
        let buffer = gst::Buffer::from_slice(data);
        self.src
            .push_buffer(buffer)
            .map_err(|e| Error::Pipeline(format!("push: {e:?}")))?;
        self.frames += 1;
        Ok(())
    }

    /// Finish the file: end of stream, wait for the muxer to write it.
    pub fn stop(self) -> Result<PathBuf> {
        let _ = self.src.end_of_stream();
        // The audio source needs its own end of stream.
        self.pipeline.send_event(gst::event::Eos::new());
        let bus = self
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
        self.pipeline.set_state(gst::State::Null)?;
        result.map(|()| {
            tracing::info!(path = %self.config.path.display(), frames = self.frames, "recording saved");
            self.config.path.clone()
        })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
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

    #[test]
    fn crop_takes_16_9_from_4_3_and_zooms_about_the_centre() {
        // The ultrawide's 2008x1508 to 1920x1080: full width, 1129 rows.
        let (l, r, t, b) = crop_for(2008, 1508, 1920, 1080, 1.0);
        assert_eq!((l, r), (0, 0));
        let h = 1508 - t - b;
        assert!((h as i32 - 1129).abs() <= 1, "{h}");
        assert!(t.abs_diff(b) <= 2);
        // At 2x, half of that each way, centred.
        let (l, r, t, b) = crop_for(2008, 1508, 1920, 1080, 2.0);
        assert_eq!(2008 - l - r, 1004);
        assert!(l.abs_diff(r) <= 2 && t.abs_diff(b) <= 2);
        // Below 1 never widens past the frame.
        assert_eq!(
            crop_for(2008, 1136, 1920, 1080, 0.5),
            crop_for(2008, 1136, 1920, 1080, 1.0)
        );
    }

    #[test]
    fn crops_are_even_and_inside_the_frame() {
        for &(w, h) in &[(2008, 1136), (2008, 1508), (1912, 1368), (640, 480)] {
            for s in [1.0, 1.3, 2.0, 3.7, 10.0] {
                let (l, r, t, b) = crop_for(w, h, 1280, 720, s);
                assert!(l + r < w && t + b < h);
                assert_eq!((w - l - r) % 2, 0);
                assert_eq!((h - t - b) % 2, 0);
            }
        }
    }

    #[test]
    fn bitrates_scale_with_size() {
        let cfg = |w, h| Config {
            path: "x".into(),
            codec: Codec::X264,
            input: InputFormat::Bgrx,
            in_width: 2008,
            in_height: 1136,
            stride: 8192,
            out_width: w,
            out_height: h,
            fps: 30,
            rotation: 270,
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
        let mut rec = Recorder::start(Config {
            path: path.clone(),
            codec,
            input: InputFormat::Bgrx,
            in_width: w,
            in_height: h,
            stride,
            out_width: 320,
            out_height: 180,
            fps: 30,
            rotation: 270,
            audio,
        })
        .unwrap();
        for i in 0..30u32 {
            let frame: Vec<u8> = (0..stride / 4 * h)
                .flat_map(|p| [(p % w) as u8, (i * 8) as u8, (p / w) as u8, 255])
                .collect();
            rec.push(frame).unwrap();
            // Zooms whose crops round to aspects a little off 16:9: the
            // stream must not renegotiate (mp4mux refuses that).
            if i == 10 {
                rec.set_zoom(1.37);
            }
            if i == 20 {
                rec.set_zoom(2.9);
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
                if audio == Audio::Test {
                    assert!(bytes.windows(4).any(|w| w == b"Opus"), "Opus track");
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
