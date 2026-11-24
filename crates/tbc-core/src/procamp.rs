//! Proc-amp style level correction: luma/chroma gain, black level
//! (pedestal), saturation, hue and gamma. Design: SPEC.md §4.4.
//!
//! Luma runs through a 256-entry LUT that folds gain, pedestal and gamma
//! together; chroma is rotated/scaled in the CbCr plane per sample pair.
//! A fully neutral configuration is an exact no-op (byte-identical),
//! including for RGB, where a BT.601 round trip would not be.

use crate::{PixelFormat, VideoFrame};

/// All values are neutral at their defaults; ranges are defined in SPEC.md.
#[derive(Debug, Clone, Copy)]
pub struct ProcAmpConfig {
    /// Luma gain, 1.0 = unity.
    pub luma_gain: f32,
    /// Black level offset added to Y after gain, in 8-bit code values.
    pub pedestal: f32,
    /// Chroma gain (saturation), 1.0 = unity.
    pub saturation: f32,
    /// Hue rotation in degrees, 0.0 = none.
    pub hue_degrees: f32,
    /// Gamma applied to Y, 1.0 = none.
    pub gamma: f32,
    /// Horizontal chroma shift relative to luma, in luma pixels
    /// (positive moves chroma to the right); compensates the chroma
    /// delay of composite sources (Issue #30). 0.0 = none. Not
    /// applied to RGB frames.
    pub chroma_shift_px: f32,
    /// The input is full-range (0..=255) rather than limited-range
    /// (16..=235): the luma LUT then applies gain around 0 instead
    /// of around the limited-range pivot (Issue #32). Set by the
    /// auto-adapt range estimator, not a user control.
    pub full_range: bool,
}

impl Default for ProcAmpConfig {
    fn default() -> Self {
        ProcAmpConfig {
            luma_gain: 1.0,
            pedestal: 0.0,
            saturation: 1.0,
            hue_degrees: 0.0,
            gamma: 1.0,
            chroma_shift_px: 0.0,
            full_range: false,
        }
    }
}

impl ProcAmpConfig {
    /// True when every control sits at its neutral position.
    pub fn is_neutral(&self) -> bool {
        self.luma_gain == 1.0
            && self.pedestal == 0.0
            && self.saturation == 1.0
            && self.hue_degrees == 0.0
            && self.gamma == 1.0
            && self.chroma_shift_px == 0.0
            && !self.full_range
    }
}

/// Shift one chroma row by `shift_q8` (Q8 chroma samples; positive
/// moves the samples to the right), linearly interpolated with the
/// edges clamped. `scratch` must be at least as long as `row` and
/// receives a copy of the original row.
fn shift_chroma_row(row: &mut [u8], scratch: &mut [u8], shift_q8: i32) {
    if shift_q8 == 0 || row.len() < 2 {
        return;
    }
    scratch[..row.len()].copy_from_slice(row);
    let n = row.len() as i32;
    for (i, out) in row.iter_mut().enumerate() {
        // Source position of this output sample, in Q8 samples.
        let p = i as i32 * 256 - shift_q8;
        let j = p.div_euclid(256);
        let frac = p.rem_euclid(256) as u32;
        let a = scratch[j.clamp(0, n - 1) as usize] as u32;
        let b = scratch[(j + 1).clamp(0, n - 1) as usize] as u32;
        *out = ((a * (256 - frac) + b * frac + 128) >> 8) as u8;
    }
}

/// Stateful proc-amp: build once per configuration, apply per frame.
#[derive(Debug, Clone)]
pub struct ProcAmp {
    config: ProcAmpConfig,
    luma_lut: [u8; 256],
    chroma_cos: f32,
    chroma_sin: f32,
    /// Chroma shift in Q8 chroma samples (Issue #30). Chroma is at
    /// half the luma rate horizontally, so one luma pixel is 128.
    chroma_shift_q8: i32,
}

impl ProcAmp {
    pub fn new(config: ProcAmpConfig) -> Self {
        let mut luma_lut = [0u8; 256];
        for (i, out) in luma_lut.iter_mut().enumerate() {
            // Limited-range expansion around the 16..235 window, gain,
            // pedestal, then optional gamma. See SPEC.md §4.4. For
            // full-range input the gain pivots at 0 instead (Issue #32).
            let mut y = if config.full_range {
                i as f32 * config.luma_gain + config.pedestal
            } else {
                (i as f32 - 16.0) * config.luma_gain + 16.0 + config.pedestal
            };
            if config.gamma != 1.0 {
                let norm = (y.clamp(0.0, 255.0)) / 255.0;
                y = 255.0 * norm.powf(1.0 / config.gamma);
            }
            *out = y.round().clamp(0.0, 255.0) as u8;
        }
        let hue_rad = config.hue_degrees.to_radians();
        ProcAmp {
            config,
            luma_lut,
            chroma_cos: hue_rad.cos(),
            chroma_sin: hue_rad.sin(),
            chroma_shift_q8: (config.chroma_shift_px * 128.0)
                .round()
                .clamp(-2048.0, 2048.0) as i32,
        }
    }

    pub fn config(&self) -> &ProcAmpConfig {
        &self.config
    }

    fn rotate_chroma(&self, cb: u8, cr: u8) -> (u8, u8) {
        let sat = self.config.saturation;
        let cb0 = (cb as f32 - 128.0) * sat;
        let cr0 = (cr as f32 - 128.0) * sat;
        let cb1 = cb0 * self.chroma_cos - cr0 * self.chroma_sin + 128.0;
        let cr1 = cb0 * self.chroma_sin + cr0 * self.chroma_cos + 128.0;
        (
            cb1.round().clamp(0.0, 255.0) as u8,
            cr1.round().clamp(0.0, 255.0) as u8,
        )
    }

    /// Apply the correction in place.
    pub fn apply(&self, frame: &mut VideoFrame) {
        if self.config.is_neutral() {
            return;
        }
        match frame.format {
            PixelFormat::Yuv422p | PixelFormat::Yuyv => self.apply_yuv422p(frame),
            PixelFormat::Uyvy => {
                // Same samples in U-Y-V-Y order: reorder into the 422
                // layout, apply, and reorder back (Issue #23).
                crate::swap_422_quads(&mut frame.data);
                self.apply_yuv422p(frame);
                crate::swap_422_quads(&mut frame.data);
            }
            PixelFormat::Yuv420p => self.apply_yuv420p(frame),
            PixelFormat::Rgb24 => self.apply_rgb24(frame),
            PixelFormat::Yuv420p10 | PixelFormat::Yuv422p10 => {
                // Process in the 8-bit domain and convert back
                // (Issue #27); the CLI pipeline converts at its
                // boundary, so this path serves direct API use.
                if let Ok(mut f8) = frame.to_8bit() {
                    self.apply(&mut f8);
                    if let Ok(back) = VideoFrame::from_8bit(&f8, frame.format) {
                        frame.data = back.data;
                    }
                }
            }
            PixelFormat::Nv12 => {
                // Process in the planar 4:2:0 domain and convert
                // back (Issue #33); same boundary pattern as 10-bit.
                if let Ok(mut fp) = frame.to_yuv420p() {
                    self.apply(&mut fp);
                    if let Ok(back) = VideoFrame::from_yuv420p(&fp, frame.format) {
                        frame.data = back.data;
                    }
                }
            }
        }
    }

    /// Shift the interleaved Cb/Cr rows of a packed 4:2:2 frame
    /// (Issue #30). Two small buffers are allocated per call, only
    /// when a shift is configured.
    fn shift_packed_chroma(&self, frame: &mut VideoFrame) {
        let w = frame.width as usize;
        let h = frame.height as usize;
        let cw = w / 2;
        let mut row = vec![0u8; cw];
        let mut scratch = vec![0u8; cw];
        for line in 0..h {
            let base = line * w * 2;
            // Cb sits at quad offset 1, Cr at offset 3.
            for ch in 0..2 {
                let off = base + 1 + ch * 2;
                for i in 0..cw {
                    row[i] = frame.data[off + i * 4];
                }
                shift_chroma_row(&mut row, &mut scratch, self.chroma_shift_q8);
                for i in 0..cw {
                    frame.data[off + i * 4] = row[i];
                }
            }
        }
    }

    fn apply_yuv422p(&self, frame: &mut VideoFrame) {
        if self.chroma_shift_q8 != 0 {
            self.shift_packed_chroma(frame);
        }
        // Packed quads: Y0 Cb Y1 Cr.
        for quad in frame.data.chunks_exact_mut(4) {
            quad[0] = self.luma_lut[quad[0] as usize];
            quad[2] = self.luma_lut[quad[2] as usize];
            let (cb, cr) = self.rotate_chroma(quad[1], quad[3]);
            quad[1] = cb;
            quad[3] = cr;
        }
    }

    fn apply_yuv420p(&self, frame: &mut VideoFrame) {
        let y_size = (frame.width * frame.height) as usize;
        let (y_plane, chroma) = frame.data.split_at_mut(y_size);
        for px in y_plane.iter_mut() {
            *px = self.luma_lut[*px as usize];
        }
        // Cb and Cr are separate planes of equal size; shift every
        // chroma row first when a chroma shift is configured.
        let c_size = chroma.len() / 2;
        if self.chroma_shift_q8 != 0 {
            let cw = frame.width as usize / 2;
            let mut scratch = vec![0u8; cw];
            for row in chroma.chunks_exact_mut(cw) {
                shift_chroma_row(row, &mut scratch, self.chroma_shift_q8);
            }
        }
        let (cb_plane, cr_plane) = chroma.split_at_mut(c_size);
        for i in 0..c_size {
            let (cb, cr) = self.rotate_chroma(cb_plane[i], cr_plane[i]);
            cb_plane[i] = cb;
            cr_plane[i] = cr;
        }
    }

    fn apply_rgb24(&self, frame: &mut VideoFrame) {
        for px in frame.data.chunks_exact_mut(3) {
            let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
            // BT.601 limited range, forward.
            let y = 0.299 * r + 0.587 * g + 0.114 * b;
            let cb = 128.0 + (b - y) * 0.564;
            let cr = 128.0 + (r - y) * 0.713;
            let y2 = self.luma_lut[y.round().clamp(0.0, 255.0) as usize] as f32;
            let (cb2, cr2) = self.rotate_chroma(
                cb.round().clamp(0.0, 255.0) as u8,
                cr.round().clamp(0.0, 255.0) as u8,
            );
            let (cb2, cr2) = (cb2 as f32, cr2 as f32);
            // BT.601 inverse from the corrected components.
            let r2 = y2 + (cr2 - 128.0) / 0.713;
            let b2 = y2 + (cb2 - 128.0) / 0.564;
            let g2 = (y2 - 0.299 * r2 - 0.114 * b2) / 0.587;
            px[0] = r2.round().clamp(0.0, 255.0) as u8;
            px[1] = g2.round().clamp(0.0, 255.0) as u8;
            px[2] = b2.round().clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldOrder;

    fn frame(format: PixelFormat, data: Vec<u8>, w: u32, h: u32) -> VideoFrame {
        VideoFrame {
            width: w,
            height: h,
            format,
            field_order: FieldOrder::Progressive,
            pts_ns: 0,
            sequence: 0,
            data,
        }
    }

    #[test]
    fn neutral_is_byte_identical_for_all_formats() {
        let amp = ProcAmp::new(ProcAmpConfig::default());
        for (format, w, h) in [
            (PixelFormat::Yuv422p, 4, 2),
            (PixelFormat::Yuv420p, 4, 2),
            (PixelFormat::Rgb24, 4, 2),
        ] {
            let data: Vec<u8> = (0..format.frame_size(w, h))
                .map(|i| (i * 37) as u8)
                .collect();
            let mut f = frame(format, data.clone(), w, h);
            amp.apply(&mut f);
            assert_eq!(
                f.data, data,
                "format {format:?} changed under neutral config"
            );
        }
    }

    #[test]
    fn pedestal_raises_black_level() {
        let amp = ProcAmp::new(ProcAmpConfig {
            pedestal: 16.0,
            ..Default::default()
        });
        // One Yuv422p quad: Y=16 (black), neutral chroma.
        let mut f = frame(PixelFormat::Yuv422p, vec![16, 128, 16, 128], 2, 1);
        amp.apply(&mut f);
        assert_eq!(f.data[0], 32);
        assert_eq!(f.data[2], 32);
    }

    #[test]
    fn zero_saturation_makes_chroma_neutral() {
        let amp = ProcAmp::new(ProcAmpConfig {
            saturation: 0.0,
            ..Default::default()
        });
        let mut f = frame(PixelFormat::Yuv422p, vec![100, 200, 100, 60], 2, 1);
        amp.apply(&mut f);
        assert_eq!(f.data[1], 128);
        assert_eq!(f.data[3], 128);
        // Luma untouched by saturation.
        assert_eq!(f.data[0], 100);
    }

    #[test]
    fn high_gain_clamps_to_255() {
        let amp = ProcAmp::new(ProcAmpConfig {
            luma_gain: 4.0,
            ..Default::default()
        });
        let mut f = frame(PixelFormat::Yuv422p, vec![200, 128, 200, 128], 2, 1);
        amp.apply(&mut f);
        assert_eq!(f.data[0], 255);
    }

    /// An 8x1 yuv422p frame: Y all 100, Cb = [10, 20, 30, 40],
    /// Cr all 200.
    fn chroma_ramp_422() -> VideoFrame {
        let mut data = Vec::new();
        for cb in [10u8, 20, 30, 40] {
            data.extend_from_slice(&[100, cb, 100, 200]);
        }
        frame(PixelFormat::Yuv422p, data, 8, 1)
    }

    fn cb_row(f: &VideoFrame) -> Vec<u8> {
        (0..4).map(|i| f.data[i * 4 + 1]).collect()
    }

    #[test]
    fn chroma_shift_moves_422_chroma_by_whole_samples() {
        // +2.0 luma px = +1 chroma sample, edge clamped (Issue #30).
        let amp = ProcAmp::new(ProcAmpConfig {
            chroma_shift_px: 2.0,
            ..Default::default()
        });
        let mut f = chroma_ramp_422();
        amp.apply(&mut f);
        assert_eq!(cb_row(&f), vec![10, 10, 20, 30]);
        // Luma and the constant Cr are untouched.
        assert!((0..4)
            .all(|i| f.data[i * 4] == 100 && f.data[i * 4 + 2] == 100 && f.data[i * 4 + 3] == 200));

        // -2.0 luma px moves the other way.
        let amp = ProcAmp::new(ProcAmpConfig {
            chroma_shift_px: -2.0,
            ..Default::default()
        });
        let mut f = chroma_ramp_422();
        amp.apply(&mut f);
        assert_eq!(cb_row(&f), vec![20, 30, 40, 40]);
    }

    #[test]
    fn chroma_shift_half_sample_interpolates() {
        // +1.0 luma px = +0.5 chroma sample: each output is the
        // average of its two neighbours.
        let amp = ProcAmp::new(ProcAmpConfig {
            chroma_shift_px: 1.0,
            ..Default::default()
        });
        let mut f = chroma_ramp_422();
        amp.apply(&mut f);
        assert_eq!(cb_row(&f), vec![10, 15, 25, 35]);
    }

    #[test]
    fn chroma_shift_applies_to_420_planes() {
        let amp = ProcAmp::new(ProcAmpConfig {
            chroma_shift_px: 2.0,
            ..Default::default()
        });
        // 4x2: Y = 100 x8, U = [10, 20], V = [30, 40].
        let mut data = vec![100u8; 8];
        data.extend_from_slice(&[10, 20]);
        data.extend_from_slice(&[30, 40]);
        let mut f = frame(PixelFormat::Yuv420p, data, 4, 2);
        amp.apply(&mut f);
        assert!(f.data[..8].iter().all(|&y| y == 100));
        assert_eq!(&f.data[8..10], [10, 10]);
        assert_eq!(&f.data[10..12], [30, 30]);
    }

    #[test]
    fn full_range_lut_pivots_at_zero() {
        // Gain 1.2 on Y=100: limited pivots at 16 -> 117,
        // full-range pivots at 0 -> 120 (Issue #32).
        let limited = ProcAmp::new(ProcAmpConfig {
            luma_gain: 1.2,
            ..Default::default()
        });
        let mut f = frame(PixelFormat::Yuv422p, vec![100, 128, 100, 128], 2, 1);
        limited.apply(&mut f);
        assert_eq!(f.data[0], 117);

        let full = ProcAmp::new(ProcAmpConfig {
            luma_gain: 1.2,
            full_range: true,
            ..Default::default()
        });
        let mut f = frame(PixelFormat::Yuv422p, vec![100, 128, 100, 128], 2, 1);
        full.apply(&mut f);
        assert_eq!(f.data[0], 120);
    }

    #[test]
    fn full_range_only_config_is_not_neutral() {
        assert!(!ProcAmpConfig {
            full_range: true,
            ..Default::default()
        }
        .is_neutral());
    }

    #[test]
    fn shift_only_config_is_not_neutral() {
        assert!(!ProcAmpConfig {
            chroma_shift_px: 1.0,
            ..Default::default()
        }
        .is_neutral());
    }

    #[test]
    fn yuv420p_planes_are_handled() {
        let amp = ProcAmp::new(ProcAmpConfig {
            pedestal: 10.0,
            ..Default::default()
        });
        // 2x2: Y plane 16 x4, Cb plane [128], Cr plane [128].
        let mut f = frame(PixelFormat::Yuv420p, vec![16, 16, 16, 16, 128, 128], 2, 2);
        amp.apply(&mut f);
        assert!(f.data[..4].iter().all(|&y| y == 26));
    }
}
