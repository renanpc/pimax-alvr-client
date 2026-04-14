//! ALVR Video Stream Integration for Pimax Crystal
//!
//! # Overview
//!
//! This module is the heart of the Pimax ALVR client. It receives encoded video streams
//! from the ALVR server (running on a PC), decodes them using Android's MediaCodec API,
//! and renders them to the Pimax headset's display using OpenGL ES.
//!
//! # Architecture
//!
//! ```text
//! ALVR Server (PC)
//!     │
//!     │ H.264/H.265 over UDP
//!     ▼
//! TCP Listener (port 9943) ─── Control/handshake
//! UDP Socket (port 9944) ───── Video stream shards
//!     │
//!     ▼
//! Android MediaCodec Decoder
//!     │
//!     │ AHardwareBuffer
//!     ▼
//! OpenGL ES Texture (GL_TEXTURE_EXTERNAL_OES)
//!     │
//!     ▼
//! Pass 1: OES → RGBA intermediate texture
//!     │
//!     ▼
//! Pass 2: RGBA → Eye framebuffer with convergence shift
//!     │
//!     ▼
//! Pimax Compositor (sxrEndXr)
//!     │
//!     │ Applies divergent warp (~0.248 NDC per eye)
//!     ▼
//! Display (lenses)
//! ```
//!
//! # Key Concepts
//!
//! ## Convergence Shift (Double Vision Fix)
//!
//! The Pimax Crystal's compositor applies a **divergent warp** to each eye's output
//! before displaying. This means:
//! - Left eye image is shifted left
//! - Right eye image is shifted right
//!
//! When ALVR sends properly converged stereo images, the Pimax warp causes double vision.
//!
//! **Solution**: Pre-shift each eye's blit output in the *opposite* direction:
//! - Left eye: shift right by +0.248 NDC
//! - Right eye: shift left by -0.248 NDC
//!
//! The Pimax compositor's warp then cancels this pre-shift, resulting in correct stereo.
//!
//! The shift value is tunable via the web UI at `http://<headset-ip>:7878/`.
//!
//! ## Color Correction (BT.709)
//!
//! ALVR sends video in BT.709 color space with limited range (16-235 for Y).
//! The Pimax display expects full range RGB. Conversion requires:
//!
//! 1. **Black Crush**: Lift the black level from 16/255 (0.0627) to compensate
//!    for display characteristics. Default: 0.072
//!
//! 2. **Color Gain**: Amplify contrast by multiplying the signal. Default: 1.22
//!
//! Formula: `output = (input - black_crush) * gain`
//!
//! ## Foveated Rendering Support
//!
//! ALVR can use foveated encoding to save bandwidth:
//! - High resolution in the center of each eye's view
//! - Lower resolution in the periphery
//!
//! This module applies a custom shader to un-distort the foveated image,
//! mapping it back to a flat rectangular texture for the Pimax compositor.
//!
//! ## Two-Pass Blit
//!
//! The rendering uses two passes:
//!
//! **Pass 1**: Convert `GL_TEXTURE_EXTERNAL_OES` (from MediaCodec) to RGBA texture
//! - Applies color correction (black crush + gain)
//! - Output: Intermediate RGBA texture
//!
//! **Pass 2**: Blit RGBA texture to eye framebuffer
//! - Applies convergence shift (per-eye)
//! - Applies foveation un-distortion (if enabled)
//! - Output: Final eye texture for Pimax compositor
//!
//! # Threading Model
//!
//! - **TCP Listener Thread**: Receives ALVR control messages
//! - **UDP Receiver Thread**: Collects video stream shards
//! - **Video Decoder Thread**: Feeds NAL units to MediaCodec
//! - **Render Thread**: Reads decoded frames and blits to eye textures
//!
//! The render thread is time-critical (90fps). All operations in the blit path
//! use lock-free atomics for tuning parameters.
//!
//! # Debug Features
//!
//! ## Debug RGBA TCP Stream (port 9950)
//!
//! A simple raw RGBA frame ingress for testing without ALVR. Send frames as:
//! ```text
//! [8-byte magic: "PIMXRGBA"]
//! [4-byte width]
//! [4-byte height]
//! [8-byte timestamp_ns]
//! [4-byte payload_size]
//! [payload: width * height * 4 bytes RGBA]
//! ```
//!
//! # Configuration
//!
//! Tuning parameters are exposed via HTTP at `http://<headset-ip>:7878/`:
//! - `convergence_shift_ndc`: Stereo convergence correction (default: 0.124)
//! - `color_black_crush`: Black level adjustment (default: 0.072)
//! - `color_gain`: Contrast gain (default: 1.22)
//! - `ipd_scale`: Stereo separation strength (default: 1.0)


use std::ffi::{c_void, CString};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use log::{info, warn};
use ndk_sys as ffi;
use parking_lot::Mutex;

use crate::pimax::EyeRenderTarget;

/// Temporary PC-to-headset raw RGBA frame ingress.
///
/// This is intentionally separate from the real ALVR stream port so we can
/// validate live texture upload without pretending this is ALVR protocol.
pub const DEBUG_RGBA_STREAM_PORT: u16 = 9950;

/// EGL constants
const EGL_NATIVE_BUFFER_ANDROID: i32 = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;
const EGL_NONE: i32 = 0x3038;

/// GL constants
const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE0: u32 = 0x84C0;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_VERTEX_SHADER: u32 = 0x8B31;
const GL_FRAGMENT_SHADER: u32 = 0x8B30;
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
const GL_INFO_LOG_LENGTH: u32 = 0x8B84;
const GL_TRIANGLES: u32 = 0x0004;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;

/// EGL function types
type EglGetNativeClientBufferAndroid = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type EglCreateImageKhr = unsafe extern "C" fn(
    dpy: *mut c_void,
    ctx: *mut c_void,
    target: i32,
    buffer: *mut c_void,
    attrib_list: *const i32,
) -> *mut c_void;
type EglDestroyImageKhr = unsafe extern "C" fn(dpy: *mut c_void, image: *mut c_void) -> i32;
type GlEglImageTargetTexture2dOes = unsafe extern "C" fn(target: u32, image: *mut c_void);

/// Load an EGL/GLES extension proc
fn load_egl_proc<T: Copy>(name: &str) -> Result<T> {
    let name = CString::new(name).context("create proc name")?;
    let proc = unsafe { eglGetProcAddress(name.as_ptr().cast()) };
    if proc.is_null() {
        bail!("missing EGL/GLES extension proc {name:?}");
    }
    Ok(unsafe { std::mem::transmute_copy(&proc) })
}

// EGL extern declarations (from pimax.rs)
extern "C" {
    fn eglGetProcAddress(procname: *const i8) -> *const c_void;
    fn eglGetCurrentDisplay() -> *mut c_void;
}

// GL extern declarations
extern "C" {
    fn glActiveTexture(texture: u32);
    fn glAttachShader(program: u32, shader: u32);
    fn glBindFramebuffer(target: u32, framebuffer: u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glCompileShader(shader: u32);
    fn glCreateProgram() -> u32;
    fn glCreateShader(type_: u32) -> u32;
    fn glDeleteProgram(program: u32);
    fn glDeleteShader(shader: u32);
    fn glDeleteTextures(n: i32, textures: *const u32);
    fn glDrawArrays(mode: u32, first: i32, count: i32);
    fn glGenTextures(n: i32, textures: *mut u32);
    fn glGetIntegerv(pname: u32, data: *mut i32);
    fn glGetProgramInfoLog(program: u32, buf_size: i32, length: *mut i32, info_log: *mut i8);
    fn glGetProgramiv(program: u32, pname: u32, params: *mut i32);
    fn glGetShaderInfoLog(shader: u32, buf_size: i32, length: *mut i32, info_log: *mut i8);
    fn glGetShaderiv(shader: u32, pname: u32, params: *mut i32);
    fn glGetUniformLocation(program: u32, name: *const i8) -> i32;
    fn glLinkProgram(program: u32);
    fn glShaderSource(shader: u32, count: i32, string: *const *const i8, length: *const i32);
    fn glTexParameteri(target: u32, pname: u32, param: i32);
    fn glTexSubImage2D(
        target: u32,
        level: i32,
        xoffset: i32,
        yoffset: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        pixels: *const c_void,
    );
    fn glCheckFramebufferStatus(target: u32) -> u32;
    fn glDeleteFramebuffers(n: i32, framebuffers: *const u32);
    fn glFlush();
    fn glFramebufferTexture2D(target: u32, attachment: u32, textarget: u32, texture: u32, level: i32);
    fn glGenFramebuffers(n: i32, framebuffers: *mut u32);
    fn glTexImage2D(
        target: u32,
        level: i32,
        internalformat: i32,
        width: i32,
        height: i32,
        border: i32,
        format: u32,
        type_: u32,
        pixels: *const c_void,
    );
    fn glClear(mask: u32);
    fn glClearColor(red: f32, green: f32, blue: f32, alpha: f32);
    fn glUniform1f(location: i32, v0: f32);
    fn glUniform1i(location: i32, v0: i32);
    fn glUniform4f(location: i32, v0: f32, v1: f32, v2: f32, v3: f32);
    fn glUseProgram(program: u32);
    fn glViewport(x: i32, y: i32, width: i32, height: i32);
}

const DEBUG_RGBA_MAGIC: &[u8; 8] = b"PIMXRGBA";
const DEBUG_RGBA_HEADER_LEN: usize = 8 + 4 + 4 + 8 + 4;
const DEBUG_RGBA_MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
static DEBUG_RGBA_TCP_STARTED: AtomicBool = AtomicBool::new(false);
static AHB_BLIT_COUNT: AtomicU64 = AtomicU64::new(0);
static BLIT_PROGRAM: Mutex<Option<BlitProgram>> = Mutex::new(None);
static PASSTHROUGH_OES_PROGRAM: Mutex<Option<PassthroughProgram>> = Mutex::new(None);
static INTERMEDIATE_FBO: Mutex<Option<IntermediateFbo>> = Mutex::new(None);
static FOVEATED_ENCODING: Mutex<Option<ActiveFoveationConfig>> = Mutex::new(None);
// Keep the stream mapping aligned with the source frame unless we have a
// headset-side repro that proves we need a correction.
const PIMAX_SWAP_STREAM_EYES: bool = false;
const PIMAX_FLIP_STREAM_VERTICAL: bool = false;
// Default starting values — exposed as `pub` so `android.rs` can pass them to `tune::init`.
// The render thread reads live values from `tune::convergence_shift_ndc()` etc. each frame.
pub const PIMAX_BLIT_CONVERGENCE_SHIFT_NDC_DEFAULT: f32 = 0.124;
/// Default BT.709 limited-range black level for the passthrough OES shader.
pub const COLOR_BLACK_CRUSH_DEFAULT: f32 = 0.072;
/// Default BT.709 contrast gain for the passthrough OES shader.
pub const COLOR_GAIN_DEFAULT: f32 = 1.22;

/// Simple passthrough shader for GL_TEXTURE_EXTERNAL_OES → RGBA conversion (pass 1).
struct PassthroughProgram {
    program: u32,
    texture_uniform: i32,
    black_crush_uniform: i32,
    color_gain_uniform: i32,
}

/// Cached intermediate RGBA framebuffer used between the two blit passes.
struct IntermediateFbo {
    framebuffer: u32,
    texture: u32,
    width: i32,
    height: i32,
}

struct BlitProgram {
    program: u32,
    texture_target: u32,
    texture_target_label: &'static str,
    texture_uniform: i32,
    uv_rect_uniform: i32,
    foveation_enabled_uniform: i32,
    view_index_uniform: i32,
    position_offset_x_uniform: i32,
    foveation_view_ratio_edge_uniform: i32,
    foveation_c1_c2_uniform: i32,
    foveation_bounds_uniform: i32,
    foveation_left_uniform: i32,
    foveation_right_uniform: i32,
    foveation_c_right_uniform: i32,
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

#[derive(Clone, Copy, Debug)]
struct ActiveFoveationConfig {
    optimized_frame_width: u32,
    optimized_frame_height: u32,
    constants: FoveationShaderConstants,
}

#[derive(Clone, Copy, Debug)]
struct FoveationShaderConstants {
    view_width_ratio: f32,
    view_height_ratio: f32,
    edge_ratio_x: f32,
    edge_ratio_y: f32,
    c1_x: f32,
    c1_y: f32,
    c2_x: f32,
    c2_y: f32,
    lo_bound_x: f32,
    lo_bound_y: f32,
    hi_bound_x: f32,
    hi_bound_y: f32,
    a_left_x: f32,
    a_left_y: f32,
    b_left_x: f32,
    b_left_y: f32,
    a_right_x: f32,
    a_right_y: f32,
    b_right_x: f32,
    b_right_y: f32,
    c_right_x: f32,
    c_right_y: f32,
}

#[derive(Clone, Copy, Debug)]
struct AxisFoveationConstants {
    optimized_aligned: u32,
    view_ratio_aligned: f32,
    edge_ratio: f32,
    c1: f32,
    c2: f32,
    lo_bound: f32,
    hi_bound: f32,
    a_left: f32,
    b_left: f32,
    a_right: f32,
    b_right: f32,
    c_right: f32,
}

/// Owned reference to an Android hardware buffer used by a decoded video frame.
#[derive(Debug)]
pub struct HardwareBufferLease {
    ptr: usize,
}

impl HardwareBufferLease {
    pub fn acquire(buffer_ptr: usize) -> Option<Arc<Self>> {
        if buffer_ptr == 0 {
            return None;
        }
        unsafe {
            ffi::AHardwareBuffer_acquire(buffer_ptr as *mut ffi::AHardwareBuffer);
        }
        Some(Arc::new(Self { ptr: buffer_ptr }))
    }
}

impl Drop for HardwareBufferLease {
    fn drop(&mut self) {
        unsafe {
            ffi::AHardwareBuffer_release(self.ptr as *mut ffi::AHardwareBuffer);
        }
    }
}

/// Video frame from the ALVR decoder
///
/// Note: VideoFrame is Send because the buffer pointer is only used from the
/// render thread and we ensure proper synchronization
#[derive(Clone)]
pub struct VideoFrame {
    /// Timestamp in nanoseconds
    pub timestamp_ns: u64,
    /// Pointer to the decoded AHardwareBuffer (as usize for Send safety)
    pub buffer_ptr: usize,
    /// Width of the frame
    pub width: u32,
    /// Height of the frame
    pub height: u32,
    /// Row pitch in bytes
    pub row_pitch: u32,
    /// Keeps an AHardwareBuffer pointer valid while cloned frames are rendered.
    pub hardware_buffer_lease: Option<Arc<HardwareBufferLease>>,
    /// Optional CPU RGBA frame payload. This is a temporary debug ingress used
    /// before the Android hardware-decoder path is wired to ALVR packets.
    pub rgba: Option<Arc<Vec<u8>>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum VideoFrameEye {
    Left,
    Right,
}

pub fn configure_foveated_encoding(config: Option<FoveatedEncodingConfig>) {
    let active = config.and_then(|config| match compute_foveation_config(config) {
        Ok(active) => Some(active),
        Err(err) => {
            warn!("failed to configure ALVR foveated blit mapping: {err:#}");
            None
        }
    });

    if let Some(active) = active {
        info!(
            "configured ALVR foveated blit mapping: optimized_frame={}x{} constants={:?}",
            active.optimized_frame_width, active.optimized_frame_height, active.constants
        );
        *FOVEATED_ENCODING.lock() = Some(active);
    } else {
        info!("ALVR foveated blit mapping disabled");
        *FOVEATED_ENCODING.lock() = None;
    }
}

fn compute_foveation_config(config: FoveatedEncodingConfig) -> Result<ActiveFoveationConfig> {
    let x = compute_foveation_axis(
        config.expanded_view_width as f32,
        config.center_size_x,
        config.center_shift_x,
        config.edge_ratio_x,
    )
    .context("compute horizontal foveation constants")?;
    let y = compute_foveation_axis(
        config.expanded_view_height as f32,
        config.center_size_y,
        config.center_shift_y,
        config.edge_ratio_y,
    )
    .context("compute vertical foveation constants")?;

    let optimized_frame_width = x
        .optimized_aligned
        .checked_mul(2)
        .context("foveated optimized frame width overflow")?;
    let optimized_frame_height = y.optimized_aligned;
    if optimized_frame_width == 0 || optimized_frame_height == 0 {
        bail!(
            "invalid foveated optimized frame size {}x{}",
            optimized_frame_width,
            optimized_frame_height
        );
    }

    Ok(ActiveFoveationConfig {
        optimized_frame_width,
        optimized_frame_height,
        constants: FoveationShaderConstants {
            view_width_ratio: x.view_ratio_aligned,
            view_height_ratio: y.view_ratio_aligned,
            edge_ratio_x: x.edge_ratio,
            edge_ratio_y: y.edge_ratio,
            c1_x: x.c1,
            c1_y: y.c1,
            c2_x: x.c2,
            c2_y: y.c2,
            lo_bound_x: x.lo_bound,
            lo_bound_y: y.lo_bound,
            hi_bound_x: x.hi_bound,
            hi_bound_y: y.hi_bound,
            a_left_x: x.a_left,
            a_left_y: y.a_left,
            b_left_x: x.b_left,
            b_left_y: y.b_left,
            a_right_x: x.a_right,
            a_right_y: y.a_right,
            b_right_x: x.b_right,
            b_right_y: y.b_right,
            c_right_x: x.c_right,
            c_right_y: y.c_right,
        },
    })
}

fn compute_foveation_axis(
    view_resolution: f32,
    center_size: f32,
    center_shift: f32,
    edge_ratio: f32,
) -> Result<AxisFoveationConstants> {
    if view_resolution <= 0.0 || center_size <= 0.0 || edge_ratio <= 1.0 {
        bail!(
            "invalid axis foveation inputs: view_resolution={view_resolution} center_size={center_size} edge_ratio={edge_ratio}"
        );
    }

    let edge_size = view_resolution - center_size * view_resolution;
    let center_size_aligned =
        1.0 - (edge_size / (edge_ratio * 2.0)).ceil() * (edge_ratio * 2.0) / view_resolution;
    let edge_size_aligned = view_resolution - center_size_aligned * view_resolution;
    if edge_size_aligned <= 0.0 {
        bail!(
            "invalid aligned foveation edge size: view_resolution={view_resolution} center_size_aligned={center_size_aligned}"
        );
    }

    let center_shift_aligned = (center_shift * edge_size_aligned / (edge_ratio * 2.0)).ceil()
        * (edge_ratio * 2.0)
        / edge_size_aligned;
    let foveation_scale = center_size_aligned + (1.0 - center_size_aligned) / edge_ratio;
    let optimized = foveation_scale * view_resolution;
    let optimized_aligned_float = (optimized / 32.0).ceil() * 32.0;
    if optimized <= 0.0 || optimized_aligned_float <= 0.0 {
        bail!(
            "invalid optimized foveation size: optimized={optimized} optimized_aligned={optimized_aligned_float}"
        );
    }

    let view_ratio_aligned = optimized / optimized_aligned_float;
    let c0 = (1.0 - center_size_aligned) * 0.5;
    let c1 = (edge_ratio - 1.0) * c0 * (center_shift_aligned + 1.0) / edge_ratio;
    let c2 = (edge_ratio - 1.0) * center_size_aligned + 1.0;
    let lo_bound = c0 * (center_shift_aligned + 1.0);
    let hi_bound = c0 * (center_shift_aligned - 1.0) + 1.0;
    let lo_bound_c = c0 * (center_shift_aligned + 1.0) / c2;
    let hi_bound_c = c0 * (center_shift_aligned - 1.0) / c2 + 1.0;

    let a_left = c2 * (1.0 - edge_ratio) / (edge_ratio * lo_bound_c);
    let b_left = (c1 + c2 * lo_bound_c) / lo_bound_c;
    let a_right = c2 * (edge_ratio - 1.0) / (edge_ratio * (1.0 - hi_bound_c));
    let b_right = (c2 - edge_ratio * c1 - 2.0 * edge_ratio * c2
        + c2 * edge_ratio * (1.0 - hi_bound_c)
        + edge_ratio)
        / (edge_ratio * (1.0 - hi_bound_c));
    let c_right = (c2 * edge_ratio - c2) * (c1 - hi_bound_c + c2 * hi_bound_c)
        / (edge_ratio * (1.0 - hi_bound_c) * (1.0 - hi_bound_c));

    let values = [
        center_size_aligned,
        center_shift_aligned,
        foveation_scale,
        optimized,
        optimized_aligned_float,
        view_ratio_aligned,
        c1,
        c2,
        lo_bound,
        hi_bound,
        lo_bound_c,
        hi_bound_c,
        a_left,
        b_left,
        a_right,
        b_right,
        c_right,
    ];
    if values.iter().any(|value| !value.is_finite()) {
        bail!("non-finite foveation constants: {values:?}");
    }

    Ok(AxisFoveationConstants {
        optimized_aligned: optimized_aligned_float as u32,
        view_ratio_aligned,
        edge_ratio,
        c1,
        c2,
        lo_bound,
        hi_bound,
        a_left,
        b_left,
        a_right,
        b_right,
        c_right,
    })
}

/// Receiver state
enum ReceiverState {
    Disconnected,
    Connecting,
    Connected,
    Streaming,
}

/// ALVR Video Receiver
///
/// This handles the connection to ALVR server and receiving video frames
pub struct AlvrVideoReceiver {
    state: Mutex<ReceiverState>,
    latest_frame: Mutex<Option<VideoFrame>>,
    connected: Arc<AtomicBool>,
    server_addr: Mutex<Option<String>>,
}

/// Global video receiver instance
static VIDEO_RECEIVER: parking_lot::Mutex<Option<Arc<AlvrVideoReceiver>>> =
    parking_lot::Mutex::new(None);

/// Initialize the ALVR video receiver
/// Returns the receiver instance
pub fn get_video_receiver() -> Arc<AlvrVideoReceiver> {
    let mut guard = VIDEO_RECEIVER.lock();
    if guard.is_none() {
        *guard = Some(Arc::new(AlvrVideoReceiver {
            state: Mutex::new(ReceiverState::Disconnected),
            latest_frame: Mutex::new(None),
            connected: Arc::new(AtomicBool::new(false)),
            server_addr: Mutex::new(None),
        }));
    }
    guard.as_ref().unwrap().clone()
}

/// Connect to ALVR server at the given address
pub async fn connect_to_alvr(
    receiver: &AlvrVideoReceiver,
    server_addr: &str,
    port: u16,
) -> Result<()> {
    *receiver.state.lock() = ReceiverState::Connecting;
    info!("Connecting to ALVR server at {}:{}", server_addr, port);

    // TODO: Implement actual ALVR connection protocol
    // For now, just mark as connected
    *receiver.state.lock() = ReceiverState::Connected;
    receiver.connected.store(true, Ordering::SeqCst);
    *receiver.server_addr.lock() = Some(format!("{}:{}", server_addr, port));

    Ok(())
}

/// Disconnect from ALVR server
pub fn disconnect(receiver: &AlvrVideoReceiver) {
    *receiver.state.lock() = ReceiverState::Disconnected;
    receiver.connected.store(false, Ordering::SeqCst);
    info!("Disconnected from ALVR server");
}

/// Push a video frame to the receiver
/// Called from the video decoder callback
pub fn push_video_frame(
    receiver: &AlvrVideoReceiver,
    timestamp_ns: u64,
    buffer_ptr: usize,
    width: u32,
    height: u32,
    row_pitch: u32,
    hardware_buffer_lease: Option<Arc<HardwareBufferLease>>,
) {
    let mut latest = receiver.latest_frame.lock();
    *latest = Some(VideoFrame {
        timestamp_ns,
        buffer_ptr,
        width,
        height,
        row_pitch,
        hardware_buffer_lease,
        rgba: None,
    });
    *receiver.state.lock() = ReceiverState::Streaming;
}

/// Push a CPU RGBA video frame to the receiver.
pub fn push_rgba_frame(
    receiver: &AlvrVideoReceiver,
    timestamp_ns: u64,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) {
    let mut latest = receiver.latest_frame.lock();
    *latest = Some(VideoFrame {
        timestamp_ns,
        buffer_ptr: 0,
        width,
        height,
        row_pitch: width.saturating_mul(4),
        hardware_buffer_lease: None,
        rgba: Some(Arc::new(rgba)),
    });
    *receiver.state.lock() = ReceiverState::Streaming;
}

/// Get the latest video frame.
///
/// This intentionally keeps the frame in the receiver so the renderer can
/// continue presenting the last decoded frame while the staging decoder is
/// slower than the Pimax display loop.
pub fn get_latest_frame(receiver: &AlvrVideoReceiver) -> Option<VideoFrame> {
    receiver.latest_frame.lock().clone()
}

/// Check if connected to ALVR server
pub fn is_connected(receiver: &AlvrVideoReceiver) -> bool {
    receiver.connected.load(Ordering::SeqCst)
}

/// Get connection info
pub fn get_connection_info(receiver: &AlvrVideoReceiver) -> Option<String> {
    receiver.server_addr.lock().clone()
}

/// Start a temporary raw-RGBA TCP receiver for compositor-path testing.
///
/// Packet format, repeated per frame:
/// - 8 bytes: ASCII `PIMXRGBA`
/// - u32 little-endian width
/// - u32 little-endian height
/// - u64 little-endian frame index
/// - u32 little-endian payload length
/// - payload bytes: tightly packed RGBA8, width * height * 4
pub fn start_debug_rgba_tcp_receiver(receiver: Arc<AlvrVideoReceiver>, port: u16) -> Result<()> {
    if DEBUG_RGBA_TCP_STARTED.swap(true, Ordering::SeqCst) {
        info!("debug RGBA TCP receiver already started");
        return Ok(());
    }

    let listener = TcpListener::bind(("0.0.0.0", port))
        .with_context(|| format!("bind debug RGBA TCP receiver on 0.0.0.0:{port}"))?;
    listener
        .set_nonblocking(false)
        .context("configure debug RGBA TCP listener blocking mode")?;

    thread::Builder::new()
        .name("debug-rgba-receiver".to_string())
        .spawn(move || {
            info!("debug RGBA TCP receiver listening on 0.0.0.0:{port}");
            for incoming in listener.incoming() {
                match incoming {
                    Ok(stream) => {
                        let peer = stream
                            .peer_addr()
                            .map(|addr| addr.to_string())
                            .unwrap_or_else(|_| "<unknown>".to_string());
                        info!("debug RGBA TCP client connected from {peer}");
                        if let Err(err) = handle_debug_rgba_client(stream, receiver.as_ref()) {
                            warn!("debug RGBA TCP client {peer} disconnected: {err:#}");
                        }
                    }
                    Err(err) => {
                        warn!("debug RGBA TCP accept failed: {err:#}");
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        })
        .context("spawn debug RGBA TCP receiver thread")?;

    Ok(())
}

fn handle_debug_rgba_client(mut stream: TcpStream, receiver: &AlvrVideoReceiver) -> Result<()> {
    stream
        .set_nodelay(true)
        .context("enable TCP_NODELAY for debug RGBA client")?;

    loop {
        let mut header = [0_u8; DEBUG_RGBA_HEADER_LEN];
        if let Err(err) = stream.read_exact(&mut header) {
            if err.kind() == ErrorKind::UnexpectedEof {
                info!("debug RGBA TCP client closed");
                return Ok(());
            }
            return Err(err).context("read debug RGBA frame header");
        }
        if &header[..8] != DEBUG_RGBA_MAGIC {
            bail!("invalid debug RGBA magic {:?}", &header[..8]);
        }

        let width = read_u32_le(&header[8..12]);
        let height = read_u32_le(&header[12..16]);
        let frame_index = read_u64_le(&header[16..24]);
        let payload_len = read_u32_le(&header[24..28]) as usize;
        let expected_len = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .context("debug RGBA dimensions overflow")?;

        if payload_len != expected_len {
            bail!(
                "debug RGBA payload length mismatch: got {payload_len}, expected {expected_len} for {width}x{height}"
            );
        }
        if payload_len > DEBUG_RGBA_MAX_PAYLOAD_BYTES {
            bail!(
                "debug RGBA payload too large: {payload_len} bytes exceeds {DEBUG_RGBA_MAX_PAYLOAD_BYTES}"
            );
        }

        let mut rgba = vec![0_u8; payload_len];
        stream
            .read_exact(&mut rgba)
            .with_context(|| format!("read debug RGBA frame {frame_index} payload"))?;
        push_rgba_frame(receiver, timestamp_now_ns(), width, height, rgba);

        if frame_index < 5 || frame_index % 120 == 0 {
            info!(
                "received debug RGBA frame {frame_index}: size={}x{} payload={} bytes",
                width, height, payload_len
            );
        }

        stream.write_all(b"OK").context("ack debug RGBA frame")?;
    }
}

fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn read_u64_le(bytes: &[u8]) -> u64 {
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

fn timestamp_now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn current_framebuffer_binding() -> u32 {
    let mut framebuffer = 0_i32;
    unsafe {
        glGetIntegerv(GL_FRAMEBUFFER_BINDING, &mut framebuffer);
    }
    framebuffer.max(0) as u32
}

/// Get or create the passthrough OES shader used in pass 1 of the two-pass blit.
fn get_passthrough_oes_program() -> Result<PassthroughProgram> {
    let mut prog = PASSTHROUGH_OES_PROGRAM.lock();
    if let Some(existing) = prog.as_ref() {
        return Ok(PassthroughProgram {
            program: existing.program,
            texture_uniform: existing.texture_uniform,
            black_crush_uniform: existing.black_crush_uniform,
            color_gain_uniform: existing.color_gain_uniform,
        });
    }

    let vertex_shader = compile_shader(
        GL_VERTEX_SHADER,
        r#"#version 300 es
precision highp float;
out vec2 v_uv;
void main() {
    vec2 pos;
    if (gl_VertexID == 0) {
        pos = vec2(-1.0, -1.0);
    } else if (gl_VertexID == 1) {
        pos = vec2(3.0, -1.0);
    } else {
        pos = vec2(-1.0, 3.0);
    }
    v_uv = (pos + vec2(1.0)) * 0.5;
    gl_Position = vec4(pos, 0.0, 1.0);
}
"#,
    )
    .context("compile passthrough OES vertex shader")?;
    let fragment_shader = compile_shader(
        GL_FRAGMENT_SHADER,
        r#"#version 300 es
#extension GL_OES_EGL_image_external_essl3 : require
precision highp float;
uniform samplerExternalOES u_texture;
uniform float u_black_crush;
uniform float u_color_gain;
in vec2 v_uv;
out vec4 out_color;
void main() {
    vec4 color = texture(u_texture, v_uv);
    // Expand BT.709 limited range (16-235) to full range (0-255).
    color.rgb = clamp((color.rgb - u_black_crush) * u_color_gain, 0.0, 1.0);
    out_color = color;
}
"#,
    )
    .context("compile passthrough OES fragment shader")?;

    let linked = link_program(vertex_shader, fragment_shader)
        .context("link passthrough OES shader program")?;
    unsafe {
        glDeleteShader(vertex_shader);
        glDeleteShader(fragment_shader);
    }

    let texture_name = CString::new("u_texture").context("create texture uniform name")?;
    let texture_uniform =
        unsafe { glGetUniformLocation(linked, texture_name.as_ptr().cast::<i8>()) };
    if texture_uniform < 0 {
        unsafe { glDeleteProgram(linked) };
        bail!("passthrough OES shader missing u_texture uniform");
    }
    let black_crush_uniform = uniform_location(linked, "u_black_crush")?;
    let color_gain_uniform = uniform_location(linked, "u_color_gain")?;

    let created = PassthroughProgram {
        program: linked,
        texture_uniform,
        black_crush_uniform,
        color_gain_uniform,
    };
    *prog = Some(PassthroughProgram {
        program: linked,
        texture_uniform,
        black_crush_uniform,
        color_gain_uniform,
    });
    info!("created passthrough OES shader program for two-pass blit");
    Ok(created)
}

/// Get or create an intermediate RGBA framebuffer for the two-pass blit.
/// Re-creates when dimensions change.
fn get_intermediate_fbo(width: i32, height: i32) -> Result<IntermediateFbo> {
    let mut fbo = INTERMEDIATE_FBO.lock();
    if let Some(existing) = fbo.as_ref() {
        if existing.width == width && existing.height == height {
            return Ok(IntermediateFbo {
                framebuffer: existing.framebuffer,
                texture: existing.texture,
                width: existing.width,
                height: existing.height,
            });
        }
        // Dimensions changed, delete old resources
        unsafe {
            glDeleteFramebuffers(1, &existing.framebuffer);
            glDeleteTextures(1, &existing.texture);
        }
        *fbo = None;
    }

    unsafe {
        let mut texture = 0_u32;
        glGenTextures(1, &mut texture);
        if texture == 0 {
            bail!("glGenTextures returned 0 for intermediate RGBA texture");
        }
        glBindTexture(GL_TEXTURE_2D, texture);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        glTexImage2D(
            GL_TEXTURE_2D,
            0,
            GL_RGBA as i32,
            width,
            height,
            0,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            ptr::null(),
        );
        glBindTexture(GL_TEXTURE_2D, 0);

        let mut framebuffer = 0_u32;
        glGenFramebuffers(1, &mut framebuffer);
        if framebuffer == 0 {
            glDeleteTextures(1, &texture);
            bail!("glGenFramebuffers returned 0 for intermediate FBO");
        }
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
        let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        if status != GL_FRAMEBUFFER_COMPLETE {
            glDeleteFramebuffers(1, &framebuffer);
            glDeleteTextures(1, &texture);
            bail!("intermediate FBO incomplete: status=0x{:X}", status);
        }

        let created = IntermediateFbo {
            framebuffer,
            texture,
            width,
            height,
        };
        info!(
            "created intermediate RGBA FBO {}x{} for two-pass blit (fbo={}, tex={})",
            width, height, framebuffer, texture
        );
        *fbo = Some(IntermediateFbo {
            framebuffer,
            texture,
            width,
            height,
        });
        Ok(created)
    }
}

fn get_blit_program() -> Result<BlitProgram> {
    let mut program = BLIT_PROGRAM.lock();
    if let Some(existing) = program.as_ref() {
        return Ok(BlitProgram {
            program: existing.program,
            texture_target: existing.texture_target,
            texture_target_label: existing.texture_target_label,
            texture_uniform: existing.texture_uniform,
            uv_rect_uniform: existing.uv_rect_uniform,
            foveation_enabled_uniform: existing.foveation_enabled_uniform,
            view_index_uniform: existing.view_index_uniform,
            position_offset_x_uniform: existing.position_offset_x_uniform,
            foveation_view_ratio_edge_uniform: existing.foveation_view_ratio_edge_uniform,
            foveation_c1_c2_uniform: existing.foveation_c1_c2_uniform,
            foveation_bounds_uniform: existing.foveation_bounds_uniform,
            foveation_left_uniform: existing.foveation_left_uniform,
            foveation_right_uniform: existing.foveation_right_uniform,
            foveation_c_right_uniform: existing.foveation_c_right_uniform,
        });
    }

    // Pass 2 of two-pass blit always samples from a standard RGBA texture via GL_TEXTURE_2D.
    let created = create_blit_program(false)
        .context("create GL_TEXTURE_2D blit program for pass 2")?;
    *program = Some(BlitProgram {
        program: created.program,
        texture_target: created.texture_target,
        texture_target_label: created.texture_target_label,
        texture_uniform: created.texture_uniform,
        uv_rect_uniform: created.uv_rect_uniform,
        foveation_enabled_uniform: created.foveation_enabled_uniform,
        view_index_uniform: created.view_index_uniform,
        position_offset_x_uniform: created.position_offset_x_uniform,
        foveation_view_ratio_edge_uniform: created.foveation_view_ratio_edge_uniform,
        foveation_c1_c2_uniform: created.foveation_c1_c2_uniform,
        foveation_bounds_uniform: created.foveation_bounds_uniform,
        foveation_left_uniform: created.foveation_left_uniform,
        foveation_right_uniform: created.foveation_right_uniform,
        foveation_c_right_uniform: created.foveation_c_right_uniform,
    });
    Ok(created)
}

fn create_blit_program(use_external_texture: bool) -> Result<BlitProgram> {
    let texture_target = if use_external_texture {
        GL_TEXTURE_EXTERNAL_OES
    } else {
        GL_TEXTURE_2D
    };
    let texture_target_label = if use_external_texture {
        "GL_TEXTURE_EXTERNAL_OES"
    } else {
        "GL_TEXTURE_2D"
    };
    let sampler_declaration = if use_external_texture {
        "#extension GL_OES_EGL_image_external_essl3 : require\nprecision highp float;\nuniform samplerExternalOES u_texture;"
    } else {
        "precision highp float;\nuniform sampler2D u_texture;"
    };

    let vertex_shader = compile_shader(
        GL_VERTEX_SHADER,
        r#"#version 300 es
precision highp float;
uniform float u_position_offset_x;
out vec2 v_uv;
void main() {
    vec2 pos;
    if (gl_VertexID == 0) {
        pos = vec2(-1.0, -1.0);
    } else if (gl_VertexID == 1) {
        pos = vec2(3.0, -1.0);
    } else {
        pos = vec2(-1.0, 3.0);
    }
    // Compute UVs from the unshifted position so content is sampled correctly,
    // then apply a convergence shift to gl_Position only. This pans the rendered
    // content within the eye buffer to pre-cancel the Pimax compositor's built-in
    // divergent warp (~0.248 NDC per eye).
    v_uv = (pos + vec2(1.0)) * 0.5;
    pos.x += u_position_offset_x;
    gl_Position = vec4(pos, 0.0, 1.0);
}
"#,
    )
    .context("compile hardware-buffer blit vertex shader")?;
    let fragment_source = format!(
        r#"#version 300 es
{sampler_declaration}
uniform vec4 u_uv_rect;
uniform int u_enable_foveation;
uniform int u_view_index;
uniform vec4 u_foveation_view_ratio_edge;
uniform vec4 u_foveation_c1_c2;
uniform vec4 u_foveation_bounds;
uniform vec4 u_foveation_left;
uniform vec4 u_foveation_right;
uniform vec4 u_foveation_c_right;
in vec2 v_uv;
out vec4 out_color;

vec2 apply_foveation(vec2 uv) {{
    vec2 corrected_uv = uv;
    if (u_enable_foveation == 0) {{
        return corrected_uv;
    }}

    vec2 view_size_ratio = u_foveation_view_ratio_edge.xy;
    vec2 edge_ratio = u_foveation_view_ratio_edge.zw;
    vec2 c1 = u_foveation_c1_c2.xy;
    vec2 c2 = u_foveation_c1_c2.zw;
    vec2 lo_bound = u_foveation_bounds.xy;
    vec2 hi_bound = u_foveation_bounds.zw;
    vec2 a_left = u_foveation_left.xy;
    vec2 b_left = u_foveation_left.zw;
    vec2 a_right = u_foveation_right.xy;
    vec2 b_right = u_foveation_right.zw;
    vec2 c_right = u_foveation_c_right.xy;

    // Removed UV flip for right eye - let the UV range swap handle it

    vec2 center = (corrected_uv - c1) * edge_ratio / c2;
    vec2 left_discriminant = max(b_left * b_left + 4.0 * a_left * corrected_uv, vec2(0.0));
    vec2 right_discriminant =
        max(b_right * b_right - 4.0 * (c_right - a_right * corrected_uv), vec2(0.0));
    vec2 left_edge = (-b_left + sqrt(left_discriminant)) / (2.0 * a_left);
    vec2 right_edge = (-b_right + sqrt(right_discriminant)) / (2.0 * a_right);

    if (corrected_uv.x < lo_bound.x) {{
        corrected_uv.x = left_edge.x;
    }} else if (corrected_uv.x > hi_bound.x) {{
        corrected_uv.x = right_edge.x;
    }} else {{
        corrected_uv.x = center.x;
    }}

    if (corrected_uv.y < lo_bound.y) {{
        corrected_uv.y = left_edge.y;
    }} else if (corrected_uv.y > hi_bound.y) {{
        corrected_uv.y = right_edge.y;
    }} else {{
        corrected_uv.y = center.y;
    }}

    corrected_uv *= view_size_ratio;

    return corrected_uv;
}}

void main() {{
    vec2 corrected_uv = clamp(apply_foveation(v_uv), vec2(0.0), vec2(1.0));
    vec2 sample_uv = mix(u_uv_rect.xy, u_uv_rect.zw, corrected_uv);
    out_color = texture(u_texture, sample_uv);
}}
"#,
    );
    let fragment_shader =
        compile_shader(GL_FRAGMENT_SHADER, &fragment_source).with_context(|| {
            format!("compile hardware-buffer blit fragment shader ({texture_target_label})")
        })?;

    let linked = link_program(vertex_shader, fragment_shader)
        .context("link hardware-buffer blit shader program")?;
    unsafe {
        glDeleteShader(vertex_shader);
        glDeleteShader(fragment_shader);
    }

    let texture_name = CString::new("u_texture").context("create texture uniform name")?;
    let uv_rect_name = CString::new("u_uv_rect").context("create uv uniform name")?;
    let texture_uniform =
        unsafe { glGetUniformLocation(linked, texture_name.as_ptr().cast::<i8>()) };
    let uv_rect_uniform =
        unsafe { glGetUniformLocation(linked, uv_rect_name.as_ptr().cast::<i8>()) };
    let foveation_enabled_uniform = uniform_location(linked, "u_enable_foveation")?;
    let view_index_uniform = uniform_location(linked, "u_view_index")?;
    let position_offset_x_uniform = uniform_location(linked, "u_position_offset_x")?;
    let foveation_view_ratio_edge_uniform =
        uniform_location(linked, "u_foveation_view_ratio_edge")?;
    let foveation_c1_c2_uniform = uniform_location(linked, "u_foveation_c1_c2")?;
    let foveation_bounds_uniform = uniform_location(linked, "u_foveation_bounds")?;
    let foveation_left_uniform = uniform_location(linked, "u_foveation_left")?;
    let foveation_right_uniform = uniform_location(linked, "u_foveation_right")?;
    let foveation_c_right_uniform = uniform_location(linked, "u_foveation_c_right")?;
    if texture_uniform < 0 || uv_rect_uniform < 0 {
        unsafe { glDeleteProgram(linked) };
        bail!(
            "hardware-buffer blit shader missing uniforms: texture={} uv_rect={}",
            texture_uniform,
            uv_rect_uniform
        );
    }

    let created = BlitProgram {
        program: linked,
        texture_target,
        texture_target_label,
        texture_uniform,
        uv_rect_uniform,
        foveation_enabled_uniform,
        view_index_uniform,
        position_offset_x_uniform,
        foveation_view_ratio_edge_uniform,
        foveation_c1_c2_uniform,
        foveation_bounds_uniform,
        foveation_left_uniform,
        foveation_right_uniform,
        foveation_c_right_uniform,
    };
    info!("created hardware-buffer stereo blit shader program using {texture_target_label}");
    Ok(created)
}

fn uniform_location(program: u32, name: &str) -> Result<i32> {
    let name = CString::new(name).with_context(|| format!("create uniform name {name}"))?;
    Ok(unsafe { glGetUniformLocation(program, name.as_ptr().cast::<i8>()) })
}

fn compile_shader(shader_type: u32, source: &str) -> Result<u32> {
    let shader = unsafe { glCreateShader(shader_type) };
    if shader == 0 {
        bail!("glCreateShader returned 0 for type=0x{shader_type:04x}");
    }

    let source = CString::new(source).context("shader source contains NUL")?;
    let source_ptr = source.as_ptr().cast::<i8>();
    unsafe {
        glShaderSource(shader, 1, &source_ptr, ptr::null());
        glCompileShader(shader);
    }

    let mut status = 0_i32;
    unsafe {
        glGetShaderiv(shader, GL_COMPILE_STATUS, &mut status);
    }
    if status == 0 {
        let log = shader_info_log(shader);
        unsafe { glDeleteShader(shader) };
        bail!("shader compile failed: {log}");
    }

    Ok(shader)
}

fn link_program(vertex_shader: u32, fragment_shader: u32) -> Result<u32> {
    let program = unsafe { glCreateProgram() };
    if program == 0 {
        bail!("glCreateProgram returned 0");
    }

    unsafe {
        glAttachShader(program, vertex_shader);
        glAttachShader(program, fragment_shader);
        glLinkProgram(program);
    }

    let mut status = 0_i32;
    unsafe {
        glGetProgramiv(program, GL_LINK_STATUS, &mut status);
    }
    if status == 0 {
        let log = program_info_log(program);
        unsafe { glDeleteProgram(program) };
        bail!("program link failed: {log}");
    }

    Ok(program)
}

fn shader_info_log(shader: u32) -> String {
    let mut len = 0_i32;
    unsafe {
        glGetShaderiv(shader, GL_INFO_LOG_LENGTH, &mut len);
    }
    let mut buffer = vec![0_u8; len.max(1) as usize];
    let mut written = 0_i32;
    unsafe {
        glGetShaderInfoLog(shader, len, &mut written, buffer.as_mut_ptr().cast());
    }
    String::from_utf8_lossy(&buffer[..(written.max(0) as usize).min(buffer.len())]).into_owned()
}

fn program_info_log(program: u32) -> String {
    let mut len = 0_i32;
    unsafe {
        glGetProgramiv(program, GL_INFO_LOG_LENGTH, &mut len);
    }
    let mut buffer = vec![0_u8; len.max(1) as usize];
    let mut written = 0_i32;
    unsafe {
        glGetProgramInfoLog(program, len, &mut written, buffer.as_mut_ptr().cast());
    }
    String::from_utf8_lossy(&buffer[..(written.max(0) as usize).min(buffer.len())]).into_owned()
}

fn active_foveation_for_frame(frame: &VideoFrame) -> Option<FoveationShaderConstants> {
    let active = *FOVEATED_ENCODING.lock();
    let active = active?;
    if frame.width == active.optimized_frame_width && frame.height == active.optimized_frame_height
    {
        Some(active.constants)
    } else {
        None
    }
}

fn source_eye_for_target(eye: VideoFrameEye) -> VideoFrameEye {
    if !PIMAX_SWAP_STREAM_EYES {
        return eye;
    }

    match eye {
        VideoFrameEye::Left => VideoFrameEye::Right,
        VideoFrameEye::Right => VideoFrameEye::Left,
    }
}

/// Render an AHardwareBuffer to an EyeRenderTarget texture using a two-pass blit.
///
/// **Pass 1**: AHardwareBuffer → intermediate RGBA texture via `GL_TEXTURE_EXTERNAL_OES`.
///   The driver's OES sampler performs YUV-to-RGB conversion and applies whatever
///   internal texture transform the vendor requires — producing a clean RGBA image.
///
/// **Pass 2**: intermediate RGBA texture → eye render target via `GL_TEXTURE_2D` with
///   the foveation + stereo-split shader.  Since the intermediate is standard RGBA,
///   UV mapping works correctly without driver interference.
pub(crate) fn render_ahardwarebuffer_to_target(
    target: &EyeRenderTarget,
    frame: &VideoFrame,
    eye: VideoFrameEye,
) -> Result<()> {
    let hardware_buffer_ptr = frame.buffer_ptr;
    if hardware_buffer_ptr == 0 {
        return Ok(());
    }

    // Load EGL extension functions
    let get_native_client_buffer =
        load_egl_proc::<EglGetNativeClientBufferAndroid>("eglGetNativeClientBufferANDROID")
            .context("load eglGetNativeClientBufferANDROID")?;
    let create_image = load_egl_proc::<EglCreateImageKhr>("eglCreateImageKHR")
        .context("load eglCreateImageKHR")?;
    let destroy_image = load_egl_proc::<EglDestroyImageKhr>("eglDestroyImageKHR")
        .context("load eglDestroyImageKHR")?;
    let image_target_fn =
        load_egl_proc::<GlEglImageTargetTexture2dOes>("glEGLImageTargetTexture2DOES")
            .context("load glEGLImageTargetTexture2DOES")?;

    let passthrough = get_passthrough_oes_program()
        .context("get passthrough OES shader for pass 1")?;
    let blit_program = get_blit_program().context("get blit shader for pass 2")?;

    let display = unsafe { eglGetCurrentDisplay() };
    if display.is_null() {
        bail!("eglGetCurrentDisplay returned null");
    }

    // Create EGLImage from AHardwareBuffer
    let client_buffer = unsafe { get_native_client_buffer(hardware_buffer_ptr as *const c_void) };
    if client_buffer.is_null() {
        bail!("eglGetNativeClientBufferANDROID returned null");
    }

    let image_attribs = [EGL_IMAGE_PRESERVED_KHR, 1, EGL_NONE];
    let egl_image = unsafe {
        create_image(
            display,
            ptr::null_mut(),
            EGL_NATIVE_BUFFER_ANDROID,
            client_buffer,
            image_attribs.as_ptr(),
        )
    };
    if egl_image.is_null() {
        bail!("eglCreateImageKHR returned null");
    }

    // Get intermediate FBO sized to the source frame
    let intermediate = get_intermediate_fbo(frame.width as i32, frame.height as i32)
        .context("get intermediate FBO for two-pass blit")?;

    let source_eye = source_eye_for_target(eye);
    let side_by_side_stereo = frame.width >= frame.height.saturating_mul(2) && frame.width >= 2;
    let (u0, u1) = if side_by_side_stereo {
        match source_eye {
            VideoFrameEye::Left => (0.0_f32, 0.5_f32),
            VideoFrameEye::Right => (0.5_f32, 1.0_f32),
        }
    } else {
        (0.0_f32, 1.0_f32)
    };
    let (v0, v1) = if PIMAX_FLIP_STREAM_VERTICAL {
        (1.0_f32, 0.0_f32)
    } else {
        (0.0_f32, 1.0_f32)
    };
    let foveation = active_foveation_for_frame(frame);
    let view_index = match eye {
        VideoFrameEye::Left => 0,
        VideoFrameEye::Right => 1,
    };

    unsafe {
        let previous_framebuffer = current_framebuffer_binding();

        // --- Pass 1: AHardwareBuffer → intermediate RGBA via OES ---
        let mut source_texture = 0_u32;
        glGenTextures(1, &mut source_texture);
        if source_texture == 0 {
            destroy_image(display, egl_image);
            bail!("glGenTextures returned 0 for decoded AHardwareBuffer source");
        }

        glBindTexture(GL_TEXTURE_EXTERNAL_OES, source_texture);
        glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        image_target_fn(GL_TEXTURE_EXTERNAL_OES, egl_image);

        glBindFramebuffer(GL_FRAMEBUFFER, intermediate.framebuffer);
        glViewport(0, 0, intermediate.width, intermediate.height);
        glUseProgram(passthrough.program);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_EXTERNAL_OES, source_texture);
        glUniform1i(passthrough.texture_uniform, 0);
        // Live-tunable color expansion (adjustable via HTTP at :7878)
        glUniform1f(passthrough.black_crush_uniform, crate::tune::color_black_crush());
        glUniform1f(passthrough.color_gain_uniform, crate::tune::color_gain());
        glDrawArrays(GL_TRIANGLES, 0, 3);

        // Clean up pass 1
        glBindTexture(GL_TEXTURE_EXTERNAL_OES, 0);
        glDeleteTextures(1, &source_texture);
        if destroy_image(display, egl_image) == 0 {
            warn!("eglDestroyImageKHR failed for decoded AHardwareBuffer image");
        }

        // --- Pass 2: intermediate RGBA → eye render target via GL_TEXTURE_2D + foveation ---
        glBindFramebuffer(GL_FRAMEBUFFER, target.framebuffer);
        glViewport(0, 0, target.width, target.height);
        // Clear to black so the strip left uncovered by the convergence pre-shift shows black
        // rather than the Pimax background panel bleeding through.
        glClearColor(0.0, 0.0, 0.0, 1.0);
        glClear(0x4000); // GL_COLOR_BUFFER_BIT = 0x00004000
        glUseProgram(blit_program.program);
        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_2D, intermediate.texture);
        glUniform1i(blit_program.texture_uniform, 0);
        glUniform4f(blit_program.uv_rect_uniform, u0, v0, u1, v1);
        glUniform1i(
            blit_program.foveation_enabled_uniform,
            if foveation.is_some() { 1 } else { 0 },
        );
        glUniform1i(blit_program.view_index_uniform, view_index);
        // Pre-shift each eye's rendered output convergently to cancel the Pimax
        // compositor's built-in divergent warp. Tunable live via HTTP at :7878.
        let shift = crate::tune::convergence_shift_ndc();
        let convergence_shift = match eye {
            VideoFrameEye::Left => shift,
            VideoFrameEye::Right => -shift,
        };
        glUniform1f(blit_program.position_offset_x_uniform, convergence_shift);
        if let Some(constants) = foveation {
            glUniform4f(
                blit_program.foveation_view_ratio_edge_uniform,
                constants.view_width_ratio,
                constants.view_height_ratio,
                constants.edge_ratio_x,
                constants.edge_ratio_y,
            );
            glUniform4f(
                blit_program.foveation_c1_c2_uniform,
                constants.c1_x,
                constants.c1_y,
                constants.c2_x,
                constants.c2_y,
            );
            glUniform4f(
                blit_program.foveation_bounds_uniform,
                constants.lo_bound_x,
                constants.lo_bound_y,
                constants.hi_bound_x,
                constants.hi_bound_y,
            );
            glUniform4f(
                blit_program.foveation_left_uniform,
                constants.a_left_x,
                constants.a_left_y,
                constants.b_left_x,
                constants.b_left_y,
            );
            glUniform4f(
                blit_program.foveation_right_uniform,
                constants.a_right_x,
                constants.a_right_y,
                constants.b_right_x,
                constants.b_right_y,
            );
            glUniform4f(
                blit_program.foveation_c_right_uniform,
                constants.c_right_x,
                constants.c_right_y,
                0.0,
                0.0,
            );
        }
        glDrawArrays(GL_TRIANGLES, 0, 3);

        // Restore state
        glBindTexture(GL_TEXTURE_2D, 0);
        glUseProgram(0);
        glBindFramebuffer(GL_FRAMEBUFFER, previous_framebuffer);
        glFlush();
    }

    let count = AHB_BLIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if count <= 5 || count % 120 == 0 {
        info!(
            "two-pass blit to {:?} eye: source={}x{} intermediate={}x{} target={}x{} uv=({:.3},{:.3}) foveated={} blits={}",
            eye,
            frame.width,
            frame.height,
            intermediate.width,
            intermediate.height,
            target.width,
            target.height,
            u0,
            u1,
            foveation.is_some(),
            count
        );
    }

    Ok(())
}

/// Copy video frame data into an EyeRenderTarget texture
///
/// This is the main entry point for ALVR video frame integration.
/// Uses zero-copy GPU upload via EGLImage when possible.
pub(crate) fn copy_video_frame_to_target(
    target: &EyeRenderTarget,
    frame: &VideoFrame,
    eye: VideoFrameEye,
) -> Result<()> {
    if let Some(rgba) = frame.rgba.as_deref() {
        return copy_rgba_frame_to_target(target, frame, rgba, eye);
    }

    // Foveated ALVR frames are intentionally smaller than the target eye buffer.
    let foveated_frame = active_foveation_for_frame(frame).is_some();
    if !foveated_frame
        && (frame.width as i32 != target.width || frame.height as i32 != target.height)
    {
        warn!(
            "Frame dimensions {}x{} don't match target {}x{}",
            frame.width, frame.height, target.width, target.height
        );
    }

    // For AHardwareBuffer, use GPU upload via EGLImage
    if frame.buffer_ptr != 0 {
        return render_ahardwarebuffer_to_target(target, frame, eye);
    }

    // Fallback: CPU copy would require actual frame data buffer
    warn!("copy_video_frame_to_target called with null buffer_ptr");
    Ok(())
}

fn copy_rgba_frame_to_target(
    target: &EyeRenderTarget,
    frame: &VideoFrame,
    rgba: &[u8],
    eye: VideoFrameEye,
) -> Result<()> {
    let expected_len = (frame.width as usize)
        .checked_mul(frame.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("RGBA frame dimensions overflow")?;
    if rgba.len() < expected_len {
        bail!(
            "RGBA frame payload too small: got {} bytes, expected at least {expected_len}",
            rgba.len()
        );
    }

    let source_eye = source_eye_for_target(eye);
    let side_by_side_stereo = frame.width >= frame.height.saturating_mul(2) && frame.width >= 2;
    let (source_x, source_width) = if side_by_side_stereo {
        let half = (frame.width / 2) as i32;
        match source_eye {
            VideoFrameEye::Left => (0_i32, half),
            VideoFrameEye::Right => (half, half),
        }
    } else {
        (0_i32, frame.width as i32)
    };

    let upload_width = (source_width as i32).min(target.width).max(1);
    let upload_height = (frame.height as i32).min(target.height).max(1);
    let x_offset = ((target.width - upload_width) / 2).max(0);
    let y_offset = ((target.height - upload_height) / 2).max(0);
    let upload_bytes = (upload_width as usize) * (upload_height as usize) * 4;

    let clipped;
    let upload_data = if !PIMAX_FLIP_STREAM_VERTICAL
        && upload_width as u32 == frame.width
        && upload_height as u32 == frame.height
    {
        &rgba[..upload_bytes]
    } else {
        clipped = {
            let mut clipped = vec![0_u8; upload_bytes];
            let source_stride = frame.row_pitch.max(frame.width.saturating_mul(4)) as usize;
            let dest_stride = upload_width as usize * 4;
            let source_x_bytes = source_x as usize * 4;
            for row in 0..upload_height as usize {
                let source_row = if PIMAX_FLIP_STREAM_VERTICAL {
                    upload_height as usize - 1 - row
                } else {
                    row
                };
                let source_start = source_row
                    .checked_mul(source_stride)
                    .and_then(|offset| offset.checked_add(source_x_bytes))
                    .context("RGBA source row offset overflow")?;
                let source_end = source_start + dest_stride;
                let dest_start = row * dest_stride;
                clipped[dest_start..dest_start + dest_stride]
                    .copy_from_slice(&rgba[source_start..source_end]);
            }
            clipped
        };
        &clipped
    };

    unsafe {
        glBindTexture(GL_TEXTURE_2D, target.texture);
        glTexSubImage2D(
            GL_TEXTURE_2D,
            0,
            x_offset,
            y_offset,
            upload_width,
            upload_height,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            upload_data.as_ptr().cast(),
        );
        glBindTexture(GL_TEXTURE_2D, 0);
        glFlush();
    }

    Ok(())
}
