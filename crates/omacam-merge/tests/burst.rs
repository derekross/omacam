//! The merge on synthetic bursts with known motion and noise.

use omacam_merge::synth::{Burst, Rng};
use omacam_merge::{merge_burst, MergeError, MergeParams, RgbImage};

const W: usize = 256;
const H: usize = 192;
const SIGMA: f32 = 8.0;

fn burst(n: usize) -> Burst {
    // Hand shake of up to about 20 px, and a 24 px square crossing the frame.
    let shifts = [
        (0, 0),
        (7, -3),
        (-11, 5),
        (18, 9),
        (-4, -16),
        (13, -8),
        (-19, 12),
        (3, 20),
    ];
    Burst {
        width: W,
        height: H,
        shifts: shifts[..n].to_vec(),
        square: (0..n).map(|i| Some((20 + 28 * i, 140))).collect(),
        square_size: 24,
    }
}

fn plain() -> MergeParams {
    MergeParams {
        fusion_evs: Vec::new(),
    }
}

/// RMS error against the clean reference frame over the pixels `keep` says.
fn rms(out: &RgbImage, truth: &[f32], keep: impl Fn(usize, usize) -> bool) -> f32 {
    let (mut s, mut n) = (0.0f64, 0usize);
    for y in 0..H {
        for x in 0..W {
            if !keep(x, y) {
                continue;
            }
            for c in 0..3 {
                let i = (y * W + x) * 3 + c;
                let d = out.data[i] as f64 - truth[i] as f64;
                s += d * d;
                n += 1;
            }
        }
    }
    (s / n as f64).sqrt() as f32
}

#[test]
fn eight_frames_cut_the_noise_on_still_parts() {
    let b = burst(8);
    let mut rng = Rng::new(7);
    let frames: Vec<RgbImage> = (0..8).map(|i| b.noisy(i, SIGMA, &mut rng)).collect();
    let out = merge_burst(&frames, &plain()).unwrap();
    let truth = b.clean(out.reference);
    // Away from the square's path and the picture's border (frames shifted
    // past the edge repeat it, so some border tiles merge fewer frames).
    let still = |x: usize, y: usize| x > 24 && x < W - 24 && y > 24 && y < 120;
    let single = rms(&frames[out.reference], &truth, still);
    let merged = rms(&out.image, &truth, still);
    eprintln!(
        "still parts: single frame RMS {single:.2}, merged {merged:.2}, {:?}",
        out.stats
    );
    // Ideal: sigma / sqrt(8) = 2.83 (measured 2.83).
    assert!(single > 7.0, "single frame RMS {single}");
    assert!(
        merged < 3.3,
        "merged RMS {merged} (single {single}), stats {:?}",
        out.stats
    );
}

#[test]
fn a_moving_square_leaves_no_ghost() {
    let b = burst(8);
    let mut rng = Rng::new(11);
    let frames: Vec<RgbImage> = (0..8).map(|i| b.noisy(i, SIGMA, &mut rng)).collect();
    let out = merge_burst(&frames, &plain()).unwrap();
    let truth = b.clean(out.reference);
    // The square's band, everywhere: where other frames had the square, the
    // merge must show the reference's background, not a faint red copy.
    let band = |x: usize, y: usize| (140..164).contains(&y) && x > 8 && x < W - 8;
    let err = rms(&out.image, &truth, band);
    eprintln!("square's band: merged RMS {err:.2}");
    // Measured 4.4, against 8 for one frame and 19 when rejection compared
    // luma only and let the red square through.
    assert!(err < 6.0, "RMS in the square's band {err}");
}

#[test]
fn hdr_fusion_lifts_a_dark_burst() {
    let b = burst(6);
    let mut rng = Rng::new(3);
    // The burst two stops under: every value a quarter in linear light,
    // about 0.53 of it in display encoding.
    let frames: Vec<RgbImage> = (0..6)
        .map(|i| {
            let mut f = b.noisy(i, SIGMA * 0.5, &mut rng);
            for v in &mut f.data {
                *v = (*v as f32 * 0.53).round() as u8;
            }
            f
        })
        .collect();
    let mean =
        |img: &RgbImage| img.data.iter().map(|&v| v as f32).sum::<f32>() / img.data.len() as f32;
    let flat = merge_burst(&frames, &plain()).unwrap();
    let hdr = merge_burst(&frames, &MergeParams::hdr()).unwrap();
    assert!(
        mean(&hdr.image) > 1.2 * mean(&flat.image),
        "{} vs {}",
        mean(&hdr.image),
        mean(&flat.image)
    );
    let clipped = hdr.image.data.iter().filter(|&&v| v == 255).count();
    assert!(
        clipped < hdr.image.data.len() / 100,
        "{clipped} clipped values"
    );
}

#[test]
fn bad_bursts_are_refused() {
    let one = vec![RgbImage::new(64, 64)];
    assert_eq!(
        merge_burst(&one, &plain()).unwrap_err(),
        MergeError::TooFewFrames(1)
    );
    let mixed = vec![RgbImage::new(64, 64), RgbImage::new(64, 48)];
    assert!(matches!(
        merge_burst(&mixed, &plain()),
        Err(MergeError::SizeMismatch(1, ..))
    ));
    let tiny = vec![RgbImage::new(16, 16), RgbImage::new(16, 16)];
    assert_eq!(
        merge_burst(&tiny, &plain()).unwrap_err(),
        MergeError::TooSmall(16, 16)
    );
}
