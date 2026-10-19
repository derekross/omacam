//! The merge tiles: squares of [`TILE`] pixels every [`TILE`]/2, so each
//! pixel is covered by up to four tiles, blended with a raised-cosine window
//! that sums to one where tiles overlap (no seams).

/// Side of a merge tile, in full-resolution pixels.
pub const TILE: usize = 32;

pub struct TileGrid {
    pub size: usize,
    pub nx: usize,
    pub ny: usize,
    width: usize,
    height: usize,
}

impl TileGrid {
    pub fn new(width: usize, height: usize) -> TileGrid {
        let size = TILE.min(width).min(height);
        let step = size / 2;
        let count = |len: usize| (len - size).div_ceil(step) + 1;
        TileGrid {
            size,
            nx: count(width),
            ny: count(height),
            width,
            height,
        }
    }

    pub fn len(&self) -> usize {
        self.nx * self.ny
    }

    /// Top-left corner of tile `i` (row by row). The last tile of a row or
    /// column is moved in so that it ends at the picture's edge.
    pub fn origin(&self, i: usize) -> (usize, usize) {
        let step = self.size / 2;
        let x = ((i % self.nx) * step).min(self.width - self.size);
        let y = ((i / self.nx) * step).min(self.height - self.size);
        (x, y)
    }
}

/// The raised-cosine window over one tile side: w(i) = sin^2(pi (i + 0.5) / n).
/// Two of them offset by n/2 sum to one.
pub fn window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let s = (std::f32::consts::PI * (i as f32 + 0.5) / n as f32).sin();
            s * s
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_cover_the_picture_and_end_at_its_edges() {
        let g = TileGrid::new(100, 70);
        let mut covered = vec![false; 100 * 70];
        for i in 0..g.len() {
            let (x0, y0) = g.origin(i);
            assert!(x0 + g.size <= 100 && y0 + g.size <= 70);
            for y in y0..y0 + g.size {
                for x in x0..x0 + g.size {
                    covered[y * 100 + x] = true;
                }
            }
        }
        assert!(covered.iter().all(|&c| c));
    }

    #[test]
    fn half_overlapping_windows_sum_to_one() {
        let w = window(32);
        for i in 0..16 {
            assert!((w[i + 16] + w[i] - 1.0).abs() < 1e-5);
        }
    }
}
