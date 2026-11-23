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
}

impl Default for ProcAmpConfig {
    fn default() -> Self {
        ProcAmpConfig {
            luma_gain: 1.0,
            pedestal: 0.0,
            saturation: 1.0,
            hue_degrees: 0.0,
            gamma: 1.0,
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
    }
}

/// Stateful proc-amp: build once per configuration, apply per frame.
#[derive(Debug, Clone)]
pub struct ProcAmp {
    config: ProcAmpConfig,
    luma_lut: [u8; 256],
    chroma_cos: f32,
    chroma_sin: f32,
}

impl ProcAmp {
    pub fn new(config: ProcAmpConfig) -> Self {
        let mut luma_lut = [0u8; 256];
        for (i, out) in luma_lut.iter_mut().enumerate() {
            // Limited-range expansion around the 16..235 window, gain,
            // pedestal, then optional gamma. See SPEC.md §4.4.
            let mut y = (i as f32 - 16.0) * config.luma_gain + 16.0 + config.pedestal;
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
            PixelFormat::Yuv422p => self.apply_yuv422p(frame),
            PixelFormat::Yuv420p => self.apply_yuv420p(frame),
            PixelFormat::Rgb24 => self.apply_rgb24(frame),
        }
    }

    fn apply_yuv422p(&self, frame: &mut VideoFrame) {
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
        // Cb and Cr are separate planes of equal size.
        let c_size = chroma.len() / 2;
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
