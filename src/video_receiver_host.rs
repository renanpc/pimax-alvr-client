//! Host-test video receiver shim.
//!
//! The production receiver depends on Android GL and MediaCodec types. Host
//! tests only need the shared foveated-encoding shape and tuning defaults.

pub const PIMAX_BLIT_CONVERGENCE_SHIFT_NDC_DEFAULT: f32 = 0.124;
pub const COLOR_BLACK_CRUSH_DEFAULT: f32 = 0.072;
pub const COLOR_GAIN_DEFAULT: f32 = 1.22;

#[derive(Clone, Copy, Debug)]
pub struct FoveatedEncodingConfig {
    pub expanded_view_width: u32,
    pub expanded_view_height: u32,
    pub center_size_x: f32,
    pub center_size_y: f32,
    pub center_shift_x: f32,
    pub center_shift_y: f32,
    pub edge_ratio_x: f32,
    pub edge_ratio_y: f32,
}

pub fn configure_foveated_encoding(_config: Option<FoveatedEncodingConfig>) {}
