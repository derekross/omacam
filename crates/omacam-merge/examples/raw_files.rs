//! The raw path on bursts saved by libcamera's `cam` (both streams, one
//! file a frame), for the S5KGN1 main camera at 4080x3072:
//!
//! `raw_files compare TUNING RAW PROCESSED AWB_R AWB_B GAIN`
//!   finishes one raw frame from its 8 high bits, as the ISP reads it, and
//!   compares it with the ISP's own XRGB8888 frame (AWB_R and AWB_B are the
//!   gains the ISP applied: the previous frame's ColourGains).
//!
//! `raw_files merge plain|night|hdr TUNING AWB_R AWB_B GAIN RAW_GAIN OUT RAW...`
//!   merges the raw frames, finishes them and writes OUT as a PPM picture.
//!
//! The color matrix, saturation and contrast are the camera's metadata for
//! these captures (`cam --metadata`).

use std::fs;
use std::io::Write;
use std::time::Instant;

use omacam_merge::isp::{self, contrast_exp, with_saturation, Detail, IspParams, Mosaic, Tuning};
use omacam_merge::{merge_raw_burst, MergeParams, RawFinish, RawQuads};

const W: usize = 4080;
const H: usize = 3072;
const STRIDE: usize = 5120;
const FIRST_RED: (usize, usize) = (1, 0);
const CCM: [[f32; 3]; 3] = [
    [1.3184, -0.1750, -0.1092],
    [-0.1442, 1.2800, -0.1456],
    [-0.1081, -0.3850, 1.5496],
];

fn params(tuning: &str, awb_r: f32, awb_b: f32, gain: f32) -> IspParams {
    let t = Tuning::parse(&fs::read_to_string(tuning).expect("tuning file"));
    IspParams {
        black: 64.0 / 1020.0,
        awb: [awb_r, 1.0, awb_b],
        ccm: with_saturation(CCM, 1.1),
        gamma: 1.0 / 2.2,
        contrast_exp: contrast_exp(1.2),
        look: t.look,
        detail: Detail::for_gain(t.detail, 1.0, gain),
    }
}

fn quads(path: &str) -> RawQuads {
    let b = fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    RawQuads::from_csi2p10(&b, W, H, STRIDE).unwrap()
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let f = |i: usize| a[i].parse::<f32>().unwrap();
    match a[0].as_str() {
        "compare" => {
            let p = params(&a[1], f(4), f(5), f(6));
            // The ISP reads the 8 high bits of each sample.
            let q = quads(&a[2]);
            let mut m = Mosaic {
                width: W,
                height: H,
                data: vec![0.0; W * H],
                first_red: FIRST_RED,
            };
            for qy in 0..H / 2 {
                for qx in 0..W / 2 {
                    for i in 0..4 {
                        let v = q.data[(qy * (W / 2) + qx) * 4 + i] >> 2;
                        m.data[(2 * qy + i / 2) * W + 2 * qx + i % 2] = v as f32 / 255.0;
                    }
                }
            }
            let t = Instant::now();
            let ours = isp::finish(&m, &p);
            let ms = t.elapsed().as_millis();
            let theirs = fs::read(&a[3]).unwrap();
            // The ISP's rows are padded (4096 pixels for 4080).
            let ts = theirs.len() / H;
            let (mut sum, mut n, mut max) = (0.0f64, 0usize, 0.0f32);
            for y in 8..H - 8 {
                for x in 8..W - 8 {
                    let s = &theirs[y * ts + x * 4..];
                    let o = &ours[(y * W + x) * 3..];
                    for (c, t) in [(0, s[2]), (1, s[1]), (2, s[0])] {
                        let d = (o[c].round().clamp(0.0, 255.0) - t as f32).abs();
                        sum += d as f64;
                        max = max.max(d);
                        n += 1;
                    }
                }
            }
            println!(
                "finish {ms} ms; mean |diff| {:.3} levels, max {max}",
                sum / n as f64
            );
            if let Some(out) = a.get(7) {
                let bytes: Vec<u8> = ours
                    .iter()
                    .map(|v| v.round().clamp(0.0, 255.0) as u8)
                    .collect();
                let mut file = fs::File::create(out).unwrap();
                write!(file, "P6\n{W} {H}\n255\n").unwrap();
                file.write_all(&bytes).unwrap();
            }
            let i = (1536 * W + 2040) * 3;
            println!(
                "pixel (2040, 1536) ours {:?} theirs {:?} raw {:?}",
                &ours[i..i + 3],
                [
                    theirs[1536 * ts + 2040 * 4 + 2],
                    theirs[1536 * ts + 2040 * 4 + 1],
                    theirs[1536 * ts + 2040 * 4]
                ],
                &m.data[1536 * W + 2039..1536 * W + 2042]
            );
        }
        "merge" => {
            let mp = match a[1].as_str() {
                "plain" => MergeParams {
                    fusion_evs: Vec::new(),
                },
                "night" => MergeParams::night(),
                _ => MergeParams {
                    fusion_evs: vec![0.0, 1.0],
                },
            };
            let fin = RawFinish {
                isp: params(&a[2], f(3), f(4), f(5)),
                first_red: FIRST_RED,
                gain: f(6),
            };
            let frames: Vec<RawQuads> = a[8..].iter().map(|p| quads(p)).collect();
            let t = Instant::now();
            let out = merge_raw_burst(&frames, &fin, &mp).unwrap();
            println!(
                "{} frames in {:.2} s on {} threads, reference {}, weights {:?}, {:?}",
                frames.len(),
                t.elapsed().as_secs_f32(),
                rayon::current_num_threads(),
                out.reference,
                out.stats.frame_weights,
                out.stats.timings
            );
            let mut file = fs::File::create(&a[7]).unwrap();
            write!(file, "P6\n{} {}\n255\n", out.image.width, out.image.height).unwrap();
            file.write_all(&out.image.data).unwrap();
        }
        m => panic!("unknown mode {m}"),
    }
}
