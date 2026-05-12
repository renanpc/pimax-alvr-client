//! Upstream ALVR zero-copy decoder wrapper.
//!
//! Replaces the legacy CPU-output decoder with upstream's AHardwareBuffer-based
//! MediaCodec pipeline. The decoded frames stay in GPU memory and are imported
//! into OpenGL via EGLImage.

use std::time::Duration;

use anyhow::Result;
use log::{error, info, warn};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::upstream_video_decoder::{
    self, VideoDecoderConfig, VideoDecoderSink, VideoDecoderSource,
};

/// Global access to the upstream decoder source so the render thread can poll
/// for decoded AHardwareBuffer frames without going through the legacy
/// video_receiver mailbox.
static UPSTREAM_DECODER_SOURCE: Mutex<Option<VideoDecoderSource>> = Mutex::new(None);

/// Dimensions of the decoded frame, set during configure and read by the
/// render thread to size the intermediate FBO.
static UPSTREAM_DECODER_DIMENSIONS: Mutex<Option<(i32, i32)>> = Mutex::new(None);
static UPSTREAM_DECODER_RESTART_COUNT: AtomicU64 = AtomicU64::new(0);
static UPSTREAM_DECODER_BACKPRESSURE_COUNT: AtomicU64 = AtomicU64::new(0);
static UPSTREAM_DECODER_FATAL_ERROR_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default)]
pub struct UpstreamDecoderDiagnosticsSnapshot {
    pub restart_count: u64,
    pub backpressure_count: u64,
    pub fatal_error_count: u64,
}

/// Thin wrapper around upstream's VideoDecoderSink / VideoDecoderSource pair.
/// Maintains the same public interface as the legacy AlvrAndroidVideoDecoder
/// so client.rs needs minimal changes.
pub struct AlvrAndroidVideoDecoder {
    sink: Mutex<Option<VideoDecoderSink>>,
    config: Mutex<Option<DecoderConfigState>>,
}

#[derive(Clone)]
struct DecoderConfigState {
    codec: upstream_video_decoder::CodecType,
    codec_label: String,
    config_buffer: Vec<u8>,
    frame_width: i32,
    frame_height: i32,
}

impl AlvrAndroidVideoDecoder {
    pub fn new() -> Self {
        Self {
            sink: Mutex::new(None),
            config: Mutex::new(None),
        }
    }

    pub fn configure(
        &self,
        _mime_type: &'static str,
        codec_label: &str,
        config_buffer: Vec<u8>,
        _frame_width: i32,
        _frame_height: i32,
    ) -> Result<()> {
        let codec = match codec_label {
            "H264" => upstream_video_decoder::CodecType::H264,
            "HEVC" => upstream_video_decoder::CodecType::Hevc,
            "AV1" => upstream_video_decoder::CodecType::AV1,
            other => {
                warn!("unknown codec label '{}', defaulting to H264", other);
                upstream_video_decoder::CodecType::H264
            }
        };

        let state = DecoderConfigState {
            codec,
            codec_label: codec_label.to_string(),
            config_buffer,
            frame_width: _frame_width,
            frame_height: _frame_height,
        };

        self.configure_from_state(&state)?;
        *self.config.lock() = Some(state);

        Ok(())
    }

    fn configure_from_state(&self, state: &DecoderConfigState) -> Result<()> {
        let config_buffer = state.config_buffer.clone();

        let config = VideoDecoderConfig {
            codec: state.codec,
            force_software_decoder: false,
            max_buffering_frames: 4.0,
            buffering_history_weight: 0.90,
            options: Vec::new(),
            config_buffer,
            width: state.frame_width,
            height: state.frame_height,
        };

        let (sink, source) = upstream_video_decoder::create_decoder(config, |result| {
            match result {
                Ok(timestamp) => {
                    // Feed the decode timestamp into ALVR client stats so
                    // diagnostics keep a real decoder stage instead of a gap.
                    crate::client::report_alvr_frame_decoded(timestamp);
                }
                Err(e) => {
                    UPSTREAM_DECODER_FATAL_ERROR_COUNT.fetch_add(1, Ordering::Relaxed);
                    error!("upstream decoder fatal error: {e:#}");
                }
            }
        });

        *self.sink.lock() = Some(sink);
        *UPSTREAM_DECODER_SOURCE.lock() = Some(source);
        *UPSTREAM_DECODER_DIMENSIONS.lock() = Some((state.frame_width, state.frame_height));

        info!(
            "upstream zero-copy decoder configured for {} ({}x{})",
            state.codec_label, state.frame_width, state.frame_height
        );
        Ok(())
    }

    fn restart_for_idr(&self) -> bool {
        let Some(state) = self.config.lock().clone() else {
            warn!("cannot restart decoder: missing saved decoder config");
            return false;
        };

        UPSTREAM_DECODER_RESTART_COUNT.fetch_add(1, Ordering::Relaxed);
        info!(
            "restarting upstream decoder for IDR recovery: {} ({}x{})",
            state.codec_label, state.frame_width, state.frame_height
        );

        match self.configure_from_state(&state) {
            Ok(()) => true,
            Err(e) => {
                error!("failed to restart upstream decoder for IDR recovery: {e:#}");
                false
            }
        }
    }

    pub fn force_stream_recovery(&self) -> bool {
        self.restart_for_idr()
    }

    pub fn push_nal(&self, timestamp_ns: u64, _is_idr: bool, data: Vec<u8>) -> bool {
        let timestamp = Duration::from_nanos(timestamp_ns);
        {
            let mut sink_guard = self.sink.lock();
            let Some(sink) = sink_guard.as_mut() else {
                warn!("dropping NAL: upstream decoder not configured");
                drop(sink_guard);
                if !_is_idr || !self.restart_for_idr() {
                    return false;
                }

                let mut restarted_sink_guard = self.sink.lock();
                let Some(restarted_sink) = restarted_sink_guard.as_mut() else {
                    warn!("decoder restart did not install a sink");
                    return false;
                };
                return restarted_sink.push_nal(timestamp, &data);
            };

            if sink.push_nal(timestamp, &data) {
                return true;
            }
        }

        if _is_idr && self.restart_for_idr() {
            let mut sink_guard = self.sink.lock();
            if let Some(sink) = sink_guard.as_mut() {
                return sink.push_nal(timestamp, &data);
            }
        }

        UPSTREAM_DECODER_BACKPRESSURE_COUNT.fetch_add(1, Ordering::Relaxed);
        warn!("upstream decoder input buffer full, dropping NAL");
        false
    }
}

impl Default for AlvrAndroidVideoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Poll the upstream decoder source for a decoded frame.
///
/// Returns `(timestamp_ns, buffer_ptr)` where `buffer_ptr` is a raw
/// `*mut AHardwareBuffer` suitable for `EGLImage` import.
///
/// The caller MUST call this again on the next frame to release the
/// previous buffer back to the decoder pool.
pub fn poll_upstream_frame() -> Option<(u64, *mut std::ffi::c_void)> {
    let mut guard = UPSTREAM_DECODER_SOURCE.lock();
    let source = guard.as_mut()?;
    source
        .get_frame()
        .map(|(timestamp, ptr)| (timestamp.as_nanos() as u64, ptr))
}

/// Release the upstream decoder source (e.g. on disconnect).
pub fn release_upstream_decoder() {
    *UPSTREAM_DECODER_SOURCE.lock() = None;
    *UPSTREAM_DECODER_DIMENSIONS.lock() = None;
}

/// Return the configured decoder frame dimensions, if any.
pub fn upstream_decoder_dimensions() -> Option<(i32, i32)> {
    *UPSTREAM_DECODER_DIMENSIONS.lock()
}

pub fn reset_upstream_decoder_diagnostics() {
    UPSTREAM_DECODER_RESTART_COUNT.store(0, Ordering::Relaxed);
    UPSTREAM_DECODER_BACKPRESSURE_COUNT.store(0, Ordering::Relaxed);
    UPSTREAM_DECODER_FATAL_ERROR_COUNT.store(0, Ordering::Relaxed);
}

pub fn upstream_decoder_diagnostics_snapshot() -> UpstreamDecoderDiagnosticsSnapshot {
    UpstreamDecoderDiagnosticsSnapshot {
        restart_count: UPSTREAM_DECODER_RESTART_COUNT.load(Ordering::Relaxed),
        backpressure_count: UPSTREAM_DECODER_BACKPRESSURE_COUNT.load(Ordering::Relaxed),
        fatal_error_count: UPSTREAM_DECODER_FATAL_ERROR_COUNT.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_upstream_decoder_diagnostics_clears_all_counters() {
        UPSTREAM_DECODER_RESTART_COUNT.store(2, Ordering::Relaxed);
        UPSTREAM_DECODER_BACKPRESSURE_COUNT.store(3, Ordering::Relaxed);
        UPSTREAM_DECODER_FATAL_ERROR_COUNT.store(4, Ordering::Relaxed);

        reset_upstream_decoder_diagnostics();

        let snapshot = upstream_decoder_diagnostics_snapshot();
        assert_eq!(snapshot.restart_count, 0);
        assert_eq!(snapshot.backpressure_count, 0);
        assert_eq!(snapshot.fatal_error_count, 0);
    }
}
