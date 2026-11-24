//! Per-line horizontal jitter (phase) detection and correction.
//!
//! Design: SPEC.md §4.2. Detection estimates how far each line's content
//! is displaced relative to a reference (the same line of the previous
//! frame) by normalised cross-correlation with parabolic peak
//! interpolation; correction shifts lines back by the inverse amount.
//!
//! Performance notes (Issue #19): detection works on luma planes extracted
//! once per frame into reused buffers (the previous frame's plane is kept
//! as the reference instead of re-extracting it per line), and the Pearson
//! correlation for every candidate shift is computed from integer sums:
//! prefix sums make the per-shift means/variances O(1), leaving a single
//! multiply-accumulate loop for the cross term. No heap allocation happens
//! per line; scratch buffers live in the detector.

use crate::{PixelFormat, TbcError, VideoFrame};

/// Estimated horizontal shift of one line, in sub-pixel units (1/256 px).
/// Positive means the line's content sits that far to the *right* of the
/// reference.
#[derive(Debug, Clone, Copy, Default)]
pub struct LineShift {
    pub line: u32,
    pub shift_q8: i32,
}

/// Extract the full luma plane of a frame into `out` (width*height bytes),
/// reusing the buffer's capacity. Returns `false` when the frame data is
/// shorter than its declared geometry.
fn extract_luma(frame: &VideoFrame, out: &mut Vec<u8>) -> bool {
    let w = frame.width as usize;
    let h = frame.height as usize;
    if frame.data.len() < frame.format.frame_size(frame.width, frame.height) {
        return false;
    }
    out.clear();
    out.resize(w * h, 0);
    match frame.format {
        PixelFormat::Yuv420p => {
            out.copy_from_slice(&frame.data[..w * h]);
        }
        PixelFormat::Yuv422p | PixelFormat::Yuyv => {
            for l in 0..h {
                let src = &frame.data[l * w * 2..(l + 1) * w * 2];
                let dst = &mut out[l * w..(l + 1) * w];
                for x in 0..w {
                    dst[x] = src[x * 2];
                }
            }
        }
        PixelFormat::Uyvy => {
            for l in 0..h {
                let src = &frame.data[l * w * 2..(l + 1) * w * 2];
                let dst = &mut out[l * w..(l + 1) * w];
                for x in 0..w {
                    dst[x] = src[x * 2 + 1];
                }
            }
        }
        PixelFormat::Rgb24 => {
            for l in 0..h {
                let src = &frame.data[l * w * 3..l * w * 3 + w * 3];
                let dst = &mut out[l * w..l * w + w];
                for x in 0..w {
                    let (r, g, b) = (
                        src[x * 3] as u32,
                        src[x * 3 + 1] as u32,
                        src[x * 3 + 2] as u32,
                    );
                    dst[x] = ((r * 77 + g * 150 + b * 29) >> 8) as u8;
                }
            }
        }
    }
    true
}

/// Extract one line of luma samples from a frame, whatever its layout.
/// Returns `None` for an out-of-range line. Used by the field module;
/// the detector itself works on whole planes (see `extract_luma`).
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
        PixelFormat::Yuv422p | PixelFormat::Yuyv => {
            let start = l * w * 2;
            let line_data = frame.data.get(start..start + w * 2)?;
            (0..w).map(|x| line_data[x * 2]).collect()
        }
        PixelFormat::Uyvy => {
            let start = l * w * 2;
            let line_data = frame.data.get(start..start + w * 2)?;
            (0..w).map(|x| line_data[x * 2 + 1]).collect()
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

/// Prefix sums of a row: `sum[i]` = sum of the first `i` samples,
/// `sq[i]` = sum of their squares. Buffers are reused across calls and
/// every entry is overwritten, so nothing is cleared per call.
fn prefix_sums(row: &[u8], sum: &mut Vec<u64>, sq: &mut Vec<u64>) {
    if sum.len() < row.len() + 1 {
        sum.resize(row.len() + 1, 0);
        sq.resize(row.len() + 1, 0);
    }
    sum[0] = 0;
    sq[0] = 0;
    for (i, &v) in row.iter().enumerate() {
        let v = v as u64;
        sum[i + 1] = sum[i] + v;
        sq[i + 1] = sq[i] + v * v;
    }
}

/// Sum of `buf[from..to]` from a prefix-sum table.
fn range_sum(prefix: &[u64], from: usize, to: usize) -> u64 {
    prefix[to] - prefix[from]
}

/// Pearson correlation between `a[x]` and `b[x + shift]` over the overlap,
/// from precomputed prefix sums of both rows plus one cross-term loop.
fn correlation(
    a: &[u8],
    a_sum: &[u64],
    a_sq: &[u64],
    b: &[u8],
    b_sum: &[u64],
    b_sq: &[u64],
    shift: i32,
) -> f64 {
    let w = a.len() as i32;
    let x0 = (-shift).max(0);
    let x1 = (w - shift).min(w);
    if x0 >= x1 {
        return 0.0;
    }
    let (x0, x1) = (x0 as usize, x1 as usize);
    let count = x1 - x0;
    if count < 8 {
        return 0.0;
    }
    let (b0, b1) = ((x0 as i32 + shift) as usize, (x1 as i32 + shift) as usize);
    let sum_a = range_sum(a_sum, x0, x1) as f64;
    let sum_b = range_sum(b_sum, b0, b1) as f64;
    let sq_a = range_sum(a_sq, x0, x1) as f64;
    let sq_b = range_sum(b_sq, b0, b1) as f64;
    // Zip iteration over the two overlap windows: no index arithmetic
    // and no per-sample bounds checks in the hot loop.
    let sum_ab: u64 = a[x0..x1]
        .iter()
        .zip(b[b0..b1].iter())
        .map(|(&u, &v)| u as u64 * v as u64)
        .sum();
    let n = count as f64;
    let cov = sum_ab as f64 - sum_a * sum_b / n;
    let var_a = sq_a - sum_a * sum_a / n;
    let var_b = sq_b - sum_b * sum_b / n;
    if var_a <= 0.0 || var_b <= 0.0 {
        return 0.0;
    }
    cov / (var_a * var_b).sqrt()
}

/// Evaluate the correlation for every shift in `range`, recording the
/// values in `corrs` (indexed by shift + max_shift_px) and returning the
/// best (shift, correlation) pair.
#[allow(clippy::too_many_arguments)]
fn eval_shifts(
    reference: &[u8],
    a_sum: &[u64],
    a_sq: &[u64],
    current: &[u8],
    b_sum: &[u64],
    b_sq: &[u64],
    range: std::ops::RangeInclusive<i32>,
    max_shift_px: i32,
    corrs: &mut [f64],
) -> (i32, f64) {
    let mut best_shift = 0i32;
    let mut best_corr = f64::NEG_INFINITY;
    for s in range {
        let c = correlation(reference, a_sum, a_sq, current, b_sum, b_sq, s);
        corrs[(s + max_shift_px) as usize] = c;
        if c > best_corr {
            best_corr = c;
            best_shift = s;
        }
    }
    (best_shift, best_corr)
}

/// Stateful per-line jitter detector. Feed frames in order.
#[derive(Debug)]
pub struct JitterDetector {
    max_shift_px: i32,
    min_mean_correlation: f64,
    /// Luma plane of the previous frame (the reference), if any.
    previous_luma: Option<Vec<u8>>,
    previous_width: u32,
    previous_height: u32,
    /// Scratch: luma plane of the frame being analysed.
    current_luma: Vec<u8>,
    /// Scratch prefix sums for the current row and the reference row.
    a_sum: Vec<u64>,
    a_sq: Vec<u64>,
    b_sum: Vec<u64>,
    b_sq: Vec<u64>,
    /// Scratch correlation values for the candidate shifts of one line.
    corrs: Vec<f64>,
    /// Best integer-pixel shift per line from the last accepted frame,
    /// used as the centre of the next frame's narrow search window.
    prev_best_shifts: Vec<i32>,
    /// Prefix-sum tables of the reference (previous) frame's luma plane,
    /// flat: one (sum, sq) pair of rows of length width+1 per line. They
    /// are built while that frame was the current one, so the reference
    /// side of the correlation never pays the build cost twice.
    ref_sum: Vec<u64>,
    ref_sq: Vec<u64>,
    /// Tables being accumulated for the current frame, swapped into
    /// `ref_sum`/`ref_sq` when the frame becomes the reference.
    next_ref_sum: Vec<u64>,
    next_ref_sq: Vec<u64>,
    ref_tables_valid: bool,
}

impl JitterDetector {
    pub fn new(max_shift_px: u32) -> Self {
        JitterDetector {
            max_shift_px: max_shift_px as i32,
            min_mean_correlation: 0.3,
            previous_luma: None,
            previous_width: 0,
            previous_height: 0,
            current_luma: Vec::new(),
            a_sum: Vec::new(),
            a_sq: Vec::new(),
            b_sum: Vec::new(),
            b_sq: Vec::new(),
            corrs: Vec::new(),
            prev_best_shifts: Vec::new(),
            ref_sum: Vec::new(),
            ref_sq: Vec::new(),
            next_ref_sum: Vec::new(),
            next_ref_sq: Vec::new(),
            ref_tables_valid: false,
        }
    }

    /// Estimate the shifts of `frame` against the reference.
    ///
    /// Returns `None` when the correlation is too weak to trust (flat or
    /// unrelated content); the caller must then skip correction for this
    /// frame. The frame is always remembered as the next reference.
    pub fn estimate(&mut self, frame: &VideoFrame) -> Option<Vec<LineShift>> {
        if !extract_luma(frame, &mut self.current_luma) {
            return None;
        }
        let w = frame.width as usize;
        let have_reference = self.previous_luma.is_some()
            && self.previous_width == frame.width
            && self.previous_height == frame.height;
        // Size the table buffers for this geometry (rows of width+1).
        let table_need = (w + 1) * frame.height as usize;
        if self.next_ref_sum.len() != table_need {
            self.next_ref_sum.resize(table_need, 0);
            self.next_ref_sq.resize(table_need, 0);
        }
        let use_ref_tables = have_reference && self.ref_tables_valid;
        self.corrs.resize((2 * self.max_shift_px + 1) as usize, 0.0);
        let mut shifts = Vec::with_capacity(frame.height as usize);
        let mut corr_sum = 0.0f64;
        let mut corr_count = 0usize;
        // Frame-to-frame tracking (Issue #19): a given line's jitter
        // changes only slightly between consecutive frames, so each line
        // first searches a narrow window around that line's peak in the
        // previous accepted frame. A weak or edge-of-window peak falls
        // back to a full search for that line.
        let mut best_shifts: Vec<i32> = Vec::with_capacity(frame.height as usize);
        for line in 0..frame.height {
            let l = line as usize;
            let current: &[u8] = &self.current_luma[l * w..l * w + w];
            // Build the current row's prefix tables first and cache them
            // for the next frame — for every line, including line 0,
            // which may have no reference to compare against this time.
            prefix_sums(current, &mut self.b_sum, &mut self.b_sq);
            {
                let off = l * (w + 1);
                self.next_ref_sum[off..off + w + 1].copy_from_slice(&self.b_sum[..w + 1]);
                self.next_ref_sq[off..off + w + 1].copy_from_slice(&self.b_sq[..w + 1]);
            }
            let reference: Option<&[u8]> = if have_reference {
                let prev = self.previous_luma.as_ref().expect("checked above");
                Some(&prev[l * w..l * w + w])
            } else if line > 0 {
                Some(&self.current_luma[(l - 1) * w..(l - 1) * w + w])
            } else {
                None
            };
            let Some(reference) = reference else {
                shifts.push(LineShift { line, shift_q8: 0 });
                best_shifts.push(0);
                continue;
            };
            // Argument order matters: the reference is `a`, the current
            // row is `b`, and the shift indexes into `b` (as in the
            // original two-pass implementation). The reference tables
            // usually come from the cache built during the previous
            // frame; the current row's tables are built now and cached
            // for the next frame.
            let (a_sum, a_sq): (&[u64], &[u64]) = if use_ref_tables {
                let off = l * (w + 1);
                (
                    &self.ref_sum[off..off + w + 1],
                    &self.ref_sq[off..off + w + 1],
                )
            } else {
                prefix_sums(reference, &mut self.a_sum, &mut self.a_sq);
                (&self.a_sum, &self.a_sq)
            };
            let (a_sum, a_sq, b_sum, b_sq) = (a_sum, a_sq, &self.b_sum[..], &self.b_sq[..]);
            for slot in self.corrs.iter_mut() {
                *slot = f64::NEG_INFINITY;
            }
            let max = self.max_shift_px;
            let full = -max..=max;
            let center = if self.prev_best_shifts.len() == frame.height as usize {
                Some(self.prev_best_shifts[l])
            } else {
                None
            };
            let (best_shift, best_corr) = match center {
                Some(t) => {
                    let lo = (t - 1).max(-max);
                    let hi = (t + 1).min(max);
                    let (bs, bc) = eval_shifts(
                        reference,
                        a_sum,
                        a_sq,
                        current,
                        b_sum,
                        b_sq,
                        lo..=hi,
                        max,
                        &mut self.corrs,
                    );
                    let at_window_edge = (bs == lo && lo > -max) || (bs == hi && hi < max);
                    if bc < 0.5 || at_window_edge {
                        eval_shifts(
                            reference,
                            a_sum,
                            a_sq,
                            current,
                            b_sum,
                            b_sq,
                            full,
                            max,
                            &mut self.corrs,
                        )
                    } else {
                        (bs, bc)
                    }
                }
                None => eval_shifts(
                    reference,
                    a_sum,
                    a_sq,
                    current,
                    b_sum,
                    b_sq,
                    full,
                    max,
                    &mut self.corrs,
                ),
            };
            best_shifts.push(best_shift);
            // Parabolic interpolation around the peak for sub-pixel reach.
            let idx = (best_shift + self.max_shift_px) as usize;
            let mut frac = 0.0f64;
            if idx > 0 && idx + 1 < self.corrs.len() {
                let (cm, c0, cp) = (self.corrs[idx - 1], self.corrs[idx], self.corrs[idx + 1]);
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
        // The current plane becomes the next frame's reference; keep
        // the old reference buffer around as next time's scratch.
        let new_previous = std::mem::take(&mut self.current_luma);
        if let Some(old) = self.previous_luma.replace(new_previous) {
            self.current_luma = old;
        }
        self.previous_width = frame.width;
        self.previous_height = frame.height;
        // The tables accumulated for this frame become the reference
        // tables for the next one.
        std::mem::swap(&mut self.ref_sum, &mut self.next_ref_sum);
        std::mem::swap(&mut self.ref_sq, &mut self.next_ref_sq);
        self.ref_tables_valid = true;
        if corr_count == 0 || corr_sum / corr_count as f64 <= self.min_mean_correlation {
            return None;
        }
        // Only accepted frames teach the tracker: a rejected frame's
        // peaks are not trustworthy starting points for the next frame.
        self.prev_best_shifts = best_shifts;
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
    // The integer/fraction parts of the source position are the same for
    // every output sample (the position is x*256 - shift_q8), so the
    // Euclidean division is hoisted out of the loop (Issue #19: per-sample
    // division dominated this function's cost). The weighted sum is
    // always non-negative, so `>> 8` replaces the division by 256.
    let q = shift_q8.div_euclid(256);
    let r = shift_q8.rem_euclid(256);
    let (i0_off, frac) = if r == 0 { (-q, 0) } else { (-q - 1, 256 - r) };
    for (x, o) in out.iter_mut().enumerate() {
        let i0 = x as i32 + i0_off;
        let a = row[i0.clamp(0, n - 1) as usize] as i32;
        let b = row[(i0 + 1).clamp(0, n - 1) as usize] as i32;
        *o = ((a * (256 - frac) + b * frac + 128) >> 8) as u8;
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
    // Scratch rows, allocated once and reused for every line (Issue #19).
    let mut line_copy = vec![0u8; line_bytes];
    let mut y_row = vec![0u8; w];
    let mut cb_row = vec![0u8; w / 2];
    let mut cr_row = vec![0u8; w / 2];
    let mut y_out = vec![0u8; w];
    let mut cb_out = vec![0u8; w / 2];
    let mut cr_out = vec![0u8; w / 2];
    for shift in shifts {
        let l = shift.line as usize;
        if l >= frame.height as usize || shift.shift_q8 == 0 {
            continue;
        }
        let start = l * line_bytes;
        line_copy.copy_from_slice(&frame.data[start..start + line_bytes]);
        // De-interleave into Y and the two chroma rows.
        for (dst, pair) in y_row.iter_mut().zip(line_copy.chunks_exact(2)) {
            *dst = pair[0];
        }
        for ((cb, cr), quad) in cb_row
            .iter_mut()
            .zip(cr_row.iter_mut())
            .zip(line_copy.chunks_exact(4))
        {
            *cb = quad[1];
            *cr = quad[3];
        }
        shift_row(&y_row, -shift.shift_q8, &mut y_out);
        // Chroma has half the horizontal resolution: half the displacement.
        shift_row(&cb_row, -shift.shift_q8 / 2, &mut cb_out);
        shift_row(&cr_row, -shift.shift_q8 / 2, &mut cr_out);
        let line_mut = &mut frame.data[start..start + line_bytes];
        for (pair, &y) in line_mut.chunks_exact_mut(2).zip(y_out.iter()) {
            pair[0] = y;
        }
        for (quad, (cb, cr)) in line_mut
            .chunks_exact_mut(4)
            .zip(cb_out.iter().zip(cr_out.iter()))
        {
            quad[1] = *cb;
            quad[3] = *cr;
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
