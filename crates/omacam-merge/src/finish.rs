//! The finish: exposure fusion (Mertens, Kautz and Van Reeth, "Exposure
//! Fusion", 2007) of the merged picture with brighter copies of itself, so
//! the shadows a short burst left dark come back while the highlights keep
//! their detail.
//!
//! The merge is cleaner than any one frame, so it can be brightened without
//! the noise a single frame would show. Fusion works on the luma only (a
//! third of the memory of fusing RGB); the result scales each pixel's RGB by
//! its new luma over its old, which keeps the color.

use crate::image::Plane;

/// The display encoding the software ISP writes is close to a 2.2 gamma.
const GAMMA: f32 = 2.2;
/// Well-exposedness: Gaussian around mid-gray with this sigma (0..1 scale).
const EXPOSEDNESS_SIGMA: f32 = 0.2;
/// The most a pixel's luma may be raised, so near-black noise isn't blown up.
const MAX_GAIN: f32 = 6.0;

fn to_linear(v: f32) -> f32 {
    (v / 255.0).clamp(0.0, 1.0).powf(GAMMA)
}

fn from_linear(v: f32) -> f32 {
    v.clamp(0.0, 1.0).powf(1.0 / GAMMA)
}

/// Mertens' weight for one luma value of one exposure: contrast (the
/// absolute Laplacian) times well-exposedness. Saturation isn't used: the
/// copies differ only in brightness.
fn weights(img: &Plane) -> Plane {
    Plane::from_rows(img.width, img.height, |y, row| {
        for (x, o) in row.iter_mut().enumerate() {
            let (xi, yi) = (x as isize, y as isize);
            let c = img.at(x, y);
            let lap = (img.at_clamped(xi - 1, yi)
                + img.at_clamped(xi + 1, yi)
                + img.at_clamped(xi, yi - 1)
                + img.at_clamped(xi, yi + 1)
                - 4.0 * c)
                .abs();
            let e = (-(c - 0.5) * (c - 0.5) / (2.0 * EXPOSEDNESS_SIGMA * EXPOSEDNESS_SIGMA)).exp();
            *o = (lap + 0.02) * e + 1e-6;
        }
    })
}

/// 5-tap binomial blur then every other pixel: one Gaussian pyramid step.
fn reduce(p: &Plane) -> Plane {
    const K: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
    let (w, h) = (p.width.div_ceil(2), p.height.div_ceil(2));
    let tmp = Plane::from_rows(w, p.height, |y, row| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut s = 0.0;
            for (k, kv) in K.iter().enumerate() {
                s += kv * p.at_clamped(2 * x as isize + k as isize - 2, y as isize);
            }
            *o = s;
        }
    });
    Plane::from_rows(w, h, |y, row| {
        for (x, o) in row.iter_mut().enumerate() {
            let mut s = 0.0;
            for (k, kv) in K.iter().enumerate() {
                s += kv * tmp.at_clamped(x as isize, 2 * y as isize + k as isize - 2);
            }
            *o = s;
        }
    })
}

/// Back up one level: bilinear upsampling to `w` x `h`.
fn expand(p: &Plane, w: usize, h: usize) -> Plane {
    Plane::from_rows(w, h, |y, row| {
        let sy = ((y as f32 + 0.5) / 2.0 - 0.5).max(0.0);
        let (y0, fy) = (sy.floor() as isize, sy.fract());
        for (x, o) in row.iter_mut().enumerate() {
            let sx = ((x as f32 + 0.5) / 2.0 - 0.5).max(0.0);
            let (x0, fx) = (sx.floor() as isize, sx.fract());
            let a = p.at_clamped(x0, y0) * (1.0 - fx) + p.at_clamped(x0 + 1, y0) * fx;
            let b = p.at_clamped(x0, y0 + 1) * (1.0 - fx) + p.at_clamped(x0 + 1, y0 + 1) * fx;
            *o = a * (1.0 - fy) + b * fy;
        }
    })
}

fn gaussian_pyramid(p: Plane, levels: usize) -> Vec<Plane> {
    let mut out = vec![p];
    for _ in 1..levels {
        let next = reduce(out.last().unwrap());
        out.push(next);
    }
    out
}

fn laplacian_pyramid(p: Plane, levels: usize) -> Vec<Plane> {
    let g = gaussian_pyramid(p, levels);
    let mut out = Vec::with_capacity(levels);
    for l in 0..levels - 1 {
        let up = expand(&g[l + 1], g[l].width, g[l].height);
        let mut d = g[l].clone();
        for (a, b) in d.data.iter_mut().zip(&up.data) {
            *a -= b;
        }
        out.push(d);
    }
    out.push(g[levels - 1].clone());
    out
}

fn collapse(mut pyr: Vec<Plane>) -> Plane {
    let mut cur = pyr.pop().unwrap();
    while let Some(mut finer) = pyr.pop() {
        let up = expand(&cur, finer.width, finer.height);
        for (a, b) in finer.data.iter_mut().zip(&up.data) {
            *a += b;
        }
        cur = finer;
    }
    cur
}

/// Fuse the luma (8-bit units, display-encoded) with copies brightened by
/// each of `evs` stops (0 is the picture itself), and return the gain per
/// pixel to apply to its RGB.
pub fn fusion_gain(luma: &Plane, evs: &[f32]) -> Plane {
    let levels = {
        let mut n = 1;
        let mut m = luma.width.min(luma.height);
        while m > 8 && n < 9 {
            m /= 2;
            n += 1;
        }
        n
    };
    let exposures: Vec<Plane> = evs
        .iter()
        .map(|ev| {
            let k = 2f32.powf(*ev);
            let mut p = luma.clone();
            for v in &mut p.data {
                *v = from_linear(to_linear(*v) * k);
            }
            p
        })
        .collect();
    let ws: Vec<Plane> = exposures.iter().map(weights).collect();
    // Normalize the weights across exposures, per pixel.
    let mut total = Plane::new(luma.width, luma.height);
    for w in &ws {
        for (t, v) in total.data.iter_mut().zip(&w.data) {
            *t += v;
        }
    }
    let mut fused: Option<Vec<Plane>> = None;
    for (img, mut w) in exposures.into_iter().zip(ws) {
        for (v, t) in w.data.iter_mut().zip(&total.data) {
            *v /= t;
        }
        let gw = gaussian_pyramid(w, levels);
        let li = laplacian_pyramid(img, levels);
        let contrib: Vec<Plane> = li
            .into_iter()
            .zip(&gw)
            .map(|(mut l, g)| {
                for (a, b) in l.data.iter_mut().zip(&g.data) {
                    *a *= b;
                }
                l
            })
            .collect();
        fused = Some(match fused {
            None => contrib,
            Some(mut acc) => {
                for (a, c) in acc.iter_mut().zip(&contrib) {
                    for (x, y) in a.data.iter_mut().zip(&c.data) {
                        *x += y;
                    }
                }
                acc
            }
        });
    }
    let out = collapse(fused.unwrap());
    let mut gain = Plane::new(luma.width, luma.height);
    for ((g, o), l) in gain.data.iter_mut().zip(&out.data).zip(&luma.data) {
        let before = (l / 255.0).max(1.0 / 255.0);
        *g = (o.clamp(0.0, 1.0) / before).clamp(0.5, MAX_GAIN);
    }
    gain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pyramid_collapses_back_to_the_picture() {
        let mut p = Plane::new(37, 23);
        for (i, v) in p.data.iter_mut().enumerate() {
            *v = ((i * 7919) % 255) as f32 / 255.0;
        }
        let back = collapse(laplacian_pyramid(p.clone(), 4));
        for (a, b) in back.data.iter().zip(&p.data) {
            assert!((a - b).abs() < 1e-4);
        }
    }

    #[test]
    fn fusion_lifts_shadows_and_spares_highlights() {
        // Left half dark (luma 20), right half bright (luma 220).
        let mut p = Plane::new(64, 32);
        for y in 0..32 {
            for x in 0..64 {
                p.data[y * 64 + x] = if x < 32 { 20.0 } else { 220.0 };
            }
        }
        let g = fusion_gain(&p, &[0.0, 1.0, 2.0]);
        assert!(g.at(8, 16) > 1.5, "shadow gain {}", g.at(8, 16));
        assert!(g.at(56, 16) < 1.1, "highlight gain {}", g.at(56, 16));
    }
}
