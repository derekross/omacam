//! Merging the aligned frames: a weighted average per tile, where a tile
//! that differs from the reference by more than the noise explains (it
//! moved, or alignment failed there) gets little or no weight. Moving
//! things then come from the reference alone: no ghosts, less noise
//! reduction just there.

use rayon::prelude::*;

use crate::image::Plane;
use crate::tiles::{window, TileGrid};

/// A tile weighs fully while its mean squared difference from the reference
/// is within this many times what two noisy copies of a still scene give.
const ROBUST_FREE: f32 = 1.5;
/// How fast the weight falls past that: w = exp(-SLOPE * (r - FREE)).
const ROBUST_SLOPE: f32 = 1.0;
/// Brightness bins for the noise estimate (noise depends on brightness).
const NOISE_BINS: usize = 8;
/// The least noise variance assumed for 8-bit samples (units squared), so
/// that a very clean burst still tolerates quantization and resampling
/// error.
pub const MIN_NOISE_VAR_8BIT: f32 = 1.0;

/// The pictures the merge takes: `C` samples a pixel, rows packed with no
/// padding. 8-bit RGB (C = 3) and raw Bayer quads (C = 4, 10-bit) both
/// fit.
#[derive(Clone, Copy)]
pub struct Frames<'a, T, const C: usize> {
    pub width: usize,
    pub height: usize,
    pub data: &'a [&'a [T]],
}

/// Mean squared difference of a tile between the reference and an aligned
/// frame, over every channel (luma alone misses a colored object on a
/// background of the same brightness), and the reference tile's mean luma.
#[allow(clippy::too_many_arguments)]
fn tile_stats<T: Copy + Into<f32>, const C: usize>(
    reference: &[T],
    ref_luma: &Plane,
    frame: &[T],
    (w, h): (usize, usize),
    x0: usize,
    y0: usize,
    size: usize,
    (dx, dy): (i32, i32),
) -> (f32, f32) {
    let (mut sq, mut mean, mut n) = (0.0, 0.0, 0.0);
    for y in (y0..y0 + size).step_by(2) {
        let fy = (y as isize + dy as isize).clamp(0, h as isize - 1) as usize;
        for x in (x0..x0 + size).step_by(2) {
            let fx = (x as isize + dx as isize).clamp(0, w as isize - 1) as usize;
            let (ri, fi) = ((y * w + x) * C, (fy * w + fx) * C);
            for c in 0..C {
                let d = reference[ri + c].into() - frame[fi + c].into();
                sq += d * d;
            }
            mean += ref_luma.at(x, y);
            n += 1.0;
        }
    }
    (sq / (C as f32 * n), mean / n)
}

/// The noise variance of one frame, per brightness bin, from the burst
/// itself: half the median squared difference between two aligned frames
/// over the tiles of that brightness (the median ignores tiles that moved).
pub fn noise_profile(diffs: &[(f32, f32)], min_var: f32) -> [f32; NOISE_BINS] {
    let median = |mut v: Vec<f32>| -> Option<f32> {
        if v.is_empty() {
            return None;
        }
        v.sort_by(f32::total_cmp);
        Some(v[v.len() / 2])
    };
    let all = median(diffs.iter().map(|d| d.0).collect()).unwrap_or(0.0);
    let mut out = [0.0; NOISE_BINS];
    for (b, slot) in out.iter_mut().enumerate() {
        let lo = 255.0 * b as f32 / NOISE_BINS as f32;
        let hi = 255.0 * (b + 1) as f32 / NOISE_BINS as f32;
        let in_bin: Vec<f32> = diffs
            .iter()
            .filter(|d| d.1 >= lo && d.1 < hi)
            .map(|d| d.0)
            .collect();
        // Too few tiles of this brightness to trust: the whole picture's.
        let m = if in_bin.len() >= 16 {
            median(in_bin).unwrap()
        } else {
            all
        };
        *slot = (0.5 * m).max(min_var);
    }
    out
}

fn bin_of(mean: f32) -> usize {
    ((mean / 255.0 * NOISE_BINS as f32) as usize).min(NOISE_BINS - 1)
}

/// The weight of one aligned tile of one frame, given its difference from
/// the reference and the noise variance at its brightness.
pub fn robust_weight(msd: f32, noise_var: f32) -> f32 {
    // Two independent noisy copies of the same scene differ by 2 sigma^2.
    let r = msd / (2.0 * noise_var);
    if r <= ROBUST_FREE {
        1.0
    } else {
        (-ROBUST_SLOPE * (r - ROBUST_FREE)).exp()
    }
}

/// What the merge did, for the log and the tests.
#[derive(Debug, Clone, Default)]
pub struct MergeStats {
    /// The mean tile weight of each non-reference frame.
    pub frame_weights: Vec<f32>,
    /// Noise standard deviation (in the samples' units, per channel) of the
    /// middle brightness bin.
    pub noise_sigma: f32,
    /// How long each stage took, in order (filled in by `merge_burst`).
    pub timings: Vec<(&'static str, std::time::Duration)>,
}

/// Merge `frames` onto `frames[reference]`, each moved per tile by its
/// `offsets` (one list per frame, empty for the reference). `lumas` are
/// the frames' brightness on a 0..255 scale (for the noise bins);
/// `min_noise_var` is the least noise variance assumed, in the samples'
/// units squared. The result has `C` floats a pixel.
pub fn merge<T: Copy + Into<f32> + Sync, const C: usize>(
    frames: Frames<'_, T, C>,
    lumas: &[Plane],
    reference: usize,
    grid: &TileGrid,
    offsets: &[Vec<(i32, i32)>],
    min_noise_var: f32,
) -> (Vec<f32>, MergeStats) {
    let (w, h) = (frames.width, frames.height);
    let ref_luma = &lumas[reference];
    let frames = frames.data;

    // Per frame: each tile's difference from the reference and brightness,
    // then the frame's noise profile and the tile weights.
    let mut weights: Vec<Vec<f32>> = vec![Vec::new(); frames.len()];
    let mut stats = MergeStats::default();
    for f in 0..frames.len() {
        if f == reference {
            continue;
        }
        let diffs: Vec<(f32, f32)> = (0..grid.len())
            .into_par_iter()
            .map(|i| {
                let (x0, y0) = grid.origin(i);
                tile_stats::<T, C>(
                    frames[reference],
                    ref_luma,
                    frames[f],
                    (w, h),
                    x0,
                    y0,
                    grid.size,
                    offsets[f][i],
                )
            })
            .collect();
        let noise = noise_profile(&diffs, min_noise_var);
        if stats.noise_sigma == 0.0 {
            stats.noise_sigma = noise[NOISE_BINS / 2].sqrt();
        }
        weights[f] = diffs
            .iter()
            .map(|&(msd, mean)| robust_weight(msd, noise[bin_of(mean)]))
            .collect();
        stats
            .frame_weights
            .push(weights[f].iter().sum::<f32>() / weights[f].len() as f32);
    }

    // Accumulate window * weight * pixel, row band by row band of tiles so
    // that threads never write the same pixels: tile rows overlap by half,
    // so even tile rows first, then odd ones.
    let win = window(grid.size);
    let mut acc = vec![0.0f32; w * h * C];
    let mut norm = vec![0.0f32; w * h];
    for parity in 0..2 {
        // Each band of a tile row covers pixel rows [y0, y0 + size); rows of
        // the same parity don't overlap.
        let rows: Vec<usize> = (parity..grid.ny).step_by(2).collect();
        let bands: Vec<(usize, Vec<f32>, Vec<f32>)> = rows
            .par_iter()
            .map(|&ty| {
                let (_, y0) = grid.origin(ty * grid.nx);
                let mut a = vec![0.0f32; w * grid.size * C];
                let mut n = vec![0.0f32; w * grid.size];
                for tx in 0..grid.nx {
                    let i = ty * grid.nx + tx;
                    let (x0, _) = grid.origin(i);
                    for (f, frame) in frames.iter().enumerate() {
                        let (wt, (dx, dy)) = if f == reference {
                            (1.0, (0, 0))
                        } else {
                            (weights[f][i], offsets[f][i])
                        };
                        if wt < 1e-3 {
                            continue;
                        }
                        let sx0 = x0 as isize + dx as isize;
                        let inside = sx0 >= 0 && sx0 + grid.size as isize <= w as isize;
                        for j in 0..grid.size {
                            let sy = (y0 + j) as isize + dy as isize;
                            let sy = sy.clamp(0, h as isize - 1) as usize;
                            let wj = wt * win[j];
                            let dst_row = j * w + x0;
                            if inside {
                                // The usual case: the moved row lies inside
                                // the frame, so no clamping per pixel.
                                let src = &frame[(sy * w + sx0 as usize) * C..][..grid.size * C];
                                let dst = &mut a[dst_row * C..][..grid.size * C];
                                let nd = &mut n[dst_row..][..grid.size];
                                for (k, ((d, s), nn)) in dst
                                    .chunks_exact_mut(C)
                                    .zip(src.chunks_exact(C))
                                    .zip(nd)
                                    .enumerate()
                                {
                                    let ww = wj * win[k];
                                    for c in 0..C {
                                        d[c] += ww * s[c].into();
                                    }
                                    *nn += ww;
                                }
                                continue;
                            }
                            for (k, &wk) in win.iter().enumerate() {
                                let ww = wj * wk;
                                let sx = (sx0 + k as isize).clamp(0, w as isize - 1) as usize;
                                let src = (sy * w + sx) * C;
                                let dst = dst_row + k;
                                for c in 0..C {
                                    a[dst * C + c] += ww * frame[src + c].into();
                                }
                                n[dst] += ww;
                            }
                        }
                    }
                }
                (y0, a, n)
            })
            .collect();
        // Bands of one parity cover different rows: add them in parallel,
        // a row each.
        let mut rows_of: Vec<Option<(&[f32], &[f32])>> = vec![None; h];
        for (y0, a, n) in &bands {
            for j in 0..grid.size {
                rows_of[y0 + j] = Some((&a[j * w * C..][..w * C], &n[j * w..][..w]));
            }
        }
        acc.par_chunks_mut(w * C)
            .zip(norm.par_chunks_mut(w))
            .zip(rows_of.par_iter())
            .for_each(|((acc_row, norm_row), band)| {
                if let Some((a, n)) = band {
                    acc_row.iter_mut().zip(*a).for_each(|(x, y)| *x += y);
                    norm_row.iter_mut().zip(*n).for_each(|(x, y)| *x += y);
                }
            });
    }

    acc.par_chunks_exact_mut(C).zip(&norm).for_each(|(p, &n)| {
        let inv = if n > 0.0 { 1.0 / n } else { 0.0 };
        for c in p {
            *c *= inv;
        }
    });
    (acc, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn still_tiles_weigh_fully_and_moved_ones_drop_out() {
        let var = 25.0;
        assert_eq!(robust_weight(2.0 * var, var), 1.0);
        assert_eq!(robust_weight(1.4 * 2.0 * var, var), 1.0);
        assert!(robust_weight(10.0 * 2.0 * var, var) < 1e-3);
    }

    #[test]
    fn the_noise_estimate_ignores_a_minority_of_moved_tiles() {
        // 80 still tiles with squared difference 50 (sigma^2 = 25) and 20
        // moved ones far off, all mid-gray.
        let mut d = vec![(50.0, 128.0); 80];
        d.extend(vec![(5000.0, 128.0); 20]);
        let p = noise_profile(&d, MIN_NOISE_VAR_8BIT);
        assert_eq!(p[bin_of(128.0)], 25.0);
    }
}
