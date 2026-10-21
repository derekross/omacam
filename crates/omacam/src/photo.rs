//! Turning a captured frame into a JPEG file: crop to the zoom, turn
//! upright (so the file needs no EXIF orientation), encode with
//! libjpeg-turbo, and make a thumbnail. Runs off the UI thread.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use omacam_core::backend::PixelLayout;

/// Side of the square thumbnail (twice the button, for scale 2).
pub const THUMB: u32 = 112;

/// A frame's pixels, as the backend gave them.
pub struct Raw<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub layout: PixelLayout,
}

/// Packed 4-byte BGRX pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Bgrx {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// The centered crop rectangle for a zoom crop factor: (x, y, w, h), even.
pub fn crop_rect(width: u32, height: u32, crop: f64) -> (u32, u32, u32, u32) {
    let crop = if crop.is_finite() { crop.max(1.0) } else { 1.0 };
    let w = (((width as f64 / crop) as u32) & !1).clamp(2.min(width), width);
    let h = (((height as f64 / crop) as u32) & !1).clamp(2.min(height), height);
    (((width - w) / 2) & !1, ((height - h) / 2) & !1, w, h)
}

fn pixel(raw: &Raw<'_>, x: u32, y: u32) -> [u8; 4] {
    let bpp = raw.layout.bytes_per_pixel() as usize;
    let i = y as usize * raw.stride as usize + x as usize * bpp;
    let p = &raw.data[i..i + bpp];
    // Out: B, G, R, X.
    match raw.layout {
        PixelLayout::Xrgb8888 | PixelLayout::Rgb888 => [p[0], p[1], p[2], 255],
        PixelLayout::Xbgr8888 | PixelLayout::Bgr888 => [p[2], p[1], p[0], 255],
    }
}

/// A fractional window (x, y, w, h) on a `width` x `height` frame as
/// whole, even pixels inside it.
pub fn window_rect(width: u32, height: u32, window: (f64, f64, f64, f64)) -> (u32, u32, u32, u32) {
    let (x, y, w, h) = window;
    let finite = |v: f64, d: f64| if v.is_finite() { v } else { d };
    let size = |v: f64, whole: u32| {
        (finite(v, whole as f64).max(0.0) as u32 & !1).clamp(2.min(whole), whole)
    };
    let (cw, ch) = (size(w, width), size(h, height));
    let edge = |v: f64, size: u32, whole: u32| {
        ((finite(v, 0.0).round().max(0.0) as u32) & !1).min((whole - size) & !1)
    };
    (edge(x, cw, width), edge(y, ch, height), cw, ch)
}

/// Crop by `crop` about the center and turn clockwise by `rotation`.
pub fn crop_and_rotate(raw: &Raw<'_>, crop: f64, rotation: u32) -> Result<Bgrx> {
    crop_window_and_rotate(raw, crop_rect(raw.width, raw.height, crop), rotation)
}

/// Crop to `rect` (x, y, w, h, inside the frame) and turn clockwise by
/// `rotation`.
pub fn crop_window_and_rotate(
    raw: &Raw<'_>,
    rect: (u32, u32, u32, u32),
    rotation: u32,
) -> Result<Bgrx> {
    let bpp = raw.layout.bytes_per_pixel();
    if raw.stride < raw.width * bpp
        || (raw.data.len() as u64)
            < raw.stride as u64 * (raw.height as u64 - 1) + (raw.width * bpp) as u64
    {
        bail!("frame data is shorter than its size says");
    }
    let (cx, cy, cw, ch) = rect;
    if cw == 0 || ch == 0 || cx + cw > raw.width || cy + ch > raw.height {
        bail!("crop outside the frame");
    }
    let rotation = rotation % 360;
    let (ow, oh) = if rotation % 180 == 90 {
        (ch, cw)
    } else {
        (cw, ch)
    };
    let mut out = vec![0u8; ow as usize * oh as usize * 4];
    for oy in 0..oh {
        let row = &mut out[(oy * ow * 4) as usize..((oy + 1) * ow * 4) as usize];
        for ox in 0..ow {
            // The source pixel that lands at (ox, oy) after turning
            // clockwise by `rotation`.
            let (sx, sy) = match rotation {
                90 => (oy, ch - 1 - ox),
                180 => (cw - 1 - ox, ch - 1 - oy),
                270 => (cw - 1 - oy, ox),
                _ => (ox, oy),
            };
            let px = pixel(raw, cx + sx, cy + sy);
            row[ox as usize * 4..ox as usize * 4 + 4].copy_from_slice(&px);
        }
    }
    Ok(Bgrx {
        width: ow,
        height: oh,
        data: out,
    })
}

/// The largest centered part of `img` with `aspect` (width over height).
pub fn crop_to_aspect(img: Bgrx, aspect: Option<f64>) -> Bgrx {
    let Some(a) = aspect.filter(|a| a.is_finite() && *a > 0.0) else {
        return img;
    };
    let (w, h) = (img.width as f64, img.height as f64);
    let (cw, ch) = if w / h > a { (h * a, h) } else { (w, w / a) };
    let (cw, ch) = ((cw as u32).max(1) & !1, (ch as u32).max(1) & !1);
    let (cw, ch) = (cw.clamp(1, img.width), ch.clamp(1, img.height));
    if (cw, ch) == (img.width, img.height) {
        return img;
    }
    let (x0, y0) = ((img.width - cw) / 2, (img.height - ch) / 2);
    let mut data = Vec::with_capacity((cw * ch * 4) as usize);
    for y in y0..y0 + ch {
        let row = (y * img.width + x0) as usize * 4;
        data.extend_from_slice(&img.data[row..row + cw as usize * 4]);
    }
    Bgrx {
        width: cw,
        height: ch,
        data,
    }
}

/// Half the width and height, each output pixel the mean of four.
pub fn half(img: &Bgrx) -> Bgrx {
    let (w, h) = ((img.width / 2).max(1), (img.height / 2).max(1));
    let mut data = vec![0u8; (w * h * 4) as usize];
    let src =
        |x: u32, y: u32| ((y.min(img.height - 1) * img.width + x.min(img.width - 1)) * 4) as usize;
    for y in 0..h {
        for x in 0..w {
            let d = ((y * w + x) * 4) as usize;
            for c in 0..4 {
                let sum = img.data[src(2 * x, 2 * y) + c] as u32
                    + img.data[src(2 * x + 1, 2 * y) + c] as u32
                    + img.data[src(2 * x, 2 * y + 1) + c] as u32
                    + img.data[src(2 * x + 1, 2 * y + 1) + c] as u32;
                data[d + c] = ((sum + 2) / 4) as u8;
            }
        }
    }
    Bgrx {
        width: w,
        height: h,
        data,
    }
}

/// Mirror left-right in place.
pub fn mirror(img: &mut Bgrx) {
    let w = img.width as usize;
    for row in img.data.chunks_exact_mut(w * 4) {
        for x in 0..w / 2 {
            let (a, b) = (x * 4, (w - 1 - x) * 4);
            for c in 0..4 {
                row.swap(a + c, b + c);
            }
        }
    }
}

pub fn encode_jpeg(img: &Bgrx, quality: i32) -> Result<Vec<u8>> {
    let image = turbojpeg::Image {
        pixels: img.data.as_slice(),
        width: img.width as usize,
        pitch: img.width as usize * 4,
        height: img.height as usize,
        format: turbojpeg::PixelFormat::BGRX,
    };
    let jpeg = turbojpeg::compress(image, quality.clamp(1, 100), turbojpeg::Subsamp::Sub2x2)
        .context("JPEG encoding failed")?;
    Ok(jpeg.to_vec())
}

/// A small square copy for the round gallery button: the center square,
/// scaled to [`THUMB`] pixels (nearest neighbor; it's tiny).
pub fn thumbnail(img: &Bgrx) -> Bgrx {
    let side = img.width.min(img.height).max(1);
    let (x0, y0) = ((img.width - side) / 2, (img.height - side) / 2);
    let n = THUMB.min(side);
    let mut data = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let sx = x0 as usize + (x as u64 * side as u64 / n as u64) as usize;
            let sy = y0 as usize + (y as u64 * side as u64 / n as u64) as usize;
            let si = (sy * img.width as usize + sx) * 4;
            let di = ((y * n + x) * 4) as usize;
            data[di..di + 4].copy_from_slice(&img.data[si..si + 4]);
        }
    }
    Bgrx {
        width: n,
        height: n,
        data,
    }
}

/// `IMG_YYYYMMDD_HHMMSS.jpg` in `dir`, with `_n` added if it exists.
pub fn next_path(dir: &Path, stamp: &str) -> PathBuf {
    let first = dir.join(format!("IMG_{stamp}.jpg"));
    if !first.exists() {
        return first;
    }
    (1..)
        .map(|n| dir.join(format!("IMG_{stamp}_{n}.jpg")))
        .find(|p| !p.exists())
        .expect("a free name")
}

/// Write `jpeg` without leaving a partial file behind.
pub fn write_atomically(path: &Path, jpeg: &[u8]) -> Result<()> {
    let tmp = path.with_extension("jpg.part");
    std::fs::write(&tmp, jpeg).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// A frame's pixels as omacam-merge's RGB picture.
pub fn to_rgb(raw: &Raw<'_>) -> omacam_merge::RgbImage {
    use rayon::prelude::*;
    let mut img = omacam_merge::RgbImage::new(raw.width as usize, raw.height as usize);
    img.data
        .par_chunks_mut(raw.width as usize * 3)
        .enumerate()
        .for_each(|(y, row)| {
            for (x, o) in row.chunks_exact_mut(3).enumerate() {
                let [b, g, r, _] = pixel(raw, x as u32, y as u32);
                o.copy_from_slice(&[r, g, b]);
            }
        });
    img
}

/// The merge's threads: one core left free and a lower priority, so the
/// viewfinder keeps moving while a burst merges.
fn merge_pool() -> Result<rayon::ThreadPool> {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    rayon::ThreadPoolBuilder::new()
        .num_threads(cores.saturating_sub(1).max(1))
        .thread_name(|i| format!("omacam-merge-{i}"))
        .start_handler(|_| {
            // SAFETY: setpriority takes no pointers; on Linux, who = 0
            // with PRIO_PROCESS is the calling thread.
            unsafe {
                libc::setpriority(libc::PRIO_PROCESS, 0, 10);
            }
        })
        .build()
        .context("the merge's threads didn't start")
}

/// Merge a burst (Night, or HDR when `hdr`) into one picture: the
/// reference frame's picture, its pixels replaced by the merge (XRGB8888).
pub fn merge_burst(
    frames: Vec<omacam_core::controller::Picture>,
    hdr: bool,
) -> Result<omacam_core::controller::Picture> {
    merge_pool()?.install(|| merge_burst_in_pool(frames, hdr))
}

/// Where libcamera's simple pipeline looks for a sensor's tuning file, in
/// order.
const TUNING_DIRS: [&str; 2] = [
    "/etc/libcamera/ipa/simple",
    "/usr/share/libcamera/ipa/simple",
];

/// The ISP's finish for a raw burst, from a frame's metadata and the
/// sensor's tuning file (the look and the detail base values).
fn raw_finish(
    p: &omacam_core::controller::Picture,
    raw: &omacam_core::backend::RawFrame,
) -> omacam_merge::IspParams {
    use omacam_merge::isp::{contrast_exp, with_saturation, Detail, IspParams, Tuning};
    let isp = &raw.isp;
    let tuning = isp
        .sensor_model
        .as_deref()
        .and_then(|m| {
            TUNING_DIRS
                .iter()
                .find_map(|d| std::fs::read_to_string(format!("{d}/{m}.yaml")).ok())
        })
        .map(|t| Tuning::parse(&t))
        .unwrap_or_else(|| {
            tracing::warn!(model = ?isp.sensor_model, "no tuning file: the raw merge has no look");
            Tuning::default()
        });
    let identity = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let meta = &p.frame.meta;
    let gains = meta.color_gains.unwrap_or([1.0, 1.0]);
    let gain = meta.analog_gain.unwrap_or(1.0) * meta.digital_gain.unwrap_or(1.0).max(1.0);
    IspParams {
        // SensorBlackLevels count on a 16-bit scale; the ISP takes the
        // 8 high bits.
        black: isp.black_level.unwrap_or(0.0) / 256.0 / 255.0,
        awb: [gains[0], 1.0, gains[1]],
        // Without a matrix the ISP applies no saturation either.
        ccm: isp
            .color_matrix
            .map(|m| with_saturation(m, isp.saturation.unwrap_or(1.0)))
            .unwrap_or(identity),
        gamma: 1.0 / isp.gamma.unwrap_or(2.2),
        contrast_exp: contrast_exp(isp.contrast.unwrap_or(1.0)),
        look: tuning.look,
        detail: Detail::for_gain(tuning.detail, isp.sharpness.unwrap_or(1.0), gain),
    }
}

/// The burst's raw frames as quads, if every frame has one and they match.
fn raw_quads(
    frames: &[omacam_core::controller::Picture],
) -> Option<(Vec<omacam_merge::RawQuads>, (usize, usize))> {
    let first = frames.first()?.frame.raw.as_ref()?;
    let quads = frames
        .iter()
        .map(|p| {
            let r = p.frame.raw.as_ref()?;
            if r.size != first.size || r.first_red != first.first_red {
                return None;
            }
            omacam_merge::RawQuads::from_csi2p10(
                &r.data,
                r.size.width as usize,
                r.size.height as usize,
                r.stride as usize,
            )
            .map_err(|e| tracing::warn!("a raw burst frame: {e}"))
            .ok()
        })
        .collect::<Option<Vec<_>>>()?;
    let fr = (first.first_red.0 as usize, first.first_red.1 as usize);
    Some((quads, fr))
}

fn merge_params(hdr: bool) -> omacam_merge::MergeParams {
    if hdr {
        omacam_merge::MergeParams::hdr(omacam_core::capture::BurstSpec::HDR.under_stops)
    } else {
        omacam_merge::MergeParams::night()
    }
}

fn merge_burst_in_pool(
    mut frames: Vec<omacam_core::controller::Picture>,
    hdr: bool,
) -> Result<omacam_core::controller::Picture> {
    let started = std::time::Instant::now();
    if let Some((quads, first_red)) = raw_quads(&frames) {
        let raw = frames[0].frame.raw.as_ref().expect("checked");
        let finish = omacam_merge::RawFinish {
            isp: raw_finish(&frames[0], raw),
            first_red,
            // HDR's burst stays at its own exposure through the finish, so
            // the look doesn't wash out bright colors, and the fusion brings
            // it back up as on the processed path.
            gain: 1.0,
        };
        let unpacked = started.elapsed();
        let out = omacam_merge::merge_raw_burst(&quads, &finish, &merge_params(hdr))?;
        tracing::info!(
            "merged {} raw frames in {} ms ({} ms to unpack), reference {}, {:?}",
            quads.len(),
            started.elapsed().as_millis(),
            unpacked.as_millis(),
            out.reference,
            out.stats
        );
        // The processed frame is the raw frame's left part.
        let picture = frames.swap_remove(out.reference);
        let image = crop_width(out.image, picture.frame.size.width as usize);
        return Ok(picture_from(picture, &image));
    }
    // One frame at a time (a Picture isn't Sync); to_rgb runs its rows
    // in parallel.
    let images = frames
        .iter()
        .map(|p| {
            let data = p
                .frame
                .data
                .bytes()
                .context("a burst frame wasn't copied to memory")?;
            Ok(to_rgb(&Raw {
                data,
                width: p.frame.size.width,
                height: p.frame.size.height,
                stride: p.frame.stride,
                layout: p.frame.layout,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let converted = started.elapsed();
    let out = omacam_merge::merge_burst(&images, &merge_params(hdr))?;
    tracing::info!(
        "merged {} frames in {} ms ({} ms to RGB), reference {}, {:?}",
        images.len(),
        started.elapsed().as_millis(),
        converted.as_millis(),
        out.reference,
        out.stats
    );
    Ok(picture_from(frames.swap_remove(out.reference), &out.image))
}

/// The left `width` columns of an image.
fn crop_width(image: omacam_merge::RgbImage, width: usize) -> omacam_merge::RgbImage {
    if width >= image.width {
        return image;
    }
    let data = image
        .data
        .chunks_exact(image.width * 3)
        .flat_map(|row| &row[..width * 3])
        .copied()
        .collect();
    omacam_merge::RgbImage {
        width,
        data,
        ..image
    }
}

/// The reference frame's picture, its pixels replaced by the merge
/// (XRGB8888) and its raw picture dropped.
fn picture_from(
    mut picture: omacam_core::controller::Picture,
    image: &omacam_merge::RgbImage,
) -> omacam_core::controller::Picture {
    use rayon::prelude::*;
    // XRGB8888 in memory is B, G, R, X.
    let mut data = vec![0u8; image.data.len() / 3 * 4];
    data.par_chunks_mut(4 * 4096)
        .zip(image.data.par_chunks(3 * 4096))
        .for_each(|(o, p)| {
            for (o, p) in o.chunks_exact_mut(4).zip(p.chunks_exact(3)) {
                o.copy_from_slice(&[p[2], p[1], p[0], 255]);
            }
        });
    picture.frame.stride = image.width as u32 * 4;
    picture.frame.layout = PixelLayout::Xrgb8888;
    picture.frame.data = omacam_core::backend::FrameData::Memory(data);
    picture.frame.raw = None;
    picture
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_width_keeps_the_left_columns() {
        let image = omacam_merge::RgbImage {
            width: 3,
            height: 2,
            data: (0..18).collect(),
        };
        let c = crop_width(image, 2);
        assert_eq!((c.width, c.height), (2, 2));
        assert_eq!(c.data, [0, 1, 2, 3, 4, 5, 9, 10, 11, 12, 13, 14]);
    }

    /// A 4x2 XRGB frame whose pixel (x, y) has blue = 10x + y.
    fn frame() -> Vec<u8> {
        let mut d = Vec::new();
        for y in 0..2u8 {
            for x in 0..4u8 {
                d.extend_from_slice(&[10 * x + y, 0, 0, 0]);
            }
        }
        d
    }

    fn raw(data: &[u8]) -> Raw<'_> {
        Raw {
            data,
            width: 4,
            height: 2,
            stride: 16,
            layout: PixelLayout::Xrgb8888,
        }
    }

    fn blue(img: &Bgrx, x: u32, y: u32) -> u8 {
        img.data[((y * img.width + x) * 4) as usize]
    }

    #[test]
    fn rotation_90_moves_the_bottom_left_to_the_top_left() {
        let d = frame();
        let img = crop_and_rotate(&raw(&d), 1.0, 90).unwrap();
        assert_eq!((img.width, img.height), (2, 4));
        // Clockwise: the old bottom-left (0, 1) is the new top-left.
        assert_eq!(blue(&img, 0, 0), 1);
        // The old top-left (0, 0) is the new top-right.
        assert_eq!(blue(&img, 1, 0), 0);
        // The old top-right (3, 0) is the new bottom-right.
        assert_eq!(blue(&img, 1, 3), 30);
    }

    #[test]
    fn rotation_270_and_180() {
        let d = frame();
        let img = crop_and_rotate(&raw(&d), 1.0, 270).unwrap();
        assert_eq!((img.width, img.height), (2, 4));
        // Counter-clockwise: the old top-right (3, 0) is the new top-left.
        assert_eq!(blue(&img, 0, 0), 30);
        let img = crop_and_rotate(&raw(&d), 1.0, 180).unwrap();
        assert_eq!(blue(&img, 0, 0), 31);
        assert_eq!(blue(&img, 3, 1), 0);
    }

    #[test]
    fn windows_become_even_pixels_inside_the_frame() {
        assert_eq!(
            window_rect(4080, 3072, (0.0, 0.0, 4080.0, 3072.0)),
            (0, 0, 4080, 3072)
        );
        assert_eq!(
            window_rect(4080, 3072, (1019.6, 768.2, 2040.4, 1536.9)),
            (1020, 768, 2040, 1536)
        );
        // Past the edges: held inside.
        assert_eq!(
            window_rect(100, 50, (90.0, -5.0, 40.0, 80.0)),
            (60, 0, 40, 50)
        );
        assert_eq!(
            window_rect(100, 50, (f64::NAN, 0.0, f64::NAN, 10.0)),
            (0, 0, 100, 10)
        );
        let d = vec![0u8; 16 * 4 * 4];
        let r = Raw {
            data: &d,
            width: 16,
            height: 4,
            stride: 64,
            layout: PixelLayout::Xrgb8888,
        };
        let img = crop_window_and_rotate(&r, window_rect(16, 4, (2.0, 0.0, 6.0, 4.0)), 90).unwrap();
        assert_eq!((img.width, img.height), (4, 6));
        assert!(crop_window_and_rotate(&r, (14, 0, 4, 4), 0).is_err());
    }

    #[test]
    fn crop_keeps_the_center() {
        assert_eq!(crop_rect(4080, 3072, 2.0), (1020, 768, 2040, 1536));
        assert_eq!(crop_rect(4080, 3072, 1.0), (0, 0, 4080, 3072));
        assert_eq!(crop_rect(4080, 3072, 0.5), (0, 0, 4080, 3072));
        assert_eq!(crop_rect(4080, 3072, f64::NAN), (0, 0, 4080, 3072));
        let d = frame();
        let img = crop_and_rotate(&raw(&d), 2.0, 0).unwrap();
        assert_eq!((img.width, img.height), (2, 2));
    }

    #[test]
    fn bgr_layouts_swap_to_bgrx() {
        let d = [1u8, 2, 3, 4, 5, 6];
        let r = Raw {
            data: &d,
            width: 2,
            height: 1,
            stride: 6,
            layout: PixelLayout::Bgr888,
        };
        let img = crop_and_rotate(&r, 1.0, 0).unwrap();
        assert_eq!(img.data, [3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn short_data_is_an_error_not_a_panic() {
        let d = [0u8; 10];
        assert!(crop_and_rotate(&raw(&d), 1.0, 90).is_err());
    }

    #[test]
    fn encodes_a_jpeg_and_a_thumbnail() {
        let img = Bgrx {
            width: 320,
            height: 240,
            data: vec![128; 320 * 240 * 4],
        };
        let jpeg = encode_jpeg(&img, 92).unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        let t = thumbnail(&img);
        assert_eq!((t.width, t.height), (112, 112));
    }

    #[test]
    fn aspect_crop_half_and_mirror() {
        let mut d = Vec::new();
        for y in 0..8u8 {
            for x in 0..4u8 {
                d.extend_from_slice(&[x, y, 0, 255]);
            }
        }
        let img = Bgrx {
            width: 4,
            height: 8,
            data: d,
        };
        let sq = crop_to_aspect(img.clone(), Some(1.0));
        assert_eq!((sq.width, sq.height), (4, 4));
        // The center rows: 2..6.
        assert_eq!(sq.data[1], 2);
        assert_eq!(crop_to_aspect(img.clone(), None), img);
        let h = half(&img);
        assert_eq!((h.width, h.height), (2, 4));
        // B is x: the mean of 0, 1, 0, 1 rounds to 1.
        assert_eq!(h.data[0], 1);
        let mut m = img.clone();
        mirror(&mut m);
        assert_eq!(m.data[0], 3, "the left pixel is the old right one");
        mirror(&mut m);
        assert_eq!(m, img);
    }

    /// An XRGB8888 frame (B, G, R, X in memory) through crop, rotation and
    /// the JPEG encoder keeps red red and blue blue.
    #[test]
    fn channel_order_survives_to_the_jpeg() {
        let (w, h) = (32u32, 16u32);
        let mut d = Vec::new();
        for _y in 0..h {
            for x in 0..w {
                // Left half blue, right half red.
                if x < w / 2 {
                    d.extend_from_slice(&[220, 0, 0, 255]);
                } else {
                    d.extend_from_slice(&[0, 0, 220, 255]);
                }
            }
        }
        let raw = Raw {
            data: &d,
            width: w,
            height: h,
            stride: w * 4,
            layout: PixelLayout::Xrgb8888,
        };
        let img = crop_and_rotate(&raw, 1.0, 0).unwrap();
        let jpeg = encode_jpeg(&img, 95).unwrap();
        let rgb = turbojpeg::decompress(&jpeg, turbojpeg::PixelFormat::RGB).unwrap();
        let px = |x: usize, y: usize| {
            let i = y * rgb.pitch + x * 3;
            (rgb.pixels[i], rgb.pixels[i + 1], rgb.pixels[i + 2])
        };
        let (r, _, b) = px(4, 8);
        assert!(b > 180 && r < 40, "left is blue: {:?}", px(4, 8));
        let (r, _, b) = px(28, 8);
        assert!(r > 180 && b < 40, "right is red: {:?}", px(28, 8));
    }

    #[test]
    fn names_count_up_when_taken() {
        let dir = std::env::temp_dir().join(format!("omacam-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = next_path(&dir, "20261014_120000");
        assert!(a.ends_with("IMG_20261014_120000.jpg"));
        write_atomically(&a, b"x").unwrap();
        let b = next_path(&dir, "20261014_120000");
        assert!(b.ends_with("IMG_20261014_120000_1.jpg"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
