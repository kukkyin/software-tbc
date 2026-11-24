//! Head-switching noise concealment (Issue #29).
//!
//! VHS and similar sources corrupt the bottom few lines of every
//! frame where the video heads switch. This stage conceals those
//! lines from the clean content above them. It runs before jitter
//! correction in the pipeline, so the noise cannot pollute the
//! per-line shift estimates, and the concealed lines inherit
//! roughly the shift of the clean line they were copied from.
//!
//! The stage works on the pipeline's 8-bit formats. Planar 4:2:0
//! is handled plane by plane (the chroma planes get the bottom
//! ceil(N/2) of their lines concealed); packed and RGB layouts are
//! handled row by row. 10-bit boundary formats are left untouched:
//! the pipeline converts them to 8-bit before this stage.

use crate::{PixelFormat, VideoFrame};

/// How the masked lines are synthesised from the clean lines above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeadSwitchMode {
    /// Copy the last clean line over every masked line.
    #[default]
    Replicate,
    /// Each masked line is the per-sample average of the two output
    /// lines above it, recursively: a smooth vertical smear with no
    /// hard edge at the mask boundary.
    Interpolate,
}

/// Head-switching concealment settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeadSwitchConfig {
    /// Number of bottom lines to conceal. 0 disables the stage.
    pub lines: u32,
    pub mode: HeadSwitchMode,
}

/// Conceal one plane region: `data` holds `total_lines` lines of
/// `line_bytes` bytes each, and the bottom `mask_lines` of them are
/// replaced. At least one clean line always remains.
fn conceal_region(
    data: &mut [u8],
    line_bytes: usize,
    total_lines: usize,
    mask_lines: usize,
    mode: HeadSwitchMode,
) {
    if mask_lines == 0 || total_lines < 2 || line_bytes == 0 {
        return;
    }
    let mask = mask_lines.min(total_lines - 1);
    let first_masked = total_lines - mask;
    match mode {
        HeadSwitchMode::Replicate => {
            let (head, tail) = data.split_at_mut(first_masked * line_bytes);
            let src = &head[(first_masked - 1) * line_bytes..first_masked * line_bytes];
            for line in tail.chunks_exact_mut(line_bytes) {
                line.copy_from_slice(src);
            }
        }
        HeadSwitchMode::Interpolate => {
            for l in first_masked..total_lines {
                let (head, tail) = data.split_at_mut(l * line_bytes);
                let cur = &mut tail[..line_bytes];
                if l >= 2 {
                    let prev1 = &head[(l - 1) * line_bytes..l * line_bytes];
                    let prev2 = &head[(l - 2) * line_bytes..(l - 1) * line_bytes];
                    for i in 0..line_bytes {
                        cur[i] = (prev1[i] as u16 + prev2[i] as u16).div_ceil(2) as u8;
                    }
                } else {
                    // Only one clean line above: replicate it.
                    cur.copy_from_slice(&head[..line_bytes]);
                }
            }
        }
    }
}

/// Apply head-switching concealment to `frame` in place. Does
/// nothing when `cfg.lines` is 0 or the format is a 10-bit boundary
/// format.
pub fn apply_head_switch(frame: &mut VideoFrame, cfg: &HeadSwitchConfig) {
    if cfg.lines == 0 || frame.height < 2 {
        return;
    }
    let w = frame.width as usize;
    let h = frame.height as usize;
    let lines = cfg.lines as usize;
    match frame.format {
        PixelFormat::Yuv420p => {
            let y_size = w * h;
            let c_size = y_size / 4;
            let (y_plane, rest) = frame.data.split_at_mut(y_size);
            let (u_plane, v_plane) = rest.split_at_mut(c_size);
            conceal_region(y_plane, w, h, lines, cfg.mode);
            // Chroma is vertically subsampled: conceal the bottom
            // ceil(N/2) chroma lines.
            let chroma_lines = lines.div_ceil(2);
            conceal_region(u_plane, w / 2, h / 2, chroma_lines, cfg.mode);
            conceal_region(v_plane, w / 2, h / 2, chroma_lines, cfg.mode);
        }
        PixelFormat::Nv12 => {
            let y_size = w * h;
            let (y_plane, uv_plane) = frame.data.split_at_mut(y_size);
            conceal_region(y_plane, w, h, lines, cfg.mode);
            // The interleaved chroma plane has half as many rows of
            // full width; conceal its bottom ceil(N/2) rows.
            let chroma_lines = lines.div_ceil(2);
            conceal_region(uv_plane, w, h / 2, chroma_lines, cfg.mode);
        }
        PixelFormat::Yuv422p | PixelFormat::Uyvy | PixelFormat::Yuyv => {
            conceal_region(&mut frame.data, w * 2, h, lines, cfg.mode);
        }
        PixelFormat::Rgb24 => {
            conceal_region(&mut frame.data, w * 3, h, lines, cfg.mode);
        }
        PixelFormat::Yuv420p10 | PixelFormat::Yuv422p10 => {
            // Boundary formats are converted to 8-bit before this
            // stage in the pipeline; leave direct-API frames alone.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldOrder;

    fn frame(format: PixelFormat, width: u32, height: u32, data: Vec<u8>) -> VideoFrame {
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

    /// A 4x4 yuv422p frame whose line `i` is filled with byte `i*10`.
    fn lined_422() -> VideoFrame {
        let mut data = Vec::new();
        for line in 0..4u8 {
            data.extend_from_slice(&[line * 10; 8]);
        }
        frame(PixelFormat::Yuv422p, 4, 4, data)
    }

    fn line_bytes(f: &VideoFrame, line: usize, bytes: usize) -> &[u8] {
        &f.data[line * bytes..(line + 1) * bytes]
    }

    #[test]
    fn replicate_copies_the_last_clean_line() {
        let mut f = lined_422();
        apply_head_switch(
            &mut f,
            &HeadSwitchConfig {
                lines: 2,
                mode: HeadSwitchMode::Replicate,
            },
        );
        assert_eq!(line_bytes(&f, 0, 8), [0; 8]);
        assert_eq!(line_bytes(&f, 1, 8), [10; 8]);
        assert_eq!(line_bytes(&f, 2, 8), [10; 8]);
        assert_eq!(line_bytes(&f, 3, 8), [10; 8]);
    }

    #[test]
    fn interpolate_ramps_from_the_lines_above() {
        // Lines are 0, 10, 20, 30; masking the bottom two gives
        // out2 = (10 + 0 + 1) / 2 = 5, out3 = (5 + 10 + 1) / 2 = 8.
        let mut f = lined_422();
        apply_head_switch(
            &mut f,
            &HeadSwitchConfig {
                lines: 2,
                mode: HeadSwitchMode::Interpolate,
            },
        );
        assert_eq!(line_bytes(&f, 0, 8), [0; 8]);
        assert_eq!(line_bytes(&f, 1, 8), [10; 8]);
        assert_eq!(line_bytes(&f, 2, 8), [5; 8]);
        assert_eq!(line_bytes(&f, 3, 8), [8; 8]);
    }

    #[test]
    fn zero_lines_is_identity() {
        let mut f = lined_422();
        let before = f.data.clone();
        apply_head_switch(&mut f, &HeadSwitchConfig::default());
        assert_eq!(f.data, before);
    }

    #[test]
    fn masking_all_but_one_line_replicates_line_zero() {
        // Boundary case: lines = height - 1 leaves a single clean line.
        let mut f = lined_422();
        apply_head_switch(
            &mut f,
            &HeadSwitchConfig {
                lines: 3,
                mode: HeadSwitchMode::Replicate,
            },
        );
        for line in 0..4 {
            assert_eq!(line_bytes(&f, line, 8), [0; 8], "line {line}");
        }
    }

    #[test]
    fn yuv420p_conceals_chroma_planes_too() {
        // 4x4: Y lines are 0,10,20,30; U and V planes have two
        // lines each (100,110 and 200,210). Masking 2 luma lines
        // conceals ceil(2/2) = 1 chroma line.
        let mut data = Vec::new();
        for line in 0..4u8 {
            data.extend_from_slice(&[line * 10; 4]);
        }
        data.extend_from_slice(&[100; 2]);
        data.extend_from_slice(&[110; 2]);
        data.extend_from_slice(&[200; 2]);
        data.extend_from_slice(&[210; 2]);
        let mut f = frame(PixelFormat::Yuv420p, 4, 4, data);
        apply_head_switch(
            &mut f,
            &HeadSwitchConfig {
                lines: 2,
                mode: HeadSwitchMode::Replicate,
            },
        );
        assert_eq!(&f.data[0..4], [0; 4]);
        assert_eq!(&f.data[4..8], [10; 4]);
        assert_eq!(&f.data[8..12], [10; 4]);
        assert_eq!(&f.data[12..16], [10; 4]);
        assert_eq!(&f.data[16..20], [100, 100, 100, 100]);
        assert_eq!(&f.data[20..24], [200, 200, 200, 200]);
    }

    #[test]
    fn rgb24_replicates_whole_lines() {
        let mut data = Vec::new();
        for line in 0..3u8 {
            data.extend_from_slice(&[line * 10; 12]);
        }
        let mut f = frame(PixelFormat::Rgb24, 4, 3, data);
        apply_head_switch(
            &mut f,
            &HeadSwitchConfig {
                lines: 1,
                mode: HeadSwitchMode::Replicate,
            },
        );
        assert_eq!(line_bytes(&f, 2, 12), [10; 12]);
    }
}
