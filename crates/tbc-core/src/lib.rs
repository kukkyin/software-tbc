//! Core signal-processing pipeline of the software TBC.
//!
//! This crate is deliberately free of I/O: it transforms [`VideoFrame`]s
//! according to a [`TbcConfig`] and reports [`Telemetry`] snapshots.
//! See `SPEC.md` at the repository root for the full design.

pub mod adapt;
pub mod field;
pub mod geometry;
pub mod guard;
pub mod head_switch;
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
    /// Planar YUV 4:2:0, 10-bit samples in u16 little-endian (values
    /// in the low 10 bits; FFmpeg `yuv420p10le`). Boundary format:
    /// converted to `Yuv420p` for processing (Issue #27).
    Yuv420p10,
    /// Planar YUV 4:2:2, 10-bit samples in u16 little-endian (values
    /// in the low 10 bits; FFmpeg `yuv422p10le`). Boundary format:
    /// converted to `Yuv422p` for processing (Issue #27). Unlike the
    /// pipeline's packed `Yuv422p`, this layout really is planar.
    Yuv422p10,
}

impl PixelFormat {
    /// Bytes required for one frame of `width` x `height` in this format.
    pub fn frame_size(self, width: u32, height: u32) -> usize {
        let px = (width * height) as usize;
        match self {
            PixelFormat::Yuv420p => px * 3 / 2,
            PixelFormat::Yuv422p | PixelFormat::Uyvy | PixelFormat::Yuyv => px * 2,
            PixelFormat::Rgb24 => px * 3,
            PixelFormat::Yuv420p10 => px * 3,
            PixelFormat::Yuv422p10 => px * 4,
        }
    }

    /// Whether this format packs two pixels into one 4-byte group and
    /// therefore requires an even width.
    pub fn is_packed_422(self) -> bool {
        matches!(self, PixelFormat::Uyvy | PixelFormat::Yuyv)
    }

    /// Whether this format carries 10-bit samples (u16 LE, values in
    /// the low 10 bits) and is converted to 8-bit for processing.
    pub fn is_10bit(self) -> bool {
        matches!(self, PixelFormat::Yuv420p10 | PixelFormat::Yuv422p10)
    }
}

/// Scale a 10-bit sample down to 8 bits, rounding to nearest
/// (1023 maps to 255 exactly). Out-of-range inputs are clamped.
pub(crate) fn scale_10_to_8(v: u16) -> u8 {
    ((v.min(1023) as u32 * 255 + 511) / 1023) as u8
}

/// Scale an 8-bit sample up to 10 bits (255 maps to 1023 exactly;
/// limited-range values scale proportionally, e.g. 16 -> 64).
pub(crate) fn scale_8_to_10(v: u8) -> u16 {
    ((v as u32 * 1023 + 127) / 255) as u16
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

impl VideoFrame {
    /// Convert a 10-bit frame into its 8-bit pipeline equivalent
    /// (Issue #27): `Yuv420p10` -> `Yuv420p` (planar to planar) and
    /// `Yuv422p10` -> `Yuv422p` (planar to the pipeline's packed
    /// layout), scaling every sample with [`scale_10_to_8`]. An
    /// 8-bit frame converts to a copy of itself.
    pub fn to_8bit(&self) -> Result<VideoFrame, TbcError> {
        self.validate()?;
        let (w, h) = (self.width as usize, self.height as usize);
        match self.format {
            PixelFormat::Yuv420p10 => {
                let mut out = self.clone();
                out.format = PixelFormat::Yuv420p;
                out.data = self
                    .data
                    .chunks_exact(2)
                    .map(|c| scale_10_to_8(u16::from_le_bytes([c[0], c[1]])))
                    .collect();
                Ok(out)
            }
            PixelFormat::Yuv422p10 => {
                let px = w * h;
                let y_plane = &self.data[..px * 2];
                let u_plane = &self.data[px * 2..px * 3];
                let v_plane = &self.data[px * 3..];
                let sample = |plane: &[u8], i: usize| {
                    scale_10_to_8(u16::from_le_bytes([plane[i * 2], plane[i * 2 + 1]]))
                };
                let mut data = Vec::with_capacity(px * 2);
                for line in 0..h {
                    for pair in 0..w / 2 {
                        let y0 = sample(y_plane, line * w + pair * 2);
                        let y1 = sample(y_plane, line * w + pair * 2 + 1);
                        let u = sample(u_plane, line * (w / 2) + pair);
                        let v = sample(v_plane, line * (w / 2) + pair);
                        data.extend_from_slice(&[y0, u, y1, v]);
                    }
                }
                let mut out = self.clone();
                out.format = PixelFormat::Yuv422p;
                out.data = data;
                Ok(out)
            }
            _ => Ok(self.clone()),
        }
    }

    /// Inverse of [`VideoFrame::to_8bit`]: expand an 8-bit frame
    /// into the 10-bit boundary format `format`, scaling every
    /// sample with [`scale_8_to_10`]. The source must be the
    /// matching 8-bit format (`Yuv420p` for `Yuv420p10`, `Yuv422p`
    /// for `Yuv422p10`); a non-10-bit target returns a copy when it
    /// equals the source format and an error otherwise.
    pub fn from_8bit(frame: &VideoFrame, format: PixelFormat) -> Result<VideoFrame, TbcError> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let push =
            |data: &mut Vec<u8>, v: u8| data.extend_from_slice(&scale_8_to_10(v).to_le_bytes());
        match format {
            PixelFormat::Yuv420p10 => {
                if frame.format != PixelFormat::Yuv420p {
                    return Err(TbcError::InvalidConfig(format!(
                        "from_8bit to yuv420p10 needs a yuv420p source, got {:?}",
                        frame.format
                    )));
                }
                frame.validate()?;
                let mut data = Vec::with_capacity(frame.data.len() * 2);
                for &b in &frame.data {
                    push(&mut data, b);
                }
                let mut out = frame.clone();
                out.format = format;
                out.data = data;
                Ok(out)
            }
            PixelFormat::Yuv422p10 => {
                if frame.format != PixelFormat::Yuv422p {
                    return Err(TbcError::InvalidConfig(format!(
                        "from_8bit to yuv422p10 needs a yuv422p source, got {:?}",
                        frame.format
                    )));
                }
                frame.validate()?;
                let px = w * h;
                let mut data = vec![0u8; px * 4];
                let put = |data: &mut [u8], plane_off: usize, i: usize, v: u8| {
                    data[plane_off + i * 2..plane_off + i * 2 + 2]
                        .copy_from_slice(&scale_8_to_10(v).to_le_bytes());
                };
                for line in 0..h {
                    for pair in 0..w / 2 {
                        let quad = line * w * 2 + pair * 4;
                        let (y0, u, y1, v) = (
                            frame.data[quad],
                            frame.data[quad + 1],
                            frame.data[quad + 2],
                            frame.data[quad + 3],
                        );
                        put(&mut data, 0, line * w + pair * 2, y0);
                        put(&mut data, 0, line * w + pair * 2 + 1, y1);
                        put(&mut data, px * 2, line * (w / 2) + pair, u);
                        put(&mut data, px * 3, line * (w / 2) + pair, v);
                    }
                }
                let mut out = frame.clone();
                out.format = format;
                out.data = data;
                Ok(out)
            }
            other if other == frame.format => Ok(frame.clone()),
            other => Err(TbcError::InvalidConfig(format!(
                "cannot convert {:?} to {other:?} with from_8bit",
                frame.format
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
    /// Head-switching noise concealment (Issue #29); disabled by
    /// default (`lines: 0`).
    pub head_switch: head_switch::HeadSwitchConfig,
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
        if matches!(self.format, PixelFormat::Yuv420p | PixelFormat::Yuv420p10)
            && (self.width % 2 == 1 || self.height % 2 == 1)
        {
            return Err(TbcError::InvalidConfig(
                "yuv420 formats require even width and height".into(),
            ));
        }
        if (self.format.is_packed_422() || self.format == PixelFormat::Yuv422p10)
            && self.width % 2 == 1
        {
            return Err(TbcError::InvalidConfig(
                "this format requires even width".into(),
            ));
        }
        if self.head_switch.lines > 0 && self.head_switch.lines >= self.height {
            return Err(TbcError::InvalidConfig(
                "head_switch lines must be smaller than the frame height".into(),
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
            head_switch: head_switch::HeadSwitchConfig::default(),
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
    fn validate_rejects_head_switch_covering_the_frame() {
        // Boundary cases for Issue #29: masking may leave exactly
        // one clean line, but not zero.
        let mut c = TbcConfig {
            height: 10,
            ..Default::default()
        };
        c.head_switch.lines = 9;
        assert!(c.validate().is_ok());
        c.head_switch.lines = 10;
        assert!(c.validate().is_err());
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

    fn data10(samples: &[u16]) -> Vec<u8> {
        samples.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn ten_bit_frame_size_and_validation() {
        assert_eq!(PixelFormat::Yuv420p10.frame_size(720, 480), 720 * 480 * 3);
        assert_eq!(PixelFormat::Yuv422p10.frame_size(720, 480), 720 * 480 * 4);
        assert!(PixelFormat::Yuv420p10.is_10bit());
        assert!(PixelFormat::Yuv422p10.is_10bit());
        assert!(!PixelFormat::Yuv422p.is_10bit());
        // The 10-bit formats keep the 8-bit geometry constraints.
        let c = TbcConfig {
            format: PixelFormat::Yuv420p10,
            height: 479,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = TbcConfig {
            format: PixelFormat::Yuv422p10,
            width: 719,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = TbcConfig {
            format: PixelFormat::Yuv422p10,
            width: 720,
            ..Default::default()
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn ten_bit_scaling_endpoints() {
        assert_eq!(scale_10_to_8(0), 0);
        assert_eq!(scale_10_to_8(1023), 255);
        assert_eq!(scale_10_to_8(512), 128);
        assert_eq!(scale_10_to_8(64), 16);
        assert_eq!(scale_10_to_8(2000), 255); // clamped
        assert_eq!(scale_8_to_10(0), 0);
        assert_eq!(scale_8_to_10(255), 1023);
        assert_eq!(scale_8_to_10(16), 64);
        assert_eq!(scale_8_to_10(128), 514);
    }

    #[test]
    fn yuv422p10_converts_to_packed_8bit() {
        // 4x1: Y = [0, 256, 512, 1023], U = [940, 100], V = [64, 200].
        let mut samples = vec![0u16, 256, 512, 1023];
        samples.extend_from_slice(&[940, 100]);
        samples.extend_from_slice(&[64, 200]);
        let frame = frame_with(PixelFormat::Yuv422p10, data10(&samples), 4, 1);
        let out = frame.to_8bit().expect("convert");
        assert_eq!(out.format, PixelFormat::Yuv422p);
        // Packed quads [Y0, U, Y1, V] with scaled samples.
        assert_eq!(out.data, vec![0, 234, 64, 16, 128, 25, 255, 50]);
        // And back: planar layout restored, values within a few LSBs.
        let back = VideoFrame::from_8bit(&out, PixelFormat::Yuv422p10).expect("expand");
        assert_eq!(back.format, PixelFormat::Yuv422p10);
        let back_samples: Vec<u16> = back
            .data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        for (a, b) in samples.iter().zip(back_samples.iter()) {
            assert!((*a as i32 - *b as i32).abs() <= 4, "{a} vs {b}");
        }
    }

    #[test]
    fn eight_bit_round_trip_through_10bit_is_exact() {
        let data: Vec<u8> = (0..PixelFormat::Yuv422p.frame_size(64, 64))
            .map(|i| (i * 37 % 256) as u8)
            .collect();
        for target in [PixelFormat::Yuv422p10, PixelFormat::Yuv420p10] {
            let source_format = if target == PixelFormat::Yuv422p10 {
                PixelFormat::Yuv422p
            } else {
                PixelFormat::Yuv420p
            };
            let d = if source_format == PixelFormat::Yuv420p {
                (0..PixelFormat::Yuv420p.frame_size(64, 64))
                    .map(|i| (i * 41 % 256) as u8)
                    .collect::<Vec<u8>>()
            } else {
                data.clone()
            };
            let frame = frame_with(source_format, d.clone(), 64, 64);
            let up = VideoFrame::from_8bit(&frame, target).expect("expand");
            let back = up.to_8bit().expect("reduce");
            assert_eq!(back.format, source_format);
            assert_eq!(back.data, d, "round trip via {target:?}");
        }
    }

    #[test]
    fn ten_bit_conversion_rejects_wrong_sources() {
        let yuv422 = frame_with(PixelFormat::Yuv422p, vec![0; 4 * 4 * 2], 4, 4);
        assert!(VideoFrame::from_8bit(&yuv422, PixelFormat::Yuv420p10).is_err());
        let yuv420 = frame_with(
            PixelFormat::Yuv420p,
            vec![0; PixelFormat::Yuv420p.frame_size(4, 4)],
            4,
            4,
        );
        assert!(VideoFrame::from_8bit(&yuv420, PixelFormat::Yuv422p10).is_err());
        // An 8-bit frame converts to itself.
        let same = yuv422.to_8bit().expect("identity");
        assert_eq!(same.data, yuv422.data);
    }
}
