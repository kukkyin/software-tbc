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

/// Bob deinterlacing of one field: take every second line (the field's
/// own lines) and synthesise the missing lines by averaging neighbours.
/// Returns a progressive frame of the same height, or `None` when the
/// frame layout cannot be split into fields this way.
pub fn bob_field(frame: &VideoFrame, top_field: bool) -> Option<VideoFrame> {
    if frame.height < 2 || frame.format == PixelFormat::Yuv420p {
        // Yuv420p chroma is shared between line pairs; a naive per-line
        // split would corrupt it. Left for a later, format-aware pass.
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
