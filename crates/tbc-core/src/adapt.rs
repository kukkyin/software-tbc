//! Auto-adaptation estimators (Issue #32).
//!
//! Two slow control loops that follow the source instead of
//! assuming it, both rate-clamped so they cannot run away:
//!
//! - [`RangeEstimator`] classifies the luma range (limited vs
//!   full) from observed frame extremes, changing its verdict only
//!   after several consecutive agreeing windows.
//! - [`JitterAdapter`] widens or narrows the jitter search range
//!   according to the estimated shifts, by at most one small step
//!   per window, inside hard bounds.
//!
//! Neither estimator touches frames; the caller applies verdicts.

use crate::jitter::LineShift;
use crate::{PixelFormat, VideoFrame};

/// Frames per evaluation window for both estimators.
const WINDOW_FRAMES: u32 = 30;
/// Consecutive agreeing windows required to change the range
/// classification (hysteresis / rate clamp).
const RANGE_LOCK_WINDOWS: u32 = 3;

/// Luma range classification of the input signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeClass {
    /// Studio-swing luma, nominally 16..=235.
    Limited,
    /// Full-swing luma, 0..=255.
    Full,
}

/// Minimum/maximum luma of a pipeline-format frame. Returns `None`
/// for formats the pipeline never carries.
fn luma_min_max(frame: &VideoFrame) -> Option<(u8, u8)> {
    let mut min = u8::MAX;
    let mut max = u8::MIN;
    match frame.format {
        PixelFormat::Yuv422p => {
            for &y in frame.data.iter().step_by(2) {
                min = min.min(y);
                max = max.max(y);
            }
        }
        PixelFormat::Yuv420p | PixelFormat::Nv12 => {
            let y_size = (frame.width * frame.height) as usize;
            for &y in frame.data.iter().take(y_size) {
                min = min.min(y);
                max = max.max(y);
            }
        }
        PixelFormat::Rgb24 => {
            for px in frame.data.chunks_exact(3) {
                let y = ((px[0] as u32 * 77 + px[1] as u32 * 150 + px[2] as u32 * 29) >> 8) as u8;
                min = min.min(y);
                max = max.max(y);
            }
        }
        _ => return None,
    }
    Some((min, max))
}

/// Estimates whether the input luma is limited- or full-range.
///
/// Each 30-frame window votes from its observed extremes: content
/// that reaches <= 6 or >= 249 cannot be limited-range; content
/// that stays inside 10..=245 is consistent with limited range.
/// The classification (initially Limited, the pipeline's standing
/// assumption) changes only after 3 consecutive windows vote the
/// other way, so a single bright or dark scene never flips it.
#[derive(Debug, Clone)]
pub struct RangeEstimator {
    current: RangeClass,
    frames: u32,
    win_min: u8,
    win_max: u8,
    seen_min: u8,
    seen_max: u8,
    full_streak: u32,
    limited_streak: u32,
}

impl RangeEstimator {
    pub fn new() -> Self {
        RangeEstimator {
            current: RangeClass::Limited,
            frames: 0,
            win_min: u8::MAX,
            win_max: u8::MIN,
            seen_min: u8::MAX,
            seen_max: u8::MIN,
            full_streak: 0,
            limited_streak: 0,
        }
    }

    /// The current classification.
    pub fn class(&self) -> RangeClass {
        self.current
    }

    /// Luma range over every frame observed so far, or `None`
    /// before the first scannable frame (Issue #37: the analysis
    /// report reuses the estimator's own observations instead of
    /// scanning the frames a second time).
    pub fn observed_range(&self) -> Option<(u8, u8)> {
        if self.seen_min <= self.seen_max {
            Some((self.seen_min, self.seen_max))
        } else {
            None
        }
    }

    /// Observe one frame. Returns the new classification at the
    /// moment it changes, `None` otherwise.
    pub fn observe(&mut self, frame: &VideoFrame) -> Option<RangeClass> {
        let (min, max) = luma_min_max(frame)?;
        self.seen_min = self.seen_min.min(min);
        self.seen_max = self.seen_max.max(max);
        self.win_min = self.win_min.min(min);
        self.win_max = self.win_max.max(max);
        self.frames += 1;
        if self.frames < WINDOW_FRAMES {
            return None;
        }
        self.frames = 0;
        let vote = if self.win_min <= 6 || self.win_max >= 249 {
            Some(RangeClass::Full)
        } else if self.win_min >= 10 && self.win_max <= 245 {
            Some(RangeClass::Limited)
        } else {
            None
        };
        self.win_min = u8::MAX;
        self.win_max = u8::MIN;
        match vote {
            Some(RangeClass::Full) => {
                self.full_streak += 1;
                self.limited_streak = 0;
            }
            Some(RangeClass::Limited) => {
                self.limited_streak += 1;
                self.full_streak = 0;
            }
            None => {}
        }
        let target = if self.full_streak >= RANGE_LOCK_WINDOWS {
            RangeClass::Full
        } else if self.limited_streak >= RANGE_LOCK_WINDOWS {
            RangeClass::Limited
        } else {
            return None;
        };
        if target != self.current {
            self.current = target;
            self.full_streak = 0;
            self.limited_streak = 0;
            return Some(target);
        }
        None
    }
}

impl Default for RangeEstimator {
    fn default() -> Self {
        Self::new()
    }
}

/// Smallest jitter search range the adapter will select.
pub const MIN_SEARCH_PX: u32 = 2;
/// Largest jitter search range the adapter will select.
pub const MAX_SEARCH_PX: u32 = 32;
/// Search-range change per window (the rate clamp).
const SEARCH_STEP_PX: u32 = 2;

/// Adapts the jitter detector's search range to the observed
/// jitter. Every 30 frames: if more than 5% of lines clipped at
/// the search boundary, widen by one step; otherwise, if the
/// window's peak shift stayed under a quarter of the range,
/// narrow by one step. Changes are returned to the caller, which
/// applies them to the detector.
#[derive(Debug, Clone)]
pub struct JitterAdapter {
    current_max: u32,
    frames: u32,
    clipped_lines: u64,
    total_lines: u64,
    peak_q8: i32,
}

impl JitterAdapter {
    pub fn new(initial_max: u32) -> Self {
        JitterAdapter {
            current_max: initial_max.clamp(MIN_SEARCH_PX, MAX_SEARCH_PX),
            frames: 0,
            clipped_lines: 0,
            total_lines: 0,
            peak_q8: 0,
        }
    }

    /// The search range currently in effect.
    pub fn current_max(&self) -> u32 {
        self.current_max
    }

    /// Observe one frame's estimated shifts. Returns the new
    /// search range at the end of a window when it changes.
    pub fn observe(&mut self, shifts: &[LineShift]) -> Option<u32> {
        // A shift counts as clipped when it sits within a quarter
        // pixel of the search boundary.
        let boundary_q8 = self.current_max as i32 * 256 - 64;
        for s in shifts {
            let a = s.shift_q8.abs();
            self.peak_q8 = self.peak_q8.max(a);
            if a >= boundary_q8 {
                self.clipped_lines += 1;
            }
            self.total_lines += 1;
        }
        self.frames += 1;
        if self.frames < WINDOW_FRAMES {
            return None;
        }
        self.frames = 0;
        let clipped_ratio = if self.total_lines > 0 {
            self.clipped_lines as f64 / self.total_lines as f64
        } else {
            0.0
        };
        let next = if clipped_ratio > 0.05 && self.current_max < MAX_SEARCH_PX {
            (self.current_max + SEARCH_STEP_PX).min(MAX_SEARCH_PX)
        } else if clipped_ratio <= 0.05
            && self.peak_q8 < self.current_max as i32 * 64
            && self.current_max > MIN_SEARCH_PX
        {
            (self.current_max - SEARCH_STEP_PX).max(MIN_SEARCH_PX)
        } else {
            self.current_max
        };
        self.clipped_lines = 0;
        self.total_lines = 0;
        self.peak_q8 = 0;
        if next != self.current_max {
            self.current_max = next;
            return Some(next);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldOrder;

    /// A 4x2 yuv422p frame whose luma samples are all `lo` except
    /// one at `hi` (so min/max are exactly lo/hi).
    fn frame_with_extremes(lo: u8, hi: u8) -> VideoFrame {
        let mut data = vec![lo; 16];
        data[2] = hi;
        VideoFrame {
            data,
            width: 4,
            height: 2,
            format: PixelFormat::Yuv422p,
            field_order: FieldOrder::Progressive,
            pts_ns: 0,
            sequence: 0,
        }
    }

    #[test]
    fn observed_range_accumulates_over_all_frames() {
        let mut est = RangeEstimator::new();
        assert_eq!(est.observed_range(), None);
        let _ = est.observe(&frame_with_extremes(16, 235));
        assert_eq!(est.observed_range(), Some((16, 235)));
        // A later, narrower frame does not shrink the range; a
        // wider one extends it, across window boundaries too.
        let _ = est.observe(&frame_with_extremes(40, 100));
        assert_eq!(est.observed_range(), Some((16, 235)));
        feed(&mut est, &frame_with_extremes(4, 250), 40);
        assert_eq!(est.observed_range(), Some((4, 250)));
    }

    fn feed(est: &mut RangeEstimator, frame: &VideoFrame, n: u32) -> Option<RangeClass> {
        let mut changed = None;
        for _ in 0..n {
            if let Some(c) = est.observe(frame) {
                changed = Some(c);
            }
        }
        changed
    }

    #[test]
    fn range_locks_to_full_after_three_windows() {
        let mut est = RangeEstimator::new();
        let full = frame_with_extremes(0, 255);
        // Two windows are not enough.
        assert_eq!(feed(&mut est, &full, 60), None);
        assert_eq!(est.class(), RangeClass::Limited);
        // The third window flips it, exactly once.
        assert_eq!(feed(&mut est, &full, 30), Some(RangeClass::Full));
        assert_eq!(feed(&mut est, &full, 30), None);
    }

    #[test]
    fn range_returns_to_limited_and_ignores_blips() {
        let mut est = RangeEstimator::new();
        let full = frame_with_extremes(0, 255);
        let limited = frame_with_extremes(16, 235);
        assert_eq!(feed(&mut est, &full, 90), Some(RangeClass::Full));
        // One full-range-blip window inside limited content must not
        // flip anything (streaks reset on opposing votes).
        assert_eq!(feed(&mut est, &limited, 30), None);
        assert_eq!(feed(&mut est, &full, 30), None);
        assert_eq!(feed(&mut est, &limited, 60), None);
        assert_eq!(est.class(), RangeClass::Full);
        // Three consecutive limited windows flip it back.
        assert_eq!(feed(&mut est, &limited, 30), Some(RangeClass::Limited));
    }

    #[test]
    fn limited_content_never_changes_the_default() {
        let mut est = RangeEstimator::new();
        let limited = frame_with_extremes(16, 235);
        assert_eq!(feed(&mut est, &limited, 200), None);
        assert_eq!(est.class(), RangeClass::Limited);
    }

    fn shifts(value_q8: i32, lines: usize) -> Vec<LineShift> {
        (0..lines)
            .map(|i| LineShift {
                line: i as u32,
                shift_q8: value_q8,
            })
            .collect()
    }

    fn feed_shifts(ad: &mut JitterAdapter, shifts: &[LineShift], n: u32) -> Option<u32> {
        let mut changed = None;
        for _ in 0..n {
            if let Some(v) = ad.observe(shifts) {
                changed = Some(v);
            }
        }
        changed
    }

    #[test]
    fn jitter_adapter_widens_on_clipping_with_rate_clamp() {
        let mut ad = JitterAdapter::new(8);
        let clipped = shifts(8 * 256, 16);
        // One window changes the range by exactly one step.
        assert_eq!(feed_shifts(&mut ad, &clipped, 30), Some(10));
        // Clipping at the new boundary widens by one more step.
        let clipped10 = shifts(10 * 256, 16);
        assert_eq!(feed_shifts(&mut ad, &clipped10, 30), Some(12));
        // Still clipped at the new boundary? No: shifts of 8px are
        // no longer near the ±12 boundary, and the peak (8) is not
        // under a quarter of 12, so the range now holds.
        assert_eq!(feed_shifts(&mut ad, &clipped, 30), None);
        assert_eq!(ad.current_max(), 12);
    }

    #[test]
    fn jitter_adapter_narrows_on_small_jitter_and_floors() {
        let mut ad = JitterAdapter::new(8);
        let still = shifts(0, 16);
        assert_eq!(feed_shifts(&mut ad, &still, 30), Some(6));
        assert_eq!(feed_shifts(&mut ad, &still, 30), Some(4));
        assert_eq!(feed_shifts(&mut ad, &still, 30), Some(2));
        assert_eq!(feed_shifts(&mut ad, &still, 60), None);
        assert_eq!(ad.current_max(), MIN_SEARCH_PX);
    }

    #[test]
    fn jitter_adapter_holds_inside_the_dead_band() {
        let mut ad = JitterAdapter::new(8);
        // Peak 3px: above 8/4 = 2 (no narrowing), nothing clipped
        // (no widening).
        let mid = shifts(3 * 256, 16);
        assert_eq!(feed_shifts(&mut ad, &mid, 90), None);
        assert_eq!(ad.current_max(), 8);
    }

    #[test]
    fn jitter_adapter_never_exceeds_the_cap() {
        let mut ad = JitterAdapter::new(30);
        let clipped = shifts(30 * 256, 16);
        assert_eq!(feed_shifts(&mut ad, &clipped, 30), Some(32));
        // At the cap, further clipping changes nothing.
        let clipped32 = shifts(32 * 256, 16);
        assert_eq!(feed_shifts(&mut ad, &clipped32, 60), None);
        assert_eq!(ad.current_max(), MAX_SEARCH_PX);
    }
}
