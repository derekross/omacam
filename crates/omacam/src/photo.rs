//! Turning a captured frame into a JPEG file: crop to the zoom, turn
//! upright (so the file needs no EXIF orientation), encode with
//! libjpeg-turbo, and make a thumbnail. Runs off the UI thread.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use omacam_core::backend::PixelLayout;

/// JPEG quality: stock-like files without the size of 95+.
pub const QUALITY: i32 = 92;
/// Longest side of the thumbnail.
pub const THUMB: u32 = 160;

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

/// The centred crop rectangle for a zoom crop factor: (x, y, w, h), even.
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

/// Crop by `crop` about the centre and turn clockwise by `rotation`.
pub fn crop_and_rotate(raw: &Raw<'_>, crop: f64, rotation: u32) -> Result<Bgrx> {
    let bpp = raw.layout.bytes_per_pixel();
    if raw.stride < raw.width * bpp
        || (raw.data.len() as u64)
            < raw.stride as u64 * (raw.height as u64 - 1) + (raw.width * bpp) as u64
    {
        bail!("frame data is shorter than its size says");
    }
    let (cx, cy, cw, ch) = crop_rect(raw.width, raw.height, crop);
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

pub fn encode_jpeg(img: &Bgrx) -> Result<Vec<u8>> {
    let image = turbojpeg::Image {
        pixels: img.data.as_slice(),
        width: img.width as usize,
        pitch: img.width as usize * 4,
        height: img.height as usize,
        format: turbojpeg::PixelFormat::BGRX,
    };
    let jpeg = turbojpeg::compress(image, QUALITY, turbojpeg::Subsamp::Sub2x2)
        .context("JPEG encoding failed")?;
    Ok(jpeg.to_vec())
}

/// A small copy for the gallery button (nearest neighbour; it's tiny).
pub fn thumbnail(img: &Bgrx) -> Bgrx {
    let longest = img.width.max(img.height).max(1);
    let s = (THUMB as f64 / longest as f64).min(1.0);
    let (w, h) = (
        ((img.width as f64 * s) as u32).max(1),
        ((img.height as f64 * s) as u32).max(1),
    );
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let sx = (x as u64 * img.width as u64 / w as u64) as usize;
            let sy = (y as u64 * img.height as u64 / h as u64) as usize;
            let si = (sy * img.width as usize + sx) * 4;
            let di = ((y * w + x) * 4) as usize;
            data[di..di + 4].copy_from_slice(&img.data[si..si + 4]);
        }
    }
    Bgrx {
        width: w,
        height: h,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn crop_keeps_the_centre() {
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
        let jpeg = encode_jpeg(&img).unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        let t = thumbnail(&img);
        assert_eq!((t.width, t.height), (160, 120));
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
