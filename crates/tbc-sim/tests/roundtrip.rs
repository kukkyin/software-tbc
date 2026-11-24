//! Round-trip regression tests (SPEC.md §8): degrade a known signal,
//! run it through the TBC processing chain, and compare against the
//! clean original with PSNR.

use tbc_core::jitter::{apply_shifts, JitterDetector};
use tbc_core::procamp::{ProcAmp, ProcAmpConfig};
use tbc_core::sync::FrameSynchronizer;
use tbc_core::{FrameRate, PixelFormat, TbcConfig};
use tbc_sim::{colour_bars, psnr, DegradeParams, SyntheticSource};

const W: u32 = 176;
const H: u32 = 144;
const FRAMES: u64 = 30;

fn rate() -> FrameRate {
    FrameRate {
        num: 30_000,
        den: 1001,
    }
}

fn config() -> TbcConfig {
    TbcConfig {
        width: W,
        height: H,
        ..Default::default()
    }
}

fn clean_frames() -> Vec<tbc_core::VideoFrame> {
    (0..FRAMES)
        .map(|i| {
            let mut f = colour_bars(W, H, PixelFormat::Yuv422p, i);
            f.pts_ns = (i as u128 * 1001 * 1_000_000_000 / 30_000) as u64;
            f
        })
        .collect()
}

fn mean_psnr(a: &[tbc_core::VideoFrame], b: &[tbc_core::VideoFrame]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let p = psnr(x, y);
            if p.is_infinite() {
                100.0
            } else {
                p
            }
        })
        .sum();
    sum / a.len() as f64
}

/// Clean signal through sync + jitter + neutral proc-amp must come out
/// essentially untouched, with the frame count preserved.
#[test]
fn roundtrip_clean_is_near_lossless() {
    let clean = clean_frames();
    let mut sync = FrameSynchronizer::new(config());
    let mut detector = JitterDetector::new(8);
    let amp = ProcAmp::new(ProcAmpConfig::default());

    let mut outputs = Vec::new();
    for frame in &clean {
        sync.ingest(frame.clone());
        let mut out = sync.next_output().expect("output");
        if let Some(shifts) = detector.estimate(&out) {
            apply_shifts(&mut out, &shifts).expect("apply");
        }
        amp.apply(&mut out);
        outputs.push(out);
    }

    assert_eq!(outputs.len(), clean.len());
    let quality = mean_psnr(&outputs, &clean);
    assert!(quality >= 40.0, "clean roundtrip PSNR was {quality:.1} dB");
}

/// Jitter + pedestal degradation, corrected by the detector (seeded
/// with the clean reference) and an inverse proc-amp, must score
/// clearly better than the uncorrected degraded signal.
#[test]
fn roundtrip_correction_improves_psnr() {
    let clean = clean_frames();
    let params = DegradeParams {
        line_jitter_px: 2.0,
        pedestal_shift: 8,
        ..Default::default()
    };
    let mut source = SyntheticSource::new(W, H, PixelFormat::Yuv422p, rate(), params, 1234);
    let degraded: Vec<_> = (0..FRAMES).map(|_| source.next_frame()).collect();

    let degraded_quality = mean_psnr(&degraded, &clean);

    // Correction chain: detector referenced on the clean signal, then
    // per-frame correction, then the inverse pedestal in the proc-amp.
    let mut detector = JitterDetector::new(8);
    let _ = detector.estimate(&clean[0]);
    let amp = ProcAmp::new(ProcAmpConfig {
        pedestal: -8.0,
        ..Default::default()
    });
    let mut corrected = Vec::new();
    for frame in &degraded {
        let mut f = frame.clone();
        if let Some(shifts) = detector.estimate(&f) {
            apply_shifts(&mut f, &shifts).expect("apply");
        }
        amp.apply(&mut f);
        corrected.push(f);
    }
    let corrected_quality = mean_psnr(&corrected, &clean);

    assert!(
        corrected_quality > degraded_quality + 3.0,
        "correction did not help enough: degraded {degraded_quality:.1} dB, corrected {corrected_quality:.1} dB"
    );
    assert!(
        corrected_quality >= 25.0,
        "corrected PSNR too low: {corrected_quality:.1} dB"
    );
}
