//! Frame sources and sinks for software-tbc.
//!
//! The pipeline talks to the outside world only through [`FrameSource`]
//! and [`FrameSink`], so file/pipe I/O today and Windows capture devices
//! tomorrow share one abstraction. See SPEC.md §6.

use std::io::{Read, Write};
use tbc_core::{PixelFormat, TbcError, VideoFrame};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IoError {
    #[error(transparent)]
    Core(#[from] TbcError),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected end of stream mid-frame")]
    TruncatedFrame,
}

/// A producer of frames: a raw file, a pipe from FFmpeg, or a capture device.
pub trait FrameSource {
    /// Read the next frame, or `Ok(None)` at end of stream.
    fn next_frame(&mut self) -> Result<Option<VideoFrame>, IoError>;
}

/// A consumer of frames: a raw file, a pipe to FFmpeg, or an output device.
pub trait FrameSink {
    fn write_frame(&mut self, frame: &VideoFrame) -> Result<(), IoError>;
    fn flush(&mut self) -> Result<(), IoError>;
}

/// Geometry shared by raw sources/sinks (raw streams carry no headers).
#[derive(Debug, Clone, Copy)]
pub struct RawGeometry {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

/// Reads headerless raw frames from any [`Read`] (file or stdin pipe).
pub struct RawReader<R: Read> {
    inner: R,
    geometry: RawGeometry,
    sequence: u64,
}

impl<R: Read> RawReader<R> {
    pub fn new(inner: R, geometry: RawGeometry) -> Self {
        RawReader {
            inner,
            geometry,
            sequence: 0,
        }
    }
}

impl<R: Read> FrameSource for RawReader<R> {
    fn next_frame(&mut self) -> Result<Option<VideoFrame>, IoError> {
        let size = self
            .geometry
            .format
            .frame_size(self.geometry.width, self.geometry.height);
        let mut data = vec![0u8; size];
        // Distinguish a clean EOF on a frame boundary (Ok(None)) from an
        // EOF in the middle of a frame (TruncatedFrame).
        let mut filled = 0usize;
        while filled < size {
            match self.inner.read(&mut data[filled..]) {
                Ok(0) => {
                    return if filled == 0 {
                        Ok(None)
                    } else {
                        Err(IoError::TruncatedFrame)
                    };
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(IoError::Io(e)),
            }
        }
        let frame = VideoFrame {
            width: self.geometry.width,
            height: self.geometry.height,
            format: self.geometry.format,
            field_order: tbc_core::FieldOrder::Progressive,
            pts_ns: 0,
            sequence: self.sequence,
            data,
        };
        self.sequence += 1;
        Ok(Some(frame))
    }
}

/// Writes headerless raw frames to any [`Write`] (file or stdout pipe).
pub struct RawWriter<W: Write> {
    inner: W,
}

impl<W: Write> RawWriter<W> {
    pub fn new(inner: W) -> Self {
        RawWriter { inner }
    }
}

impl<W: Write> FrameSink for RawWriter<W> {
    fn write_frame(&mut self, frame: &VideoFrame) -> Result<(), IoError> {
        frame.validate()?;
        self.inner.write_all(&frame.data)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        self.inner.flush()?;
        Ok(())
    }
}

/// Format a sequence file name: supports `%d` and zero-padded `%0Nd`
/// conversions in `pattern`, e.g. `frame_%06d.raw`.
fn sequence_filename(pattern: &str, index: u64) -> String {
    if let Some(pos) = pattern.find('%') {
        let rest = &pattern[pos + 1..];
        if let Some(dpos) = rest.find('d') {
            let spec = &rest[..dpos];
            let formatted = if let Some(width) = spec.strip_prefix('0') {
                let w: usize = width.parse().unwrap_or(0);
                format!("{:0width$}", index, width = w)
            } else {
                format!("{index}")
            };
            return format!(
                "{}{}{}",
                &pattern[..pos],
                formatted,
                &pattern[pos + 1 + dpos + 1..]
            );
        }
    }
    pattern.to_string()
}

/// Reads raw frames from a numbered file sequence (`frame_%06d.raw`).
/// The sequence ends at the first missing index.
pub struct SequenceSource {
    directory: std::path::PathBuf,
    pattern: String,
    geometry: RawGeometry,
    next_index: u64,
    sequence: u64,
}

impl SequenceSource {
    pub fn new(
        directory: impl Into<std::path::PathBuf>,
        pattern: impl Into<String>,
        geometry: RawGeometry,
        start_index: u64,
    ) -> Self {
        SequenceSource {
            directory: directory.into(),
            pattern: pattern.into(),
            geometry,
            next_index: start_index,
            sequence: 0,
        }
    }
}

impl FrameSource for SequenceSource {
    fn next_frame(&mut self) -> Result<Option<VideoFrame>, IoError> {
        let path = self
            .directory
            .join(sequence_filename(&self.pattern, self.next_index));
        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(IoError::Io(e)),
        };
        let expected = self
            .geometry
            .format
            .frame_size(self.geometry.width, self.geometry.height);
        if data.len() < expected {
            return Err(IoError::TruncatedFrame);
        }
        if data.len() > expected {
            return Err(IoError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}: {} bytes, expected {expected}",
                    path.display(),
                    data.len()
                ),
            )));
        }
        self.next_index += 1;
        let frame = VideoFrame {
            width: self.geometry.width,
            height: self.geometry.height,
            format: self.geometry.format,
            field_order: tbc_core::FieldOrder::Progressive,
            pts_ns: 0,
            sequence: self.sequence,
            data,
        };
        self.sequence += 1;
        Ok(Some(frame))
    }
}

/// Writes raw frames as a numbered file sequence (`frame_%06d.raw`),
/// creating the target directory on first write if needed.
pub struct SequenceSink {
    directory: std::path::PathBuf,
    pattern: String,
    next_index: u64,
    dir_ready: bool,
}

impl SequenceSink {
    pub fn new(
        directory: impl Into<std::path::PathBuf>,
        pattern: impl Into<String>,
        start_index: u64,
    ) -> Self {
        SequenceSink {
            directory: directory.into(),
            pattern: pattern.into(),
            next_index: start_index,
            dir_ready: false,
        }
    }
}

impl FrameSink for SequenceSink {
    fn write_frame(&mut self, frame: &VideoFrame) -> Result<(), IoError> {
        frame.validate()?;
        if !self.dir_ready {
            std::fs::create_dir_all(&self.directory)?;
            self.dir_ready = true;
        }
        let path = self
            .directory
            .join(sequence_filename(&self.pattern, self.next_index));
        std::fs::write(&path, &frame.data)?;
        self.next_index += 1;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("software-tbc-io-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn geometry() -> RawGeometry {
        RawGeometry {
            width: 4,
            height: 2,
            format: PixelFormat::Rgb24,
        }
    }

    fn frame(sequence: u64, fill: u8) -> VideoFrame {
        VideoFrame {
            width: 4,
            height: 2,
            format: PixelFormat::Rgb24,
            field_order: tbc_core::FieldOrder::Progressive,
            pts_ns: 0,
            sequence,
            data: vec![fill; PixelFormat::Rgb24.frame_size(4, 2)],
        }
    }

    #[test]
    fn filename_formatting() {
        assert_eq!(sequence_filename("frame_%06d.raw", 42), "frame_000042.raw");
        assert_eq!(sequence_filename("frame_%d.raw", 7), "frame_7.raw");
        assert_eq!(sequence_filename("plain.raw", 3), "plain.raw");
    }

    #[test]
    fn sequence_round_trip() {
        let dir = temp_dir("roundtrip");
        let mut sink = SequenceSink::new(&dir, "frame_%06d.raw", 0);
        for i in 0..3u8 {
            sink.write_frame(&frame(i as u64, i * 10)).expect("write");
        }
        sink.flush().expect("flush");

        let mut source = SequenceSource::new(&dir, "frame_%06d.raw", geometry(), 0);
        for i in 0..3u8 {
            let f = source.next_frame().expect("read").expect("frame");
            assert_eq!(f.sequence, i as u64);
            assert!(f.data.iter().all(|&b| b == i * 10));
        }
        assert!(source.next_frame().expect("eof").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sequence_stops_at_gap() {
        let dir = temp_dir("gap");
        let mut sink = SequenceSink::new(&dir, "frame_%06d.raw", 0);
        sink.write_frame(&frame(0, 1)).expect("write 0");
        sink.write_frame(&frame(1, 2)).expect("write 1");
        // Skip index 2, write index 3 manually: the reader must stop at 2.
        std::fs::write(dir.join("frame_000003.raw"), vec![3u8; 24]).expect("write 3");

        let mut source = SequenceSource::new(&dir, "frame_%06d.raw", geometry(), 0);
        assert!(source.next_frame().expect("0").is_some());
        assert!(source.next_frame().expect("1").is_some());
        assert!(source.next_frame().expect("gap").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sequence_rejects_wrong_sized_file() {
        let dir = temp_dir("badsize");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("frame_000000.raw"), vec![0u8; 5]).expect("write");
        let mut source = SequenceSource::new(&dir, "frame_%06d.raw", geometry(), 0);
        assert!(matches!(source.next_frame(), Err(IoError::TruncatedFrame)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
