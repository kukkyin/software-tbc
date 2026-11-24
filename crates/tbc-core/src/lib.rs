//! Core signal-processing pipeline of the software TBC.
//!
//! This crate is deliberately free of I/O: it transforms [`VideoFrame`]s
//! according to a [`TbcConfig`] and reports [`Telemetry`] snapshots.
//! See `SPEC.md` at the repository root for the full design.

pub mod field;
pub mod guard;
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
    /// YUV 4:2:2, 8-bit, in the pipeline's packed byte order
    /// (Y0 Cb Y1 Cr quads; see Issue #23's note in SPEC.md §3).
    Yuv422p,
    /// Packed RGB, 8-bit per channel (FFmpeg `rgb24`).
    Rgb24,
    /// Packed YUV 4:2:2, 8-bit, U-Y-V-Y byte order (FFmpeg `uyvy422`).
    /// Carried at the I/O boundary and converted to `Yuv422p` for
    /// processing (Issue #23).
    Uyvy,
    /// Packed YUV 4:2:2, 8-bit, Y-U-Y-V byte order (FFmpeg `yuyv422`).
    /// Same boundary role as `Uyvy`.
    Yuyv,
}

impl PixelFormat {
    /// Bytes required for one frame of `width` x `height` in this format.
    pub fn frame_size(self, width: u32, height: u32) -> usize {
        let px = (width * height) as usize;
        match self {
            PixelFormat::Yuv420p => px * 3 / 2,
            PixelFormat::Yuv422p | PixelFormat::Uyvy | PixelFormat::Yuyv => px * 2,
            PixelFormat::Rgb24 => px * 3,
        }
    }

    /// Whether this format packs two pixels into one 4-byte group and
    /// therefore requires an even width.
    pub fn is_packed_422(self) -> bool {
        matches!(self, PixelFormat::Uyvy | PixelFormat::Yuyv)
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

    /// Convert a packed 4:2:2 frame (`Uyvy` / `Yuyv`) into the
    /// pipeline's `Yuv422p` representation (Issue #23). Both packed
    /// formats share 4:2:2 sampling with `Yuv422p`, so this is a pure
    /// byte reorder: `Yuyv` already uses the same byte order and only
    /// changes its tag, while `Uyvy` swaps the bytes of each 4-byte
    /// group. A `Yuv422p` frame converts to a copy of itself; any
    /// other format is an error. Exactly reversible by
    /// [`VideoFrame::from_yuv422p`].
    pub fn to_yuv422p(&self) -> Result<VideoFrame, TbcError> {
        self.validate()?;
        match self.format {
            PixelFormat::Yuv422p => Ok(self.clone()),
            PixelFormat::Yuyv => {
                let mut frame = self.clone();
                frame.format = PixelFormat::Yuv422p;
                Ok(frame)
            }
            PixelFormat::Uyvy => {
                let mut frame = self.clone();
                swap_422_quads(&mut frame.data);
                frame.format = PixelFormat::Yuv422p;
                Ok(frame)
            }
            other => Err(TbcError::InvalidConfig(format!(
                "cannot convert {other:?} to yuv422p"
            ))),
        }
    }

    /// Inverse of [`VideoFrame::to_yuv422p`]: re-pack a `Yuv422p`
    /// frame into the boundary format `format` (`Uyvy` or `Yuyv`;
    /// `Yuv422p` returns a copy). Any other combination is an error.
    pub fn from_yuv422p(frame: &VideoFrame, format: PixelFormat) -> Result<VideoFrame, TbcError> {
        if frame.format != PixelFormat::Yuv422p {
            return Err(TbcError::InvalidConfig(format!(
                "from_yuv422p needs a yuv422p source, got {:?}",
                frame.format
            )));
        }
        frame.validate()?;
        match format {
            PixelFormat::Yuv422p => Ok(frame.clone()),
            PixelFormat::Yuyv => {
                let mut out = frame.clone();
                out.format = PixelFormat::Yuyv;
                Ok(out)
            }
            PixelFormat::Uyvy => {
                let mut out = frame.clone();
                swap_422_quads(&mut out.data);
                out.format = PixelFormat::Uyvy;
                Ok(out)
            }
            other => Err(TbcError::InvalidConfig(format!(
                "cannot convert yuv422p to {other:?}"
            ))),
        }
    }
}

/// Reorder each 4-byte group of packed 4:2:2 data between the
/// U-Y-V-Y and Y-U-Y-V orders (the operation is its own inverse):
/// `[a, b, c, d]` becomes `[b, a, d, c]`.
pub(crate) fn swap_422_quads(data: &mut [u8]) {
    for quad in data.chunks_exact_mut(4) {
        quad.swap(0, 1);
        quad.swap(2, 3);
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
        if self.format.is_packed_422() && self.width % 2 == 1 {
            return Err(TbcError::InvalidConfig(
                "uyvy/yuyv require even width".into(),
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

    fn frame_with(format: PixelFormat, data: Vec<u8>, width: u32, height: u32) -> VideoFrame {
        VideoFrame {
            width,
            height,
            format,
            field_order: FieldOrder::Progressive,
            pts_ns: 0,
            sequence: 0,
            data,
        }
    }

    #[test]
    fn packed_422_frame_size_and_validation() {
        assert_eq!(PixelFormat::Uyvy.frame_size(720, 480), 720 * 480 * 2);
        assert_eq!(PixelFormat::Yuyv.frame_size(720, 480), 720 * 480 * 2);
        // Odd widths are invalid for the packed formats only.
        for format in [PixelFormat::Uyvy, PixelFormat::Yuyv] {
            let c = TbcConfig {
                format,
                width: 719,
                ..Default::default()
            };
            assert!(matches!(c.validate(), Err(TbcError::InvalidConfig(_))));
            let c = TbcConfig {
                format,
                width: 720,
                ..Default::default()
            };
            assert!(c.validate().is_ok());
        }
    }

    #[test]
    fn uyvy_conversion_reorders_quads() {
        // Two pixel pairs: (U=10, Y0=20, V=30, Y1=40), (U=50, Y0=60, V=70, Y1=80).
        let uyvy = frame_with(
            PixelFormat::Uyvy,
            vec![10, 20, 30, 40, 50, 60, 70, 80],
            4,
            1,
        );
        let converted = uyvy.to_yuv422p().expect("convert");
        assert_eq!(converted.format, PixelFormat::Yuv422p);
        assert_eq!(converted.data, vec![20, 10, 40, 30, 60, 50, 80, 70]);
        // And back.
        let back = VideoFrame::from_yuv422p(&converted, PixelFormat::Uyvy).expect("re-pack");
        assert_eq!(back.format, PixelFormat::Uyvy);
        assert_eq!(back.data, uyvy.data);
    }

    #[test]
    fn yuyv_conversion_is_a_retag() {
        let yuyv = frame_with(
            PixelFormat::Yuyv,
            vec![20, 10, 40, 30, 60, 50, 80, 70],
            4,
            1,
        );
        let converted = yuyv.to_yuv422p().expect("convert");
        assert_eq!(converted.format, PixelFormat::Yuv422p);
        assert_eq!(converted.data, yuyv.data);
        let back = VideoFrame::from_yuv422p(&converted, PixelFormat::Yuyv).expect("re-pack");
        assert_eq!(back.format, PixelFormat::Yuyv);
        assert_eq!(back.data, yuyv.data);
    }

    #[test]
    fn packed_round_trip_is_identity_on_a_full_frame() {
        // Deterministic byte pattern over a whole 64x64 frame.
        let data: Vec<u8> = (0..PixelFormat::Uyvy.frame_size(64, 64))
            .map(|i| (i * 37 % 251) as u8)
            .collect();
        for format in [PixelFormat::Uyvy, PixelFormat::Yuyv] {
            let frame = frame_with(format, data.clone(), 64, 64);
            let converted = frame.to_yuv422p().expect("convert");
            let back = VideoFrame::from_yuv422p(&converted, format).expect("re-pack");
            assert_eq!(back.data, data, "round trip for {format:?}");
        }
    }

    #[test]
    fn conversion_rejects_other_formats() {
        let rgb = frame_with(PixelFormat::Rgb24, vec![0; 4 * 4 * 3], 4, 4);
        assert!(rgb.to_yuv422p().is_err());
        let yuv420 = frame_with(
            PixelFormat::Yuv420p,
            vec![0; PixelFormat::Yuv420p.frame_size(4, 4)],
            4,
            4,
        );
        assert!(VideoFrame::from_yuv422p(&yuv420, PixelFormat::Uyvy).is_err());
        let yuv422 = frame_with(PixelFormat::Yuv422p, vec![0; 4 * 4 * 2], 4, 4);
        assert!(VideoFrame::from_yuv422p(&yuv422, PixelFormat::Rgb24).is_err());
        // A yuv422p frame converts to itself.
        let same = yuv422.to_yuv422p().expect("identity");
        assert_eq!(same.data, yuv422.data);
    }

    #[test]
    fn conversion_rejects_size_mismatch() {
        // Boundary case: a packed frame whose buffer is too short.
        let short = frame_with(PixelFormat::Uyvy, vec![0; 6], 4, 1);
        assert!(matches!(
            short.to_yuv422p(),
            Err(TbcError::FrameSizeMismatch { .. })
        ));
    }
}
