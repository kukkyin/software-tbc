//! Per-stage processing-time measurement with `std::time::Instant`
//! only (no external benchmarking crates, per the project rules).
//!
//! Run with: `cargo run --release -p tbc-sim --example bench`
//! (drop `--release` for the debug profile). Baseline values are
//! recorded in Issue #22.

use std::time::Instant;
use tbc_core::jitter::{apply_shifts, JitterDetector};
use tbc_core::procamp::{ProcAmp, ProcAmpConfig};
use tbc_core::sync::FrameSynchronizer;
use tbc_core::{FrameRate, PixelFormat, TbcConfig};
use tbc_sim::{DegradeParams, SyntheticSource};

fn ntsc() -> FrameRate {
    FrameRate {
        num: 30_000,
        den: 1001,
    }
}

fn pal() -> FrameRate {
    FrameRate { num: 25, den: 1 }
}

fn bench_stage(name: &str, frames: usize, mut f: impl FnMut()) {
    // Warm-up, then measure.
    for _ in 0..10 {
        f();
    }
    let start = Instant::now();
    for _ in 0..frames {
        f();
    }
    let elapsed = start.elapsed();
    let per_frame_us = elapsed.as_micros() as f64 / frames as f64;
    println!(
        "{name:<28} {per_frame_us:>10.1} us/frame  ({:.0} fps)",
        1_000_000.0 / per_frame_us
    );
}

fn main() {
    let frames = 300;
    for (label, width, height, rate) in [
        ("176x144 (QCIF-ish)", 176, 144, ntsc()),
        ("720x480 (NTSC SD)", 720, 480, ntsc()),
        ("720x576 (PAL SD)", 720, 576, pal()),
    ] {
        println!("== {label} ==");
        let params = DegradeParams {
            line_jitter_px: 3.0,
            dropout_probability: 0.01,
            pedestal_shift: 4,
            ..Default::default()
        };
        let mut source =
            SyntheticSource::new(width, height, PixelFormat::Yuv422p, rate, params, 99);
        let sample: Vec<_> = (0..frames).map(|_| source.next_frame()).collect();

        let config = TbcConfig {
            width,
            height,
            target_rate: rate,
            ..Default::default()
        };

        let mut idx = 0usize;
        bench_stage("sync ingest+output", frames, || {
            let mut sync = FrameSynchronizer::new(config.clone());
            let f = sample[idx % sample.len()].clone();
            idx += 1;
            sync.ingest(f);
            let _ = sync.next_output();
        });

        let mut detector = JitterDetector::new(8);
        let mut idx = 0usize;
        bench_stage("jitter estimate+apply", frames, || {
            let mut f = sample[idx % sample.len()].clone();
            idx += 1;
            if let Some(shifts) = detector.estimate(&f) {
                let _ = apply_shifts(&mut f, &shifts);
            }
        });

        // Split view of the jitter stage (Issue #22): detection and
        // correction measured separately.
        let mut detector = JitterDetector::new(8);
        let mut idx = 0usize;
        bench_stage("jitter estimate", frames, || {
            let f = sample[idx % sample.len()].clone();
            idx += 1;
            let _ = detector.estimate(&f);
        });
        if let Some(shifts) = JitterDetector::new(8).estimate(&sample[0]) {
            let mut idx = 0usize;
            bench_stage("jitter apply", frames, || {
                let mut f = sample[idx % sample.len()].clone();
                idx += 1;
                let _ = apply_shifts(&mut f, &shifts);
            });
        }

        let amp = ProcAmp::new(ProcAmpConfig {
            luma_gain: 1.05,
            pedestal: -4.0,
            saturation: 1.1,
            ..Default::default()
        });
        let mut idx = 0usize;
        bench_stage("proc-amp apply", frames, || {
            let mut f = sample[idx % sample.len()].clone();
            idx += 1;
            amp.apply(&mut f);
        });

        // End to end: sync -> jitter -> proc-amp on the degraded stream.
        let mut sync = FrameSynchronizer::new(config.clone());
        let mut detector = JitterDetector::new(8);
        let mut idx = 0usize;
        bench_stage("full pipeline", frames, || {
            let f = sample[idx % sample.len()].clone();
            idx += 1;
            sync.ingest(f);
            if let Some(mut out) = sync.next_output() {
                if let Some(shifts) = detector.estimate(&out) {
                    let _ = apply_shifts(&mut out, &shifts);
                }
                amp.apply(&mut out);
            }
        });
        println!();
    }
}
