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
