//! Aligning each burst frame to the reference: tile block matching, coarse
//! to fine over a luma pyramid (Hasinoff et al. 2016, section 4).
//!
//! The coarse levels find large motion cheaply on small pictures; each finer
//! level starts from the coarser level's answer and searches a little around
//! it. The last step refines the offsets of the merge tiles themselves on the
//! full-resolution luma, so the merge gets one whole-pixel offset per tile.

use rayon::prelude::*;

use crate::image::Plane;
use crate::tiles::TileGrid;

/// Side of the alignment tiles on the pyramid levels above full resolution.
const LEVEL_TILE: usize = 16;
/// Search radius on the coarsest level, in that level's pixels.
const COARSE_RADIUS: i32 = 8;
/// Search radius on the other levels.
const LEVEL_RADIUS: i32 = 2;
/// Search radius of the final, full-resolution refinement.
const FINE_RADIUS: i32 = 1;
/// The pyramid stops before a level would be shorter than this.
const COARSEST_MIN: usize = 32;

/// A luma pyramid: level 0 is full resolution, each next level half the size.
pub struct Pyramid {
    pub levels: Vec<Plane>,
}

impl Pyramid {
    /// The pyramid of a luma picture. Level 0 is blurred over 3x3 first:
    /// matching is about structure, and per-pixel noise otherwise pulls the
    /// full-resolution refinement a pixel off on smooth tiles.
    pub fn new(luma: Plane) -> Pyramid {
        let mut levels = vec![blur3(&luma)];
        while levels.len() < 6 {
            let last = levels.last().unwrap();
            if last.width.min(last.height) / 2 < COARSEST_MIN {
                break;
            }
            let next = last.half();
            levels.push(next);
        }
        Pyramid { levels }
    }
}

/// A 3x3 box blur, edges clamped: three rows summed, then three columns.
fn blur3(p: &Plane) -> Plane {
    let w = p.width;
    Plane::from_rows(w, p.height, |y, row| {
        let line = |j: isize| {
            let yy = (y as isize + j).clamp(0, p.height as isize - 1) as usize;
            &p.data[yy * w..][..w]
        };
        let (a, b, c) = (line(-1), line(0), line(1));
        let mut col: Vec<f32> = (0..w).map(|x| a[x] + b[x] + c[x]).collect();
        col.push(col[w - 1]);
        let mut prev = col[0];
        for x in 0..w {
            let s = prev + col[x] + col[x + 1];
            prev = col[x];
            row[x] = s / 9.0;
        }
    })
}

/// Sum of squared differences between a tile of `reference` at (x0, y0) and
/// the same tile of `frame` moved by (dx, dy). `step` 2 samples every other
/// row and column, for a quarter of the work on large tiles.
fn tile_cost(
    reference: &Plane,
    frame: &Plane,
    x0: usize,
    y0: usize,
    size: usize,
    (dx, dy): (i32, i32),
    step: usize,
) -> f32 {
    let mut cost = 0.0;
    let x_end = (x0 + size).min(reference.width);
    let y_end = (y0 + size).min(reference.height);
    let (fx0, fy0) = (x0 as isize + dx as isize, y0 as isize + dy as isize);
    let inside = fx0 >= 0
        && fy0 >= 0
        && x_end as isize + dx as isize <= frame.width as isize
        && y_end as isize + dy as isize <= frame.height as isize;
    if inside && step == 1 {
        // The usual case: the moved tile lies inside the frame, so whole
        // rows compare as slices (no clamping, and the compiler
        // vectorizes it).
        let (fx0, fy0) = (fx0 as usize, fy0 as usize);
        let n = x_end - x0;
        for (j, y) in (y0..y_end).enumerate() {
            let r = &reference.data[y * reference.width + x0..][..n];
            let f = &frame.data[(fy0 + j) * frame.width + fx0..][..n];
            cost += r.iter().zip(f).map(|(a, b)| (a - b) * (a - b)).sum::<f32>();
        }
        return cost;
    }
    for y in (y0..y_end).step_by(step) {
        let row = y * reference.width;
        let fy = y as isize + dy as isize;
        for x in (x0..x_end).step_by(step) {
            let d = reference.data[row + x] - frame.at_clamped(x as isize + dx as isize, fy);
            cost += d * d;
        }
    }
    cost
}

/// The offset with the lowest cost among `guesses` and the square of
/// `radius` around the best of them.
#[allow(clippy::too_many_arguments)]
fn search(
    reference: &Plane,
    frame: &Plane,
    x0: usize,
    y0: usize,
    size: usize,
    guesses: &[(i32, i32)],
    radius: i32,
    step: usize,
) -> (i32, i32) {
    let cost = |o| tile_cost(reference, frame, x0, y0, size, o, step);
    let mut best = guesses[0];
    let mut best_cost = cost(best);
    for &g in &guesses[1..] {
        let c = cost(g);
        if c < best_cost {
            (best, best_cost) = (g, c);
        }
    }
    let start = best;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            if dx == 0 && dy == 0 {
                continue;
            }
            let o = (start.0 + dx, start.1 + dy);
            let c = cost(o);
            // Prefer the smaller motion on a tie (flat tiles match anywhere).
            if c < best_cost
                || (c == best_cost && o.0.abs() + o.1.abs() < best.0.abs() + best.1.abs())
            {
                (best, best_cost) = (o, c);
            }
        }
    }
    best
}

/// Offsets on one pyramid level's own tile grid (tiles of [`LEVEL_TILE`],
/// not overlapping), row by row.
struct LevelField {
    nx: usize,
    ny: usize,
    offsets: Vec<(i32, i32)>,
}

impl LevelField {
    /// The guesses for a point (in this level's pixels): the offset of the
    /// tile it falls in and of that tile's four neighbors.
    fn guesses_at(&self, x: usize, y: usize, scale: i32) -> Vec<(i32, i32)> {
        let tx = (x / LEVEL_TILE).min(self.nx - 1) as isize;
        let ty = (y / LEVEL_TILE).min(self.ny - 1) as isize;
        let mut out = Vec::with_capacity(5);
        for (ox, oy) in [(0, 0), (-1, 0), (1, 0), (0, -1), (0, 1)] {
            let (nx, ny) = (tx + ox, ty + oy);
            if nx < 0 || ny < 0 || nx >= self.nx as isize || ny >= self.ny as isize {
                continue;
            }
            let o = self.offsets[ny as usize * self.nx + nx as usize];
            let g = (o.0 * scale, o.1 * scale);
            if !out.contains(&g) {
                out.push(g);
            }
        }
        out
    }
}

fn align_level(
    reference: &Plane,
    frame: &Plane,
    coarser: Option<&LevelField>,
    radius: i32,
) -> LevelField {
    let nx = reference.width.div_ceil(LEVEL_TILE).max(1);
    let ny = reference.height.div_ceil(LEVEL_TILE).max(1);
    let offsets = (0..nx * ny)
        .into_par_iter()
        .map(|i| {
            let (x0, y0) = ((i % nx) * LEVEL_TILE, (i / nx) * LEVEL_TILE);
            let guesses = match coarser {
                Some(c) => c.guesses_at((x0 + LEVEL_TILE / 2) / 2, (y0 + LEVEL_TILE / 2) / 2, 2),
                None => vec![(0, 0)],
            };
            search(reference, frame, x0, y0, LEVEL_TILE, &guesses, radius, 1)
        })
        .collect();
    LevelField { nx, ny, offsets }
}

/// One whole-pixel offset per merge tile of `grid`: where that tile of the
/// reference is found in `frame`.
pub fn align(reference: &Pyramid, frame: &Pyramid, grid: &TileGrid) -> Vec<(i32, i32)> {
    let top = reference.levels.len() - 1;
    let mut field: Option<LevelField> = None;
    for level in (1..=top).rev() {
        let radius = if level == top {
            COARSE_RADIUS
        } else {
            LEVEL_RADIUS
        };
        field = Some(align_level(
            &reference.levels[level],
            &frame.levels[level],
            field.as_ref(),
            radius,
        ));
    }
    let (r0, f0) = (&reference.levels[0], &frame.levels[0]);
    (0..grid.len())
        .into_par_iter()
        .map(|i| {
            let (x0, y0) = grid.origin(i);
            let guesses = match &field {
                Some(c) => c.guesses_at((x0 + grid.size / 2) / 2, (y0 + grid.size / 2) / 2, 2),
                // A picture too small for a pyramid: search near zero.
                None => vec![(0, 0)],
            };
            let radius = if field.is_some() {
                FINE_RADIUS
            } else {
                COARSE_RADIUS
            };
            search(r0, f0, x0, y0, grid.size, &guesses, radius, 1)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::luma;
    use crate::synth::{Burst, Rng};

    #[test]
    fn noisy_whole_frame_shifts_are_found_to_the_pixel() {
        let shifts = vec![(0, 0), (7, -3), (-11, 5), (18, 9), (-19, 12), (3, 20)];
        let n = shifts.len();
        let b = Burst {
            width: 256,
            height: 192,
            shifts: shifts.clone(),
            square: vec![None; n],
            square_size: 0,
        };
        let mut rng = Rng::new(5);
        let lumas: Vec<Plane> = (0..n).map(|i| luma(&b.noisy(i, 8.0, &mut rng))).collect();
        let grid = TileGrid::new(256, 192);
        let reference = Pyramid::new(lumas[0].clone());
        for f in 1..n {
            let want = (shifts[0].0 - shifts[f].0, shifts[0].1 - shifts[f].1);
            let got = align(&reference, &Pyramid::new(lumas[f].clone()), &grid);
            // Tiles whose matching content lies inside the frame. A tile
            // with little texture may land a pixel off, which costs the
            // merge nothing; more than a pixel would blur it.
            let (mut right, mut total) = (0, 0);
            for (i, o) in got.iter().enumerate() {
                let (x0, y0) = grid.origin(i);
                let (sx, sy) = (x0 as i32 + want.0, y0 as i32 + want.1);
                if sx < 0 || sy < 0 || sx + 32 > 256 || sy + 32 > 192 {
                    continue;
                }
                total += 1;
                right += (*o == want) as usize;
                assert!(
                    (o.0 - want.0).abs() <= 1 && (o.1 - want.1).abs() <= 1,
                    "frame {f}: tile {i} at {o:?}, want {want:?}"
                );
            }
            eprintln!("frame {f}: {right}/{total} interior tiles exact (want {want:?})");
            assert!(right * 100 >= total * 85, "frame {f}: {right}/{total}");
        }
    }
}
