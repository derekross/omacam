//! The pictures the merge works on: 8-bit RGB in and out, float planes
//! inside.

use rayon::prelude::*;

/// An 8-bit RGB picture, rows packed with no padding, as the software ISP's
/// output becomes after its XRGB8888 is unpacked.
#[derive(Clone, Debug, PartialEq)]
pub struct RgbImage {
    pub width: usize,
    pub height: usize,
    /// `width * height * 3` bytes, R G B per pixel.
    pub data: Vec<u8>,
}

impl RgbImage {
    pub fn new(width: usize, height: usize) -> RgbImage {
        RgbImage {
            width,
            height,
            data: vec![0; width * height * 3],
        }
    }

    pub fn from_raw(width: usize, height: usize, data: Vec<u8>) -> Option<RgbImage> {
        (data.len() == width * height * 3).then_some(RgbImage {
            width,
            height,
            data,
        })
    }

    pub fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.width + x) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }
}

/// One float plane.
#[derive(Clone, Debug)]
pub struct Plane {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl Plane {
    pub fn new(width: usize, height: usize) -> Plane {
        Plane {
            width,
            height,
            data: vec![0.0; width * height],
        }
    }

    /// A plane filled row by row in parallel: `f(y, row)` writes row `y`.
    pub fn from_rows(width: usize, height: usize, f: impl Fn(usize, &mut [f32]) + Sync) -> Plane {
        let mut p = Plane::new(width, height);
        if width > 0 {
            p.data
                .par_chunks_mut(width)
                .enumerate()
                .for_each(|(y, row)| f(y, row));
        }
        p
    }

    #[inline]
    pub fn at(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.width + x]
    }

    /// The value at (x, y), with coordinates outside clamped to the edge.
    #[inline]
    pub fn at_clamped(&self, x: isize, y: isize) -> f32 {
        let x = x.clamp(0, self.width as isize - 1) as usize;
        let y = y.clamp(0, self.height as isize - 1) as usize;
        self.data[y * self.width + x]
    }

    /// Half the size in each direction, each pixel the mean of a 2x2 block
    /// (an odd last row or column is dropped).
    pub fn half(&self) -> Plane {
        Plane::from_rows(self.width / 2, self.height / 2, |y, row| {
            for (x, o) in row.iter_mut().enumerate() {
                let s = self.at(2 * x, 2 * y)
                    + self.at(2 * x + 1, 2 * y)
                    + self.at(2 * x, 2 * y + 1)
                    + self.at(2 * x + 1, 2 * y + 1);
                *o = 0.25 * s;
            }
        })
    }
}

/// Luma (BT.601 weights) of an RGB picture, in 0..=255.
pub fn luma(img: &RgbImage) -> Plane {
    let mut out = Plane::new(img.width, img.height);
    for (o, p) in out.data.iter_mut().zip(img.data.chunks_exact(3)) {
        *o = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
    }
    out
}
