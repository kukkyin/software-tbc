//! Per-line horizontal jitter (phase) detection and correction.
//!
//! Design: SPEC.md §4.2. Detection estimates how far each line's content
//! is displaced relative to a reference (the same line of the previous
//! frame) by normalised cross-correlation with parabolic peak
//! interpolation; correction shifts lines back by the inverse amount.

use crate::{PixelFormat, TbcError, VideoFrame};

/// Estimated horizontal shift of one line, in sub-pixel units (1/256 px).
/// Positive means the line's content sits that far to the *right* of the
/// reference.
#[derive(Debug, Clone, Copy, Default)]
pub struct LineShift {
    pub line: u32,
    pub shift_q8: i32,
}

/// Extract one line of luma samples from a frame, whatever its layout.
/// Returns `None` for an out-of-range line.
pub(crate) fn luma_row(frame: &VideoFrame, line: u32) -> Option<Vec<u8>> {
    if line >= frame.height {
        return None;
    }
    let w = frame.width as usize;
    let l = line as usize;
    let row = match frame.format {
        PixelFormat::Yuv420p => {
            let start = l * w;
            frame.data.get(start..start + w)?.to_vec()
        }
        PixelFormat::Yuv422p => {
            let start = l * w * 2;
            let line_data = frame.data.get(start..start + w * 2)?;
            (0..w).map(|x| line_data[x * 2]).collect()
        }
        PixelFormat::Rgb24 => {
            let start = l * w * 3;
            let line_data = frame.data.get(start..start + w * 3)?;
            (0..w)
                .map(|x| {
                    let (r, g, b) = (
                        line_data[x * 3] as u32,
                        line_data[x * 3 + 1] as u32,
                        line_data[x * 3 + 2] as u32,
                    );
                    ((r * 77 + g * 150 + b * 29) >> 8) as u8
                })
                .collect()
        }
    };
    Some(row)
}

/// Pearson correlation between `a[x]` and `b[x + shift]` over the overlap.
fn correlation(a: &[u8], b: &[u8], shift: i32) -> f64 {
    let mut sum_a = 0.0f64;
    let mut sum_b = 0.0f64;
    let mut count = 0usize;
    for (x, &av) in a.iter().enumerate() {
        let bx = x as i32 + shift;
        if bx < 0 || bx as usize >= b.len() {
            continue;
        }
        sum_a += av as f64;
        sum_b += b[bx as usize] as f64;
        count += 1;
    }
    if count < 8 {
        return 0.0;
    }
    let mean_a = sum_a / count as f64;
    let mean_b = sum_b / count as f64;
    let (mut cov, mut var_a, mut var_b) = (0.0f64, 0.0f64, 0.0f64);
    for (x, &av) in a.iter().enumerate() {
        let bx = x as i32 + shift;
        if bx < 0 || bx as usize >= b.len() {
            continue;
        }
        let da = av as f64 - mean_a;
        let db = b[bx as usize] as f64 - mean_b;
        cov += da * db;
        var_a += da * da;
        var_b += db * db;
    }
    if var_a <= 0.0 || var_b <= 0.0 {
        return 0.0;
    }
    cov / (var_a.sqrt() * var_b.sqrt())
}

/// Stateful per-line jitter detector. Feed frames in order.
#[derive(Debug)]
pub struct JitterDetector {
    max_shift_px: i32,
    min_mean_correlation: f64,
    previous: Option<VideoFrame>,
}

impl JitterDetector {
    pub fn new(max_shift_px: u32) -> Self {
        JitterDetector {
            max_shift_px: max_shift_px as i32,
            min_mean_correlation: 0.3,
            previous: None,
        }
    }

    /// Estimate the shifts of `frame` against the reference.
    ///
    /// Returns `None` when the correlation is too weak to trust (flat or
    /// unrelated content); the caller must then skip correction for this
    /// frame. The frame is always remembered as the next reference.
    pub fn estimate(&mut self, frame: &VideoFrame) -> Option<Vec<LineShift>> {
        let mut shifts = Vec::with_capacity(frame.height as usize);
        let mut corr_sum = 0.0f64;
        let mut corr_count = 0usize;
        for line in 0..frame.height {
            let current = luma_row(frame, line)?;
            let reference = match &self.previous {
                Some(prev) => luma_row(prev, line),
                None if line > 0 => luma_row(frame, line - 1),
                None => None,
            };
            let Some(reference) = reference else {
                shifts.push(LineShift { line, shift_q8: 0 });
                continue;
            };
            let mut best_shift = 0i32;
            let mut best_corr = f64::NEG_INFINITY;
            let mut corrs = Vec::with_capacity((2 * self.max_shift_px + 1) as usize);
            for s in -self.max_shift_px..=self.max_shift_px {
                let c = correlation(&reference, &current, s);
                corrs.push(c);
                if c > best_corr {
                    best_corr = c;
                    best_shift = s;
                }
            }
            // Parabolic interpolation around the peak for sub-pixel reach.
            let idx = (best_shift + self.max_shift_px) as usize;
            let mut frac = 0.0f64;
            if idx > 0 && idx + 1 < corrs.len() {
                let (cm, c0, cp) = (corrs[idx - 1], corrs[idx], corrs[idx + 1]);
                let denom = cm - 2.0 * c0 + cp;
                if denom.abs() > f64::EPSILON {
                    frac = (0.5 * (cm - cp) / denom).clamp(-1.0, 1.0);
                }
            }
            let shift_q8 = ((best_shift as f64 + frac) * 256.0).round() as i32;
            shifts.push(LineShift { line, shift_q8 });
            corr_sum += best_corr;
            corr_count += 1;
        }
        self.previous = Some(frame.clone());
        if corr_count == 0 || corr_sum / corr_count as f64 <= self.min_mean_correlation {
            return None;
        }
        median_filter(&mut shifts);
        Some(shifts)
    }
}

/// Window-3 median filter over the shift series, to drop one-line outliers.
fn median_filter(shifts: &mut [LineShift]) {
    if shifts.len() < 3 {
        return;
    }
    let original: Vec<i32> = shifts.iter().map(|s| s.shift_q8).collect();
    for i in 1..shifts.len() - 1 {
        let mut window = [original[i - 1], original[i], original[i + 1]];
        window.sort_unstable();
        shifts[i].shift_q8 = window[1];
    }
}

/// Shift one row of samples by `shift_q8` (positive moves content right),
/// with linear interpolation and edge replication.
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

/// Apply the inverse of the detected shifts, correcting each line.
///
/// Only Yuv422p is supported in this step; other formats are left
/// untouched and reported as `Ok`, per the Phase 2 issue scope.
pub fn apply_shifts(frame: &mut VideoFrame, shifts: &[LineShift]) -> Result<(), TbcError> {
    frame.validate()?;
    if frame.format != PixelFormat::Yuv422p {
        return Ok(());
    }
    let w = frame.width as usize;
    let line_bytes = w * 2;
    for shift in shifts {
        let l = shift.line as usize;
        if l >= frame.height as usize || shift.shift_q8 == 0 {
            continue;
        }
        let start = l * line_bytes;
        let line_data = frame.data[start..start + line_bytes].to_vec();
        // De-interleave into Y and the two chroma rows.
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
        shift_row(&y_row, -shift.shift_q8, &mut y_out);
        // Chroma has half the horizontal resolution: half the displacement.
        shift_row(&cb_row, -shift.shift_q8 / 2, &mut cb_out);
        shift_row(&cr_row, -shift.shift_q8 / 2, &mut cr_out);
        let line_mut = &mut frame.data[start..start + line_bytes];
        for x in 0..w {
            line_mut[x * 2] = y_out[x];
        }
        for i in 0..w / 2 {
            line_mut[i * 4 + 1] = cb_out[i];
            line_mut[i * 4 + 3] = cr_out[i];
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldOrder;

    /// Deterministic pseudo-random byte stream (LCG), no external crates.
    fn lcg_bytes(seed: u32, count: usize) -> Vec<u8> {
        let mut state = seed;
        (0..count)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    fn yuv422p_frame(width: u32, height: u32, luma: &[u8]) -> VideoFrame {
        let mut data = Vec::with_capacity((width * height * 2) as usize);
        for &y in luma {
            data.push(y);
            data.push(128);
        }
        VideoFrame {
            width,
            height,
            format: PixelFormat::Yuv422p,
            field_order: FieldOrder::Progressive,
            pts_ns: 0,
            sequence: 0,
            data,
        }
    }

    fn shift_luma_right(luma: &[u8], width: usize, px: usize) -> Vec<u8> {
        let mut out = vec![0u8; luma.len()];
        for (i, v) in out.iter_mut().enumerate() {
            let x = i % width;
            let src_x = x.saturating_sub(px);
            *v = luma[i - x + src_x];
        }
        out
    }

    #[test]
    fn detects_a_known_three_pixel_shift() {
        let (w, h) = (128usize, 8usize);
        let pattern = lcg_bytes(42, w * h);
        let base = yuv422p_frame(w as u32, h as u32, &pattern);
        let shifted_luma = shift_luma_right(&pattern, w, 3);
        let shifted = yuv422p_frame(w as u32, h as u32, &shifted_luma);

        let mut detector = JitterDetector::new(8);
        let _ = detector.estimate(&base);
        let shifts = detector.estimate(&shifted).expect("should detect");
        for s in &shifts {
            assert!(
                (s.shift_q8 - 3 * 256).abs() <= 64,
                "line {} estimated {} (q8), expected ~768",
                s.line,
                s.shift_q8
            );
        }
    }

    #[test]
    fn unrelated_noise_is_rejected() {
        let (w, h) = (128usize, 8usize);
        let a = yuv422p_frame(w as u32, h as u32, &lcg_bytes(1, w * h));
        let b = yuv422p_frame(w as u32, h as u32, &lcg_bytes(999, w * h));
        let mut detector = JitterDetector::new(8);
        let _ = detector.estimate(&a);
        assert!(detector.estimate(&b).is_none());
    }

    #[test]
    fn zero_shift_application_is_identity() {
        let (w, h) = (32u32, 2u32);
        let luma = lcg_bytes(7, (w * h) as usize);
        let mut frame = yuv422p_frame(w, h, &luma);
        let original = frame.data.clone();
        let shifts: Vec<LineShift> = (0..h).map(|line| LineShift { line, shift_q8: 0 }).collect();
        apply_shifts(&mut frame, &shifts).expect("apply");
        assert_eq!(frame.data, original);
    }

    #[test]
    fn correction_reduces_error_after_fractional_shift() {
        let (w, h) = (64u32, 4u32);
        let luma = lcg_bytes(123, (w * h) as usize);
        let original = yuv422p_frame(w, h, &luma);

        // Displace content by +2.5 px: apply_shifts moves content by the
        // inverse of the given estimate, so a -640 estimate moves +2.5px.
        let mut displaced = original.clone();
        let neg: Vec<LineShift> = (0..h)
            .map(|line| LineShift {
                line,
                shift_q8: -640,
            })
            .collect();
        apply_shifts(&mut displaced, &neg).expect("displace");

        let err_before: u64 = original
            .data
            .iter()
            .zip(&displaced.data)
            .map(|(a, b)| (*a as i64 - *b as i64).unsigned_abs())
            .sum();

        let mut corrected = displaced.clone();
        let pos: Vec<LineShift> = (0..h)
            .map(|line| LineShift {
                line,
                shift_q8: 640,
            })
            .collect();
        apply_shifts(&mut corrected, &pos).expect("correct");
        let err_after: u64 = original
            .data
            .iter()
            .zip(&corrected.data)
            .map(|(a, b)| (*a as i64 - *b as i64).unsigned_abs())
            .sum();
        assert!(
            err_after < err_before,
            "correction did not help: before {err_before}, after {err_after}"
        );
    }
}
