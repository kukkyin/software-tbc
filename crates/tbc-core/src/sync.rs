//! Frame synchronisation and rate alignment.
//!
//! Design: SPEC.md §4.1. Implementation is tracked in the Phase 2 issues;
//! this module currently exposes only the intended public surface.

use crate::{RatePolicy, TbcConfig, VideoFrame};
use std::collections::VecDeque;

/// Stability gate for drift re-anchoring (Issue #35).
///
/// Re-anchoring starts only after the drift estimate has stayed
/// beyond the start threshold, with a consistent sign, for
/// `REQUIRED_FRAMES` consecutive output frames, so a transient EMA
/// excursion (arrival jitter, a stall, a single bad interval) never
/// moves the schedule. Once active, the gate stays open down to the
/// lower stop threshold (hysteresis) and closes immediately on a
/// sign flip. Estimates beyond the hard limit are not drift at all
/// (a genuinely different input rate) and reset the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelockState {
    Inactive,
    Qualifying { sign: i64, frames: u32 },
    Active { sign: i64 },
}

#[derive(Debug, Clone, Copy)]
struct RelockGate {
    state: RelockState,
}

impl RelockGate {
    /// Drift magnitude that starts qualification, ppm.
    const START_PPM: i64 = 500;
    /// Drift magnitude that ends an active re-lock, ppm.
    const STOP_PPM: i64 = 350;
    /// Hard limit, ppm: beyond this the gate resets and never opens.
    const MAX_PPM: i64 = 10_000;
    /// Consecutive in-band output frames required to open the gate.
    const REQUIRED_FRAMES: u32 = 30;

    fn new() -> Self {
        RelockGate {
            state: RelockState::Inactive,
        }
    }

    fn is_active(&self) -> bool {
        matches!(self.state, RelockState::Active { .. })
    }

    /// Close the gate unconditionally (input discontinuity, Issue #35).
    fn reset(&mut self) {
        self.state = RelockState::Inactive;
    }

    /// Feed the current drift estimate (once per output frame);
    /// returns whether re-anchoring is allowed for this frame.
    fn update(&mut self, drift_ppm: i64) -> bool {
        let magnitude = drift_ppm.saturating_abs();
        if magnitude > Self::MAX_PPM {
            self.state = RelockState::Inactive;
            return false;
        }
        let sign = drift_ppm.signum();
        match self.state {
            RelockState::Inactive => {
                if magnitude > Self::START_PPM {
                    self.state = RelockState::Qualifying { sign, frames: 1 };
                }
                false
            }
            RelockState::Qualifying {
                sign: prev_sign,
                frames,
            } => {
                if sign != prev_sign || magnitude <= Self::START_PPM {
                    self.state = RelockState::Inactive;
                    return false;
                }
                let next = frames.saturating_add(1);
                if next >= Self::REQUIRED_FRAMES {
                    self.state = RelockState::Active { sign };
                    true
                } else {
                    self.state = RelockState::Qualifying { sign, frames: next };
                    false
                }
            }
            RelockState::Active { sign: prev_sign } => {
                if sign != prev_sign || magnitude <= Self::STOP_PPM {
                    self.state = RelockState::Inactive;
                    return false;
                }
                true
            }
        }
    }
}

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
    /// Remainder carried across EMA updates (Issue #35): without it,
    /// errors smaller than the 1/256 step are truncated away forever
    /// and the estimate keeps a systematic bias over long runs.
    ema_remainder_ns: i64,
    drift_ppm: i64,
    /// Stability gate deciding when re-anchoring may run (Issue #35).
    relock: RelockGate,
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
            ema_remainder_ns: 0,
            drift_ppm: 0,
            relock: RelockGate::new(),
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
            let nominal = self.nominal_interval_ns();
            // Outlier rejection (Issue #35): an interval far outside
            // the plausible drift band is a gap or discontinuity
            // (signal loss, a stall, a seek), not clock drift. Feeding
            // it to the EMA would spike the estimate and, on recovery,
            // drag the schedule with it; the frame itself is still
            // queued normally.
            let plausible =
                nominal > 0 && interval >= nominal / 4 && interval <= nominal.saturating_mul(4);
            if plausible {
                // The EMA seeds at the nominal interval, not at the
                // first observed one (Issue #35): a single noisy first
                // interval would otherwise read as several thousand
                // ppm of phantom drift and decay through the gate's
                // band for hundreds of frames.
                let start = self.ema_interval_ns.unwrap_or(nominal);
                // Carry the division remainder across updates so
                // sub-step errors are not truncated away (Issue #35).
                let numerator = (interval - start) + self.ema_remainder_ns;
                self.ema_remainder_ns = numerator % 256;
                let ema = start + numerator / 256;
                self.ema_interval_ns = Some(ema);
                self.drift_ppm = (((ema - nominal) as i128 * 1_000_000) / nominal as i128) as i64;
            } else if nominal > 0 {
                // A rejected interval marks a discontinuity: any gate
                // qualification was earned under pre-gap conditions,
                // so the gate closes and must re-qualify on the
                // post-gap stream (Issue #35).
                self.relock.reset();
            }
        }
        self.last_ingest_pts = Some(frame.pts_ns);
        self.queue.push_back(frame);
    }

    /// Whether the drift re-lock gate is currently open (Issue #35;
    /// exposed for telemetry and tests).
    pub fn relock_active(&self) -> bool {
        self.relock.is_active()
    }

    /// Gradual re-anchor: while the stability gate is open (the
    /// estimated drift has stayed in the ±500..10 000 ppm band, one
    /// sign, for 30 consecutive output frames), shift the schedule by
    /// the per-frame error, capped at 1/8 of a frame interval per
    /// output frame (SPEC.md §4.1, Issue #35).
    ///
    /// Estimates beyond ±10 000 ppm (1%) are not clock drift at all but
    /// a genuinely different input rate (rate conversion); the gate
    /// keeps the schedule from chasing those.
    fn reanchor(&mut self) {
        if !self.relock.update(self.drift_ppm) {
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
    fn relock_gate_opens_only_after_thirty_sustained_frames() {
        let mut gate = RelockGate::new();
        for _ in 0..29 {
            assert!(!gate.update(2_000));
        }
        assert!(gate.update(2_000));
        assert!(gate.is_active());
    }

    #[test]
    fn relock_gate_resets_on_return_sign_flip_and_hard_limit() {
        // Returning inside the start band resets qualification.
        let mut gate = RelockGate::new();
        for _ in 0..10 {
            gate.update(2_000);
        }
        assert!(!gate.update(100));
        for _ in 0..29 {
            assert!(!gate.update(2_000));
        }
        assert!(gate.update(2_000));
        // A sign flip closes an active gate immediately.
        assert!(!gate.update(-2_000));
        assert!(!gate.is_active());
        // The hard limit: exactly 10 000 ppm qualifies, beyond resets.
        let mut gate = RelockGate::new();
        for _ in 0..29 {
            assert!(!gate.update(10_000));
        }
        assert!(gate.update(10_000));
        assert!(!gate.update(10_001));
        assert!(!gate.is_active());
    }

    #[test]
    fn relock_gate_hysteresis_holds_until_stop_threshold() {
        let mut gate = RelockGate::new();
        for _ in 0..30 {
            gate.update(2_000);
        }
        assert!(gate.is_active());
        // Between the stop and start thresholds the gate stays open.
        assert!(gate.update(400));
        assert!(gate.update(351));
        // At the stop threshold it closes.
        assert!(!gate.update(350));
        assert!(!gate.is_active());
    }

    #[test]
    fn relock_gate_never_opens_when_drift_hovers_at_the_threshold() {
        // Alternating across the start threshold keeps resetting the
        // qualification counter.
        let mut gate = RelockGate::new();
        for _ in 0..100 {
            for _ in 0..10 {
                assert!(!gate.update(600));
            }
            assert!(!gate.update(400));
        }
        assert!(!gate.is_active());
    }

    #[test]
    fn duplicate_pts_frames_do_not_pollute_the_estimate() {
        // Repeated timestamps (interval 0) are discontinuities, not
        // drift: the estimate and the anchor must not move.
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        for i in 0..300u64 {
            sync.ingest(frame(i, i * DUR));
            let _ = sync.next_output();
        }
        for i in 300..400u64 {
            sync.ingest(frame(i, 299 * DUR)); // same pts, repeatedly
            let _ = sync.next_output();
        }
        assert_eq!(sync.anchor_adjust_ns, 0);
        assert!(sync.drift_ppm_estimate().abs() < 50);
    }

    #[test]
    fn ema_remainder_carry_converges_below_the_old_dead_zone() {
        // Intervals 100 ns longer than nominal (~3 ppm): the old
        // truncating EMA never moved at all (100 / 256 == 0 per step).
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        for i in 0..5_000u64 {
            sync.ingest(frame(i, i * (DUR + 100)));
            let _ = sync.next_output();
        }
        let ema = sync.ema_interval_ns.expect("ema exists");
        assert!(
            (DUR as i64 + 98..=DUR as i64 + 100).contains(&ema),
            "ema was {ema}"
        );
        assert!((1..=6).contains(&sync.drift_ppm_estimate()));
    }

    /// Deterministic test RNG (seeded, per the repo rules).
    struct XorShift(u64);

    impl XorShift {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        /// Uniform value in [-span, span].
        fn jitter(&mut self, span: i64) -> i64 {
            (self.next_u64() % (2 * span as u64 + 1)) as i64 - span
        }
    }

    #[test]
    fn long_run_drift_tracking_stays_bounded() {
        // +2000 ppm with ±0.5% arrival jitter, 100k frames (~56 min
        // at 29.97 fps, accelerated): the gate must lock, the
        // cumulative shift must track the theoretical drift integral,
        // and the queue must not diverge.
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let base = DUR as i64 + DUR as i64 / 500; // +2000 ppm
        let span = DUR as i64 / 200; // ±0.5%
        let mut pts: u64 = 0;
        let mut last_out_pts: Option<u64> = None;
        let mut outputs = 0u64;
        for i in 0..100_000u64 {
            let interval = base + rng.jitter(span);
            pts = pts.saturating_add(interval.max(1) as u64);
            sync.ingest(frame(i, pts));
            if let Some(out) = sync.next_output() {
                if let Some(prev) = last_out_pts {
                    assert!(out.pts_ns >= prev, "output pts went backwards");
                }
                last_out_pts = Some(out.pts_ns);
                outputs += 1;
            }
        }
        assert!(sync.relock_active(), "gate never locked");
        assert!(outputs > 99_000, "outputs were {outputs}");
        // Theoretical cumulative shift: 100k outputs x 2000 ppm of DUR.
        let theory = 100_000i128 * DUR as i128 / 500;
        let adjust = sync.anchor_adjust_ns as i128;
        assert!(
            adjust * 100 >= theory * 80 && adjust * 100 <= theory * 120,
            "anchor adjust {adjust} vs theory {theory}"
        );
        assert!(
            sync.queue.len() < 100,
            "queue diverged: {}",
            sync.queue.len()
        );
        assert!(
            sync.duplicated_frames() < 1_000,
            "duplicated {}",
            sync.duplicated_frames()
        );
        assert!(
            sync.dropped_frames() < 10,
            "dropped {}",
            sync.dropped_frames()
        );
    }

    #[test]
    fn jitter_only_never_opens_the_gate() {
        // 0 ppm drift with ±0.5% arrival jitter: no re-anchoring at
        // all may happen (the false-positive case the gate exists for).
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        let mut rng = XorShift(0x1234_5678_9ABC_DEF0);
        let span = DUR as i64 / 200;
        let mut pts: u64 = 0;
        for i in 0..20_000u64 {
            let interval = DUR as i64 + rng.jitter(span);
            pts = pts.saturating_add(interval.max(1) as u64);
            sync.ingest(frame(i, pts));
            let _ = sync.next_output();
            assert_eq!(sync.anchor_adjust_ns, 0, "anchor moved at frame {i}");
        }
        assert!(!sync.relock_active());
    }

    #[test]
    fn sign_flip_relocks_in_the_new_direction_without_runaway() {
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        let fast = DUR + DUR / 500;
        let slow = DUR - DUR / 500;
        let mut pts: u64 = 0;
        for i in 0..20_000u64 {
            pts += fast;
            sync.ingest(frame(i, pts));
            let _ = sync.next_output();
        }
        let adjust_up = sync.anchor_adjust_ns;
        assert!(adjust_up > 1_000_000_000, "phase 1 adjust {adjust_up}");
        for i in 20_000..40_000u64 {
            pts += slow;
            sync.ingest(frame(i, pts));
            let _ = sync.next_output();
        }
        // The accumulated shift unwinds towards the new direction and
        // the gate is locked again, negatively.
        assert!(
            sync.anchor_adjust_ns < adjust_up / 2,
            "adjust did not unwind: {} (was {adjust_up})",
            sync.anchor_adjust_ns
        );
        assert!(sync.anchor_adjust_ns > -adjust_up, "adjust ran away");
        assert!(sync.relock_active());
        assert!(sync.drift_ppm_estimate() < -1_500);
    }

    #[test]
    fn ingest_gap_does_not_pollute_the_drift_estimate() {
        // A 1 s hole in the input (signal loss / stall) is a
        // discontinuity, not drift: the EMA must ignore the gap
        // interval and the anchor must not jump on recovery.
        let mut sync = FrameSynchronizer::new(config(RatePolicy::HoldDrop));
        let mut pts: u64 = 0;
        for i in 0..2_000u64 {
            pts += DUR;
            sync.ingest(frame(i, pts));
            let _ = sync.next_output();
        }
        assert_eq!(sync.anchor_adjust_ns, 0);
        pts += 1_000_000_000; // the gap
        for i in 2_000..4_000u64 {
            pts += DUR;
            sync.ingest(frame(i, pts));
            let _ = sync.next_output();
        }
        assert_eq!(sync.anchor_adjust_ns, 0, "anchor jumped after a gap");
        assert!(
            sync.drift_ppm_estimate().abs() < 50,
            "drift after gap was {}",
            sync.drift_ppm_estimate()
        );
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
