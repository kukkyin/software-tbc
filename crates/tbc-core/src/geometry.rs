//! Frame geometry transforms: cropping and padding (Issue #31).
//!
//! Real sources often deliver frames larger than the active
//! picture (e.g. 720x486 for a 720x480 picture). Cropping extracts
//! the processing window from such a frame; padding places a
//! processed frame onto a larger black canvas for output. Both
//! operate per pixel-format layout class and preserve the frame's
//! metadata. Callers must supply chroma-aligned offsets (even where
//! the format subsamples chroma); the CLI validates them.

use crate::guard::black_frame;
use crate::{PixelFormat, TbcError, VideoFrame};

/// How a format's bytes are laid out for row-wise region copies.
enum Layout {
    /// One interleaved plane; the value is bytes per pixel.
    Packed(usize),
    /// Planar 4:2:0; the value is bytes per sample (1 or 2).
    Planar420(usize),
    /// Planar 4:2:2; the value is bytes per sample (2).
    Planar422(usize),
}

fn layout(format: PixelFormat) -> Layout {
    match format {
        PixelFormat::Yuv422p | PixelFormat::Uyvy | PixelFormat::Yuyv => Layout::Packed(2),
        PixelFormat::Rgb24 => Layout::Packed(3),
        PixelFormat::Yuv420p => Layout::Planar420(1),
        PixelFormat::Yuv420p10 => Layout::Planar420(2),
        PixelFormat::Yuv422p10 => Layout::Planar422(2),
    }
}

/// One plane's region copy; offsets and lengths are in bytes,
/// `rows` counts plane rows.
struct PlaneCopy {
    src_offset: usize,
    dst_offset: usize,
    src_row_bytes: usize,
    dst_row_bytes: usize,
    src_x: usize,
    dst_x: usize,
    src_y: usize,
    dst_y: usize,
    rows: usize,
    row_len: usize,
}

fn copy_region(src: &[u8], dst: &mut [u8], p: &PlaneCopy) {
    for r in 0..p.rows {
        let s = p.src_offset + (p.src_y + r) * p.src_row_bytes + p.src_x;
        let d = p.dst_offset + (p.dst_y + r) * p.dst_row_bytes + p.dst_x;
        dst[d..d + p.row_len].copy_from_slice(&src[s..s + p.row_len]);
    }
}

/// Plane copies moving an `rw x rh` window: read from (`sx`, `sy`)
/// of a `sw x sh` frame, written to (`dx`, `dy`) of a `dw x dh`
/// frame. Chroma coordinates follow each layout's subsampling.
#[allow(clippy::too_many_arguments)]
fn plane_copies(
    format: PixelFormat,
    sw: u32,
    sh: u32,
    sx: u32,
    sy: u32,
    dw: u32,
    dh: u32,
    dx: u32,
    dy: u32,
    rw: u32,
    rh: u32,
) -> Vec<PlaneCopy> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let (sx, sy, dx, dy, rw, rh) = (
        sx as usize,
        sy as usize,
        dx as usize,
        dy as usize,
        rw as usize,
        rh as usize,
    );
    match layout(format) {
        Layout::Packed(bpp) => vec![PlaneCopy {
            src_offset: 0,
            dst_offset: 0,
            src_row_bytes: sw * bpp,
            dst_row_bytes: dw * bpp,
            src_x: sx * bpp,
            dst_x: dx * bpp,
            src_y: sy,
            dst_y: dy,
            rows: rh,
            row_len: rw * bpp,
        }],
        Layout::Planar420(sb) => {
            let mut planes = vec![PlaneCopy {
                src_offset: 0,
                dst_offset: 0,
                src_row_bytes: sw * sb,
                dst_row_bytes: dw * sb,
                src_x: sx * sb,
                dst_x: dx * sb,
                src_y: sy,
                dst_y: dy,
                rows: rh,
                row_len: rw * sb,
            }];
            for i in 0..2 {
                planes.push(PlaneCopy {
                    src_offset: sw * sh * sb + i * (sw / 2) * (sh / 2) * sb,
                    dst_offset: dw * dh * sb + i * (dw / 2) * (dh / 2) * sb,
                    src_row_bytes: (sw / 2) * sb,
                    dst_row_bytes: (dw / 2) * sb,
                    src_x: (sx / 2) * sb,
                    dst_x: (dx / 2) * sb,
                    src_y: sy / 2,
                    dst_y: dy / 2,
                    rows: rh / 2,
                    row_len: (rw / 2) * sb,
                });
            }
            planes
        }
        Layout::Planar422(sb) => {
            let mut planes = vec![PlaneCopy {
                src_offset: 0,
                dst_offset: 0,
                src_row_bytes: sw * sb,
                dst_row_bytes: dw * sb,
                src_x: sx * sb,
                dst_x: dx * sb,
                src_y: sy,
                dst_y: dy,
                rows: rh,
                row_len: rw * sb,
            }];
            for i in 0..2 {
                planes.push(PlaneCopy {
                    src_offset: sw * sh * sb + i * (sw / 2) * sh * sb,
                    dst_offset: dw * dh * sb + i * (dw / 2) * dh * sb,
                    src_row_bytes: (sw / 2) * sb,
                    dst_row_bytes: (dw / 2) * sb,
                    src_x: (sx / 2) * sb,
                    dst_x: (dx / 2) * sb,
                    src_y: sy,
                    dst_y: dy,
                    rows: rh,
                    row_len: (rw / 2) * sb,
                });
            }
            planes
        }
    }
}

/// Extract the `width x height` window at (`left`, `top`) from
/// `frame`. The window must fit inside the frame.
pub fn crop_frame(
    frame: &VideoFrame,
    left: u32,
    top: u32,
    width: u32,
    height: u32,
) -> Result<VideoFrame, TbcError> {
    if width == 0
        || height == 0
        || left as u64 + width as u64 > frame.width as u64
        || top as u64 + height as u64 > frame.height as u64
    {
        return Err(TbcError::InvalidConfig(format!(
            "crop window at {left},{top} of {width}x{height} does not fit in {}x{}",
            frame.width, frame.height
        )));
    }
    let size = frame.format.frame_size(width, height);
    let mut out = frame.clone();
    out.width = width;
    out.height = height;
    out.data = vec![0u8; size];
    let copies = plane_copies(
        frame.format,
        frame.width,
        frame.height,
        left,
        top,
        width,
        height,
        0,
        0,
        width,
        height,
    );
    for p in &copies {
        copy_region(&frame.data, &mut out.data, p);
    }
    Ok(out)
}

/// Place `frame` onto a black `out_width x out_height` canvas at
/// (`left`, `top`). The frame must fit inside the canvas. The
/// canvas black comes from the guard's black-frame generation, so
/// limited-range and 10-bit formats pad with correct black.
pub fn pad_frame(
    frame: &VideoFrame,
    left: u32,
    top: u32,
    out_width: u32,
    out_height: u32,
) -> Result<VideoFrame, TbcError> {
    if left as u64 + frame.width as u64 > out_width as u64
        || top as u64 + frame.height as u64 > out_height as u64
    {
        return Err(TbcError::InvalidConfig(format!(
            "frame {}x{} at {left},{top} does not fit in output {out_width}x{out_height}",
            frame.width, frame.height
        )));
    }
    let canvas = black_frame(out_width, out_height, frame.format);
    let mut out = frame.clone();
    out.width = out_width;
    out.height = out_height;
    out.data = canvas.data;
    let copies = plane_copies(
        frame.format,
        frame.width,
        frame.height,
        0,
        0,
        out_width,
        out_height,
        left,
        top,
        frame.width,
        frame.height,
    );
    for p in &copies {
        copy_region(&frame.data, &mut out.data, p);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldOrder;

    fn frame(format: PixelFormat, data: Vec<u8>, width: u32, height: u32) -> VideoFrame {
        VideoFrame {
            data,
            width,
            height,
            format,
            field_order: FieldOrder::Progressive,
            pts_ns: 7,
            sequence: 3,
        }
    }

    #[test]
    fn crop_packed_422_extracts_the_window() {
        // 6x4 packed, byte value = its index.
        let data: Vec<u8> = (0..48).collect();
        let f = frame(PixelFormat::Yuv422p, data, 6, 4);
        let out = crop_frame(&f, 2, 1, 4, 2).expect("crop");
        assert_eq!((out.width, out.height), (4, 2));
        // Line 1 (bytes 12..24) from x=2 (byte 4), line 2 likewise.
        let mut expected: Vec<u8> = (16..24).collect();
        expected.extend(28..36);
        assert_eq!(out.data, expected);
        // Metadata rides along.
        assert_eq!((out.pts_ns, out.sequence), (7, 3));
    }

    #[test]
    fn crop_420_moves_chroma_at_half_offsets() {
        // 4x4: Y = row*4+col, U = [100,101,102,103], V = [110..114].
        let mut data: Vec<u8> = (0..16).collect();
        data.extend_from_slice(&[100, 101, 102, 103]);
        data.extend_from_slice(&[110, 111, 112, 113]);
        let f = frame(PixelFormat::Yuv420p, data, 4, 4);
        let out = crop_frame(&f, 2, 2, 2, 2).expect("crop");
        assert_eq!(out.data, vec![10, 11, 14, 15, 103, 113]);
    }

    #[test]
    fn crop_window_out_of_bounds_is_an_error() {
        let f = frame(PixelFormat::Yuv422p, vec![0u8; 48], 6, 4);
        assert!(crop_frame(&f, 4, 0, 4, 2).is_err());
        assert!(crop_frame(&f, 0, 3, 4, 2).is_err());
        assert!(crop_frame(&f, 0, 0, 0, 2).is_err());
    }

    #[test]
    fn pad_packed_places_frame_on_black() {
        let f = frame(PixelFormat::Yuv422p, vec![90u8; 16], 4, 2);
        let out = pad_frame(&f, 2, 1, 8, 4).expect("pad");
        assert_eq!(out.data.len(), 64);
        let black_line: Vec<u8> = [16u8, 128, 16, 128]
            .iter()
            .cycle()
            .take(16)
            .copied()
            .collect();
        assert_eq!(&out.data[0..16], black_line.as_slice());
        assert_eq!(&out.data[48..64], black_line.as_slice());
        // Content line: 2 black px, the 4 source px (all 90), 2 black.
        let mut line = vec![16u8, 128, 16, 128];
        line.extend_from_slice(&[90u8; 8]);
        line.extend_from_slice(&[16, 128, 16, 128]);
        assert_eq!(&out.data[16..32], line.as_slice());
        assert_eq!(&out.data[32..48], line.as_slice());
    }

    #[test]
    fn pad_420_offsets_chroma_planes() {
        // 2x2 all-100 Y, U = [60], V = [70]; pad to 4x4 at (2, 2).
        let f = frame(PixelFormat::Yuv420p, vec![100, 100, 100, 100, 60, 70], 2, 2);
        let out = pad_frame(&f, 2, 2, 4, 4).expect("pad");
        // Y plane: black (16) except the bottom-right 2x2.
        let mut y = vec![16u8; 16];
        y[10] = 100;
        y[11] = 100;
        y[14] = 100;
        y[15] = 100;
        assert_eq!(&out.data[..16], y.as_slice());
        // U plane 2x2: only the bottom-right sample is the source's.
        assert_eq!(&out.data[16..20], [128, 128, 128, 60]);
        assert_eq!(&out.data[20..24], [128, 128, 128, 70]);
    }

    #[test]
    fn pad_then_crop_is_identity() {
        let data: Vec<u8> = (0..32).collect();
        let f = frame(PixelFormat::Yuv422p, data, 4, 4);
        let padded = pad_frame(&f, 2, 2, 8, 8).expect("pad");
        let back = crop_frame(&padded, 2, 2, 4, 4).expect("crop");
        assert_eq!(back.data, f.data);
        assert_eq!((back.width, back.height), (4, 4));
    }

    #[test]
    fn pad_frame_out_of_bounds_is_an_error() {
        let f = frame(PixelFormat::Yuv422p, vec![0u8; 16], 4, 2);
        assert!(pad_frame(&f, 6, 0, 8, 4).is_err());
        assert!(pad_frame(&f, 0, 3, 8, 4).is_err());
    }
}
