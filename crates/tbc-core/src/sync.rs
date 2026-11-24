//! Frame synchronisation and rate alignment.
//!
//! Design: SPEC.md §4.1. Implementation is tracked in the Phase 2 issues;
//! this module currently exposes only the intended public surface.

use crate::{RatePolicy, TbcConfig, VideoFrame};
use std::collections::VecDeque;

/// Stateful synchroniser: ingest frames on the input clock, release them
/// on the target-rate output clock according to the configured policy.
///
/// Output frame *k* is due at `t_k = t_0 + k * (den/num) s`, computed in
/// integer nanoseconds from the rational rate (never floats, so the
/// schedule cannot drift). `t_0` anchors on the first ingested frame;
/// a slow re-anchor (below) compensates input clock drift.
pub struct FrameSynchronizer {
    config: TbcConfig,
    queue: VecDeque<VideoFrame>,
    anchor_ns: Option<u64>,
    /// Cumulative gradual schedule shift from drift re-anchoring.
    anchor_adjust_ns: i64,
    output_index: u64,
    /// The newest frame that is due at the current target time: the one
    /// to hold, emit, or blend from.
    current: Option<VideoFrame>,
    /// Whether `current` has already contributed to an output.
    current_was_used: bool,
    last_emitted_seq: Option<u64>,
    dropped: u64,
    duplicated: u64,
    // Drift estimation: integer EMA of inter-arrival intervals.
    last_ingest_pts: Option<u64>,
    ema_interval_ns: Option<i64>,
    drift_ppm: i64,
}

impl FrameSynchronizer {
    pub fn new(config: TbcConfig) -> Self {
        FrameSynchronizer {
            config,
            queue: VecDeque::new(),
            anchor_ns: None,
            anchor_adjust_ns: 0,
            output_index: 0,
            current: None,
            current_was_used: false,
            last_emitted_seq: None,
            dropped: 0,
            duplicated: 0,
            last_ingest_pts: None,
            ema_interval_ns: None,
            drift_ppm: 0,
        }
    }

    pub fn config(&self) -> &TbcConfig {
        &self.config
    }

    /// Change the rate policy for subsequent outputs (Issue #21's
    /// overload fallback). The anchor, queue and counters are kept:
    /// only how due frames are emitted changes.
    pub fn set_rate_policy(&mut self, policy: RatePolicy) {
        self.config.rate_policy = policy;
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }

    pub fn duplicated_frames(&self) -> u64 {
        self.duplicated
    }

    /// Estimated input clock drift in parts per million, relative to the
    /// target rate (positive = input clock runs fast).
    pub fn drift_ppm_estimate(&self) -> i64 {
        self.drift_ppm
    }

    /// Nominal frame interval of the target rate, in nanoseconds.
    fn nominal_interval_ns(&self) -> i64 {
        (self.config.target_rate.den as i64 * 1_000_000_000) / self.config.target_rate.num as i64
    }

    /// Target time of output frame `k`, in nanoseconds.
    fn target_ns(&self, k: u64) -> u64 {
        let anchor = self.anchor_ns.unwrap_or(0) as i128;
        let rate = self.config.target_rate;
        let elapsed = (k as i128 * rate.den as i128 * 1_000_000_000) / rate.num as i128;
        let t = anchor + elapsed + self.anchor_adjust_ns as i128;
        t.max(0) as u64
    }

    /// Ingest one frame on the input clock. Frames must arrive with
    /// non-decreasing `pts_ns`.
    pub fn ingest(&mut self, frame: VideoFrame) {
        if self.anchor_ns.is_none() {
            self.anchor_ns = Some(frame.pts_ns);
        }
        if let Some(last) = self.last_ingest_pts {
            let interval = frame.pts_ns.saturating_sub(last) as i64;
            let ema = match self.ema_interval_ns {
                None => interval,
                Some(e) => e + (interval - e) / 256,
            };
            self.ema_interval_ns = Some(ema);
            let nominal = self.nominal_interval_ns();
            if nominal > 0 {
                self.drift_ppm = ((ema - nominal) * 1_000_000) / nominal;
            }
        }
        self.last_ingest_pts = Some(frame.pts_ns);
        self.queue.push_back(frame);
    }

    /// Gradual re-anchor: when the estimated drift exceeds ±500 ppm,
    /// shift the schedule by the per-frame error, capped at 1/8 of a
    /// frame interval per output frame (SPEC.md §4.1).
    ///
    /// Estimates beyond ±10 000 ppm (1%) are not clock drift at all but
    /// a genuinely different input rate (rate conversion); the schedule
    /// must not chase those.
    fn reanchor(&mut self) {
        if self.drift_ppm.abs() <= 500 || self.drift_ppm.abs() > 10_000 {
            return;
        }
        let Some(ema) = self.ema_interval_ns else {
            return;
        };
        let error = ema - self.nominal_interval_ns();
        let cap = self.nominal_interval_ns() / 8;
        self.anchor_adjust_ns += error.clamp(-cap, cap);
    }

    /// Produce the next frame on the output clock, or `None` when no
    /// frame has ever become due.
    pub fn next_output(&mut self) -> Option<VideoFrame> {
        self.anchor_ns?;
        self.reanchor();
        let t = self.target_ns(self.output_index);

        // Retire every frame that has become due into `current`; a due
        // frame replaced before it was ever used counts as dropped.
        while let Some(front) = self.queue.front() {
            if front.pts_ns > t {
                break;
            }
            let frame = self.queue.pop_front().expect("front exists");
            if self.current.is_some() && !self.current_was_used {
                self.dropped += 1;
            }
            self.current = Some(frame);
            self.current_was_used = false;
        }

        let current = self.current.as_ref()?;

        // Blend policy: synthesise the frame due at t from the current
        // frame and the next future frame.
        if self.config.rate_policy == RatePolicy::Blend && t > current.pts_ns {
            if let Some(next) = self.queue.front() {
                let mut out = blend_frames(
                    current,
                    next,
                    t - current.pts_ns,
                    next.pts_ns - current.pts_ns,
                );
                out.pts_ns = t;
                self.current_was_used = true;
                self.last_emitted_seq = Some(out.sequence);
                self.output_index += 1;
                return Some(out);
            }
        }

        // Hold/drop policy (and blend with nothing to blend towards):
        // emit the current frame; re-emitting it counts as duplicated.
        let out = current.clone();
        if self.last_emitted_seq == Some(out.sequence) {
            self.duplicated += 1;
        }
        self.current_was_used = true;
        self.last_emitted_seq = Some(out.sequence);
        self.output_index += 1;
        Some(out)
    }
}

/// Temporal blend of two frames: `out = a*(1-w) + b*w` per byte, with
/// `w = w_num / w_den`, in integer arithmetic with rounding.
fn blend_frames(a: &VideoFrame, b: &VideoFrame, w_num: u64, w_den: u64) -> VideoFrame {
    let mut out = a.clone();
    if w_den == 0 || a.data.len() != b.data.len() {
        return out;
    }
    // Reduce the weight to Q16 first: the raw w_num/w_den are timestamp
    // differences and would overflow u32 when multiplied by pixel values.
    let w_q16 = ((w_num.min(w_den) as u128 * 65_536) / w_den as u128) as u32;
    for (o, (x, y)) in out.data.iter_mut().zip(a.data.iter().zip(b.data.iter())) {
        *o = ((*x as u32 * (65_536 - w_q16) + *y as u32 * w_q16 + 32_768) / 65_536) as u8;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldOrder, PixelFormat};

    const DUR: u64 = 33_366_666; // ~29.97 fps, truncated like FrameRate

    fn config(policy: RatePolicy) -> TbcConfig {
        TbcConfig {
            rate_policy: policy,
            ..Default::default()
        }
    }

    fn frame(sequence: u64, pts_ns: u64) -> VideoFrame {
        VideoFrame {
            width: 4,
            height: 2,
            format: PixelFormat::Rgb24,
            field_order: FieldOrder::Progressive,
            pts_ns,
            sequence,
            data: vec![0u8; PixelFormat::Rgb24.frame_size(4, 2)],
        }
    }

    #[test]
    fn matched_rates_pass_every_frame_in_order() {
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        for i in 0..10 {
            sync.ingest(frame(i, i * DUR));
        }
        for i in 0..10 {
            let out = sync.next_output().expect("output");
            assert_eq!(out.sequence, i);
        }
        assert_eq!(sync.dropped_frames(), 0);
        assert_eq!(sync.duplicated_frames(), 0);
    }

    #[test]
    fn double_rate_input_drops_about_half() {
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        for i in 0..20 {
            sync.ingest(frame(i, i * DUR / 2));
        }
        let mut last_seq = 0;
        for _ in 0..10 {
            let out = sync.next_output().expect("output");
            assert!(out.sequence >= last_seq);
            last_seq = out.sequence;
        }
        assert!(
            sync.dropped_frames() >= 8,
            "dropped was {}",
            sync.dropped_frames()
        );
    }

    #[test]
    fn half_rate_input_duplicates_frames() {
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        for i in 0..5 {
            sync.ingest(frame(i, i * 2 * DUR));
        }
        for _ in 0..10 {
            assert!(sync.next_output().is_some());
        }
        assert!(
            sync.duplicated_frames() >= 4,
            "duplicated was {}",
            sync.duplicated_frames()
        );
    }

    #[test]
    fn no_input_no_output() {
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        assert!(sync.next_output().is_none());
    }

    #[test]
    fn drift_estimate_converges_for_fast_input() {
        // Input clock 1000 ppm fast: intervals are 0.1% longer than nominal.
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        let fast_interval = DUR + DUR / 1000;
        for i in 0..600u64 {
            sync.ingest(frame(i, i * fast_interval));
            let _ = sync.next_output();
        }
        let ppm = sync.drift_ppm_estimate();
        assert!((800..=1200).contains(&ppm), "drift estimate was {ppm} ppm");
    }

    #[test]
    fn blend_frames_math() {
        let mut a = frame(0, 0);
        a.data = vec![0u8; 24];
        let mut b = frame(1, DUR);
        b.data = vec![200u8; 24];
        let mid = blend_frames(&a, &b, 1, 2);
        assert!(mid.data.iter().all(|&v| v == 100));
        let at_a = blend_frames(&a, &b, 0, 2);
        assert!(at_a.data.iter().all(|&v| v == 0));
        let at_b = blend_frames(&a, &b, 2, 2);
        assert!(at_b.data.iter().all(|&v| v == 200));
    }

    #[test]
    fn blend_policy_outputs_interpolated_frames() {
        let mut sync = FrameSynchronizer::new(config(RatePolicy::Blend));
        let mut a = frame(0, 0);
        a.data = vec![0u8; 24];
        let mut b = frame(1, 2 * DUR);
        b.data = vec![200u8; 24];
        sync.ingest(a);
        sync.ingest(b);
        let first = sync.next_output().expect("first"); // t=0 -> a itself
        assert!(first.data.iter().all(|&v| v == 0));
        let second = sync.next_output().expect("second"); // t=DUR -> halfway
        assert!(second.data.iter().all(|&v| v == 100));
    }

    #[test]
    fn set_rate_policy_switches_blend_to_hold_drop() {
        let mut sync = FrameSynchronizer::new(config(RatePolicy::Blend));
        let mut a = frame(0, 0);
        a.data = vec![0u8; 24];
        let mut b = frame(1, 2 * DUR);
        b.data = vec![200u8; 24];
        sync.ingest(a);
        sync.ingest(b);
        let first = sync.next_output().expect("first"); // t=0 -> a itself
        assert!(first.data.iter().all(|&v| v == 0));
        sync.set_rate_policy(RatePolicy::HoldDrop);
        assert_eq!(sync.config().rate_policy, RatePolicy::HoldDrop);
        // At t=DUR the blend policy would emit the halfway frame (100);
        // hold/drop re-emits a (b is not due until 2*DUR) instead.
        let second = sync.next_output().expect("second");
        assert!(second.data.iter().all(|&v| v == 0));
        assert_eq!(sync.duplicated_frames(), 1);
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
