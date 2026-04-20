//! Pimax XR Integration
//!
//! # Overview
//!
//! This module integrates with the Pimax Crystal's XR runtime to render video
//! to the headset's display. It uses the Pimax SDK (PxrApi/PxrServiceApi) to:
//!
//! - Initialize the XR session
//! - Get head tracking poses
//! - Submit eye textures for display
//! - Handle hardware events (IPD changes, proximity sensor, etc.)
//!
//! # Architecture
//!
//! ```text
//! Pimax XR Runtime (PxrServiceApi)
//!     │
//!     ├── Head Tracking
//!     │   └── Provides orientation + position at 90Hz+
//!     │
//!     ├── Display Timing (VSync)
//!     │   └── Signals when to submit frames
//!     │
//!     └── Compositor
//!         ├── Receives eye textures from app
//!         ├── Applies distortion correction
//!         ├── Applies chromatic aberration correction
//!         ├── Applies divergent warp (~0.248 NDC per eye)
//!         └── Scans out to display panels
//! ```
//!
//! # Key Concepts
//!
//! ## XR Session Lifecycle
//!
//! 1. **Initialize**: Load PxrApi, query capabilities
//! 2. **Begin XR**: `sxrBeginXr()` - starts compositor, VSync
//! 3. **Render Loop**: Poll VSync, submit layers each frame
//! 4. **End XR**: `sxrEndXr()` - stops compositor
//! 5. **Shutdown**: `sxrShutdown()` - releases resources
//!
//! ## Tracking Modes
//!
//! The Pimax runtime supports different tracking modes:
//!
//! - **Rotation Only (1)**: 3DOF, no position tracking
//! - **Position (2)**: 6DOF with inside-out tracking
//! - **Rotation | Position (3)**: Full 6DOF
//!
//! For ALVR, we use **Rotation Only** mode because:
//! - ALVR provides the actual head tracking from PC VR runtime
//! - Pimax inside-out tracking would conflict
//! - We only need the display compositor, not Pimax's tracker
//!
//! ## Layer Submission
//!
//! Frames are submitted as `PimaxLayer` structures containing:
//! - Left and right eye textures (GL texture IDs)
//! - Head pose at time of rendering
//! - Timing information (vsync, predicted display time)
//!
//! The Pimax compositor then:
//! 1. Applies lens distortion
//! 2. Applies chromatic aberration correction
//! 3. Applies divergent warp (causes double vision if not compensated)
//! 4. Scans out to the physical display
//!
//! ## Divergent Warp
//!
//! The Pimax compositor applies a **divergent warp** to each eye:
//! - Left eye: shifted left
//! - Right eye: shifted right
//!
//! This is designed for content rendered natively on the headset.
//! For ALVR content (which already has correct stereo), this causes
//! double vision.
//!
//! **Solution**: Pre-shift the blit output in the opposite direction
//! so the compositor's warp cancels it out. See `video_receiver.rs`
//! for the convergence shift implementation.
//!
//! ## Hardware Buffer Integration
//!
//! Video frames from Android's MediaCodec arrive as `AHardwareBuffer`
//! objects. These are imported into OpenGL as `GL_TEXTURE_EXTERNAL_OES`
//! via EGLImageKHR:
//!
//! ```text
//! MediaCodec Decoder
//!     │
//!     │ AHardwareBuffer
//!     │ (Android native buffer)
//!     ▼
//! eglCreateImageKHR
//!     │
//!     │ EGLImageKHR
//!     ▼
//! glEglImageTargetTexture2DOES
//!     │
//!     │ GL_TEXTURE_EXTERNAL_OES
//!     ▼
//! Fragment Shader (OES sampler)
//! ```
//!
//! # JNI Integration
//!
//! The Pimax SDK uses Java/Kotlin classes. This module bridges Rust and Java:
//!
//! - `PxrApi`: Low-level XR functions (begin, end, submit)
//! - `PxrServiceApi`: High-level service (tracking, VSync)
//! - `PiHalUtils`: Hardware utilities (IPD, display mode)
//!
//! JNI calls are cached for performance (global refs to classes/methods).
//!
//! # Threading
//!
//! - **Main Thread**: Must call PxrApi functions (thread-affine)
//! - **VSync Pump**: Polls for display refresh signals
//! - **Render Thread**: Submits layers from decoded video frames
//!
//! # Error Handling
//!
//! PxrApi functions return status codes:
//! - `0`: Success
//! - Negative: Error (logged but often non-fatal)
//!
//! Many Pimax API calls are wrapped with error logging but don't abort
//! the render loop - a dropped frame is better than a crashed app.
//!
//! # Shutdown
//!
//! Graceful shutdown is triggered by:
//! - Java activity lifecycle (onPause, onDestroy)
//! - Proximity sensor (user removes headset)
//! - Panic or signal
//!
//! The `SHUTDOWN_REQUESTED` flag is checked each frame to exit cleanly.

use anyhow::bail;
use anyhow::{Context, Result};
use jni::{
    objects::{GlobalRef, JClass, JObject, JObjectArray, JString, JValue},
    JavaVM,
};
use log::{error, info, warn};
use ndk::hardware_buffer::{
    HardwareBuffer, HardwareBufferDesc, HardwareBufferRef, HardwareBufferUsage,
};
use std::os::raw::c_void;
use std::{
    ffi::CStr,
    ffi::CString,
    mem, ptr,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

/// Whether to run the expensive reflective Pimax SDK introspection during startup.
///
/// This is useful while reverse-engineering the controller/input surface, but it can
/// take long enough to trip the Android launch timeout on some boots. Keep the
/// default path lightweight so the headset gets a frame up quickly.
const PIMAX_VERBOSE_STARTUP_INTROSPECTION: bool = false;
const ENABLE_NATIVE_PIMAX_CONTROLLER_RUNTIME: bool = true;

type EGLDisplay = *mut c_void;
type EGLConfig = *mut c_void;
type EGLContext = *mut c_void;
type EGLSurface = *mut c_void;
type EGLImageKHR = *mut c_void;
type EGLClientBuffer = *mut c_void;

/// Global flag for graceful shutdown.
///
/// Set by JNI callbacks from VrRenderActivity when the Android
/// activity is paused, stopped, or destroyed.
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Global flag that asks the render loop to reassert Pimax presentation state.
///
/// Raised when the headset comes back on-face or the screen reports itself on again.
static PRESENTATION_REFRESH_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Latest headset proximity state reported by the Android activity.
///
/// The render loop uses this to avoid waking the panel immediately after Pimax's
/// own proximity state-machine has intentionally put it to sleep off-head.
static HEADSET_NEAR: AtomicBool = AtomicBool::new(false);

/// Check if shutdown has been requested
fn is_shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

fn reset_shutdown_requested() {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
}

fn request_shutdown(reason: &str) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    log::info!("Pimax shutdown requested: {reason}");
}

fn request_presentation_refresh(reason: &str) {
    PRESENTATION_REFRESH_REQUESTED.store(true, Ordering::SeqCst);
    log::info!("Pimax presentation refresh requested: {reason}");
}

fn take_presentation_refresh_requested() -> bool {
    PRESENTATION_REFRESH_REQUESTED.swap(false, Ordering::SeqCst)
}

fn is_headset_near() -> bool {
    HEADSET_NEAR.load(Ordering::SeqCst)
}

#[no_mangle]
pub extern "system" fn Java_com_pimax_alvr_client_VrRenderActivity_nativeRequestShutdown(
    _env: jni::JNIEnv<'_>,
    _class: JClass<'_>,
) {
    request_shutdown("VrRenderActivity lifecycle");
}

#[no_mangle]
pub extern "system" fn Java_com_pimax_alvr_client_VrRenderActivity_nativeResetShutdown(
    _env: jni::JNIEnv<'_>,
    _class: JClass<'_>,
) {
    reset_shutdown_requested();
    log::info!("Pimax shutdown reset: VrRenderActivity active");
}

#[no_mangle]
pub extern "system" fn Java_com_pimax_alvr_client_VrRenderActivity_nativeNotifyIpdChange(
    _env: jni::JNIEnv<'_>,
    _class: JClass<'_>,
    raw_ipd: f32,
) {
    crate::client::update_alvr_ipd_from_pimax(raw_ipd);
}

#[no_mangle]
pub extern "system" fn Java_com_pimax_alvr_client_VrRenderActivity_nativeNotifyProximity(
    _env: jni::JNIEnv<'_>,
    _class: JClass<'_>,
    is_near: jni::sys::jboolean,
) {
    let is_near = is_near != 0;
    HEADSET_NEAR.store(is_near, Ordering::SeqCst);
    info!("Pimax proximity state changed: near={is_near}");
    if is_near {
        request_presentation_refresh("proximity near");
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pimax_alvr_client_VrRenderActivity_nativeNotifyScreen(
    _env: jni::JNIEnv<'_>,
    _class: JClass<'_>,
    is_screen_on: jni::sys::jboolean,
) {
    let is_screen_on = is_screen_on != 0;
    info!("Pimax screen state changed: on={is_screen_on}");
    if is_screen_on && is_headset_near() {
        request_presentation_refresh("screen on");
    }
}

fn jhand_to_controller_hand(hand: jni::sys::jint) -> Option<crate::controller::Hand> {
    match hand {
        0 => Some(crate::controller::Hand::Left),
        1 => Some(crate::controller::Hand::Right),
        other => {
            warn!("ignoring controller JNI call with invalid hand index {other}");
            None
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_com_pimax_alvr_client_VrRenderActivity_nativeNotifyControllerState(
    _env: jni::JNIEnv<'_>,
    _class: JClass<'_>,
    hand: jni::sys::jint,
    handle: jni::sys::jint,
    buttons_pressed: jni::sys::jint,
    buttons_touched: jni::sys::jint,
    trigger: jni::sys::jfloat,
    grip: jni::sys::jfloat,
    thumbstick_x: jni::sys::jfloat,
    thumbstick_y: jni::sys::jfloat,
    battery: jni::sys::jint,
) {
    let Some(hand) = jhand_to_controller_hand(hand) else {
        return;
    };
    let state = crate::controller::SingleControllerState {
        connected: true,
        handle,
        motion: None,
        buttons_pressed: buttons_pressed as u32,
        buttons_touched: buttons_touched as u32,
        trigger,
        grip,
        thumbstick_x,
        thumbstick_y,
        battery_percent: battery.clamp(0, 100) as u8,
        last_updated: std::time::Instant::now(),
    };
    crate::controller::update_controller_state(hand, state);
}

#[no_mangle]
pub extern "system" fn Java_com_pimax_alvr_client_VrRenderActivity_nativeNotifyControllerConnection(
    _env: jni::JNIEnv<'_>,
    _class: JClass<'_>,
    hand: jni::sys::jint,
    connected: jni::sys::jboolean,
) {
    let Some(hand) = jhand_to_controller_hand(hand) else {
        return;
    };
    crate::controller::update_controller_connection(hand, connected != 0);
}

// Signal handler setup (platform-specific)
#[cfg(target_os = "android")]
extern "C" fn signal_handler(_sig: i32) {
    request_shutdown("process signal");
}

const EGL_FALSE: u32 = 0;
const EGL_TRUE: i32 = 1;
const EGL_NONE: i32 = 0x3038;
const EGL_RED_SIZE: i32 = 0x3024;
const EGL_GREEN_SIZE: i32 = 0x3023;
const EGL_BLUE_SIZE: i32 = 0x3022;
const EGL_ALPHA_SIZE: i32 = 0x3021;
const EGL_CONFIG_ID: i32 = 0x3028;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_PBUFFER_BIT: i32 = 0x0001;
const EGL_WINDOW_BIT: i32 = 0x0004;
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
const EGL_OPENGL_ES3_BIT_KHR: i32 = 0x0040;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;
const EGL_NATIVE_BUFFER_ANDROID: i32 = 0x3140;
const EGL_PROTECTED_CONTENT_EXT: i32 = 0x32C0;

#[link(name = "GLESv2")]
extern "C" {
    fn glClear(mask: u32);
    fn glClearColor(red: f32, green: f32, blue: f32, alpha: f32);
    fn glCheckFramebufferStatus(target: u32) -> u32;
    fn glColorMask(red: u8, green: u8, blue: u8, alpha: u8);
    fn glDeleteFramebuffers(n: i32, framebuffers: *const u32);
    fn glDeleteTextures(n: i32, textures: *const u32);
    fn glDisable(cap: u32);
    fn glFramebufferTexture2D(
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: u32,
        level: i32,
    );
    fn glGetError() -> u32;
    fn glGetIntegerv(pname: u32, data: *mut i32);
    fn glFinish();
    fn glGenFramebuffers(n: i32, framebuffers: *mut u32);
    fn glGenTextures(n: i32, textures: *mut u32);
    fn glBindFramebuffer(target: u32, framebuffer: u32);
    fn glBindTexture(target: u32, texture: u32);
    fn glViewport(x: i32, y: i32, width: i32, height: i32);
    fn glTexParameteri(target: u32, pname: u32, param: i32);
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
    fn glFlush();
    fn glGetString(name: u32) -> *const u8;
    fn glReadPixels(
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        format: u32,
        type_: u32,
        pixels: *mut c_void,
    );
}

#[link(name = "EGL")]
extern "C" {
    fn eglGetDisplay(display_id: *mut c_void) -> EGLDisplay;
    fn eglGetCurrentDisplay() -> EGLDisplay;
    fn eglGetCurrentContext() -> EGLContext;
    fn eglGetError() -> i32;
    fn eglGetProcAddress(procname: *const i8) -> *const c_void;
    fn eglInitialize(dpy: EGLDisplay, major: *mut i32, minor: *mut i32) -> u32;
    fn eglChooseConfig(
        dpy: EGLDisplay,
        attrib_list: *const i32,
        configs: *mut EGLConfig,
        config_size: i32,
        num_config: *mut i32,
    ) -> u32;
    fn eglGetConfigAttrib(
        dpy: EGLDisplay,
        config: EGLConfig,
        attribute: i32,
        value: *mut i32,
    ) -> u32;
    fn eglCreatePbufferSurface(
        dpy: EGLDisplay,
        config: EGLConfig,
        attrib_list: *const i32,
    ) -> EGLSurface;
    fn eglCreateWindowSurface(
        dpy: EGLDisplay,
        config: EGLConfig,
        win: *mut c_void,
        attrib_list: *const i32,
    ) -> EGLSurface;
    fn eglCreateContext(
        dpy: EGLDisplay,
        config: EGLConfig,
        share_context: EGLContext,
        attrib_list: *const i32,
    ) -> EGLContext;
    fn eglMakeCurrent(dpy: EGLDisplay, draw: EGLSurface, read: EGLSurface, ctx: EGLContext) -> u32;
    fn eglSwapBuffers(dpy: EGLDisplay, surface: EGLSurface) -> u32;
    fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> u32;
    fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> u32;
    fn eglTerminate(dpy: EGLDisplay) -> u32;
}

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_RG: u32 = 0x8227;
const GL_RGBA8: i32 = 0x8058;
const GL_RG32F: i32 = 0x8230;
const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;
const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
const GL_FRAMEBUFFER: u32 = 0x8D40;
const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
const GL_FRAMEBUFFER_BINDING: u32 = 0x8CA6;
const GL_FLOAT: u32 = 0x1406;
const GL_VENDOR: u32 = 0x1F00;
const GL_RENDERER: u32 = 0x1F01;
const GL_VERSION: u32 = 0x1F02;
const GL_SCISSOR_TEST: u32 = 0x0C11;
const GL_TRUE_BOOLEAN: u8 = 1;
const PIMAX_LAYER_FLAGS: i32 = 2; // sxrLayerFlags::kLayerFlagOpaque.
const PIMAX_BEGIN_OPTION_FLAGS: i32 = 0;
const PIMAX_FRAME_OPTIONS: i32 = 0;
const PIMAX_SUBMITTED_TEXTURE_TYPE: &str = "kTypeTexture";
const PIMAX_USE_EXPLICIT_EYE_RENDER_CALLS: bool = false;
const PIMAX_FORCE_GL_FINISH_BEFORE_SUBMIT: bool = true;
const PIMAX_CALL_ENABLE_PRESENTATION_AFTER_BEGIN: bool = false;
const PIMAX_USE_NATIVE_VSYNC_PUMP: bool = false;
// Let PxrApi own the service binding and VR-mode lifecycle. Manually creating
// a second PvrServiceClient produced duplicate "now existing 2 vr client" state
// on Crystal OG and left cleanup vulnerable to double-unbind Java exceptions.
const PIMAX_USE_EXPLICIT_PVR_SERVICE_CLIENT: bool = false;
const PIMAX_EGL_PROTECTED_PREFERENCE: [bool; 1] = [false];
const PIMAX_FRAME_MIN_VSYNCS: i32 = 0;
const PIMAX_EYE_BUFFER_PAIR_COUNT: usize = 4;
const PIMAX_EYE_RENDER_SCALE: f32 = 1.25;
const PIMAX_HIDE_SYSTEM_UI_EVERY_N_FRAMES: i32 = 120;
const PIMAX_UV_MAP_SAMPLES: i32 = 50;
const PIMAX_ENABLE_UV_MAP_HACK: bool = false;
const PIMAX_CONFIGURE_TEXTURE_METADATA: bool = false;
const PIMAX_RENDER_DIAGNOSTIC_PATTERN: bool = true;
const PIMAX_ANIMATE_DIAGNOSTIC_PATTERN: bool = true;
const PIMAX_RENDER_WINDOW_SURFACE_MIRROR: bool = false;
const PIMAX_BOOTSTRAP_WITH_ROTATION_ONLY: bool = true;
const PIMAX_PROMOTE_TO_POSITIONAL_TRACKING: bool = false;
const TRACKING_MODE_ROTATION: i32 = 1;
const TRACKING_MODE_POSITION: i32 = 2;
const TRACKING_MODE_ROTATION_POSITION: i32 = TRACKING_MODE_ROTATION | TRACKING_MODE_POSITION;
const PIMAX_CONTROLLER_QUERY_INTERVAL: Duration = Duration::from_secs(1);
const PIMAX_CONTROLLER_RING_SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
const PIMAX_CONTROLLER_RING_MAX_CHANGE_LOG_WORDS: usize = 32;
const PIMAX_NATIVE_CONTROLLER_POLL_INTERVAL: Duration = Duration::from_millis(20);
const PIMAX_NATIVE_CONTROLLER_CHANGE_LOG_INTERVAL: Duration = Duration::from_millis(250);
const PIMAX_NATIVE_CONTROLLER_BATTERY_INTERVAL: Duration = Duration::from_secs(1);
const PIMAX_NATIVE_CONTROLLER_STATE_SIZE: usize = 0xd0;
const PIMAX_NATIVE_CONTROLLER_MAX_CHANGE_LOG_WORDS: usize = 24;

const PIMAX_CONTROLLER_QUERY_BATTERY: i32 = 0;
const PIMAX_CONTROLLER_QUERY_ACTIVE_BUTTONS: i32 = 4;
const PIMAX_CONTROLLER_QUERY_ACTIVE_2D_ANALOGS: i32 = 5;
const PIMAX_CONTROLLER_QUERY_ACTIVE_1D_ANALOGS: i32 = 6;
const PIMAX_CONTROLLER_QUERY_ACTIVE_TOUCH_BUTTONS: i32 = 7;

const PIMAX_BUTTON_ONE: u32 = 1;
const PIMAX_BUTTON_TWO: u32 = 2;
const PIMAX_BUTTON_THREE: u32 = 4;
const PIMAX_BUTTON_FOUR: u32 = 8;
const PIMAX_BUTTON_BACK: u32 = 512;
const PIMAX_BUTTON_START: u32 = 256;
const PIMAX_BUTTON_PRIMARY_INDEX_TRIGGER: u32 = 8192;
const PIMAX_BUTTON_PRIMARY_HAND_TRIGGER: u32 = 16384;
const PIMAX_BUTTON_PRIMARY_THUMBSTICK: u32 = 32768;
const PIMAX_BUTTON_PRIMARY_THUMBSTICK_UP: u32 = 65536;
const PIMAX_BUTTON_PRIMARY_THUMBSTICK_DOWN: u32 = 131072;
const PIMAX_BUTTON_PRIMARY_THUMBSTICK_LEFT: u32 = 262144;
const PIMAX_BUTTON_PRIMARY_THUMBSTICK_RIGHT: u32 = 524288;
const PIMAX_BUTTON_SECONDARY_INDEX_TRIGGER: u32 = 2097152;
const PIMAX_BUTTON_SECONDARY_HAND_TRIGGER: u32 = 4194304;
const PIMAX_BUTTON_SECONDARY_THUMBSTICK: u32 = 8388608;
const PIMAX_BUTTON_SECONDARY_THUMBSTICK_UP: u32 = 16777216;
const PIMAX_BUTTON_SECONDARY_THUMBSTICK_DOWN: u32 = 33554432;
const PIMAX_BUTTON_SECONDARY_THUMBSTICK_LEFT: u32 = 67108864;
const PIMAX_BUTTON_SECONDARY_THUMBSTICK_RIGHT: u32 = 134217728;

const PIMAX_TOUCH_ONE: u32 = 1;
const PIMAX_TOUCH_TWO: u32 = 2;
const PIMAX_TOUCH_THREE: u32 = 4;
const PIMAX_TOUCH_FOUR: u32 = 8;
const PIMAX_TOUCH_PRIMARY_THUMBSTICK: u32 = 16;
const PIMAX_TOUCH_SECONDARY_THUMBSTICK: u32 = 32;
const PIMAX_NATIVE_TOUCH_TRIGGER: u32 = 0x0000_2000;
const PIMAX_NATIVE_TOUCH_GRIP: u32 = 0x0000_4000;

const PIMAX_AXIS_1D_PRIMARY_INDEX_TRIGGER: u32 = 1 << 0;
const PIMAX_AXIS_1D_SECONDARY_INDEX_TRIGGER: u32 = 1 << 1;
const PIMAX_AXIS_1D_PRIMARY_HAND_TRIGGER: u32 = 1 << 2;
const PIMAX_AXIS_1D_SECONDARY_HAND_TRIGGER: u32 = 1 << 3;
const PIMAX_AXIS_2D_PRIMARY_THUMBSTICK: u32 = 1 << 0;
const PIMAX_AXIS_2D_SECONDARY_THUMBSTICK: u32 = 1 << 1;

const ALVR_BUTTON_TRIGGER: u32 = 1 << 0;
const ALVR_BUTTON_THUMBSTICK_CLICK: u32 = 1 << 1;
const ALVR_BUTTON_MENU: u32 = 1 << 2;
const ALVR_BUTTON_GRIP: u32 = 1 << 3;
const ALVR_BUTTON_AX: u32 = 1 << 4;
const ALVR_BUTTON_BY: u32 = 1 << 5;

#[derive(Clone, Copy, Debug)]
enum SubmittedImageHandleKind {
    TextureId,
    EglImage,
    EglClientBuffer,
    HardwareBufferPtr,
}

const PIMAX_SUBMITTED_HANDLE_KIND: SubmittedImageHandleKind = SubmittedImageHandleKind::TextureId;
#[derive(Debug, Default, Clone)]
pub struct PimaxProbeReport {
    pub pxr_version: Option<String>,
    pub pxr_client_version: Option<String>,
    pub pxr_service_version: Option<String>,
    pub supported_tracking_modes: Option<i32>,
    pub current_tracking_mode: Option<i32>,
    pub current_tracking_mode_after_set: Option<i32>,
    pub service_supported_tracking_modes: Option<i32>,
    pub service_current_tracking_mode: Option<i32>,
    pub set_tracking_mode_result: Option<i32>,
    pub vr_mode: Option<i32>,
    pub start_vr_mode_result: Option<i32>,
}

impl PimaxProbeReport {
    pub fn summary(&self) -> String {
        format!(
            "pxr_version={:?}, client_version={:?}, service_version={:?}, supported_tracking_modes={:?}, current_tracking_mode={:?}, current_tracking_mode_after_set={:?}, service_supported_tracking_modes={:?}, service_current_tracking_mode={:?}, set_tracking_mode_result={:?}, vr_mode={:?}, start_vr_mode_result={:?}",
            self.pxr_version,
            self.pxr_client_version,
            self.pxr_service_version,
            self.supported_tracking_modes,
            self.current_tracking_mode,
            self.current_tracking_mode_after_set,
            self.service_supported_tracking_modes,
            self.service_current_tracking_mode,
            self.set_tracking_mode_result,
            self.vr_mode,
            self.start_vr_mode_result
        )
    }
}

/// Read an Android system property and parse it as f32.
fn read_system_property_float(name: &str) -> Option<f32> {
    use std::ffi::CString;
    use std::os::raw::c_char;
    extern "C" {
        fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
    }
    let c_name = CString::new(name).ok()?;
    let mut buf = [0 as c_char; 92];
    let len = unsafe { __system_property_get(c_name.as_ptr(), buf.as_mut_ptr()) };
    if len <= 0 {
        return None;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    s.to_str().ok()?.parse::<f32>().ok()
}

fn format_tracking_modes(bits: i32) -> String {
    let mut parts = Vec::new();
    if bits & TRACKING_MODE_ROTATION != 0 {
        parts.push("rotation");
    }
    if bits & TRACKING_MODE_POSITION != 0 {
        parts.push("position");
    }
    if parts.is_empty() {
        format!("none ({bits})")
    } else {
        format!("{} ({bits})", parts.join("|"))
    }
}

fn select_bootstrap_tracking_mode(supported_modes: Option<i32>) -> i32 {
    let supported_modes = supported_modes.unwrap_or(TRACKING_MODE_ROTATION_POSITION);
    if PIMAX_BOOTSTRAP_WITH_ROTATION_ONLY && (supported_modes & TRACKING_MODE_ROTATION) != 0 {
        TRACKING_MODE_ROTATION
    } else if supported_modes != 0 {
        supported_modes
    } else {
        TRACKING_MODE_ROTATION_POSITION
    }
}

fn select_promotion_tracking_mode(
    supported_modes: Option<i32>,
    bootstrap_mode: i32,
) -> Option<i32> {
    if !PIMAX_PROMOTE_TO_POSITIONAL_TRACKING {
        return None;
    }

    let supported_modes = supported_modes?;
    if (supported_modes & TRACKING_MODE_ROTATION_POSITION) == TRACKING_MODE_ROTATION_POSITION
        && bootstrap_mode != TRACKING_MODE_ROTATION_POSITION
    {
        Some(TRACKING_MODE_ROTATION_POSITION)
    } else {
        None
    }
}

fn set_pimax_tracking_mode(
    env: &mut jni::JNIEnv<'_>,
    tracking_mode: i32,
    reason: &str,
) -> Result<()> {
    call_static_void(
        env,
        "com/pimax/pxrapi/PxrApi",
        "sxrSetTrackingMode",
        "(I)V",
        &[JValue::Int(tracking_mode)],
    )
    .with_context(|| format!("set PxrApi tracking mode to {} ({reason})", tracking_mode))?;

    match call_static_int(
        env,
        "com/pimax/vrservice/PxrServiceApi",
        "SetTrackingMode",
        "(I)I",
        &[JValue::Int(tracking_mode)],
    ) {
        Ok(result) => info!(
            "PxrServiceApi.SetTrackingMode({}) -> {result} ({reason})",
            format_tracking_modes(tracking_mode)
        ),
        Err(err) => warn!(
            "PxrServiceApi.SetTrackingMode({}) failed ({reason}): {err:#}",
            format_tracking_modes(tracking_mode)
        ),
    }

    Ok(())
}

fn call_static_int<'a>(
    env: &mut jni::JNIEnv<'a>,
    class: &str,
    method: &str,
    signature: &str,
    args: &[JValue<'a, 'a>],
) -> Result<i32> {
    let value = match env.call_static_method(class, method, signature, args) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("call {class}.{method}{signature} failed: {err:#}; {summary}");
        }
    };
    value.i().context("decode jint result")
}

fn call_static_void(
    env: &mut jni::JNIEnv<'_>,
    class: &str,
    method: &str,
    signature: &str,
    args: &[JValue<'_, '_>],
) -> Result<()> {
    if let Err(err) = env.call_static_method(class, method, signature, args) {
        let summary = take_java_exception_summary(env)
            .unwrap_or_else(|| "no pending Java exception summary".to_string());
        bail!("call {class}.{method}{signature} failed: {err:#}; {summary}");
    }
    Ok(())
}

fn call_static_float(
    env: &mut jni::JNIEnv<'_>,
    class: &str,
    method: &str,
    signature: &str,
    args: &[JValue<'_, '_>],
) -> Result<f32> {
    let value = match env.call_static_method(class, method, signature, args) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("call {class}.{method}{signature} failed: {err:#}; {summary}");
        }
    };
    value.f().context("decode jfloat result")
}

fn call_method_float<'local>(
    env: &mut jni::JNIEnv<'local>,
    object: &JObject<'local>,
    method: &str,
    signature: &str,
    args: &[JValue<'local, 'local>],
) -> Result<f32> {
    let value = match env.call_method(object, method, signature, args) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("call object.{method}{signature} failed: {err:#}; {summary}");
        }
    };
    value.f().context("decode jfloat result")
}

fn call_static_long(
    env: &mut jni::JNIEnv<'_>,
    class: &str,
    method: &str,
    signature: &str,
    args: &[JValue<'_, '_>],
) -> Result<i64> {
    let value = match env.call_static_method(class, method, signature, args) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("call {class}.{method}{signature} failed: {err:#}; {summary}");
        }
    };
    value.j().context("decode jlong result")
}

fn call_static_string(
    env: &mut jni::JNIEnv<'_>,
    class: &str,
    method: &str,
    signature: &str,
    args: &[JValue<'_, '_>],
) -> Result<String> {
    let value = match env.call_static_method(class, method, signature, args) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("call {class}.{method}{signature} failed: {err:#}; {summary}");
        }
    };
    let object = value.l().context("decode jobject result")?;
    let jstring = JString::from(object);
    let text: String = env
        .get_string(&jstring)
        .context("convert Java string")?
        .into();
    Ok(text)
}

fn call_static_object<'local>(
    env: &mut jni::JNIEnv<'local>,
    class: &str,
    method: &str,
    signature: &str,
    args: &[JValue<'_, '_>],
) -> Result<JObject<'local>> {
    let value = match env.call_static_method(class, method, signature, args) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("call {class}.{method}{signature} failed: {err:#}; {summary}");
        }
    };
    value.l().context("decode jobject result")
}

fn object_to_string(env: &mut jni::JNIEnv<'_>, object: JObject<'_>) -> Result<String> {
    let jstring = JString::from(object);
    let text: String = env
        .get_string(&jstring)
        .context("convert Java string")?
        .into();
    Ok(text)
}

fn take_java_exception_summary(env: &mut jni::JNIEnv<'_>) -> Option<String> {
    let exception = env.exception_occurred().ok()?;
    if exception.is_null() {
        return None;
    }
    let _ = env.exception_clear();
    env.call_method(&exception, "toString", "()Ljava/lang/String;", &[])
        .ok()
        .and_then(|value| value.l().ok())
        .and_then(|object| object_to_string(env, object).ok())
        .or_else(|| Some("Java exception was thrown".to_string()))
}

fn set_vr_work_mode(env: &mut jni::JNIEnv<'_>, mode: i32) -> Result<bool> {
    let pi_hal_utils = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&JObject::from(
            env.new_string("android.os.PiHalUtils")
                .context("create PiHalUtils class name string")?,
        ))],
    )
    .context("load PiHalUtils class")?;
    let int_class = env
        .get_static_field("java/lang/Integer", "TYPE", "Ljava/lang/Class;")
        .context("get Integer.TYPE")?
        .l()
        .context("decode Integer.TYPE")?;
    let param_types = env
        .new_object_array(1, "java/lang/Class", JObject::null())
        .context("create PiHalUtils method parameter array")?;
    env.set_object_array_element(&param_types, 0, int_class)
        .context("set PiHalUtils parameter type")?;
    let method = env
        .call_method(
            &pi_hal_utils,
            "getDeclaredMethod",
            "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
            &[
                JValue::Object(&JObject::from(
                    env.new_string("setVrWorkMode")
                        .context("create setVrWorkMode method name string")?,
                )),
                JValue::Object(&JObject::from(param_types)),
            ],
        )
        .context("resolve PiHalUtils.setVrWorkMode")?
        .l()
        .context("decode PiHalUtils.setVrWorkMode method")?;
    let arg = call_static_object(
        env,
        "java/lang/Integer",
        "valueOf",
        "(I)Ljava/lang/Integer;",
        &[JValue::Int(mode)],
    )
    .context("box VR work mode argument")?;
    let args = env
        .new_object_array(1, "java/lang/Object", JObject::null())
        .context("create PiHalUtils invoke args")?;
    env.set_object_array_element(&args, 0, arg)
        .context("store PiHalUtils invoke arg")?;
    let result = env
        .call_method(
            &method,
            "invoke",
            "(Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object;",
            &[
                JValue::Object(&JObject::null()),
                JValue::Object(&JObject::from(args)),
            ],
        )
        .context("invoke PiHalUtils.setVrWorkMode")?
        .l()
        .context("decode PiHalUtils invocation result")?;
    if result.is_null() {
        bail!("PiHalUtils.setVrWorkMode returned null");
    }
    let enabled = env
        .call_method(&result, "booleanValue", "()Z", &[])
        .context("decode PiHalUtils.setVrWorkMode result")?
        .z()
        .context("convert PiHalUtils.setVrWorkMode result")?;
    Ok(enabled)
}

fn configure_activity_window() {
    use ndk::hardware_buffer_format::HardwareBufferFormat;
    use ndk::native_activity::WindowFlags;

    ndk_glue::native_activity().set_window_format(HardwareBufferFormat::R8G8B8A8_UNORM);
    ndk_glue::native_activity().set_window_flags(
        WindowFlags::KEEP_SCREEN_ON
            | WindowFlags::TURN_SCREEN_ON
            | WindowFlags::SHOW_WHEN_LOCKED
            | WindowFlags::FULLSCREEN,
        WindowFlags::empty(),
    );
}

fn get_system_service<'local>(
    env: &mut jni::JNIEnv<'local>,
    context: &JObject<'local>,
    service_name: &str,
) -> Result<JObject<'local>> {
    let service_name_text = service_name.to_string();
    let service_name = env
        .new_string(service_name)
        .with_context(|| format!("create {service_name_text} service name string"))?;
    env.call_method(
        context,
        "getSystemService",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        &[JValue::Object(&JObject::from(service_name))],
    )
    .with_context(|| format!("getSystemService({service_name_text})"))?
    .l()
    .context("decode system service object")
}

fn get_application_context<'local>(
    env: &mut jni::JNIEnv<'local>,
    context: &JObject<'local>,
) -> Result<JObject<'local>> {
    let application_context = env
        .call_method(
            context,
            "getApplicationContext",
            "()Landroid/content/Context;",
            &[],
        )
        .context("call Context.getApplicationContext")?
        .l()
        .context("decode application context object")?;
    if application_context.is_null() {
        bail!("Context.getApplicationContext returned null");
    }
    Ok(application_context)
}

fn get_power_manager_constant(env: &mut jni::JNIEnv<'_>, field_name: &str) -> Result<i32> {
    env.get_static_field("android/os/PowerManager", field_name, "I")
        .with_context(|| format!("get PowerManager.{field_name}"))?
        .i()
        .with_context(|| format!("decode PowerManager.{field_name}"))
}

fn create_display_wake_lock<'local>(
    env: &mut jni::JNIEnv<'local>,
    context: &JObject<'local>,
) -> Result<GlobalRef> {
    let power_manager =
        get_system_service(env, context, "power").context("get PowerManager system service")?;
    let full_wake_lock = get_power_manager_constant(env, "FULL_WAKE_LOCK")
        .or_else(|_| get_power_manager_constant(env, "SCREEN_BRIGHT_WAKE_LOCK"))
        .context("resolve display wake-lock level")?;
    let acquire_causes_wakeup =
        get_power_manager_constant(env, "ACQUIRE_CAUSES_WAKEUP").unwrap_or(0x1000_0000);
    let on_after_release =
        get_power_manager_constant(env, "ON_AFTER_RELEASE").unwrap_or(0x2000_0000);
    let wake_lock_flags = full_wake_lock | acquire_causes_wakeup | on_after_release;
    let wake_lock_tag = env
        .new_string("PimaxALVR:DisplayWakeLock")
        .context("create display wake-lock tag")?;
    let wake_lock = env
        .call_method(
            &power_manager,
            "newWakeLock",
            "(ILjava/lang/String;)Landroid/os/PowerManager$WakeLock;",
            &[
                JValue::Int(wake_lock_flags),
                JValue::Object(&JObject::from(wake_lock_tag)),
            ],
        )
        .context("create display wake lock")?
        .l()
        .context("decode display wake lock")?;
    env.call_method(
        &wake_lock,
        "setReferenceCounted",
        "(Z)V",
        &[JValue::Bool(0)],
    )
    .context("configure display wake lock as non-reference-counted")?;
    env.new_global_ref(wake_lock)
        .context("promote display wake lock to global ref")
}

fn acquire_display_wake_lock(env: &mut jni::JNIEnv<'_>, wake_lock: &GlobalRef) -> Result<()> {
    env.call_method(wake_lock.as_obj(), "acquire", "()V", &[])
        .context("acquire display wake lock")?;
    Ok(())
}

fn release_display_wake_lock(env: &mut jni::JNIEnv<'_>, wake_lock: &GlobalRef) -> Result<()> {
    let held = env
        .call_method(wake_lock.as_obj(), "isHeld", "()Z", &[])
        .context("query display wake lock held state")?
        .z()
        .context("decode display wake lock held state")?;
    if held {
        env.call_method(wake_lock.as_obj(), "release", "()V", &[])
            .context("release display wake lock")?;
    }
    Ok(())
}

fn is_power_interactive<'local>(
    env: &mut jni::JNIEnv<'local>,
    context: &JObject<'local>,
) -> Result<bool> {
    let power_manager = get_system_service(env, context, "power")
        .context("get PowerManager for interactive query")?;
    match env.call_method(&power_manager, "isInteractive", "()Z", &[]) {
        Ok(value) => value.z().context("decode PowerManager.isInteractive"),
        Err(_) => env
            .call_method(&power_manager, "isScreenOn", "()Z", &[])
            .context("call deprecated PowerManager.isScreenOn fallback")?
            .z()
            .context("decode PowerManager.isScreenOn fallback"),
    }
}

fn create_pvr_service_client(
    env: &mut jni::JNIEnv<'_>,
    context: &JObject<'_>,
) -> Result<GlobalRef> {
    let client = env
        .new_object(
            "com/pimax/pxrapi/PvrServiceClient",
            "(Landroid/content/Context;J)V",
            &[JValue::Object(context), JValue::Long(0)],
        )
        .context("create PvrServiceClient")?;
    env.new_global_ref(client)
        .context("promote PvrServiceClient to global ref")
}

fn connect_pvr_service_client(env: &mut jni::JNIEnv<'_>, client: &GlobalRef) -> Result<()> {
    if let Err(err) = env.call_method(client.as_obj(), "Connect", "()V", &[]) {
        let summary = take_java_exception_summary(env)
            .unwrap_or_else(|| "no pending Java exception summary".to_string());
        bail!("call PvrServiceClient.Connect failed: {err:#}; {summary}");
    }
    Ok(())
}

fn disconnect_pvr_service_client(env: &mut jni::JNIEnv<'_>, client: &GlobalRef) -> Result<()> {
    if let Err(err) = env.call_method(client.as_obj(), "Disconnect", "()V", &[]) {
        let summary = take_java_exception_summary(env)
            .unwrap_or_else(|| "no pending Java exception summary".to_string());
        bail!("call PvrServiceClient.Disconnect failed: {err:#}; {summary}");
    }
    Ok(())
}

fn wait_for_pvr_service_interface(
    env: &mut jni::JNIEnv<'_>,
    client: &GlobalRef,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let interface = env
            .call_method(
                client.as_obj(),
                "GetInterface",
                "()Lcom/pimax/vrservice/IPvrServiceInterface;",
                &[],
            )
            .context("call PvrServiceClient.GetInterface")?
            .l()
            .context("decode PvrServiceClient.GetInterface")?;
        if !interface.is_null() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("PvrServiceClient did not connect within {:?}", timeout);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn pvr_service_call_int(
    env: &mut jni::JNIEnv<'_>,
    client: &GlobalRef,
    method: &str,
    signature: &str,
    args: &[JValue<'_, '_>],
) -> Result<i32> {
    let value = match env.call_method(client.as_obj(), method, signature, args) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("call PvrServiceClient.{method}{signature} failed: {err:#}; {summary}");
        }
    };
    value.i().context("decode PvrServiceClient jint result")
}

fn pvr_service_call_string(
    env: &mut jni::JNIEnv<'_>,
    client: &GlobalRef,
    method: &str,
    ui_type: &str,
) -> Result<()> {
    let ui_type = env
        .new_string(ui_type)
        .context("create PvrServiceClient string arg")?;
    let ui_type = JObject::from(ui_type);
    if let Err(err) = env.call_method(
        client.as_obj(),
        method,
        "(Ljava/lang/String;)V",
        &[JValue::Object(&ui_type)],
    ) {
        let summary = take_java_exception_summary(env)
            .unwrap_or_else(|| "no pending Java exception summary".to_string());
        bail!("call PvrServiceClient.{method}(String) failed: {err:#}; {summary}");
    }
    Ok(())
}

fn pvr_service_hide_system_ui(
    env: &mut jni::JNIEnv<'_>,
    client: &GlobalRef,
    ui_type: &str,
) -> Result<()> {
    pvr_service_call_string(env, client, "HideSystemUI", ui_type)
}

fn pvr_service_resume_vr_mode(env: &mut jni::JNIEnv<'_>, client: &GlobalRef) -> Result<i32> {
    pvr_service_call_int(env, client, "ResumeVRMode", "()I", &[])
}

fn pvr_service_stop_vr_mode(env: &mut jni::JNIEnv<'_>, client: &GlobalRef) -> Result<i32> {
    pvr_service_call_int(env, client, "StopVRMode", "()I", &[])
}

fn pvr_service_start_vr_mode(env: &mut jni::JNIEnv<'_>, client: &GlobalRef) -> Result<()> {
    if let Err(err) = env.call_method(client.as_obj(), "StartVRMode", "()V", &[]) {
        let summary = take_java_exception_summary(env)
            .unwrap_or_else(|| "no pending Java exception summary".to_string());
        bail!("call PvrServiceClient.StartVRMode failed: {err:#}; {summary}");
    }
    Ok(())
}

fn pvr_service_set_display_interrupt_capture(
    env: &mut jni::JNIEnv<'_>,
    client: &GlobalRef,
    capture_id: i32,
    mode: i32,
) -> Result<i32> {
    pvr_service_call_int(
        env,
        client,
        "SetDisplayInterruptCapture",
        "(II)I",
        &[JValue::Int(capture_id), JValue::Int(mode)],
    )
}

fn set_int_field(
    env: &mut jni::JNIEnv<'_>,
    obj: &JObject<'_>,
    name: &str,
    value: i32,
) -> Result<()> {
    env.set_field(obj, name, "I", JValue::Int(value))
        .with_context(|| format!("set field {name}"))
}

fn set_float_field(
    env: &mut jni::JNIEnv<'_>,
    obj: &JObject<'_>,
    name: &str,
    value: f32,
) -> Result<()> {
    env.set_field(obj, name, "F", JValue::Float(value))
        .with_context(|| format!("set field {name}"))
}

fn get_int_field(env: &mut jni::JNIEnv<'_>, obj: &JObject<'_>, name: &str) -> Result<i32> {
    env.get_field(obj, name, "I")
        .with_context(|| format!("get field {name}"))?
        .i()
        .context("decode jint field")
}

fn get_float_field(env: &mut jni::JNIEnv<'_>, obj: &JObject<'_>, name: &str) -> Result<f32> {
    env.get_field(obj, name, "F")
        .with_context(|| format!("get field {name}"))?
        .f()
        .context("decode jfloat field")
}

fn get_long_field(env: &mut jni::JNIEnv<'_>, obj: &JObject<'_>, name: &str) -> Result<i64> {
    env.get_field(obj, name, "J")
        .with_context(|| format!("get field {name}"))?
        .j()
        .context("decode jlong field")
}

fn get_object_field<'local>(
    env: &mut jni::JNIEnv<'local>,
    obj: &JObject<'local>,
    name: &str,
    signature: &str,
) -> Result<JObject<'local>> {
    env.get_field(obj, name, signature)
        .with_context(|| format!("get field {name}"))?
        .l()
        .context("decode jobject field")
}

#[derive(Clone, Copy, Debug)]
struct PimaxHeadTrackingPose {
    orientation: glam::Quat,
    position: glam::Vec3,
    pose_timestamp: Duration,
    expected_display_timestamp: Duration,
    fetch_timestamp: Duration,
    status: i32,
}

fn read_pimax_head_tracking_pose<'local>(
    env: &mut jni::JNIEnv<'local>,
    pose_state: &JObject<'local>,
) -> Result<PimaxHeadTrackingPose> {
    let expected_display_time_ns =
        get_long_field(env, pose_state, "expectedDisplayTimeNs").unwrap_or_default();
    let pose_fetch_time_ns = get_long_field(env, pose_state, "poseFetchTimeNs").unwrap_or_default();
    let pose_timestamp_ns = get_long_field(env, pose_state, "poseTimeStampNs").unwrap_or_default();
    let status = get_int_field(env, pose_state, "poseStatus").unwrap_or_default();

    let pose = get_object_field(
        env,
        pose_state,
        "pose",
        "Lcom/pimax/pxrapi/PxrApi$sxrHeadPose;",
    )
    .context("get Pimax head pose object")?;
    let pose = env.auto_local(pose);

    let position = get_object_field(
        env,
        &pose,
        "position",
        "Lcom/pimax/pxrapi/PxrApi$sxrVector3;",
    )
    .context("get Pimax head position object")?;
    let position = env.auto_local(position);

    let rotation = get_object_field(
        env,
        &pose,
        "rotation",
        "Lcom/pimax/pxrapi/PxrApi$sxrQuaternion;",
    )
    .context("get Pimax head rotation object")?;
    let rotation = env.auto_local(rotation);

    let raw_position = glam::Vec3::new(
        get_float_field(env, &position, "x").context("get Pimax head position.x")?,
        get_float_field(env, &position, "y").context("get Pimax head position.y")?,
        get_float_field(env, &position, "z").context("get Pimax head position.z")?,
    );
    let raw_orientation = glam::Quat::from_xyzw(
        get_float_field(env, &rotation, "x").context("get Pimax head rotation.x")?,
        get_float_field(env, &rotation, "y").context("get Pimax head rotation.y")?,
        get_float_field(env, &rotation, "z").context("get Pimax head rotation.z")?,
        get_float_field(env, &rotation, "w").context("get Pimax head rotation.w")?,
    );

    // Pimax reports head height with the opposite vertical sign from ALVR.
    // Without this, SteamVR places the user's eyes below the floor.
    let position = glam::vec3(raw_position.x, -raw_position.y, raw_position.z);
    // Conjugate the Pimax quaternion so ALVR receives the correct rotation
    // direction. Without this, yaw and pitch are inverted (looking left
    // turns the camera right, looking down turns it up).
    let orientation = raw_orientation.conjugate();

    if !position.x.is_finite() || !position.y.is_finite() || !position.z.is_finite() {
        bail!("invalid Pimax head position: {position:?}");
    }

    let orientation_len_sq = orientation.length_squared();
    if !orientation_len_sq.is_finite() || orientation_len_sq < 1.0e-6 {
        bail!("invalid Pimax head orientation: {orientation:?}");
    }

    Ok(PimaxHeadTrackingPose {
        orientation: orientation.normalize(),
        position,
        pose_timestamp: Duration::from_nanos(pose_timestamp_ns.max(0) as u64),
        expected_display_timestamp: Duration::from_nanos(expected_display_time_ns.max(0) as u64),
        fetch_timestamp: Duration::from_nanos(pose_fetch_time_ns.max(0) as u64),
        status,
    })
}

fn dump_class_schema(env: &mut jni::JNIEnv<'_>, class_name: &str) -> Result<()> {
    let class_name_text = class_name.to_string();
    let class_name = env
        .new_string(class_name)
        .context("create class name string")?;
    let class_name_obj = JObject::from(class_name);
    let class = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&class_name_obj)],
    )
    .with_context(|| format!("load class schema for {class_name_text}"))?;

    let fields = env
        .call_method(
            &class,
            "getDeclaredFields",
            "()[Ljava/lang/reflect/Field;",
            &[],
        )
        .context("get declared fields")?
        .l()
        .context("decode declared fields")?;
    let fields = JObjectArray::from(fields);
    let count = env
        .get_array_length(&fields)
        .context("count declared fields")?;

    info!("schema for {class_name_text}: {count} declared fields");
    for index in 0..count {
        let field = env
            .get_object_array_element(&fields, index)
            .with_context(|| format!("get declared field {index}"))?;
        let field_name: String = env
            .call_method(&field, "getName", "()Ljava/lang/String;", &[])
            .context("get field name")?
            .l()
            .context("decode field name")
            .and_then(|object| object_to_string(env, object).context("field name string"))?;
        let type_obj = env
            .call_method(&field, "getType", "()Ljava/lang/Class;", &[])
            .context("get field type")?
            .l()
            .context("decode field type")?;
        let type_name: String = env
            .call_method(&type_obj, "getName", "()Ljava/lang/String;", &[])
            .context("get type name")?
            .l()
            .context("decode type name")
            .and_then(|object| object_to_string(env, object).context("type name string"))?;
        let modifiers = env
            .call_method(&field, "getModifiers", "()I", &[])
            .context("get field modifiers")?
            .i()
            .context("decode modifiers")?;

        info!(
            "schema field {class_name_text}.{field_name}: type={type_name}, modifiers={modifiers}"
        );
    }

    Ok(())
}

fn java_value_to_string(env: &mut jni::JNIEnv<'_>, value: &JObject<'_>) -> Result<String> {
    let text = call_static_object(
        env,
        "java/lang/String",
        "valueOf",
        "(Ljava/lang/Object;)Ljava/lang/String;",
        &[JValue::Object(value)],
    )
    .context("call String.valueOf")?;
    object_to_string(env, text).context("convert String.valueOf result")
}

fn dump_object_declared_fields(
    env: &mut jni::JNIEnv<'_>,
    object: &JObject<'_>,
    label: &str,
) -> Result<()> {
    if object.is_null() {
        info!("{label}: <null>");
        return Ok(());
    }

    let class = env
        .call_method(object, "getClass", "()Ljava/lang/Class;", &[])
        .context("get object class")?
        .l()
        .context("decode object class")?;
    let class_name: String = env
        .call_method(&class, "getName", "()Ljava/lang/String;", &[])
        .context("get object class name")?
        .l()
        .context("decode object class name")
        .and_then(|object| object_to_string(env, object).context("object class name string"))?;
    let object_text = java_value_to_string(env, object).unwrap_or_else(|err| format!("{err:#}"));
    info!("{label}: class={class_name} value={object_text}");

    let fields = env
        .call_method(
            &class,
            "getDeclaredFields",
            "()[Ljava/lang/reflect/Field;",
            &[],
        )
        .context("get object declared fields")?
        .l()
        .context("decode object declared fields")?;
    let fields = JObjectArray::from(fields);
    let count = env
        .get_array_length(&fields)
        .context("count object declared fields")?;
    info!("{label}: {count} declared fields");

    for index in 0..count {
        let field = env
            .get_object_array_element(&fields, index)
            .with_context(|| format!("get object declared field {index}"))?;
        let _ = env.call_method(&field, "setAccessible", "(Z)V", &[JValue::Bool(1)]);
        let field_name: String = env
            .call_method(&field, "getName", "()Ljava/lang/String;", &[])
            .context("get object field name")?
            .l()
            .context("decode object field name")
            .and_then(|object| object_to_string(env, object).context("object field name string"))?;
        let type_obj = env
            .call_method(&field, "getType", "()Ljava/lang/Class;", &[])
            .context("get object field type")?
            .l()
            .context("decode object field type")?;
        let type_name: String = env
            .call_method(&type_obj, "getName", "()Ljava/lang/String;", &[])
            .context("get object field type name")?
            .l()
            .context("decode object field type name")
            .and_then(|object| {
                object_to_string(env, object).context("object field type name string")
            })?;
        let value = match env.call_method(
            &field,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[JValue::Object(object)],
        ) {
            Ok(value) => value.l().context("decode reflected field value")?,
            Err(err) => {
                let summary = take_java_exception_summary(env)
                    .unwrap_or_else(|| "no pending Java exception summary".to_string());
                warn!("{label}.{field_name}: unable to read field: {err:#}; {summary}");
                continue;
            }
        };
        let value_text = java_value_to_string(env, &value).unwrap_or_else(|err| format!("{err:#}"));
        info!("{label}.{field_name}: type={type_name} value={value_text}");
    }

    Ok(())
}

fn get_declared_int_field_by_name(
    env: &mut jni::JNIEnv<'_>,
    object: &JObject<'_>,
    field_name: &str,
) -> Result<i32> {
    let class = env
        .call_method(object, "getClass", "()Ljava/lang/Class;", &[])
        .context("get object class")?
        .l()
        .context("decode object class")?;
    let field_name_obj = JObject::from(
        env.new_string(field_name)
            .context("create reflected field name")?,
    );
    let field = match env.call_method(
        &class,
        "getDeclaredField",
        "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
        &[JValue::Object(&field_name_obj)],
    ) {
        Ok(field) => field.l().context("decode reflected field")?,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("getDeclaredField({field_name}) failed: {err:#}; {summary}");
        }
    };
    let _ = env.call_method(&field, "setAccessible", "(Z)V", &[JValue::Bool(1)]);
    let value = match env.call_method(
        &field,
        "getInt",
        "(Ljava/lang/Object;)I",
        &[JValue::Object(object)],
    ) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("Field.getInt({field_name}) failed: {err:#}; {summary}");
        }
    };
    value.i().context("decode reflected int field")
}

fn infer_controller_start_handle(
    env: &mut jni::JNIEnv<'_>,
    start_info: &JObject<'_>,
) -> Option<i32> {
    for name in [
        "handle",
        "mHandle",
        "m_handle",
        "controllerHandle",
        "mControllerHandle",
        "controller_handle",
        "fd",
        "mFd",
    ] {
        if let Ok(handle) = get_declared_int_field_by_name(env, start_info, name) {
            info!("ControllerStartInfo inferred handle from {name}={handle}");
            return Some(handle);
        }
    }
    warn!("ControllerStartInfo handle field was not inferred from known names");
    None
}

fn pvr_service_controller_start<'local>(
    env: &mut jni::JNIEnv<'local>,
    client: &GlobalRef,
    service: &str,
) -> Result<JObject<'local>> {
    let service = JObject::from(
        env.new_string(service)
            .context("create ControllerStart service string")?,
    );
    let value = match env.call_method(
        client.as_obj(),
        "ControllerStart",
        "(Ljava/lang/String;)Lcom/pimax/pxrapi/controller/ControllerStartInfo;",
        &[JValue::Object(&service)],
    ) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!("call PvrServiceClient.ControllerStart failed: {err:#}; {summary}");
        }
    };
    value.l().context("decode ControllerStartInfo")
}

fn pvr_service_controller_stop(
    env: &mut jni::JNIEnv<'_>,
    client: &GlobalRef,
    handle: i32,
) -> Result<()> {
    if let Err(err) = env.call_method(
        client.as_obj(),
        "ControllerStop",
        "(I)V",
        &[JValue::Int(handle)],
    ) {
        let summary = take_java_exception_summary(env)
            .unwrap_or_else(|| "no pending Java exception summary".to_string());
        bail!("call PvrServiceClient.ControllerStop({handle}) failed: {err:#}; {summary}");
    }
    Ok(())
}

fn pvr_service_controller_query_int(
    env: &mut jni::JNIEnv<'_>,
    client: &GlobalRef,
    handle: i32,
    query_type: i32,
) -> Result<i32> {
    let value = match env.call_method(
        client.as_obj(),
        "ControllerQueryInt",
        "(II)I",
        &[JValue::Int(handle), JValue::Int(query_type)],
    ) {
        Ok(value) => value,
        Err(err) => {
            let summary = take_java_exception_summary(env)
                .unwrap_or_else(|| "no pending Java exception summary".to_string());
            bail!(
                "call PvrServiceClient.ControllerQueryInt({handle}, {query_type}) failed: {err:#}; {summary}"
            );
        }
    };
    value.i().context("decode ControllerQueryInt result")
}

struct PimaxControllerRuntime {
    client: GlobalRef,
    handle: i32,
    native_fd: i32,
    ring_mapping: Option<PimaxControllerRingMapping>,
    last_ring_sample: Vec<u8>,
    last_ring_change_log: Instant,
    last_poll: Instant,
    last_query_poll: Instant,
    last_buttons: u32,
    last_touches: u32,
    last_active_1d: u32,
    last_active_2d: u32,
    last_battery: i32,
    poll_count: u64,
}

struct PimaxControllerRingMapping {
    ptr: *mut u8,
    len: usize,
}

fn clamp_battery_percent(value: i32) -> u8 {
    value.clamp(0, 100) as u8
}

fn has_flag(value: u32, flag: u32) -> bool {
    (value & flag) != 0
}

fn derive_stick_axis(negative: bool, positive: bool) -> f32 {
    match (negative, positive) {
        (true, false) => -1.0,
        (false, true) => 1.0,
        _ => 0.0,
    }
}

fn map_pimax_controller_ring_buffer(
    native_fd: i32,
    fd_size: usize,
) -> Option<PimaxControllerRingMapping> {
    if native_fd < 0 || fd_size == 0 {
        warn!(
            "Pimax controller ring buffer unavailable: native_fd={} fd_size={}",
            native_fd, fd_size
        );
        return None;
    }

    let ptr = unsafe {
        libc::mmap(
            ptr::null_mut(),
            fd_size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            native_fd,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        warn!(
            "Pimax controller ring buffer mmap failed: native_fd={} fd_size={} errno={}",
            native_fd,
            fd_size,
            std::io::Error::last_os_error()
        );
        return None;
    }

    info!(
        "Pimax controller ring buffer mapped: native_fd={} fd_size={} ptr={ptr:p}",
        native_fd, fd_size
    );
    Some(PimaxControllerRingMapping {
        ptr: ptr.cast(),
        len: fd_size,
    })
}

fn unmap_pimax_controller_ring_buffer(mapping: PimaxControllerRingMapping) {
    let result = unsafe { libc::munmap(mapping.ptr.cast::<c_void>(), mapping.len) };
    if result != 0 {
        warn!(
            "Pimax controller ring buffer munmap failed: ptr={:p} len={} errno={}",
            mapping.ptr,
            mapping.len,
            std::io::Error::last_os_error()
        );
    } else {
        info!(
            "Pimax controller ring buffer unmapped: ptr={:p} len={}",
            mapping.ptr, mapping.len
        );
    }
}

fn close_pimax_controller_fd(native_fd: i32) {
    if native_fd < 0 {
        return;
    }
    let result = unsafe { libc::close(native_fd) };
    if result != 0 {
        warn!(
            "Pimax controller native fd close failed: fd={} errno={}",
            native_fd,
            std::io::Error::last_os_error()
        );
    } else {
        info!("Pimax controller native fd closed: fd={native_fd}");
    }
}

fn checksum_bytes(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn format_hex_bytes(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len().saturating_mul(3));
    for (index, byte) in bytes.iter().enumerate() {
        if index > 0 {
            text.push(' ');
        }
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

fn sample_pimax_controller_ring_buffer(runtime: &mut PimaxControllerRuntime) {
    let Some(mapping) = runtime.ring_mapping.as_ref() else {
        return;
    };
    let sample_len = mapping.len;
    if sample_len == 0 {
        return;
    }

    let sample = unsafe { std::slice::from_raw_parts(mapping.ptr, sample_len) };
    if runtime.last_ring_sample.len() != sample_len {
        runtime.last_ring_sample.clear();
        runtime.last_ring_sample.extend_from_slice(sample);
        info!(
            "Pimax controller ring initial sample: len={} checksum=0x{:016x} head={} tail={}",
            sample_len,
            checksum_bytes(sample),
            format_hex_bytes(&sample[..sample_len.min(64)]),
            format_hex_bytes(&sample[sample_len.saturating_sub(64)..])
        );
        return;
    }

    let mut changed_words = Vec::new();
    let mut changed_word_count = 0_usize;
    for offset in (0..sample_len).step_by(4) {
        let end = (offset + 4).min(sample_len);
        if sample[offset..end] != runtime.last_ring_sample[offset..end] {
            changed_word_count += 1;
            let mut before = [0_u8; 4];
            let mut after = [0_u8; 4];
            before[..end - offset].copy_from_slice(&runtime.last_ring_sample[offset..end]);
            after[..end - offset].copy_from_slice(&sample[offset..end]);
            if changed_words.len() < PIMAX_CONTROLLER_RING_MAX_CHANGE_LOG_WORDS {
                changed_words.push((
                    offset,
                    u32::from_le_bytes(before),
                    u32::from_le_bytes(after),
                ));
            }
        }
    }

    if changed_word_count == 0 {
        return;
    }

    let should_log = runtime.last_ring_change_log.elapsed() >= Duration::from_millis(250)
        || runtime.poll_count <= 10;
    if should_log {
        let changes = changed_words
            .iter()
            .map(|(offset, before, after)| format!("0x{offset:04x}:0x{before:08x}->0x{after:08x}"))
            .collect::<Vec<_>>()
            .join(", ");
        info!(
            "Pimax controller ring changed: poll_count={} changed_words={} checksum=0x{:016x} changes=[{}]",
            runtime.poll_count,
            changed_word_count,
            checksum_bytes(sample),
            changes
        );
        runtime.last_ring_change_log = Instant::now();
    }

    runtime.last_ring_sample.copy_from_slice(sample);
}

fn push_pimax_controller_states(
    runtime: &mut PimaxControllerRuntime,
    buttons: u32,
    touches: u32,
    active_1d: u32,
    active_2d: u32,
    battery: i32,
) {
    let mut left_buttons = 0_u32;
    let mut right_buttons = 0_u32;

    if has_flag(buttons, PIMAX_BUTTON_PRIMARY_INDEX_TRIGGER) {
        left_buttons |= ALVR_BUTTON_TRIGGER;
    }
    if has_flag(buttons, PIMAX_BUTTON_PRIMARY_HAND_TRIGGER) {
        left_buttons |= ALVR_BUTTON_GRIP;
    }
    if has_flag(buttons, PIMAX_BUTTON_PRIMARY_THUMBSTICK) {
        left_buttons |= ALVR_BUTTON_THUMBSTICK_CLICK;
    }
    if has_flag(buttons, PIMAX_BUTTON_BACK) {
        left_buttons |= ALVR_BUTTON_MENU;
    }
    if has_flag(buttons, PIMAX_BUTTON_THREE) {
        left_buttons |= ALVR_BUTTON_AX;
    }
    if has_flag(buttons, PIMAX_BUTTON_FOUR) {
        left_buttons |= ALVR_BUTTON_BY;
    }

    if has_flag(buttons, PIMAX_BUTTON_SECONDARY_INDEX_TRIGGER) {
        right_buttons |= ALVR_BUTTON_TRIGGER;
    }
    if has_flag(buttons, PIMAX_BUTTON_SECONDARY_HAND_TRIGGER) {
        right_buttons |= ALVR_BUTTON_GRIP;
    }
    if has_flag(buttons, PIMAX_BUTTON_SECONDARY_THUMBSTICK) {
        right_buttons |= ALVR_BUTTON_THUMBSTICK_CLICK;
    }
    if has_flag(buttons, PIMAX_BUTTON_START) {
        right_buttons |= ALVR_BUTTON_MENU;
    }
    if has_flag(buttons, PIMAX_BUTTON_ONE) {
        right_buttons |= ALVR_BUTTON_AX;
    }
    if has_flag(buttons, PIMAX_BUTTON_TWO) {
        right_buttons |= ALVR_BUTTON_BY;
    }

    let mut left_touches = 0_u32;
    let mut right_touches = 0_u32;
    if has_flag(touches, PIMAX_TOUCH_PRIMARY_THUMBSTICK) {
        left_touches |= ALVR_BUTTON_THUMBSTICK_CLICK;
    }
    if has_flag(touches, PIMAX_TOUCH_THREE) {
        left_touches |= ALVR_BUTTON_AX;
    }
    if has_flag(touches, PIMAX_TOUCH_FOUR) {
        left_touches |= ALVR_BUTTON_BY;
    }
    if has_flag(touches, PIMAX_TOUCH_SECONDARY_THUMBSTICK) {
        right_touches |= ALVR_BUTTON_THUMBSTICK_CLICK;
    }
    if has_flag(touches, PIMAX_TOUCH_ONE) {
        right_touches |= ALVR_BUTTON_AX;
    }
    if has_flag(touches, PIMAX_TOUCH_TWO) {
        right_touches |= ALVR_BUTTON_BY;
    }

    let left_trigger_active = has_flag(active_1d, PIMAX_AXIS_1D_PRIMARY_INDEX_TRIGGER)
        || has_flag(buttons, PIMAX_BUTTON_PRIMARY_INDEX_TRIGGER);
    let right_trigger_active = has_flag(active_1d, PIMAX_AXIS_1D_SECONDARY_INDEX_TRIGGER)
        || has_flag(buttons, PIMAX_BUTTON_SECONDARY_INDEX_TRIGGER);
    let left_grip_active = has_flag(active_1d, PIMAX_AXIS_1D_PRIMARY_HAND_TRIGGER)
        || has_flag(buttons, PIMAX_BUTTON_PRIMARY_HAND_TRIGGER);
    let right_grip_active = has_flag(active_1d, PIMAX_AXIS_1D_SECONDARY_HAND_TRIGGER)
        || has_flag(buttons, PIMAX_BUTTON_SECONDARY_HAND_TRIGGER);

    let left_stick_active = has_flag(active_2d, PIMAX_AXIS_2D_PRIMARY_THUMBSTICK)
        || has_flag(buttons, PIMAX_BUTTON_PRIMARY_THUMBSTICK);
    let right_stick_active = has_flag(active_2d, PIMAX_AXIS_2D_SECONDARY_THUMBSTICK)
        || has_flag(buttons, PIMAX_BUTTON_SECONDARY_THUMBSTICK);

    let left_thumbstick_x = if left_stick_active {
        derive_stick_axis(
            has_flag(buttons, PIMAX_BUTTON_PRIMARY_THUMBSTICK_LEFT),
            has_flag(buttons, PIMAX_BUTTON_PRIMARY_THUMBSTICK_RIGHT),
        )
    } else {
        0.0
    };
    let left_thumbstick_y = if left_stick_active {
        derive_stick_axis(
            has_flag(buttons, PIMAX_BUTTON_PRIMARY_THUMBSTICK_DOWN),
            has_flag(buttons, PIMAX_BUTTON_PRIMARY_THUMBSTICK_UP),
        )
    } else {
        0.0
    };
    let right_thumbstick_x = if right_stick_active {
        derive_stick_axis(
            has_flag(buttons, PIMAX_BUTTON_SECONDARY_THUMBSTICK_LEFT),
            has_flag(buttons, PIMAX_BUTTON_SECONDARY_THUMBSTICK_RIGHT),
        )
    } else {
        0.0
    };
    let right_thumbstick_y = if right_stick_active {
        derive_stick_axis(
            has_flag(buttons, PIMAX_BUTTON_SECONDARY_THUMBSTICK_DOWN),
            has_flag(buttons, PIMAX_BUTTON_SECONDARY_THUMBSTICK_UP),
        )
    } else {
        0.0
    };

    let now = Instant::now();
    let battery = clamp_battery_percent(battery);
    crate::controller::update_controller_state(
        crate::controller::Hand::Left,
        crate::controller::SingleControllerState {
            connected: true,
            handle: runtime.handle,
            motion: None,
            buttons_pressed: left_buttons,
            buttons_touched: left_touches,
            trigger: if left_trigger_active { 1.0 } else { 0.0 },
            grip: if left_grip_active { 1.0 } else { 0.0 },
            thumbstick_x: left_thumbstick_x,
            thumbstick_y: left_thumbstick_y,
            battery_percent: battery,
            last_updated: now,
        },
    );
    crate::controller::update_controller_state(
        crate::controller::Hand::Right,
        crate::controller::SingleControllerState {
            connected: true,
            handle: runtime.handle,
            motion: None,
            buttons_pressed: right_buttons,
            buttons_touched: right_touches,
            trigger: if right_trigger_active { 1.0 } else { 0.0 },
            grip: if right_grip_active { 1.0 } else { 0.0 },
            thumbstick_x: right_thumbstick_x,
            thumbstick_y: right_thumbstick_y,
            battery_percent: battery,
            last_updated: now,
        },
    );

    runtime.poll_count = runtime.poll_count.wrapping_add(1);
    let changed = buttons != runtime.last_buttons
        || touches != runtime.last_touches
        || active_1d != runtime.last_active_1d
        || active_2d != runtime.last_active_2d
        || battery as i32 != runtime.last_battery;
    if changed || runtime.poll_count <= 5 || runtime.poll_count % 120 == 0 {
        info!(
            "Pimax controller SDK poll: count={} handle={} raw_buttons=0x{buttons:08x} raw_touches=0x{touches:08x} active_1d=0x{active_1d:08x} active_2d=0x{active_2d:08x} battery={} left_buttons=0x{left_buttons:08x} right_buttons=0x{right_buttons:08x} left_stick=({left_thumbstick_x:.1},{left_thumbstick_y:.1}) right_stick=({right_thumbstick_x:.1},{right_thumbstick_y:.1})",
            runtime.poll_count, runtime.handle, battery
        );
    }
    runtime.last_buttons = buttons;
    runtime.last_touches = touches;
    runtime.last_active_1d = active_1d;
    runtime.last_active_2d = active_2d;
    runtime.last_battery = battery as i32;
}

fn start_pimax_controller_runtime(
    env: &mut jni::JNIEnv<'_>,
    context: &JObject<'_>,
) -> Result<Option<PimaxControllerRuntime>> {
    let Some(default_service) = probe_pvr_controller_defaults(env) else {
        warn!("Pimax controller SDK runtime disabled: default service unavailable");
        return Ok(None);
    };

    let client =
        create_pvr_service_client(env, context).context("create controller runtime client")?;
    if let Err(err) = connect_pvr_service_client(env, &client) {
        warn!("Pimax controller SDK runtime Connect failed: {err:#}");
        return Ok(None);
    }
    if let Err(err) = wait_for_pvr_service_interface(env, &client, Duration::from_secs(3)) {
        warn!("Pimax controller SDK runtime service wait failed: {err:#}");
        if let Err(disconnect_err) = disconnect_pvr_service_client(env, &client) {
            warn!("Pimax controller SDK runtime disconnect after failed wait failed: {disconnect_err:#}");
        }
        return Ok(None);
    }

    let start_info = match pvr_service_controller_start(env, &client, &default_service) {
        Ok(info) => info,
        Err(err) => {
            warn!("Pimax controller SDK runtime ControllerStart failed: {err:#}");
            if let Err(disconnect_err) = disconnect_pvr_service_client(env, &client) {
                warn!(
                    "Pimax controller SDK runtime disconnect after failed start failed: {disconnect_err:#}"
                );
            }
            return Ok(None);
        }
    };
    if let Err(err) = dump_object_declared_fields(env, &start_info, "ControllerRuntimeStartInfo") {
        warn!("failed to dump ControllerRuntimeStartInfo: {err:#}");
    }
    let Some(handle) = infer_controller_start_handle(env, &start_info) else {
        if let Err(disconnect_err) = disconnect_pvr_service_client(env, &client) {
            warn!(
                "Pimax controller SDK runtime disconnect after missing handle failed: {disconnect_err:#}"
            );
        }
        bail!("ControllerStart succeeded but no controller handle was found");
    };
    let native_fd = get_declared_int_field_by_name(env, &start_info, "m_nativeFd").unwrap_or(-1);
    let fd_size =
        get_declared_int_field_by_name(env, &start_info, "m_fd_size").unwrap_or_default() as usize;
    let ring_mapping = map_pimax_controller_ring_buffer(native_fd, fd_size);
    info!(
        "Pimax controller SDK runtime started: service={default_service} handle={handle} native_fd={native_fd} fd_size={fd_size}"
    );

    Ok(Some(PimaxControllerRuntime {
        client,
        handle,
        native_fd,
        ring_mapping,
        last_ring_sample: Vec::new(),
        last_ring_change_log: Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now),
        last_poll: Instant::now()
            .checked_sub(PIMAX_CONTROLLER_RING_SAMPLE_INTERVAL)
            .unwrap_or_else(Instant::now),
        last_query_poll: Instant::now()
            .checked_sub(PIMAX_CONTROLLER_QUERY_INTERVAL)
            .unwrap_or_else(Instant::now),
        last_buttons: u32::MAX,
        last_touches: u32::MAX,
        last_active_1d: u32::MAX,
        last_active_2d: u32::MAX,
        last_battery: i32::MIN,
        poll_count: 0,
    }))
}

fn poll_pimax_controller_runtime(env: &mut jni::JNIEnv<'_>, runtime: &mut PimaxControllerRuntime) {
    if runtime.last_poll.elapsed() >= PIMAX_CONTROLLER_RING_SAMPLE_INTERVAL {
        runtime.last_poll = Instant::now();
        sample_pimax_controller_ring_buffer(runtime);
    }

    if runtime.last_query_poll.elapsed() < PIMAX_CONTROLLER_QUERY_INTERVAL {
        return;
    }
    runtime.last_query_poll = Instant::now();

    let query = |env: &mut jni::JNIEnv<'_>, query_type| {
        pvr_service_controller_query_int(env, &runtime.client, runtime.handle, query_type)
    };
    let battery = match query(env, PIMAX_CONTROLLER_QUERY_BATTERY) {
        Ok(value) => value,
        Err(err) => {
            warn!("Pimax controller SDK battery query failed: {err:#}");
            return;
        }
    };
    let buttons = match query(env, PIMAX_CONTROLLER_QUERY_ACTIVE_BUTTONS) {
        Ok(value) => value as u32,
        Err(err) => {
            warn!("Pimax controller SDK active-buttons query failed: {err:#}");
            return;
        }
    };
    let active_2d = match query(env, PIMAX_CONTROLLER_QUERY_ACTIVE_2D_ANALOGS) {
        Ok(value) => value as u32,
        Err(err) => {
            warn!("Pimax controller SDK active-2d query failed: {err:#}");
            return;
        }
    };
    let active_1d = match query(env, PIMAX_CONTROLLER_QUERY_ACTIVE_1D_ANALOGS) {
        Ok(value) => value as u32,
        Err(err) => {
            warn!("Pimax controller SDK active-1d query failed: {err:#}");
            return;
        }
    };
    let touches = match query(env, PIMAX_CONTROLLER_QUERY_ACTIVE_TOUCH_BUTTONS) {
        Ok(value) => value as u32,
        Err(err) => {
            warn!("Pimax controller SDK active-touch query failed: {err:#}");
            return;
        }
    };

    push_pimax_controller_states(runtime, buttons, touches, active_1d, active_2d, battery);
}

fn stop_pimax_controller_runtime(env: &mut jni::JNIEnv<'_>, mut runtime: PimaxControllerRuntime) {
    if let Err(err) = pvr_service_controller_stop(env, &runtime.client, runtime.handle) {
        warn!(
            "Pimax controller SDK runtime ControllerStop({}) failed: {err:#}",
            runtime.handle
        );
    } else {
        info!(
            "Pimax controller SDK runtime stopped handle={}",
            runtime.handle
        );
    }
    if let Err(err) = disconnect_pvr_service_client(env, &runtime.client) {
        warn!("Pimax controller SDK runtime Disconnect failed: {err:#}");
    }
    if let Some(mapping) = runtime.ring_mapping.take() {
        unmap_pimax_controller_ring_buffer(mapping);
    }
    close_pimax_controller_fd(runtime.native_fd);
    crate::controller::update_controller_connection(crate::controller::Hand::Left, false);
    crate::controller::update_controller_connection(crate::controller::Hand::Right, false);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PimaxNativeControllerState {
    bytes: [u8; PIMAX_NATIVE_CONTROLLER_STATE_SIZE],
}

impl Default for PimaxNativeControllerState {
    fn default() -> Self {
        Self {
            bytes: [0; PIMAX_NATIVE_CONTROLLER_STATE_SIZE],
        }
    }
}

type SxrControllerStartTrackingFn = unsafe extern "C" fn(*const libc::c_char) -> i32;
type SxrControllerStopTrackingFn = unsafe extern "C" fn(i32);
type SxrControllerGetStateFn = unsafe extern "C" fn(i32, i32) -> PimaxNativeControllerState;

struct PimaxNativeControllerApi {
    dl_handle: *mut c_void,
    start_tracking: SxrControllerStartTrackingFn,
    stop_tracking: SxrControllerStopTrackingFn,
    get_state: SxrControllerGetStateFn,
}

struct PimaxNativeControllerRuntime {
    api: PimaxNativeControllerApi,
    controllers: Vec<PimaxNativeControllerHandle>,
    last_poll: Instant,
    last_change_log: Instant,
    poll_count: u64,
}

struct PimaxNativeControllerHandle {
    hand: crate::controller::Hand,
    descriptor: CString,
    handle: i32,
    last_state: PimaxNativeControllerState,
    last_parsed: PimaxNativeControllerParsed,
    last_battery: u8,
    last_battery_poll: Instant,
}

#[derive(Clone, Copy, Debug)]
struct PimaxNativeControllerParsed {
    orientation: glam::Quat,
    position: glam::Vec3,
    linear_velocity: glam::Vec3,
    angular_velocity: glam::Vec3,
    timestamp_ns: u64,
    buttons: u32,
    touches: u32,
    thumbstick_x: f32,
    thumbstick_y: f32,
    trigger: f32,
    grip: f32,
}

impl Default for PimaxNativeControllerParsed {
    fn default() -> Self {
        Self {
            orientation: glam::Quat::IDENTITY,
            position: glam::Vec3::ZERO,
            linear_velocity: glam::Vec3::ZERO,
            angular_velocity: glam::Vec3::ZERO,
            timestamp_ns: 0,
            buttons: 0,
            touches: 0,
            thumbstick_x: 0.0,
            thumbstick_y: 0.0,
            trigger: 0.0,
            grip: 0.0,
        }
    }
}

fn dlerror_string() -> String {
    let err = unsafe { libc::dlerror() };
    if err.is_null() {
        "unknown dlerror".to_string()
    } else {
        unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned()
    }
}

unsafe fn load_pimax_native_symbol<T: Copy>(dl_handle: *mut c_void, name: &str) -> Result<T> {
    let symbol_name = CString::new(name).with_context(|| format!("create symbol name {name}"))?;
    libc::dlerror();
    let raw = libc::dlsym(dl_handle, symbol_name.as_ptr());
    if raw.is_null() {
        bail!("dlsym({name}) failed: {}", dlerror_string());
    }
    Ok(mem::transmute_copy::<*mut c_void, T>(&raw))
}

fn open_pimax_native_controller_api() -> Result<Option<PimaxNativeControllerApi>> {
    let library = CString::new("libpxrapi.so").context("create libpxrapi.so name")?;
    unsafe {
        libc::dlerror();
    }
    let dl_handle = unsafe { libc::dlopen(library.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if dl_handle.is_null() {
        warn!(
            "native Pimax controller runtime disabled: dlopen(libpxrapi.so) failed: {}",
            dlerror_string()
        );
        return Ok(None);
    }

    let load_result = unsafe {
        let start_tracking = load_pimax_native_symbol::<SxrControllerStartTrackingFn>(
            dl_handle,
            "sxrControllerStartTracking",
        )?;
        let stop_tracking = load_pimax_native_symbol::<SxrControllerStopTrackingFn>(
            dl_handle,
            "sxrControllerStopTracking",
        )?;
        let get_state = load_pimax_native_symbol::<SxrControllerGetStateFn>(
            dl_handle,
            "sxrControllerGetState",
        )?;
        Ok::<_, anyhow::Error>(PimaxNativeControllerApi {
            dl_handle,
            start_tracking,
            stop_tracking,
            get_state,
        })
    };

    match load_result {
        Ok(api) => Ok(Some(api)),
        Err(err) => {
            unsafe {
                libc::dlclose(dl_handle);
            }
            Err(err).context("load native Pimax controller symbols")
        }
    }
}

fn close_pimax_native_controller_api(api: PimaxNativeControllerApi) {
    let result = unsafe { libc::dlclose(api.dl_handle) };
    if result != 0 {
        warn!("dlclose(libpxrapi.so) failed: {}", dlerror_string());
    }
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    let Some(window) = bytes.get(offset..offset + 4) else {
        return 0;
    };
    u32::from_le_bytes([window[0], window[1], window[2], window[3]])
}

fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
    let Some(window) = bytes.get(offset..offset + 8) else {
        return 0;
    };
    u64::from_le_bytes([
        window[0], window[1], window[2], window[3], window[4], window[5], window[6], window[7],
    ])
}

fn read_f32_at(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(read_u32_at(bytes, offset))
}

fn read_vec3_at(bytes: &[u8], offset: usize) -> glam::Vec3 {
    let value = glam::vec3(
        read_f32_at(bytes, offset),
        read_f32_at(bytes, offset + 4),
        read_f32_at(bytes, offset + 8),
    );
    if value.is_finite() {
        value
    } else {
        glam::Vec3::ZERO
    }
}

fn sanitize_pimax_quat(raw: glam::Quat) -> glam::Quat {
    if !raw.is_finite() {
        return glam::Quat::IDENTITY;
    }
    let length_squared = raw.length_squared();
    if !(0.001..=4.0).contains(&length_squared) {
        return glam::Quat::IDENTITY;
    }
    raw.normalize()
}

fn normalize_pimax_native_axis(value: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    if value.abs() > 2.0 {
        ((value - 128.0) / 127.0).clamp(-1.0, 1.0)
    } else {
        value.clamp(-1.0, 1.0)
    }
}

fn normalize_pimax_native_trigger(value: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    if value > 1.5 {
        (value / 255.0).clamp(0.0, 1.0)
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn parse_pimax_native_controller_state(
    state: &PimaxNativeControllerState,
) -> PimaxNativeControllerParsed {
    let bytes = &state.bytes;
    let orientation = sanitize_pimax_quat(glam::Quat::from_xyzw(
        read_f32_at(bytes, 0x00),
        read_f32_at(bytes, 0x04),
        read_f32_at(bytes, 0x08),
        read_f32_at(bytes, 0x0c),
    ));
    let position = read_vec3_at(bytes, 0x10);
    let angular_velocity = read_vec3_at(bytes, 0x1c);
    let linear_velocity = read_vec3_at(bytes, 0x28);
    let timestamp_ns = read_u64_at(bytes, 0x38);
    let buttons = read_u32_at(bytes, 0x40);
    let thumbstick_x = normalize_pimax_native_axis(read_f32_at(bytes, 0x44));
    let thumbstick_y = normalize_pimax_native_axis(read_f32_at(bytes, 0x48));
    let trigger = normalize_pimax_native_trigger(read_f32_at(bytes, 0x64));
    let grip = normalize_pimax_native_trigger(read_f32_at(bytes, 0x6c));
    let touches = read_u32_at(bytes, 0x84);

    PimaxNativeControllerParsed {
        orientation,
        position,
        linear_velocity,
        angular_velocity,
        timestamp_ns,
        buttons,
        touches,
        thumbstick_x,
        thumbstick_y,
        trigger,
        grip,
    }
}

fn format_pimax_native_state_changes(
    before: &PimaxNativeControllerState,
    after: &PimaxNativeControllerState,
) -> String {
    let mut changed_words = Vec::new();
    for offset in (0..PIMAX_NATIVE_CONTROLLER_STATE_SIZE).step_by(4) {
        let old = read_u32_at(&before.bytes, offset);
        let new = read_u32_at(&after.bytes, offset);
        if old != new {
            if changed_words.len() < PIMAX_NATIVE_CONTROLLER_MAX_CHANGE_LOG_WORDS {
                changed_words.push(format!("0x{offset:02x}:0x{old:08x}->0x{new:08x}"));
            } else {
                break;
            }
        }
    }
    changed_words.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_grip_pose_offset_uses_tuned_pitch_degrees() {
        let offset =
            pimax_native_controller_grip_pose_offset_from_degrees(glam::vec3(45.0, 0.0, 0.0));
        let grip_up = offset * glam::Vec3::Y;
        let expected_up = glam::vec3(
            0.0,
            std::f32::consts::FRAC_1_SQRT_2,
            std::f32::consts::FRAC_1_SQRT_2,
        );
        assert!((grip_up - expected_up).length() < 1.0e-5);
    }
}

fn read_pimax_controller_battery(hand: crate::controller::Hand) -> Option<u8> {
    let path = match hand {
        crate::controller::Hand::Left => "/sys/class/pimax_controller/controller_left/battery",
        crate::controller::Hand::Right => "/sys/class/pimax_controller/controller_right/battery",
    };
    let text = std::fs::read_to_string(path).ok()?;
    let value = text.trim().parse::<i32>().ok()?;
    Some(clamp_battery_percent(value))
}

fn map_pimax_native_buttons(
    _hand: crate::controller::Hand,
    parsed: &PimaxNativeControllerParsed,
) -> (u32, u32) {
    let mut pressed = 0_u32;
    let mut touched = 0_u32;
    let buttons = parsed.buttons;
    let touches = parsed.touches;

    if has_flag(buttons, PIMAX_BUTTON_PRIMARY_INDEX_TRIGGER)
        || has_flag(buttons, PIMAX_BUTTON_SECONDARY_INDEX_TRIGGER)
        || parsed.trigger > 0.2
    {
        pressed |= ALVR_BUTTON_TRIGGER;
    }
    if has_flag(buttons, PIMAX_BUTTON_PRIMARY_HAND_TRIGGER)
        || has_flag(buttons, PIMAX_BUTTON_SECONDARY_HAND_TRIGGER)
        || parsed.grip > 0.2
    {
        pressed |= ALVR_BUTTON_GRIP;
    }
    if has_flag(buttons, PIMAX_BUTTON_PRIMARY_THUMBSTICK)
        || has_flag(buttons, PIMAX_BUTTON_SECONDARY_THUMBSTICK)
    {
        pressed |= ALVR_BUTTON_THUMBSTICK_CLICK;
    }
    if has_flag(buttons, PIMAX_BUTTON_BACK) || has_flag(buttons, PIMAX_BUTTON_START) {
        pressed |= ALVR_BUTTON_MENU;
    }

    // Crystal OG native sxrControllerGetState reports the face buttons as the
    // low two bits for both hands: left X/right A = bit 0, left Y/right B = bit 1.
    // Keep the older THREE/FOUR constants as a harmless fallback for the Binder
    // query layout seen in Pimax/Qualcomm docs.
    if has_flag(buttons, PIMAX_BUTTON_ONE) || has_flag(buttons, PIMAX_BUTTON_THREE) {
        pressed |= ALVR_BUTTON_AX;
    }
    if has_flag(buttons, PIMAX_BUTTON_TWO) || has_flag(buttons, PIMAX_BUTTON_FOUR) {
        pressed |= ALVR_BUTTON_BY;
    }

    if has_flag(touches, PIMAX_NATIVE_TOUCH_TRIGGER) || parsed.trigger > 0.01 {
        touched |= ALVR_BUTTON_TRIGGER;
    }
    if has_flag(touches, PIMAX_NATIVE_TOUCH_GRIP) || parsed.grip > 0.01 {
        touched |= ALVR_BUTTON_GRIP;
    }
    if has_flag(touches, PIMAX_TOUCH_PRIMARY_THUMBSTICK)
        || has_flag(touches, PIMAX_TOUCH_SECONDARY_THUMBSTICK)
    {
        touched |= ALVR_BUTTON_THUMBSTICK_CLICK;
    }
    if has_flag(touches, PIMAX_TOUCH_ONE) || has_flag(touches, PIMAX_TOUCH_THREE) {
        touched |= ALVR_BUTTON_AX;
    }
    if has_flag(touches, PIMAX_TOUCH_TWO) || has_flag(touches, PIMAX_TOUCH_FOUR) {
        touched |= ALVR_BUTTON_BY;
    }

    (pressed, touched)
}

fn pimax_native_controller_grip_pose_offset_from_degrees(rotation_deg: glam::Vec3) -> glam::Quat {
    glam::Quat::from_euler(
        glam::EulerRot::XYZ,
        rotation_deg.x.to_radians(),
        rotation_deg.y.to_radians(),
        rotation_deg.z.to_radians(),
    )
    .normalize()
}

fn pimax_native_controller_grip_pose_offset() -> glam::Quat {
    // Native sxrControllerGetState's model basis is pitched forward relative
    // to ALVR/OpenXR grip poses. A full +90 degree axis swap overcorrects on
    // Crystal OG, so keep the offset live-tunable while calibrating hardware.
    pimax_native_controller_grip_pose_offset_from_degrees(crate::tune::controller_rotation_deg())
}

fn convert_pimax_native_controller_motion(
    parsed: &PimaxNativeControllerParsed,
) -> crate::client::DeviceMotion {
    let grip_orientation =
        (parsed.orientation * pimax_native_controller_grip_pose_offset()).normalize();

    crate::client::DeviceMotion {
        pose: crate::client::Pose {
            orientation: grip_orientation,
            // The native position stream already tracks the user's motion in
            // the expected up/down direction; do not mirror Y here.
            position: parsed.position,
        },
        linear_velocity: parsed.linear_velocity,
        angular_velocity: parsed.angular_velocity,
    }
}

fn push_pimax_native_controller_state(
    controller: &PimaxNativeControllerHandle,
    parsed: PimaxNativeControllerParsed,
) {
    let (buttons_pressed, buttons_touched) = map_pimax_native_buttons(controller.hand, &parsed);
    let motion = convert_pimax_native_controller_motion(&parsed);

    crate::controller::update_controller_state(
        controller.hand,
        crate::controller::SingleControllerState {
            connected: true,
            handle: controller.handle,
            motion: Some(motion),
            buttons_pressed,
            buttons_touched,
            trigger: parsed.trigger,
            grip: parsed.grip,
            thumbstick_x: parsed.thumbstick_x,
            thumbstick_y: parsed.thumbstick_y,
            battery_percent: controller.last_battery,
            last_updated: Instant::now(),
        },
    );
}

fn start_pimax_native_controller_runtime() -> Result<Option<PimaxNativeControllerRuntime>> {
    let Some(api) = open_pimax_native_controller_api()? else {
        return Ok(None);
    };

    let mut controllers = Vec::new();
    for (hand, descriptor) in [
        (crate::controller::Hand::Left, "left"),
        (crate::controller::Hand::Right, "right"),
    ] {
        let descriptor = CString::new(descriptor).context("create native controller descriptor")?;
        let handle = unsafe { (api.start_tracking)(descriptor.as_ptr()) };
        if handle < 0 {
            warn!(
                "native Pimax controller start failed: hand={hand:?} descriptor={} handle={handle}",
                descriptor.to_string_lossy()
            );
            continue;
        }
        let battery = read_pimax_controller_battery(hand).unwrap_or(100);
        info!(
            "native Pimax controller started: hand={hand:?} descriptor={} handle={handle} battery={battery}",
            descriptor.to_string_lossy()
        );
        crate::controller::update_controller_connection(hand, true);
        controllers.push(PimaxNativeControllerHandle {
            hand,
            descriptor,
            handle,
            last_state: PimaxNativeControllerState::default(),
            last_parsed: PimaxNativeControllerParsed::default(),
            last_battery: battery,
            last_battery_poll: Instant::now()
                .checked_sub(PIMAX_NATIVE_CONTROLLER_BATTERY_INTERVAL)
                .unwrap_or_else(Instant::now),
        });
    }

    if controllers.is_empty() {
        warn!("native Pimax controller runtime disabled: no controller handles started");
        close_pimax_native_controller_api(api);
        return Ok(None);
    }

    info!(
        "native Pimax controller runtime active: handles={}",
        controllers
            .iter()
            .map(|controller| format!("{:?}:{}", controller.hand, controller.handle))
            .collect::<Vec<_>>()
            .join(", ")
    );

    Ok(Some(PimaxNativeControllerRuntime {
        api,
        controllers,
        last_poll: Instant::now()
            .checked_sub(PIMAX_NATIVE_CONTROLLER_POLL_INTERVAL)
            .unwrap_or_else(Instant::now),
        last_change_log: Instant::now()
            .checked_sub(PIMAX_NATIVE_CONTROLLER_CHANGE_LOG_INTERVAL)
            .unwrap_or_else(Instant::now),
        poll_count: 0,
    }))
}

fn poll_pimax_native_controller_runtime(runtime: &mut PimaxNativeControllerRuntime) {
    if runtime.last_poll.elapsed() < PIMAX_NATIVE_CONTROLLER_POLL_INTERVAL {
        return;
    }
    runtime.last_poll = Instant::now();
    runtime.poll_count = runtime.poll_count.wrapping_add(1);

    let can_log_change =
        runtime.last_change_log.elapsed() >= PIMAX_NATIVE_CONTROLLER_CHANGE_LOG_INTERVAL;
    let mut logged_change = false;

    for controller in &mut runtime.controllers {
        if controller.last_battery_poll.elapsed() >= PIMAX_NATIVE_CONTROLLER_BATTERY_INTERVAL {
            if let Some(battery) = read_pimax_controller_battery(controller.hand) {
                controller.last_battery = battery;
            }
            controller.last_battery_poll = Instant::now();
        }

        let state = unsafe { (runtime.api.get_state)(controller.handle, 0) };
        let parsed = parse_pimax_native_controller_state(&state);
        let state_changed = state != controller.last_state;
        let controls_changed = parsed.buttons != controller.last_parsed.buttons
            || parsed.touches != controller.last_parsed.touches
            || (parsed.trigger - controller.last_parsed.trigger).abs() > 0.01
            || (parsed.grip - controller.last_parsed.grip).abs() > 0.01
            || (parsed.thumbstick_x - controller.last_parsed.thumbstick_x).abs() > 0.01
            || (parsed.thumbstick_y - controller.last_parsed.thumbstick_y).abs() > 0.01;

        let should_log_control_change =
            controls_changed && (can_log_change || runtime.poll_count <= 5);
        let should_log_initial_state = state_changed && runtime.poll_count <= 5;

        if should_log_control_change || should_log_initial_state {
            let changes = format_pimax_native_state_changes(&controller.last_state, &state);
            info!(
                "native Pimax controller control state: count={} hand={:?} descriptor={} handle={} buttons=0x{:08x} touches=0x{:08x} trigger={:.3} grip={:.3} stick=({:.3},{:.3}) pos=({:.3},{:.3},{:.3}) rot=({:.3},{:.3},{:.3},{:.3}) timestamp_ns={} battery={} changes=[{}]",
                runtime.poll_count,
                controller.hand,
                controller.descriptor.to_string_lossy(),
                controller.handle,
                parsed.buttons,
                parsed.touches,
                parsed.trigger,
                parsed.grip,
                parsed.thumbstick_x,
                parsed.thumbstick_y,
                parsed.position.x,
                parsed.position.y,
                parsed.position.z,
                parsed.orientation.x,
                parsed.orientation.y,
                parsed.orientation.z,
                parsed.orientation.w,
                parsed.timestamp_ns,
                controller.last_battery,
                changes
            );
            logged_change = true;
        } else if runtime.poll_count <= 5 || runtime.poll_count % 180 == 0 {
            info!(
                "native Pimax controller poll: count={} hand={:?} handle={} buttons=0x{:08x} touches=0x{:08x} trigger={:.3} grip={:.3} stick=({:.3},{:.3}) pos=({:.3},{:.3},{:.3}) rot=({:.3},{:.3},{:.3},{:.3}) battery={}",
                runtime.poll_count,
                controller.hand,
                controller.handle,
                parsed.buttons,
                parsed.touches,
                parsed.trigger,
                parsed.grip,
                parsed.thumbstick_x,
                parsed.thumbstick_y,
                parsed.position.x,
                parsed.position.y,
                parsed.position.z,
                parsed.orientation.x,
                parsed.orientation.y,
                parsed.orientation.z,
                parsed.orientation.w,
                controller.last_battery
            );
        }

        push_pimax_native_controller_state(controller, parsed);
        controller.last_state = state;
        controller.last_parsed = parsed;
    }

    if logged_change {
        runtime.last_change_log = Instant::now();
    }
}

fn stop_pimax_native_controller_runtime(runtime: PimaxNativeControllerRuntime) {
    for controller in &runtime.controllers {
        unsafe {
            (runtime.api.stop_tracking)(controller.handle);
        }
        info!(
            "native Pimax controller stopped: hand={:?} descriptor={} handle={}",
            controller.hand,
            controller.descriptor.to_string_lossy(),
            controller.handle
        );
        crate::controller::update_controller_connection(controller.hand, false);
    }
    close_pimax_native_controller_api(runtime.api);
}

fn probe_pvr_controller_defaults(env: &mut jni::JNIEnv<'_>) -> Option<String> {
    match call_static_string(
        env,
        "com/pimax/vrservice/PxrServiceApi",
        "ControllerGetDefaultService",
        "()Ljava/lang/String;",
        &[],
    ) {
        Ok(service) => {
            info!("PxrServiceApi.ControllerGetDefaultService() -> {service}");
            Some(service)
        }
        Err(err) => {
            warn!("PxrServiceApi.ControllerGetDefaultService() failed: {err:#}");
            None
        }
    }
    .inspect(|_| {
        match call_static_int(
            env,
            "com/pimax/vrservice/PxrServiceApi",
            "ControllerGetDefaultBufferCnt",
            "()I",
            &[],
        ) {
            Ok(count) => info!("PxrServiceApi.ControllerGetDefaultBufferCnt() -> {count}"),
            Err(err) => warn!("PxrServiceApi.ControllerGetDefaultBufferCnt() failed: {err:#}"),
        }
    })
}

fn probe_pvr_controller_client(
    env: &mut jni::JNIEnv<'_>,
    context: &JObject<'_>,
    default_service: Option<&str>,
) -> Result<()> {
    let Some(default_service) = default_service else {
        warn!("skipping ControllerStart probe because default controller service is unavailable");
        return Ok(());
    };

    info!("starting short-lived PvrServiceClient controller probe");
    let client =
        create_pvr_service_client(env, context).context("create controller probe client")?;
    let mut connected = false;
    let mut started_handle = None;

    let probe_result = (|| -> Result<()> {
        connect_pvr_service_client(env, &client).context("connect controller probe client")?;
        connected = true;
        wait_for_pvr_service_interface(env, &client, Duration::from_secs(3))
            .context("wait for controller probe service interface")?;
        info!("PvrServiceClient controller probe connected");

        let start_info = pvr_service_controller_start(env, &client, default_service)
            .with_context(|| format!("ControllerStart({default_service})"))?;
        dump_object_declared_fields(env, &start_info, "ControllerStartInfo")
            .context("dump ControllerStartInfo")?;
        started_handle = infer_controller_start_handle(env, &start_info);
        Ok(())
    })();

    if let Some(handle) = started_handle {
        if let Err(err) = pvr_service_controller_stop(env, &client, handle) {
            warn!("ControllerStop({handle}) failed during probe cleanup: {err:#}");
        } else {
            info!("ControllerStop({handle}) succeeded during probe cleanup");
        }
    }
    if connected {
        if let Err(err) = disconnect_pvr_service_client(env, &client) {
            warn!("PvrServiceClient.Disconnect failed after controller probe: {err:#}");
        } else {
            info!("PvrServiceClient controller probe disconnected");
        }
    }

    probe_result
}

fn dump_enum_constants(env: &mut jni::JNIEnv<'_>, class_name: &str) -> Result<()> {
    let class_name_text = class_name.to_string();
    let class_name = env
        .new_string(class_name)
        .context("create enum class name string")?;
    let class_name_obj = JObject::from(class_name);
    let class = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&class_name_obj)],
    )
    .with_context(|| format!("load enum class {class_name_text}"))?;

    let values = env
        .call_method(&class, "getEnumConstants", "()[Ljava/lang/Object;", &[])
        .context("get enum constants")?
        .l()
        .context("decode enum constants")?;
    if values.is_null() {
        info!("{class_name_text} has no enum constants");
        return Ok(());
    }

    let values = JObjectArray::from(values);
    let count = env
        .get_array_length(&values)
        .context("count enum constants")?;
    info!("{class_name_text} enum constants: {count}");
    for index in 0..count {
        let constant = env
            .get_object_array_element(&values, index)
            .with_context(|| format!("get enum constant {index}"))?;
        let name: String = env
            .call_method(&constant, "name", "()Ljava/lang/String;", &[])
            .context("get enum constant name")?
            .l()
            .context("decode enum constant name")
            .and_then(|object| {
                object_to_string(env, object).context("enum constant name string")
            })?;
        info!("{class_name_text}::{name}");
    }

    Ok(())
}

fn dump_matching_methods(
    env: &mut jni::JNIEnv<'_>,
    class_name: &str,
    keywords: &[&str],
) -> Result<()> {
    let class_name_text = class_name.to_string();
    let class_name = env
        .new_string(class_name)
        .context("create class name string")?;
    let class_name_obj = JObject::from(class_name);
    let class = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&class_name_obj)],
    )
    .with_context(|| format!("load class {class_name_text}"))?;

    let methods = env
        .call_method(
            &class,
            "getDeclaredMethods",
            "()[Ljava/lang/reflect/Method;",
            &[],
        )
        .context("get declared methods")?
        .l()
        .context("decode declared methods")?;
    let methods = JObjectArray::from(methods);
    let count = env
        .get_array_length(&methods)
        .context("count declared methods")?;
    info!("{class_name_text} has {count} declared methods");
    for index in 0..count {
        let method = env
            .get_object_array_element(&methods, index)
            .with_context(|| format!("get declared method {index}"))?;
        let signature: String = env
            .call_method(&method, "toString", "()Ljava/lang/String;", &[])
            .context("get method signature")?
            .l()
            .context("decode method signature")
            .and_then(|object| object_to_string(env, object).context("method signature string"))?;
        if keywords.iter().any(|keyword| signature.contains(keyword)) {
            info!("{class_name_text}::{signature}");
        }
    }
    Ok(())
}

fn dump_all_methods(env: &mut jni::JNIEnv<'_>, class_name: &str) -> Result<()> {
    let class_name_text = class_name.to_string();
    let class_name = env
        .new_string(class_name)
        .context("create class name string")?;
    let class_name_obj = JObject::from(class_name);
    let class = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&class_name_obj)],
    )
    .with_context(|| format!("load class {class_name_text}"))?;

    let methods = env
        .call_method(
            &class,
            "getDeclaredMethods",
            "()[Ljava/lang/reflect/Method;",
            &[],
        )
        .context("get declared methods")?
        .l()
        .context("decode declared methods")?;
    let methods = JObjectArray::from(methods);
    let count = env
        .get_array_length(&methods)
        .context("count declared methods")?;
    info!("{class_name_text} has {count} declared methods");
    for index in 0..count {
        let method = env
            .get_object_array_element(&methods, index)
            .with_context(|| format!("get declared method {index}"))?;
        let signature: String = env
            .call_method(&method, "toString", "()Ljava/lang/String;", &[])
            .context("get method signature")?
            .l()
            .context("decode method signature")
            .and_then(|object| object_to_string(env, object).context("method signature string"))?;
        info!("{class_name_text}::{signature}");
    }
    Ok(())
}

fn dump_declared_inner_classes(env: &mut jni::JNIEnv<'_>, class_name: &str) -> Result<()> {
    let class_name_text = class_name.to_string();
    let class_name = env
        .new_string(class_name)
        .context("create class name string")?;
    let class_name_obj = JObject::from(class_name);
    let class = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&class_name_obj)],
    )
    .with_context(|| format!("load class {class_name_text}"))?;

    let classes = env
        .call_method(&class, "getDeclaredClasses", "()[Ljava/lang/Class;", &[])
        .context("get declared inner classes")?
        .l()
        .context("decode declared inner classes")?;
    let classes = JObjectArray::from(classes);
    let count = env
        .get_array_length(&classes)
        .context("count declared inner classes")?;
    info!("{class_name_text} has {count} declared inner classes");

    for index in 0..count {
        let class = env
            .get_object_array_element(&classes, index)
            .with_context(|| format!("get declared inner class {index}"))?;
        let name: String = env
            .call_method(&class, "getName", "()Ljava/lang/String;", &[])
            .context("get inner class name")?
            .l()
            .context("decode inner class name")
            .and_then(|object| object_to_string(env, object).context("inner class name string"))?;
        info!("{class_name_text} inner class: {name}");
    }

    Ok(())
}

fn dump_matching_fields(
    env: &mut jni::JNIEnv<'_>,
    class_name: &str,
    keywords: &[&str],
) -> Result<()> {
    let class_name_text = class_name.to_string();
    let class_name = env
        .new_string(class_name)
        .context("create class name string")?;
    let class_name_obj = JObject::from(class_name);
    let class = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&class_name_obj)],
    )
    .with_context(|| format!("load class {class_name_text}"))?;

    let fields = env
        .call_method(
            &class,
            "getDeclaredFields",
            "()[Ljava/lang/reflect/Field;",
            &[],
        )
        .context("get declared fields")?
        .l()
        .context("decode declared fields")?;
    let fields = JObjectArray::from(fields);
    let count = env
        .get_array_length(&fields)
        .context("count declared fields")?;
    info!("{class_name_text} has {count} declared fields");
    for index in 0..count {
        let field = env
            .get_object_array_element(&fields, index)
            .with_context(|| format!("get declared field {index}"))?;
        let name: String = env
            .call_method(&field, "getName", "()Ljava/lang/String;", &[])
            .context("get field name")?
            .l()
            .context("decode field name")
            .and_then(|object| object_to_string(env, object).context("field name string"))?;
        if keywords.iter().any(|keyword| name.contains(keyword)) {
            let modifiers = env
                .call_method(&field, "getModifiers", "()I", &[])
                .context("get field modifiers")?
                .i()
                .context("decode field modifiers")?;
            info!("{class_name_text}::{name} modifiers={modifiers}");
        }
    }
    Ok(())
}

fn enum_constant<'local>(
    env: &mut jni::JNIEnv<'local>,
    class_name: &str,
    constant_name: &str,
) -> Result<JObject<'local>> {
    let class_name_text = class_name.to_string();
    let constant_name_text = constant_name.to_string();
    let class_name = env
        .new_string(class_name)
        .context("create enum class name string")?;
    let class_name_obj = JObject::from(class_name);
    let class = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&class_name_obj)],
    )
    .with_context(|| format!("load enum class {class_name_text}"))?;
    let values = env
        .call_method(&class, "getEnumConstants", "()[Ljava/lang/Object;", &[])
        .context("get enum constants")?
        .l()
        .context("decode enum constants")?;
    if values.is_null() {
        anyhow::bail!("{class_name_text} is not an enum");
    }

    let values = JObjectArray::from(values);
    let count = env
        .get_array_length(&values)
        .context("count enum constants")?;
    for index in 0..count {
        let constant = env
            .get_object_array_element(&values, index)
            .with_context(|| format!("get enum constant {index}"))?;
        let name: String = env
            .call_method(&constant, "name", "()Ljava/lang/String;", &[])
            .context("get enum constant name")?
            .l()
            .context("decode enum constant name")
            .and_then(|object| {
                object_to_string(env, object).context("enum constant name string")
            })?;
        if name == constant_name_text {
            return Ok(constant);
        }
    }

    anyhow::bail!("{class_name_text}::{constant_name_text} not found")
}

fn set_object_field(
    env: &mut jni::JNIEnv<'_>,
    obj: &JObject<'_>,
    name: &str,
    signature: &str,
    value: &JObject<'_>,
) -> Result<()> {
    env.set_field(obj, name, signature, JValue::Object(value))
        .with_context(|| format!("set field {name}"))
}

fn set_static_object_field_via_reflection(
    env: &mut jni::JNIEnv<'_>,
    class_name: &str,
    field_name: &str,
    value: &JObject<'_>,
) -> Result<()> {
    let class_name_text = class_name.to_string();
    let class_name = env
        .new_string(class_name)
        .context("create class name string")?;
    let class_name_obj = JObject::from(class_name);
    let class = call_static_object(
        env,
        "java/lang/Class",
        "forName",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[JValue::Object(&class_name_obj)],
    )
    .with_context(|| format!("load class {class_name_text}"))?;
    let field = env
        .call_method(
            &class,
            "getDeclaredField",
            "(Ljava/lang/String;)Ljava/lang/reflect/Field;",
            &[JValue::Object(&JObject::from(
                env.new_string(field_name)
                    .context("create field name string")?,
            ))],
        )
        .context("get declared field")?
        .l()
        .context("decode declared field")?;
    let _ = env.call_method(&field, "setAccessible", "(Z)V", &[JValue::Bool(1)]);
    env.call_method(
        &field,
        "set",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[JValue::Object(&JObject::null()), JValue::Object(value)],
    )
    .context("set reflected static field")?;
    Ok(())
}

fn set_float_array_field(
    env: &mut jni::JNIEnv<'_>,
    obj: &JObject<'_>,
    name: &str,
    values: &[f32],
) -> Result<()> {
    let array = env
        .new_float_array(values.len() as i32)
        .context("create float array")?;
    env.set_float_array_region(&array, 0, values)
        .context("fill float array")?;
    let array_obj = JObject::from(array);
    env.set_field(obj, name, "[F", JValue::Object(&array_obj))
        .with_context(|| format!("set float array field {name}"))
}

fn set_simple_layout_coords(env: &mut jni::JNIEnv<'_>, coords: &JObject<'_>) -> Result<()> {
    set_float_array_field(env, coords, "LowerLeftPos", &[-1.0, -1.0, 0.0, 1.0])?;
    set_float_array_field(env, coords, "LowerRightPos", &[1.0, -1.0, 0.0, 1.0])?;
    set_float_array_field(env, coords, "UpperLeftPos", &[-1.0, 1.0, 0.0, 1.0])?;
    set_float_array_field(env, coords, "UpperRightPos", &[1.0, 1.0, 0.0, 1.0])?;
    set_float_array_field(env, coords, "LowerUVs", &[0.0, 1.0, 1.0, 1.0])?;
    set_float_array_field(env, coords, "UpperUVs", &[0.0, 0.0, 1.0, 0.0])?;
    set_float_array_field(
        env,
        coords,
        "TransformMatrix",
        &[
            1.0, 0.0, 0.0, 0.0, //
            0.0, 1.0, 0.0, 0.0, //
            0.0, 0.0, 1.0, 0.0, //
            0.0, 0.0, 0.0, 1.0,
        ],
    )?;
    Ok(())
}

fn create_identity_uv_map_texture(samples: i32) -> Result<UvMapTexture> {
    let samples = samples.max(1);
    let size = samples + 1;
    let mut pixels = Vec::<f32>::with_capacity((size * size * 2) as usize);
    for y in 0..=samples {
        let v = y as f32 / samples as f32;
        for x in 0..=samples {
            let u = x as f32 / samples as f32;
            pixels.push(u);
            pixels.push(v);
        }
    }

    let mut texture = 0_u32;
    unsafe {
        glGenTextures(1, &mut texture as *mut u32);
        glBindTexture(GL_TEXTURE_2D, texture);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexImage2D(
            GL_TEXTURE_2D,
            0,
            GL_RG32F,
            size,
            size,
            0,
            GL_RG,
            GL_FLOAT,
            pixels.as_ptr().cast(),
        );
        glBindTexture(GL_TEXTURE_2D, 0);
    }

    Ok(UvMapTexture { texture, size })
}

fn create_presentation_surface<'local>(
    env: &mut jni::JNIEnv<'local>,
    context: &JObject<'local>,
) -> Result<JObject<'local>> {
    let display_key = env
        .new_string("display")
        .context("create display service key")?;
    let display_manager = env
        .call_method(
            context,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&JObject::from(display_key))],
        )
        .context("get display service")?
        .l()
        .context("decode display service")?;
    let displays = env
        .call_method(
            &display_manager,
            "getDisplays",
            "()[Landroid/view/Display;",
            &[],
        )
        .context("enumerate displays")?
        .l()
        .context("decode display array")?;
    let displays = JObjectArray::from(displays);
    let display_count = env.get_array_length(&displays).context("count displays")?;
    if display_count == 0 {
        bail!("no displays available");
    }

    let mut chosen_display: Option<JObject<'local>> = None;
    let mut chosen_display_id = 0;
    let mut chosen_display_name = String::new();

    for index in 0..display_count {
        let display = env
            .get_object_array_element(&displays, index)
            .with_context(|| format!("get display {index}"))?;
        let display_id = env
            .call_method(&display, "getDisplayId", "()I", &[])
            .context("query display id")?
            .i()
            .context("decode display id")?;
        let display_name_obj = env
            .call_method(&display, "getName", "()Ljava/lang/String;", &[])
            .context("query display name")?
            .l()
            .context("decode display name")?;
        let display_name =
            object_to_string(env, display_name_obj).context("display name string")?;
        let display_flags = env
            .call_method(&display, "getFlags", "()I", &[])
            .context("query display flags")?
            .i()
            .context("decode display flags")?;
        info!("display candidate #{display_id} name={display_name} flags=0x{display_flags:08x}");

        let is_presentation_display = (display_flags & 0x8) != 0
            && matches!(
                display_name.as_str(),
                "PxrScreenShot" | "PxrScreenRecord" | "PxrScreenCast"
            );
        if is_presentation_display {
            chosen_display = Some(display);
            chosen_display_id = display_id;
            chosen_display_name = display_name;
            break;
        }
    }

    let display = chosen_display.context("choose suitable presentation display")?;
    info!("attempting PxrPresentation on display #{chosen_display_id} named {chosen_display_name}");

    call_static_void(env, "android/os/Looper", "prepare", "()V", &[])
        .context("prepare looper for PxrPresentation")?;
    let presentation = env
        .new_object(
            "com/pimax/pxrapi/xrcasting/PxrPresentation",
            "(Landroid/content/Context;Landroid/view/Display;)V",
            &[JValue::Object(context), JValue::Object(&display)],
        )
        .map_err(|err| {
            if let Some(summary) = take_java_exception_summary(env) {
                warn!("PxrPresentation constructor threw: {summary}");
            }
            err
        })
        .context("create PxrPresentation")?;
    if let Err(err) = env.call_method(&presentation, "show", "()V", &[]) {
        let summary = take_java_exception_summary(env)
            .unwrap_or_else(|| "Java exception was thrown".to_string());
        bail!("show PxrPresentation: {summary}; {err:#}");
    }
    thread::sleep(Duration::from_millis(250));

    let surface_view = env
        .get_field(
            &presentation,
            "mSurfaceView",
            "Lcom/pimax/pxrapi/xrcasting/PxrPresentation$SxrPresentationSurfaceView;",
        )
        .context("get presentation surface view")?
        .l()
        .context("decode presentation surface view")?;
    let holder = env
        .call_method(
            &surface_view,
            "getHolder",
            "()Landroid/view/SurfaceHolder;",
            &[],
        )
        .context("get presentation surface holder")?
        .l()
        .context("decode presentation surface holder")?;
    env.call_method(&holder, "getSurface", "()Landroid/view/Surface;", &[])
        .context("get presentation surface")?
        .l()
        .context("decode presentation surface")
}

fn capture_activity_window<'local>(
    env: &mut jni::JNIEnv<'local>,
    timeout: Duration,
) -> Result<(JObject<'local>, ndk::native_window::NativeWindow)> {
    let native_window = wait_for_activity_native_window(timeout)?;
    info!(
        "using NativeActivity window surface {}x{} format {:?}",
        native_window.width(),
        native_window.height(),
        native_window.format()
    );
    let surface = unsafe { native_window.to_surface(env.get_native_interface()) };
    if surface.is_null() {
        bail!("native window to Surface conversion returned null");
    }
    Ok((unsafe { JObject::from_raw(surface) }, native_window))
}

fn capture_activity_window_blocking<'local>(
    env: &mut jni::JNIEnv<'local>,
) -> Result<(JObject<'local>, ndk::native_window::NativeWindow)> {
    let mut announced_wait = false;
    let mut last_progress_log = Instant::now();
    loop {
        match capture_activity_window(env, Duration::from_secs(2)) {
            Ok(window) => return Ok(window),
            Err(err) => {
                if !announced_wait {
                    warn!(
                        "real NativeActivity window is still unavailable; waiting instead of using a risky presentation fallback: {err:#}"
                    );
                    announced_wait = true;
                    last_progress_log = Instant::now();
                } else if last_progress_log.elapsed() >= Duration::from_secs(5) {
                    info!("still waiting for a real NativeActivity window");
                    last_progress_log = Instant::now();
                }
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
}

struct EglState {
    display: EGLDisplay,
    context: EGLContext,
    surface: EGLSurface,
    width: i32,
    height: i32,
    is_window_surface: bool,
    protected_content: bool,
    _window: Option<ndk::native_window::NativeWindow>,
}

pub(crate) struct EyeRenderTarget {
    pub texture: u32,
    pub framebuffer: u32,
    pub width: i32,
    pub height: i32,
    egl_image: Option<EGLImageKHR>,
    egl_client_buffer: Option<EGLClientBuffer>,
    hardware_buffer: Option<HardwareBufferRef>,
}

struct EyeBufferPair {
    left: EyeRenderTarget,
    right: EyeRenderTarget,
}

#[derive(Clone, Copy)]
struct TextureRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

struct DiagnosticPatternState {
    left_base: Vec<u8>,
    right_base: Vec<u8>,
    last_marker_rects: Vec<Option<TextureRect>>,
}

struct UvMapTexture {
    texture: u32,
    size: i32,
}

impl Drop for EyeRenderTarget {
    fn drop(&mut self) {
        unsafe {
            if self.framebuffer != 0 {
                glDeleteFramebuffers(1, &self.framebuffer as *const u32);
            }
            if self.texture != 0 {
                glDeleteTextures(1, &self.texture as *const u32);
            }
        }
        if let Some(egl_image) = self.egl_image.take() {
            let display = unsafe { eglGetCurrentDisplay() };
            if !display.is_null() {
                if let Ok(destroy_image) = load_egl_proc::<
                    unsafe extern "C" fn(EGLDisplay, EGLImageKHR) -> u32,
                >("eglDestroyImageKHR")
                {
                    unsafe {
                        let _ = destroy_image(display, egl_image);
                    }
                }
            }
        }
    }
}

impl Drop for UvMapTexture {
    fn drop(&mut self) {
        unsafe {
            if self.texture != 0 {
                glDeleteTextures(1, &self.texture as *const u32);
            }
        }
    }
}

impl Drop for EglState {
    fn drop(&mut self) {
        unsafe {
            let _ = eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            let _ = eglDestroySurface(self.display, self.surface);
            let _ = eglDestroyContext(self.display, self.context);
            let _ = eglTerminate(self.display);
        }
    }
}

fn wait_for_activity_native_window(timeout: Duration) -> Result<ndk::native_window::NativeWindow> {
    let deadline = Instant::now() + timeout;
    let mut announced_wait = false;
    loop {
        if let Some(native_window) = ndk_glue::native_window() {
            return Ok((*native_window).clone());
        }

        if Instant::now() >= deadline {
            bail!("activity native window unavailable after {:?}", timeout);
        }

        if !announced_wait {
            info!(
                "waiting up to {}ms for NativeActivity window",
                timeout.as_millis()
            );
            announced_wait = true;
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn current_android_thread_id(env: &mut jni::JNIEnv<'_>) -> Result<i32> {
    call_static_int(env, "android/os/Process", "myTid", "()I", &[])
        .context("get current Android thread id")
}

fn gl_string(name: u32) -> Option<String> {
    unsafe {
        let ptr = glGetString(name);
        if ptr.is_null() {
            return None;
        }
        Some(CStr::from_ptr(ptr.cast()).to_string_lossy().into_owned())
    }
}

fn choose_egl_config(
    display: EGLDisplay,
    surface_type_bits: i32,
    renderable_type: i32,
) -> Result<EGLConfig> {
    let config_attribs = [
        EGL_RENDERABLE_TYPE,
        renderable_type,
        EGL_SURFACE_TYPE,
        surface_type_bits,
        EGL_RED_SIZE,
        8,
        EGL_GREEN_SIZE,
        8,
        EGL_BLUE_SIZE,
        8,
        EGL_ALPHA_SIZE,
        8,
        EGL_NONE,
    ];
    let mut config: EGLConfig = ptr::null_mut();
    let mut num_config = 0;
    unsafe {
        if eglChooseConfig(
            display,
            config_attribs.as_ptr(),
            &mut config,
            1,
            &mut num_config,
        ) == EGL_FALSE
            || num_config <= 0
            || config.is_null()
        {
            anyhow::bail!("eglChooseConfig failed for renderable type 0x{renderable_type:04x}");
        }
    }
    Ok(config)
}

fn egl_config_attrib(display: EGLDisplay, config: EGLConfig, attribute: i32) -> Option<i32> {
    let mut value = 0;
    unsafe {
        if eglGetConfigAttrib(display, config, attribute, &mut value) == EGL_FALSE {
            None
        } else {
            Some(value)
        }
    }
}

fn create_and_bind_egl_context(
    display: EGLDisplay,
    config: EGLConfig,
    surface: EGLSurface,
    preferred_versions: &[i32],
) -> Result<(EGLContext, i32, bool)> {
    let mut failures = Vec::new();

    for &protected_content in &PIMAX_EGL_PROTECTED_PREFERENCE {
        for version in preferred_versions {
            let context_attribs = if protected_content {
                vec![
                    EGL_CONTEXT_CLIENT_VERSION,
                    *version,
                    EGL_PROTECTED_CONTENT_EXT,
                    EGL_TRUE,
                    EGL_NONE,
                ]
            } else {
                vec![EGL_CONTEXT_CLIENT_VERSION, *version, EGL_NONE]
            };
            let context = unsafe {
                eglCreateContext(display, config, ptr::null_mut(), context_attribs.as_ptr())
            };
            if context.is_null() {
                failures.push(format!(
                    "eglCreateContext GLES{} protected={} egl_error=0x{:04x}",
                    version,
                    protected_content,
                    unsafe { eglGetError() }
                ));
                continue;
            }

            if unsafe { eglMakeCurrent(display, surface, surface, context) } == EGL_FALSE {
                let egl_error = unsafe { eglGetError() };
                unsafe {
                    eglDestroyContext(display, context);
                }
                failures.push(format!(
                    "eglMakeCurrent GLES{} protected={} egl_error=0x{:04x}",
                    version, protected_content, egl_error
                ));
                continue;
            }

            return Ok((context, *version, protected_content));
        }
    }

    anyhow::bail!(
        "failed to create and bind EGL context: {}",
        failures.join(", ")
    )
}

fn initialize_egl_context(
    surface_type_bits: i32,
    prefer_gles3: bool,
) -> Result<(EGLDisplay, EGLConfig, i32, i32)> {
    unsafe {
        let display = eglGetDisplay(ptr::null_mut());
        if display.is_null() {
            anyhow::bail!("eglGetDisplay returned null");
        }

        let mut major = 0;
        let mut minor = 0;
        if eglInitialize(display, &mut major, &mut minor) == EGL_FALSE {
            anyhow::bail!("eglInitialize failed");
        }

        let config = if prefer_gles3 {
            choose_egl_config(display, surface_type_bits, EGL_OPENGL_ES3_BIT_KHR).or_else(
                |err| {
                    warn!("GLES3 EGL config selection failed, falling back to GLES2: {err:#}");
                    choose_egl_config(display, surface_type_bits, EGL_OPENGL_ES2_BIT)
                },
            )?
        } else {
            choose_egl_config(display, surface_type_bits, EGL_OPENGL_ES2_BIT)?
        };

        Ok((display, config, major, minor))
    }
}

fn initialize_pbuffer_egl_context() -> Result<EglState> {
    unsafe {
        let (display, config, major, minor) =
            initialize_egl_context(EGL_PBUFFER_BIT, true).context("choose EGL pbuffer config")?;

        let mut surface = ptr::null_mut();
        let mut surface_protected = false;
        let mut last_surface_error = 0;
        for &prefer_protected_surface in &PIMAX_EGL_PROTECTED_PREFERENCE {
            let surface_attribs = if prefer_protected_surface {
                vec![
                    EGL_WIDTH,
                    2,
                    EGL_HEIGHT,
                    2,
                    EGL_PROTECTED_CONTENT_EXT,
                    EGL_TRUE,
                    EGL_NONE,
                ]
            } else {
                vec![EGL_WIDTH, 2, EGL_HEIGHT, 2, EGL_NONE]
            };
            surface = eglCreatePbufferSurface(display, config, surface_attribs.as_ptr());
            if !surface.is_null() {
                surface_protected = prefer_protected_surface;
                break;
            }
            last_surface_error = eglGetError();
            warn!(
                "eglCreatePbufferSurface failed with protected={} (egl_error=0x{:04x})",
                prefer_protected_surface, last_surface_error
            );
        }
        if surface.is_null() {
            anyhow::bail!(
                "eglCreatePbufferSurface failed for both protected and unprotected attempts (last egl_error=0x{:04x})",
                last_surface_error
            );
        }

        let (context, gles_version, context_protected) =
            create_and_bind_egl_context(display, config, surface, &[3, 2])
                .context("create and bind EGL context")?;
        let config_id = egl_config_attrib(display, config, EGL_CONFIG_ID).unwrap_or(-1);

        info!(
            "EGL pbuffer context ready: EGL={major}.{minor} GLES={gles_version} vendor={:?} renderer={:?} gl={:?} config_id={} protected_surface={} protected_context={}",
            gl_string(GL_VENDOR),
            gl_string(GL_RENDERER),
            gl_string(GL_VERSION),
            config_id,
            surface_protected,
            context_protected
        );

        Ok(EglState {
            display,
            context,
            surface,
            width: 2,
            height: 2,
            is_window_surface: false,
            protected_content: surface_protected || context_protected,
            _window: None,
        })
    }
}

fn initialize_window_egl_context(
    native_window: ndk::native_window::NativeWindow,
) -> Result<EglState> {
    unsafe {
        let (display, config, major, minor) =
            initialize_egl_context(EGL_WINDOW_BIT, true).context("choose EGL window config")?;
        let width = native_window.width();
        let height = native_window.height();
        let format = native_window.format();
        let config_id = egl_config_attrib(display, config, EGL_CONFIG_ID).unwrap_or(-1);
        info!(
            "preparing NativeWindow for EGL surface: size={}x{} format {:?} config_id={}",
            width, height, format, config_id
        );
        match native_window.set_buffers_geometry(
            width,
            height,
            Some(ndk::hardware_buffer_format::HardwareBufferFormat::R8G8B8A8_UNORM),
        ) {
            Ok(()) => info!(
                "requested NativeWindow buffer geometry {}x{} RGBA8888 before eglCreateWindowSurface",
                width, height
            ),
            Err(err) => warn!(
                "failed to request NativeWindow buffer geometry {}x{} RGBA8888 before eglCreateWindowSurface: {err:#}",
                width, height
            ),
        }

        let mut surface = ptr::null_mut();
        let mut surface_protected = false;
        let mut last_surface_error = 0;
        for &prefer_protected_surface in &PIMAX_EGL_PROTECTED_PREFERENCE {
            let surface_attribs = if prefer_protected_surface {
                vec![EGL_PROTECTED_CONTENT_EXT, EGL_TRUE, EGL_NONE]
            } else {
                vec![EGL_NONE]
            };
            surface = eglCreateWindowSurface(
                display,
                config,
                native_window.ptr().as_ptr().cast(),
                surface_attribs.as_ptr(),
            );
            if !surface.is_null() {
                surface_protected = prefer_protected_surface;
                break;
            }
            let initial_error = eglGetError();
            warn!(
                "eglCreateWindowSurface failed after explicit buffer geometry with protected={} (egl_error=0x{:04x}); resetting NativeWindow buffers to defaults and retrying",
                prefer_protected_surface, initial_error
            );
            match native_window.set_buffers_geometry(0, 0, None) {
                Ok(()) => info!("reset NativeWindow buffer geometry to defaults for EGL retry"),
                Err(err) => {
                    warn!("failed to reset NativeWindow buffer geometry before EGL retry: {err:#}")
                }
            }
            surface = eglCreateWindowSurface(
                display,
                config,
                native_window.ptr().as_ptr().cast(),
                surface_attribs.as_ptr(),
            );
            if !surface.is_null() {
                surface_protected = prefer_protected_surface;
                break;
            }
            last_surface_error = eglGetError();
            warn!(
                "eglCreateWindowSurface retry still failed with protected={} (egl_error=0x{:04x})",
                prefer_protected_surface, last_surface_error
            );
        }
        if surface.is_null() {
            anyhow::bail!(
                "eglCreateWindowSurface failed for both protected and unprotected attempts (last egl_error=0x{:04x})",
                last_surface_error
            );
        }

        let (context, gles_version, context_protected) =
            create_and_bind_egl_context(display, config, surface, &[3, 2])
                .context("create and bind EGL context")?;

        info!(
            "EGL window context ready: EGL={major}.{minor} GLES={gles_version} vendor={:?} renderer={:?} gl={:?} config_id={} protected_surface={} protected_context={}",
            gl_string(GL_VENDOR),
            gl_string(GL_RENDERER),
            gl_string(GL_VERSION),
            config_id,
            surface_protected,
            context_protected
        );

        Ok(EglState {
            display,
            context,
            surface,
            width,
            height,
            is_window_surface: true,
            protected_content: surface_protected || context_protected,
            _window: Some(native_window),
        })
    }
}

fn initialize_window_egl_context_from_surface(
    env: &jni::JNIEnv<'_>,
    surface: &JObject<'_>,
) -> Result<EglState> {
    let native_window = unsafe {
        ndk::native_window::NativeWindow::from_surface(env.get_native_interface(), surface.as_raw())
    }
    .context("convert Java Surface to ANativeWindow")?;
    initialize_window_egl_context(native_window)
}

fn ensure_gl_context(
    env: &jni::JNIEnv<'_>,
    surface: Option<&JObject<'_>>,
) -> Result<Option<EglState>> {
    unsafe {
        let current_context = eglGetCurrentContext();
        if !current_context.is_null() {
            info!(
                "using existing EGL context from runtime: {:?}",
                current_context
            );
            return Ok(None);
        }
    }

    if let Some(surface) = surface {
        let egl_state = initialize_window_egl_context_from_surface(env, surface)
            .context("initialize EGL window context from Java Surface")?;
        return Ok(Some(egl_state));
    }

    if let Some(native_window) = ndk_glue::native_window() {
        let egl_state = initialize_window_egl_context((*native_window).clone())
            .context("initialize EGL window context")?;
        return Ok(Some(egl_state));
    }

    let egl_state = initialize_pbuffer_egl_context().context("initialize EGL pbuffer context")?;
    Ok(Some(egl_state))
}

fn ensure_offscreen_gl_context() -> Result<Option<EglState>> {
    unsafe {
        let current_context = eglGetCurrentContext();
        if !current_context.is_null() {
            info!(
                "using existing EGL context from runtime: {:?}",
                current_context
            );
            return Ok(None);
        }
    }

    let egl_state = initialize_pbuffer_egl_context().context("initialize offscreen EGL context")?;
    Ok(Some(egl_state))
}

fn render_eye_clear(width: i32, height: i32, color: [u8; 4]) {
    let red = color[0] as f32 / 255.0;
    let green = color[1] as f32 / 255.0;
    let blue = color[2] as f32 / 255.0;
    let alpha = color[3] as f32 / 255.0;

    unsafe {
        glViewport(0, 0, width.max(1), height.max(1));
        prepare_solid_color_clear_state();
        glClearColor(red, green, blue, alpha);
        glClear(GL_COLOR_BUFFER_BIT);
        glFlush();
        glFinish();
    }
}

fn render_window_surface_mirror(
    egl_state: Option<&EglState>,
    frame_index: i32,
    color: [u8; 4],
) -> Result<()> {
    let Some(egl_state) = egl_state else {
        return Ok(());
    };
    if !egl_state.is_window_surface {
        return Ok(());
    }
    if egl_state.protected_content {
        return Ok(());
    }

    let red = color[0] as f32 / 255.0;
    let green = color[1] as f32 / 255.0;
    let blue = color[2] as f32 / 255.0;
    let alpha = color[3] as f32 / 255.0;
    let previous_framebuffer = current_framebuffer_binding();

    unsafe {
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        glViewport(0, 0, egl_state.width.max(1), egl_state.height.max(1));
        prepare_solid_color_clear_state();
        glClearColor(red, green, blue, alpha);
        glClear(GL_COLOR_BUFFER_BIT);
        glFlush();
    }

    unsafe {
        if eglSwapBuffers(egl_state.display, egl_state.surface) == EGL_FALSE {
            anyhow::bail!(
                "eglSwapBuffers failed for frame {frame_index} (egl_error=0x{:04x})",
                eglGetError()
            );
        }
    }

    unsafe {
        glBindFramebuffer(GL_FRAMEBUFFER, previous_framebuffer as u32);
    }
    Ok(())
}

fn load_egl_proc<T: Copy>(name: &str) -> Result<T> {
    let name = CString::new(name).context("create proc name")?;
    let proc = unsafe { eglGetProcAddress(name.as_ptr().cast()) };
    if proc.is_null() {
        bail!("missing EGL/GLES extension proc {name:?}");
    }
    Ok(unsafe { std::mem::transmute_copy(&proc) })
}

fn create_hardware_buffer_eye_render_target(width: i32, height: i32) -> Result<EyeRenderTarget> {
    type EglGetNativeClientBufferAndroid =
        unsafe extern "C" fn(buffer: *const c_void) -> EGLClientBuffer;
    type EglCreateImageKhr = unsafe extern "C" fn(
        dpy: EGLDisplay,
        ctx: EGLContext,
        target: i32,
        buffer: EGLClientBuffer,
        attrib_list: *const i32,
    ) -> EGLImageKHR;
    type GlEglImageTargetTexture2dOes = unsafe extern "C" fn(target: u32, image: EGLImageKHR);

    let width = width.max(1) as u32;
    let height = height.max(1) as u32;
    let mut usage = HardwareBufferUsage::GPU_SAMPLED_IMAGE;
    usage.0 .0 |= HardwareBufferUsage::GPU_FRAMEBUFFER.0 .0;
    usage.0 .0 |= HardwareBufferUsage::COMPOSER_OVERLAY.0 .0;
    let desc = HardwareBufferDesc {
        width,
        height,
        layers: 1,
        format: ndk::hardware_buffer_format::HardwareBufferFormat::R8G8B8A8_UNORM,
        usage,
        stride: 0,
    };
    let hardware_buffer = HardwareBuffer::allocate(desc).context("allocate AHardwareBuffer")?;
    let get_native_client_buffer =
        load_egl_proc::<EglGetNativeClientBufferAndroid>("eglGetNativeClientBufferANDROID")
            .context("load eglGetNativeClientBufferANDROID")?;
    let create_image = load_egl_proc::<EglCreateImageKhr>("eglCreateImageKHR")
        .context("load eglCreateImageKHR")?;
    let image_target =
        load_egl_proc::<GlEglImageTargetTexture2dOes>("glEGLImageTargetTexture2DOES")
            .context("load glEGLImageTargetTexture2DOES")?;

    let display = unsafe { eglGetCurrentDisplay() };
    if display.is_null() {
        bail!("eglGetCurrentDisplay returned null while creating eye target");
    }

    let client_buffer = unsafe { get_native_client_buffer(hardware_buffer.as_ptr().cast()) };
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
        bail!("eglCreateImageKHR returned null (egl=0x{:04x})", unsafe {
            eglGetError()
        });
    }

    let mut texture = 0_u32;
    let mut framebuffer = 0_u32;
    unsafe {
        glGenTextures(1, &mut texture as *mut u32);
        glBindTexture(GL_TEXTURE_2D, texture);
        // The Pimax compositor samples these submitted eye textures directly.
        // Use linear filtering so panel upscaling does not look blocky.
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        image_target(GL_TEXTURE_2D, egl_image);

        glGenFramebuffers(1, &mut framebuffer as *mut u32);
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glFramebufferTexture2D(
            GL_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            texture,
            0,
        );
        let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        glBindTexture(GL_TEXTURE_2D, 0);
        if status != GL_FRAMEBUFFER_COMPLETE {
            bail!(
                "hardware-buffer GL framebuffer incomplete: status=0x{status:04x} tex={texture} fb={framebuffer}"
            );
        }
    }

    Ok(EyeRenderTarget {
        texture,
        framebuffer,
        width: width as i32,
        height: height as i32,
        egl_image: Some(egl_image),
        egl_client_buffer: Some(client_buffer),
        hardware_buffer: Some(hardware_buffer),
    })
}

fn use_hardware_buffer_eye_targets() -> bool {
    !(PIMAX_SUBMITTED_TEXTURE_TYPE == "kTypeTexture"
        && matches!(
            PIMAX_SUBMITTED_HANDLE_KIND,
            SubmittedImageHandleKind::TextureId
        ))
}

fn create_eye_render_target(width: i32, height: i32) -> Result<EyeRenderTarget> {
    let width = width.max(1);
    let height = height.max(1);
    if use_hardware_buffer_eye_targets() {
        match create_hardware_buffer_eye_render_target(width, height) {
            Ok(target) => {
                info!(
                    "created AHardwareBuffer-backed eye render target tex={} fb={} size={}x{}",
                    target.texture, target.framebuffer, target.width, target.height
                );
                return Ok(target);
            }
            Err(err) => {
                warn!(
                    "AHardwareBuffer eye target unavailable, falling back to glTexImage2D: {err:#}"
                )
            }
        }
    } else {
        info!(
            "using plain GL eye textures because submission path is {PIMAX_SUBMITTED_TEXTURE_TYPE} via {:?}",
            PIMAX_SUBMITTED_HANDLE_KIND
        );
    }
    let mut texture = 0_u32;
    let mut framebuffer = 0_u32;

    unsafe {
        glGenTextures(1, &mut texture as *mut u32);
        glBindTexture(GL_TEXTURE_2D, texture);
        // The Pimax compositor samples these submitted eye textures directly.
        // Use linear filtering so panel upscaling does not look blocky.
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        glTexImage2D(
            GL_TEXTURE_2D,
            0,
            GL_RGBA8,
            width,
            height,
            0,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            ptr::null(),
        );

        glGenFramebuffers(1, &mut framebuffer as *mut u32);
        glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
        glFramebufferTexture2D(
            GL_FRAMEBUFFER,
            GL_COLOR_ATTACHMENT0,
            GL_TEXTURE_2D,
            texture,
            0,
        );
        let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
        glBindFramebuffer(GL_FRAMEBUFFER, 0);
        glBindTexture(GL_TEXTURE_2D, 0);
        if status != GL_FRAMEBUFFER_COMPLETE {
            anyhow::bail!(
                "GL framebuffer incomplete for eye target: status=0x{status:04x} tex={texture} fb={framebuffer}"
            );
        }
    }

    Ok(EyeRenderTarget {
        texture,
        framebuffer,
        width,
        height,
        egl_image: None,
        egl_client_buffer: None,
        hardware_buffer: None,
    })
}

fn truncate_native_handle(label: &str, raw: usize) -> Result<i32> {
    if raw == 0 {
        bail!("{label} handle is null");
    }
    if raw >> 32 != 0 {
        warn!(
            "{label} handle 0x{raw:016x} exceeds 32 bits; submitting truncated low word 0x{:08x}",
            raw as u32
        );
    }
    Ok(raw as u32 as i32)
}

fn submitted_image_handle(target: &EyeRenderTarget) -> Result<i32> {
    match PIMAX_SUBMITTED_HANDLE_KIND {
        SubmittedImageHandleKind::TextureId => Ok(target.texture as i32),
        SubmittedImageHandleKind::EglImage => {
            let egl_image = target
                .egl_image
                .context("missing EGLImage for kTypeImage submission")?;
            truncate_native_handle("EGLImageKHR", egl_image as usize)
        }
        SubmittedImageHandleKind::EglClientBuffer => {
            let client_buffer = target
                .egl_client_buffer
                .context("missing EGLClientBuffer for kTypeImage submission")?;
            truncate_native_handle("EGLClientBuffer", client_buffer as usize)
        }
        SubmittedImageHandleKind::HardwareBufferPtr => {
            let hardware_buffer = target
                .hardware_buffer
                .as_ref()
                .context("missing AHardwareBuffer for kTypeImage submission")?;
            truncate_native_handle("AHardwareBuffer", hardware_buffer.as_ptr() as usize)
        }
    }
}

fn log_submission_handle_strategy(pair: &EyeBufferPair) {
    let left_handle = submitted_image_handle(&pair.left);
    let right_handle = submitted_image_handle(&pair.right);
    info!(
        "submitting layers as {} via {:?}: left={{tex={}, egl_image={:?}, client_buffer={:?}, ahb={:?}, handle={:?}}} right={{tex={}, egl_image={:?}, client_buffer={:?}, ahb={:?}, handle={:?}}}",
        PIMAX_SUBMITTED_TEXTURE_TYPE,
        PIMAX_SUBMITTED_HANDLE_KIND,
        pair.left.texture,
        pair.left.egl_image.map(|value| value as usize),
        pair.left.egl_client_buffer.map(|value| value as usize),
        pair.left.hardware_buffer.as_ref().map(|value| value.as_ptr() as usize),
        left_handle,
        pair.right.texture,
        pair.right.egl_image.map(|value| value as usize),
        pair.right.egl_client_buffer.map(|value| value as usize),
        pair.right.hardware_buffer.as_ref().map(|value| value.as_ptr() as usize),
        right_handle
    );
}

fn configure_texture_layer_info<'local>(
    env: &mut jni::JNIEnv<'local>,
    layer: &JObject<'local>,
    width: i32,
    height: i32,
    label: &str,
) -> Result<()> {
    let width = width.max(1);
    let height = height.max(1);
    let bytes_per_pixel = 4;
    let mem_size = width.saturating_mul(height).saturating_mul(bytes_per_pixel);
    let vulkan_info = get_object_field(
        env,
        layer,
        "vulkanInfo",
        "Lcom/pimax/pxrapi/PxrApi$sxrVulkanTexInfo;",
    )?;

    set_int_field(env, &vulkan_info, "width", width)?;
    set_int_field(env, &vulkan_info, "height", height)?;
    set_int_field(env, &vulkan_info, "bytesPerPixel", bytes_per_pixel)?;
    set_int_field(env, &vulkan_info, "numMips", 1)?;
    set_int_field(env, &vulkan_info, "memSize", mem_size)?;
    set_int_field(env, &vulkan_info, "renderSemaphore", 0)?;
    info!(
        "configured {label} layer texture metadata: {}x{} bpp={} mem_size={}",
        width, height, bytes_per_pixel, mem_size
    );
    Ok(())
}

fn make_diagnostic_eye_pattern(width: i32, height: i32, left_eye: bool) -> Vec<u8> {
    let width = width.max(1) as usize;
    let height = height.max(1) as usize;
    let mut pixels = vec![0_u8; width * height * 4];
    let denom_x = width.saturating_sub(1).max(1);
    let denom_y = height.saturating_sub(1).max(1);
    let center_x = width / 2;
    let center_y = height / 2;
    let line = (width.min(height) / 180).max(2);
    let border = (width.min(height) / 80).max(6);
    let grid = (width.min(height) / 12).max(96);
    let grid_line = (width.min(height) / 360).max(2);

    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let u = ((x * 255) / denom_x) as u8;
            let v = ((y * 255) / denom_y) as u8;
            let checker = (((x / grid) ^ (y / grid)) & 1) as u8;

            let (mut r, mut g, mut b) = if left_eye {
                (
                    180_u8.saturating_add(u / 4),
                    70_u8.saturating_add(v / 5),
                    28,
                )
            } else {
                (
                    24,
                    130_u8.saturating_add(v / 5),
                    180_u8.saturating_add(u / 4),
                )
            };

            if checker != 0 {
                r = r.saturating_add(26);
                g = g.saturating_add(26);
                b = b.saturating_add(26);
            }

            if x < border || y < border || width - 1 - x < border || height - 1 - y < border {
                r = 255;
                g = 255;
                b = 255;
            }

            if x % grid < grid_line || y % grid < grid_line {
                r = r.saturating_sub(70);
                g = g.saturating_sub(70);
                b = b.saturating_sub(70);
            }

            let on_center_cross = x.abs_diff(center_x) < line || y.abs_diff(center_y) < line;
            let diag_a = ((x * height).abs_diff(y * width)) < width.max(height) * line;
            let diag_b =
                (((width - 1 - x) * height).abs_diff(y * width)) < width.max(height) * line;
            if on_center_cross || diag_a || diag_b {
                r = 255;
                g = 255;
                b = 255;
            }

            pixels[idx] = r;
            pixels[idx + 1] = g;
            pixels[idx + 2] = b;
            pixels[idx + 3] = 255;
        }
    }

    draw_eye_label(&mut pixels, width, height, left_eye);
    pixels
}

fn draw_eye_label(pixels: &mut [u8], width: usize, height: usize, left_eye: bool) {
    let glyph_l = [
        "10000", "10000", "10000", "10000", "10000", "10000", "11111",
    ];
    let glyph_r = [
        "11110", "10001", "10001", "11110", "10100", "10010", "10001",
    ];
    let glyph = if left_eye { &glyph_l } else { &glyph_r };
    let scale = (width.min(height) / 24).max(12);
    let origin_x = width / 10;
    let origin_y = height / 10;

    for (row, bits) in glyph.iter().enumerate() {
        for (col, bit) in bits.as_bytes().iter().enumerate() {
            if *bit == b'1' {
                fill_rect_rgba(
                    pixels,
                    width,
                    height,
                    origin_x + col * scale,
                    origin_y + row * scale,
                    scale.saturating_sub(2).max(1),
                    scale.saturating_sub(2).max(1),
                    [255, 255, 255, 255],
                );
            }
        }
    }
}

fn fill_rect_rgba(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    x0: usize,
    y0: usize,
    rect_width: usize,
    rect_height: usize,
    color: [u8; 4],
) {
    let x1 = x0.saturating_add(rect_width).min(width);
    let y1 = y0.saturating_add(rect_height).min(height);
    for y in y0.min(height)..y1 {
        for x in x0.min(width)..x1 {
            let idx = (y * width + x) * 4;
            pixels[idx..idx + 4].copy_from_slice(&color);
        }
    }
}

fn diagnostic_marker_rect(frame_index: i32, width: i32, height: i32) -> TextureRect {
    let marker_size = (width.min(height) / 8).clamp(96, 320);
    let max_x = (width - marker_size).max(0);
    let max_y = (height - marker_size).max(0);
    let period = 180_i32;
    let half_period = period / 2;
    let phase_x = frame_index.rem_euclid(period);
    let phase_y = (frame_index / 2).rem_euclid(period);
    let bounce_x = if phase_x <= half_period {
        phase_x
    } else {
        period - phase_x
    };
    let bounce_y = if phase_y <= half_period {
        phase_y
    } else {
        period - phase_y
    };

    TextureRect {
        x: ((bounce_x as i64 * max_x as i64) / half_period as i64) as i32,
        y: ((bounce_y as i64 * max_y as i64) / half_period as i64) as i32,
        width: marker_size,
        height: marker_size,
    }
}

fn make_texture_rect_from_base(
    base_pixels: &[u8],
    texture_width: i32,
    texture_height: i32,
    rect: TextureRect,
    marker_color: Option<[u8; 4]>,
) -> Result<Vec<u8>> {
    let texture_width = texture_width.max(1) as usize;
    let texture_height = texture_height.max(1) as usize;
    let x = rect.x.max(0) as usize;
    let y = rect.y.max(0) as usize;
    let rect_width = rect.width.max(1) as usize;
    let rect_height = rect.height.max(1) as usize;
    let expected_len = texture_width
        .saturating_mul(texture_height)
        .saturating_mul(4);
    if base_pixels.len() < expected_len {
        bail!(
            "diagnostic base texture too small: got {} bytes, expected at least {}",
            base_pixels.len(),
            expected_len
        );
    }
    if x + rect_width > texture_width || y + rect_height > texture_height {
        bail!(
            "diagnostic texture rect out of bounds: rect={}x{}+{},{} texture={}x{}",
            rect_width,
            rect_height,
            x,
            y,
            texture_width,
            texture_height
        );
    }

    let mut rect_pixels = vec![0_u8; rect_width * rect_height * 4];
    for row in 0..rect_height {
        let src_start = ((y + row) * texture_width + x) * 4;
        let src_end = src_start + rect_width * 4;
        let dst_start = row * rect_width * 4;
        rect_pixels[dst_start..dst_start + rect_width * 4]
            .copy_from_slice(&base_pixels[src_start..src_end]);
    }

    if let Some(color) = marker_color {
        let border = (rect_width.min(rect_height) / 10).max(4);
        for row in 0..rect_height {
            for col in 0..rect_width {
                let idx = (row * rect_width + col) * 4;
                let draw_border = col < border
                    || row < border
                    || rect_width - 1 - col < border
                    || rect_height - 1 - row < border;
                let marker = if draw_border { [0, 0, 0, 255] } else { color };
                rect_pixels[idx..idx + 4].copy_from_slice(&marker);
            }
        }
    }

    Ok(rect_pixels)
}

fn upload_texture_rect(texture: u32, rect: TextureRect, pixels: &[u8]) {
    unsafe {
        glBindTexture(GL_TEXTURE_2D, texture);
        glTexSubImage2D(
            GL_TEXTURE_2D,
            0,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            pixels.as_ptr().cast(),
        );
        glBindTexture(GL_TEXTURE_2D, 0);
        glFlush();
    }
}

fn upload_diagnostic_rect_from_base(
    target: &EyeRenderTarget,
    base_pixels: &[u8],
    rect: TextureRect,
    marker_color: Option<[u8; 4]>,
) -> Result<()> {
    let rect_pixels =
        make_texture_rect_from_base(base_pixels, target.width, target.height, rect, marker_color)?;
    upload_texture_rect(target.texture, rect, &rect_pixels);
    Ok(())
}

fn update_diagnostic_pattern_marker(
    state: &mut DiagnosticPatternState,
    pair: &EyeBufferPair,
    slot_index: usize,
    frame_index: i32,
) -> Result<()> {
    let marker_rect = diagnostic_marker_rect(frame_index, pair.left.width, pair.left.height);

    if let Some(previous_rect) = state.last_marker_rects[slot_index] {
        upload_diagnostic_rect_from_base(&pair.left, &state.left_base, previous_rect, None)?;
        upload_diagnostic_rect_from_base(&pair.right, &state.right_base, previous_rect, None)?;
    }

    upload_diagnostic_rect_from_base(
        &pair.left,
        &state.left_base,
        marker_rect,
        Some([32, 255, 96, 255]),
    )?;
    upload_diagnostic_rect_from_base(
        &pair.right,
        &state.right_base,
        marker_rect,
        Some([255, 64, 220, 255]),
    )?;
    state.last_marker_rects[slot_index] = Some(marker_rect);

    if frame_index < 5 || frame_index % 120 == 0 {
        info!(
            "updated diagnostic marker: frame_index={frame_index} slot={slot_index} rect={}x{}+{},{}",
            marker_rect.width, marker_rect.height, marker_rect.x, marker_rect.y
        );
    }

    Ok(())
}

fn copy_video_frame_to_target(target: &EyeRenderTarget, frame_data: &[u8]) {
    // Copy video frame data into the GL texture
    // frame_data is expected to be RGBA format
    unsafe {
        let previous_framebuffer = current_framebuffer_binding().max(0) as u32;
        glBindFramebuffer(GL_FRAMEBUFFER, target.framebuffer);
        glViewport(0, 0, target.width, target.height);

        glBindTexture(GL_TEXTURE_2D, target.texture);
        glTexImage2D(
            GL_TEXTURE_2D,
            0,
            GL_RGBA as i32,
            target.width,
            target.height,
            0,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            frame_data.as_ptr().cast(),
        );
        glBindTexture(GL_TEXTURE_2D, 0);
        glFlush();

        glBindFramebuffer(GL_FRAMEBUFFER, previous_framebuffer);
    }
}

fn render_target_clear(target: &EyeRenderTarget, color: [u8; 4]) {
    let red = color[0] as f32 / 255.0;
    let green = color[1] as f32 / 255.0;
    let blue = color[2] as f32 / 255.0;
    let alpha = color[3] as f32 / 255.0;

    unsafe {
        let previous_framebuffer = current_framebuffer_binding().max(0) as u32;
        glBindFramebuffer(GL_FRAMEBUFFER, target.framebuffer);
        glViewport(0, 0, target.width, target.height);
        prepare_solid_color_clear_state();
        glClearColor(red, green, blue, alpha);
        glClear(GL_COLOR_BUFFER_BIT);
        glBindFramebuffer(GL_FRAMEBUFFER, previous_framebuffer);
    }
}

fn prepare_solid_color_clear_state() {
    unsafe {
        glDisable(GL_SCISSOR_TEST);
        glColorMask(
            GL_TRUE_BOOLEAN,
            GL_TRUE_BOOLEAN,
            GL_TRUE_BOOLEAN,
            GL_TRUE_BOOLEAN,
        );
    }
}

struct EyeLumaStats {
    center_pixel: [u8; 4],
    samples: u32,
    average_luma: f32,
    dark_percent: f32,
    bright_percent: f32,
}

fn pixel_luma(pixel: [u8; 4]) -> f32 {
    (0.2126 * pixel[0] as f32) + (0.7152 * pixel[1] as f32) + (0.0722 * pixel[2] as f32)
}

fn readback_eye_luma_stats(target: &EyeRenderTarget) -> EyeLumaStats {
    let sample_steps = [1_i32, 3, 5, 7, 9];
    let mut pixel = [0_u8; 4];
    let mut center_pixel = [0_u8; 4];
    let mut samples = 0_u32;
    let mut dark_samples = 0_u32;
    let mut bright_samples = 0_u32;
    let mut total_luma = 0.0_f32;

    unsafe {
        let previous_framebuffer = current_framebuffer_binding().max(0) as u32;
        glBindFramebuffer(GL_FRAMEBUFFER, target.framebuffer);

        for y_step in sample_steps {
            for x_step in sample_steps {
                let x = ((target.width.max(1) - 1) * x_step / 10).max(0);
                let y = ((target.height.max(1) - 1) * y_step / 10).max(0);
                glReadPixels(
                    x,
                    y,
                    1,
                    1,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    pixel.as_mut_ptr().cast(),
                );
                if x_step == 5 && y_step == 5 {
                    center_pixel = pixel;
                }
                let luma = pixel_luma(pixel);
                total_luma += luma;
                samples += 1;
                if luma < 8.0 {
                    dark_samples += 1;
                }
                if luma > 32.0 {
                    bright_samples += 1;
                }
            }
        }

        glBindFramebuffer(GL_FRAMEBUFFER, previous_framebuffer);
    }

    let sample_count = samples.max(1) as f32;
    EyeLumaStats {
        center_pixel,
        samples,
        average_luma: total_luma / sample_count,
        dark_percent: (dark_samples as f32 * 100.0) / sample_count,
        bright_percent: (bright_samples as f32 * 100.0) / sample_count,
    }
}

fn current_framebuffer_binding() -> i32 {
    let mut framebuffer = 0_i32;
    unsafe {
        glGetIntegerv(GL_FRAMEBUFFER_BINDING, &mut framebuffer as *mut i32);
    }
    framebuffer
}

fn current_gl_error() -> u32 {
    unsafe { glGetError() }
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn scaled_eye_dimension(dimension: i32) -> i32 {
    let scale = PIMAX_EYE_RENDER_SCALE.max(0.01);
    ((dimension.max(1) as f32 * scale).round() as i32).max(1)
}

fn query_pimax_vsync_offset_nanos(env: &mut jni::JNIEnv<'_>, context: &JObject<'_>) -> Result<i64> {
    call_static_long(
        env,
        "com/pimax/pxrapi/PxrApi",
        "getVsyncOffsetNanos",
        "(Landroid/content/Context;)J",
        &[JValue::Object(context)],
    )
    .context("query Pimax vsync offset nanos")
}

fn pump_pimax_vsync(env: &mut jni::JNIEnv<'_>, vsync_offset_nanos: i64) -> Result<i64> {
    let now_ns = call_static_long(env, "java/lang/System", "nanoTime", "()J", &[])
        .context("query System.nanoTime")?;
    let vsync_time_ns = now_ns.saturating_add(vsync_offset_nanos);
    call_static_void(
        env,
        "com/pimax/pxrapi/PxrApi",
        "nativeVsync",
        "(J)V",
        &[JValue::Long(vsync_time_ns)],
    )
    .with_context(|| format!("submit Pimax nativeVsync timestamp {vsync_time_ns}"))?;
    Ok(vsync_time_ns)
}

fn cleanup_pimax_render_session(
    env: &mut jni::JNIEnv<'_>,
    context: &JObject<'_>,
    pvr_service_client: Option<&GlobalRef>,
    display_wake_lock: Option<&GlobalRef>,
) {
    info!("cleaning up Pimax render session");

    if let Some(client) = pvr_service_client {
        match pvr_service_set_display_interrupt_capture(env, client, 0, 0) {
            Ok(result) => {
                info!("PvrServiceClient.SetDisplayInterruptCapture(VSYNC, 0) -> {result}")
            }
            Err(err) => {
                warn!("PvrServiceClient.SetDisplayInterruptCapture(VSYNC, 0) failed: {err:#}")
            }
        }
    } else {
        match call_static_int(
            env,
            "com/pimax/vrservice/PxrServiceApi",
            "SetDisplayInterruptCapture",
            "(II)I",
            &[JValue::Int(0), JValue::Int(0)],
        ) {
            Ok(result) => info!("PxrServiceApi.SetDisplayInterruptCapture(VSYNC, 0) -> {result}"),
            Err(err) => warn!("PxrServiceApi.SetDisplayInterruptCapture(VSYNC, 0) failed: {err:#}"),
        }
    }

    if let Err(err) = call_static_void(
        env,
        "com/pimax/pxrapi/PxrApi",
        "stopVsync",
        "(Landroid/content/Context;)V",
        &[JValue::Object(context)],
    ) {
        warn!("PxrApi.stopVsync failed during cleanup: {err:#}");
    } else {
        info!("PxrApi.stopVsync requested during cleanup");
    }

    if let Err(err) = call_static_void(
        env,
        "com/pimax/pxrapi/PxrApi",
        "enablePresentation",
        "(Landroid/content/Context;Z)V",
        &[JValue::Object(context), JValue::Bool(0)],
    ) {
        warn!("PxrApi.enablePresentation(false) failed during cleanup: {err:#}");
    } else {
        info!("PxrApi.enablePresentation(false) requested during cleanup");
    }

    if let Err(err) = call_static_void(env, "com/pimax/pxrapi/PxrApi", "sxrEndXr", "()V", &[]) {
        warn!("PxrApi.sxrEndXr failed during cleanup: {err:#}");
    } else {
        info!("PxrApi.sxrEndXr succeeded during cleanup");
    }

    if let Err(err) = call_static_void(env, "com/pimax/pxrapi/PxrApi", "sxrShutdown", "()V", &[]) {
        warn!("PxrApi.sxrShutdown() failed during cleanup: {err:#}");
    } else {
        info!("PxrApi.sxrShutdown() succeeded during cleanup");
    }

    if let Some(client) = pvr_service_client {
        match pvr_service_stop_vr_mode(env, client) {
            Ok(result) => info!("PvrServiceClient.StopVRMode() -> {result}"),
            Err(err) => warn!("PvrServiceClient.StopVRMode() failed during cleanup: {err:#}"),
        }
    }

    if let Some(wake_lock) = display_wake_lock {
        if let Err(err) = release_display_wake_lock(env, wake_lock) {
            warn!("failed to release display wake lock during cleanup: {err:#}");
        } else {
            info!("released display wake lock during cleanup");
        }
    }
}

fn refresh_pimax_presentation(
    env: &mut jni::JNIEnv<'_>,
    context: &JObject<'_>,
    pvr_service_client: Option<&GlobalRef>,
    reason: &str,
) {
    info!("refreshing Pimax presentation state: {reason}");

    let mut temp_client_connected = false;

    if let Some(client) = pvr_service_client {
        cycle_pimax_vr_mode_refresh(env, client, "PvrServiceClient");
    } else {
        let temp_client = match create_pvr_service_client(env, context) {
            Ok(client) => Some(client),
            Err(err) => {
                warn!(
                    "failed to create temporary PvrServiceClient for presentation refresh: {err:#}"
                );
                None
            }
        };
        if let Some(client) = temp_client.as_ref() {
            if let Err(err) = connect_pvr_service_client(env, client) {
                warn!("temporary PvrServiceClient.Connect failed during refresh: {err:#}");
            } else {
                temp_client_connected = true;
                match wait_for_pvr_service_interface(env, client, Duration::from_secs(1)) {
                    Ok(()) => {
                        info!("temporary PvrServiceClient connected for presentation refresh")
                    }
                    Err(err) => warn!(
                        "temporary PvrServiceClient did not connect cleanly during refresh: {err:#}"
                    ),
                }
                cycle_pimax_vr_mode_refresh(env, client, "temporary PvrServiceClient");
            }
        }
        match call_static_int(
            env,
            "com/pimax/vrservice/PxrServiceApi",
            "SetDisplayInterruptCapture",
            "(II)I",
            &[JValue::Int(0), JValue::Int(1)],
        ) {
            Ok(result) => info!("PxrServiceApi.SetDisplayInterruptCapture(VSYNC, 1) -> {result}"),
            Err(err) => warn!("PxrServiceApi.SetDisplayInterruptCapture(VSYNC, 1) failed: {err:#}"),
        }

        if temp_client_connected {
            if let Some(client) = temp_client.as_ref() {
                if let Err(err) = disconnect_pvr_service_client(env, client) {
                    warn!("temporary PvrServiceClient.Disconnect failed after refresh: {err:#}");
                } else {
                    info!("temporary PvrServiceClient disconnected after presentation refresh");
                }
            }
        }
    }

    if let Err(err) = call_static_void(
        env,
        "com/pimax/pxrapi/PxrApi",
        "enablePresentation",
        "(Landroid/content/Context;Z)V",
        &[JValue::Object(context), JValue::Bool(1)],
    ) {
        warn!("PxrApi.enablePresentation(true) failed while refreshing presentation: {err:#}");
    } else {
        info!("PxrApi.enablePresentation(true) requested while refreshing presentation");
    }

    if let Err(err) = call_static_void(
        env,
        "com/pimax/pxrapi/PxrApi",
        "startVsync",
        "(Landroid/content/Context;)V",
        &[JValue::Object(context)],
    ) {
        warn!("PxrApi.startVsync failed while refreshing presentation: {err:#}");
    } else {
        info!("PxrApi.startVsync requested while refreshing presentation");
    }
}

fn cycle_pimax_vr_mode_refresh(env: &mut jni::JNIEnv<'_>, client: &GlobalRef, client_label: &str) {
    match pvr_service_set_display_interrupt_capture(env, client, 0, 1) {
        Ok(result) => info!("{client_label}.SetDisplayInterruptCapture(VSYNC, 1) -> {result}"),
        Err(err) => warn!("{client_label}.SetDisplayInterruptCapture(VSYNC, 1) failed: {err:#}"),
    }
    match pvr_service_stop_vr_mode(env, client) {
        Ok(result) => info!("{client_label}.StopVRMode() during refresh -> {result}"),
        Err(err) => warn!("{client_label}.StopVRMode() during refresh failed: {err:#}"),
    }
    thread::sleep(Duration::from_millis(50));
    match pvr_service_start_vr_mode(env, client) {
        Ok(()) => info!("{client_label}.StartVRMode() during refresh succeeded"),
        Err(err) => {
            warn!("{client_label}.StartVRMode() during refresh failed (will try resume): {err:#}");
            match pvr_service_resume_vr_mode(env, client) {
                Ok(result) => info!("{client_label}.ResumeVRMode() during refresh -> {result}"),
                Err(err) => warn!("{client_label}.ResumeVRMode() during refresh failed: {err:#}"),
            }
        }
    }
    for ui_type in ["vr_first_frame_ready", "6dofWarning_low_quality"] {
        match pvr_service_hide_system_ui(env, client, ui_type) {
            Ok(()) => info!("{client_label}.HideSystemUI({ui_type}) during refresh"),
            Err(err) => {
                warn!("{client_label}.HideSystemUI({ui_type}) during refresh failed: {err:#}")
            }
        }
    }
}

fn try_begin_and_submit_frame<'local>(
    env: &mut jni::JNIEnv<'local>,
    context: &JObject<'local>,
    pvr_service_client: Option<&GlobalRef>,
    bootstrap_tracking_mode: i32,
    promotion_tracking_mode: Option<i32>,
    mut pre_vr_activity_window: Option<(JObject<'local>, ndk::native_window::NativeWindow)>,
) -> Result<()> {
    match set_vr_work_mode(env, 1) {
        Ok(true) => info!("PiHalUtils.setVrWorkMode(1) succeeded"),
        Ok(false) => warn!("PiHalUtils.setVrWorkMode(1) returned false"),
        Err(err) => warn!("PiHalUtils.setVrWorkMode(1) failed: {err:#}"),
    }
    configure_activity_window();
    info!("requested NativeActivity headset window flags");
    let display_wake_lock = match create_display_wake_lock(env, context) {
        Ok(wake_lock) => {
            if is_headset_near() {
                if let Err(err) = acquire_display_wake_lock(env, &wake_lock) {
                    warn!("failed to acquire display wake lock: {err:#}");
                } else {
                    info!("acquired display wake lock for headset session");
                }
            } else {
                info!("created display wake lock but left it released until headset is worn");
            }
            Some(wake_lock)
        }
        Err(err) => {
            warn!("failed to create display wake lock: {err:#}");
            None
        }
    };

    match set_pimax_tracking_mode(env, bootstrap_tracking_mode, "pre-begin bootstrap") {
        Ok(()) => info!(
            "requested bootstrap tracking mode {} before sxrBeginXr",
            format_tracking_modes(bootstrap_tracking_mode)
        ),
        Err(err) => warn!(
            "failed to request bootstrap tracking mode {} before sxrBeginXr: {err:#}",
            format_tracking_modes(bootstrap_tracking_mode)
        ),
    }

    if let Some(client) = pvr_service_client {
        match wait_for_pvr_service_interface(env, client, Duration::from_secs(3)) {
            Ok(()) => info!("PvrServiceClient connected to com.pimax.vrservice"),
            Err(err) => {
                warn!("PvrServiceClient connection did not come up before XR begin: {err:#}")
            }
        }

        // Crystal OG is sensitive to runtime ordering. For standalone launch,
        // we need to start a fresh VR mode. When connected to Airlink, ResumeVRMode
        // attaches to the existing session created by the PC.
        match pvr_service_start_vr_mode(env, client) {
            Ok(()) => info!("PvrServiceClient.StartVRMode() succeeded"),
            Err(err) => {
                warn!("PvrServiceClient.StartVRMode() failed (will try resume): {err:#}");
                // Fall back to resume if start fails (e.g., when connected to Airlink)
                match pvr_service_resume_vr_mode(env, client) {
                    Ok(result) => info!("PvrServiceClient.ResumeVRMode() -> {result}"),
                    Err(err) => warn!("PvrServiceClient.ResumeVRMode() also failed: {err:#}"),
                }
            }
        }
        for ui_type in ["vr_first_frame_ready", "6dofWarning_low_quality"] {
            match pvr_service_hide_system_ui(env, client, ui_type) {
                Ok(()) => info!("requested early PvrServiceClient.HideSystemUI({ui_type})"),
                Err(err) => warn!("early PvrServiceClient.HideSystemUI({ui_type}) failed: {err:#}"),
            }
        }
    }

    thread::sleep(Duration::from_millis(250));

    let outer = env
        .new_object("com/pimax/pxrapi/PxrApi", "()V", &[])
        .context("create PxrApi outer instance")?;
    let color_space = enum_constant(
        env,
        "com.pimax.pxrapi.PxrApi$sxrColorSpace",
        "kColorSpaceSRGB",
    )?;
    let perf_level = enum_constant(env, "com.pimax.pxrapi.PxrApi$sxrPerfLevel", "kPerfMaximum")?;
    let warp_type = enum_constant(env, "com.pimax.pxrapi.PxrApi$sxrWarpType", "kSimple")?;
    let (surface, native_window) =
        if let Some((surface, native_window)) = pre_vr_activity_window.take() {
            info!("using pre-captured NativeActivity-backed surface");
            (surface, native_window)
        } else {
            let (surface, native_window) = capture_activity_window_blocking(env)?;
            info!("using NativeActivity-backed surface");
            (surface, native_window)
        };
    let begin_params = env
        .new_object(
            "com/pimax/pxrapi/PxrApi$sxrBeginParams",
            "(Lcom/pimax/pxrapi/PxrApi;Landroid/view/Surface;)V",
            &[JValue::Object(&outer), JValue::Object(&surface)],
        )
        .context("create sxrBeginParams")?;
    set_object_field(
        env,
        &begin_params,
        "colorSpace",
        "Lcom/pimax/pxrapi/PxrApi$sxrColorSpace;",
        &color_space,
    )?;
    set_object_field(
        env,
        &begin_params,
        "cpuPerfLevel",
        "Lcom/pimax/pxrapi/PxrApi$sxrPerfLevel;",
        &perf_level,
    )?;
    set_object_field(
        env,
        &begin_params,
        "gpuPerfLevel",
        "Lcom/pimax/pxrapi/PxrApi$sxrPerfLevel;",
        &perf_level,
    )?;
    // Keep this fixed for Crystal compatibility. Passing our worker-thread TID
    // can destabilize timing, while 0/1 values are accepted by the runtime.
    set_int_field(env, &begin_params, "mainThreadId", 0)?;
    set_int_field(env, &begin_params, "optionFlags", PIMAX_BEGIN_OPTION_FLAGS)?;
    info!("Pimax begin params: option_flags={PIMAX_BEGIN_OPTION_FLAGS}");

    let egl_state = match initialize_window_egl_context(native_window.clone()) {
        Ok(state) => {
            info!("created NativeActivity window EGL context before sxrBeginXr");
            Some(state)
        }
        Err(err) => {
            warn!("failed to create NativeActivity window EGL context before sxrBeginXr: {err:#}");
            match initialize_pbuffer_egl_context() {
                Ok(state) => {
                    warn!(
                        "falling back to pbuffer EGL context before sxrBeginXr; this may still be insufficient on Crystal"
                    );
                    Some(state)
                }
                Err(pbuffer_err) => {
                    warn!(
                        "failed to create fallback pbuffer EGL context before sxrBeginXr: {pbuffer_err:#}"
                    );
                    None
                }
            }
        }
    };

    call_static_void(
        env,
        "com/pimax/pxrapi/PxrApi",
        "sxrBeginXr",
        "(Landroid/content/Context;Lcom/pimax/pxrapi/PxrApi$sxrBeginParams;)V",
        &[JValue::Object(context), JValue::Object(&begin_params)],
    )
    .context("begin Pimax XR session")?;
    info!("Pimax XR session begun");
    if let Some(egl_state) = egl_state.as_ref() {
        info!(
            "keeping pre-begin EGL context current after sxrBeginXr: window_surface={} size={}x{}",
            egl_state.is_window_surface, egl_state.width, egl_state.height
        );
    } else {
        warn!("no EGL context is current after sxrBeginXr");
    }

    if let Err(err) = call_static_void(
        env,
        "com/pimax/pxrapi/PxrApi",
        "startVsync",
        "(Landroid/content/Context;)V",
        &[JValue::Object(context)],
    ) {
        warn!("PxrApi.startVsync after sxrBeginXr failed: {err:#}");
    } else {
        info!("PxrApi.startVsync after sxrBeginXr succeeded");
    }

    let vsync_offset_nanos = if PIMAX_USE_NATIVE_VSYNC_PUMP {
        match query_pimax_vsync_offset_nanos(env, context) {
            Ok(offset) => {
                info!("PxrApi vsync offset nanos: {offset}");
                Some(offset)
            }
            Err(err) => {
                warn!("PxrApi.getVsyncOffsetNanos failed: {err:#}");
                None
            }
        }
    } else {
        info!("synthetic Pimax nativeVsync pump disabled for this run");
        None
    };

    if let Some(client) = pvr_service_client {
        match pvr_service_set_display_interrupt_capture(env, client, 0, 1) {
            Ok(result) => {
                info!("PvrServiceClient.SetDisplayInterruptCapture(VSYNC, 1) -> {result}")
            }
            Err(err) => {
                warn!("PvrServiceClient.SetDisplayInterruptCapture(VSYNC, 1) failed: {err:#}")
            }
        }
    } else {
        match call_static_int(
            env,
            "com/pimax/vrservice/PxrServiceApi",
            "SetDisplayInterruptCapture",
            "(II)I",
            &[JValue::Int(0), JValue::Int(1)],
        ) {
            Ok(result) => info!("PxrServiceApi.SetDisplayInterruptCapture(VSYNC, 1) -> {result}"),
            Err(err) => warn!("PxrServiceApi.SetDisplayInterruptCapture(VSYNC, 1) failed: {err:#}"),
        }
    }

    if PIMAX_CALL_ENABLE_PRESENTATION_AFTER_BEGIN {
        if let Err(err) = call_static_void(
            env,
            "com/pimax/pxrapi/PxrApi",
            "enablePresentation",
            "(Landroid/content/Context;Z)V",
            &[JValue::Object(context), JValue::Bool(1)],
        ) {
            warn!("PxrApi.enablePresentation(true) failed after sxrBeginXr: {err:#}");
        } else {
            info!("PxrApi.enablePresentation(true) requested after sxrBeginXr");
        }
    } else {
        info!("skipping explicit PxrApi.enablePresentation(true) after sxrBeginXr; sxrBeginXr starts presentation internally");
    }

    if let Some(offset) = vsync_offset_nanos {
        if let Err(err) = pump_pimax_vsync(env, offset) {
            warn!("failed to prime Pimax nativeVsync after sxrBeginXr: {err:#}");
        }
    }
    info!("using the pre-begin EGL context for texture-layer submission");

    let device_info = call_static_object(
        env,
        "com/pimax/pxrapi/PxrApi",
        "sxrGetDeviceInfo",
        "()Lcom/pimax/pxrapi/PxrApi$sxrDeviceInfo;",
        &[],
    )
    .context("query device info")?;
    let display_width = get_int_field(env, &device_info, "displayWidthPixels")?;
    let display_height = get_int_field(env, &device_info, "displayHeightPixels")?;
    let eye_width = get_int_field(env, &device_info, "targetEyeWidthPixels")?;
    let eye_height = get_int_field(env, &device_info, "targetEyeHeightPixels")?;
    let display_refresh = get_float_field(env, &device_info, "displayRefreshRateHz")?;
    let fov_x = get_float_field(env, &device_info, "targetFovXRad")?;
    let fov_y = get_float_field(env, &device_info, "targetFovYRad")?;
    let eye_convergence_m =
        get_float_field(env, &device_info, "targetEyeConvergence").unwrap_or(0.0);
    info!("Pimax targetEyeConvergence: {eye_convergence_m:.4} m");
    match call_static_float(
        env,
        "com/pimax/pxrapi/PxrApi",
        "sxrGetInterpupillaryDistance",
        "()F",
        &[],
    )
    .or_else(|_| {
        call_static_float(
            env,
            "com/pimax/pxrapi/PxrApi",
            "pxrGetInterpupillaryDistance",
            "()F",
            &[],
        )
    })
    .or_else(|_| call_method_float(env, &outer, "pxrGetInterpupillaryDistance", "()F", &[]))
    .or_else(|_| call_method_float(env, &outer, "sxrGetInterpupillaryDistance", "()F", &[]))
    {
        Ok(runtime_ipd) => {
            info!("Pimax runtime IPD reported by PxrApi: raw={runtime_ipd:.3}");
            crate::client::update_alvr_ipd_from_pimax(runtime_ipd);
        }
        Err(err) => {
            warn!("PxrApi IPD getter unavailable; falling back to system property: {err:#}");
            // Read IPD from Android system property as fallback
            match read_system_property_float("persist.sys.pmx.ipd") {
                Some(raw_ipd) => {
                    info!("Pimax IPD from system property persist.sys.pmx.ipd: raw={raw_ipd:.3}");
                    crate::client::update_alvr_ipd_from_pimax(raw_ipd);
                }
                None => {
                    warn!(
                        "persist.sys.pmx.ipd system property unavailable; keeping default ALVR IPD"
                    );
                }
            }
        }
    }
    let warp_mesh_type = match get_object_field(
        env,
        &device_info,
        "warpMeshType",
        "Lcom/pimax/pxrapi/PxrApi$sxrWarpMeshType;",
    ) {
        Ok(value) if !value.is_null() => value,
        Ok(_) | Err(_) => enum_constant(
            env,
            "com.pimax.pxrapi.PxrApi$sxrWarpMeshType",
            "kMeshTypeColumsLtoR",
        )
        .context("choose default warp mesh type")?,
    };
    let warp_mesh_type_name: String = env
        .call_method(&warp_mesh_type, "name", "()Ljava/lang/String;", &[])
        .context("get warpMeshType name")?
        .l()
        .context("decode warpMeshType name")
        .and_then(|object| object_to_string(env, object).context("warpMeshType name string"))?;
    info!("Pimax device warp mesh type: {warp_mesh_type_name}");
    let warp_mesh_type_ordinal = env
        .call_method(&warp_mesh_type, "ordinal", "()I", &[])
        .context("get warpMeshType ordinal")?
        .i()
        .context("decode warpMeshType ordinal")?;
    if let Err(err) = env.call_method(
        &outer,
        "setSvrWarpMeshType",
        "(I)V",
        &[JValue::Int(warp_mesh_type_ordinal)],
    ) {
        warn!("failed to call setSvrWarpMeshType: {err:#}");
    } else {
        info!("called setSvrWarpMeshType with ordinal {warp_mesh_type_ordinal}");
    }
    if let Err(err) = set_static_object_field_via_reflection(
        env,
        "com.pimax.pxrapi.PxrApi",
        "warpMeshType",
        &warp_mesh_type,
    ) {
        warn!("failed to set static warpMeshType via reflection: {err:#}");
    }
    info!(
        "Pimax device info: display={}x{} eye={}x{} refresh={}Hz fov=({}, {}) convergence={:.4}m",
        display_width,
        display_height,
        eye_width,
        eye_height,
        display_refresh,
        fov_x,
        fov_y,
        eye_convergence_m
    );
    crate::client::update_alvr_views_config_from_pimax(fov_x, fov_y, eye_width, eye_height);

    let predicted_time_pipelined = call_static_float(
        env,
        "com/pimax/pxrapi/PxrApi",
        "sxrGetPredictedDisplayTimePipelined",
        "(I)F",
        &[JValue::Int(1)],
    )
    .context("query pipelined predicted display time")?;

    if predicted_time_pipelined <= 0.0 {
        warn!(
            "initial pipelined predicted display time is {predicted_time_pipelined}; continuing to submit frames so runtime timing can prime"
        );
    }

    info!("initial pipelined predicted display time(1): {predicted_time_pipelined}");

    let frame_params = env
        .new_object(
            "com/pimax/pxrapi/PxrApi$sxrFrameParams",
            "(Lcom/pimax/pxrapi/PxrApi;)V",
            &[JValue::Object(&outer)],
        )
        .context("create sxrFrameParams")?;
    set_int_field(env, &frame_params, "frameIndex", 0)?;
    set_int_field(env, &frame_params, "minVsyncs", PIMAX_FRAME_MIN_VSYNCS)?;
    set_float_field(env, &frame_params, "fieldOfView", fov_x.max(fov_y))?;
    set_object_field(
        env,
        &frame_params,
        "warpType",
        "Lcom/pimax/pxrapi/PxrApi$sxrWarpType;",
        &warp_type,
    )?;
    set_int_field(env, &frame_params, "frameOptions", PIMAX_FRAME_OPTIONS)?;
    info!(
        "Pimax frame params: explicit_eye_calls={} min_vsyncs={} field_of_view={} frame_options={} layer_flags={}",
        PIMAX_USE_EXPLICIT_EYE_RENDER_CALLS,
        PIMAX_FRAME_MIN_VSYNCS,
        fov_x.max(fov_y),
        PIMAX_FRAME_OPTIONS,
        PIMAX_LAYER_FLAGS
    );

    let render_layers = env
        .get_field(
            &frame_params,
            "renderLayers",
            "[Lcom/pimax/pxrapi/PxrApi$sxrRenderLayer;",
        )
        .context("get renderLayers")?
        .l()
        .context("decode renderLayers array")?;
    let render_layers: JObjectArray<'_> = JObjectArray::from(render_layers);
    let render_layer_count = env
        .get_array_length(&render_layers)
        .context("count render layers")?;
    info!("Pimax render layer count: {render_layer_count}");
    let texture_type = enum_constant(
        env,
        "com.pimax.pxrapi.PxrApi$sxrTextureType",
        PIMAX_SUBMITTED_TEXTURE_TYPE,
    )?;
    let left_eye = enum_constant(env, "com.pimax.pxrapi.PxrApi$sxrWhichEye", "kLeftEye")?;
    let right_eye = enum_constant(env, "com.pimax.pxrapi.PxrApi$sxrWhichEye", "kRightEye")?;
    let left_eye_mask = enum_constant(env, "com.pimax.pxrapi.PxrApi$sxrEyeMask", "kEyeMaskLeft")?;
    let right_eye_mask = enum_constant(env, "com.pimax.pxrapi.PxrApi$sxrEyeMask", "kEyeMaskRight")?;
    for index in 0..render_layer_count {
        let layer = env
            .get_object_array_element(&render_layers, index)
            .with_context(|| format!("get render layer {index}"))?;
        set_int_field(env, &layer, "imageHandle", 0)?;
        set_int_field(env, &layer, "layerFlags", 0)?;
    }
    let uv_maps = if PIMAX_ENABLE_UV_MAP_HACK {
        let left_uv_map = create_identity_uv_map_texture(PIMAX_UV_MAP_SAMPLES)
            .context("create left identity UV map texture")?;
        let right_uv_map = create_identity_uv_map_texture(PIMAX_UV_MAP_SAMPLES)
            .context("create right identity UV map texture")?;
        info!(
            "created identity UV map textures: left(tex={}, size={}x{}) right(tex={}, size={}x{})",
            left_uv_map.texture,
            left_uv_map.size,
            left_uv_map.size,
            right_uv_map.texture,
            right_uv_map.size,
            right_uv_map.size
        );
        Some((left_uv_map, right_uv_map))
    } else {
        info!("UV-map companion texture hack disabled for this run");
        None
    };
    let render_eye_width = scaled_eye_dimension(eye_width);
    let render_eye_height = scaled_eye_dimension(eye_height);
    info!(
        "Pimax eye render scale experiment: device_eye={}x{} render_eye={}x{} scale={}",
        eye_width, eye_height, render_eye_width, render_eye_height, PIMAX_EYE_RENDER_SCALE
    );
    let mut eye_buffers = Vec::new();
    for pair_index in 0..PIMAX_EYE_BUFFER_PAIR_COUNT {
        let left = create_eye_render_target(render_eye_width, render_eye_height)
            .with_context(|| format!("create left eye render target {pair_index}"))?;
        let right = create_eye_render_target(render_eye_width, render_eye_height)
            .with_context(|| format!("create right eye render target {pair_index}"))?;
        info!(
            "created eye render target pair {pair_index}: left(tex={}, fb={}) right(tex={}, fb={}) size={}x{}",
            left.texture,
            left.framebuffer,
            right.texture,
            right.framebuffer,
            render_eye_width,
            render_eye_height
        );
        eye_buffers.push(EyeBufferPair { left, right });
    }
    if let Some(first_pair) = eye_buffers.first() {
        log_submission_handle_strategy(first_pair);
    }
    let mut diagnostic_pattern_state = None;
    if PIMAX_RENDER_DIAGNOSTIC_PATTERN {
        let left_pattern = make_diagnostic_eye_pattern(render_eye_width, render_eye_height, true);
        let right_pattern = make_diagnostic_eye_pattern(render_eye_width, render_eye_height, false);
        for (pair_index, pair) in eye_buffers.iter().enumerate() {
            copy_video_frame_to_target(&pair.left, &left_pattern);
            copy_video_frame_to_target(&pair.right, &right_pattern);
            info!(
                "uploaded diagnostic eye pattern to buffer pair {pair_index}: left_tex={} right_tex={} size={}x{}",
                pair.left.texture, pair.right.texture, render_eye_width, render_eye_height
            );
        }
        unsafe {
            glFinish();
        }
        diagnostic_pattern_state = Some(DiagnosticPatternState {
            left_base: left_pattern,
            right_base: right_pattern,
            last_marker_rects: vec![None; eye_buffers.len()],
        });
    }

    let left_layer = env
        .get_object_array_element(&render_layers, 0)
        .context("get left render layer")?;
    set_int_field(env, &left_layer, "layerFlags", PIMAX_LAYER_FLAGS)?;
    set_object_field(
        env,
        &left_layer,
        "imageType",
        "Lcom/pimax/pxrapi/PxrApi$sxrTextureType;",
        &texture_type,
    )?;
    set_object_field(
        env,
        &left_layer,
        "eyeMask",
        "Lcom/pimax/pxrapi/PxrApi$sxrEyeMask;",
        &left_eye_mask,
    )?;
    let left_coords = get_object_field(
        env,
        &left_layer,
        "imageCoords",
        "Lcom/pimax/pxrapi/PxrApi$sxrLayoutCoords;",
    )?;
    set_simple_layout_coords(env, &left_coords)?;
    if PIMAX_CONFIGURE_TEXTURE_METADATA {
        configure_texture_layer_info(
            env,
            &left_layer,
            render_eye_width,
            render_eye_height,
            "left",
        )?;
    } else {
        info!("leaving left layer texture metadata at Pimax defaults for this run");
    }
    if let Some((left_uv_map, _)) = uv_maps.as_ref() {
        let left_vulkan_info = get_object_field(
            env,
            &left_layer,
            "vulkanInfo",
            "Lcom/pimax/pxrapi/PxrApi$sxrVulkanTexInfo;",
        )?;
        set_int_field(
            env,
            &left_vulkan_info,
            "memSize",
            left_uv_map.texture as i32,
        )?;
        info!(
            "configured left layer companion UV map handle via vulkanInfo.memSize={}",
            left_uv_map.texture
        );
    }

    let right_layer = env
        .get_object_array_element(&render_layers, 1)
        .context("get right render layer")?;
    set_int_field(env, &right_layer, "layerFlags", PIMAX_LAYER_FLAGS)?;
    set_object_field(
        env,
        &right_layer,
        "imageType",
        "Lcom/pimax/pxrapi/PxrApi$sxrTextureType;",
        &texture_type,
    )?;
    set_object_field(
        env,
        &right_layer,
        "eyeMask",
        "Lcom/pimax/pxrapi/PxrApi$sxrEyeMask;",
        &right_eye_mask,
    )?;
    let right_coords = get_object_field(
        env,
        &right_layer,
        "imageCoords",
        "Lcom/pimax/pxrapi/PxrApi$sxrLayoutCoords;",
    )?;
    set_simple_layout_coords(env, &right_coords)?;
    if PIMAX_CONFIGURE_TEXTURE_METADATA {
        configure_texture_layer_info(
            env,
            &right_layer,
            render_eye_width,
            render_eye_height,
            "right",
        )?;
    } else {
        info!("leaving right layer texture metadata at Pimax defaults for this run");
    }
    if let Some((_, right_uv_map)) = uv_maps.as_ref() {
        let right_vulkan_info = get_object_field(
            env,
            &right_layer,
            "vulkanInfo",
            "Lcom/pimax/pxrapi/PxrApi$sxrVulkanTexInfo;",
        )?;
        set_int_field(
            env,
            &right_vulkan_info,
            "memSize",
            right_uv_map.texture as i32,
        )?;
        info!(
            "configured right layer companion UV map handle via vulkanInfo.memSize={}",
            right_uv_map.texture
        );
    }

    let frame_interval = Duration::from_secs_f32(1.0 / display_refresh.max(1.0));
    let video_receiver = crate::video_receiver::get_video_receiver();
    let mut frame_index = 0_i32;
    let mut live_video_frame_count = 0_u64;
    let mut head_tracking_pose_update_count = 0_u64;
    let mut head_tracking_pose_failure_count = 0_u64;
    let mut last_video_timestamps_by_slot = vec![None::<u64>; eye_buffers.len()];
    let mut signalled_first_frame = false;
    let mut promoted_tracking_mode = promotion_tracking_mode.is_none();
    let mut render_window_surface_mirror_enabled = PIMAX_RENDER_WINDOW_SURFACE_MIRROR;
    if !render_window_surface_mirror_enabled {
        info!("NativeActivity surface mirror disabled for VR texture submission run");
    }
    let mut native_controller_runtime = if ENABLE_NATIVE_PIMAX_CONTROLLER_RUNTIME {
        match start_pimax_native_controller_runtime() {
            Ok(runtime) => runtime,
            Err(err) => {
                warn!("native Pimax controller runtime unavailable: {err:#}");
                None
            }
        }
    } else {
        info!("native Pimax controller runtime disabled; using Java/Binder controller runtime");
        None
    };
    let mut controller_runtime = if native_controller_runtime.is_some() {
        info!("skipping Java/Binder controller runtime because native pxrapi controller runtime is active");
        None
    } else {
        match start_pimax_controller_runtime(env, context) {
            Ok(runtime) => runtime,
            Err(err) => {
                warn!("Pimax controller SDK runtime unavailable: {err:#}");
                None
            }
        }
    };
    loop {
        if is_shutdown_requested() {
            info!("Pimax render loop exiting after shutdown request at frame {frame_index}");
            break;
        }
        if take_presentation_refresh_requested() {
            if is_headset_near() {
                if let Some(wake_lock) = display_wake_lock.as_ref() {
                    if let Err(err) = acquire_display_wake_lock(env, wake_lock) {
                        warn!("failed to acquire display wake lock for near-face refresh: {err:#}");
                    } else {
                        info!("acquired display wake lock for near-face refresh");
                    }
                }
                let controller_client = controller_runtime.as_ref().map(|runtime| &runtime.client);
                let presentation_client = controller_client.or(pvr_service_client);
                refresh_pimax_presentation(env, context, presentation_client, "near-face wake");
            } else {
                info!("skipping presentation refresh while headset proximity is far");
            }
        }

        let frame_start = Instant::now();
        let mut pump_ms = 0.0_f64;
        let predicted_ms: f64;
        let pose_ms: f64;
        let render_ms: f64;
        let mirror_ms: f64;
        let finish_ms: f64;
        let submit_ms: f64;
        let mut readback_ms = 0.0_f64;
        let slot_index = (frame_index.rem_euclid(eye_buffers.len() as i32)) as usize;
        let current_buffers = &eye_buffers[slot_index];
        let mut submitted_video_timestamp = None::<u64>;
        if let Some(runtime) = native_controller_runtime.as_mut() {
            poll_pimax_native_controller_runtime(runtime);
        } else if let Some(runtime) = controller_runtime.as_mut() {
            poll_pimax_controller_runtime(env, runtime);
        }
        if let Some(offset) = vsync_offset_nanos {
            let phase_start = Instant::now();
            if let Err(err) = pump_pimax_vsync(env, offset) {
                if frame_index < 5 || frame_index % 120 == 0 {
                    warn!("failed to pump Pimax nativeVsync for frame {frame_index}: {err:#}");
                }
            }
            pump_ms = duration_ms(phase_start.elapsed());
        }
        set_int_field(env, &frame_params, "frameIndex", frame_index)?;
        // kEyeMaskLeft routes to the physical left eye, kEyeMaskRight to the
        // physical right eye (normal SXR naming convention).  Map left buffer to
        // left layer and right buffer to right layer for convergent stereo.
        set_int_field(
            env,
            &left_layer,
            "imageHandle",
            submitted_image_handle(&current_buffers.left)?,
        )?;
        set_int_field(
            env,
            &right_layer,
            "imageHandle",
            submitted_image_handle(&current_buffers.right)?,
        )?;
        let phase_start = Instant::now();
        let predicted_time = call_static_float(
            env,
            "com/pimax/pxrapi/PxrApi",
            "sxrGetPredictedDisplayTimePipelined",
            "(I)F",
            &[JValue::Int(1)],
        )
        .or_else(|_| {
            call_static_float(
                env,
                "com/pimax/pxrapi/PxrApi",
                "sxrGetPredictedDisplayTime",
                "()F",
                &[],
            )
        })
        .with_context(|| format!("query predicted display time for frame {frame_index}"))?;
        predicted_ms = duration_ms(phase_start.elapsed());
        let phase_start = Instant::now();
        let pose_state = call_static_object(
            env,
            "com/pimax/pxrapi/PxrApi",
            "sxrGetPredictedHeadPose",
            "(F)Lcom/pimax/pxrapi/PxrApi$sxrHeadPoseState;",
            &[JValue::Float(predicted_time)],
        )
        .with_context(|| format!("query predicted head pose for frame {frame_index}"))?;
        pose_ms = duration_ms(phase_start.elapsed());
        let pose_state = env.auto_local(pose_state);
        match read_pimax_head_tracking_pose(env, &pose_state) {
            Ok(head_pose) => {
                crate::client::update_head_tracking_pose(
                    head_pose.orientation,
                    head_pose.position,
                    head_pose.fetch_timestamp,
                );
                head_tracking_pose_update_count = head_tracking_pose_update_count.wrapping_add(1);
                if head_tracking_pose_update_count <= 5
                    || head_tracking_pose_update_count % 120 == 0
                {
                    info!(
                        "updated ALVR head tracking pose from Pimax: updates={} frame={frame_index} status={} fetch_ns={} pose_ns={} expected_ns={} position=({:.3},{:.3},{:.3}) orientation=({:.3},{:.3},{:.3},{:.3})",
                        head_tracking_pose_update_count,
                        head_pose.status,
                        head_pose.fetch_timestamp.as_nanos(),
                        head_pose.pose_timestamp.as_nanos(),
                        head_pose.expected_display_timestamp.as_nanos(),
                        head_pose.position.x,
                        head_pose.position.y,
                        head_pose.position.z,
                        head_pose.orientation.x,
                        head_pose.orientation.y,
                        head_pose.orientation.z,
                        head_pose.orientation.w
                    );
                }
            }
            Err(err) => {
                head_tracking_pose_failure_count = head_tracking_pose_failure_count.wrapping_add(1);
                if head_tracking_pose_failure_count <= 5
                    || head_tracking_pose_failure_count % 120 == 0
                {
                    warn!(
                        "failed to update ALVR head tracking pose from Pimax: failures={} frame={frame_index}: {err:#}",
                        head_tracking_pose_failure_count
                    );
                }
            }
        }
        env.set_field(
            &frame_params,
            "headPoseState",
            "Lcom/pimax/pxrapi/PxrApi$sxrHeadPoseState;",
            JValue::Object(&pose_state),
        )
        .with_context(|| format!("set frame headPoseState for frame {frame_index}"))?;

        let pulse = ((frame_index / 24) & 1) != 0;
        let left_color = if pulse {
            [255, 64, 32, 255]
        } else {
            [255, 200, 32, 255]
        };
        let right_color = if pulse {
            [32, 96, 255, 255]
        } else {
            [32, 220, 255, 255]
        };
        let mirror_color = if pulse {
            [255, 32, 96, 255]
        } else {
            [32, 180, 255, 255]
        };
        let phase_start = Instant::now();
        if let Some(video_frame) = crate::video_receiver::get_latest_frame(video_receiver.as_ref())
        {
            if last_video_timestamps_by_slot[slot_index] != Some(video_frame.timestamp_ns) {
                crate::client::report_alvr_compositor_start(Duration::from_nanos(
                    video_frame.timestamp_ns,
                ));
                crate::video_receiver::copy_video_frame_to_target(
                    &current_buffers.left,
                    &video_frame,
                    crate::video_receiver::VideoFrameEye::Left,
                )
                .with_context(|| {
                    format!("copy live video frame to left eye for frame {frame_index}")
                })?;
                crate::video_receiver::copy_video_frame_to_target(
                    &current_buffers.right,
                    &video_frame,
                    crate::video_receiver::VideoFrameEye::Right,
                )
                .with_context(|| {
                    format!("copy live video frame to right eye for frame {frame_index}")
                })?;
                last_video_timestamps_by_slot[slot_index] = Some(video_frame.timestamp_ns);
                submitted_video_timestamp = Some(video_frame.timestamp_ns);
                live_video_frame_count = live_video_frame_count.wrapping_add(1);
                if live_video_frame_count <= 5 || live_video_frame_count % 120 == 0 {
                    info!(
                        "uploaded live video frame {} to eye textures: source={}x{} timestamp_ns={} render_frame={frame_index} slot={slot_index}",
                        live_video_frame_count,
                        video_frame.width,
                        video_frame.height,
                        video_frame.timestamp_ns
                    );
                }
            }
        } else if PIMAX_RENDER_DIAGNOSTIC_PATTERN {
            // Force diagnostic pattern when enabled
            if let Some(diagnostic_state) = diagnostic_pattern_state.as_mut() {
                if PIMAX_ANIMATE_DIAGNOSTIC_PATTERN {
                    update_diagnostic_pattern_marker(
                        diagnostic_state,
                        current_buffers,
                        slot_index,
                        frame_index,
                    )
                    .with_context(|| {
                        format!("update diagnostic pattern marker for frame {frame_index}")
                    })?;
                }
            }
        } else if PIMAX_USE_EXPLICIT_EYE_RENDER_CALLS {
            call_static_void(
                env,
                "com/pimax/pxrapi/PxrApi",
                "sxrBeginEye",
                "(Lcom/pimax/pxrapi/PxrApi$sxrWhichEye;)V",
                &[JValue::Object(&left_eye)],
            )
            .with_context(|| format!("begin left eye for frame {frame_index}"))?;
            render_target_clear(&current_buffers.left, left_color);
            call_static_void(
                env,
                "com/pimax/pxrapi/PxrApi",
                "sxrEndEye",
                "(Lcom/pimax/pxrapi/PxrApi$sxrWhichEye;)V",
                &[JValue::Object(&left_eye)],
            )
            .with_context(|| format!("end left eye for frame {frame_index}"))?;
            call_static_void(
                env,
                "com/pimax/pxrapi/PxrApi",
                "sxrBeginEye",
                "(Lcom/pimax/pxrapi/PxrApi$sxrWhichEye;)V",
                &[JValue::Object(&right_eye)],
            )
            .with_context(|| format!("begin right eye for frame {frame_index}"))?;
            render_target_clear(&current_buffers.right, right_color);
            call_static_void(
                env,
                "com/pimax/pxrapi/PxrApi",
                "sxrEndEye",
                "(Lcom/pimax/pxrapi/PxrApi$sxrWhichEye;)V",
                &[JValue::Object(&right_eye)],
            )
            .with_context(|| format!("end right eye for frame {frame_index}"))?;
        } else {
            render_target_clear(&current_buffers.left, left_color);
            render_target_clear(&current_buffers.right, right_color);
        }
        render_ms = duration_ms(phase_start.elapsed());

        let phase_start = Instant::now();
        if render_window_surface_mirror_enabled {
            if let Err(err) =
                render_window_surface_mirror(egl_state.as_ref(), frame_index, mirror_color)
            {
                render_window_surface_mirror_enabled = false;
                warn!("disabled NativeActivity surface mirror after frame {frame_index} failure: {err:#}");
            }
        }
        mirror_ms = duration_ms(phase_start.elapsed());

        let phase_start = Instant::now();
        if PIMAX_FORCE_GL_FINISH_BEFORE_SUBMIT {
            unsafe {
                glFinish();
            }
        } else {
            unsafe {
                glFlush();
            }
        }
        finish_ms = duration_ms(phase_start.elapsed());
        let phase_start = Instant::now();
        call_static_void(
            env,
            "com/pimax/pxrapi/PxrApi",
            "sxrSubmitFrame",
            "(Landroid/content/Context;Lcom/pimax/pxrapi/PxrApi$sxrFrameParams;)V",
            &[JValue::Object(context), JValue::Object(&frame_params)],
        )
        .with_context(|| format!("submit Pimax frame {frame_index}"))?;
        submit_ms = duration_ms(phase_start.elapsed());
        if let Some(timestamp_ns) = submitted_video_timestamp {
            crate::client::report_alvr_frame_submitted(
                Duration::from_nanos(timestamp_ns),
                frame_interval.saturating_sub(frame_start.elapsed()),
            );
        }

        if !signalled_first_frame {
            if predicted_time > 0.0 {
                if !promoted_tracking_mode {
                    if let Some(target_mode) = promotion_tracking_mode {
                        match set_pimax_tracking_mode(env, target_mode, "first-valid-frame promotion")
                        {
                            Ok(()) => info!(
                                "promoted tracking mode to {} after valid predicted time",
                                format_tracking_modes(target_mode)
                            ),
                            Err(err) => warn!(
                                "failed to promote tracking mode to {} after valid predicted time: {err:#}",
                                format_tracking_modes(target_mode)
                            ),
                        }
                    }
                    promoted_tracking_mode = true;
                }

                if let Err(err) = call_static_void(
                    env,
                    "com/pimax/pxrapi/PxrApi",
                    "firstPresentationFrameComplete",
                    "(Landroid/content/Context;)V",
                    &[JValue::Object(context)],
                ) {
                    warn!("PxrApi.firstPresentationFrameComplete failed: {err:#}");
                } else {
                    info!(
                        "PxrApi.firstPresentationFrameComplete succeeded at frame {frame_index} with predicted_time={predicted_time:.3}"
                    );
                }
                if let Some(client) = pvr_service_client {
                    for ui_type in ["vr_first_frame_ready", "6dofWarning_low_quality"] {
                        match pvr_service_hide_system_ui(env, client, ui_type) {
                            Ok(()) => info!("requested PvrServiceClient.HideSystemUI({ui_type})"),
                            Err(err) => {
                                warn!("PvrServiceClient.HideSystemUI({ui_type}) failed: {err:#}")
                            }
                        }
                    }
                }
                signalled_first_frame = true;
            } else if frame_index < 5 || frame_index % 120 == 0 {
                info!(
                    "deferring firstPresentationFrameComplete until predicted time is valid (frame={frame_index} predicted_time={predicted_time:.3})"
                );
            }
        }

        if frame_index >= 0 && frame_index % 30 == 0 {
            if !is_headset_near() {
                if let Some(wake_lock) = display_wake_lock.as_ref() {
                    if let Err(err) = release_display_wake_lock(env, wake_lock) {
                        warn!("failed to release display wake lock while off-head: {err:#}");
                    } else if frame_index % 720 == 0 {
                        info!(
                            "display wake lock remains released while headset proximity is far at frame {frame_index}"
                        );
                    }
                }
            } else {
                match is_power_interactive(env, context) {
                    Ok(true) => {
                        if frame_index % 720 == 0 {
                            info!("display power is still interactive at frame {frame_index}");
                        }
                    }
                    Ok(false) => {
                        warn!(
                            "display power became non-interactive at frame {frame_index}; headset is near, re-acquiring wake lock"
                        );
                        if let Some(wake_lock) = display_wake_lock.as_ref() {
                            if let Err(err) = acquire_display_wake_lock(env, wake_lock) {
                                warn!("failed to reacquire display wake lock: {err:#}");
                            }
                        }
                    }
                    Err(err) => warn!(
                        "failed to query display interactive state at frame {frame_index}: {err:#}"
                    ),
                }
            }
        }
        if frame_index > 0 && frame_index % PIMAX_HIDE_SYSTEM_UI_EVERY_N_FRAMES == 0 {
            if let Some(client) = pvr_service_client {
                for ui_type in ["vr_first_frame_ready", "6dofWarning_low_quality"] {
                    if let Err(err) = pvr_service_hide_system_ui(env, client, ui_type) {
                        warn!(
                            "periodic PvrServiceClient.HideSystemUI({ui_type}) failed at frame {frame_index}: {err:#}"
                        );
                    }
                }
            }
        }

        let should_log_frame = frame_index < 10
            || frame_start.elapsed() > Duration::from_millis(50)
            || frame_index % 120 == 0;
        if frame_index < 5 || frame_index % 120 == 0 {
            let phase_start = Instant::now();
            let framebuffer = current_framebuffer_binding();
            let gl_error = current_gl_error();
            let left_stats = readback_eye_luma_stats(&current_buffers.left);
            let right_stats = readback_eye_luma_stats(&current_buffers.right);
            readback_ms = duration_ms(phase_start.elapsed());
            info!(
                "Pimax texture frame submitted: frame_index={frame_index} predicted_time={predicted_time:.3} fb={} left_tex={} right_tex={} left_pixel={:?} right_pixel={:?} left_luma_avg={:.1} left_dark_pct={:.1} left_bright_pct={:.1} right_luma_avg={:.1} right_dark_pct={:.1} right_bright_pct={:.1} samples={} gl_error=0x{gl_error:04x}",
                framebuffer,
                current_buffers.left.texture,
                current_buffers.right.texture,
                left_stats.center_pixel,
                right_stats.center_pixel,
                left_stats.average_luma,
                left_stats.dark_percent,
                left_stats.bright_percent,
                right_stats.average_luma,
                right_stats.dark_percent,
                right_stats.bright_percent,
                left_stats.samples.min(right_stats.samples)
            );
        }
        if should_log_frame {
            info!(
                "Pimax frame timings: frame_index={frame_index} predicted_time={predicted_time:.3} total_ms={:.3} pump_ms={pump_ms:.3} predict_ms={predicted_ms:.3} pose_ms={pose_ms:.3} render_ms={render_ms:.3} mirror_ms={mirror_ms:.3} finish_ms={finish_ms:.3} submit_ms={submit_ms:.3} readback_ms={readback_ms:.3}",
                duration_ms(frame_start.elapsed())
            );
        }

        frame_index = frame_index.wrapping_add(1);
        thread::sleep(frame_interval);
    }

    if let Some(runtime) = native_controller_runtime.take() {
        stop_pimax_native_controller_runtime(runtime);
    }
    if let Some(runtime) = controller_runtime.take() {
        stop_pimax_controller_runtime(env, runtime);
    }
    cleanup_pimax_render_session(env, context, pvr_service_client, display_wake_lock.as_ref());
    Ok(())
}

pub fn probe() -> PimaxProbeReport {
    reset_shutdown_requested();
    // Setup signal handlers for graceful shutdown
    unsafe {
        libc::signal(libc::SIGINT, signal_handler as *const () as usize);
        libc::signal(libc::SIGTERM, signal_handler as *const () as usize);
    }

    let mut report = PimaxProbeReport::default();

    let android_context = ndk_context::android_context();
    let vm = match unsafe { JavaVM::from_raw(android_context.vm().cast()) } {
        Ok(vm) => vm,
        Err(err) => {
            error!("unable to wrap Android JavaVM: {err}");
            return report;
        }
    };

    let context = unsafe { JObject::from_raw(android_context.context() as jni::sys::jobject) };
    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(err) => {
            error!("unable to attach JNI thread: {err}");
            return report;
        }
    };

    info!("probing Pimax XR runtime via com.pimax.pxrapi.PxrApi");

    let application_context = match get_application_context(&mut env, &context) {
        Ok(application_context) => {
            info!("using application context for PxrApi service lifecycle calls");
            Some(application_context)
        }
        Err(err) => {
            warn!("failed to get application context; using NativeActivity context for PxrApi service lifecycle calls: {err:#}");
            None
        }
    };
    let pxr_context = application_context.as_ref().unwrap_or(&context);

    match set_vr_work_mode(&mut env, 1) {
        Ok(true) => info!("PiHalUtils.setVrWorkMode(1) succeeded during probe"),
        Ok(false) => warn!("PiHalUtils.setVrWorkMode(1) returned false during probe"),
        Err(err) => warn!("PiHalUtils.setVrWorkMode(1) failed during probe: {err:#}"),
    }
    configure_activity_window();
    info!("requested NativeActivity headset window flags during probe");

    if let Err(err) = call_static_void(
        &mut env,
        "com/pimax/pxrapi/PxrApi",
        "sxrInitialize",
        "(Landroid/content/Context;)V",
        &[JValue::Object(pxr_context)],
    ) {
        warn!("PxrApi.sxrInitialize failed: {err:#}");
    } else {
        info!("PxrApi.sxrInitialize succeeded");
    }
    if let Err(err) = dump_matching_methods(
        &mut env,
        "com.pimax.pxrapi.PxrApi",
        &[
            "sxrSetXr",
            "sxrEnd",
            "Shutdown",
            "Destroy",
            "Release",
            "Disconnect",
            "Surface",
            "Presentation",
            "Vsync",
            "CoreHandle",
            "Ipd",
            "Interpupillary",
        ],
    ) {
        warn!("failed to dump matching PxrApi methods: {err:#}");
    }
    let default_controller_service = if PIMAX_VERBOSE_STARTUP_INTROSPECTION {
        if let Err(err) = dump_matching_methods(
            &mut env,
            "com.pimax.pxrapi.PxrApi",
            &["Image", "Texture", "Vulkan"],
        ) {
            warn!("failed to dump image/texture/vulkan PxrApi methods: {err:#}");
        }
        if let Err(err) = dump_matching_methods(
            &mut env,
            "com.pimax.vrservice.PxrServiceApi",
            &["DisplayInterrupt", "VRMode", "TrackingMode"],
        ) {
            warn!("failed to dump matching PxrServiceApi methods: {err:#}");
        }
        let controller_keywords = &[
            "Controller",
            "controller",
            "Input",
            "input",
            "Button",
            "button",
            "Key",
            "key",
            "Trigger",
            "trigger",
            "Grip",
            "grip",
            "Joystick",
            "joystick",
            "Thumb",
            "thumb",
            "Stick",
            "stick",
            "Pose",
            "pose",
            "Tracking",
            "tracking",
            "Query",
            "query",
            "State",
            "state",
            "Haptic",
            "haptic",
            "Vibrator",
            "vibrator",
        ];
        info!("probing Pimax controller/input SDK surface");
        for class_name in [
            "com.pimax.pxrapi.PxrApi",
            "com.pimax.vrservice.PxrServiceApi",
            "com.pimax.pxrapi.PvrServiceClient",
            "com.pimax.vrservice.IPvrServiceInterface",
        ] {
            if let Err(err) = dump_matching_methods(&mut env, class_name, controller_keywords) {
                warn!("failed to dump controller/input methods for {class_name}: {err:#}");
            }
            if let Err(err) = dump_matching_fields(&mut env, class_name, controller_keywords) {
                warn!("failed to dump controller/input fields for {class_name}: {err:#}");
            }
            if let Err(err) = dump_declared_inner_classes(&mut env, class_name) {
                warn!("failed to dump inner classes for {class_name}: {err:#}");
            }
        }
        for class_name in [
            "com.pimax.pxrapi.controller.ControllerStartInfo",
            "com.pimax.pxrapi.controller.ControllerFd",
            "com.pimax.pvrapi.controllers.IControllerInterfaceCallback",
        ] {
            if let Err(err) = dump_class_schema(&mut env, class_name) {
                warn!("failed to dump controller class schema for {class_name}: {err:#}");
            }
            if let Err(err) = dump_all_methods(&mut env, class_name) {
                warn!("failed to dump controller class methods for {class_name}: {err:#}");
            }
        }
        if let Err(err) = dump_enum_constants(&mut env, "com.pimax.pxrapi.PxrApi$sxrTextureType") {
            warn!("failed to dump sxrTextureType constants: {err:#}");
        }
        if let Err(err) = dump_matching_fields(
            &mut env,
            "com.pimax.pxrapi.PxrApi$sxrRenderLayer",
            &["image", "buffer", "vulkan", "gl"],
        ) {
            warn!("failed to dump matching sxrRenderLayer fields: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrRenderLayer") {
            warn!("failed to dump sxrRenderLayer schema: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrDeviceInfo") {
            warn!("failed to dump sxrDeviceInfo schema: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrFrameParams") {
            warn!("failed to dump sxrFrameParams schema: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrHeadPoseState") {
            warn!("failed to dump sxrHeadPoseState schema: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrHeadPose") {
            warn!("failed to dump sxrHeadPose schema: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrVector3") {
            warn!("failed to dump sxrVector3 schema: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrQuaternion") {
            warn!("failed to dump sxrQuaternion schema: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrLayoutCoords") {
            warn!("failed to dump sxrLayoutCoords schema: {err:#}");
        }
        if let Err(err) = dump_class_schema(&mut env, "com.pimax.pxrapi.PxrApi$sxrVulkanTexInfo") {
            warn!("failed to dump sxrVulkanTexInfo schema: {err:#}");
        }
        probe_pvr_controller_defaults(&mut env)
    } else {
        info!("skipping verbose Pimax startup introspection to keep launch responsive");
        None
    };

    report.pxr_version = call_static_string(
        &mut env,
        "com/pimax/pxrapi/PxrApi",
        "sxrGetVersion",
        "()Ljava/lang/String;",
        &[],
    )
    .ok();
    report.pxr_client_version = call_static_string(
        &mut env,
        "com/pimax/pxrapi/PxrApi",
        "sxrGetXrClientVersion",
        "()Ljava/lang/String;",
        &[],
    )
    .ok();
    report.pxr_service_version = call_static_string(
        &mut env,
        "com/pimax/pxrapi/PxrApi",
        "sxrGetXrServiceVersion",
        "()Ljava/lang/String;",
        &[],
    )
    .ok();

    report.supported_tracking_modes = call_static_int(
        &mut env,
        "com/pimax/pxrapi/PxrApi",
        "sxrGetSupportedTrackingModes",
        "()I",
        &[],
    )
    .ok();
    report.current_tracking_mode = call_static_int(
        &mut env,
        "com/pimax/pxrapi/PxrApi",
        "sxrGetTrackingMode",
        "()I",
        &[],
    )
    .ok();

    if let Some(bits) = report.supported_tracking_modes {
        info!(
            "PxrApi supported tracking modes: {}",
            format_tracking_modes(bits)
        );
    }
    if let Some(mode) = report.current_tracking_mode {
        info!(
            "PxrApi current tracking mode: {}",
            format_tracking_modes(mode)
        );
    }
    if let Err(err) =
        probe_pvr_controller_client(&mut env, pxr_context, default_controller_service.as_deref())
    {
        warn!("PvrServiceClient controller probe failed: {err:#}");
    }

    let bootstrap_tracking_mode = select_bootstrap_tracking_mode(report.supported_tracking_modes);
    let promotion_tracking_mode =
        select_promotion_tracking_mode(report.supported_tracking_modes, bootstrap_tracking_mode);
    match set_pimax_tracking_mode(&mut env, bootstrap_tracking_mode, "probe bootstrap") {
        Ok(()) => {
            report.set_tracking_mode_result = Some(bootstrap_tracking_mode);
            report.current_tracking_mode_after_set = call_static_int(
                &mut env,
                "com/pimax/pxrapi/PxrApi",
                "sxrGetTrackingMode",
                "()I",
                &[],
            )
            .ok();
        }
        Err(err) => warn!(
            "failed to set bootstrap tracking mode {} during probe: {err:#}",
            format_tracking_modes(bootstrap_tracking_mode)
        ),
    }
    info!(
        "requested bootstrap tracking mode {} -> {:?}",
        format_tracking_modes(bootstrap_tracking_mode),
        report.set_tracking_mode_result
    );
    if let Some(target_mode) = promotion_tracking_mode {
        info!(
            "will promote tracking mode to {} after first valid predicted frame time",
            format_tracking_modes(target_mode)
        );
    }

    info!("probing Pimax service-level VR state via com.pimax.vrservice.PxrServiceApi");

    report.service_current_tracking_mode = call_static_int(
        &mut env,
        "com/pimax/vrservice/PxrServiceApi",
        "GetCurrentTrackingMode",
        "()I",
        &[],
    )
    .ok();
    report.service_supported_tracking_modes = call_static_int(
        &mut env,
        "com/pimax/vrservice/PxrServiceApi",
        "GetSupportTrackingModes",
        "()I",
        &[],
    )
    .ok();
    report.vr_mode = call_static_int(
        &mut env,
        "com/pimax/vrservice/PxrServiceApi",
        "GetVRMode",
        "()I",
        &[],
    )
    .ok();
    let pvr_service_client = if PIMAX_USE_EXPLICIT_PVR_SERVICE_CLIENT {
        match create_pvr_service_client(&mut env, pxr_context) {
            Ok(client) => {
                if let Err(err) = connect_pvr_service_client(&mut env, &client) {
                    warn!("PvrServiceClient.Connect failed: {err:#}");
                } else {
                    info!("PvrServiceClient.Connect requested");
                }
                Some(client)
            }
            Err(err) => {
                warn!("unable to construct PvrServiceClient: {err:#}");
                None
            }
        }
    } else {
        info!("skipping explicit PvrServiceClient.Connect; PxrApi owns VR mode lifecycle");
        None
    };
    let pre_vr_activity_window = match capture_activity_window(&mut env, Duration::from_secs(8)) {
        Ok(window) => {
            info!("captured NativeActivity surface before entering VR mode");
            Some(window)
        }
        Err(err) => {
            warn!("failed to capture NativeActivity surface before entering VR mode: {err:#}");
            None
        }
    };
    if let Some(vr_mode) = report.vr_mode {
        info!("Pimax VR mode before start attempt: {vr_mode}");
    }
    if let Some(bits) = report.service_supported_tracking_modes {
        info!(
            "PxrServiceApi supported tracking modes: {}",
            format_tracking_modes(bits)
        );
    }
    if let Some(mode) = report.service_current_tracking_mode {
        info!(
            "PxrServiceApi current tracking mode: {}",
            format_tracking_modes(mode)
        );
    }
    info!("Pimax VR mode initialization complete, calling sxrBeginXr");

    thread::sleep(Duration::from_millis(500));

    if let Err(err) = try_begin_and_submit_frame(
        &mut env,
        pxr_context,
        pvr_service_client.as_ref(),
        bootstrap_tracking_mode,
        promotion_tracking_mode,
        pre_vr_activity_window,
    ) {
        warn!("Pimax frame submission probe failed: {err:#}");
    }

    if let Some(client) = pvr_service_client.as_ref() {
        if let Err(err) = disconnect_pvr_service_client(&mut env, client) {
            warn!("PvrServiceClient.Disconnect failed: {err:#}");
        }
    }

    info!("Pimax probe summary: {}", report.summary());
    if is_shutdown_requested() {
        info!("shutdown requested after Pimax cleanup; exiting process to stop background ALVR threads");
        std::process::exit(0);
    }

    report
}
