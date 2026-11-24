//! Field handling for interlaced frames: field-order bookkeeping, the
//! weave check (is this frame effectively progressive?) and a simple
//! Bob deinterlacer. Design: SPEC.md §4.3.

use crate::jitter::luma_row;
use crate::{FieldOrder, PixelFormat, VideoFrame};

/// Mean absolute luma difference between the two fields of a frame,
/// estimated by comparing each line with the next line (which belongs
/// to the other field). Returns `None` for frames with fewer than
/// 2 lines or an unsupported layout.
pub fn field_motion(frame: &VideoFrame) -> Option<f64> {
    if frame.height < 2 {
        return None;
    }
    let mut sum = 0.0f64;
    let mut count = 0usize;
    for line in 0..frame.height - 1 {
        let a = luma_row(frame, line)?;
        let b = luma_row(frame, line + 1)?;
        for (x, y) in a.iter().zip(b.iter()) {
            sum += (*x as f64 - *y as f64).abs();
            count += 1;
        }
    }
    if count == 0 {
        return None;
    }
    Some(sum / count as f64)
}

/// Weave check: true when the two fields are similar enough that the
/// frame can be treated as progressive (SPEC.md §4.3).
pub fn weave_check(frame: &VideoFrame, threshold: f64) -> bool {
    match field_motion(frame) {
        Some(motion) => motion < threshold,
        None => false,
    }
}

/// Whole-frame luma plane (one byte per pixel), or `None` when a
/// line cannot be read.
fn luma_plane(frame: &VideoFrame) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity((frame.width * frame.height) as usize);
    for line in 0..frame.height {
        out.extend_from_slice(&luma_row(frame, line)?);
    }
    Some(out)
}

/// Sample one field of a luma plane at `line`: the field's own lines
/// are returned as-is; a line of the opposite parity is synthesised
/// by averaging the field lines above and below it (clamped at the
/// frame edges). `top_parity` selects the field: true = top field
/// (even lines), false = bottom field (odd lines).
fn sample_field_at(
    plane: &[u8],
    width: usize,
    height: usize,
    line: usize,
    x: usize,
    top_parity: bool,
) -> f64 {
    if (line % 2 == 0) == top_parity {
        return plane[line * width + x] as f64;
    }
    let above = (line > 0).then(|| plane[(line - 1) * width + x] as f64);
    let below = (line + 1 < height).then(|| plane[(line + 1) * width + x] as f64);
    match (above, below) {
        (Some(a), Some(b)) => (a + b) / 2.0,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => plane[line * width + x] as f64,
    }
}

/// Automatic field-order detector (Issue #26).
///
/// For each consecutive frame pair, the detector predicts the
/// previous frame's bottom field from its two neighbouring top
/// fields (the same frame's and the next frame's, each vertically
/// interpolated to the bottom field's lines and averaged), and the
/// previous frame's top field from its neighbouring bottom fields.
/// Under the true field order the predicted field is the temporal
/// midpoint of its neighbours, so its prediction error is
/// systematically smaller; under the wrong order the target lies
/// outside the neighbour interval and the error is larger. Errors
/// and a motion measure accumulate over a window of frame pairs; the
/// decision locks once one hypothesis is clearly better and enough
/// motion has been seen, and never changes afterwards.
///
/// Still, low-motion, very short or effectively progressive input
/// never locks: `observe` keeps returning `None` and the caller
/// keeps its configured fallback order. Observation is capped, so an
/// undecided detector costs nothing after the window closes.
pub struct FieldOrderDetector {
    prev_plane: Vec<u8>,
    width: u32,
    height: u32,
    err_tff: f64,
    err_bff: f64,
    motion: f64,
    pairs: u32,
    decided: Option<FieldOrder>,
}

/// Frame pairs to observe before a decision is allowed.
const MIN_PAIRS: u32 = 8;
/// Observation window cap; beyond it the detector gives up.
const MAX_PAIRS: u32 = 120;
/// Mean absolute frame-to-frame luma change per pair required
/// before the evidence counts as motion at all.
const MIN_MOTION_PER_PAIR: f64 = 1.0;
/// The winning hypothesis must beat the other by this factor.
const DECISION_RATIO: f64 = 0.85;

impl Default for FieldOrderDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl FieldOrderDetector {
    pub fn new() -> Self {
        FieldOrderDetector {
            prev_plane: Vec::new(),
            width: 0,
            height: 0,
            err_tff: 0.0,
            err_bff: 0.0,
            motion: 0.0,
            pairs: 0,
            decided: None,
        }
    }

    /// The locked decision, if one has been made.
    pub fn decided(&self) -> Option<FieldOrder> {
        self.decided
    }

    /// Feed one frame. Returns the locked field order once decided
    /// (and on every later call), or `None` while undecided.
    pub fn observe(&mut self, frame: &VideoFrame) -> Option<FieldOrder> {
        if let Some(order) = self.decided {
            return Some(order);
        }
        if self.pairs >= MAX_PAIRS || frame.height < 4 || frame.width == 0 {
            return None;
        }
        if frame.width != self.width || frame.height != self.height {
            // First frame, or the geometry changed: restart the window.
            self.width = frame.width;
            self.height = frame.height;
            self.prev_plane.clear();
            self.err_tff = 0.0;
            self.err_bff = 0.0;
            self.motion = 0.0;
            self.pairs = 0;
        }
        let plane = luma_plane(frame)?;
        if !self.prev_plane.is_empty() {
            self.accumulate(&plane);
            self.pairs += 1;
            if self.pairs >= MIN_PAIRS && self.motion >= MIN_MOTION_PER_PAIR * self.pairs as f64 {
                if self.err_tff < self.err_bff * DECISION_RATIO {
                    self.decided = Some(FieldOrder::TopFieldFirst);
                } else if self.err_bff < self.err_tff * DECISION_RATIO {
                    self.decided = Some(FieldOrder::BottomFieldFirst);
                }
            }
        }
        self.prev_plane = plane;
        self.decided
    }

    /// Add one frame pair's evidence: `cur` is the frame after the
    /// stored previous frame.
    fn accumulate(&mut self, cur: &[u8]) {
        let w = self.width as usize;
        let h = self.height as usize;
        let prev = &self.prev_plane;

        let mut motion_sum = 0.0f64;
        for (a, b) in prev.iter().zip(cur.iter()) {
            motion_sum += (*a as f64 - *b as f64).abs();
        }
        self.motion += motion_sum / prev.len() as f64;

        // Predict the previous frame's bottom field (odd lines) from
        // the neighbouring top fields: small error under TFF.
        let mut err = 0.0f64;
        let mut count = 0usize;
        for line in (1..h).step_by(2) {
            for x in 0..w {
                let pred = (sample_field_at(prev, w, h, line, x, true)
                    + sample_field_at(cur, w, h, line, x, true))
                    / 2.0;
                err += (pred - prev[line * w + x] as f64).abs();
                count += 1;
            }
        }
        if count > 0 {
            self.err_tff += err / count as f64;
        }

        // Predict the previous frame's top field (even lines) from
        // the neighbouring bottom fields: small error under BFF.
        let mut err = 0.0f64;
        let mut count = 0usize;
        for line in (0..h).step_by(2) {
            for x in 0..w {
                let pred = (sample_field_at(prev, w, h, line, x, false)
                    + sample_field_at(cur, w, h, line, x, false))
                    / 2.0;
                err += (pred - prev[line * w + x] as f64).abs();
                count += 1;
            }
        }
        if count > 0 {
            self.err_bff += err / count as f64;
        }
    }
}

/// Bob deinterlacing of one field: take every second line (the field's
/// own lines) and synthesise the missing lines by averaging neighbours.
/// Returns a progressive frame of the same height, or `None` when the
/// frame layout cannot be split into fields this way.
pub fn bob_field(frame: &VideoFrame, top_field: bool) -> Option<VideoFrame> {
    if frame.height < 2 || frame.format == PixelFormat::Yuv420p || frame.format.is_10bit() {
        // Yuv420p chroma is shared between line pairs; a naive per-line
        // split would corrupt it. 10-bit frames store u16 samples, so
        // the byte-wise line work below does not apply either. Both
        // are left for a later, format-aware pass.
        return None;
    }
    let first = if top_field { 0 } else { 1 };
    let mut out = frame.clone();
    out.field_order = FieldOrder::Progressive;
    let bytes_per_line = (frame.data.len() as u32 / frame.height) as usize;
    for line in 0..frame.height as usize {
        if line % 2 == first {
            continue; // the field's own line: already in place
        }
        let above = if line == 0 { line + 1 } else { line - 1 };
        let below = if line + 1 >= frame.height as usize {
            line - 1
        } else {
            line + 1
        };
        for i in 0..bytes_per_line {
            let a = frame.data[above * bytes_per_line + i] as u16;
            let b = frame.data[below * bytes_per_line + i] as u16;
            out.data[line * bytes_per_line + i] = (a + b).div_ceil(2) as u8;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            field_order: FieldOrder::BottomFieldFirst,
            pts_ns: 0,
            sequence: 0,
            data,
        }
    }

    #[test]
    fn identical_fields_have_zero_motion_and_weave() {
        // All lines equal: the two fields are identical.
        let frame = yuv422p_frame(8, 4, &[100; 32]);
        assert_eq!(field_motion(&frame), Some(0.0));
        assert!(weave_check(&frame, 1.0));
    }

    #[test]
    fn shifted_field_raises_motion_and_fails_weave() {
        // Even lines dark, odd lines bright: maximal field difference.
        let mut luma = Vec::new();
        for line in 0..4 {
            let v = if line % 2 == 0 { 40 } else { 200 };
            luma.extend(std::iter::repeat_n(v, 8));
        }
        let frame = yuv422p_frame(8, 4, &luma);
        let motion = field_motion(&frame).expect("motion");
        assert!(motion > 100.0, "motion was {motion}");
        assert!(!weave_check(&frame, 1.0));
    }

    #[test]
    fn bob_produces_progressive_frame_of_same_size() {
        let frame = yuv422p_frame(8, 4, &[90; 32]);
        let bobbed = bob_field(&frame, true).expect("bob");
        assert_eq!(bobbed.field_order, FieldOrder::Progressive);
        assert_eq!(bobbed.data.len(), frame.data.len());
        assert!(bobbed.data.iter().all(|&b| b == 90 || b == 128));
    }

    /// Smooth test scene sampled at field time `t`: a horizontal
    /// sine wave drifting 2 px per field step, plus a gentle linear
    /// vertical gradient (exact under vertical interpolation).
    fn scene(x: u32, y: u32, t: u32) -> u8 {
        let phase = (x as f64 + 2.0 * t as f64) * std::f64::consts::PI / 32.0;
        (128.0 + 70.0 * phase.sin() + y as f64 * 0.5).round() as u8
    }

    /// One interlaced frame whose fields are sampled at consecutive
    /// field times: frame `index` covers times 2*index and 2*index+1.
    /// With `tff`, even lines take the earlier time; without, odd
    /// lines do. With `progressive`, both fields share time `index`.
    fn timed_frame(index: u32, tff: bool, progressive: bool) -> VideoFrame {
        let (width, height) = (64u32, 16u32);
        let mut luma = vec![0u8; (width * height) as usize];
        for line in 0..height {
            let top = line % 2 == 0;
            let t = if progressive {
                index
            } else if top == tff {
                2 * index
            } else {
                2 * index + 1
            };
            for x in 0..width {
                luma[(line * width + x) as usize] = scene(x, line, t);
            }
        }
        yuv422p_frame(width, height, &luma)
    }

    #[test]
    fn detector_locks_top_field_first() {
        let mut det = FieldOrderDetector::new();
        let mut result = None;
        for i in 0..20 {
            result = det.observe(&timed_frame(i, true, false));
        }
        assert_eq!(result, Some(FieldOrder::TopFieldFirst));
        assert_eq!(det.decided(), Some(FieldOrder::TopFieldFirst));
    }

    #[test]
    fn detector_locks_bottom_field_first() {
        let mut det = FieldOrderDetector::new();
        let mut result = None;
        for i in 0..20 {
            result = det.observe(&timed_frame(i, false, false));
        }
        assert_eq!(result, Some(FieldOrder::BottomFieldFirst));
    }

    #[test]
    fn detector_never_locks_on_still_frames() {
        let mut det = FieldOrderDetector::new();
        let still = timed_frame(0, true, false);
        for _ in 0..40 {
            assert_eq!(det.observe(&still), None);
        }
    }

    #[test]
    fn detector_needs_more_than_a_few_frames() {
        let mut det = FieldOrderDetector::new();
        for i in 0..4 {
            assert_eq!(det.observe(&timed_frame(i, true, false)), None);
        }
    }

    #[test]
    fn detector_never_locks_on_progressive_content() {
        let mut det = FieldOrderDetector::new();
        for i in 0..30 {
            assert_eq!(det.observe(&timed_frame(i, true, true)), None);
        }
    }

    #[test]
    fn detector_decision_is_sticky() {
        let mut det = FieldOrderDetector::new();
        for i in 0..20 {
            det.observe(&timed_frame(i, true, false));
        }
        assert_eq!(det.decided(), Some(FieldOrder::TopFieldFirst));
        // Opposite content afterwards must not flip the decision.
        for i in 0..20 {
            let result = det.observe(&timed_frame(i, false, false));
            assert_eq!(result, Some(FieldOrder::TopFieldFirst));
        }
    }

    #[test]
    fn bob_rejects_yuv420p_for_now() {
        let frame = VideoFrame {
            width: 4,
            height: 4,
            format: PixelFormat::Yuv420p,
            field_order: FieldOrder::TopFieldFirst,
            pts_ns: 0,
            sequence: 0,
            data: vec![0u8; PixelFormat::Yuv420p.frame_size(4, 4)],
        };
        assert!(bob_field(&frame, true).is_none());
    }
}
