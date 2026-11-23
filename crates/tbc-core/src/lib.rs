//! Core signal-processing pipeline of the software TBC.
//!
//! This crate is deliberately free of I/O: it transforms [`VideoFrame`]s
//! according to a [`TbcConfig`] and reports [`Telemetry`] snapshots.
//! See `SPEC.md` at the repository root for the full design.

pub mod jitter;
pub mod pool;
pub mod procamp;
pub mod sync;
pub mod telemetry;

use thiserror::Error;

/// Errors produced by the TBC pipeline.
#[derive(Debug, Error)]
pub enum TbcError {
    /// A frame buffer did not match the size implied by its format/geometry.
    #[error("frame size mismatch: expected {expected} bytes, got {actual}")]
    FrameSizeMismatch { expected: usize, actual: usize },
    /// The supplied configuration is internally inconsistent.
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    /// The input signal was lost or unusable and the guard policy gave up.
    #[error("signal lost")]
    SignalLost,
}

/// Raw pixel layouts the pipeline can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Planar YUV 4:2:0, 8-bit (FFmpeg `yuv420p`).
    Yuv420p,
    /// Planar YUV 4:2:2, 8-bit (FFmpeg `yuv422p`).
    Yuv422p,
    /// Packed RGB, 8-bit per channel (FFmpeg `rgb24`).
    Rgb24,
}

impl PixelFormat {
    /// Bytes required for one frame of `width` x `height` in this format.
    pub fn frame_size(self, width: u32, height: u32) -> usize {
        let px = (width * height) as usize;
        match self {
            PixelFormat::Yuv420p => px * 3 / 2,
            PixelFormat::Yuv422p => px * 2,
            PixelFormat::Rgb24 => px * 3,
        }
    }
}

/// Rational frame rate, e.g. 30000/1001 for 29.97 fps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

impl FrameRate {
    pub const NTSC_29_97: FrameRate = FrameRate {
        num: 30000,
        den: 1001,
    };
    pub const NTSC_59_94: FrameRate = FrameRate {
        num: 60000,
        den: 1001,
    };
    pub const PAL_25: FrameRate = FrameRate { num: 25, den: 1 };
    pub const PAL_50: FrameRate = FrameRate { num: 50, den: 1 };

    /// Frames per second as a float (display/telemetry only, never scheduling).
    pub fn as_fps(&self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// Duration of one frame in nanoseconds (integer scheduling base).
    pub fn frame_duration_ns(&self) -> u64 {
        1_000_000_000u64 * self.den as u64 / self.num as u64
    }
}

/// Which field is stored/displayed first in an interlaced frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOrder {
    Progressive,
    TopFieldFirst,
    BottomFieldFirst,
}

/// One video frame plus the metadata the pipeline needs.
///
/// Pixel data lives in a reusable buffer owned by a frame pool
/// (see SPEC.md §5); `data` is the scaffold representation.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub field_order: FieldOrder,
    /// Presentation timestamp in nanoseconds, on the *input* clock.
    pub pts_ns: u64,
    /// Monotonic sequence number assigned at ingest.
    pub sequence: u64,
    pub data: Vec<u8>,
}

impl VideoFrame {
    /// Validate that `data` matches the declared geometry and format.
    pub fn validate(&self) -> Result<(), TbcError> {
        let expected = self.format.frame_size(self.width, self.height);
        if self.data.len() != expected {
            return Err(TbcError::FrameSizeMismatch {
                expected,
                actual: self.data.len(),
            });
        }
        Ok(())
    }
}

/// What to do when the output clock needs a frame but none is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RatePolicy {
    /// Repeat the previous frame (hold) / drop late frames. Lowest latency.
    HoldDrop,
    /// Blend the two neighbouring frames by temporal weight.
    Blend,
    /// Motion-compensated interpolation (future work, falls back to Blend).
    MotionInterpolate,
}

/// What to output while the input signal is unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalLossPolicy {
    /// Freeze on the last good frame.
    LastFrameHold,
    /// Output black frames.
    Blackout,
    /// Output a simple test pattern (colour bars).
    TestPattern,
}

/// Full pipeline configuration. Mirrors the TOML schema in SPEC.md §7.
#[derive(Debug, Clone)]
pub struct TbcConfig {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub field_order: FieldOrder,
    pub target_rate: FrameRate,
    pub rate_policy: RatePolicy,
    /// Ring-buffer depth in frames. Larger = more jitter absorption, more latency.
    pub buffer_frames: usize,
    pub procamp: procamp::ProcAmpConfig,
    pub jitter_correction: bool,
    pub signal_loss: SignalLossPolicy,
}

impl TbcConfig {
    /// Check the configuration for inconsistencies before a run starts.
    pub fn validate(&self) -> Result<(), TbcError> {
        if self.width == 0 || self.height == 0 {
            return Err(TbcError::InvalidConfig(
                "width and height must be non-zero".into(),
            ));
        }
        if self.format == PixelFormat::Yuv420p && (self.width % 2 == 1 || self.height % 2 == 1) {
            return Err(TbcError::InvalidConfig(
                "yuv420p requires even width and height".into(),
            ));
        }
        if self.buffer_frames == 0 || self.buffer_frames > 240 {
            return Err(TbcError::InvalidConfig(
                "buffer_frames must be within 1..=240".into(),
            ));
        }
        if self.target_rate.num == 0 || self.target_rate.den == 0 {
            return Err(TbcError::InvalidConfig(
                "target_rate numerator and denominator must be non-zero".into(),
            ));
        }
        let p = &self.procamp;
        if ![
            p.luma_gain,
            p.pedestal,
            p.saturation,
            p.hue_degrees,
            p.gamma,
        ]
        .iter()
        .all(|v| v.is_finite())
        {
            return Err(TbcError::InvalidConfig(
                "procamp values must be finite".into(),
            ));
        }
        Ok(())
    }
}

impl Default for TbcConfig {
    fn default() -> Self {
        TbcConfig {
            width: 720,
            height: 480,
            format: PixelFormat::Yuv422p,
            field_order: FieldOrder::BottomFieldFirst,
            target_rate: FrameRate::NTSC_29_97,
            rate_policy: RatePolicy::HoldDrop,
            buffer_frames: 8,
            procamp: procamp::ProcAmpConfig::default(),
            jitter_correction: true,
            signal_loss: SignalLossPolicy::LastFrameHold,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_matches_format() {
        assert_eq!(PixelFormat::Yuv420p.frame_size(720, 480), 720 * 480 * 3 / 2);
        assert_eq!(PixelFormat::Yuv422p.frame_size(720, 480), 720 * 480 * 2);
        assert_eq!(PixelFormat::Rgb24.frame_size(720, 480), 720 * 480 * 3);
    }

    #[test]
    fn default_config_is_valid() {
        assert!(TbcConfig::default().validate().is_ok());
    }

    #[test]
    fn validate_rejects_bad_geometry() {
        let c = TbcConfig {
            width: 0,
            ..Default::default()
        };
        assert!(matches!(c.validate(), Err(TbcError::InvalidConfig(_))));
        let c = TbcConfig {
            format: PixelFormat::Yuv420p,
            width: 719,
            ..Default::default()
        };
        assert!(matches!(c.validate(), Err(TbcError::InvalidConfig(_))));
    }

    #[test]
    fn validate_rejects_bad_buffer_and_rate() {
        let c = TbcConfig {
            buffer_frames: 0,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = TbcConfig {
            buffer_frames: 241,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = TbcConfig {
            target_rate: FrameRate {
                num: 30_000,
                den: 0,
            },
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_finite_procamp() {
        let c = TbcConfig {
            procamp: procamp::ProcAmpConfig {
                luma_gain: f32::NAN,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = TbcConfig {
            procamp: procamp::ProcAmpConfig {
                gamma: f32::INFINITY,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn ntsc_frame_duration_is_1001_based() {
        // 29.97 fps -> 33.3667 ms per frame (integer division truncates).
        assert_eq!(FrameRate::NTSC_29_97.frame_duration_ns(), 33_366_666);
    }
}
