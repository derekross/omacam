//! omacam's burst merge: the Night and HDR modes (Omarchy Mobile's
//! docs/CAMERA-QUALITY-PLAN.md, Q2).
//!
//! The approach is HDR+'s (Hasinoff et al., "Burst photography for high
//! dynamic range and low-light imaging on mobile cameras", SIGGRAPH Asia
//! 2016): every frame of the burst has the same short exposure, so the
//! highlights don't clip. The frames are aligned to the sharpest of the
//! first few, merged with tiles that moved left out, and the cleaner result
//! is tone mapped to bring the shadows back. One exposure for every frame
//! avoids most of bracketing's alignment and ghosting trouble.
//!
//! This is the CPU reference: plain Rust, parallel with rayon, tested on
//! synthetic bursts with known motion. A GPU version must match it.
//!
//! Stage 1 merges the software ISP's processed 8-bit frames, which keeps
//! libcamera's whole look; the noise in N frames dithers away the 8-bit
//! steps.

mod align;
mod finish;
pub mod image;
mod merge;
#[doc(hidden)]
pub mod synth;
mod tiles;

use rayon::prelude::*;

pub use image::RgbImage;
pub use merge::MergeStats;

use align::Pyramid;
use image::{luma, Plane};
use tiles::TileGrid;

/// The fewest frames a burst needs.
pub const MIN_FRAMES: usize = 2;
/// The reference is the sharpest of this many first frames (the closest to
/// the shutter press; a later one could have the subject gone).
const REFERENCE_CANDIDATES: usize = 3;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum MergeError {
    #[error("a burst needs at least {MIN_FRAMES} frames, got {0}")]
    TooFewFrames(usize),
    #[error("frame {0} is {1}x{2}, the first is {3}x{4}")]
    SizeMismatch(usize, usize, usize, usize, usize),
    #[error("frames of {0}x{1} are too small to merge")]
    TooSmall(usize, usize),
}

/// How to merge and finish.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeParams {
    /// Exposure fusion of the merge with copies this many stops brighter
    /// (include 0.0 for the merge itself). Empty: no tone mapping, the plain
    /// merge (the frames' own exposure).
    pub fusion_evs: Vec<f32>,
}

impl MergeParams {
    /// Night: a burst of the AGC's exposure capped for hand shake, so the
    /// merge mostly cuts noise; fusion lifts the darkest parts a little.
    pub fn night() -> MergeParams {
        MergeParams {
            fusion_evs: vec![0.0, 1.0],
        }
    }

    /// HDR: a burst `under_stops` under the AGC's exposure, fused back up:
    /// the merge itself for the highlights, a copy brought back to the
    /// AGC's exposure for the midtones and one a stop brighter still for
    /// the shadows.
    pub fn hdr(under_stops: f32) -> MergeParams {
        MergeParams {
            fusion_evs: vec![0.0, under_stops, under_stops + 1.0],
        }
    }
}

/// The merged picture and what happened.
#[derive(Debug, Clone)]
pub struct MergeOutput {
    pub image: RgbImage,
    /// The frame the others were aligned to.
    pub reference: usize,
    pub stats: MergeStats,
}

/// Sharpness of a luma picture: mean squared gradient on a half-size copy
/// (the half size ignores per-pixel noise).
fn sharpness(l: &Plane) -> f32 {
    let p = l.half();
    let mut s = 0.0;
    for y in 0..p.height - 1 {
        for x in 0..p.width - 1 {
            let gx = p.at(x + 1, y) - p.at(x, y);
            let gy = p.at(x, y + 1) - p.at(x, y);
            s += gx * gx + gy * gy;
        }
    }
    s / (p.width * p.height) as f32
}

/// Merge a burst into one picture.
pub fn merge_burst(frames: &[RgbImage], params: &MergeParams) -> Result<MergeOutput, MergeError> {
    if frames.len() < MIN_FRAMES {
        return Err(MergeError::TooFewFrames(frames.len()));
    }
    let (w, h) = (frames[0].width, frames[0].height);
    for (i, f) in frames.iter().enumerate() {
        if (f.width, f.height) != (w, h) {
            return Err(MergeError::SizeMismatch(i, f.width, f.height, w, h));
        }
    }
    if w < tiles::TILE || h < tiles::TILE {
        return Err(MergeError::TooSmall(w, h));
    }

    let mut timings = Vec::new();
    let mut t = std::time::Instant::now();
    let mut lap = |name: &'static str| {
        timings.push((name, t.elapsed()));
        t = std::time::Instant::now();
    };
    let lumas: Vec<Plane> = frames.par_iter().map(luma).collect();
    let reference = (0..frames.len().min(REFERENCE_CANDIDATES))
        .max_by(|&a, &b| sharpness(&lumas[a]).total_cmp(&sharpness(&lumas[b])))
        .unwrap();
    lap("luma");

    let grid = TileGrid::new(w, h);
    let ref_pyramid = Pyramid::new(lumas[reference].clone());
    let offsets: Vec<Vec<(i32, i32)>> = lumas
        .par_iter()
        .enumerate()
        .map(|(i, l)| {
            if i == reference {
                Vec::new()
            } else {
                align::align(&ref_pyramid, &Pyramid::new(l.clone()), &grid)
            }
        })
        .collect();
    lap("align");

    let (merged, mut stats) = merge::merge(frames, &lumas, reference, &grid, &offsets);
    lap("merge");

    let gain = if params.fusion_evs.len() > 1 {
        let mut ml = Plane::new(w, h);
        for (o, p) in ml.data.iter_mut().zip(merged.chunks_exact(3)) {
            *o = 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
        }
        Some(finish::fusion_gain(&ml, &params.fusion_evs))
    } else {
        None
    };
    lap("fusion");

    let mut image = RgbImage::new(w, h);
    image
        .data
        .par_chunks_exact_mut(3)
        .zip(merged.par_chunks_exact(3))
        .enumerate()
        .for_each(|(i, (o, p))| {
            let g = gain.as_ref().map_or(1.0, |g| g.data[i]);
            for c in 0..3 {
                o[c] = (p[c] * g).round().clamp(0.0, 255.0) as u8;
            }
        });
    lap("output");
    stats.timings = timings;
    Ok(MergeOutput {
        image,
        reference,
        stats,
    })
}
