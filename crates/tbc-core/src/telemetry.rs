//! Pipeline telemetry: latency, buffer fill, drop/duplicate counts and
//! jitter estimates. Design: SPEC.md §4.6.

/// Point-in-time snapshot of pipeline health.
#[derive(Debug, Clone, Copy, Default)]
pub struct Telemetry {
    /// End-to-end latency of the most recently emitted frame, milliseconds.
    pub latency_ms: f64,
    /// Ring-buffer fill, 0.0 (empty) to 1.0 (full).
    pub buffer_fill_ratio: f64,
    pub dropped_frames: u64,
    pub duplicated_frames: u64,
    /// Estimated input jitter (standard deviation of arrival deltas), ms.
    pub jitter_ms: f64,
    pub frames_in: u64,
    pub frames_out: u64,
    /// Processing time per output frame, exponential moving average
    /// (alpha 1/16), milliseconds (Issue #36).
    pub proc_ms: f64,
    /// Output frames whose processing time exceeded the frame
    /// period (the deadline set with `set_deadline_ns`), cumulative
    /// (Issue #36).
    pub deadline_misses: u64,
}

/// Consumer of telemetry snapshots. The pipeline calls `report` at a
/// cadence chosen by the owner (the CLI uses `telemetry_interval_ms`).
pub trait TelemetrySink {
    fn report(&mut self, telemetry: &Telemetry);
}

/// A sink that discards everything, for tests and quiet runs.
impl TelemetrySink for () {
    fn report(&mut self, _telemetry: &Telemetry) {}
}

/// Online estimator of arrival-interval jitter using Welford's method.
#[derive(Debug, Default)]
pub struct JitterEstimator {
    last_pts_ns: Option<u64>,
    count: u64,
    mean_ns: f64,
    m2_ns: f64,
}

impl JitterEstimator {
    pub fn new() -> Self {
        JitterEstimator::default()
    }

    /// Feed one arrival timestamp (input clock, nanoseconds).
    pub fn observe_pts(&mut self, pts_ns: u64) {
        if let Some(last) = self.last_pts_ns {
            self.observe_interval_ns(pts_ns.saturating_sub(last) as f64);
        }
        self.last_pts_ns = Some(pts_ns);
    }

    /// Feed one inter-arrival interval directly (nanoseconds).
    pub fn observe_interval_ns(&mut self, interval_ns: f64) {
        self.count += 1;
        let delta = interval_ns - self.mean_ns;
        self.mean_ns += delta / self.count as f64;
        let delta2 = interval_ns - self.mean_ns;
        self.m2_ns += delta * delta2;
    }

    /// Standard deviation of the observed intervals, in milliseconds.
    pub fn jitter_ms(&self) -> f64 {
        if self.count < 2 {
            return 0.0;
        }
        let variance = self.m2_ns / (self.count - 1) as f64;
        variance.max(0.0).sqrt() / 1_000_000.0
    }

    /// Mean inter-arrival interval, in nanoseconds.
    pub fn mean_interval_ns(&self) -> f64 {
        self.mean_ns
    }
}

/// Aggregates counters and produces [`Telemetry`] snapshots.
#[derive(Debug, Default)]
pub struct TelemetryCollector {
    frames_in: u64,
    frames_out: u64,
    dropped_frames: u64,
    duplicated_frames: u64,
    latency_ms: f64,
    buffer_fill_ratio: f64,
    jitter: JitterEstimator,
    // Processing-time statistics, integer nanoseconds (Issue #36).
    proc_ema_ns: Option<u64>,
    proc_window_max_ns: u64,
    proc_max_ns: u64,
    proc_total_ns: u128,
    proc_samples: u64,
    deadline_ns: Option<u64>,
    deadline_misses: u64,
}

impl TelemetryCollector {
    pub fn new() -> Self {
        TelemetryCollector::default()
    }

    pub fn note_input(&mut self, pts_ns: u64) {
        self.frames_in += 1;
        self.jitter.observe_pts(pts_ns);
    }

    pub fn note_output(&mut self, latency_ms: f64) {
        self.frames_out += 1;
        self.latency_ms = latency_ms;
    }

    pub fn note_dropped(&mut self, count: u64) {
        self.dropped_frames += count;
    }

    pub fn note_duplicated(&mut self, count: u64) {
        self.duplicated_frames += count;
    }

    pub fn set_buffer_fill_ratio(&mut self, ratio: f64) {
        self.buffer_fill_ratio = ratio.clamp(0.0, 1.0);
    }

    /// Set the per-frame processing deadline (normally the target
    /// frame interval) used for deadline-miss counting (Issue #36).
    pub fn set_deadline_ns(&mut self, deadline_ns: u64) {
        self.deadline_ns = Some(deadline_ns);
    }

    /// Record one output frame's processing time (Issue #36).
    pub fn note_processing_time(&mut self, elapsed_ns: u64) {
        match self.proc_ema_ns {
            None => self.proc_ema_ns = Some(elapsed_ns),
            Some(ema) => {
                let next = (ema as i128 + (elapsed_ns as i128 - ema as i128) / 16).max(0);
                self.proc_ema_ns = Some(next as u64);
            }
        }
        self.proc_window_max_ns = self.proc_window_max_ns.max(elapsed_ns);
        self.proc_max_ns = self.proc_max_ns.max(elapsed_ns);
        self.proc_total_ns = self.proc_total_ns.saturating_add(elapsed_ns as u128);
        self.proc_samples = self.proc_samples.saturating_add(1);
        if let Some(deadline) = self.deadline_ns {
            if elapsed_ns > deadline {
                self.deadline_misses += 1;
            }
        }
    }

    /// The largest processing time since the last call, in
    /// milliseconds; resets the window (Issue #36).
    pub fn take_proc_window_max_ms(&mut self) -> f64 {
        let max = self.proc_window_max_ns;
        self.proc_window_max_ns = 0;
        max as f64 / 1_000_000.0
    }

    /// Mean processing time over the whole run, milliseconds.
    pub fn proc_avg_ms(&self) -> f64 {
        if self.proc_samples == 0 {
            return 0.0;
        }
        (self.proc_total_ns as f64 / self.proc_samples as f64) / 1_000_000.0
    }

    /// Largest processing time over the whole run, milliseconds.
    pub fn proc_max_ms(&self) -> f64 {
        self.proc_max_ns as f64 / 1_000_000.0
    }

    /// Number of processing-time samples recorded.
    pub fn proc_samples(&self) -> u64 {
        self.proc_samples
    }

    pub fn snapshot(&self) -> Telemetry {
        Telemetry {
            latency_ms: self.latency_ms,
            buffer_fill_ratio: self.buffer_fill_ratio,
            dropped_frames: self.dropped_frames,
            duplicated_frames: self.duplicated_frames,
            jitter_ms: self.jitter.jitter_ms(),
            frames_in: self.frames_in,
            frames_out: self.frames_out,
            proc_ms: self.proc_ema_ns.unwrap_or(0) as f64 / 1_000_000.0,
            deadline_misses: self.deadline_misses,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_time_ema_seeds_on_first_sample_and_tracks() {
        let mut c = TelemetryCollector::new();
        c.note_processing_time(16_000_000); // 16 ms seed
        assert_eq!(c.snapshot().proc_ms, 16.0);
        // Feeding 0 ms moves the EMA down by 1/16 of the gap per step.
        c.note_processing_time(0);
        assert_eq!(c.snapshot().proc_ms, 15.0);
        for _ in 0..200 {
            c.note_processing_time(0);
        }
        assert!(c.snapshot().proc_ms < 0.01);
    }

    #[test]
    fn proc_time_window_max_resets_but_total_max_and_avg_persist() {
        let mut c = TelemetryCollector::new();
        c.note_processing_time(5_000_000);
        c.note_processing_time(9_000_000);
        assert_eq!(c.take_proc_window_max_ms(), 9.0);
        // Window was reset by the take; the run max and mean persist.
        c.note_processing_time(1_000_000);
        assert_eq!(c.take_proc_window_max_ms(), 1.0);
        assert_eq!(c.proc_max_ms(), 9.0);
        assert_eq!(c.proc_samples(), 3);
        assert_eq!(c.proc_avg_ms(), 5.0);
    }

    #[test]
    fn deadline_misses_count_only_overruns() {
        let mut c = TelemetryCollector::new();
        c.note_processing_time(50_000_000); // no deadline set: no miss
        assert_eq!(c.snapshot().deadline_misses, 0);
        c.set_deadline_ns(33_366_666);
        c.note_processing_time(10_000_000);
        c.note_processing_time(33_366_666); // exactly at: not a miss
        assert_eq!(c.snapshot().deadline_misses, 0);
        c.note_processing_time(40_000_000);
        assert_eq!(c.snapshot().deadline_misses, 1);
    }

    #[test]
    fn constant_intervals_have_zero_jitter() {
        let mut est = JitterEstimator::new();
        for i in 0..100u64 {
            est.observe_pts(i * 33_366_666);
        }
        assert!(est.jitter_ms() < 0.001, "jitter was {}", est.jitter_ms());
    }

    #[test]
    fn alternating_intervals_match_theoretical_stddev() {
        // Intervals alternating 30ms / 40ms: mean 35ms, population spread
        // 5ms; the sample standard deviation is ~5ms.
        let mut est = JitterEstimator::new();
        for i in 0..101u64 {
            let interval = if i % 2 == 0 {
                30_000_000.0
            } else {
                40_000_000.0
            };
            est.observe_interval_ns(interval);
        }
        let jitter = est.jitter_ms();
        assert!((jitter - 5.0).abs() < 0.2, "jitter was {jitter}");
    }

    #[test]
    fn collector_snapshot_reflects_counters() {
        let mut c = TelemetryCollector::new();
        c.note_input(0);
        c.note_input(33_366_666);
        c.note_output(12.5);
        c.note_dropped(3);
        c.note_duplicated(2);
        c.set_buffer_fill_ratio(0.75);
        let t = c.snapshot();
        assert_eq!(t.frames_in, 2);
        assert_eq!(t.frames_out, 1);
        assert_eq!(t.dropped_frames, 3);
        assert_eq!(t.duplicated_frames, 2);
        assert_eq!(t.latency_ms, 12.5);
        assert_eq!(t.buffer_fill_ratio, 0.75);
    }

    #[test]
    fn sink_receives_snapshots() {
        #[derive(Default)]
        struct VecSink(Vec<Telemetry>);
        impl TelemetrySink for VecSink {
            fn report(&mut self, telemetry: &Telemetry) {
                self.0.push(*telemetry);
            }
        }
        let mut sink = VecSink::default();
        let c = TelemetryCollector::new();
        sink.report(&c.snapshot());
        assert_eq!(sink.0.len(), 1);
    }
}
