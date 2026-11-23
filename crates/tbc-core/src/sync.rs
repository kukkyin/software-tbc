//! Frame synchronisation and rate alignment.
//!
//! Design: SPEC.md §4.1. Implementation is tracked in the Phase 2 issues;
//! this module currently exposes only the intended public surface.

use crate::{TbcConfig, VideoFrame};

/// Stateful synchroniser: ingest frames on the input clock, release them
/// on the target-rate output clock according to the configured policy.
pub struct FrameSynchronizer {
    _config: TbcConfig,
}

impl FrameSynchronizer {
    pub fn new(config: TbcConfig) -> Self {
        FrameSynchronizer { _config: config }
    }

    /// Placeholder until the ring buffer lands (Phase 1/2 issues).
    pub fn ingest(&mut self, _frame: VideoFrame) {
        unimplemented!("tracked in the Phase 1 ring-buffer issue")
    }
}

/// Lock-free single-producer / single-consumer ring buffer of frames.
///
/// One producer thread calls [`RingBuffer::push`], one consumer thread
/// calls [`RingBuffer::pop`]; no other sharing pattern is supported.
/// Counters are monotonic and slots are indexed modulo capacity, so the
/// full state is `head - tail == capacity` and empty is `head == tail`.
pub mod ring {
    use crate::VideoFrame;
    use std::cell::UnsafeCell;
    use std::fmt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Error returned by `push` when the ring is full. Carries the frame
    /// back so the caller decides whether to drop or retry it.
    pub struct Full(pub VideoFrame);

    impl fmt::Debug for Full {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_tuple("Full").field(&self.0.sequence).finish()
        }
    }

    pub struct RingBuffer {
        slots: Box<[UnsafeCell<Option<VideoFrame>>]>,
        /// Next write position. Written only by the producer.
        head: AtomicUsize,
        /// Next read position. Written only by the consumer.
        tail: AtomicUsize,
        capacity: usize,
    }

    // SAFETY: in the SPSC contract, each slot is written by the producer
    // before the Release store to `head` that publishes it, and read by
    // the consumer after the Acquire load of `head` that observes it;
    // a slot is never touched by both sides at the same time because the
    // producer only writes slots at distance >= 1 ahead of `tail` and the
    // consumer only reads slots behind `head`.
    unsafe impl Send for RingBuffer {}
    unsafe impl Sync for RingBuffer {}

    impl RingBuffer {
        /// Create a ring holding up to `capacity` frames.
        pub fn new(capacity: usize) -> Self {
            assert!(capacity > 0, "ring capacity must be at least 1");
            let slots = (0..capacity)
                .map(|_| UnsafeCell::new(None))
                .collect::<Vec<_>>()
                .into_boxed_slice();
            RingBuffer {
                slots,
                head: AtomicUsize::new(0),
                tail: AtomicUsize::new(0),
                capacity,
            }
        }

        pub fn capacity(&self) -> usize {
            self.capacity
        }

        /// Number of frames currently in the ring.
        pub fn len(&self) -> usize {
            let head = self.head.load(Ordering::Acquire);
            let tail = self.tail.load(Ordering::Acquire);
            head.wrapping_sub(tail)
        }

        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }

        /// Fill level, 0.0 (empty) to 1.0 (full).
        pub fn fill_ratio(&self) -> f64 {
            self.len() as f64 / self.capacity as f64
        }

        /// Producer side: enqueue a frame. Fails with the frame back when
        /// the ring is full; existing contents are never overwritten.
        pub fn push(&self, frame: VideoFrame) -> Result<(), Full> {
            let head = self.head.load(Ordering::Relaxed);
            let tail = self.tail.load(Ordering::Acquire);
            if head.wrapping_sub(tail) >= self.capacity {
                return Err(Full(frame));
            }
            let idx = head % self.capacity;
            // SAFETY: see the type-level note; this slot is beyond `tail`
            // and not yet published to the consumer.
            unsafe {
                *self.slots[idx].get() = Some(frame);
            }
            self.head.store(head.wrapping_add(1), Ordering::Release);
            Ok(())
        }

        /// Consumer side: dequeue the oldest frame, if any.
        pub fn pop(&self) -> Option<VideoFrame> {
            let tail = self.tail.load(Ordering::Relaxed);
            let head = self.head.load(Ordering::Acquire);
            if head.wrapping_sub(tail) == 0 {
                return None;
            }
            let idx = tail % self.capacity;
            // SAFETY: see the type-level note; this slot was published by
            // the producer and is not written again until it wraps, which
            // requires passing through `tail` first.
            let frame = unsafe { (*self.slots[idx].get()).take() };
            self.tail.store(tail.wrapping_add(1), Ordering::Release);
            frame
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::{FieldOrder, PixelFormat};

        fn frame(sequence: u64) -> VideoFrame {
            VideoFrame {
                width: 4,
                height: 2,
                format: PixelFormat::Rgb24,
                field_order: FieldOrder::Progressive,
                pts_ns: sequence * 33_366_666,
                sequence,
                data: vec![0u8; PixelFormat::Rgb24.frame_size(4, 2)],
            }
        }

        #[test]
        fn fifo_order_is_preserved() {
            let ring = RingBuffer::new(4);
            for i in 0..4 {
                assert!(ring.push(frame(i)).is_ok());
            }
            for i in 0..4 {
                assert_eq!(ring.pop().map(|f| f.sequence), Some(i));
            }
            assert!(ring.pop().is_none());
        }

        #[test]
        fn full_ring_rejects_push_and_keeps_contents() {
            let ring = RingBuffer::new(2);
            assert!(ring.push(frame(0)).is_ok());
            assert!(ring.push(frame(1)).is_ok());
            let rejected = ring.push(frame(2)).expect_err("ring is full");
            assert_eq!(rejected.0.sequence, 2);
            assert_eq!(ring.pop().map(|f| f.sequence), Some(0));
            assert_eq!(ring.pop().map(|f| f.sequence), Some(1));
        }

        #[test]
        fn fill_ratio_tracks_occupancy() {
            let ring = RingBuffer::new(4);
            assert_eq!(ring.fill_ratio(), 0.0);
            assert!(ring.push(frame(0)).is_ok());
            assert!(ring.push(frame(1)).is_ok());
            assert_eq!(ring.fill_ratio(), 0.5);
            assert!(ring.pop().is_some());
            assert_eq!(ring.fill_ratio(), 0.25);
        }

        #[test]
        fn wraps_around_many_times() {
            let ring = RingBuffer::new(3);
            for i in 0..100 {
                assert!(ring.push(frame(i)).is_ok());
                assert_eq!(ring.pop().map(|f| f.sequence), Some(i));
            }
            assert!(ring.is_empty());
        }

        #[test]
        fn spsc_across_threads() {
            use std::sync::Arc;
            let ring = Arc::new(RingBuffer::new(8));
            let producer = {
                let ring = Arc::clone(&ring);
                std::thread::spawn(move || {
                    for i in 0..1_000u64 {
                        let mut f = frame(i);
                        loop {
                            match ring.push(f) {
                                Ok(()) => break,
                                Err(Full(back)) => {
                                    f = back;
                                    std::thread::yield_now();
                                }
                            }
                        }
                    }
                })
            };
            let consumer = {
                let ring = Arc::clone(&ring);
                std::thread::spawn(move || {
                    let mut expected = 0u64;
                    while expected < 1_000 {
                        match ring.pop() {
                            Some(f) => {
                                assert_eq!(f.sequence, expected);
                                expected += 1;
                            }
                            None => std::thread::yield_now(),
                        }
                    }
                })
            };
            producer.join().expect("producer");
            consumer.join().expect("consumer");
        }
    }
}
