//! Pre-allocated frame buffer pool (SPEC.md §5).
//!
//! Buffers are allocated once at start-up; the hot path only borrows and
//! returns them. When the pool is exhausted, [`FramePool::acquire`] returns
//! `None` and counts the event instead of allocating behind the caller's
//! back — running out of buffers is a tuning problem, and telemetry should
//! say so.

use crate::PixelFormat;

/// A frame-sized byte buffer borrowed from a [`FramePool`].
#[derive(Debug)]
pub struct PooledBuffer {
    data: Vec<u8>,
}

impl PooledBuffer {
    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Fill the whole buffer with one byte value.
    pub fn fill(&mut self, value: u8) {
        self.data.fill(value);
    }
}

/// Fixed-size pool of frame buffers for one geometry/format.
#[derive(Debug)]
pub struct FramePool {
    free: Vec<Vec<u8>>,
    frame_size: usize,
    exhausted_count: u64,
}

impl FramePool {
    /// Pre-allocate `count` buffers of the size implied by the geometry.
    pub fn new(format: PixelFormat, width: u32, height: u32, count: usize) -> Self {
        let frame_size = format.frame_size(width, height);
        let free = (0..count).map(|_| vec![0u8; frame_size]).collect();
        FramePool {
            free,
            frame_size,
            exhausted_count: 0,
        }
    }

    /// Borrow a buffer, or `None` (counted) when the pool is empty.
    pub fn acquire(&mut self) -> Option<PooledBuffer> {
        match self.free.pop() {
            Some(data) => Some(PooledBuffer { data }),
            None => {
                self.exhausted_count += 1;
                None
            }
        }
    }

    /// Return a buffer to the pool. Buffers of a foreign size are dropped
    /// rather than pooled, so a size bug cannot corrupt later frames.
    pub fn release(&mut self, buffer: PooledBuffer) {
        if buffer.data.len() == self.frame_size {
            self.free.push(buffer.data);
        }
    }

    /// Buffers currently available to borrow.
    pub fn available(&self) -> usize {
        self.free.len()
    }

    /// Size of every buffer in this pool, in bytes.
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// How many times `acquire` found the pool empty (telemetry).
    pub fn exhausted_count(&self) -> u64 {
        self.exhausted_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(count: usize) -> FramePool {
        FramePool::new(PixelFormat::Yuv422p, 720, 480, count)
    }

    #[test]
    fn acquire_up_to_capacity_then_none_and_counted() {
        let mut pool = pool(2);
        assert!(pool.acquire().is_some());
        assert!(pool.acquire().is_some());
        assert!(pool.acquire().is_none());
        assert_eq!(pool.exhausted_count(), 1);
        assert!(pool.acquire().is_none());
        assert_eq!(pool.exhausted_count(), 2);
    }

    #[test]
    fn released_buffer_can_be_acquired_again() {
        let mut pool = pool(1);
        let buf = pool.acquire().expect("first acquire");
        assert!(pool.acquire().is_none());
        pool.release(buf);
        assert_eq!(pool.available(), 1);
        assert!(pool.acquire().is_some());
    }

    #[test]
    fn buffer_size_matches_format_frame_size() {
        let mut pool = pool(1);
        let buf = pool.acquire().expect("acquire");
        assert_eq!(buf.len(), PixelFormat::Yuv422p.frame_size(720, 480));
        assert_eq!(pool.frame_size(), buf.len());
    }

    #[test]
    fn foreign_sized_buffer_is_not_pooled() {
        let mut pool = pool(1);
        let foreign = PooledBuffer { data: vec![0u8; 8] };
        pool.release(foreign);
        assert_eq!(pool.available(), 1);
    }
}
