//! Times the merge on a synthetic full-size burst:
//! `cargo run --release -p omacam-merge --example bench [frames] [width] [height]`.

use std::time::Instant;

use omacam_merge::synth::{Burst, Rng};
use omacam_merge::{merge_burst, MergeParams};

fn main() {
    let mut args = std::env::args()
        .skip(1)
        .map(|a| a.parse::<usize>().unwrap());
    let n = args.next().unwrap_or(12);
    let w = args.next().unwrap_or(4080);
    let h = args.next().unwrap_or(3072);
    let b = Burst {
        width: w,
        height: h,
        shifts: (0..n as i32).map(|i| (3 * i - 15, 11 - 2 * i)).collect(),
        square: vec![None; n],
        square_size: 0,
    };
    let mut rng = Rng::new(1);
    let frames: Vec<_> = (0..n).map(|i| b.noisy(i, 8.0, &mut rng)).collect();
    for (name, params) in [
        (
            "plain",
            MergeParams {
                fusion_evs: Vec::new(),
            },
        ),
        ("night", MergeParams::night()),
        ("hdr", MergeParams::hdr(1.5)),
    ] {
        let t = Instant::now();
        let out = merge_burst(&frames, &params).unwrap();
        println!(
            "{name}: {n} frames of {w}x{h} in {:.2} s on {} threads (reference {})",
            t.elapsed().as_secs_f32(),
            rayon::current_num_threads(),
            out.reference
        );
        for (stage, d) in &out.stats.timings {
            println!("  {stage}: {:.0} ms", d.as_secs_f64() * 1000.0);
        }
    }
}
