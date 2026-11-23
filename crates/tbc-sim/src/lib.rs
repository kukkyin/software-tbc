//! Synthetic unstable-video generator and measurement helpers.
//!
//! Produces clean test frames, then degrades them with the artefacts a
//! real TBC exists to fix: horizontal line jitter, clock drift, dropped
//! frames, dropout noise and pedestal shift. See SPEC.md §8.

use tbc_core::{FieldOrder, PixelFormat, VideoFrame};

/// Parameters of the synthetic degradation. All-zero = clean signal.
#[derive(Debug, Clone, Copy, Default)]
pub struct DegradeParams {
    /// Max horizontal line shift, in pixels (applied as a random walk).
    pub line_jitter_px: f32,
    /// Input clock drift relative to nominal, in parts per million.
    pub clock_drift_ppm: f64,
    /// Probability that any given frame is dropped entirely, 0.0..=1.0.
    pub drop_probability: f64,
    /// Probability that a line is replaced by noise (dropout), 0.0..=1.0.
    pub dropout_probability: f64,
    /// Constant luma offset added to every pixel, in code values.
    pub pedestal_shift: i16,
}

/// Generate one clean synthetic frame (colour bars) with the given geometry.
/// Degradation is layered on by the generator tracked in the Phase 4 issues.
pub fn colour_bars(width: u32, height: u32, format: PixelFormat, sequence: u64) -> VideoFrame {
    let mut frame = VideoFrame {
        width,
        height,
        format,
        field_order: FieldOrder::Progressive,
        pts_ns: 0,
        sequence,
        data: vec![0u8; format.frame_size(width, height)],
    };
    // Neutral grey in every layout the scaffold supports: good enough as a
    // deterministic placeholder until the Phase 4 generator lands.
    for b in frame.data.iter_mut() {
        *b = 128;
    }
    frame
}
