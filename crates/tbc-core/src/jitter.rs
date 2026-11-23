//! Per-line horizontal jitter (phase) detection and correction.
//!
//! Design: SPEC.md §4.2. Implementation is tracked in the Phase 2 issues.

/// Estimated horizontal shift of one line, in sub-pixel units (1/256 px).
#[derive(Debug, Clone, Copy, Default)]
pub struct LineShift {
    pub line: u32,
    pub shift_q8: i32,
}
