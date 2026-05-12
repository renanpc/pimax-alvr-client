//! Host-test video receiver shim.
//!
//! The production receiver depends on Android GL and MediaCodec types. Host
//! tests only need the shared foveated-encoding shape and tuning defaults.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};

pub const PIMAX_BLIT_CONVERGENCE_SHIFT_NDC_DEFAULT: f32 = 0.124;
pub const COLOR_BLACK_CRUSH_DEFAULT: f32 = 0.072;
pub const COLOR_GAIN_DEFAULT: f32 = 1.22;

#[derive(Debug, Clone, Default)]
pub struct VideoRenderDiagnosticsSnapshot {
    pub zero_copy_attempts: u64,
    pub zero_copy_success_count: u64,
    pub zero_copy_failure_count: u64,
    pub zero_copy_gl_error_count: u64,
    pub last_zero_copy_failure: Option<String>,
}

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

pub fn configure_hdr_stream(_enable_hdr: bool) {}

pub struct AlvrVideoReceiver {
    connected: AtomicBool,
}

static VIDEO_RECEIVER: OnceLock<Arc<AlvrVideoReceiver>> = OnceLock::new();

pub fn get_video_receiver() -> Arc<AlvrVideoReceiver> {
    VIDEO_RECEIVER
        .get_or_init(|| {
            Arc::new(AlvrVideoReceiver {
                connected: AtomicBool::new(false),
            })
        })
        .clone()
}

pub fn disconnect(receiver: &AlvrVideoReceiver) {
    receiver.connected.store(false, Ordering::SeqCst);
}

pub fn reset_video_render_diagnostics() {}

pub fn video_render_diagnostics_snapshot() -> VideoRenderDiagnosticsSnapshot {
    VideoRenderDiagnosticsSnapshot::default()
}

#[cfg(test)]
mod tests {
    use super::{COLOR_BLACK_CRUSH_DEFAULT, COLOR_GAIN_DEFAULT};

    fn color_adjustment_for_hdr_enabled(enable_hdr: bool) -> (f32, f32) {
        if enable_hdr {
            (0.0, 1.0)
        } else {
            (COLOR_BLACK_CRUSH_DEFAULT, COLOR_GAIN_DEFAULT)
        }
    }

    #[test]
    fn hdr_enabled_uses_neutral_color_adjustment() {
        assert_eq!(color_adjustment_for_hdr_enabled(true), (0.0, 1.0));
    }

    #[test]
    fn hdr_disabled_uses_sdr_defaults() {
        assert_eq!(
            color_adjustment_for_hdr_enabled(false),
            (COLOR_BLACK_CRUSH_DEFAULT, COLOR_GAIN_DEFAULT)
        );
    }
}
