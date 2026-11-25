//! Design tokens for the Greg Audio GUI – flat, minimal, readable.
//!
//! All colours, sizes, radii and gutters used across every widget live here
//! so that the visual language stays consistent across the whole suite.

/// Background / surface colours (Ableton‑Live / Valhalla inspired).
pub mod colours {
    pub const SURFACE:    [u8; 3] = [32, 35, 42];      // #20232A
    pub const BORDER:     [u8; 3] = [46, 51, 61];      // #2E333D
    pub const ACCENT:     [u8; 3] = [229, 169, 59];    // #E5A93B
    pub const TEXT:       [u8; 3] = [241, 245, 249];   // #F1F5F9
}

/// Standard UI metrics.
pub mod metrics {
    pub const KNOB_W:        f32 = 80.0;
    pub const KNOB_H:        f32 = 88.0;
    pub const KNOB_RADIUS:   f32 = 24.0;
    pub const GUTTER:        f32 = 10.0;

    /// Knob visual diameters (px) per `KnobSize`.
    pub const KNOB_SIZE_LARGE: f32 = 64.0;
    pub const KNOB_SIZE_MEDIUM: f32 = 48.0;
    pub const KNOB_SIZE_SMALL: f32 = 36.0;
}