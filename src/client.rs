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
        IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener, TcpStream as StdTcpStream,
        UdpSocket as StdUdpSocket,
    },
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use log::{info, warn};
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
    ) -> Result<()> {
        Ok(())
    }

    fn push_nal(&self, _timestamp_ns: u64, _is_idr: bool, _data: Vec<u8>) {}
}

const HANDSHAKE_ACTION_TIMEOUT: Duration = Duration::from_secs(5);
const ALVR_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);
const ALVR_IDR_REQUEST_INTERVAL: Duration = Duration::from_secs(2);
const ALVR_STREAM_RECV_TIMEOUT: Duration = Duration::from_millis(500);
const ALVR_STREAM_SHARD_PREFIX_SIZE: usize = 18;
const ALVR_TRACKING_STREAM_ID: u16 = 0;
const ALVR_VIDEO_STREAM_ID: u16 = 3;
const ALVR_STATISTICS_STREAM_ID: u16 = 4;
const ALVR_STREAM_LOG_EVERY: u64 = 3_600;
const ALVR_INITIAL_IDR_REQUESTS: u32 = 5;
const ALVR_TRACKING_SEND_INTERVAL: Duration = Duration::from_micros(13_889);
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
// Keep the const for clarity but reads always use tune::ipd_scale() at runtime.
const ALVR_IPD_SCALE: f32 = ALVR_IPD_SCALE_DEFAULT;
const ALVR_HEAD_PATH: &str = "/user/head";

static ALVR_CONTROL_LISTENER_STARTED: AtomicBool = AtomicBool::new(false);
static LATEST_HEAD_TRACKING_POSE: Mutex<Option<AlvrHeadTrackingPose>> = Mutex::new(None);
static ALVR_STATISTICS_STATE: Mutex<Option<AlvrClientStatisticsState>> = Mutex::new(None);
static ALVR_STATISTICS_SENDER: Mutex<Option<AlvrStreamHeaderSender>> = Mutex::new(None);
static ALVR_VIEW_CONFIG_STATE: Mutex<Option<VersionedViewsConfig>> = Mutex::new(None);
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

/// Returns the current IPD in metres, already scaled by ALVR_IPD_SCALE, ready to send to ALVR.
/// The state stores the scaled IPD; do NOT multiply by ALVR_IPD_SCALE again at the call site.
fn current_alvr_ipd_m() -> f32 {
    latest_alvr_views_config()
        .map(|state| state.config.ipd_m)
        .unwrap_or(ALVR_DEFAULT_IPD_M * crate::tune::ipd_scale())
}

fn normalize_pimax_ipd_m(raw_ipd: f32) -> Option<f32> {
    if !raw_ipd.is_finite() || raw_ipd <= 0.0 {
        return None;
    }

    let ipd_m = if raw_ipd > 1.0 { raw_ipd / 1000.0 } else { raw_ipd };
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
    if !fov_x_rad.is_finite() || !fov_y_rad.is_finite() || fov_x_rad <= 0.0 || fov_y_rad <= 0.0 {
        warn!(
            "ignoring invalid Pimax ALVR view config input: fov_x_rad={} fov_y_rad={} eye={}x{}",
            fov_x_rad, fov_y_rad, eye_width, eye_height
        );
        return;
    }

    let horizontal_tan = (fov_x_rad * 0.5).tan().clamp(0.01, 8.0);
    let vertical_tan = (fov_y_rad * 0.5).tan().clamp(0.01, 8.0);
    let fov = Fov {
        left: -horizontal_tan,
        right: horizontal_tan,
        up: vertical_tan,
        down: -vertical_tan,
    };
    let config = ViewsConfig {
        // current_alvr_ipd_m() already returns the scaled IPD; do NOT multiply by ALVR_IPD_SCALE again.
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
        "updated ALVR ViewsConfig from Pimax device info: version={} eye={}x{} ipd_m={:.3} fov_rad=({:.6},{:.6}) fov_tan=left:{:.3} right:{:.3} up:{:.3} down:{:.3}",
        version,
        eye_width,
        eye_height,
        config.ipd_m,
        fov_x_rad,
        fov_y_rad,
        fov.left,
        fov.right,
        fov.up,
        fov.down
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
    *state = Some(VersionedViewsConfig { version, config: config.clone() });
    info!(
        "tune: IPD scale changed → physical_m={:.4} scale={:.2} alvr_ipd_m={:.4} version={}",
        physical, crate::tune::ipd_scale(), config.ipd_m, version
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
    mdns_daemon: std::sync::Mutex<Option<mdns_sd::ServiceDaemon>>,
}

impl AlvrClient {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            mdns_daemon: std::sync::Mutex::new(None),
        }
    }

    /// Advertise this client via mDNS so an ALVR v20 server can discover and
    /// connect back on TCP port 9943.
    ///
    /// First call registers the mDNS service; subsequent calls are no-ops
    /// because the ServiceDaemon re-announces automatically. Retries on the
    /// next call if the first attempt fails (e.g. WiFi not yet up).
    pub fn announce(&self) -> Result<()> {
        let mut guard = self.mdns_daemon.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }

        let local_ip = IpAddr::V4(wifi_ipv4().context("get local IPv4 for mDNS")?);
        let protocol_str = alvr_protocol_string(&self.config.version_string);

        let daemon =
            mdns_sd::ServiceDaemon::new().context("create mDNS ServiceDaemon")?;

        let service_info = mdns_sd::ServiceInfo::new(
            "_alvr._tcp.local.",
            &format!("alvr-{}", self.config.client_name),
            &format!("{}.local.", self.config.client_name),
            local_ip,
            self.config.discovery_port,
            &[
                ("protocol", protocol_str.as_str()),
                ("device_id", self.config.client_name.as_str()),
            ][..],
        )
        .context("build mDNS ServiceInfo")?;

        daemon
            .register(service_info)
            .context("register mDNS service")?;

        *guard = Some(daemon);

        info!(
            "mDNS: registered _alvr._tcp.local. hostname={} addr={}:{} protocol={}",
            self.config.client_name, local_ip, self.config.discovery_port, protocol_str
        );
        Ok(())
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
                        }
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
        default_view_resolution: glam::UVec2::new(2880, 2880),
        supported_refresh_rates: vec![72.0, 90.0],
        microphone_sample_rate: 48_000,
        supports_foveated_encoding: true,
        encoder_high_profile: true,
        encoder_10_bits: false,
        encoder_av1: false,
        multimodal_protocol: false,
        prefer_10bit: false,
        prefer_full_range: true,
        preferred_encoding_gamma: 1.0,
        prefer_hdr: false,
    };
    let legacy_caps = encode_video_streaming_capabilities(&capabilities)
        .context("encode ALVR capabilities to legacy format")?;

    send_framed(
        &mut stream,
        &ClientConnectionResult::ConnectionAccepted {
            client_protocol_id: config.protocol_id().as_u64(),
            display_name: "Pimax Crystal OG ALVR Dev".to_string(),
            server_ip: peer.ip(),
            streaming_capabilities: Some(legacy_caps),
        },
    )
    .context("send ALVR ConnectionAccepted")?;
    info!(
        "sent ALVR ConnectionAccepted to {peer}: protocol={} ({})",
        config.protocol_id(),
        config.protocol_id().as_u64()
    );

    let stream_config: StreamConfigPacket =
        recv_framed(&mut stream).context("receive ALVR stream config packet")?;
    info!(
        "received ALVR stream config: session_json={} bytes negotiated_json={} bytes",
        stream_config.session.len(),
        stream_config.negotiated.len()
    );

    let server_control: ServerControlPacket =
        recv_framed(&mut stream).context("receive ALVR server control packet")?;
    match server_control {
        ServerControlPacket::StartStream => {
            info!("received ALVR StartStream; opening minimal stream socket");
            run_minimal_alvr_stream(&mut stream, peer, &stream_config)
                .context("run minimal ALVR stream socket")?;
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
    let settings = StreamSocketSettings::from_stream_config(stream_config)?;
    crate::video_receiver::configure_foveated_encoding(settings.foveated_encoding);
    if settings.protocol != StreamProtocol::Udp {
        bail!(
            "minimal stream socket only supports UDP for now; negotiated {:?}",
            settings.protocol
        );
    }

    let udp = StdUdpSocket::bind((Ipv4Addr::UNSPECIFIED, settings.port))
        .with_context(|| format!("bind ALVR UDP stream socket on 0.0.0.0:{}", settings.port))?;
    udp.set_read_timeout(Some(ALVR_STREAM_RECV_TIMEOUT))
        .context("set ALVR UDP stream read timeout")?;
    udp.connect(SocketAddr::new(peer.ip(), settings.port))
        .with_context(|| {
            format!(
                "connect ALVR UDP stream socket to {}:{}",
                peer.ip(),
                settings.port
            )
        })?;

    info!(
        "ALVR UDP stream socket ready: local=0.0.0.0:{} peer={}:{} packet_size={}",
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
    send_framed_locked(
        &control_writer,
        &ClientControlPacket::ViewsConfig(initial_views_config),
    )
    .context("send initial ALVR ViewsConfig")?;
    info!(
        "sent ALVR StreamReady and initial ViewsConfig; waiting for UDP stream shards and control keepalives"
    );

    let video_decoder = Arc::new(VideoDecoderBridge::new());
    thread::Builder::new()
        .name("alvr-control-maintenance".to_string())
        .spawn({
            let control_writer = Arc::clone(&control_writer);
            move || maintain_alvr_control_socket(control_writer)
        })
        .context("spawn ALVR control maintenance thread")?;

    let receive_packet_size = settings.packet_size + 4;
    reset_alvr_statistics_state();
    let _statistics_sender_guard = install_alvr_statistics_sender(
        udp.try_clone()
            .context("clone ALVR UDP stream socket for statistics sender")?,
        receive_packet_size,
    )
    .context("install ALVR statistics stream sender")?;
    let tracking_udp = udp
        .try_clone()
        .context("clone ALVR UDP stream socket for tracking sender")?;
    thread::Builder::new()
        .name("alvr-tracking-send".to_string())
        .spawn(move || send_minimal_tracking_stream(tracking_udp, receive_packet_size))
        .context("spawn ALVR tracking sender thread")?;

    thread::Builder::new()
        .name("alvr-udp-stream-recv".to_string())
        .spawn({
            let video_decoder = Arc::clone(&video_decoder);
            move || receive_alvr_udp_stream(udp, receive_packet_size, video_decoder)
        })
        .context("spawn ALVR UDP stream receiver thread")?;

    let mut decoder_configured = false;
    let mut ignored_decoder_configs = 0_u64;
    loop {
        match recv_framed::<ServerControlPacket>(stream) {
            Ok(ServerControlPacket::KeepAlive) => {}
            Ok(ServerControlPacket::DecoderConfig(config)) => {
                info!(
                    "received ALVR decoder config: codec={:?} config_bytes={}",
                    config.codec,
                    config.config_buffer.len()
                );
                if decoder_configured {
                    ignored_decoder_configs = ignored_decoder_configs.wrapping_add(1);
                    if ignored_decoder_configs <= 5
                        || ignored_decoder_configs % ALVR_STREAM_LOG_EVERY == 0
                    {
                        info!(
                            "ignored duplicate ALVR decoder config after initial decoder setup: duplicates={ignored_decoder_configs}"
                        );
                    }
                    continue;
                }

                video_decoder
                    .configure(
                        config.codec.mime_type(),
                        config.codec.label(),
                        config.config_buffer,
                    )
                    .with_context(|| format!("configure decoder for {:?}", config.codec))?;
                decoder_configured = true;
                send_framed_locked(&control_writer, &ClientControlPacket::RequestIdr)
                    .context("request IDR after decoder config")?;
                info!("requested ALVR IDR after decoder configuration");
            }
            Ok(ServerControlPacket::ReservedBuffer(buffer)) => {
                info!(
                    "received ALVR reserved realtime config/control buffer: {} bytes",
                    buffer.len()
                );
            }
            Ok(ServerControlPacket::Restarting) => {
                info!("ALVR server requested SteamVR restart during stream");
                return Ok(());
            }
            Ok(other) => {
                info!("received ALVR control packet during stream: {other:?}");
            }
            Err(err) => {
                warn!("ALVR control receive loop ended: {err:#}");
                return Ok(());
            }
        }
    }
}

fn receive_alvr_udp_stream(
    socket: StdUdpSocket,
    packet_size: usize,
    video_decoder: Arc<VideoDecoderBridge>,
) {
    let mut buffer = vec![0_u8; packet_size.max(ALVR_STREAM_SHARD_PREFIX_SIZE)];
    let mut video_assembler =
        VideoPacketAssembler::new(packet_size - ALVR_STREAM_SHARD_PREFIX_SIZE);
    let mut shards = 0_u64;
    let mut video_shards = 0_u64;
    let start = Instant::now();

    loop {
        match socket.recv(&mut buffer) {
            Ok(len) if len >= ALVR_STREAM_SHARD_PREFIX_SIZE => {
                shards += 1;

                let announced_len =
                    u32::from_be_bytes(buffer[0..4].try_into().unwrap()) as usize + 4;
                let stream_id = u16::from_be_bytes(buffer[4..6].try_into().unwrap());
                let packet_index = u32::from_be_bytes(buffer[6..10].try_into().unwrap());
                let shard_count = u32::from_be_bytes(buffer[10..14].try_into().unwrap());
                let shard_index = u32::from_be_bytes(buffer[14..18].try_into().unwrap());
                let video_details = if stream_id == ALVR_VIDEO_STREAM_ID {
                    decode_video_packet_details(&buffer[ALVR_STREAM_SHARD_PREFIX_SIZE..len])
                } else {
                    None
                };

                if stream_id == ALVR_VIDEO_STREAM_ID {
                    video_shards += 1;
                    if let Some(packet) = video_assembler.push(
                        packet_index,
                        shard_count,
                        shard_index,
                        &buffer[ALVR_STREAM_SHARD_PREFIX_SIZE..len],
                    ) {
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
                        report_alvr_video_packet_received(packet.header.timestamp);
                        video_decoder.push_nal(
                            packet.header.timestamp.as_nanos().min(u128::from(u64::MAX)) as u64,
                            packet.header.is_idr,
                            packet.payload,
                        );
                    }
                }

                if shards <= 10
                    || (stream_id == ALVR_VIDEO_STREAM_ID
                        && video_shards % ALVR_STREAM_LOG_EVERY == 0)
                    || shards % (ALVR_STREAM_LOG_EVERY * 4) == 0
                {
                    info!(
                        "received ALVR stream shard: stream_id={} packet_index={} shard={}/{} udp_len={} announced_len={} video_details={} total_shards={} video_shards={} elapsed_ms={}",
                        stream_id,
                        packet_index,
                        shard_index + 1,
                        shard_count,
                        len,
                        announced_len,
                        video_details.as_deref().unwrap_or("n/a"),
                        shards,
                        video_shards,
                        start.elapsed().as_millis()
                    );
                }
            }
            Ok(len) => {
                warn!("received short ALVR UDP stream datagram: {len} bytes");
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(err) => {
                warn!("ALVR UDP stream receiver exiting: {err:#}");
                break;
            }
        }
    }
}

fn send_minimal_tracking_stream(socket: StdUdpSocket, max_packet_size: usize) {
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
        let tracking = Tracking {
            target_timestamp: timestamp,
            device_motions: vec![(
                head_id,
                DeviceMotion {
                    pose: Pose {
                        orientation: head_pose.orientation,
                        position: head_pose.position,
                    },
                    linear_velocity: glam::Vec3::ZERO,
                    angular_velocity: glam::Vec3::ZERO,
                },
            )],
            hand_skeletons: [None, None],
            face_data: FaceData::default(),
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
    let payload = bincode::serialize(header).context("serialize ALVR stream header")?;
    let datagram_len = ALVR_STREAM_SHARD_PREFIX_SIZE
        .checked_add(payload.len())
        .context("ALVR stream packet length overflow")?;
    if datagram_len > max_packet_size {
        bail!(
            "ALVR stream header packet too large: {datagram_len} bytes exceeds max {max_packet_size}"
        );
    }

    let mut datagram = vec![0_u8; datagram_len];
    datagram[0..4].copy_from_slice(
        &u32::try_from(datagram_len - std::mem::size_of::<u32>())
            .context("ALVR stream datagram length exceeds u32")?
            .to_be_bytes(),
    );
    datagram[4..6].copy_from_slice(&stream_id.to_be_bytes());
    datagram[6..10].copy_from_slice(&packet_index.to_be_bytes());
    datagram[10..14].copy_from_slice(&1_u32.to_be_bytes());
    datagram[14..18].copy_from_slice(&0_u32.to_be_bytes());
    datagram[ALVR_STREAM_SHARD_PREFIX_SIZE..].copy_from_slice(&payload);

    let bytes_sent = socket
        .send(&datagram)
        .context("send ALVR stream header datagram")?;
    if bytes_sent != datagram_len {
        bail!("short ALVR UDP send: sent {bytes_sent} of {datagram_len} bytes");
    }

    Ok(bytes_sent)
}

fn maintain_alvr_control_socket(writer: SharedControlWriter) {
    let mut next_keepalive = Instant::now();
    let mut next_idr_request = Instant::now();
    let mut idr_requests_sent = 0_u32;
    let mut last_views_config_version = latest_alvr_views_config().map(|state| state.version);
    let mut keepalives_sent = 0_u64;

    loop {
        let now = Instant::now();

        if now >= next_keepalive {
            if let Err(err) = send_framed_locked(&writer, &ClientControlPacket::KeepAlive) {
                warn!("ALVR control maintenance thread exiting after keepalive failure: {err:#}");
                break;
            }
            keepalives_sent = keepalives_sent.wrapping_add(1);
            if keepalives_sent <= 5 || keepalives_sent % 20 == 0 {
                info!("sent ALVR KeepAlive on control socket: count={keepalives_sent}");
            }
            next_keepalive = now + ALVR_KEEPALIVE_INTERVAL;
        }

        if let Some(views_config) = latest_alvr_views_config() {
            if Some(views_config.version) != last_views_config_version {
                if let Err(err) = send_framed_locked(
                    &writer,
                    &ClientControlPacket::ViewsConfig(views_config.config.clone()),
                ) {
                    warn!(
                        "ALVR control maintenance thread exiting after ViewsConfig update failure: {err:#}"
                    );
                    break;
                }
                info!(
                    "sent updated ALVR ViewsConfig from Pimax device info: version={}",
                    views_config.version
                );
                last_views_config_version = Some(views_config.version);
            }
        }

        if idr_requests_sent < ALVR_INITIAL_IDR_REQUESTS && now >= next_idr_request {
            if let Err(err) = send_framed_locked(&writer, &ClientControlPacket::RequestIdr) {
                warn!("ALVR control maintenance thread exiting after IDR request failure: {err:#}");
                break;
            }

            idr_requests_sent += 1;
            info!(
                "sent ALVR RequestIdr during stream startup ({}/{})",
                idr_requests_sent, ALVR_INITIAL_IDR_REQUESTS
            );
            next_idr_request = now + ALVR_IDR_REQUEST_INTERVAL;
        }

        thread::sleep(Duration::from_millis(25));
    }
}

fn decode_video_packet_details(data: &[u8]) -> Option<String> {
    let mut cursor = data;
    let header = bincode::deserialize_from::<_, VideoPacketHeader>(&mut cursor).ok()?;
    Some(format!(
        "timestamp_ns={} is_idr={} payload_bytes={}",
        header.timestamp.as_nanos(),
        header.is_idr,
        cursor.len()
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
        let mut data = partial.data.as_slice();
        let header = match bincode::deserialize_from::<_, VideoPacketHeader>(&mut data) {
            Ok(header) => header,
            Err(err) => {
                warn!(
                    "failed to decode completed ALVR video packet header for packet_index={packet_index}: {err:#}"
                );
                return None;
            }
        };

        let payload = data.to_vec();
        self.completed_count += 1;
        Some(CompletedVideoPacket {
            header,
            payload_len: payload.len(),
            payload,
            completed_count: self.completed_count,
        })
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

impl StreamSocketSettings {
    fn from_stream_config(packet: &StreamConfigPacket) -> Result<Self> {
        let session: serde_json::Value =
            serde_json::from_str(&packet.session).context("parse ALVR session JSON")?;
        let negotiated: serde_json::Value =
            serde_json::from_str(&packet.negotiated).context("parse ALVR negotiated JSON")?;

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

        let wired = negotiated
            .get("wired")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let protocol = if wired {
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
        let foveated_encoding = parse_foveated_encoding(&session);

        Ok(Self {
            protocol,
            port,
            packet_size,
            foveated_encoding,
        })
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

    let Some(expanded_view_width) = get_u32("target_eye_resolution_width", "eye_resolution_width")
    else {
        warn!("ALVR foveated encoding is enabled but stream config has no target eye width");
        return None;
    };
    let Some(expanded_view_height) =
        get_u32("target_eye_resolution_height", "eye_resolution_height")
    else {
        warn!("ALVR foveated encoding is enabled but stream config has no target eye height");
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
    let payload = bincode::serialize(packet).context("serialize ALVR framed packet")?;
    let len = u32::try_from(payload.len()).context("ALVR framed packet too large")?;
    stream
        .write_all(&len.to_be_bytes())
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

fn recv_framed<R: DeserializeOwned>(stream: &mut StdTcpStream) -> Result<R> {
    let mut len_bytes = [0_u8; 4];
    stream
        .read_exact(&mut len_bytes)
        .context("read ALVR frame length")?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    if len > 64 * 1024 * 1024 {
        bail!("ALVR frame too large: {len} bytes");
    }

    let mut payload = vec![0_u8; len];
    stream
        .read_exact(&mut payload)
        .context("read ALVR frame payload")?;
    bincode::deserialize(&payload).context("deserialize ALVR framed packet")
}

#[derive(Serialize, Deserialize)]
enum ClientConnectionResult {
    ConnectionAccepted {
        client_protocol_id: u64,
        display_name: String,
        server_ip: IpAddr,
        streaming_capabilities: Option<VideoStreamingCapabilitiesLegacy>,
    },
    ClientStandby,
}

#[derive(Serialize, Deserialize, Clone)]
struct VideoStreamingCapabilitiesLegacy {
    default_view_resolution: glam::UVec2,
    supported_refresh_rates_plus_extra_data: Vec<f32>,
    microphone_sample_rate: u32,
}

#[derive(Serialize, Deserialize, Clone)]
struct VideoStreamingCapabilities {
    default_view_resolution: glam::UVec2,
    supported_refresh_rates: Vec<f32>,
    microphone_sample_rate: u32,
    supports_foveated_encoding: bool,
    encoder_high_profile: bool,
    encoder_10_bits: bool,
    encoder_av1: bool,
    multimodal_protocol: bool,
    prefer_10bit: bool,
    prefer_full_range: bool,
    preferred_encoding_gamma: f32,
    prefer_hdr: bool,
}

fn encode_video_streaming_capabilities(
    caps: &VideoStreamingCapabilities,
) -> Result<VideoStreamingCapabilitiesLegacy> {
    let mut packed = caps.supported_refresh_rates.clone();
    let json = serde_json::to_string(caps).context("encode capabilities JSON")?;
    for byte in json.as_bytes() {
        packed.push(-(*byte as f32));
    }
    Ok(VideoStreamingCapabilitiesLegacy {
        default_view_resolution: caps.default_view_resolution,
        supported_refresh_rates_plus_extra_data: packed,
        microphone_sample_rate: caps.microphone_sample_rate,
    })
}

#[derive(Serialize, Deserialize)]
struct StreamConfigPacket {
    session: String,
    negotiated: String,
}

#[derive(Serialize, Deserialize, Debug)]
enum ServerControlPacket {
    StartStream,
    DecoderConfig(DecoderInitializationConfig),
    Restarting,
    KeepAlive,
    ServerPredictionAverage(Duration),
    Reserved(String),
    ReservedBuffer(Vec<u8>),
}

#[derive(Serialize, Deserialize, Debug)]
struct DecoderInitializationConfig {
    codec: CodecType,
    config_buffer: Vec<u8>,
}

#[derive(Deserialize)]
struct VideoPacketHeader {
    timestamp: Duration,
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

#[derive(Serialize, Deserialize, Clone, Default)]
struct FaceData {
    eye_gazes: [Option<Pose>; 2],
    fb_face_expression: Option<Vec<f32>>,
    htc_eye_expression: Option<Vec<f32>>,
    htc_lip_expression: Option<Vec<f32>>,
}

#[derive(Serialize, Deserialize, Default)]
struct Tracking {
    target_timestamp: Duration,
    device_motions: Vec<(u64, DeviceMotion)>,
    hand_skeletons: [Option<[Pose; 26]>; 2],
    face_data: FaceData,
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

#[derive(Serialize, Deserialize, Clone)]
struct BatteryInfo {
    device_id: u64,
    gauge_value: f32,
    is_plugged: bool,
}

#[derive(Serialize, Deserialize)]
enum ClientControlPacket {
    PlayspaceSync(Option<glam::Vec2>),
    RequestIdr,
    KeepAlive,
    StreamReady,
    ViewsConfig(ViewsConfig),
    Battery(BatteryInfo),
    VideoErrorReport,
    Buttons(Vec<crate::controller::ButtonEntry>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DISCOVERY_PORT;

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

    fn sample_capabilities() -> VideoStreamingCapabilities {
        VideoStreamingCapabilities {
            default_view_resolution: glam::UVec2::new(2880, 2880),
            supported_refresh_rates: vec![72.0, 90.0],
            microphone_sample_rate: 48_000,
            supports_foveated_encoding: true,
            encoder_high_profile: true,
            encoder_10_bits: false,
            encoder_av1: false,
            multimodal_protocol: false,
            prefer_10bit: false,
            prefer_full_range: true,
            preferred_encoding_gamma: 1.0,
            prefer_hdr: false,
        }
    }

    // Regression: ALVR v20.14.1 server expects the legacy capabilities wire
    // format — refresh rates followed by JSON bytes packed as negative floats.
    // Without this trick the server hangs up after ConnectionAccepted with
    // "read ALVR frame length: failed to fill whole buffer".
    #[test]
    fn encode_capabilities_packs_json_as_negative_floats_after_refresh_rates() {
        let caps = sample_capabilities();
        let legacy = encode_video_streaming_capabilities(&caps).unwrap();

        let refresh_rate_count = caps.supported_refresh_rates.len();
        assert_eq!(
            &legacy.supported_refresh_rates_plus_extra_data[..refresh_rate_count],
            &caps.supported_refresh_rates[..],
            "refresh rates must be at the head of the packed vector",
        );

        let json_bytes: Vec<u8> = legacy.supported_refresh_rates_plus_extra_data
            [refresh_rate_count..]
            .iter()
            .map(|f| {
                assert!(*f <= 0.0, "JSON byte floats must be non-positive");
                (-*f) as u8
            })
            .collect();
        let json_str = std::str::from_utf8(&json_bytes).expect("packed JSON is valid UTF-8");
        let decoded: serde_json::Value = serde_json::from_str(json_str).expect("packed JSON parses");

        // Must use v20.14.1 field names (not the older "foveated_encoding").
        assert!(decoded.get("supports_foveated_encoding").is_some());
        assert!(decoded.get("multimodal_protocol").is_some());
        assert!(decoded.get("prefer_10bit").is_some());
        assert!(decoded.get("prefer_hdr").is_some());
        assert_eq!(decoded["supports_foveated_encoding"], true);
        assert_eq!(decoded["microphone_sample_rate"], 48_000);
    }

    #[test]
    fn encode_capabilities_preserves_resolution_and_sample_rate() {
        let caps = sample_capabilities();
        let legacy = encode_video_streaming_capabilities(&caps).unwrap();
        assert_eq!(legacy.default_view_resolution, caps.default_view_resolution);
        assert_eq!(legacy.microphone_sample_rate, caps.microphone_sample_rate);
    }

    // Regression: bincode encodes enum variant index as u32 LE. ALVR v20
    // server matches by ordinal — reorder the variants and the server
    // mis-decodes every control packet.
    #[test]
    fn client_control_packet_variant_indices_match_alvr_v20() {
        fn variant_index(packet: ClientControlPacket) -> u32 {
            let bytes = bincode::serialize(&packet).expect("serialize control packet");
            assert!(bytes.len() >= 4);
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        }

        assert_eq!(variant_index(ClientControlPacket::PlayspaceSync(None)), 0);
        assert_eq!(variant_index(ClientControlPacket::RequestIdr), 1);
        assert_eq!(variant_index(ClientControlPacket::KeepAlive), 2);
        assert_eq!(variant_index(ClientControlPacket::StreamReady), 3);
        assert_eq!(
            variant_index(ClientControlPacket::ViewsConfig(default_views_config())),
            4,
        );
    }
}
