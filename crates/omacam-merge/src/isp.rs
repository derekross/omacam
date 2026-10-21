//! The software ISP's finish on the CPU: libcamera's GPU debayer
//! (`dpc_packed.frag`, `bayer_1x_packed.frag`, `postfilter.frag` in
//! libcamera 0.7.2 with Omarchy Mobile's patches 0029-0055) over a raw
//! Bayer picture, so a merged raw burst comes out with the look of the
//! camera's own frames.
//!
//! The ISP reads the 8 high bits of each 10-bit sample and keeps an 8-bit
//! picture between its passes. Here the samples stay floats throughout,
//! which is what a merge of many frames needs: its precision is finer than
//! one frame's. Fed one frame's 8 high bits, the result matches the
//! camera's own output to 0.05 levels (Omarchy Mobile's
//! docs/CAMERA-QUALITY-PLAN.md, Q5.1).
//!
//! The demosaic is McGuire's ("Efficient, High-Quality Bayer Demosaic
//! Filtering on GPUs", 2008) as simplified by Linaro for libcamera
//! (BSD-2-Clause, see NOTICE). The defective pixel correction, the color
//! and tone steps and the post-filter port Omarchy Mobile's own
//! libcamera patches (LGPL-2.1-or-later, taken here under the GPL).

use rayon::prelude::*;

/// A raw Bayer picture: one sample per pixel, 0..1 of the 8-bit scale the
/// ISP reads (a 10-bit sample divided by 1020), black level included.
#[derive(Clone, Debug)]
pub struct Mosaic {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
    /// Where the red sample of the 2x2 pattern sits: (column, row), each 0
    /// or 1 (libcamera's `firstRed`).
    pub first_red: (usize, usize),
}

/// Hue and vibrance by hue, and the shadow lift (libcamera's Look).
#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    /// Hue shift in radians at 0, 30, ... 330 degrees.
    pub hue_shift: [f32; 12],
    pub vibrance: [f32; 12],
    pub vibrance_limit: f32,
    pub tone_lift: f32,
}

impl Default for Look {
    fn default() -> Look {
        Look {
            hue_shift: [0.0; 12],
            vibrance: [0.0; 12],
            vibrance_limit: 1.0,
            tone_lift: 0.0,
        }
    }
}

/// The post-filter's strengths (libcamera's Detail, scaled by gain).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Detail {
    pub denoise: f32,
    pub chroma_denoise: f32,
    pub sharpen: f32,
}

impl Detail {
    /// The strengths libcamera's Detail sets for a frame at `gain` (analog
    /// times digital), from the tuning's base values and the Sharpness
    /// control.
    pub fn for_gain(base: Detail, sharpness: f32, gain: f32) -> Detail {
        let gain = gain.max(1.0);
        Detail {
            denoise: (base.denoise * gain.sqrt()).min(0.15),
            chroma_denoise: (base.chroma_denoise * gain.sqrt()).min(0.25),
            sharpen: base.sharpen * sharpness / (1.0 + gain.log2() / 3.0),
        }
    }
}

/// Everything the finish takes: the debayer's uniforms.
#[derive(Clone, Debug, PartialEq)]
pub struct IspParams {
    /// Black level on the 0..1 scale of [`Mosaic`].
    pub black: f32,
    /// White balance gains, red green blue.
    pub awb: [f32; 3],
    /// The color matrix with the saturation folded in, rows by output.
    pub ccm: [[f32; 3]; 3],
    /// Exponent of the display encoding (1 / 2.2).
    pub gamma: f32,
    /// The contrast curve's exponent ([`contrast_exp`]).
    pub contrast_exp: f32,
    pub look: Look,
    pub detail: Detail,
}

/// What a camera's libcamera tuning file (the simple pipeline's YAML)
/// says about the finish: the Look and the Detail base values. The rest of
/// [`IspParams`] comes from each frame's metadata.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Tuning {
    pub look: Look,
    pub detail: Detail,
}

impl Tuning {
    /// Read the `Look` and `Detail` sections of a tuning file. Missing
    /// sections or keys keep libcamera's defaults (no look; no detail).
    /// Only the flow style the tuning files use is understood: `key: 1.0`
    /// and `key: [ 1, 2, ... ]`, a list possibly over several lines.
    pub fn parse(yaml: &str) -> Tuning {
        let mut t = Tuning::default();
        let mut section = String::new();
        let mut lines = yaml.lines();
        while let Some(line) = lines.next() {
            let line = line.split('#').next().unwrap_or("");
            let trimmed = line.trim();
            if let Some(name) = trimmed.strip_prefix("- ") {
                section = name.trim_end_matches(':').trim().to_string();
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            let mut value = value.trim().to_string();
            if value.starts_with('[') {
                while !value.contains(']') {
                    match lines.next() {
                        Some(more) => value.push_str(more.split('#').next().unwrap_or("")),
                        None => break,
                    }
                }
            }
            let nums: Vec<f32> = value
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .filter_map(|v| v.trim().parse().ok())
                .collect();
            let one = nums.first().copied();
            match (section.as_str(), key.trim()) {
                ("Look", "hueShift") if nums.len() == 12 => {
                    for (o, v) in t.look.hue_shift.iter_mut().zip(&nums) {
                        *o = v.to_radians();
                    }
                }
                ("Look", "vibrance") if nums.len() == 12 => {
                    for (o, v) in t.look.vibrance.iter_mut().zip(&nums) {
                        *o = v.max(0.0);
                    }
                }
                ("Look", "vibranceLimit") => {
                    t.look.vibrance_limit = one.unwrap_or(1.0).max(0.05);
                }
                ("Look", "toneLift") => t.look.tone_lift = one.unwrap_or(0.0).max(0.0),
                ("Detail", "sharpen") => t.detail.sharpen = one.unwrap_or(0.0),
                ("Detail", "denoise") => t.detail.denoise = one.unwrap_or(0.0),
                ("Detail", "chromaDenoise") => t.detail.chroma_denoise = one.unwrap_or(0.0),
                _ => {}
            }
        }
        t
    }
}

/// libcamera's contrast exponent for a Contrast control value.
pub fn contrast_exp(contrast: f32) -> f32 {
    let a = (contrast as f64 * std::f64::consts::FRAC_PI_4)
        .clamp(0.0, std::f64::consts::FRAC_PI_2 - 0.00001);
    a.tan() as f32
}

/// A color matrix with libcamera's saturation folded in: through BT.601
/// YCbCr, the chroma scaled.
pub fn with_saturation(ccm: [[f32; 3]; 3], saturation: f32) -> [[f32; 3]; 3] {
    const RGB2YCC: [[f64; 3]; 3] = [
        [0.256788235294, 0.504129411765, 0.0979058823529],
        [-0.148223529412, -0.290992156863, 0.439215686275],
        [0.439215686275, -0.367788235294, -0.0714274509804],
    ];
    const YCC2RGB: [[f64; 3]; 3] = [
        [1.16438356164, 0.0, 1.59602678571],
        [1.16438356164, -0.391762290094, -0.812967647235],
        [1.16438356164, 2.01723214285, 0.0],
    ];
    let mul = |a: [[f64; 3]; 3], b: [[f64; 3]; 3]| {
        let mut o = [[0.0; 3]; 3];
        for (i, row) in o.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = (0..3).map(|k| a[i][k] * b[k][j]).sum();
            }
        }
        o
    };
    let s = saturation as f64;
    let sat = [[1.0, 0.0, 0.0], [0.0, s, 0.0], [0.0, 0.0, s]];
    let m = ccm.map(|r| r.map(|v| v as f64));
    mul(mul(mul(YCC2RGB, sat), RGB2YCC), m).map(|r| r.map(|v| v as f32))
}

/// The defective pixel pass: a sample far above (or below) all eight of its
/// color two pixels away takes their highest (or lowest).
fn dpc(m: &Mosaic) -> Vec<f32> {
    let (w, h) = (m.width as isize, m.height as isize);
    // Off the edge, the sample on the other side (two pixels the other way).
    let mirror = |i: isize, d: isize, n: isize| {
        let j = i + d;
        if j < 0 || j > n - 1 {
            i - d
        } else {
            j
        }
    };
    let mut out = vec![0.0f32; m.data.len()];
    out.par_chunks_mut(m.width)
        .enumerate()
        .for_each(|(y, row)| {
            let y = y as isize;
            for (x, o) in row.iter_mut().enumerate() {
                let x = x as isize;
                let c = m.data[(y * w + x) as usize];
                let (mut hi, mut lo) = (f32::MIN, f32::MAX);
                for dy in [-2, 0, 2] {
                    let sy = mirror(y, dy, h);
                    for dx in [-2, 0, 2] {
                        if dx == 0 && dy == 0 {
                            continue;
                        }
                        let v = m.data[(sy * w + mirror(x, dx, w)) as usize];
                        hi = hi.max(v);
                        lo = lo.min(v);
                    }
                }
                *o = if c - hi > hi.max(0.08) {
                    hi
                } else if lo - c > (0.5 * lo).max(0.08) {
                    lo
                } else {
                    c
                };
            }
        });
    out
}

/// One pixel's RGB from its 3x3 neighborhood (bilinear, McGuire/Linaro).
#[inline]
fn debayer_at(d: &[f32], w: usize, h: usize, x: usize, y: usize, fr: (usize, usize)) -> [f32; 3] {
    let at = |dx: isize, dy: isize| {
        let sx = (x as isize + dx).clamp(0, w as isize - 1) as usize;
        let sy = (y as isize + dy).clamp(0, h as isize - 1) as usize;
        d[sy * w + sx]
    };
    let c = at(0, 0);
    let px = (at(0, -1) + at(0, 1)) * 0.5;
    let py = (at(-1, 0) + at(1, 0)) * 0.5;
    let pz = (px + py) * 0.5;
    let pw = ((at(-1, -1) + at(1, 1)) * 0.5 + (at(1, -1) + at(-1, 1)) * 0.5) * 0.5;
    let even_col = (x + fr.0).is_multiple_of(2);
    let even_row = (y + fr.1).is_multiple_of(2);
    match (even_col, even_row) {
        (true, true) => [c, pz, pw],
        (true, false) => [px, c, py],
        (false, true) => [py, c, px],
        (false, false) => [pw, pz, c],
    }
}

const LUMA709: [f32; 3] = [0.2126, 0.7152, 0.0722];
const LUMA601: [f32; 3] = [0.299, 0.587, 0.114];

#[inline]
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Hue shift and vibrance by hue, on linear RGB.
fn look(rgb: [f32; 3], l: &Look) -> [f32; 3] {
    const STEP: f32 = std::f32::consts::FRAC_PI_6; // 30 degrees
    let y = dot(rgb, LUMA709);
    let ys = if y < 1e-4 { 1.0 } else { y };
    let cx = (rgb[2] - y) / ys;
    let cy = (rgb[0] - y) / ys;
    let r = cx.hypot(cy);
    if y < 1e-4 || r < 1e-4 {
        return rgb;
    }
    let mut h = cy.atan2(cx);
    if h < 0.0 {
        h += std::f32::consts::TAU;
    }
    let (mut dh, mut v) = (0.0, 0.0);
    for i in 0..12 {
        let d = (h - i as f32 * STEP).abs();
        let d = d.min(std::f32::consts::TAU - d);
        let wt = (1.0 - d / STEP).max(0.0);
        dh += wt * l.hue_shift[i];
        v += wt * l.vibrance[i];
    }
    let t = ((r - 0.10) / 0.20).clamp(0.0, 1.0);
    let wt = t * t * (3.0 - 2.0 * t);
    let h2 = h + wt * dh;
    let r2 = r * (1.0 + wt * v * (1.0 - r / l.vibrance_limit).max(0.0));
    let red = y * (1.0 + r2 * h2.sin());
    let blue = y * (1.0 + r2 * h2.cos());
    let green = (y - 0.2126 * red - 0.0722 * blue) / 0.7152;
    [red, green, blue]
}

/// The highlight roll-off above 0.6.
#[inline]
fn shoulder(x: f32) -> f32 {
    let t = ((x - 0.6) / 1.4).clamp(0.0, 1.0);
    0.6 + 0.4 * (1.0 - (-3.0 * t).exp()) / (1.0 - (-3.0f32).exp())
}

/// One demosaiced pixel to display-encoded RGB in 0..1: black level, white
/// balance, the clipped-highlight fade, color matrix, look, roll-off, tone
/// lift, gamut, contrast and gamma.
fn finish_pixel(rgb: [f32; 3], p: &IspParams) -> [f32; 3] {
    let k = 1.0 / (1.0 - p.black);
    let sensor = rgb.map(|v| (v - p.black) * k);
    let mut c = [
        sensor[0] * p.awb[0],
        sensor[1] * p.awb[1],
        sensor[2] * p.awb[2],
    ];
    // Where the sensor clipped, fade towards the brightest channel (white).
    let smax = sensor[0].max(sensor[1]).max(sensor[2]);
    let mut hl = ((smax - 0.8) / 0.2).clamp(0.0, 1.0);
    hl = hl * hl * (3.0 - 2.0 * hl);
    hl *= hl;
    let cmax = c[0].max(c[1]).max(c[2]);
    for v in &mut c {
        *v += (cmax - *v) * hl;
    }
    let m = &p.ccm;
    c = [dot(m[0], c), dot(m[1], c), dot(m[2], c)];
    c = look(c, &p.look);
    let peak = c[0].max(c[1]).max(c[2]);
    if peak > 0.6 {
        let s = shoulder(peak) / peak;
        c = c.map(|v| v * s);
    }
    let lift = p.look.tone_lift;
    if lift > 0.0 {
        let y = dot(c, LUMA709);
        if y >= 1e-5 {
            let yc = y.min(4.0);
            let f = yc * (1.0 + lift) / (yc + lift) / y;
            c = c.map(|v| v * f);
        }
    }
    // Into gamut along constant luma.
    let luma = dot(c, LUMA601).clamp(0.0, 1.0);
    let mut g = 1.0f32;
    for v in c {
        let ch = v - luma;
        if ch > 1e-5 {
            g = g.min((1.0 - luma) / ch);
        } else if ch < -1e-5 {
            g = g.min(luma / -ch);
        }
    }
    let e = p.contrast_exp;
    c.map(|v| {
        let v = (luma + (v - luma) * g).clamp(0.0, 1.0);
        let v = if v < 0.5 {
            0.5 * (v / 0.5).powf(e)
        } else {
            1.0 - 0.5 * ((1.0 - v) / 0.5).powf(e)
        };
        v.powf(p.gamma)
    })
}

/// The chroma filter's 16 taps: 8 at radius 2, 8 at radius 4, as the
/// shader's nearest-texel sampling rounds them.
fn chroma_taps() -> [(isize, isize, f32); 16] {
    let mut t = [(0, 0, 0.0); 16];
    for (k, o) in t.iter_mut().enumerate() {
        let a = k as f32 * std::f32::consts::FRAC_PI_4;
        let rad = if k < 8 { 2.0 } else { 4.0 };
        *o = (
            (0.5 + a.cos() * rad).floor() as isize,
            (0.5 + a.sin() * rad).floor() as isize,
            if k < 8 { 1.0 } else { 0.6 },
        );
    }
    t
}

/// The post-filter: luma noise reduction and sharpening, then chroma noise
/// reduction, over display-encoded RGB in 0..1 (three floats a pixel).
fn post(c: &[f32], w: usize, h: usize, d: Detail) -> Vec<f32> {
    let luma: Vec<f32> = c
        .par_chunks_exact(3)
        .map(|p| dot([p[0], p[1], p[2]], LUMA601))
        .collect();
    let taps = chroma_taps();
    let inv_den = if d.denoise > 0.0 {
        1.0 / d.denoise
    } else {
        0.0
    };
    let inv_y = 1.0 / (0.04 + 2.0 * d.denoise);
    let inv_c = if d.chroma_denoise > 0.0 {
        1.0 / d.chroma_denoise
    } else {
        0.0
    };
    let mut out = vec![0.0f32; c.len()];
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let clamp = |x: isize, y: isize| {
            let x = x.clamp(0, w as isize - 1) as usize;
            let y = y.clamp(0, h as isize - 1) as usize;
            y * w + x
        };
        for (x, o) in row.chunks_exact_mut(3).enumerate() {
            let i = y * w + x;
            let yc = luma[i];
            let (mut blur, mut ss, mut sw) = (4.0 * yc, 4.0 * yc, 4.0f32);
            for j in -1..=1isize {
                for k in -1..=1isize {
                    if j == 0 && k == 0 {
                        continue;
                    }
                    let yn = luma[clamp(x as isize + k, y as isize + j)];
                    let wt = if j == 0 || k == 0 { 2.0 } else { 1.0 };
                    let dd = (yn - yc) * inv_den;
                    let ws = wt * (-dd * dd).exp();
                    blur += wt * yn;
                    ss += ws * yn;
                    sw += ws;
                }
            }
            blur /= 16.0;
            let mut yo = if d.denoise > 0.0 { ss / sw } else { yc };
            let det = yc - blur;
            let det = det.signum() * (det.abs() - 0.5 * d.denoise).max(0.0);
            yo = (yo + d.sharpen * det).clamp(0.0, 1.0);
            let p = &c[i * 3..i * 3 + 3];
            if d.chroma_denoise <= 0.0 {
                for ch in 0..3 {
                    o[ch] = (p[ch] + yo - yc).clamp(0.0, 1.0);
                }
                continue;
            }
            let cc = [p[2] - yc, p[0] - yc];
            let (mut sb, mut sr, mut cw) = (cc[0], cc[1], 1.0f32);
            for &(dx, dy, wk) in &taps {
                let n = clamp(x as isize + dx, y as isize + dy);
                let q = &c[n * 3..n * 3 + 3];
                let yn = luma[n];
                let (nb, nr) = (q[2] - yn, q[0] - yn);
                let ddy = (yn - yc) * inv_y;
                let db = (nb - cc[0]) * inv_c;
                let dr = (nr - cc[1]) * inv_c;
                let wt = wk * (-ddy * ddy - db * db - dr * dr).exp();
                sb += wt * nb;
                sr += wt * nr;
                cw += wt;
            }
            let b = yo + sb / cw;
            let r = yo + sr / cw;
            let g = (yo - 0.299 * r - 0.114 * b) / 0.587;
            o[0] = r.clamp(0.0, 1.0);
            o[1] = g.clamp(0.0, 1.0);
            o[2] = b.clamp(0.0, 1.0);
        }
    });
    out
}

/// Finish a raw picture: RGB, three floats a pixel in 0..255 (the merge's
/// scale), display-encoded.
pub fn finish(m: &Mosaic, p: &IspParams) -> Vec<f32> {
    let (w, h) = (m.width, m.height);
    let d = dpc(m);
    let mut rgb = vec![0.0f32; w * h * 3];
    rgb.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        for (x, o) in row.chunks_exact_mut(3).enumerate() {
            o.copy_from_slice(&finish_pixel(debayer_at(&d, w, h, x, y, m.first_red), p));
        }
    });
    drop(d);
    let mut out = post(&rgb, w, h, p.detail);
    out.par_iter_mut().for_each(|v| *v *= 255.0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> IspParams {
        IspParams {
            black: 64.0 / 1020.0,
            awb: [1.0; 3],
            ccm: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            gamma: 1.0 / 2.2,
            contrast_exp: 1.0,
            look: Look::default(),
            detail: Detail::default(),
        }
    }

    #[test]
    fn contrast_and_saturation_match_libcamera() {
        // Contrast 1.2 gives the debayer's 1.37638.
        assert!((contrast_exp(1.2) - 1.37638).abs() < 1e-4);
        assert!((contrast_exp(1.0) - 1.0).abs() < 1e-5);
        // The S5KGN1's matrix with saturation 1.1 is what the debayer got.
        let ccm = [
            [1.3184, -0.1750, -0.1092],
            [-0.1442, 1.2800, -0.1456],
            [-0.1081, -0.3850, 1.5496],
        ];
        let want = [
            [1.42052, -0.258014, -0.125974],
            [-0.188343, 1.34248, -0.166014],
            [-0.148635, -0.489013, 1.69871],
        ];
        let got = with_saturation(ccm, 1.1);
        for i in 0..3 {
            for j in 0..3 {
                assert!((got[i][j] - want[i][j]).abs() < 1e-4, "{got:?}");
            }
        }
    }

    #[test]
    fn the_tuning_file_gives_the_look_and_detail() {
        let yaml = "algorithms:\n  - Ccm:\n      ccms:\n        - ct: 5000\n          ccm: [ 1, 0, 0,\n                 0, 1, 0 ]\n  - Look:\n      #  0 30\n      hueShift: [ -5, -2,  0,  0,  0, -5, -15, -15, -6,  0,  0, -5 ]\n      vibrance: [ 0.6, 0.4, 0.0, 0.0, 0.0, 0.3, 1.0, 1.0, 0.8, 0.4, 0.4, 0.6 ]\n      vibranceLimit: 1.0\n      toneLift: 0.45\n  - Detail:\n      sharpen: 0.6\n      denoise: 0.015\n      chromaDenoise: 0.03\n  - Agc:\n";
        let t = Tuning::parse(yaml);
        assert!((t.look.hue_shift[6] + 15f32.to_radians()).abs() < 1e-6);
        assert_eq!(t.look.vibrance[8], 0.8);
        assert_eq!(t.look.tone_lift, 0.45);
        assert_eq!(
            t.detail,
            Detail {
                denoise: 0.015,
                chroma_denoise: 0.03,
                sharpen: 0.6
            }
        );
        // At gain 8.5, what the debayer got.
        let d = Detail::for_gain(t.detail, 1.0, 8.5);
        assert!((d.denoise - 0.043732).abs() < 1e-5 && (d.sharpen - 0.29569).abs() < 1e-4);
        assert_eq!(Tuning::parse(""), Tuning::default());
    }

    #[test]
    fn gray_stays_gray_and_black_stays_black() {
        let p = params();
        let mut m = Mosaic {
            width: 16,
            height: 16,
            data: vec![0.4; 256],
            first_red: (1, 0),
        };
        let out = finish(&m, &p);
        let px = &out[(8 * 16 + 8) * 3..][..3];
        assert!(
            (px[0] - px[1]).abs() < 0.5 && (px[2] - px[1]).abs() < 0.5,
            "{px:?}"
        );
        assert!(px[1] > 100.0 && px[1] < 200.0, "{px:?}");
        m.data.fill(p.black);
        assert!(finish(&m, &p).iter().all(|&v| v < 0.5));
    }

    #[test]
    fn the_bayer_phase_picks_the_red_sample() {
        // Red samples bright, the rest at black: the picture is red.
        let p = params();
        let (w, h) = (16, 16);
        let fr = (1, 0);
        let mut m = Mosaic {
            width: w,
            height: h,
            data: vec![p.black; w * h],
            first_red: fr,
        };
        for y in 0..h {
            for x in 0..w {
                if (x + fr.0) % 2 == 0 && (y + fr.1) % 2 == 0 {
                    m.data[y * w + x] = 0.5;
                }
            }
        }
        let out = finish(&m, &p);
        let px = &out[(8 * w + 8) * 3..][..3];
        assert!(px[0] > 100.0 && px[1] < 30.0 && px[2] < 30.0, "{px:?}");
    }

    #[test]
    fn a_hot_pixel_is_corrected() {
        let mut m = Mosaic {
            width: 16,
            height: 16,
            data: vec![0.2; 256],
            first_red: (1, 0),
        };
        m.data[8 * 16 + 8] = 0.9;
        assert_eq!(dpc(&m)[8 * 16 + 8], 0.2);
    }
}
