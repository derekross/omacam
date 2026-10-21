//! The raw path (Omarchy Mobile's docs/CAMERA-QUALITY-PLAN.md, Q5): merge
//! the burst's raw Bayer frames, then finish the merge as the software ISP
//! would ([`crate::isp`]).
//!
//! Merging before the finish keeps what the ISP throws away: it reads only
//! 8 bits of each 10-bit sample, and its tone curve crushes the shadows of
//! an underexposed frame into a few 8-bit levels that no later merge gets
//! back. Merged raw, those shadows come back clean.
//!
//! The merge works on 2x2 Bayer quads (R, Gr, Gb, B as four channels at
//! half the size), so a tile only ever moves by whole quads and the color
//! pattern stays in place.

use crate::image::Plane;
use crate::isp::{self, IspParams, Mosaic};

/// Samples of a 10-bit sensor, as the ISP's 8-bit scale counts them.
const SCALE: f32 = 1020.0;
/// The least noise variance assumed, in 10-bit units squared (the 8-bit
/// floor, four times finer).
pub(crate) const MIN_NOISE_VAR: f32 = 16.0;
/// Where the raw gain's highlight knee starts, and the level it rolls off
/// towards, of the range above black. The ISP fades samples above 0.8
/// towards white as clipped (`finish_pixel`), so brightened ones stay
/// under it.
const KNEE: f32 = 0.5;
const KNEE_TOP: f32 = 0.79;
/// A merged sample this close to the sensor's top was clipped.
const CLIPPED: f32 = 1000.0;

/// A raw frame as 2x2 Bayer quads: `width` x `height` quads, four 10-bit
/// samples each, in the order (0, 0), (1, 0), (0, 1), (1, 1) of the block.
#[derive(Clone, Debug, PartialEq)]
pub struct RawQuads {
    pub width: usize,
    pub height: usize,
    pub data: Vec<u16>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RawError {
    #[error("a raw frame of {0}x{1} must have even sides")]
    OddSize(usize, usize),
    #[error("a raw frame of {0}x{1} with stride {2} needs {3} bytes, got {4}")]
    Short(usize, usize, usize, usize, usize),
}

impl RawQuads {
    /// Unpack MIPI CSI-2 packed 10-bit raw (libcamera's `*10_CSI2P`): four
    /// samples in five bytes, the high 8 bits of each first, then a byte of
    /// their low 2 bits.
    pub fn from_csi2p10(
        bytes: &[u8],
        width: usize,
        height: usize,
        stride: usize,
    ) -> Result<RawQuads, RawError> {
        use rayon::prelude::*;
        if !width.is_multiple_of(4) || !height.is_multiple_of(2) {
            return Err(RawError::OddSize(width, height));
        }
        let need = stride * (height - 1) + width / 4 * 5;
        if bytes.len() < need || stride < width / 4 * 5 {
            return Err(RawError::Short(width, height, stride, need, bytes.len()));
        }
        let (qw, qh) = (width / 2, height / 2);
        let mut data = vec![0u16; qw * qh * 4];
        data.par_chunks_mut(qw * 4)
            .enumerate()
            .for_each(|(qy, out)| {
                for half in 0..2 {
                    let row = &bytes[(2 * qy + half) * stride..];
                    for g in 0..width / 4 {
                        let b = &row[g * 5..g * 5 + 5];
                        for i in 0..4 {
                            let v = (b[i] as u16) << 2 | (b[4] as u16 >> (2 * i)) & 3;
                            let x = g * 4 + i;
                            out[(x / 2) * 4 + half * 2 + x % 2] = v;
                        }
                    }
                }
            });
        Ok(RawQuads {
            width: qw,
            height: qh,
            data,
        })
    }

    /// Brightness for alignment and the noise bins: the quad's mean, on a
    /// 0..255 scale.
    pub fn luma(&self) -> Plane {
        Plane::from_rows(self.width, self.height, |y, row| {
            let src = &self.data[y * self.width * 4..][..self.width * 4];
            for (o, q) in row.iter_mut().zip(src.chunks_exact(4)) {
                *o = (q[0] + q[1] + q[2] + q[3]) as f32 * (255.0 / (4.0 * SCALE));
            }
        })
    }
}

/// The merged quads back to a Bayer picture on the ISP's scale, brightened
/// by `gain` above black with a knee that keeps the highlights: each quad's
/// brightest channel after white balance (the first to clip) rolls off
/// smoothly below the ISP's clipped range, and the quad's four samples
/// scale together, so its color stays. Quads the sensor clipped go to the
/// top, where the ISP turns them white as it does in a single frame.
pub fn to_mosaic(
    merged: &[f32],
    qw: usize,
    qh: usize,
    p: &IspParams,
    first_red: (usize, usize),
    gain: f32,
) -> Mosaic {
    use rayon::prelude::*;
    let (w, h) = (qw * 2, qh * 2);
    let black = p.black * SCALE;
    let range = SCALE - black;
    // The white balance of each quad position: red, blue or green.
    let wb = |i: usize| {
        let (x, y) = (i % 2, i / 2);
        match ((x + first_red.0) % 2, (y + first_red.1) % 2) {
            (0, 0) => p.awb[0],
            (1, 1) => p.awb[2],
            _ => p.awb[1],
        }
    };
    let wbs = [wb(0), wb(1), wb(2), wb(3)];
    let wmax = wbs.iter().cloned().fold(1.0f32, f32::max);
    let knee = |x: f32| {
        if x <= KNEE {
            x
        } else {
            let room = KNEE_TOP - KNEE;
            KNEE + room * (1.0 - (-(x - KNEE) / room).exp())
        }
    };
    let mut data = vec![0.0f32; w * h];
    data.par_chunks_mut(2 * w)
        .enumerate()
        .for_each(|(qy, rows)| {
            let (top, bottom) = rows.split_at_mut(w);
            for qx in 0..qw {
                let q = &merged[(qy * qw + qx) * 4..][..4];
                let mut s = [0.0f32; 4];
                let mut peak = 0.0f32;
                let mut clipped = false;
                for i in 0..4 {
                    s[i] = (q[i] - black).max(0.0) / range;
                    peak = peak.max(s[i] * wbs[i] / wmax);
                    clipped |= q[i] >= CLIPPED;
                }
                let k = if gain == 1.0 || peak <= 0.0 {
                    1.0
                } else if clipped {
                    gain.min(1.0 / peak)
                } else {
                    knee(peak * gain) / peak
                };
                let v = |i: usize| (black + s[i] * k * range) / SCALE;
                top[2 * qx] = v(0);
                top[2 * qx + 1] = v(1);
                bottom[2 * qx] = v(2);
                bottom[2 * qx + 1] = v(3);
            }
        });
    Mosaic {
        width: w,
        height: h,
        data,
        first_red,
    }
}

/// Finish merged quads into RGB on the merge's 0..255 scale.
pub(crate) fn finish(
    merged: &[f32],
    qw: usize,
    qh: usize,
    p: &IspParams,
    first_red: (usize, usize),
    gain: f32,
) -> Vec<f32> {
    isp::finish(&to_mosaic(merged, qw, qh, p, first_red, gain), p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi2p10_unpacks_into_quads() {
        // 8x2 pixels: sample (x, y) = 100 * y + 10 * x + 3 (low bits 3).
        let (w, h, stride) = (8, 2, 12);
        let mut bytes = vec![0u8; stride * h];
        for y in 0..h {
            for g in 0..w / 4 {
                let mut low = 0u8;
                for i in 0..4 {
                    let v = (100 * y + 10 * (g * 4 + i) + 3) as u16;
                    bytes[y * stride + g * 5 + i] = (v >> 2) as u8;
                    low |= ((v & 3) as u8) << (2 * i);
                }
                bytes[y * stride + g * 5 + 4] = low;
            }
        }
        let q = RawQuads::from_csi2p10(&bytes, w, h, stride).unwrap();
        assert_eq!((q.width, q.height), (4, 1));
        // Quad 1 covers x 2..4: (2,0) (3,0) (2,1) (3,1).
        assert_eq!(&q.data[4..8], &[23, 33, 123, 133]);
        assert_eq!(
            RawQuads::from_csi2p10(&bytes[..10], w, h, stride),
            Err(RawError::Short(8, 2, 12, 22, 10))
        );
    }

    #[test]
    fn the_knee_keeps_color_and_the_range() {
        let p = IspParams {
            black: 64.0 / SCALE,
            awb: [2.0, 1.0, 1.5],
            ccm: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            gamma: 1.0 / 2.2,
            contrast_exp: 1.0,
            look: Default::default(),
            detail: Default::default(),
        };
        // One quad (first red at (0, 0)): a bright red-orange.
        let q = [700.0, 300.0, 300.0, 150.0];
        let m = to_mosaic(&q, 1, 1, &p, (0, 0), 8.0);
        let s: Vec<f32> = m.data.iter().map(|v| v * SCALE - 64.0).collect();
        // Under the ISP's clipped range, and the ratios between the
        // samples kept.
        let top = 0.8 * (SCALE - 64.0);
        assert!(s.iter().all(|&v| v < top), "{s:?}");
        let ratio = |a: f32, b: f32| a / b;
        assert!((ratio(s[0], s[1]) - ratio(636.0, 236.0)).abs() < 1e-3);
        assert!((ratio(s[3], s[1]) - ratio(86.0, 236.0)).abs() < 1e-3);
        // A dark quad is brightened by the full gain.
        let d = to_mosaic(&[74.0, 74.0, 74.0, 74.0], 1, 1, &p, (0, 0), 8.0);
        assert!((d.data[0] * SCALE - 64.0 - 80.0).abs() < 1e-2);
        // A quad the sensor clipped goes to the top.
        let c = to_mosaic(&[1023.0, 900.0, 900.0, 600.0], 1, 1, &p, (0, 0), 8.0);
        assert!((c.data[0] - 1.0).abs() < 1e-6, "{:?}", c.data);
    }
}
