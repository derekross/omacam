//! Merges a burst saved by libcamera's `cam` (XRGB8888 frames, one file
//! each) and writes the reference frame and the merge as PPM pictures:
//!
//! `merge_files WIDTH HEIGHT STRIDE plain|night|hdr OUT_DIR FRAME...`
//!
//! STRIDE is the bytes per row (`cam` reports it; usually WIDTH * 4).

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use omacam_merge::{merge_burst, MergeParams, RgbImage};

fn read_xrgb(path: &str, w: usize, h: usize, stride: usize) -> RgbImage {
    let raw = fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(
        raw.len() >= stride * h,
        "{path}: {} bytes, want {}",
        raw.len(),
        stride * h
    );
    let mut img = RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            // XRGB8888 is little-endian 0xXXRRGGBB: bytes B, G, R, X.
            let s = y * stride + x * 4;
            let d = (y * w + x) * 3;
            img.data[d] = raw[s + 2];
            img.data[d + 1] = raw[s + 1];
            img.data[d + 2] = raw[s];
        }
    }
    img
}

fn write_ppm(path: &Path, img: &RgbImage) {
    let mut f = fs::File::create(path).unwrap();
    write!(f, "P6\n{} {}\n255\n", img.width, img.height).unwrap();
    f.write_all(&img.data).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 6 {
        eprintln!("usage: merge_files WIDTH HEIGHT STRIDE plain|night|hdr OUT_DIR FRAME...");
        std::process::exit(2);
    }
    let (w, h, stride) = (
        args[0].parse().unwrap(),
        args[1].parse().unwrap(),
        args[2].parse().unwrap(),
    );
    let params = match args[3].as_str() {
        "plain" => MergeParams {
            fusion_evs: Vec::new(),
        },
        "night" => MergeParams::night(),
        "hdr" => MergeParams::hdr(1.5),
        m => panic!("unknown mode {m}"),
    };
    let out_dir = Path::new(&args[4]);
    fs::create_dir_all(out_dir).unwrap();
    let frames: Vec<RgbImage> = args[5..]
        .iter()
        .map(|p| read_xrgb(p, w, h, stride))
        .collect();
    let t = Instant::now();
    let out = merge_burst(&frames, &params).unwrap();
    println!(
        "{} frames merged in {:.2} s, reference {}, {:?}",
        frames.len(),
        t.elapsed().as_secs_f32(),
        out.reference,
        out.stats
    );
    write_ppm(&out_dir.join("reference.ppm"), &frames[out.reference]);
    write_ppm(&out_dir.join(format!("merged-{}.ppm", args[3])), &out.image);
}
