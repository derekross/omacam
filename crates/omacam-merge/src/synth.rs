//! Synthetic bursts for the tests: a textured scene, hand shake as a whole
//! frame shift, sensor noise, and a square that moves from frame to frame.

use crate::image::RgbImage;

/// A small deterministic generator (xorshift64*), so tests need no crate.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    pub fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        ((self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 40) as f32) / (1u64 << 24) as f32
    }

    /// Standard normal (Box-Muller).
    pub fn normal(&mut self) -> f32 {
        let u1 = self.next_f32().max(1e-7);
        let u2 = self.next_f32();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f32::consts::PI * u2).cos()
    }
}

/// The scene's color at a point: gradients, edges and fine texture.
pub fn scene(x: f32, y: f32) -> [f32; 3] {
    let base = 60.0 + 0.25 * x + 0.15 * y;
    let check = if ((x / 40.0).floor() + (y / 40.0).floor()) as i64 % 2 == 0 {
        40.0
    } else {
        0.0
    };
    let tex = 12.0 * ((x * 0.35).sin() * (y * 0.27).cos());
    [
        base + check + tex,
        0.8 * base + check + 0.5 * tex,
        0.6 * base + 0.5 * check + 20.0,
    ]
}

/// A burst description: per-frame shift (hand shake) and the moving square's
/// top-left corner (`None`: no square in that frame).
pub struct Burst {
    pub width: usize,
    pub height: usize,
    pub shifts: Vec<(i32, i32)>,
    pub square: Vec<Option<(usize, usize)>>,
    pub square_size: usize,
}

impl Burst {
    /// The clean frame `i`: the scene seen through shift `i`, with the square.
    pub fn clean(&self, i: usize) -> Vec<f32> {
        let (dx, dy) = self.shifts[i];
        let mut out = vec![0.0; self.width * self.height * 3];
        for y in 0..self.height {
            for x in 0..self.width {
                let mut c = scene((x as i32 + dx) as f32, (y as i32 + dy) as f32);
                if let Some((sx, sy)) = self.square[i] {
                    if x >= sx && x < sx + self.square_size && y >= sy && y < sy + self.square_size
                    {
                        c = [230.0, 40.0, 40.0];
                    }
                }
                out[(y * self.width + x) * 3..][..3].copy_from_slice(&c);
            }
        }
        out
    }

    /// Frame `i` with Gaussian noise of `sigma` (8-bit units), quantized.
    pub fn noisy(&self, i: usize, sigma: f32, rng: &mut Rng) -> RgbImage {
        let data = self
            .clean(i)
            .iter()
            .map(|v| (v + sigma * rng.normal()).round().clamp(0.0, 255.0) as u8)
            .collect();
        RgbImage::from_raw(self.width, self.height, data).unwrap()
    }
}
