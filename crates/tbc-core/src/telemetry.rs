//! Pipeline telemetry: latency, buffer fill, drop/duplicate counts and
//! jitter estimates. Design: SPEC.md §4.4.

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
}
