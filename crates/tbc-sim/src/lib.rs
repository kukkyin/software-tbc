//! Synthetic unstable-video generator and measurement helpers.
//!
//! Produces clean test frames, then degrades them with the artefacts a
//! real TBC exists to fix: horizontal line jitter, clock drift, dropped
//! frames, dropout noise and pedestal shift. See SPEC.md §8.
//!
//! Randomness comes from a small seeded xorshift implementation so runs
//! are perfectly reproducible without external crates.

use tbc_core::guard::test_pattern;
use tbc_core::{FieldOrder, FrameRate, PixelFormat, VideoFrame};

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

/// xorshift64* pseudo-random generator with a fixed-seed contract.
#[derive(Debug, Clone)]
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    pub fn new(seed: u64) -> Self {
        // Zero is a fixed point of xorshift; remap it.
        XorShift64 {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform value in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform value in [min, max].
    pub fn next_i32(&mut self, min: i32, max: i32) -> i32 {
        if max <= min {
            return min;
        }
        let span = (max - min + 1) as u64;
        min + (self.next_u64() % span) as i32
    }
}

/// Generate one clean synthetic frame (colour bars) with the given geometry.
pub fn colour_bars(width: u32, height: u32, format: PixelFormat, sequence: u64) -> VideoFrame {
    let mut frame = test_pattern(width, height, format);
    frame.sequence = sequence;
    frame
}

/// Shift one row of samples by `shift_q8` (positive moves content
/// right), linear interpolation, edge replication.
fn shift_row(row: &[u8], shift_q8: i32, out: &mut [u8]) {
    let n = row.len() as i32;
    if n == 0 {
        return;
    }
    for (x, o) in out.iter_mut().enumerate() {
        let src_q8 = x as i32 * 256 - shift_q8;
        let i0 = src_q8.div_euclid(256);
        let frac = src_q8.rem_euclid(256);
        let a = row[i0.clamp(0, n - 1) as usize] as i32;
        let b = row[(i0 + 1).clamp(0, n - 1) as usize] as i32;
        *o = ((a * (256 - frac) + b * frac + 128) / 256) as u8;
    }
}

/// A synthetic source of degraded frames with reproducible artefacts.
#[derive(Debug)]
pub struct SyntheticSource {
    width: u32,
    height: u32,
    format: PixelFormat,
    rate: FrameRate,
    params: DegradeParams,
    rng: XorShift64,
    /// Source-clock index of the next frame (dropped frames count too).
    next_sequence: u64,
    /// Per-line random-walk phase, in 1/256 px.
    line_walk_q8: Vec<i32>,
}

impl SyntheticSource {
    pub fn new(
        width: u32,
        height: u32,
        format: PixelFormat,
        rate: FrameRate,
        params: DegradeParams,
        seed: u64,
    ) -> Self {
        SyntheticSource {
            width,
            height,
            format,
            rate,
            params,
            rng: XorShift64::new(seed),
            next_sequence: 0,
            line_walk_q8: vec![0; height as usize],
        }
    }

    /// Source frames generated so far, including dropped ones.
    pub fn produced_count(&self) -> u64 {
        self.next_sequence
    }

    fn nominal_pts(&self, sequence: u64) -> u64 {
        let base =
            (sequence as u128 * self.rate.den as u128 * 1_000_000_000) / self.rate.num as u128;
        let scale = 1.0 + self.params.clock_drift_ppm / 1_000_000.0;
        (base as f64 * scale) as u64
    }

    /// Produce the next surviving frame. Frames dropped by the drop
    /// probability are skipped internally (their timestamps vanish with
    /// them, exactly like a capture that never delivered them).
    pub fn next_frame(&mut self) -> VideoFrame {
        loop {
            let sequence = self.next_sequence;
            self.next_sequence += 1;
            if self.params.drop_probability > 0.0
                && self.rng.next_f64() < self.params.drop_probability
            {
                continue;
            }
            let mut frame = colour_bars(self.width, self.height, self.format, sequence);
            frame.pts_ns = self.nominal_pts(sequence);
            frame.field_order = FieldOrder::Progressive;
            self.apply_jitter(&mut frame);
            self.apply_dropout(&mut frame);
            self.apply_pedestal(&mut frame);
            return frame;
        }
    }

    fn apply_jitter(&mut self, frame: &mut VideoFrame) {
        let max_q8 = (self.params.line_jitter_px * 256.0) as i32;
        if max_q8 <= 0 {
            return;
        }
        if frame.format == PixelFormat::Uyvy {
            // Jitter in the pipeline's 422 byte order, then restore
            // the packed order (Issue #23).
            if let Ok(mut converted) = frame.to_yuv422p() {
                self.apply_jitter(&mut converted);
                frame.data.copy_from_slice(&converted.data);
            }
            return;
        }
        if frame.format.is_10bit() {
            // Jitter the 8-bit equivalent, then expand back
            // (Issue #27).
            if let Ok(mut converted) = frame.to_8bit() {
                self.apply_jitter(&mut converted);
                if let Ok(back) = VideoFrame::from_8bit(&converted, frame.format) {
                    frame.data.copy_from_slice(&back.data);
                }
            }
            return;
        }
        // Advance the random walk by at most 1/4 px per line per frame.
        for walk in self.line_walk_q8.iter_mut() {
            *walk = (*walk + self.rng.next_i32(-64, 64)).clamp(-max_q8, max_q8);
        }
        let shifts = self.line_walk_q8.clone();
        match frame.format {
            PixelFormat::Yuv422p | PixelFormat::Yuyv => {
                let w = frame.width as usize;
                for (line, &shift) in shifts.iter().enumerate() {
                    if shift == 0 {
                        continue;
                    }
                    let start = line * w * 2;
                    let line_data = frame.data[start..start + w * 2].to_vec();
                    let mut y_row = vec![0u8; w];
                    let mut cb_row = vec![0u8; w / 2];
                    let mut cr_row = vec![0u8; w / 2];
                    for x in 0..w {
                        y_row[x] = line_data[x * 2];
                    }
                    for i in 0..w / 2 {
                        cb_row[i] = line_data[i * 4 + 1];
                        cr_row[i] = line_data[i * 4 + 3];
                    }
                    let mut y_out = vec![0u8; w];
                    let mut cb_out = vec![0u8; w / 2];
                    let mut cr_out = vec![0u8; w / 2];
                    shift_row(&y_row, shift, &mut y_out);
                    shift_row(&cb_row, shift / 2, &mut cb_out);
                    shift_row(&cr_row, shift / 2, &mut cr_out);
                    let line_mut = &mut frame.data[start..start + w * 2];
                    for x in 0..w {
                        line_mut[x * 2] = y_out[x];
                    }
                    for i in 0..w / 2 {
                        line_mut[i * 4 + 1] = cb_out[i];
                        line_mut[i * 4 + 3] = cr_out[i];
                    }
                }
            }
            PixelFormat::Yuv420p => {
                let w = frame.width as usize;
                let y_size = w * frame.height as usize;
                for (line, &shift) in shifts.iter().enumerate() {
                    if shift == 0 {
                        continue;
                    }
                    let start = line * w;
                    let row = frame.data[start..start + w].to_vec();
                    let mut out = vec![0u8; w];
                    shift_row(&row, shift, &mut out);
                    frame.data[start..start + w].copy_from_slice(&out);
                }
                // Chroma planes: half resolution in both axes.
                let c_size = y_size / 4;
                let c_width = w / 2;
                for plane in 0..2 {
                    let plane_start = y_size + plane * c_size;
                    for cline in 0..frame.height as usize / 2 {
                        let shift = shifts[cline * 2] / 2;
                        if shift == 0 {
                            continue;
                        }
                        let start = plane_start + cline * c_width;
                        let row = frame.data[start..start + c_width].to_vec();
                        let mut out = vec![0u8; c_width];
                        shift_row(&row, shift, &mut out);
                        frame.data[start..start + c_width].copy_from_slice(&out);
                    }
                }
            }
            // Uyvy is converted and handled before this match (see the
            // top of this function), so it never reaches here.
            PixelFormat::Uyvy => {}
            // 10-bit formats are converted and handled before this
            // match too (Issue #27).
            PixelFormat::Yuv420p10 | PixelFormat::Yuv422p10 => {}
            PixelFormat::Rgb24 => {
                let w = frame.width as usize;
                for (line, &shift) in shifts.iter().enumerate() {
                    if shift == 0 {
                        continue;
                    }
                    let start = line * w * 3;
                    let line_data = frame.data[start..start + w * 3].to_vec();
                    for ch in 0..3 {
                        let row: Vec<u8> = (0..w).map(|x| line_data[x * 3 + ch]).collect();
                        let mut out = vec![0u8; w];
                        shift_row(&row, shift, &mut out);
                        for x in 0..w {
                            frame.data[start + x * 3 + ch] = out[x];
                        }
                    }
                }
            }
        }
    }

    fn apply_dropout(&mut self, frame: &mut VideoFrame) {
        if self.params.dropout_probability <= 0.0 {
            return;
        }
        let bytes_per_line = frame.data.len() / frame.height.max(1) as usize;
        for line in 0..frame.height as usize {
            if self.rng.next_f64() < self.params.dropout_probability {
                let start = line * bytes_per_line;
                for b in &mut frame.data[start..start + bytes_per_line] {
                    *b = (self.rng.next_u64() & 0xFF) as u8;
                }
            }
        }
    }

    fn apply_pedestal(&mut self, frame: &mut VideoFrame) {
        let shift = self.params.pedestal_shift;
        if shift == 0 {
            return;
        }
        if frame.format.is_10bit() {
            // Shift the 8-bit equivalent, then expand back
            // (Issue #27).
            if let Ok(mut converted) = frame.to_8bit() {
                self.apply_pedestal(&mut converted);
                if let Ok(back) = VideoFrame::from_8bit(&converted, frame.format) {
                    frame.data.copy_from_slice(&back.data);
                }
            }
            return;
        }
        let adjust = |v: u8| -> u8 { (v as i16 + shift).clamp(0, 255) as u8 };
        match frame.format {
            PixelFormat::Yuv422p | PixelFormat::Yuyv => {
                for quad in frame.data.chunks_exact_mut(4) {
                    quad[0] = adjust(quad[0]);
                    quad[2] = adjust(quad[2]);
                }
            }
            PixelFormat::Uyvy => {
                for quad in frame.data.chunks_exact_mut(4) {
                    quad[1] = adjust(quad[1]);
                    quad[3] = adjust(quad[3]);
                }
            }
            PixelFormat::Yuv420p => {
                let y_size = (frame.width * frame.height) as usize;
                for b in &mut frame.data[..y_size] {
                    *b = adjust(*b);
                }
            }
            PixelFormat::Rgb24 => {
                for b in frame.data.iter_mut() {
                    *b = adjust(*b);
                }
            }
            // 10-bit formats are converted and handled before this
            // match (see the top of this function).
            PixelFormat::Yuv420p10 | PixelFormat::Yuv422p10 => {}
        }
    }
}

/// Peak signal-to-noise ratio between two frames' raw bytes, in dB.
/// Identical frames give `f64::INFINITY`.
pub fn psnr(a: &VideoFrame, b: &VideoFrame) -> f64 {
    assert_eq!(a.data.len(), b.data.len(), "psnr needs equal-sized frames");
    if a.data.is_empty() {
        return f64::INFINITY;
    }
    let mse = a
        .data
        .iter()
        .zip(b.data.iter())
        .map(|(x, y)| {
            let d = *x as f64 - *y as f64;
            d * d
        })
        .sum::<f64>()
        / a.data.len() as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate() -> FrameRate {
        FrameRate {
            num: 30_000,
            den: 1001,
        }
    }

    #[test]
    fn rng_is_reproducible() {
        let mut a = XorShift64::new(42);
        let mut b = XorShift64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = XorShift64::new(43);
        assert_ne!(a.next_u64(), c.next_u64());
    }

    #[test]
    fn clean_source_matches_colour_bars() {
        let mut src = SyntheticSource::new(
            176,
            144,
            PixelFormat::Yuv422p,
            rate(),
            DegradeParams::default(),
            1,
        );
        let frame = src.next_frame();
        let clean = colour_bars(176, 144, PixelFormat::Yuv422p, 0);
        assert_eq!(frame.data, clean.data);
        assert_eq!(frame.pts_ns, 0);
        let frame2 = src.next_frame();
        assert_eq!(frame2.pts_ns, 33_366_666);
    }

    #[test]
    fn drift_stretches_timestamps() {
        let params = DegradeParams {
            clock_drift_ppm: 1000.0,
            ..Default::default()
        };
        let mut src = SyntheticSource::new(176, 144, PixelFormat::Yuv422p, rate(), params, 1);
        let _ = src.next_frame();
        let f = src.next_frame();
        // Nominal 33_366_666 ns stretched by 1000 ppm.
        assert!(
            (f.pts_ns as i64 - 33_400_033).abs() <= 2,
            "pts was {}",
            f.pts_ns
        );
    }

    #[test]
    fn drop_probability_skips_frames() {
        let params = DegradeParams {
            drop_probability: 1.0,
            ..Default::default()
        };
        // Probability 1.0 would loop forever by design; use 0.5 and check
        // that produced_count advances faster than returned frames.
        let params = DegradeParams {
            drop_probability: 0.5,
            ..params
        };
        let mut src = SyntheticSource::new(64, 48, PixelFormat::Yuv422p, rate(), params, 7);
        let mut last_seq = None;
        for _ in 0..20 {
            let f = src.next_frame();
            if let Some(prev) = last_seq {
                assert!(f.sequence > prev);
            }
            last_seq = Some(f.sequence);
        }
        assert!(src.produced_count() > 20, "expected some drops");
    }

    #[test]
    fn pedestal_shift_moves_luma() {
        let params = DegradeParams {
            pedestal_shift: 16,
            ..Default::default()
        };
        let mut src = SyntheticSource::new(70, 4, PixelFormat::Yuv422p, rate(), params, 1);
        let frame = src.next_frame();
        let clean = colour_bars(70, 4, PixelFormat::Yuv422p, 0);
        // First pixel is the white bar's Y: 235 + 16 clamps to 251... it
        // must simply be higher than clean (no clamp at 235+16=251).
        assert_eq!(frame.data[0], 251);
        assert_eq!(clean.data[0], 235);
    }

    #[test]
    fn psnr_of_identical_frames_is_infinite() {
        let a = colour_bars(64, 48, PixelFormat::Yuv422p, 0);
        assert_eq!(psnr(&a, &a), f64::INFINITY);
        let mut b = a.clone();
        b.data[0] = b.data[0].wrapping_add(10);
        let value = psnr(&a, &b);
        assert!(value > 20.0 && value < 80.0, "psnr was {value}");
    }
}
