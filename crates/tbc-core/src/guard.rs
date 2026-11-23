//! Signal-loss guard: detects an unusable input and substitutes a safe
//! output without ever stalling the output clock. Design: SPEC.md §4.5.

use crate::{FieldOrder, PixelFormat, SignalLossPolicy, VideoFrame};

/// Events emitted on signal-loss transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardEvent {
    SignalLost,
    SignalRecovered,
}

/// Stateful guard. The pipeline feeds it observations; it tracks the
/// lost/recovered state and produces fallback frames while lost.
#[derive(Debug)]
pub struct SignalGuard {
    policy: SignalLossPolicy,
    timeout_ms: u64,
    lost: bool,
    bad_streak: u32,
    good_streak: u32,
    last_good: Option<VideoFrame>,
}

impl SignalGuard {
    pub fn new(policy: SignalLossPolicy, timeout_ms: u64) -> Self {
        SignalGuard {
            policy,
            timeout_ms,
            lost: false,
            bad_streak: 0,
            good_streak: 0,
            last_good: None,
        }
    }

    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// A frame passed validation and was output normally.
    /// Returns `SignalRecovered` on the transition back from lost,
    /// which requires two consecutive good frames.
    pub fn note_good_frame(&mut self, frame: &VideoFrame) -> Option<GuardEvent> {
        self.last_good = Some(frame.clone());
        self.bad_streak = 0;
        if self.lost {
            self.good_streak += 1;
            if self.good_streak >= 2 {
                self.lost = false;
                self.good_streak = 0;
                return Some(GuardEvent::SignalRecovered);
            }
        }
        None
    }

    /// A frame failed validation. Three in a row mean signal loss.
    pub fn note_bad_frame(&mut self) -> Option<GuardEvent> {
        self.good_streak = 0;
        self.bad_streak += 1;
        if !self.lost && self.bad_streak >= 3 {
            self.lost = true;
            return Some(GuardEvent::SignalLost);
        }
        None
    }

    /// The output has been holding the same frame for `hold_ms`
    /// milliseconds because nothing new arrived.
    pub fn note_hold_duration(&mut self, hold_ms: u64) -> Option<GuardEvent> {
        if !self.lost && hold_ms > self.timeout_ms {
            self.lost = true;
            self.good_streak = 0;
            return Some(GuardEvent::SignalLost);
        }
        None
    }

    /// The frame to emit while the signal is lost, per policy.
    /// `None` only when the policy is LastFrameHold and no good frame
    /// has ever been seen.
    pub fn fallback_frame(
        &self,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Option<VideoFrame> {
        let frame = match self.policy {
            SignalLossPolicy::LastFrameHold => self.last_good.clone()?,
            SignalLossPolicy::Blackout => black_frame(width, height, format),
            SignalLossPolicy::TestPattern => test_pattern(width, height, format),
        };
        Some(frame)
    }
}

fn blank_frame(width: u32, height: u32, format: PixelFormat) -> VideoFrame {
    VideoFrame {
        width,
        height,
        format,
        field_order: FieldOrder::Progressive,
        pts_ns: 0,
        sequence: 0,
        data: vec![0u8; format.frame_size(width, height)],
    }
}

/// Broadcast black: Y at 16, chroma neutral (RGB: all zero).
pub fn black_frame(width: u32, height: u32, format: PixelFormat) -> VideoFrame {
    let mut frame = blank_frame(width, height, format);
    match format {
        PixelFormat::Yuv422p => {
            for quad in frame.data.chunks_exact_mut(4) {
                quad[0] = 16;
                quad[1] = 128;
                quad[2] = 16;
                quad[3] = 128;
            }
        }
        PixelFormat::Yuv420p => {
            let y_size = (width * height) as usize;
            for b in &mut frame.data[..y_size] {
                *b = 16;
            }
            for b in &mut frame.data[y_size..] {
                *b = 128;
            }
        }
        PixelFormat::Rgb24 => {}
    }
    frame
}

/// Seven vertical colour bars (white, yellow, cyan, green, magenta,
/// red, blue) in BT.601 limited-range values.
const BARS_YUV: [(u8, u8, u8); 7] = [
    (235, 128, 128), // white
    (210, 16, 146),  // yellow
    (170, 166, 16),  // cyan
    (145, 54, 34),   // green
    (106, 202, 222), // magenta
    (81, 90, 240),   // red
    (41, 240, 110),  // blue
];

const BARS_RGB: [(u8, u8, u8); 7] = [
    (255, 255, 255),
    (255, 255, 0),
    (0, 255, 255),
    (0, 255, 0),
    (255, 0, 255),
    (255, 0, 0),
    (0, 0, 255),
];

fn bar_index(x: u32, width: u32) -> usize {
    ((x as usize * 7) / width.max(1) as usize).min(6)
}

pub fn test_pattern(width: u32, height: u32, format: PixelFormat) -> VideoFrame {
    let mut frame = blank_frame(width, height, format);
    match format {
        PixelFormat::Yuv422p => {
            for line in 0..height as usize {
                let start = line * width as usize * 2;
                for x in 0..width {
                    let (y, cb, cr) = BARS_YUV[bar_index(x, width)];
                    let o = start + x as usize * 2;
                    frame.data[o] = y;
                    if x % 2 == 0 {
                        frame.data[o + 1] = cb;
                    } else {
                        frame.data[o + 1] = cr;
                    }
                }
            }
        }
        PixelFormat::Yuv420p => {
            let y_size = (width * height) as usize;
            let c_size = (width * height / 4) as usize;
            let (y_plane, chroma) = frame.data.split_at_mut(y_size);
            for (i, px) in y_plane.iter_mut().enumerate() {
                let x = (i as u32) % width;
                *px = BARS_YUV[bar_index(x, width)].0;
            }
            let (cb_plane, cr_plane) = chroma.split_at_mut(c_size);
            let c_width = (width / 2).max(1) as usize;
            for (plane, is_cr) in [(cb_plane, false), (cr_plane, true)] {
                for (i, px) in plane.iter_mut().enumerate() {
                    let x = ((i % c_width) * 2) as u32;
                    let (_, cb, cr) = BARS_YUV[bar_index(x, width)];
                    *px = if is_cr { cr } else { cb };
                }
            }
        }
        PixelFormat::Rgb24 => {
            for px in 0..(width * height) {
                let x = px % width;
                let (r, g, b) = BARS_RGB[bar_index(x, width)];
                let o = px as usize * 3;
                frame.data[o] = r;
                frame.data[o + 1] = g;
                frame.data[o + 2] = b;
            }
        }
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_frame() -> VideoFrame {
        let mut frame = black_frame(4, 2, PixelFormat::Yuv422p);
        frame.sequence = 1;
        frame
    }

    #[test]
    fn hold_timeout_declares_loss() {
        let mut guard = SignalGuard::new(SignalLossPolicy::Blackout, 200);
        assert_eq!(guard.note_hold_duration(150), None);
        assert!(!guard.is_lost());
        assert_eq!(guard.note_hold_duration(250), Some(GuardEvent::SignalLost));
        assert!(guard.is_lost());
    }

    #[test]
    fn three_bad_frames_declare_loss() {
        let mut guard = SignalGuard::new(SignalLossPolicy::Blackout, 200);
        assert_eq!(guard.note_bad_frame(), None);
        assert_eq!(guard.note_bad_frame(), None);
        assert_eq!(guard.note_bad_frame(), Some(GuardEvent::SignalLost));
    }

    #[test]
    fn recovery_needs_two_good_frames() {
        let mut guard = SignalGuard::new(SignalLossPolicy::Blackout, 200);
        guard.note_hold_duration(999);
        assert!(guard.is_lost());
        assert_eq!(guard.note_good_frame(&good_frame()), None);
        assert!(guard.is_lost());
        assert_eq!(
            guard.note_good_frame(&good_frame()),
            Some(GuardEvent::SignalRecovered)
        );
        assert!(!guard.is_lost());
    }

    #[test]
    fn fallback_frames_match_format_size() {
        for policy in [SignalLossPolicy::Blackout, SignalLossPolicy::TestPattern] {
            let guard = SignalGuard::new(policy, 200);
            let frame = guard
                .fallback_frame(720, 480, PixelFormat::Yuv422p)
                .expect("fallback");
            assert_eq!(frame.data.len(), PixelFormat::Yuv422p.frame_size(720, 480));
            assert!(frame.validate().is_ok());
        }
    }

    #[test]
    fn last_frame_hold_returns_the_last_good_frame() {
        let mut guard = SignalGuard::new(SignalLossPolicy::LastFrameHold, 200);
        // Nothing seen yet: no fallback available.
        assert!(guard.fallback_frame(4, 2, PixelFormat::Yuv422p).is_none());
        guard.note_good_frame(&good_frame());
        let frame = guard
            .fallback_frame(4, 2, PixelFormat::Yuv422p)
            .expect("fallback");
        assert_eq!(frame.sequence, 1);
    }

    #[test]
    fn test_pattern_has_seven_distinct_bars() {
        let frame = test_pattern(70, 2, PixelFormat::Rgb24);
        let first_pixels: Vec<_> = (0..7)
            .map(|i| {
                let o = i * 10 * 3;
                (frame.data[o], frame.data[o + 1], frame.data[o + 2])
            })
            .collect();
        let mut dedup = first_pixels.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), 7);
    }
}
