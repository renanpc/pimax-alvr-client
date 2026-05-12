//! ALVR Client Protocol Implementation
//!
//! # Overview
//!
//! This module implements the ALVR client protocol for communicating with the ALVR server
//! (running on a PC). It handles:
//!
//! - **Discovery**: Broadcasting client presence and finding servers
//! - **Handshake**: Negotiating codec, resolution, and stream parameters
//! - **Video Reception**: Collecting UDP shards into complete video packets
//! - **Head Tracking**: Sending pose updates to the server (90+ Hz)
//! - **Statistics**: Reporting frame timing and dropped frames
//!
//! # ALVR Protocol Architecture
//!
//! ## Connection Model
//!
//! ALVR uses a **server-connects-to-client** model:
//!
//! ```text
//! Client (Headset)                    Server (PC)
//!      │                                  │
//!      │◄──── Discovery Broadcast ────────│  UDP 9943
//!      │     "ALVR...DISCOVERY"           │
//!      │                                  │
//!      │───── Discovery Response ────────►│  "ALVR...<hostname>"
//!      │                                  │
//!      │◄──── TCP Connect ────────────────│  Port 9943 (control)
//!      │     (server initiates!)          │
//!      │                                  │
//!      │───── Handshake ─────────────────►│  Client info, capabilities
//!      │◄──── Stream Config ──────────────│  Codec, resolution, etc.
//!      │                                  │
//!      │◄──── UDP Video Stream ───────────│  Port 9944 (video)
//!      │     (sharded packets)            │
//!      │                                  │
//!      │───── Head Tracking ─────────────►│  Pose updates (90Hz+)
//!      │───── Statistics ────────────────►│  Frame timing feedback
//! ```
//!
//! ## Key Design Decisions
//!
//! ### Why Server-Connects-to-Client?
//!
//! 1. **NAT/Firewall Friendly**: PC is typically on a wired network with stable IP
//! 2. **Mobile Headset**: Headset may roam between networks; easier to discover
//! 3. **Multiple Clients**: Server can choose which client to connect to
//!
//! ### Packet Sharding
//!
//! Video frames are split into UDP shards because:
//! - Ethernet MTU is ~1500 bytes
//! - Video frames are 100KB-500KB
//! - Each shard has an 18-byte header with packet/shard indices
//!
//! ### IPD Scale (Stereo Blending)
//!
//! The Pimax Crystal has its own stereo rendering in the compositor. ALVR also
//! renders stereo. If both contribute full stereo, the result is excessive
//! separation causing eye strain.
//!
//! The `ipd_scale` parameter blends between:
//! - `0.0`: Monoscopic ALVR (all stereo from Pimax compositor)
//! - `1.0`: Full ALVR stereo (physical IPD from headset sensors)
//! - `>1.0`: Exaggerated stereo separation
//!
//! **Important**: The physical IPD is stored in `PHYSICAL_IPD_M`. The scale is
//! applied exactly once when building `ViewsConfig`. Never apply scale twice.
//!
//! # Threading Model
//!
//! - **Control Listener Thread**: Waits for server TCP connection (blocking)
//! - **Video Receiver Thread**: Collects UDP shards (blocking recv)
//! - **Tracking Thread**: Sends head poses at 90Hz+ (timed loop)
//! - **Render Thread**: Reads decoded frames and calls into this module
//!
//! # State Management
//!
//! Shared state between threads uses:
//! - `Mutex<T>`: For complex state (ViewsConfig, statistics)
//! - `AtomicU32`: For simple values (IPD, flags)
//! - `Arc<Mutex<T>>`: For shared ownership across threads
//!
//! # Configuration
//!
//! Client identity and settings are loaded from `ClientConfig`:
//! - `client_name`: Hostname for identification
//! - `version_string`: ALVR protocol version
//! - `discovery_port`: UDP port for discovery (default: 9943)
//! - `stream_port`: TCP/UDP port for streaming (default: 9944)
//!
//! # Error Handling
//!
//! Most operations return `Result<T>` with context:
//! - Network errors: Connection refused, timeout
//! - Protocol errors: Invalid packet format, version mismatch
//! - Codec errors: Decoder configuration failure
//!
//! On critical errors, the connection is closed and must be re-established.

use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    net::{
        IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener as StdTcpListener,
        TcpStream as StdTcpStream, UdpSocket as StdUdpSocket,
    },
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, LazyLock, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
use std::os::fd::AsRawFd;

use anyhow::{anyhow, bail, Context, Result};
use log::{info, warn};

#[derive(Debug, Default, Clone)]
struct AlvrUdpStreamDiagnostics {
    total_shards: u64,
    video_shards: u64,
    audio_shards: u64,
    completed_video_packets: u64,
    completed_audio_packets: u64,
    waiting_for_idr_drops: u64,
    pre_decoder_config_drops: u64,
    packet_gap_resets: u64,
    decoder_backpressure_resets: u64,
    decoder_idr_unavailable_resets: u64,
    short_datagrams: u64,
    udp_terminal_errors: u64,
    idr_requests: u64,
}

impl AlvrUdpStreamDiagnostics {
    fn note_idr_request(&mut self) {
        self.idr_requests = self.idr_requests.wrapping_add(1);
    }
}

#[derive(Debug, Default, Clone)]
struct RuntimeVideoRecoveryState {
    queued: bool,
    in_flight: bool,
    last_requested_at: Option<Instant>,
    last_started_at: Option<Instant>,
    last_completed_at: Option<Instant>,
}

impl RuntimeVideoRecoveryState {
    fn queue_if_due(&mut self, now: Instant) -> bool {
        self.expire_if_timed_out(now);

        if self.in_flight {
            return false;
        }

        if let Some(previous) = self
            .last_completed_at
            .or(self.last_requested_at)
            .or(self.last_started_at)
        {
            if now.duration_since(previous) < ALVR_RUNTIME_VIDEO_RECOVERY_COOLDOWN {
                return false;
            }
        }

        self.queued = true;
        self.last_requested_at = Some(now);
        true
    }

    fn take_queued(&mut self, now: Instant) -> bool {
        self.expire_if_timed_out(now);

        if !self.queued || self.in_flight {
            return false;
        }

        self.queued = false;
        self.in_flight = true;
        self.last_started_at = Some(now);
        true
    }

    fn mark_completed(&mut self, now: Instant) -> bool {
        self.expire_if_timed_out(now);

        if !self.in_flight {
            return false;
        }

        self.in_flight = false;
        self.queued = false;
        self.last_completed_at = Some(now);
        true
    }

    fn expire_if_timed_out(&mut self, now: Instant) {
        if self
            .last_started_at
            .is_some_and(|started| now.duration_since(started) >= ALVR_RUNTIME_VIDEO_RECOVERY_TIMEOUT)
        {
            self.in_flight = false;
            self.queued = false;
            self.last_started_at = None;
        }
    }

    #[cfg(test)]
    fn with_times(
        queued: bool,
        in_flight: bool,
        last_requested_at: Option<Instant>,
        last_started_at: Option<Instant>,
        last_completed_at: Option<Instant>,
    ) -> Self {
        Self {
            queued,
            in_flight,
            last_requested_at,
            last_started_at,
            last_completed_at,
        }
    }
}

fn classify_alvr_failure_class(
    stream: &AlvrUdpStreamDiagnostics,
    render: &crate::video_receiver::VideoRenderDiagnosticsSnapshot,
) -> &'static str {
    if render.zero_copy_failure_count > 0 || render.zero_copy_gl_error_count > 0 {
        "compositor-submit"
    } else if stream.decoder_backpressure_resets > 0 || stream.waiting_for_idr_drops > 0 {
        "decoder-or-stream-recovery"
    } else if stream.packet_gap_resets > 0 {
        "network-packet-gap"
    } else if stream.udp_terminal_errors > 0 {
        "udp-terminal-error"
    } else {
        "none-observed"
    }
}

fn log_alvr_diagnostics_summary(
    session_id: u32,
    reason: &str,
    uptime: Duration,
    stream: &AlvrUdpStreamDiagnostics,
) {
    #[cfg(target_os = "android")]
    let (decoder_backpressure_count, decoder_restart_count, decoder_fatal_error_count) = {
        let decoder = crate::android_video_decoder::upstream_decoder_diagnostics_snapshot();
        (
            decoder.backpressure_count,
            decoder.restart_count,
            decoder.fatal_error_count,
        )
    };
    #[cfg(not(target_os = "android"))]
    let (decoder_backpressure_count, decoder_restart_count, decoder_fatal_error_count) =
        (0_u64, 0_u64, 0_u64);

    let render = crate::video_receiver::video_render_diagnostics_snapshot();
    let failure_class = classify_alvr_failure_class(stream, &render);

    info!(
        "ALVR diagnostics summary: session_id={} reason=\"{}\" uptime_ms={} failure_class={} stream={{shards_total={},video_shards={},audio_shards={},video_packets={},audio_packets={},short_datagrams={},udp_terminal_errors={}}} recovery={{idr_requests={},packet_gap_resets={},waiting_for_idr_drops={},pre_decoder_config_drops={},decoder_backpressure_resets={},decoder_idr_unavailable_resets={}}} decoder={{backpressure_count={},restart_count={},fatal_error_count={}}} render={{zero_copy_attempts={},zero_copy_success_count={},zero_copy_failure_count={},zero_copy_gl_error_count={},last_zero_copy_failure={:?}}}",
        session_id,
        reason,
        uptime.as_millis(),
        failure_class,
        stream.total_shards,
        stream.video_shards,
        stream.audio_shards,
        stream.completed_video_packets,
        stream.completed_audio_packets,
        stream.short_datagrams,
        stream.udp_terminal_errors,
        stream.idr_requests,
        stream.packet_gap_resets,
        stream.waiting_for_idr_drops,
        stream.pre_decoder_config_drops,
        stream.decoder_backpressure_resets,
        stream.decoder_idr_unavailable_resets,
        decoder_backpressure_count,
        decoder_restart_count,
        decoder_fatal_error_count,
        render.zero_copy_attempts,
        render.zero_copy_success_count,
        render.zero_copy_failure_count,
        render.zero_copy_gl_error_count,
        render.last_zero_copy_failure,
    );
}
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    net::{TcpStream, UdpSocket as TokioUdpSocket},
    time::timeout,
};

use crate::{
    config::ClientConfig,
    protocol::{hash_string, DiscoveryPacket, ProtocolId},
};

/// Returns the WiFi IPv4 address by routing a dummy UDP packet.
fn wifi_ipv4() -> Result<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").context("bind probe socket")?;
    socket
        .connect("8.8.8.8:53")
        .context("connect probe socket")?;
    match socket.local_addr()?.ip() {
        IpAddr::V4(ip) => Ok(ip),
        IpAddr::V6(_) => anyhow::bail!("only IPv4 supported for mDNS"),
    }
}

/// Derives the ALVR protocol string from a semver version string.
/// Stable releases use only the major version ("20").
/// Pre-releases append the pre-release tag ("20-alpha.1").
fn alvr_protocol_string(version_string: &str) -> String {
    semver::Version::parse(version_string)
        .map(|v| {
            if v.pre.is_empty() {
                v.major.to_string()
            } else {
                format!("{}-{}", v.major, v.pre)
            }
        })
        .unwrap_or_else(|_| version_string.to_owned())
}

/// Shared handle to the ALVR control TCP stream.
///
/// Wrapped in Arc<Mutex<>> because:
/// - Multiple threads may read/write (handshake, keepalive, config)
/// - TcpStream is not Clone, so we share ownership
type SharedControlWriter = Arc<Mutex<StdTcpStream>>;

struct AlvrStreamSession {
    session_id: u32,
    shutdown_requested: Arc<AtomicBool>,
    cleanup_reason: String,
    control_maintenance_handle: Option<JoinHandle<()>>,
    tracking_sender_handle: Option<JoinHandle<()>>,
    udp_receiver_handle: Option<JoinHandle<()>>,
    microphone_capture: Option<crate::audio::MicrophoneCapture>,
    game_audio_output: Option<crate::audio::GameAudioOutput>,
    statistics_sender_guard: Option<AlvrStatisticsSenderGuard>,
}

impl AlvrStreamSession {
    fn new(
        session_id: u32,
        microphone_capture: Option<crate::audio::MicrophoneCapture>,
        game_audio_output: Option<crate::audio::GameAudioOutput>,
    ) -> Self {
        Self {
            session_id,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            cleanup_reason: "ALVR stream session ended".to_owned(),
            control_maintenance_handle: None,
            tracking_sender_handle: None,
            udp_receiver_handle: None,
            microphone_capture,
            game_audio_output,
            statistics_sender_guard: None,
        }
    }

    fn shutdown_signal(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown_requested)
    }

    fn set_cleanup_reason(&mut self, reason: impl Into<String>) {
        self.cleanup_reason = reason.into();
    }

    fn request_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Release);
    }
}

impl Drop for AlvrStreamSession {
    fn drop(&mut self) {
        self.request_shutdown();
        info!(
            "ALVR stream session cleanup starting: session_id={} reason={}",
            self.session_id, self.cleanup_reason
        );

        if let Some(capture) = self.microphone_capture.as_ref() {
            info!(
                "ALVR stream session cleanup: shutting down microphone capture for session_id={}",
                self.session_id
            );
            capture.shutdown();
        }
        if let Some(output) = self.game_audio_output.as_ref() {
            info!(
                "ALVR stream session cleanup: shutting down game audio output for session_id={}",
                self.session_id
            );
            output.shutdown();
        }

        self.statistics_sender_guard.take();
        clear_alvr_statistics_state();

        if let Some(handle) = self.control_maintenance_handle.take() {
            info!(
                "ALVR stream session cleanup: joining control-maintenance for session_id={}",
                self.session_id
            );
            drain_alvr_stream_thread("control-maintenance", self.session_id, handle);
        }
        if let Some(handle) = self.tracking_sender_handle.take() {
            info!(
                "ALVR stream session cleanup: joining tracking-sender for session_id={}",
                self.session_id
            );
            drain_alvr_stream_thread("tracking-sender", self.session_id, handle);
        }
        if let Some(handle) = self.udp_receiver_handle.take() {
            info!(
                "ALVR stream session cleanup: joining udp-receiver for session_id={}",
                self.session_id
            );
            drain_alvr_stream_thread("udp-receiver", self.session_id, handle);
        }

        info!(
            "ALVR stream session cleanup: releasing decoder state for session_id={}",
            self.session_id
        );
        release_alvr_stream_decoder();
        let receiver = crate::video_receiver::get_video_receiver();
        info!(
            "ALVR stream session cleanup: disconnecting video receiver for session_id={}",
            self.session_id
        );
        crate::video_receiver::disconnect(receiver.as_ref());

        info!(
            "ALVR stream session cleanup complete: session_id={} reason={}",
            self.session_id, self.cleanup_reason
        );
    }
}

#[cfg(target_os = "android")]
type VideoDecoderBridge = crate::android_video_decoder::AlvrAndroidVideoDecoder;

#[cfg(not(target_os = "android"))]
#[derive(Default)]
struct VideoDecoderBridge;

#[cfg(not(target_os = "android"))]
impl VideoDecoderBridge {
    fn new() -> Self {
        Self
    }

    fn configure(
        &self,
        _mime_type: &'static str,
        _codec_label: &str,
        _config_buffer: Vec<u8>,
        _frame_width: i32,
        _frame_height: i32,
    ) -> Result<()> {
        Ok(())
    }

    fn push_nal(&self, _timestamp_ns: u64, _is_idr: bool, _data: Vec<u8>) -> bool {
        true
    }

    fn force_stream_recovery(&self) -> bool {
        true
    }
}

const HANDSHAKE_ACTION_TIMEOUT: Duration = Duration::from_secs(5);
const ALVR_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);
const ALVR_RUNTIME_IDR_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(500);
const ALVR_RUNTIME_VIDEO_RECOVERY_COOLDOWN: Duration = Duration::from_secs(3);
const ALVR_RUNTIME_VIDEO_RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const ALVR_CONTROL_RECV_TIMEOUT: Duration = Duration::from_millis(500);
const ALVR_STREAM_THREAD_JOIN_GRACE: Duration = Duration::from_millis(500);
const ALVR_DISCOVERY_RECOVERY_ATTEMPTS: usize = 30;
const ALVR_DISCOVERY_RECOVERY_INTERVAL: Duration = Duration::from_secs(2);
const ALVR_STREAM_SHARD_PREFIX_SIZE: usize = 14;
#[cfg(target_os = "android")]
const ALVR_UDP_RECEIVE_BUFFER_BYTES: i32 = 4 * 1024 * 1024;
const ALVR_TRACKING_STREAM_ID: u16 = 0;
const ALVR_AUDIO_STREAM_ID: u16 = 2;
const ALVR_VIDEO_STREAM_ID: u16 = 3;
const ALVR_STATISTICS_STREAM_ID: u16 = 4;
const ALVR_STREAM_LOG_EVERY: u64 = 3_600;
const ALVR_BUFFER_MODE_VIEW_RESOLUTION: u32 = 2880;
const ALVR_TRACKING_SEND_INTERVAL: Duration = Duration::from_micros(13_889);
// 30 Hz button send rate. Higher than typical OpenXR sample rate but well
// below the 90 Hz tracking rate to avoid flooding the control TCP socket.
const ALVR_BUTTONS_SEND_INTERVAL: Duration = Duration::from_millis(33);
const ALVR_DEFAULT_FRAME_INTERVAL: Duration = Duration::from_micros(13_889);
const ALVR_STATISTICS_HISTORY_SIZE: usize = 512;
const ALVR_DEFAULT_IPD_M: f32 = 0.064;
// Scale factor applied to the physical IPD before sending to ALVR.
// Scaling IPD down reduces ALVR's contribution until the total stereo feels correct.
// 0.0 = monoscopic from ALVR (all stereo from Pimax compositor), 1.0 = full ALVR stereo.
// NOTE: ALVR_VIEW_CONFIG_STATE stores the *physical* IPD; this scale is applied exactly once
// at the point of building ViewsConfig (in both update_alvr_views_config_from_pimax and
// update_alvr_ipd_from_pimax). Do NOT apply it a second time via current_alvr_ipd_m().
/// Default IPD scale — exposed so `android.rs` can pass it to `tune::init`.
/// The actual live value is read from `tune::ipd_scale()` each time a ViewsConfig is sent.
pub const ALVR_IPD_SCALE_DEFAULT: f32 = 1.0;
const ALVR_HEAD_PATH: &str = "/user/head";

static ALVR_CONTROL_LISTENER_STARTED: AtomicBool = AtomicBool::new(false);
static ALVR_STREAM_SESSION_COUNTER: AtomicU32 = AtomicU32::new(1);
static ALVR_DISCOVERY_RECOVERY_OWNER: AtomicU32 = AtomicU32::new(0);
static ALVR_DISCOVERY_RECOVERY_GENERATION: AtomicU32 = AtomicU32::new(1);
static ALVR_RUNTIME_VIDEO_RECOVERY_REQUESTED: AtomicBool = AtomicBool::new(false);
static ALVR_RUNTIME_VIDEO_RECOVERY_STATE: LazyLock<Mutex<RuntimeVideoRecoveryState>> =
    LazyLock::new(|| Mutex::new(RuntimeVideoRecoveryState::default()));
static ALVR_MDNS_DAEMON: LazyLock<Mutex<Option<mdns_sd::ServiceDaemon>>> =
    LazyLock::new(|| Mutex::new(None));
static LATEST_HEAD_TRACKING_POSE: Mutex<Option<AlvrHeadTrackingPose>> = Mutex::new(None);
static ALVR_STATISTICS_STATE: Mutex<Option<AlvrClientStatisticsState>> = Mutex::new(None);
static ALVR_STATISTICS_SENDER: Mutex<Option<AlvrStreamHeaderSender>> = Mutex::new(None);
static ALVR_VIEW_CONFIG_STATE: Mutex<Option<VersionedViewsConfig>> = Mutex::new(None);
static PIMAX_VIEW_INFO: Mutex<Option<PimaxViewInfo>> = Mutex::new(None);
/// Raw physical IPD in metres, last reported by the Pimax hardware sensor.
/// Stored separately so the tune IPD scale can be applied live without waiting
/// for the next hardware IPD event.
static PHYSICAL_IPD_M: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug)]
struct AlvrHeadTrackingPose {
    orientation: glam::Quat,
    position: glam::Vec3,
    timestamp: Duration,
}

#[derive(Clone, Copy, Debug)]
struct PimaxViewInfo {
    fov_x_rad: f32,
    fov_y_rad: f32,
    eye_width: i32,
    eye_height: i32,
}

pub(crate) fn update_head_tracking_pose(
    orientation: glam::Quat,
    position: glam::Vec3,
    timestamp: Duration,
) {
    if let Ok(mut pose) = LATEST_HEAD_TRACKING_POSE.lock() {
        *pose = Some(AlvrHeadTrackingPose {
            orientation,
            position,
            timestamp,
        });
    }
}

fn latest_head_tracking_pose() -> Option<AlvrHeadTrackingPose> {
    LATEST_HEAD_TRACKING_POSE.lock().ok().and_then(|pose| *pose)
}

/// Returns the current IPD in metres, already scaled by `tune::ipd_scale()`,
/// ready to send to ALVR. Do not multiply by the tune scale again at the call site.
fn current_alvr_ipd_m() -> f32 {
    latest_alvr_views_config()
        .map(|state| state.config.ipd_m)
        .unwrap_or(ALVR_DEFAULT_IPD_M * crate::tune::ipd_scale())
}

fn normalize_pimax_ipd_m(raw_ipd: f32) -> Option<f32> {
    if !raw_ipd.is_finite() || raw_ipd <= 0.0 {
        return None;
    }

    let ipd_m = if raw_ipd > 1.0 {
        raw_ipd / 1000.0
    } else {
        raw_ipd
    };
    if !ipd_m.is_finite() || ipd_m <= 0.0 {
        return None;
    }

    Some(ipd_m.clamp(0.05, 0.08))
}

pub(crate) fn update_alvr_views_config_from_pimax(
    fov_x_rad: f32,
    fov_y_rad: f32,
    eye_width: i32,
    eye_height: i32,
) {
    if let Ok(mut info) = PIMAX_VIEW_INFO.lock() {
        *info = Some(PimaxViewInfo {
            fov_x_rad,
            fov_y_rad,
            eye_width,
            eye_height,
        });
    }
    update_alvr_views_config_from_pimax_scaled(
        fov_x_rad,
        fov_y_rad,
        eye_width,
        eye_height,
        crate::tune::eye_render_scale(),
    );
}

pub(crate) fn notify_fov_scale_changed() {
    let Some(info) = PIMAX_VIEW_INFO.lock().ok().and_then(|info| *info) else {
        return;
    };
    update_alvr_views_config_from_pimax_scaled(
        info.fov_x_rad,
        info.fov_y_rad,
        info.eye_width,
        info.eye_height,
        crate::tune::eye_render_scale(),
    );
}

fn update_alvr_views_config_from_pimax_scaled(
    fov_x_rad: f32,
    fov_y_rad: f32,
    eye_width: i32,
    eye_height: i32,
    eye_render_scale: f32,
) {
    if !fov_x_rad.is_finite() || !fov_y_rad.is_finite() || fov_x_rad <= 0.0 || fov_y_rad <= 0.0 {
        warn!(
            "ignoring invalid Pimax ALVR view config input: fov_x_rad={} fov_y_rad={} eye={}x{}",
            fov_x_rad, fov_y_rad, eye_width, eye_height
        );
        return;
    }

    let eye_scale = eye_render_scale.max(0.5);
    let fov_scale = crate::tune::fov_scale().clamp(0.8, 1.2);
    let eye_width = ((eye_width.max(1) as f32) * eye_scale).round().max(1.0) as i32;
    let eye_height = ((eye_height.max(1) as f32) * eye_scale).round().max(1.0) as i32;

    let horizontal_tan = ((fov_x_rad * 0.5).tan() * fov_scale).clamp(0.01, 8.0);
    let vertical_tan = ((fov_y_rad * 0.5).tan() * fov_scale).clamp(0.01, 8.0);
    let fov = Fov {
        left: -horizontal_tan,
        right: horizontal_tan,
        up: vertical_tan,
        down: -vertical_tan,
    };
    let config = ViewsConfig {
        // current_alvr_ipd_m() already returns the scaled IPD.
        ipd_m: current_alvr_ipd_m(),
        fov: [fov, fov],
    };

    let mut state = match ALVR_VIEW_CONFIG_STATE.lock() {
        Ok(state) => state,
        Err(_) => {
            warn!("ALVR view config mutex is poisoned");
            return;
        }
    };
    let version = state
        .as_ref()
        .map(|state| state.version.wrapping_add(1).max(1))
        .unwrap_or(1);
    *state = Some(VersionedViewsConfig {
        version,
        config: config.clone(),
    });
    info!(
        "updated ALVR ViewsConfig from Pimax device info: version={} eye={}x{} scale={:.3} fov_scale={:.3} ipd_m={:.3} fov_rad=({:.6},{:.6}) fov_tan=left:{:.3} right:{:.3} up:{:.3} down:{:.3}",
        version,
        eye_width,
        eye_height,
        eye_scale,
        fov_scale,
        config.ipd_m,
        fov_x_rad,
        fov_y_rad,
        fov.left,
        fov.right,
        fov.up,
        fov.down
    );
}

pub(crate) fn notify_eye_render_scale_changed() {
    let Some(info) = PIMAX_VIEW_INFO.lock().ok().and_then(|info| *info) else {
        return;
    };
    update_alvr_views_config_from_pimax_scaled(
        info.fov_x_rad,
        info.fov_y_rad,
        info.eye_width,
        info.eye_height,
        crate::tune::eye_render_scale(),
    );
}

pub(crate) fn update_alvr_ipd_from_pimax(raw_ipd: f32) {
    let Some(ipd_m) = normalize_pimax_ipd_m(raw_ipd) else {
        warn!("ignoring invalid Pimax IPD update: raw_ipd={raw_ipd}");
        return;
    };

    // Store physical IPD so notify_ipd_scale_changed() can recompute without a hardware event.
    PHYSICAL_IPD_M.store(ipd_m.to_bits(), Ordering::Relaxed);

    let mut state = match ALVR_VIEW_CONFIG_STATE.lock() {
        Ok(state) => state,
        Err(_) => {
            warn!("ALVR view config mutex is poisoned");
            return;
        }
    };
    let version = state
        .as_ref()
        .map(|state| state.version.wrapping_add(1).max(1))
        .unwrap_or(1);
    let mut config = state
        .as_ref()
        .map(|state| state.config.clone())
        .unwrap_or_else(default_views_config);
    config.ipd_m = ipd_m * crate::tune::ipd_scale();
    *state = Some(VersionedViewsConfig {
        version,
        config: config.clone(),
    });
    info!(
        "updated ALVR IPD from Pimax device info: version={} raw_ipd={:.3} physical_m={:.4} alvr_ipd_m={:.4} (scale={:.2})",
        version,
        raw_ipd,
        ipd_m,
        config.ipd_m,
        crate::tune::ipd_scale(),
    );
}

/// Called by the tune HTTP server when the IPD scale slider changes.
/// Re-applies the new scale to the last known physical IPD and bumps the
/// ViewsConfig version so the ALVR sender thread picks it up immediately.
pub(crate) fn notify_ipd_scale_changed() {
    let physical = f32::from_bits(PHYSICAL_IPD_M.load(Ordering::Relaxed));
    if physical <= 0.0 || !physical.is_finite() {
        return; // No physical IPD known yet; skip.
    }

    let mut state = match ALVR_VIEW_CONFIG_STATE.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    let version = state
        .as_ref()
        .map(|s| s.version.wrapping_add(1).max(1))
        .unwrap_or(1);
    let mut config = state
        .as_ref()
        .map(|s| s.config.clone())
        .unwrap_or_else(default_views_config);
    config.ipd_m = physical * crate::tune::ipd_scale();
    *state = Some(VersionedViewsConfig {
        version,
        config: config.clone(),
    });
    info!(
        "tune: IPD scale changed → physical_m={:.4} scale={:.2} alvr_ipd_m={:.4} version={}",
        physical,
        crate::tune::ipd_scale(),
        config.ipd_m,
        version
    );
}

fn latest_alvr_views_config() -> Option<VersionedViewsConfig> {
    ALVR_VIEW_CONFIG_STATE
        .lock()
        .ok()
        .and_then(|state| state.clone())
}

fn current_alvr_views_config() -> ViewsConfig {
    latest_alvr_views_config()
        .map(|state| state.config)
        .unwrap_or_else(default_views_config)
}

pub(crate) fn report_alvr_video_packet_received(timestamp: Duration) {
    with_alvr_statistics_state(|state| state.report_video_packet_received(timestamp));
}

pub(crate) fn report_alvr_frame_decoded(timestamp: Duration) {
    with_alvr_statistics_state(|state| state.report_frame_decoded(timestamp));
}

pub(crate) fn report_alvr_compositor_start(timestamp: Duration) {
    with_alvr_statistics_state(|state| state.report_compositor_start(timestamp));
}

pub(crate) fn report_alvr_frame_submitted(timestamp: Duration, vsync_queue: Duration) {
    let Some(stats) =
        with_alvr_statistics_state(|state| state.report_submit(timestamp, vsync_queue)).flatten()
    else {
        return;
    };

    let mut sender = match ALVR_STATISTICS_SENDER.lock() {
        Ok(sender) => sender,
        Err(_) => {
            warn!("ALVR statistics sender mutex is poisoned");
            return;
        }
    };
    let Some(sender) = sender.as_mut() else {
        return;
    };

    match sender.send_header(&stats) {
        Ok((packet_index, bytes_sent, sent_packets)) => {
            if sent_packets <= 5 || sent_packets % ALVR_STREAM_LOG_EVERY == 0 {
                info!(
                    "sent ALVR client statistics packet: packet_index={} timestamp_ns={} bytes={} sent_packets={} frame_interval_ms={:.3} decode_ms={:.3} queue_ms={:.3} render_ms={:.3} vsync_ms={:.3} total_ms={:.3}",
                    packet_index,
                    stats.target_timestamp.as_nanos(),
                    bytes_sent,
                    sent_packets,
                    stats.frame_interval.as_secs_f64() * 1000.0,
                    stats.video_decode.as_secs_f64() * 1000.0,
                    stats.video_decoder_queue.as_secs_f64() * 1000.0,
                    stats.rendering.as_secs_f64() * 1000.0,
                    stats.vsync_queue.as_secs_f64() * 1000.0,
                    stats.total_pipeline_latency.as_secs_f64() * 1000.0
                );
            }
        }
        Err(err) => warn!("failed to send ALVR client statistics packet: {err:#}"),
    }
}

fn report_alvr_tracking_input_acquired(timestamp: Duration) {
    with_alvr_statistics_state(|state| state.report_input_acquired(timestamp));
}

fn reset_alvr_statistics_state() {
    if let Ok(mut state) = ALVR_STATISTICS_STATE.lock() {
        *state = Some(AlvrClientStatisticsState::new());
    } else {
        warn!("ALVR statistics state mutex is poisoned");
    }
}

fn clear_alvr_statistics_state() {
    if let Ok(mut state) = ALVR_STATISTICS_STATE.lock() {
        *state = None;
        info!("ALVR statistics state cleared");
    } else {
        warn!("ALVR statistics state mutex is poisoned during cleanup");
    }
}

fn with_alvr_statistics_state<T>(f: impl FnOnce(&mut AlvrClientStatisticsState) -> T) -> Option<T> {
    let mut state = match ALVR_STATISTICS_STATE.lock() {
        Ok(state) => state,
        Err(_) => {
            warn!("ALVR statistics state mutex is poisoned");
            return None;
        }
    };
    let state = state.get_or_insert_with(AlvrClientStatisticsState::new);
    Some(f(state))
}

fn install_alvr_statistics_sender(
    socket: StdUdpSocket,
    max_packet_size: usize,
) -> Result<AlvrStatisticsSenderGuard> {
    let mut sender = ALVR_STATISTICS_SENDER
        .lock()
        .map_err(|_| anyhow!("ALVR statistics sender mutex is poisoned"))?;
    *sender = Some(AlvrStreamHeaderSender {
        socket,
        stream_id: ALVR_STATISTICS_STREAM_ID,
        max_packet_size,
        packet_index: 0,
        sent_packets: 0,
    });
    info!(
        "ALVR client statistics sender ready: stream_id={} max_packet_size={}",
        ALVR_STATISTICS_STREAM_ID, max_packet_size
    );
    Ok(AlvrStatisticsSenderGuard)
}

struct AlvrStatisticsSenderGuard;

impl Drop for AlvrStatisticsSenderGuard {
    fn drop(&mut self) {
        if let Ok(mut sender) = ALVR_STATISTICS_SENDER.lock() {
            *sender = None;
            info!("ALVR client statistics sender cleared");
        }
    }
}

fn join_alvr_stream_thread(name: &str, handle: JoinHandle<()>) {
    match handle.join() {
        Ok(()) => info!("ALVR stream thread joined: {name}"),
        Err(_) => warn!("ALVR stream thread panicked during join: {name}"),
    }
}

fn drain_alvr_stream_thread(name: &str, session_id: u32, handle: JoinHandle<()>) {
    let deadline = Instant::now() + ALVR_STREAM_THREAD_JOIN_GRACE;

    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    if handle.is_finished() {
        join_alvr_stream_thread(name, handle);
    } else {
        warn!(
            "ALVR stream thread did not finish within {:?}; detaching: session_id={} thread={}",
            ALVR_STREAM_THREAD_JOIN_GRACE, session_id, name
        );
        drop(handle);
    }
}

fn request_alvr_control_shutdown(
    writer: &SharedControlWriter,
    shutdown_requested: &Arc<AtomicBool>,
    reason: &str,
) {
    let already_requested = shutdown_requested.swap(true, Ordering::AcqRel);
    if !already_requested {
        warn!("requesting ALVR control session shutdown: {reason}");
    }

    match writer.lock() {
        Ok(stream) => {
            if let Err(err) = stream.shutdown(Shutdown::Both) {
                warn!("failed to shutdown ALVR control socket after {reason}: {err:#}");
            }
        }
        Err(_) => warn!("failed to lock ALVR control socket for shutdown after {reason}"),
    }
}

#[cfg(target_os = "android")]
fn release_alvr_stream_decoder() {
    crate::android_video_decoder::release_upstream_decoder();
    info!("released ALVR upstream decoder state");
}

#[cfg(not(target_os = "android"))]
fn release_alvr_stream_decoder() {}

struct AlvrStreamHeaderSender {
    socket: StdUdpSocket,
    stream_id: u16,
    max_packet_size: usize,
    packet_index: u32,
    sent_packets: u64,
}

impl AlvrStreamHeaderSender {
    fn send_header<H: Serialize>(&mut self, header: &H) -> Result<(u32, usize, u64)> {
        let packet_index = self.packet_index;
        let bytes_sent = send_alvr_stream_header_packet(
            &self.socket,
            self.stream_id,
            packet_index,
            header,
            self.max_packet_size,
        )?;
        self.packet_index = self.packet_index.wrapping_add(1);
        self.sent_packets = self.sent_packets.wrapping_add(1);
        Ok((packet_index, bytes_sent, self.sent_packets))
    }
}

struct TrackedClientFrame {
    target_timestamp: Duration,
    input_acquired: Instant,
    video_packet_received: Option<Instant>,
    frame_decoded: Option<Instant>,
    compositor_start: Option<Instant>,
    submitted: bool,
    client_stats: ClientStatistics,
}

struct AlvrClientStatisticsState {
    frames: VecDeque<TrackedClientFrame>,
    prev_vsync: Option<Instant>,
}

#[derive(Clone)]
struct VersionedViewsConfig {
    version: u64,
    config: ViewsConfig,
}

impl AlvrClientStatisticsState {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            prev_vsync: None,
        }
    }

    fn report_input_acquired(&mut self, timestamp: Duration) {
        if self.frame_mut(timestamp).is_some() {
            return;
        }

        self.frames.push_front(TrackedClientFrame {
            target_timestamp: timestamp,
            input_acquired: Instant::now(),
            video_packet_received: None,
            frame_decoded: None,
            compositor_start: None,
            submitted: false,
            client_stats: ClientStatistics {
                target_timestamp: timestamp,
                frame_interval: ALVR_DEFAULT_FRAME_INTERVAL,
                ..ClientStatistics::default()
            },
        });

        while self.frames.len() > ALVR_STATISTICS_HISTORY_SIZE {
            self.frames.pop_back();
        }
    }

    fn report_video_packet_received(&mut self, timestamp: Duration) {
        if let Some(frame) = self.frame_mut(timestamp) {
            frame.video_packet_received = Some(Instant::now());
        }
    }

    fn report_frame_decoded(&mut self, timestamp: Duration) {
        let Some(frame) = self.frame_mut(timestamp) else {
            return;
        };
        let now = Instant::now();
        if let Some(video_packet_received) = frame.video_packet_received {
            frame.client_stats.video_decode = now.saturating_duration_since(video_packet_received);
        }
        frame.frame_decoded = Some(now);
    }

    fn report_compositor_start(&mut self, timestamp: Duration) {
        let Some(frame) = self.frame_mut(timestamp) else {
            return;
        };
        let now = Instant::now();
        if let Some(frame_decoded) = frame.frame_decoded {
            frame.client_stats.video_decoder_queue = now.saturating_duration_since(frame_decoded);
        } else if let Some(video_packet_received) = frame.video_packet_received {
            frame.client_stats.video_decoder_queue = now
                .saturating_duration_since(video_packet_received + frame.client_stats.video_decode);
        }
        frame.compositor_start = Some(now);
    }

    fn report_submit(
        &mut self,
        timestamp: Duration,
        vsync_queue: Duration,
    ) -> Option<ClientStatistics> {
        let prev_vsync = self.prev_vsync;
        let Some(frame) = self.frame_mut(timestamp) else {
            return None;
        };
        if frame.submitted {
            return None;
        }

        let now = Instant::now();
        if let Some(compositor_start) = frame.compositor_start {
            frame.client_stats.rendering = now.saturating_duration_since(compositor_start);
        } else if let Some(frame_decoded) = frame.frame_decoded {
            frame.client_stats.rendering = now.saturating_duration_since(frame_decoded);
        }

        let vsync = now + vsync_queue;
        frame.client_stats.frame_interval = prev_vsync
            .map(|prev_vsync| vsync.saturating_duration_since(prev_vsync))
            .unwrap_or(ALVR_DEFAULT_FRAME_INTERVAL);
        frame.client_stats.vsync_queue = vsync_queue;
        frame.client_stats.total_pipeline_latency =
            now.saturating_duration_since(frame.input_acquired) + vsync_queue;
        frame.submitted = true;

        let stats = frame.client_stats.clone();
        self.prev_vsync = Some(vsync);
        Some(stats)
    }

    fn frame_mut(&mut self, timestamp: Duration) -> Option<&mut TrackedClientFrame> {
        self.frames
            .iter_mut()
            .find(|frame| frame.target_timestamp == timestamp)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredStreamer {
    pub addr: SocketAddr,
    pub hostname: Option<String>,
    pub protocol_id: Option<ProtocolId>,
}

pub struct SessionHandle {
    pub peer: SocketAddr,
    pub control: TcpStream,
    pub stream: TokioUdpSocket,
}

impl SessionHandle {
    pub async fn close(self) -> Result<()> {
        drop(self);
        Ok(())
    }
}

pub struct AlvrClient {
    pub config: ClientConfig,
}

impl AlvrClient {
    pub fn new(config: ClientConfig) -> Self {
        Self { config }
    }

    /// Advertise this client via mDNS so an ALVR v20 server can discover and
    /// connect back on TCP port 9943.
    ///
    /// First call registers the mDNS service; subsequent calls are no-ops
    /// because the ServiceDaemon re-announces automatically. Retries on the
    /// next call if the first attempt fails (e.g. WiFi not yet up).
    pub fn announce(&self) -> Result<()> {
        ensure_alvr_mdns_registration(&self.config, false)
    }

    pub fn refresh_announcement(&self) -> Result<()> {
        ensure_alvr_mdns_registration(&self.config, true)
    }

    pub fn send_discovery_heartbeat(&self, server_ip: Option<IpAddr>) -> Result<()> {
        let packet = DiscoveryPacket {
            protocol_id: self.config.protocol_id(),
            hostname: self.config.client_name.clone(),
        };
        let socket = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .context("bind discovery heartbeat socket")?;
        socket
            .set_broadcast(true)
            .context("enable discovery heartbeat broadcast")?;

        let mut errors = Vec::new();
        let mut sent_any = false;
        for target in discovery_heartbeat_targets(server_ip, self.config.discovery_port) {
            match socket.send_to(&packet.encode(), target) {
                Ok(_) => sent_any = true,
                Err(err) => errors.push(format!("{target}: {err}")),
            }
        }

        if sent_any {
            Ok(())
        } else {
            bail!(
                "send ALVR discovery heartbeat failed for all targets: {}",
                errors.join("; ")
            );
        }
    }

    pub async fn discover(&self, listen_timeout: Duration) -> Result<Vec<DiscoveredStreamer>> {
        let packet = DiscoveryPacket {
            protocol_id: self.config.protocol_id(),
            hostname: self.config.client_name.clone(),
        };

        let socket = TokioUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .context("bind discovery socket")?;
        socket.set_broadcast(true).context("enable broadcast")?;

        let broadcast = SocketAddr::from((Ipv4Addr::BROADCAST, self.config.discovery_port));
        socket
            .send_to(&packet.encode(), broadcast)
            .await
            .with_context(|| format!("broadcast discovery packet to {broadcast}"))?;

        let mut found = Vec::new();
        let deadline = timeout(listen_timeout, async {
            let mut buf = [0_u8; 1024];
            loop {
                let (len, addr) = socket.recv_from(&mut buf).await?;
                let response = &buf[..len];
                let decoded = DiscoveryPacket::decode(response);

                if let Some(decoded) = decoded {
                    if decoded.protocol_id == self.config.protocol_id() {
                        found.push(DiscoveredStreamer {
                            addr,
                            hostname: Some(decoded.hostname),
                            protocol_id: Some(decoded.protocol_id),
                        });
                    }
                } else {
                    found.push(DiscoveredStreamer {
                        addr,
                        hostname: None,
                        protocol_id: None,
                    });
                }
            }
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        });

        match deadline.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(_) => {}
        }

        found.sort_by(|a, b| a.addr.cmp(&b.addr));
        found.dedup_by(|a, b| a.addr == b.addr);
        Ok(found)
    }

    pub async fn connect(&self, server_ip: IpAddr) -> Result<SessionHandle> {
        let peer = SocketAddr::new(server_ip, self.config.stream_port);

        let control = TcpStream::connect(peer)
            .await
            .with_context(|| format!("connect control socket to {peer}"))?;
        control.set_nodelay(true).context("enable TCP_NODELAY")?;

        let stream = TokioUdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .await
            .context("bind stream socket")?;
        stream
            .connect(peer)
            .await
            .with_context(|| format!("connect stream socket to {peer}"))?;

        Ok(SessionHandle {
            peer,
            control,
            stream,
        })
    }

    pub async fn connect_last_known(&self) -> Result<SessionHandle> {
        let ip = self
            .config
            .last_server_ip
            .as_deref()
            .ok_or_else(|| anyhow!("no last known server IP configured"))?
            .parse::<IpAddr>()
            .context("parse last known server IP")?;
        self.connect(ip).await
    }
}

fn discovery_heartbeat_targets(server_ip: Option<IpAddr>, discovery_port: u16) -> Vec<SocketAddr> {
    let mut targets = vec![SocketAddr::from((Ipv4Addr::BROADCAST, discovery_port))];

    if let Some(IpAddr::V4(server_ip)) = server_ip {
        let target = SocketAddr::from((server_ip, discovery_port));
        if !targets.contains(&target) {
            targets.push(target);
        }
    }

    targets
}

fn shutdown_alvr_mdns_daemon(daemon: mdns_sd::ServiceDaemon) {
    if let Err(err) = daemon.shutdown() {
        warn!("mDNS: failed to shutdown previous daemon before refresh: {err:#}");
    }
}

fn ensure_alvr_mdns_registration(config: &ClientConfig, force_refresh: bool) -> Result<()> {
    let mut guard = ALVR_MDNS_DAEMON.lock().unwrap();
    if force_refresh {
        if let Some(previous) = guard.take() {
            shutdown_alvr_mdns_daemon(previous);
        }
    } else if guard.is_some() {
        return Ok(());
    }

    let local_ip = IpAddr::V4(wifi_ipv4().context("get local IPv4 for mDNS")?);
    let protocol_str = alvr_protocol_string(&config.version_string);

    let daemon = mdns_sd::ServiceDaemon::new().context("create mDNS ServiceDaemon")?;

    let service_info = mdns_sd::ServiceInfo::new(
        "_alvr._tcp.local.",
        &format!("alvr-{}", config.client_name),
        &format!("{}.local.", config.client_name),
        local_ip,
        config.discovery_port,
        &[
            ("protocol", protocol_str.as_str()),
            ("device_id", config.client_name.as_str()),
        ][..],
    )
    .context("build mDNS ServiceInfo")?;

    daemon
        .register(service_info)
        .context("register mDNS service")?;

    *guard = Some(daemon);

    let action = if force_refresh {
        "refreshed"
    } else {
        "registered"
    };
    info!(
        "mDNS: {action} _alvr._tcp.local. hostname={} addr={}:{} protocol={}",
        config.client_name, local_ip, config.discovery_port, protocol_str
    );

    Ok(())
}

fn refresh_alvr_discovery_after_control_disconnect(config: &ClientConfig) {
    let discovery_client = AlvrClient::new(config.clone());
    if let Err(err) = discovery_client.refresh_announcement() {
        warn!("ALVR discovery refresh after control disconnect failed: {err:#}");
    }

    let directed_server_ip = crate::tune::get_server_ip().parse::<IpAddr>().ok();
    if let Err(err) = discovery_client.send_discovery_heartbeat(directed_server_ip) {
        warn!("ALVR discovery heartbeat after control disconnect failed: {err:#}");
    }
}

fn next_alvr_discovery_recovery_token() -> u32 {
    loop {
        let current = ALVR_DISCOVERY_RECOVERY_GENERATION.load(Ordering::Acquire);
        let next = current.wrapping_add(1).max(1);
        match ALVR_DISCOVERY_RECOVERY_GENERATION.compare_exchange(
            current,
            next,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return next,
            Err(_) => continue,
        }
    }
}

fn try_claim_alvr_discovery_recovery(token: u32) -> bool {
    ALVR_DISCOVERY_RECOVERY_OWNER
        .compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn is_current_alvr_discovery_recovery(token: u32) -> bool {
    ALVR_DISCOVERY_RECOVERY_OWNER.load(Ordering::Acquire) == token
}

fn release_alvr_discovery_recovery(token: u32) {
    let _ = ALVR_DISCOVERY_RECOVERY_OWNER.compare_exchange(
        token,
        0,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

fn start_alvr_discovery_recovery(config: ClientConfig) {
    let token = next_alvr_discovery_recovery_token();
    if !try_claim_alvr_discovery_recovery(token) {
        return;
    }

    let spawn_result = thread::Builder::new()
        .name("alvr-discovery-recovery".to_string())
        .spawn(move || {
            for attempt in 0..ALVR_DISCOVERY_RECOVERY_ATTEMPTS {
                if !is_current_alvr_discovery_recovery(token) {
                    break;
                }

                refresh_alvr_discovery_after_control_disconnect(&config);

                if attempt + 1 < ALVR_DISCOVERY_RECOVERY_ATTEMPTS {
                    thread::sleep(ALVR_DISCOVERY_RECOVERY_INTERVAL);
                }
            }

            release_alvr_discovery_recovery(token);
        });

    if let Err(err) = spawn_result {
        release_alvr_discovery_recovery(token);
        warn!("failed to spawn ALVR discovery recovery helper: {err:#}");
    }
}

fn stop_alvr_discovery_recovery() {
    ALVR_DISCOVERY_RECOVERY_OWNER.store(0, Ordering::Release);
}

pub fn start_alvr_control_listener(config: ClientConfig) -> Result<()> {
    if ALVR_CONTROL_LISTENER_STARTED.swap(true, Ordering::SeqCst) {
        info!("ALVR control listener already started");
        return Ok(());
    }

    let listener = StdTcpListener::bind((Ipv4Addr::UNSPECIFIED, config.discovery_port))
        .with_context(|| {
            format!(
                "bind ALVR TCP control listener on 0.0.0.0:{}",
                config.discovery_port
            )
        })?;
    listener
        .set_nonblocking(false)
        .context("configure ALVR TCP control listener blocking mode")?;

    thread::Builder::new()
        .name("alvr-control-listener".to_string())
        .spawn(move || {
            info!(
                "ALVR TCP control listener waiting for server callbacks on 0.0.0.0:{}",
                config.discovery_port
            );
            for incoming in listener.incoming() {
                match incoming {
                    Ok(stream) => {
                        if let Err(err) = handle_alvr_server_control(stream, &config) {
                            warn!("ALVR server control connection ended: {err:#}");
                        } else {
                            info!("ALVR server control connection ended cleanly; listener ready for reconnect");
                        }
                        start_alvr_discovery_recovery(config.clone());
                    }
                    Err(err) => warn!("ALVR TCP control accept failed: {err:#}"),
                }
            }
        })
        .context("spawn ALVR TCP control listener thread")?;

    Ok(())
}

fn handle_alvr_server_control(mut stream: StdTcpStream, config: &ClientConfig) -> Result<()> {
    let peer = stream.peer_addr().context("query ALVR control peer")?;
    stream
        .set_nodelay(true)
        .context("enable TCP_NODELAY on ALVR control socket")?;
    stream
        .set_read_timeout(Some(HANDSHAKE_ACTION_TIMEOUT))
        .context("set ALVR control read timeout")?;
    stream
        .set_write_timeout(Some(HANDSHAKE_ACTION_TIMEOUT))
        .context("set ALVR control write timeout")?;

    info!("ALVR server connected to client control listener from {peer}");
    let capabilities = VideoStreamingCapabilities {
        // Match the Crystal panel resolution so ALVR negotiates the real per-eye size.
        default_view_resolution: glam::UVec2::splat(ALVR_BUFFER_MODE_VIEW_RESOLUTION),
        max_view_resolution: glam::UVec2::splat(ALVR_BUFFER_MODE_VIEW_RESOLUTION),
        refresh_rates: vec![72.0, 90.0],
        microphone_sample_rate: 48_000,
        foveated_encoding: true,
        encoder_high_profile: true,
        // Match the upstream Android client so the server can negotiate HDR-capable encoding.
        encoder_10_bits: true,
        encoder_av1: false,
        prefer_10bit: false,
        preferred_encoding_gamma: 1.0,
        prefer_hdr: true,
        ext_str: String::new(),
    };

    send_framed(
        &mut stream,
        &ClientConnectionResult::ConnectionAccepted(Box::new(ConnectionAcceptedInfo {
            client_protocol_id: config.protocol_id().as_u64(),
            platform_string: "Pimax Crystal OG ALVR Dev".to_string(),
            server_ip: peer.ip(),
            streaming_capabilities: Some(capabilities),
        })),
    )
    .context("send ALVR ConnectionAccepted")?;
    info!(
        "sent ALVR ConnectionAccepted to {peer}: protocol={} ({})",
        config.protocol_id(),
        config.protocol_id().as_u64()
    );

    let stream_config: StreamConfigPacket =
        recv_framed(&mut stream).context("receive ALVR stream config packet")?;
    stop_alvr_discovery_recovery();
    info!(
        "received ALVR stream config: session_json={} bytes negotiated={{view={}x{} refresh={} foveated={} wired={} hdr={}}}",
        stream_config.session.len(),
        stream_config.negotiated.view_resolution.x,
        stream_config.negotiated.view_resolution.y,
        stream_config.negotiated.refresh_rate_hint,
        stream_config.negotiated.enable_foveated_encoding,
        stream_config.negotiated.wired,
        stream_config.negotiated.enable_hdr,
    );
    crate::video_receiver::configure_hdr_stream(stream_config.negotiated.enable_hdr);

    let server_control: ServerControlPacket =
        recv_framed(&mut stream).context("receive ALVR server control packet")?;
    match server_control {
        ServerControlPacket::StartStream => {
            info!("received ALVR StartStream; opening minimal stream socket");
            run_minimal_alvr_stream(&mut stream, peer, &stream_config)
                .context("run minimal ALVR stream socket")?;
            info!("ALVR stream session returned to control handler for peer {peer}");
        }
        ServerControlPacket::Restarting => {
            info!("ALVR server requested SteamVR restart after config negotiation");
        }
        other => {
            info!("received ALVR server control packet before stream readiness: {other:?}");
        }
    }

    Ok(())
}

fn run_minimal_alvr_stream(
    stream: &mut StdTcpStream,
    peer: SocketAddr,
    stream_config: &StreamConfigPacket,
) -> Result<()> {
    let session_id = ALVR_STREAM_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    stream
        .set_read_timeout(Some(ALVR_CONTROL_RECV_TIMEOUT))
        .context("set ALVR control socket read timeout for stream session")?;
    let settings = StreamSocketSettings::from_stream_config(stream_config)?;
    let audio_settings = AudioStreamSettings::from_stream_config(stream_config);
    crate::video_receiver::configure_foveated_encoding(settings.foveated_encoding);
    #[cfg(target_os = "android")]
    crate::audio::set_negotiated_game_audio_sample_rate(
        stream_config.negotiated.game_audio_sample_rate,
    );
    if settings.protocol != StreamProtocol::Udp {
        bail!(
            "minimal stream socket only supports UDP for now; negotiated {:?}",
            settings.protocol
        );
    }

    let udp = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, settings.port))
        .with_context(|| format!("bind ALVR UDP stream socket on 0.0.0.0:{}", settings.port))?;
    configure_udp_receive_buffer(&udp);
    udp.set_nonblocking(true)
        .context("set ALVR UDP stream socket nonblocking mode")?;
    udp.connect(SocketAddr::new(peer.ip(), settings.port))
        .with_context(|| {
            format!(
                "connect ALVR UDP stream socket to {}:{}",
                peer.ip(),
                settings.port
            )
        })?;

    let game_audio_output = if audio_settings.game_audio_enabled
        && stream_config.negotiated.game_audio_sample_rate != 0
    {
        match crate::audio::start_game_audio_output(
            stream_config.negotiated.game_audio_sample_rate,
            audio_settings.game_audio_buffering,
        ) {
            Ok(output) => {
                info!(
                    "started ALVR game audio output: sample_rate={} buffering={{avg_ms={}, batch_ms={}}}",
                    stream_config.negotiated.game_audio_sample_rate,
                    audio_settings.game_audio_buffering.average_buffering_ms,
                    audio_settings.game_audio_buffering.batch_ms,
                );
                Some(output)
            }
            Err(err) => {
                warn!("failed to start ALVR game audio output: {err:#}");
                None
            }
        }
    } else {
        info!("ALVR game audio output disabled by session settings or negotiation");
        None
    };

    let microphone_capture = if audio_settings.microphone_enabled {
        match crate::audio::start_microphone_capture(
            48_000,
            settings.packet_size,
            udp.try_clone()
                .context("clone ALVR UDP stream socket for microphone capture")?,
        ) {
            Ok(capture) => {
                info!(
                    "started ALVR microphone capture: sample_rate=48000 buffering={{avg_ms={}, batch_ms={}}}",
                    audio_settings.microphone_buffering.average_buffering_ms,
                    audio_settings.microphone_buffering.batch_ms,
                );
                Some(capture)
            }
            Err(err) => {
                warn!("failed to start ALVR microphone capture: {err:#}");
                None
            }
        }
    } else {
        info!("ALVR microphone capture disabled by session settings");
        None
    };

    info!(
        "ALVR UDP stream socket ready: session_id={} local=0.0.0.0:{} peer={}:{} packet_size={}",
        session_id,
        settings.port,
        peer.ip(),
        settings.port,
        settings.packet_size
    );

    let control_writer = Arc::new(Mutex::new(
        stream
            .try_clone()
            .context("clone ALVR control socket for synchronized writer")?,
    ));

    send_framed_locked(&control_writer, &ClientControlPacket::StreamReady)
        .context("send ALVR StreamReady")?;
    let initial_views_config = current_alvr_views_config();
    send_alvr_local_view_params(&control_writer, &initial_views_config)
        .context("send initial ALVR LocalViewParams")?;
    request_alvr_idr_best_effort(
        &control_writer,
        "stream startup so the server sends DecoderConfig and a fresh keyframe",
    );
    info!(
        "sent ALVR StreamReady and initial LocalViewParams; session_id={} waiting for UDP stream shards and control keepalives",
        session_id
    );

    let video_decoder = Arc::new(VideoDecoderBridge::new());
    let decoder_config_ready = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "android")]
    crate::android_video_decoder::reset_upstream_decoder_diagnostics();
    crate::video_receiver::reset_video_render_diagnostics();
    ALVR_RUNTIME_VIDEO_RECOVERY_REQUESTED.store(false, Ordering::Release);
    *ALVR_RUNTIME_VIDEO_RECOVERY_STATE
        .lock()
        .expect("lock runtime video recovery state") = RuntimeVideoRecoveryState::default();
    let mut stream_session =
        AlvrStreamSession::new(session_id, microphone_capture, game_audio_output);
    let session_shutdown = stream_session.shutdown_signal();
    info!(
        "ALVR stream session started: session_id={} peer={} udp_port={}",
        session_id, peer, settings.port
    );

    stream_session.control_maintenance_handle = Some(
        thread::Builder::new()
            .name("alvr-control-maintenance".to_string())
            .spawn({
                let control_writer = Arc::clone(&control_writer);
                let session_shutdown = Arc::clone(&session_shutdown);
                move || maintain_alvr_control_socket(control_writer, session_shutdown)
            })
            .context("spawn ALVR control maintenance thread")?,
    );

    let receive_packet_size = settings.packet_size;
    reset_alvr_statistics_state();
    stream_session.statistics_sender_guard = Some(
        install_alvr_statistics_sender(
            udp.try_clone()
                .context("clone ALVR UDP stream socket for statistics sender")?,
            receive_packet_size,
        )
        .context("install ALVR statistics stream sender")?,
    );
    let tracking_udp = udp
        .try_clone()
        .context("clone ALVR UDP stream socket for tracking sender")?;
    stream_session.tracking_sender_handle = Some(
        thread::Builder::new()
            .name("alvr-tracking-send".to_string())
            .spawn({
                let session_shutdown = Arc::clone(&session_shutdown);
                move || {
                    send_minimal_tracking_stream(
                        tracking_udp,
                        receive_packet_size,
                        session_shutdown,
                    )
                }
            })
            .context("spawn ALVR tracking sender thread")?,
    );

    stream_session.udp_receiver_handle = Some(
        thread::Builder::new()
            .name("alvr-udp-stream-recv".to_string())
            .spawn({
                let video_decoder = Arc::clone(&video_decoder);
                let control_writer = Arc::clone(&control_writer);
                let decoder_config_ready = Arc::clone(&decoder_config_ready);
                let game_audio_output = stream_session.game_audio_output.clone();
                let session_shutdown = Arc::clone(&session_shutdown);
                move || {
                    receive_alvr_udp_stream(
                        session_id,
                        udp,
                        receive_packet_size,
                        video_decoder,
                        control_writer,
                        decoder_config_ready,
                        session_shutdown,
                        game_audio_output,
                    )
                }
            })
            .context("spawn ALVR UDP stream receiver thread")?,
    );

    let mut decoder_configured = false;
    let mut last_decoder_config: Option<(CodecType, Vec<u8>)> = None;
    loop {
        match recv_framed_until_shutdown::<ServerControlPacket>(stream, &session_shutdown) {
            Ok(ServerControlPacket::KeepAlive) => {}
            Ok(ServerControlPacket::DecoderConfig(config)) => {
                info!(
                    "received ALVR decoder config: codec={:?} config_bytes={}",
                    config.codec,
                    config.config_buffer.len()
                );

                let is_duplicate_config = last_decoder_config
                    .as_ref()
                    .is_some_and(|previous| same_decoder_config(previous, &config));
                if decoder_configured && is_duplicate_config {
                    info!(
                        "ignoring duplicate ALVR decoder config while decoder is already configured"
                    );
                    continue;
                }
                if decoder_configured {
                    info!("reconfiguring decoder after ALVR decoder config change");
                }

                let frame_width = (stream_config.negotiated.view_resolution.x * 2) as i32;
                let frame_height = stream_config.negotiated.view_resolution.y as i32;
                let decoder_config_buffer = config.config_buffer.clone();
                video_decoder
                    .configure(
                        config.codec.mime_type(),
                        config.codec.label(),
                        decoder_config_buffer.clone(),
                        frame_width,
                        frame_height,
                    )
                    .with_context(|| format!("configure decoder for {:?}", config.codec))?;
                decoder_configured = true;
                last_decoder_config = Some((config.codec, decoder_config_buffer));
                decoder_config_ready.store(true, Ordering::Release);
                complete_runtime_video_stream_recovery("decoder config received");
                request_alvr_idr_best_effort(&control_writer, "decoder configuration completed");
            }
            Ok(ServerControlPacket::ReservedBuffer(buffer)) => {
                info!(
                    "received ALVR reserved realtime config/control buffer: {} bytes",
                    buffer.len()
                );
            }
            Ok(ServerControlPacket::RealTimeConfig(rtc)) => {
                info!(
                    "received ALVR RealTimeConfig (not applied): passthrough={} post_processing={} cpu_level={} gpu_level={} ext_str_len={}",
                    rtc.passthrough.is_some(),
                    rtc.clientside_post_processing.is_some(),
                    rtc.cpu_performance_level.is_some(),
                    rtc.gpu_performance_level.is_some(),
                    rtc.ext_str.len(),
                );
            }
            Ok(ServerControlPacket::Restarting) => {
                info!("ALVR server requested SteamVR restart during stream");
                stream_session
                    .set_cleanup_reason("ALVR server requested SteamVR restart during stream");
                return Ok(());
            }
            Ok(other) => {
                info!("received ALVR control packet during stream: {other:?}");
            }
            Err(err) => {
                warn!("ALVR control receive loop ended: {err:#}");
                stream_session
                    .set_cleanup_reason(format!("ALVR control receive loop ended: {err:#}"));
                return Ok(());
            }
        }
    }
}

#[cfg(target_os = "android")]
fn configure_udp_receive_buffer(socket: &StdUdpSocket) {
    let size = ALVR_UDP_RECEIVE_BUFFER_BYTES;
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&size as *const i32).cast(),
            std::mem::size_of_val(&size) as libc::socklen_t,
        )
    };
    if result == 0 {
        info!("configured ALVR UDP SO_RCVBUF request: {size} bytes");
    } else {
        warn!(
            "failed to configure ALVR UDP SO_RCVBUF: os_error={:?}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(not(target_os = "android"))]
fn configure_udp_receive_buffer(_socket: &StdUdpSocket) {}

fn request_alvr_idr_best_effort(control_writer: &SharedControlWriter, reason: &str) {
    if let Err(err) = send_framed_locked(control_writer, &ClientControlPacket::RequestIdr) {
        warn!("failed to request ALVR IDR after {reason}: {err:#}");
    } else {
        info!("requested ALVR IDR after {reason}");
    }
}

fn send_alvr_local_view_params(
    control_writer: &SharedControlWriter,
    views_config: &ViewsConfig,
) -> Result<()> {
    send_framed_locked(
        control_writer,
        &ClientControlPacket::LocalViewParams(views_config_to_local_view_params(views_config)),
    )
}

#[cfg(target_os = "android")]
fn take_lifecycle_stream_recovery_request() -> bool {
    crate::pimax::take_alvr_stream_recovery_request()
}

#[cfg(not(target_os = "android"))]
fn take_lifecycle_stream_recovery_request() -> bool {
    false
}

fn request_runtime_video_stream_recovery(reason: &str) -> bool {
    let now = Instant::now();
    let queued = {
        let mut state = ALVR_RUNTIME_VIDEO_RECOVERY_STATE
            .lock()
            .expect("lock runtime video recovery state");
        state.queue_if_due(now)
    };

    if queued {
        ALVR_RUNTIME_VIDEO_RECOVERY_REQUESTED.store(true, Ordering::Release);
        info!("queued ALVR runtime video stream recovery: {reason}");
    } else {
        info!("suppressed duplicate ALVR runtime video stream recovery during cooldown/in-flight state: {reason}");
    }

    queued
}

fn take_runtime_video_stream_recovery_request() -> bool {
    let requested = ALVR_RUNTIME_VIDEO_RECOVERY_REQUESTED.swap(false, Ordering::AcqRel);
    if !requested {
        return false;
    }

    let mut state = ALVR_RUNTIME_VIDEO_RECOVERY_STATE
        .lock()
        .expect("lock runtime video recovery state");
    state.take_queued(Instant::now())
}

fn complete_runtime_video_stream_recovery(reason: &str) {
    let completed = {
        let mut state = ALVR_RUNTIME_VIDEO_RECOVERY_STATE
            .lock()
            .expect("lock runtime video recovery state");
        state.mark_completed(Instant::now())
    };

    if completed {
        info!("completed ALVR runtime video stream recovery: {reason}");
    }
}

fn request_alvr_idr_if_due(
    control_writer: &SharedControlWriter,
    reason: &str,
    last_request_at: &mut Option<Instant>,
) {
    let now = Instant::now();
    if let Some(previous) = *last_request_at {
        if now.duration_since(previous) < ALVR_RUNTIME_IDR_REQUEST_MIN_INTERVAL {
            return;
        }
    }

    request_alvr_idr_best_effort(control_writer, reason);
    *last_request_at = Some(now);
}

fn enter_waiting_for_idr(
    waiting_for_idr: &mut bool,
    video_assembler: &mut VideoPacketAssembler,
    last_completed_video_packet_index: &mut Option<u32>,
    reason: &str,
) {
    *waiting_for_idr = true;
    video_assembler.clear();
    *last_completed_video_packet_index = None;
    info!("reset ALVR video continuity state while waiting for next IDR: {reason}");
}

fn receive_alvr_udp_stream(
    session_id: u32,
    socket: StdUdpSocket,
    packet_size: usize,
    video_decoder: Arc<VideoDecoderBridge>,
    control_writer: SharedControlWriter,
    decoder_config_ready: Arc<AtomicBool>,
    shutdown_requested: Arc<AtomicBool>,
    game_audio_output: Option<crate::audio::GameAudioOutput>,
) {
    let session_start = Instant::now();
    let mut buffer = vec![0_u8; packet_size.max(ALVR_STREAM_SHARD_PREFIX_SIZE)];
    let mut video_assembler =
        VideoPacketAssembler::new(packet_size - ALVR_STREAM_SHARD_PREFIX_SIZE);
    let mut audio_assembler = RawPacketAssembler::new(packet_size - ALVR_STREAM_SHARD_PREFIX_SIZE);
    let mut shards = 0_u64;
    let mut video_shards = 0_u64;
    let mut audio_shards = 0_u64;
    let start = Instant::now();
    let mut waiting_for_idr = true;
    let mut last_completed_video_packet_index = None::<u32>;
    let mut last_idr_request_at = None::<Instant>;
    let mut decoder_backpressure_drops = 0_u64;
    let mut diagnostics = AlvrUdpStreamDiagnostics::default();
    let game_audio_output = game_audio_output;

    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            info!("ALVR UDP stream receiver exiting after session shutdown request");
            break;
        }

        if take_runtime_video_stream_recovery_request() {
            enter_waiting_for_idr(
                &mut waiting_for_idr,
                &mut video_assembler,
                &mut last_completed_video_packet_index,
                "native lifecycle recovery",
            );
            if video_decoder.force_stream_recovery() {
                info!("restarted ALVR decoder bridge for native lifecycle recovery");
            } else {
                warn!("failed to restart ALVR decoder bridge for native lifecycle recovery");
            }
            diagnostics.decoder_idr_unavailable_resets = diagnostics
                .decoder_idr_unavailable_resets
                .wrapping_add(1);
        }

        match socket.recv(&mut buffer) {
            Ok(len) if len >= ALVR_STREAM_SHARD_PREFIX_SIZE => {
                if shutdown_requested.load(Ordering::Acquire) {
                    info!("ALVR UDP stream receiver exiting after session shutdown request");
                    break;
                }

                shards += 1;
                diagnostics.total_shards = shards;

                let stream_id = u16::from_le_bytes(buffer[0..2].try_into().unwrap());
                let packet_index = u32::from_le_bytes(buffer[2..6].try_into().unwrap());
                let shard_count = u32::from_le_bytes(buffer[6..10].try_into().unwrap());
                let shard_index = u32::from_le_bytes(buffer[10..14].try_into().unwrap());
                let video_details = if stream_id == ALVR_VIDEO_STREAM_ID {
                    decode_video_packet_details(&buffer[ALVR_STREAM_SHARD_PREFIX_SIZE..len])
                } else {
                    None
                };

                if stream_id == ALVR_VIDEO_STREAM_ID {
                    video_shards += 1;
                    diagnostics.video_shards = video_shards;
                    if let Some(packet) = video_assembler.push(
                        packet_index,
                        shard_count,
                        shard_index,
                        &buffer[ALVR_STREAM_SHARD_PREFIX_SIZE..len],
                    ) {
                        if shutdown_requested.load(Ordering::Acquire) {
                            info!(
                                "ALVR UDP stream receiver exiting after session shutdown request"
                            );
                            break;
                        }

                        if !waiting_for_idr {
                            if let Some(previous) = last_completed_video_packet_index {
                                let expected = previous.wrapping_add(1);
                                if packet_index != expected {
                                    if packet.header.is_idr {
                                        info!(
                                            "resynchronized ALVR video stream on IDR after packet gap: expected_packet_index={} got={}",
                                            expected,
                                            packet_index
                                        );
                                    } else {
                                        diagnostics.packet_gap_resets =
                                            diagnostics.packet_gap_resets.wrapping_add(1);
                                        enter_waiting_for_idr(
                                            &mut waiting_for_idr,
                                            &mut video_assembler,
                                            &mut last_completed_video_packet_index,
                                            "video packet gap",
                                        );
                                        warn!(
                                            "detected ALVR video packet gap: expected_packet_index={} got={} - waiting for next IDR",
                                            expected,
                                            packet_index
                                        );
                                        request_alvr_idr_if_due(
                                            &control_writer,
                                            "video packet gap",
                                            &mut last_idr_request_at,
                                        );
                                    }
                                }
                            }
                        }
                        last_completed_video_packet_index = Some(packet_index);

                        if packet.completed_count <= 10
                            || packet.header.is_idr
                            || packet.completed_count % ALVR_STREAM_LOG_EVERY == 0
                        {
                            info!(
                                "completed ALVR video packet: packet_index={} shards={} timestamp_ns={} is_idr={} payload_bytes={} completed_packets={} elapsed_ms={}",
                                packet_index,
                                shard_count,
                                packet.header.timestamp.as_nanos(),
                                packet.header.is_idr,
                                packet.payload_len,
                                packet.completed_count,
                                start.elapsed().as_millis()
                            );
                        }
                        diagnostics.completed_video_packets = packet.completed_count;
                        report_alvr_video_packet_received(packet.header.timestamp);
                        if packet.header.is_idr && decoder_config_ready.load(Ordering::Acquire) {
                            waiting_for_idr = false;
                            decoder_backpressure_drops = 0;
                        }

                        if waiting_for_idr {
                            diagnostics.note_idr_request();
                            request_alvr_idr_if_due(
                                &control_writer,
                                "corrupted video stream while waiting for IDR",
                                &mut last_idr_request_at,
                            );
                            diagnostics.waiting_for_idr_drops =
                                diagnostics.waiting_for_idr_drops.wrapping_add(1);
                            if packet.completed_count <= 10
                                || packet.completed_count % ALVR_STREAM_LOG_EVERY == 0
                            {
                                warn!(
                                    "dropping ALVR video packet while waiting for IDR: packet_index={} bytes={} completed_packets={}",
                                    packet_index,
                                    packet.payload_len,
                                    packet.completed_count
                                );
                            }
                            continue;
                        }

                        if !decoder_config_ready.load(Ordering::Acquire) {
                            diagnostics.note_idr_request();
                            request_alvr_idr_if_due(
                                &control_writer,
                                "decoder configuration not yet received",
                                &mut last_idr_request_at,
                            );
                            diagnostics.pre_decoder_config_drops =
                                diagnostics.pre_decoder_config_drops.wrapping_add(1);
                            if packet.completed_count <= 10
                                || packet.completed_count % ALVR_STREAM_LOG_EVERY == 0
                            {
                                warn!(
                                    "dropping ALVR video packet before decoder config: packet_index={} bytes={} completed_packets={} is_idr={}",
                                    packet_index,
                                    packet.payload_len,
                                    packet.completed_count,
                                    packet.header.is_idr
                                );
                            }
                            continue;
                        }

                        if packet.completed_count <= 5 {
                            let probe: Vec<u8> = packet.payload.iter().take(8).copied().collect();
                            info!(
                                "ALVR video payload probe: packet_index={} len={} first_bytes={:02x?}",
                                packet_index,
                                packet.payload_len,
                                probe
                            );
                        }
                        let submitted = video_decoder.push_nal(
                            packet.header.timestamp.as_nanos().min(u128::from(u64::MAX)) as u64,
                            packet.header.is_idr,
                            packet.payload,
                        );
                        if submitted {
                            decoder_backpressure_drops = 0;
                            if packet.header.is_idr {
                                complete_runtime_video_stream_recovery(
                                    "fresh IDR accepted by decoder",
                                );
                            }
                        } else {
                            if packet.header.is_idr {
                                diagnostics.decoder_idr_unavailable_resets = diagnostics
                                    .decoder_idr_unavailable_resets
                                    .wrapping_add(1);
                            } else {
                                diagnostics.decoder_backpressure_resets = diagnostics
                                    .decoder_backpressure_resets
                                    .wrapping_add(1);
                            }
                            enter_waiting_for_idr(
                                &mut waiting_for_idr,
                                &mut video_assembler,
                                &mut last_completed_video_packet_index,
                                if packet.header.is_idr {
                                    "decoder unavailable for IDR packet"
                                } else {
                                    "decoder saturation"
                                },
                            );
                            request_alvr_idr_if_due(
                                &control_writer,
                                if packet.header.is_idr {
                                    "decoder unavailable for IDR packet"
                                } else {
                                    "decoder saturation"
                                },
                                &mut last_idr_request_at,
                            );
                            diagnostics.note_idr_request();
                            decoder_backpressure_drops = decoder_backpressure_drops.wrapping_add(1);
                            if decoder_backpressure_drops <= 5
                                || decoder_backpressure_drops % ALVR_STREAM_LOG_EVERY == 0
                            {
                                warn!(
                                    "dropping ALVR video packet after decoder backpressure: packet_index={} bytes={} consecutive_drops={} is_idr={}",
                                    packet_index,
                                    packet.payload_len,
                                    decoder_backpressure_drops,
                                    packet.header.is_idr
                                );
                            }
                        }
                    }
                } else if stream_id == ALVR_AUDIO_STREAM_ID {
                    audio_shards += 1;
                    diagnostics.audio_shards = audio_shards;
                    if let Some(payload) = audio_assembler.push(
                        packet_index,
                        shard_count,
                        shard_index,
                        &buffer[ALVR_STREAM_SHARD_PREFIX_SIZE..len],
                    ) {
                        if shutdown_requested.load(Ordering::Acquire) {
                            info!(
                                "ALVR UDP stream receiver exiting after session shutdown request"
                            );
                            break;
                        }

                        if let Some(output) = game_audio_output.as_ref() {
                            output.push_payload(&payload);
                        }
                        if audio_shards <= 10 || audio_shards % ALVR_STREAM_LOG_EVERY == 0 {
                            info!(
                                "completed ALVR audio packet: packet_index={} shards={} payload_bytes={} completed_packets={} elapsed_ms={}",
                                packet_index,
                                shard_count,
                                payload.len(),
                                audio_assembler.completed_count(),
                                start.elapsed().as_millis()
                            );
                        }
                        diagnostics.completed_audio_packets = audio_assembler.completed_count();
                    }
                }

                if shards <= 10
                    || (stream_id == ALVR_VIDEO_STREAM_ID
                        && video_shards % ALVR_STREAM_LOG_EVERY == 0)
                    || shards % (ALVR_STREAM_LOG_EVERY * 4) == 0
                {
                    info!(
                        "received ALVR stream shard: stream_id={} packet_index={} shard={}/{} udp_len={} video_details={} total_shards={} video_shards={} elapsed_ms={}",
                        stream_id,
                        packet_index,
                        shard_index + 1,
                        shard_count,
                        len,
                        video_details.as_deref().unwrap_or("n/a"),
                        shards,
                        video_shards,
                        start.elapsed().as_millis()
                    );
                }
            }
            Ok(len) => {
                diagnostics.short_datagrams = diagnostics.short_datagrams.wrapping_add(1);
                warn!("received short ALVR UDP stream datagram: {len} bytes");
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if shutdown_requested.load(Ordering::Acquire) {
                    info!("ALVR UDP stream receiver observed shutdown while idle");
                    break;
                }
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(err) => {
                diagnostics.udp_terminal_errors = diagnostics.udp_terminal_errors.wrapping_add(1);
                warn!("ALVR UDP stream receiver exiting: {err:#}");
                break;
            }
        }
    }

    log_alvr_diagnostics_summary(
        session_id,
        if shutdown_requested.load(Ordering::Acquire) {
            "session shutdown"
        } else {
            "udp receiver exit"
        },
        session_start.elapsed(),
        &diagnostics,
    );
}

fn send_minimal_tracking_stream(
    socket: StdUdpSocket,
    max_packet_size: usize,
    shutdown_requested: Arc<AtomicBool>,
) {
    let head_id = hash_string(ALVR_HEAD_PATH);
    let start = Instant::now();
    let mut next_send = Instant::now();
    let mut packet_index = 0_u32;
    let mut sent_packets = 0_u64;

    info!(
        "ALVR minimal tracking sender started: stream_id={} head_path={} head_id={:#x} interval_us={} max_packet_size={}",
        ALVR_TRACKING_STREAM_ID,
        ALVR_HEAD_PATH,
        head_id,
        ALVR_TRACKING_SEND_INTERVAL.as_micros(),
        max_packet_size
    );

    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            info!("ALVR tracking sender exiting after session shutdown request");
            break;
        }

        let now = Instant::now();
        if now < next_send {
            thread::sleep((next_send - now).min(Duration::from_millis(5)));
            continue;
        }

        let fallback_timestamp = start.elapsed();
        let latest_pose = latest_head_tracking_pose();
        let head_pose = latest_pose.unwrap_or(AlvrHeadTrackingPose {
            orientation: glam::Quat::IDENTITY,
            position: glam::Vec3::ZERO,
            timestamp: fallback_timestamp,
        });
        let timestamp = head_pose.timestamp;
        let mut device_motions = Vec::with_capacity(3);
        device_motions.push((
            head_id,
            DeviceMotion {
                pose: Pose {
                    orientation: head_pose.orientation,
                    position: head_pose.position,
                },
                linear_velocity: glam::Vec3::ZERO,
                angular_velocity: glam::Vec3::ZERO,
            },
        ));

        let controller_snapshot = crate::controller::latest_controller_state();
        device_motions.extend(crate::controller::build_controller_device_motions(
            &controller_snapshot,
        ));

        let tracking = TrackingData {
            poll_timestamp: timestamp,
            device_motions,
            hand_skeletons: [None, None],
            face: FaceData::default(),
            body: None,
            markers: Vec::new(),
        };

        match send_alvr_stream_header_packet(
            &socket,
            ALVR_TRACKING_STREAM_ID,
            packet_index,
            &tracking,
            max_packet_size,
        ) {
            Ok(bytes_sent) => {
                sent_packets = sent_packets.wrapping_add(1);
                if sent_packets <= 5 || sent_packets % ALVR_STREAM_LOG_EVERY == 0 {
                    info!(
                        "sent minimal ALVR tracking packet: packet_index={} timestamp_ns={} bytes={} sent_packets={} pose_source={} position=({:.3},{:.3},{:.3}) orientation=({:.3},{:.3},{:.3},{:.3})",
                        packet_index,
                        timestamp.as_nanos(),
                        bytes_sent,
                        sent_packets,
                        if latest_pose.is_some() { "pimax" } else { "identity" },
                        head_pose.position.x,
                        head_pose.position.y,
                        head_pose.position.z,
                        head_pose.orientation.x,
                        head_pose.orientation.y,
                        head_pose.orientation.z,
                        head_pose.orientation.w
                    );
                }
                packet_index = packet_index.wrapping_add(1);
                report_alvr_tracking_input_acquired(timestamp);
            }
            Err(err) => {
                warn!("ALVR tracking sender exiting after send failure: {err:#}");
                break;
            }
        }

        next_send += ALVR_TRACKING_SEND_INTERVAL;
        let after_send = Instant::now();
        if next_send <= after_send {
            next_send = after_send + ALVR_TRACKING_SEND_INTERVAL;
        }
    }
}

fn send_alvr_stream_header_packet<H: Serialize>(
    socket: &StdUdpSocket,
    stream_id: u16,
    packet_index: u32,
    header: &H,
    max_packet_size: usize,
) -> Result<usize> {
    let payload = bincode::serde::encode_to_vec(header, bincode::config::standard())
        .context("serialize ALVR stream header")?;
    let datagram_len = ALVR_STREAM_SHARD_PREFIX_SIZE
        .checked_add(payload.len())
        .context("ALVR stream packet length overflow")?;
    if datagram_len > max_packet_size {
        bail!(
            "ALVR stream header packet too large: {datagram_len} bytes exceeds max {max_packet_size}"
        );
    }

    let mut datagram = vec![0_u8; datagram_len];
    datagram[0..2].copy_from_slice(&stream_id.to_le_bytes());
    datagram[2..6].copy_from_slice(&packet_index.to_le_bytes());
    datagram[6..10].copy_from_slice(&1_u32.to_le_bytes());
    datagram[10..14].copy_from_slice(&0_u32.to_le_bytes());
    datagram[ALVR_STREAM_SHARD_PREFIX_SIZE..].copy_from_slice(&payload);

    let bytes_sent = socket
        .send(&datagram)
        .context("send ALVR stream header datagram")?;
    if bytes_sent != datagram_len {
        bail!("short ALVR UDP send: sent {bytes_sent} of {datagram_len} bytes");
    }

    Ok(bytes_sent)
}

fn maintain_alvr_control_socket(writer: SharedControlWriter, shutdown_requested: Arc<AtomicBool>) {
    let mut next_keepalive = Instant::now();
    let mut next_buttons_send = Instant::now();
    let mut last_views_config_version = latest_alvr_views_config().map(|state| state.version);
    let mut last_active_interaction_profiles: Option<
        Vec<crate::controller::ActiveInteractionProfile>,
    > = None;
    let mut keepalives_sent = 0_u64;
    let mut buttons_sent = 0_u64;
    let mut active_profiles_sent = 0_u64;
    let mut last_left_button_debug_key: Option<(u32, u32, bool, bool)> = None;
    let mut last_lifecycle_recovery_idr_request_at = None::<Instant>;

    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            info!("ALVR control maintenance thread exiting after session shutdown request");
            break;
        }

        let now = Instant::now();

        if now >= next_keepalive {
            if let Err(err) = send_framed_locked(&writer, &ClientControlPacket::KeepAlive) {
                warn!("ALVR control maintenance thread exiting after keepalive failure: {err:#}");
                request_alvr_control_shutdown(
                    &writer,
                    &shutdown_requested,
                    "keepalive send failure",
                );
                break;
            }
            keepalives_sent = keepalives_sent.wrapping_add(1);
            if keepalives_sent <= 5 || keepalives_sent % 20 == 0 {
                info!("sent ALVR KeepAlive on control socket: count={keepalives_sent}");
            }
            next_keepalive = now + ALVR_KEEPALIVE_INTERVAL;
        }

        if take_lifecycle_stream_recovery_request() {
            let current_views_config = current_alvr_views_config();
            if let Err(err) = send_alvr_local_view_params(&writer, &current_views_config) {
                warn!(
                    "ALVR control maintenance thread exiting after lifecycle LocalViewParams refresh failure: {err:#}"
                );
                request_alvr_control_shutdown(
                    &writer,
                    &shutdown_requested,
                    "lifecycle LocalViewParams refresh failure",
                );
                break;
            }
            info!("resent ALVR LocalViewParams after native lifecycle recovery");
            if request_runtime_video_stream_recovery("native lifecycle recovery") {
                request_alvr_idr_if_due(
                    &writer,
                    "native lifecycle recovery",
                    &mut last_lifecycle_recovery_idr_request_at,
                );
            }
        }

        if let Some(views_config) = latest_alvr_views_config() {
            if Some(views_config.version) != last_views_config_version {
                if let Err(err) = send_alvr_local_view_params(&writer, &views_config.config) {
                    warn!(
                        "ALVR control maintenance thread exiting after LocalViewParams update failure: {err:#}"
                    );
                    request_alvr_control_shutdown(
                        &writer,
                        &shutdown_requested,
                        "LocalViewParams update failure",
                    );
                    break;
                }
                info!(
                    "sent updated ALVR LocalViewParams from Pimax device info: version={}",
                    views_config.version
                );
                last_views_config_version = Some(views_config.version);
            }
        }

        if now >= next_buttons_send {
            let snapshot = crate::controller::latest_controller_state();
            let active_profiles = crate::controller::build_active_interaction_profiles(&snapshot);
            if Some(&active_profiles) != last_active_interaction_profiles.as_ref() {
                let mut profile_send_failed = false;
                if active_profiles.is_empty() {
                    info!("cleared ALVR active interaction profiles");
                    last_active_interaction_profiles = None;
                } else {
                    for profile in &active_profiles {
                        if let Err(err) = send_framed_locked(
                            &writer,
                            &ClientControlPacket::ActiveInteractionProfile {
                                device_id: profile.device_id,
                                profile_id: profile.profile_id,
                                input_ids: profile.input_ids.clone(),
                            },
                        ) {
                            warn!(
                                "ALVR control maintenance thread exiting after ActiveInteractionProfile send failure: {err:#}"
                            );
                            request_alvr_control_shutdown(
                                &writer,
                                &shutdown_requested,
                                "ActiveInteractionProfile send failure",
                            );
                            profile_send_failed = true;
                            break;
                        }
                    }
                    if profile_send_failed {
                        break;
                    }
                    active_profiles_sent = active_profiles_sent.wrapping_add(1);
                    info!(
                        "sent ALVR ActiveInteractionProfile packet set: count={} profiles={}",
                        active_profiles_sent,
                        active_profiles.len()
                    );
                    last_active_interaction_profiles = Some(active_profiles);
                }
            }

            let entries = crate::controller::build_button_entries(&snapshot);
            if !entries.is_empty() {
                let entry_count = entries.len();
                let left_button_debug_key = snapshot.left.as_ref().map(|state| {
                    (
                        state.buttons_pressed,
                        state.buttons_touched,
                        state.trigger > 0.01,
                        state.grip > 0.01,
                    )
                });
                if left_button_debug_key != last_left_button_debug_key {
                    if let Some(state) = snapshot.left.as_ref() {
                        info!(
                            "ALVR normalized left ButtonEntry stream: buttons=0x{:08x} touch=0x{:08x} trigger={:.3} grip={:.3} stick=({:.3},{:.3}) entries=[{}]",
                            state.buttons_pressed,
                            state.buttons_touched,
                            state.trigger,
                            state.grip,
                            state.thumbstick_x,
                            state.thumbstick_y,
                            crate::controller::format_button_entries_for_hand(
                                &entries,
                                crate::controller::LEFT_HAND_PATH,
                            )
                        );
                    } else {
                        info!("ALVR normalized left ButtonEntry stream: no fresh left controller");
                    }
                    last_left_button_debug_key = left_button_debug_key;
                }
                if let Err(err) =
                    send_framed_locked(&writer, &ClientControlPacket::Buttons(entries))
                {
                    warn!("ALVR control maintenance thread exiting after Buttons send failure: {err:#}");
                    request_alvr_control_shutdown(
                        &writer,
                        &shutdown_requested,
                        "Buttons send failure",
                    );
                    break;
                }
                buttons_sent = buttons_sent.wrapping_add(1);
                if buttons_sent <= 5 || buttons_sent % ALVR_STREAM_LOG_EVERY == 0 {
                    info!("sent ALVR Buttons packet: count={buttons_sent} entries={entry_count}");
                }
            }
            next_buttons_send = now + ALVR_BUTTONS_SEND_INTERVAL;
        }

        thread::sleep(Duration::from_millis(25));
    }
}

fn decode_video_packet_details(data: &[u8]) -> Option<String> {
    let (header, consumed) = bincode::serde::decode_from_slice::<VideoPacketHeader, _>(
        data,
        bincode::config::standard(),
    )
    .ok()?;
    Some(format!(
        "timestamp_ns={} is_idr={} payload_bytes={}",
        header.timestamp.as_nanos(),
        header.is_idr,
        data.len().saturating_sub(consumed)
    ))
}

struct CompletedVideoPacket {
    header: VideoPacketHeader,
    payload_len: usize,
    payload: Vec<u8>,
    completed_count: u64,
}

struct PartialVideoPacket {
    shards_count: u32,
    received: Vec<bool>,
    received_count: u32,
    data: Vec<u8>,
    first_seen: Instant,
}

struct VideoPacketAssembler {
    packets: HashMap<u32, PartialVideoPacket>,
    max_shard_data_size: usize,
    completed_count: u64,
}

impl VideoPacketAssembler {
    fn new(max_shard_data_size: usize) -> Self {
        Self {
            packets: HashMap::new(),
            max_shard_data_size,
            completed_count: 0,
        }
    }

    fn clear(&mut self) {
        self.packets.clear();
    }

    fn push(
        &mut self,
        packet_index: u32,
        shards_count: u32,
        shard_index: u32,
        shard_payload: &[u8],
    ) -> Option<CompletedVideoPacket> {
        if shards_count == 0 || shard_index >= shards_count {
            warn!(
                "dropping invalid ALVR video shard: packet_index={packet_index} shard={}/{}",
                shard_index + 1,
                shards_count
            );
            return None;
        }

        if self.packets.len() > 64 {
            let stale_before = Instant::now() - Duration::from_secs(2);
            self.packets
                .retain(|_, packet| packet.first_seen >= stale_before);
        }

        let partial = self
            .packets
            .entry(packet_index)
            .or_insert_with(|| PartialVideoPacket {
                shards_count,
                received: vec![false; shards_count as usize],
                received_count: 0,
                data: Vec::new(),
                first_seen: Instant::now(),
            });

        if partial.shards_count != shards_count {
            warn!(
                "dropping ALVR video shard with inconsistent shard count: packet_index={packet_index} got={shards_count} expected={}",
                partial.shards_count
            );
            return None;
        }

        let shard_index_usize = shard_index as usize;
        if partial.received[shard_index_usize] {
            return None;
        }

        let offset = shard_index_usize.checked_mul(self.max_shard_data_size)?;
        let end = offset.checked_add(shard_payload.len())?;
        if partial.data.len() < end {
            partial.data.resize(end, 0);
        }
        partial.data[offset..end].copy_from_slice(shard_payload);
        partial.received[shard_index_usize] = true;
        partial.received_count += 1;

        if partial.received_count != partial.shards_count {
            return None;
        }

        let partial = self.packets.remove(&packet_index)?;
        let data = partial.data.as_slice();
        let (header, consumed) = match bincode::serde::decode_from_slice::<VideoPacketHeader, _>(
            data,
            bincode::config::standard(),
        ) {
            Ok(result) => result,
            Err(err) => {
                warn!(
                    "failed to decode completed ALVR video packet header for packet_index={packet_index}: {err:#}"
                );
                return None;
            }
        };

        let payload = data[consumed..].to_vec();
        self.completed_count += 1;
        Some(CompletedVideoPacket {
            header,
            payload_len: payload.len(),
            payload,
            completed_count: self.completed_count,
        })
    }

    fn completed_count(&self) -> u64 {
        self.completed_count
    }
}

struct PartialRawPacket {
    shards_count: u32,
    received: Vec<bool>,
    received_count: u32,
    data: Vec<u8>,
    first_seen: Instant,
}

struct RawPacketAssembler {
    packets: HashMap<u32, PartialRawPacket>,
    max_shard_data_size: usize,
    completed_count: u64,
}

impl RawPacketAssembler {
    fn new(max_shard_data_size: usize) -> Self {
        Self {
            packets: HashMap::new(),
            max_shard_data_size,
            completed_count: 0,
        }
    }

    fn push(
        &mut self,
        packet_index: u32,
        shards_count: u32,
        shard_index: u32,
        shard_payload: &[u8],
    ) -> Option<Vec<u8>> {
        if shards_count == 0 || shard_index >= shards_count {
            warn!(
                "dropping invalid ALVR audio shard: packet_index={packet_index} shard={}/{}",
                shard_index + 1,
                shards_count
            );
            return None;
        }

        if self.packets.len() > 64 {
            let stale_before = Instant::now() - Duration::from_secs(2);
            self.packets
                .retain(|_, packet| packet.first_seen >= stale_before);
        }

        let partial = self
            .packets
            .entry(packet_index)
            .or_insert_with(|| PartialRawPacket {
                shards_count,
                received: vec![false; shards_count as usize],
                received_count: 0,
                data: Vec::new(),
                first_seen: Instant::now(),
            });

        if partial.shards_count != shards_count {
            warn!(
                "dropping ALVR audio shard with inconsistent shard count: packet_index={packet_index} got={shards_count} expected={}",
                partial.shards_count
            );
            return None;
        }

        let shard_index_usize = shard_index as usize;
        if partial.received[shard_index_usize] {
            return None;
        }

        let offset = shard_index_usize.checked_mul(self.max_shard_data_size)?;
        let end = offset.checked_add(shard_payload.len())?;
        if partial.data.len() < end {
            partial.data.resize(end, 0);
        }
        partial.data[offset..end].copy_from_slice(shard_payload);
        partial.received[shard_index_usize] = true;
        partial.received_count += 1;

        if partial.received_count != partial.shards_count {
            return None;
        }

        let partial = self.packets.remove(&packet_index)?;
        self.completed_count += 1;
        Some(partial.data)
    }

    fn completed_count(&self) -> u64 {
        self.completed_count
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamProtocol {
    Udp,
    Tcp,
}

#[derive(Clone, Copy, Debug)]
struct StreamSocketSettings {
    protocol: StreamProtocol,
    port: u16,
    packet_size: usize,
    foveated_encoding: Option<crate::video_receiver::FoveatedEncodingConfig>,
}

#[derive(Clone, Copy, Debug)]
struct AudioStreamSettings {
    game_audio_enabled: bool,
    game_audio_buffering: crate::audio::AudioBufferingConfig,
    microphone_enabled: bool,
    microphone_buffering: crate::audio::AudioBufferingConfig,
}

impl StreamSocketSettings {
    fn from_stream_config(packet: &StreamConfigPacket) -> Result<Self> {
        let session: serde_json::Value =
            serde_json::from_str(&packet.session).context("parse ALVR session JSON")?;

        let connection = session
            .pointer("/session_settings/connection")
            .or_else(|| session.pointer("/connection"));

        let port = connection
            .and_then(|value| value.get("stream_port"))
            .and_then(|value| value.as_u64())
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(9944);
        let packet_size = connection
            .and_then(|value| value.get("packet_size"))
            .and_then(|value| value.as_u64())
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(1400);

        let protocol = if packet.negotiated.wired {
            StreamProtocol::Tcp
        } else {
            match connection
                .and_then(|value| value.get("stream_protocol"))
                .and_then(|value| value.get("variant"))
                .and_then(|value| value.as_str())
                .unwrap_or("Udp")
            {
                "Tcp" => StreamProtocol::Tcp,
                _ => StreamProtocol::Udp,
            }
        };
        // Skip the more expensive openvr_config parse if the negotiated config
        // already tells us foveation is off.
        let foveated_encoding = if packet.negotiated.enable_foveated_encoding {
            parse_foveated_encoding(&session)
        } else {
            None
        };

        Ok(Self {
            protocol,
            port,
            packet_size,
            foveated_encoding,
        })
    }
}

impl AudioStreamSettings {
    fn from_stream_config(packet: &StreamConfigPacket) -> Self {
        let session: serde_json::Value = serde_json::from_str(&packet.session).unwrap_or_default();
        let audio = session
            .pointer("/session_settings/audio")
            .or_else(|| session.pointer("/audio"));

        fn buffering_from(
            parent: Option<&serde_json::Value>,
            default_average_buffering_ms: u64,
            default_batch_ms: u64,
        ) -> crate::audio::AudioBufferingConfig {
            let buffering = parent.and_then(|value| value.get("buffering"));
            crate::audio::AudioBufferingConfig {
                average_buffering_ms: buffering
                    .and_then(|value| value.get("average_buffering_ms"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(default_average_buffering_ms),
                batch_ms: buffering
                    .and_then(|value| value.get("batch_ms"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(default_batch_ms),
            }
        }

        let game_audio = audio.and_then(|value| value.get("game_audio"));
        let microphone = audio.and_then(|value| value.get("microphone"));

        Self {
            game_audio_enabled: game_audio
                .and_then(|value| value.get("enabled"))
                .and_then(|value| value.as_bool())
                .unwrap_or(true),
            game_audio_buffering: buffering_from(game_audio, 50, 5),
            microphone_enabled: microphone
                .and_then(|value| value.get("enabled"))
                .and_then(|value| value.as_bool())
                .unwrap_or(true),
            microphone_buffering: buffering_from(microphone, 50, 5),
        }
    }
}

fn parse_foveated_encoding(
    session: &serde_json::Value,
) -> Option<crate::video_receiver::FoveatedEncodingConfig> {
    let openvr = session.pointer("/openvr_config")?;
    let enabled = openvr
        .get("enable_foveated_encoding")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    if !enabled {
        return None;
    }

    let get_f32 = |name: &str| {
        openvr
            .get(name)
            .and_then(|value| value.as_f64())
            .map(|value| value as f32)
    };
    let get_u32 = |primary: &str, fallback: &str| {
        openvr
            .get(primary)
            .or_else(|| openvr.get(fallback))
            .and_then(|value| value.as_u64())
            .and_then(|value| u32::try_from(value).ok())
    };

    // ALVR session JSON exposes both the transcoded eye resolution and the
    // emulated headset target resolution. The foveated stream layout is based
    // on the encoded/transcoded eye resolution, not the headset target size.
    // Prefer eye_resolution_* and fall back to target_eye_resolution_* only for
    // older sessions.
    let Some(expanded_view_width) = get_u32("eye_resolution_width", "target_eye_resolution_width")
    else {
        warn!("ALVR foveated encoding is enabled but stream config has no encoded eye width");
        return None;
    };
    let Some(expanded_view_height) =
        get_u32("eye_resolution_height", "target_eye_resolution_height")
    else {
        warn!("ALVR foveated encoding is enabled but stream config has no encoded eye height");
        return None;
    };

    let config = crate::video_receiver::FoveatedEncodingConfig {
        expanded_view_width,
        expanded_view_height,
        center_size_x: get_f32("foveation_center_size_x").unwrap_or(0.45),
        center_size_y: get_f32("foveation_center_size_y").unwrap_or(0.4),
        center_shift_x: get_f32("foveation_center_shift_x").unwrap_or(0.0),
        center_shift_y: get_f32("foveation_center_shift_y").unwrap_or(0.0),
        edge_ratio_x: get_f32("foveation_edge_ratio_x").unwrap_or(4.0),
        edge_ratio_y: get_f32("foveation_edge_ratio_y").unwrap_or(5.0),
    };
    info!("parsed ALVR foveated encoding config from stream session: {config:?}");
    Some(config)
}

fn send_framed<S: Serialize>(stream: &mut StdTcpStream, packet: &S) -> Result<()> {
    let payload = bincode::serde::encode_to_vec(packet, bincode::config::standard())
        .context("serialize ALVR framed packet")?;
    let len = u32::try_from(payload.len()).context("ALVR framed packet too large")?;
    stream
        .write_all(&len.to_le_bytes())
        .context("write ALVR frame length")?;
    stream
        .write_all(&payload)
        .context("write ALVR frame payload")?;
    Ok(())
}

fn send_framed_locked<S: Serialize>(writer: &SharedControlWriter, packet: &S) -> Result<()> {
    let mut stream = writer
        .lock()
        .map_err(|_| anyhow!("ALVR control writer mutex is poisoned"))?;
    send_framed(&mut stream, packet)
}

fn read_exact_until_shutdown(
    stream: &mut StdTcpStream,
    buffer: &mut [u8],
    shutdown_requested: &Arc<AtomicBool>,
    context: &'static str,
) -> Result<()> {
    let mut read = 0;
    while read < buffer.len() {
        if shutdown_requested.load(Ordering::Acquire) {
            bail!("ALVR control session shutdown requested while waiting to {context}");
        }

        wait_for_alvr_control_read_ready(stream, shutdown_requested, context)?;

        match read_alvr_control_socket(stream, &mut buffer[read..]) {
            Ok(0) => bail!("ALVR control socket closed while trying to {context}"),
            Ok(bytes_read) => {
                read += bytes_read;
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(err) => return Err(err).with_context(|| format!("{context}")),
        }
    }

    Ok(())
}

#[cfg(target_os = "android")]
fn read_alvr_control_socket(
    stream: &mut StdTcpStream,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    let result = unsafe {
        libc::recv(
            stream.as_raw_fd(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            libc::MSG_DONTWAIT,
        )
    };

    if result >= 0 {
        return Ok(result as usize);
    }

    Err(std::io::Error::last_os_error())
}

#[cfg(not(target_os = "android"))]
fn read_alvr_control_socket(
    stream: &mut StdTcpStream,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    stream.read(buffer)
}

#[cfg(target_os = "android")]
fn wait_for_alvr_control_read_ready(
    stream: &StdTcpStream,
    shutdown_requested: &Arc<AtomicBool>,
    context: &'static str,
) -> Result<()> {
    const ALVR_CONTROL_POLL_INTERVAL_MS: i32 = 100;

    loop {
        if shutdown_requested.load(Ordering::Acquire) {
            bail!("ALVR control session shutdown requested while waiting to {context}");
        }

        let mut poll_fd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll_fd, 1, ALVR_CONTROL_POLL_INTERVAL_MS) };

        if result > 0 {
            if poll_fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                bail!("ALVR control socket became invalid while trying to {context}");
            }

            return Ok(());
        }

        if result == 0 {
            continue;
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }

        return Err(err).with_context(|| format!("poll ALVR control socket to {context}"));
    }
}

#[cfg(not(target_os = "android"))]
fn wait_for_alvr_control_read_ready(
    _stream: &StdTcpStream,
    shutdown_requested: &Arc<AtomicBool>,
    context: &'static str,
) -> Result<()> {
    if shutdown_requested.load(Ordering::Acquire) {
        bail!("ALVR control session shutdown requested while waiting to {context}");
    }

    Ok(())
}

fn recv_framed<R: DeserializeOwned>(stream: &mut StdTcpStream) -> Result<R> {
    let mut len_bytes = [0_u8; 4];
    stream
        .read_exact(&mut len_bytes)
        .context("read ALVR frame length")?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > 64 * 1024 * 1024 {
        bail!("ALVR frame too large: {len} bytes");
    }

    let mut payload = vec![0_u8; len];
    stream
        .read_exact(&mut payload)
        .context("read ALVR frame payload")?;
    let (value, _consumed) =
        bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .context("deserialize ALVR framed packet")?;
    Ok(value)
}

fn recv_framed_until_shutdown<R: DeserializeOwned>(
    stream: &mut StdTcpStream,
    shutdown_requested: &Arc<AtomicBool>,
) -> Result<R> {
    let mut len_bytes = [0_u8; 4];
    read_exact_until_shutdown(
        stream,
        &mut len_bytes,
        shutdown_requested,
        "read ALVR frame length",
    )?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > 64 * 1024 * 1024 {
        bail!("ALVR frame too large: {len} bytes");
    }

    let mut payload = vec![0_u8; len];
    read_exact_until_shutdown(
        stream,
        &mut payload,
        shutdown_requested,
        "read ALVR frame payload",
    )?;
    let (value, _consumed) =
        bincode::serde::decode_from_slice(&payload, bincode::config::standard())
            .context("deserialize ALVR framed packet")?;
    Ok(value)
}

fn same_decoder_config(
    previous: &(CodecType, Vec<u8>),
    current: &DecoderInitializationConfig,
) -> bool {
    previous.0 == current.codec && previous.1 == current.config_buffer
}

#[derive(Serialize, Deserialize)]
enum ClientConnectionResult {
    ConnectionAccepted(Box<ConnectionAcceptedInfo>),
    ClientStandby,
}

#[derive(Serialize, Deserialize)]
struct ConnectionAcceptedInfo {
    client_protocol_id: u64,
    platform_string: String,
    server_ip: IpAddr,
    streaming_capabilities: Option<VideoStreamingCapabilities>,
}

#[derive(Serialize, Deserialize, Clone)]
struct VideoStreamingCapabilities {
    default_view_resolution: glam::UVec2,
    max_view_resolution: glam::UVec2,
    refresh_rates: Vec<f32>,
    microphone_sample_rate: u32,
    foveated_encoding: bool,
    encoder_high_profile: bool,
    encoder_10_bits: bool,
    encoder_av1: bool,
    prefer_10bit: bool,
    preferred_encoding_gamma: f32,
    prefer_hdr: bool,
    ext_str: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct NegotiatedStreamingConfig {
    view_resolution: glam::UVec2,
    refresh_rate_hint: f32,
    game_audio_sample_rate: u32,
    enable_foveated_encoding: bool,
    encoding_gamma: f32,
    enable_hdr: bool,
    wired: bool,
    ext_str: String,
}

#[derive(Serialize, Deserialize)]
struct StreamConfigPacket {
    session: String,
    negotiated: NegotiatedStreamingConfig,
}

// ServerControlPacket is only received on the wire, never sent — so we only
// need `Deserialize` and no Serialize for the RealTimeConfig subtree.
#[derive(Deserialize, Debug)]
enum ServerControlPacket {
    StartStream,
    DecoderConfig(DecoderInitializationConfig),
    Restarting,
    KeepAlive,
    RealTimeConfig(RealTimeConfig),
    Reserved(String),
    ReservedBuffer(Vec<u8>),
}

#[derive(Deserialize, Debug)]
struct DecoderInitializationConfig {
    codec: CodecType,
    config_buffer: Vec<u8>,
    #[allow(dead_code)]
    ext_str: String,
}

// RealTimeConfig and nested types exist only so bincode can deserialize a
// `ServerControlPacket::RealTimeConfig(...)` without failing. The pimax client
// does not apply any of these values yet — field order must match
// `D:/Code/ALVR/alvr/session/src/settings.rs` verbatim or the server packet
// will fail to decode mid-stream.
#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct RealTimeConfig {
    passthrough: Option<PassthroughMode>,
    clientside_post_processing: Option<ClientsidePostProcessingConfig>,
    cpu_performance_level: Option<PerformanceLevel>,
    gpu_performance_level: Option<PerformanceLevel>,
    ext_str: String,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
enum PassthroughMode {
    Blend {
        premultiplied_alpha: bool,
        threshold: f32,
    },
    RgbChromaKey(RgbChromaKeyConfig),
    HsvChromaKey(HsvChromaKeyConfig),
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct RgbChromaKeyConfig {
    red: u8,
    green: u8,
    blue: u8,
    distance_threshold: u8,
    feathering: f32,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct HsvChromaKeyConfig {
    hue_start_max_deg: f32,
    hue_start_min_deg: f32,
    hue_end_min_deg: f32,
    hue_end_max_deg: f32,
    saturation_start_max: f32,
    saturation_start_min: f32,
    saturation_end_min: f32,
    saturation_end_max: f32,
    value_start_max: f32,
    value_start_min: f32,
    value_end_min: f32,
    value_end_max: f32,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct ClientsidePostProcessingConfig {
    super_sampling: ClientsidePostProcessingSuperSamplingMode,
    sharpening: ClientsidePostProcessingSharpeningMode,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
enum ClientsidePostProcessingSuperSamplingMode {
    Disabled,
    Normal,
    Quality,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
enum ClientsidePostProcessingSharpeningMode {
    Disabled,
    Normal,
    Quality,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
enum PerformanceLevel {
    PowerSavings,
    SustainedLow,
    SustainedHigh,
    Boost,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[allow(dead_code)]
struct DynamicFoveationParams {
    center_shift_x: f32,
    center_shift_y: f32,
    frame_sequence: u64,
}

#[derive(Serialize, Deserialize)]
struct VideoPacketHeader {
    timestamp: Duration,
    global_view_params: [ViewParams; 2],
    is_idr: bool,
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
enum CodecType {
    H264 = 0,
    Hevc = 1,
    AV1 = 2,
}

impl CodecType {
    fn mime_type(self) -> &'static str {
        match self {
            Self::H264 => "video/avc",
            Self::Hevc => "video/hevc",
            Self::AV1 => "video/av01",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::H264 => "H264",
            Self::Hevc => "HEVC",
            Self::AV1 => "AV1",
        }
    }
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Copy)]
struct Fov {
    left: f32,
    right: f32,
    up: f32,
    down: f32,
}

impl Default for Fov {
    fn default() -> Self {
        Self {
            left: -1.0,
            right: 1.0,
            up: 1.0,
            down: -1.0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug)]
pub(crate) struct Pose {
    pub orientation: glam::Quat,
    pub position: glam::Vec3,
}

#[derive(Serialize, Deserialize, Clone, Copy, Default, Debug)]
pub(crate) struct DeviceMotion {
    pub pose: Pose,
    pub linear_velocity: glam::Vec3,
    pub angular_velocity: glam::Vec3,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
struct ViewParams {
    pose: Pose,
    fov: Fov,
}

impl ViewParams {
    const DUMMY: Self = Self {
        pose: Pose {
            orientation: glam::Quat::IDENTITY,
            position: glam::Vec3::ZERO,
        },
        fov: Fov {
            left: -1.0,
            right: 1.0,
            up: 1.0,
            down: -1.0,
        },
    };
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[allow(dead_code)]
enum FaceExpressions {
    Fb(Vec<f32>),
    Bd(Vec<f32>),
    Htc {
        eye: Option<Vec<f32>>,
        lip: Option<Vec<f32>>,
    },
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct FaceData {
    eyes_combined: Option<glam::Quat>,
    eyes_social: [Option<glam::Quat>; 2],
    face_expressions: Option<FaceExpressions>,
}

#[derive(Serialize, Deserialize)]
struct TrackingData {
    poll_timestamp: Duration,
    device_motions: Vec<(u64, DeviceMotion)>,
    hand_skeletons: [Option<[Pose; 26]>; 2],
    face: FaceData,
    // ALVR master wire format carries `Option<BodySkeleton>` here. This client
    // never sends body tracking, so `Option<()>::None` (one 0 tag byte) is
    // wire-identical to `Option<BodySkeleton>::None`.
    body: Option<()>,
    markers: Vec<(String, Pose)>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct ClientStatistics {
    target_timestamp: Duration,
    frame_interval: Duration,
    video_decode: Duration,
    video_decoder_queue: Duration,
    rendering: Duration,
    vsync_queue: Duration,
    total_pipeline_latency: Duration,
}

#[derive(Serialize, Deserialize, Clone)]
struct ViewsConfig {
    ipd_m: f32,
    fov: [Fov; 2],
}

fn default_views_config() -> ViewsConfig {
    ViewsConfig {
        ipd_m: 0.064,
        fov: [Fov::default(), Fov::default()],
    }
}

/// Convert the Pimax-side `ViewsConfig { ipd_m, fov }` into the master ALVR
/// `[ViewParams; 2]` shape expected by `ClientControlPacket::LocalViewParams`.
/// The poses are head-local: each eye is offset by ±(ipd/2) along X.
fn views_config_to_local_view_params(config: &ViewsConfig) -> [ViewParams; 2] {
    let half_ipd = config.ipd_m * 0.5;
    [
        ViewParams {
            pose: Pose {
                orientation: glam::Quat::IDENTITY,
                position: glam::Vec3::new(-half_ipd, 0.0, 0.0),
            },
            fov: config.fov[0],
        },
        ViewParams {
            pose: Pose {
                orientation: glam::Quat::IDENTITY,
                position: glam::Vec3::new(half_ipd, 0.0, 0.0),
            },
            fov: config.fov[1],
        },
    ]
}

#[derive(Serialize, Deserialize, Clone)]
struct BatteryInfo {
    device_id: u64,
    gauge_value: f32,
    is_plugged: bool,
}

// Matches `alvr_common::logging::LogSeverity`. We never emit `Log` today but
// need a concrete type for the variant so the enum tag indices stay correct.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq, Eq)]
#[allow(dead_code)]
enum LogSeverity {
    Error,
    Warning,
    Info,
    Debug,
}

#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
enum ClientControlPacket {
    PlayspaceSync(Option<glam::Vec2>),
    RequestIdr,
    KeepAlive,
    StreamReady,
    LocalViewParams([ViewParams; 2]),
    Battery(BatteryInfo),
    Buttons(Vec<crate::controller::ButtonEntry>),
    ActiveInteractionProfile {
        device_id: u64,
        profile_id: u64,
        input_ids: std::collections::HashSet<u64>,
    },
    Log {
        level: LogSeverity,
        message: String,
    },
    ProximityState(bool),
    Reserved(String),
    ReservedBuffer(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DISCOVERY_PORT;
    use std::sync::LazyLock;

    static DISCOVERY_RECOVERY_TEST_GUARD: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn discovered_streamer_can_be_debugged() {
        let streamer = DiscoveredStreamer {
            addr: SocketAddr::from(([192, 168, 1, 5], DISCOVERY_PORT)),
            hostname: Some("pimax".to_string()),
            protocol_id: None,
        };
        let text = format!("{streamer:?}");
        assert!(text.contains("pimax"));
    }

    // Regression: ALVR v20 mDNS protocol TXT record uses the major version only
    // for stable releases ("20"), and "<major>-<pre>" for prereleases. Anything
    // else and the server filters us out of discovery.
    #[test]
    fn alvr_protocol_string_stable_uses_major_only() {
        assert_eq!(alvr_protocol_string("20.14.1"), "20");
        assert_eq!(alvr_protocol_string("21.0.0"), "21");
    }

    #[test]
    fn alvr_protocol_string_prerelease_appends_pre_tag() {
        assert_eq!(alvr_protocol_string("20.14.1-alpha.1"), "20-alpha.1");
        assert_eq!(alvr_protocol_string("21.0.0-rc.2"), "21-rc.2");
    }

    #[test]
    fn alvr_protocol_string_unparseable_falls_back_to_input() {
        assert_eq!(alvr_protocol_string("not-a-version"), "not-a-version");
    }

    #[test]
    fn discovery_heartbeat_targets_include_broadcast_and_directed_ipv4() {
        let targets =
            discovery_heartbeat_targets(Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 44))), 9943);

        assert_eq!(
            targets,
            vec![
                SocketAddr::from((Ipv4Addr::BROADCAST, 9943)),
                SocketAddr::from((Ipv4Addr::new(192, 168, 1, 44), 9943)),
            ]
        );
    }

    #[test]
    fn discovery_heartbeat_targets_ignore_non_ipv4_directed_targets() {
        let targets = discovery_heartbeat_targets(
            Some(IpAddr::V6("fe80::1".parse().unwrap())),
            DISCOVERY_PORT,
        );

        assert_eq!(
            targets,
            vec![SocketAddr::from((Ipv4Addr::BROADCAST, DISCOVERY_PORT))]
        );
    }

    #[test]
    fn discovery_recovery_owner_can_only_be_released_by_current_token() {
        let _guard = DISCOVERY_RECOVERY_TEST_GUARD.lock().unwrap();
        ALVR_DISCOVERY_RECOVERY_OWNER.store(0, Ordering::Release);

        let first = next_alvr_discovery_recovery_token();
        let second = next_alvr_discovery_recovery_token();

        assert!(try_claim_alvr_discovery_recovery(first));
        assert!(!try_claim_alvr_discovery_recovery(second));

        release_alvr_discovery_recovery(second);
        assert!(is_current_alvr_discovery_recovery(first));

        stop_alvr_discovery_recovery();
        assert!(!is_current_alvr_discovery_recovery(first));

        assert!(try_claim_alvr_discovery_recovery(second));
        release_alvr_discovery_recovery(first);
        assert!(is_current_alvr_discovery_recovery(second));

        release_alvr_discovery_recovery(second);
        assert_eq!(ALVR_DISCOVERY_RECOVERY_OWNER.load(Ordering::Acquire), 0);
    }

    #[test]
    fn discovery_recovery_tokens_never_use_zero() {
        let _guard = DISCOVERY_RECOVERY_TEST_GUARD.lock().unwrap();
        ALVR_DISCOVERY_RECOVERY_GENERATION.store(u32::MAX, Ordering::Release);

        let token = next_alvr_discovery_recovery_token();

        assert_ne!(token, 0);
        ALVR_DISCOVERY_RECOVERY_GENERATION.store(1, Ordering::Release);
    }

    #[test]
    fn diagnostics_classification_prefers_compositor_failures() {
        let stream = AlvrUdpStreamDiagnostics {
            decoder_backpressure_resets: 3,
            ..Default::default()
        };
        let render = crate::video_receiver::VideoRenderDiagnosticsSnapshot {
            zero_copy_failure_count: 1,
            ..Default::default()
        };

        assert_eq!(
            classify_alvr_failure_class(&stream, &render),
            "compositor-submit"
        );
    }

    #[test]
    fn diagnostics_classification_marks_decoder_recovery() {
        let stream = AlvrUdpStreamDiagnostics {
            waiting_for_idr_drops: 2,
            ..Default::default()
        };

        assert_eq!(
            classify_alvr_failure_class(&stream, &Default::default()),
            "decoder-or-stream-recovery"
        );
    }

    #[test]
    fn diagnostics_classification_marks_packet_gaps_without_decoder_signals() {
        let stream = AlvrUdpStreamDiagnostics {
            packet_gap_resets: 1,
            ..Default::default()
        };

        assert_eq!(
            classify_alvr_failure_class(&stream, &Default::default()),
            "network-packet-gap"
        );
    }

    // Regression: bincode 2 with `config::standard()` encodes enum variant
    // indices as varints. The ALVR master server matches by ordinal — reorder
    // the variants and the server mis-decodes every control packet. Variant
    // indices must match `alvr_packets::ClientControlPacket` in D:/Code/ALVR.
    #[test]
    fn client_control_packet_variant_indices_match_alvr_master() {
        fn variant_index(packet: ClientControlPacket) -> u8 {
            let bytes = bincode::serde::encode_to_vec(&packet, bincode::config::standard())
                .expect("serialize control packet");
            assert!(!bytes.is_empty());
            bytes[0]
        }

        assert_eq!(variant_index(ClientControlPacket::PlayspaceSync(None)), 0);
        assert_eq!(variant_index(ClientControlPacket::RequestIdr), 1);
        assert_eq!(variant_index(ClientControlPacket::KeepAlive), 2);
        assert_eq!(variant_index(ClientControlPacket::StreamReady), 3);
        assert_eq!(
            variant_index(ClientControlPacket::LocalViewParams([ViewParams::DUMMY; 2])),
            4,
        );
        assert_eq!(variant_index(ClientControlPacket::Buttons(vec![])), 6);
    }

    // Guard the VideoPacketHeader wire format so refactors don't silently
    // desync the inline protocol from upstream ALVR packets.
    #[test]
    fn video_packet_header_roundtrip_preserves_fields() {
        let header = VideoPacketHeader {
            timestamp: Duration::from_nanos(1_234_567_890),
            global_view_params: [ViewParams::DUMMY; 2],
            is_idr: true,
        };
        let bytes = bincode::serde::encode_to_vec(&header, bincode::config::standard())
            .expect("serialize video packet header");
        let (decoded, _): (VideoPacketHeader, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
                .expect("deserialize video packet header");
        assert_eq!(decoded.timestamp, header.timestamp);
        assert_eq!(decoded.is_idr, header.is_idr);
        assert_eq!(
            decoded.global_view_params[0].pose.position,
            header.global_view_params[0].pose.position
        );
    }

    #[test]
    fn stream_session_drop_requests_shutdown_and_clears_statistics_state() {
        reset_alvr_statistics_state();

        let shutdown_requested = {
            let session = AlvrStreamSession::new(99, None, None);
            let shutdown_requested = session.shutdown_signal();
            assert!(!shutdown_requested.load(Ordering::Acquire));
            shutdown_requested
        };

        assert!(shutdown_requested.load(Ordering::Acquire));
        assert!(ALVR_STATISTICS_STATE.lock().unwrap().is_none());
    }

    #[test]
    fn enter_waiting_for_idr_clears_video_continuity_state() {
        let mut waiting_for_idr = false;
        let mut video_assembler = VideoPacketAssembler::new(16);
        video_assembler.packets.insert(
            7,
            PartialVideoPacket {
                shards_count: 2,
                received: vec![true, false],
                received_count: 1,
                data: vec![1, 2, 3],
                first_seen: Instant::now(),
            },
        );
        let mut last_completed_video_packet_index = Some(42);

        enter_waiting_for_idr(
            &mut waiting_for_idr,
            &mut video_assembler,
            &mut last_completed_video_packet_index,
            "test reset",
        );

        assert!(waiting_for_idr);
        assert!(video_assembler.packets.is_empty());
        assert_eq!(last_completed_video_packet_index, None);
    }

    #[test]
    fn request_alvr_control_shutdown_marks_session_and_closes_socket() {
        let listener =
            StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback control socket");
        let addr = listener.local_addr().expect("listener local addr");
        let client = StdTcpStream::connect(addr).expect("connect loopback control socket");
        let (mut server, _) = listener.accept().expect("accept loopback control socket");
        server
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("set server read timeout");

        let writer = Arc::new(Mutex::new(client));
        let shutdown_requested = Arc::new(AtomicBool::new(false));

        request_alvr_control_shutdown(&writer, &shutdown_requested, "unit test");

        assert!(shutdown_requested.load(Ordering::Acquire));

        let mut byte = [0_u8; 1];
        let read = server
            .read(&mut byte)
            .expect("server observes peer shutdown");
        assert_eq!(read, 0);
    }

    #[test]
    fn recv_framed_until_shutdown_exits_when_session_shutdown_is_requested() {
        let listener =
            StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback control socket");
        let addr = listener.local_addr().expect("listener local addr");
        let client = StdTcpStream::connect(addr).expect("connect loopback control socket");
        let (mut server, _) = listener.accept().expect("accept loopback control socket");
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set client read timeout");
        server
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("set server read timeout");

        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let shutdown_signal = Arc::clone(&shutdown_requested);
        let wake_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(120));
            shutdown_signal.store(true, Ordering::Release);
        });

        let result =
            recv_framed_until_shutdown::<ServerControlPacket>(&mut server, &shutdown_requested);
        wake_thread.join().expect("join shutdown wake thread");

        let err = result.expect_err("shutdown should interrupt framed recv");
        assert!(
            err.to_string()
                .contains("ALVR control session shutdown requested"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn same_decoder_config_detects_exact_duplicates_only() {
        let previous = (CodecType::Hevc, vec![1, 2, 3, 4]);
        let same = DecoderInitializationConfig {
            codec: CodecType::Hevc,
            config_buffer: vec![1, 2, 3, 4],
            ext_str: String::new(),
        };
        let different_codec = DecoderInitializationConfig {
            codec: CodecType::H264,
            config_buffer: vec![1, 2, 3, 4],
            ext_str: String::new(),
        };
        let different_bytes = DecoderInitializationConfig {
            codec: CodecType::Hevc,
            config_buffer: vec![1, 2, 3, 5],
            ext_str: String::new(),
        };

        assert!(same_decoder_config(&previous, &same));
        assert!(!same_decoder_config(&previous, &different_codec));
        assert!(!same_decoder_config(&previous, &different_bytes));
    }

    #[test]
    fn runtime_video_stream_recovery_request_is_one_shot() {
        ALVR_RUNTIME_VIDEO_RECOVERY_REQUESTED.store(false, Ordering::Release);
        *ALVR_RUNTIME_VIDEO_RECOVERY_STATE
            .lock()
            .expect("lock runtime video recovery state") = RuntimeVideoRecoveryState::default();

        request_runtime_video_stream_recovery("test");

        assert!(take_runtime_video_stream_recovery_request());
        assert!(!take_runtime_video_stream_recovery_request());
    }

    #[test]
    fn runtime_video_recovery_state_suppresses_duplicates_while_in_flight() {
        let now = Instant::now();
        let mut state = RuntimeVideoRecoveryState::default();

        assert!(state.queue_if_due(now));
        assert!(state.take_queued(now));
        assert!(!state.queue_if_due(now + Duration::from_millis(500)));
        assert!(state.mark_completed(now + Duration::from_secs(1)));
        assert!(!state.queue_if_due(now + Duration::from_secs(2)));
        assert!(state.queue_if_due(
            now + Duration::from_secs(1) + ALVR_RUNTIME_VIDEO_RECOVERY_COOLDOWN
        ));
    }

    #[test]
    fn runtime_video_recovery_state_expires_stale_in_flight_request() {
        let now = Instant::now();
        let mut state = RuntimeVideoRecoveryState::with_times(
            false,
            true,
            Some(now),
            Some(now),
            None,
        );

        assert!(state.queue_if_due(
            now + ALVR_RUNTIME_VIDEO_RECOVERY_TIMEOUT + Duration::from_millis(1)
        ));
        assert!(state.take_queued(
            now + ALVR_RUNTIME_VIDEO_RECOVERY_TIMEOUT + Duration::from_millis(1)
        ));
    }
}
