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
