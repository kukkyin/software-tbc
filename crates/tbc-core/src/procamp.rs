//! Proc-amp style level correction: luma/chroma gain, black level
//! (pedestal), saturation, hue and gamma. Design: SPEC.md §4.3.

/// All values are neutral at their defaults; ranges are defined in SPEC.md.
#[derive(Debug, Clone, Copy)]
pub struct ProcAmpConfig {
    /// Luma gain, 1.0 = unity.
    pub luma_gain: f32,
    /// Black level offset added to Y after gain, in 8-bit code values.
    pub pedestal: f32,
    /// Chroma gain (saturation), 1.0 = unity.
    pub saturation: f32,
    /// Hue rotation in degrees, 0.0 = none.
    pub hue_degrees: f32,
    /// Gamma applied to Y, 1.0 = none.
    pub gamma: f32,
}

impl Default for ProcAmpConfig {
    fn default() -> Self {
        ProcAmpConfig {
            luma_gain: 1.0,
            pedestal: 0.0,
            saturation: 1.0,
            hue_degrees: 0.0,
            gamma: 1.0,
        }
    }
}
