//! Android Entry Point for Pimax ALVR Client
//!
//! # Overview
//!
//! This module is the main entry point for the Pimax ALVR client when running
//! on Android (Pimax Crystal headset). It orchestrates the entire application
//! lifecycle from startup to shutdown.
//!
//! # Application Lifecycle
//!
//! ```text
//! main() (via ndk_glue)
//!     │
//!     ▼
//! run()
//!     │
//!     ├── Init logging (Android logcat)
//!     │
//!     ├── Set panic hook (log panics to logcat)
//!     │
//!     ▼
//! run_inner()
//!     │
//!     ├── tune::init() ──────────────── Start HTTP settings server (:7878)
//!     │
//!     ├── Load/create config ────────── /sdcard/Android/data/.../client.json
//!     │
//!     ├── Optionally start debug RGBA ─ Port 9950 (diagnostic only)
//!     │
//!     ├── Start ALVR control listener ─ Port 9943 (server callback)
//!     │
//!     ├── Load server IP from config
//!     │
//!     ├── mDNS discovery ────────────── Advertise _alvr._tcp.local.
//!     │
//!     ├── pimax::probe() ────────────── Initialize Pimax XR, start render loop
//!     │
//!     ▼
//! [Render loop running in pimax module]
//!     │
//!     │── Receive video from ALVR
//!     │── Decode via MediaCodec
//!     │── Blit to eye textures
//!     │── Send head tracking
//!     │
//!     ▼
//! [On shutdown]
//!     │
//!     └── Cleanup Pimax XR session
//! ```
//!
//! # Startup Sequence
//!
//! 1. **Logging**: Initialize android_logger with tag "PimaxALVR"
//! 2. **Panic Hook**: Capture Rust panics to logcat for debugging
//! 3. **Tuning Server**: Start HTTP server on port 7878 for browser-based settings
//! 4. **Config**: Load or create client.json with identity and settings
//! 5. **Debug Receiver**: Optional RGBA TCP receiver (port 9950), disabled by default
//! 6. **ALVR Listener**: Start TCP listener for server callback (port 9943)
//! 7. **Discovery**: Advertise client presence via mDNS
//! 8. **Pimax Probe**: Initialize Pimax XR and enter render loop
//!
//! # Server Connection Strategy
//!
//! The client currently uses a passive callback/listener approach for connecting
//! to the ALVR server:
//!
//! ## Passive Discovery
//!
//! - Advertises `_alvr._tcp.local.` via mDNS
//! - Waits for server to connect back (TCP to port 9943)
//! - Works automatically when server is on same network
//!
//! # Error Handling
//!
//! On critical error, the app:
//! 1. Logs the error to logcat
//! 2. Enters infinite loop (30s sleep) to prevent restart loop
//! 3. Allows manual intervention via logcat inspection
//!
//! This is intentional: a crash loop would make debugging impossible.
//!
//! # Threading
//!
//! - **Main Thread**: Runs the Pimax render loop (time-critical)
//! - **Tokio Runtime**: Single-threaded runtime for async ALVR operations
//! - **Background Threads**: HTTP server, TCP listener, video receiver
//!
//! # Configuration
//!
//! Config is stored at:
//! `/sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json`
//!
//! Accessible via ADB:
//! ```bash
//! adb shell cat /sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json
//! ```

use std::{sync::Once, thread, time::Duration};

use android_logger::Config as AndroidLoggerConfig;
use anyhow::{Context, Result};
use log::{error, info, warn, LevelFilter};

use crate::tune::set_server_status;
use crate::{config, AlvrClient, ClientConfig};

/// Enables the temporary raw-RGBA TCP frame ingress on port 9950.
///
/// Keep disabled for normal headset runs. It was useful while validating the
/// compositor upload path, but the real ALVR stream should be the only active
/// video ingress unless we are deliberately running that diagnostic.
const ENABLE_DEBUG_RGBA_TCP_RECEIVER: bool = false;

/// Logger initialization guard.
///
/// Ensures logging is initialized exactly once, even if run() is called
/// multiple times (shouldn't happen, but Once protects against it).
static LOGGER: Once = Once::new();

pub fn run() {
    init_logging();
    std::panic::set_hook(Box::new(|panic_info| {
        error!("panic: {panic_info}");
    }));

    if let Err(err) = run_inner() {
        error!("android client exited with error: {err:#}");
        loop {
            thread::sleep(Duration::from_secs(30));
        }
    }
}

fn init_logging() {
    LOGGER.call_once(|| {
        android_logger::init_once(
            AndroidLoggerConfig::default()
                .with_max_level(LevelFilter::Info)
                .with_tag("PimaxALVR"),
        );
    });
}

fn run_inner() -> Result<()> {
    info!("starting Pimax Crystal OG native Pimax runtime probe");

    // Start runtime tuning HTTP server — connect from browser at http://<headset-ip>:7878/
    crate::tune::init(
        crate::video_receiver::PIMAX_BLIT_CONVERGENCE_SHIFT_NDC_DEFAULT,
        crate::client::ALVR_IPD_SCALE_DEFAULT,
        crate::video_receiver::COLOR_BLACK_CRUSH_DEFAULT,
        crate::video_receiver::COLOR_GAIN_DEFAULT,
    );

    let config_path = config::default_config_path();
    let mut config = ClientConfig::load_or_create(&config_path)
        .with_context(|| format!("load config from {}", config_path.display()))?;
    config.ensure_fresh_identity();
    config
        .save(&config_path)
        .with_context(|| format!("save config to {}", config_path.display()))?;

    info!(
        "client={}, discovery_port={}, stream_port={}, config={}",
        config.client_name,
        config.discovery_port,
        config.stream_port,
        config_path.display()
    );

    if ENABLE_DEBUG_RGBA_TCP_RECEIVER {
        let video_receiver = crate::video_receiver::get_video_receiver();
        match crate::video_receiver::start_debug_rgba_tcp_receiver(
            video_receiver,
            crate::video_receiver::DEBUG_RGBA_STREAM_PORT,
        ) {
            Ok(()) => info!(
                "debug RGBA TCP frame receiver ready on port {}",
                crate::video_receiver::DEBUG_RGBA_STREAM_PORT
            ),
            Err(err) => warn!("debug RGBA TCP frame receiver unavailable: {err:#}"),
        }
    } else {
        info!("debug RGBA TCP frame receiver disabled for normal ALVR startup");
    }

    match crate::client::start_alvr_control_listener(config.clone()) {
        Ok(()) => info!("ALVR control listener ready for server callback"),
        Err(err) => warn!("ALVR control listener unavailable: {err:#}"),
    }

    // Get the configured server IP for directed announcement
    let server_ip = crate::tune::get_server_ip();
    info!("configured server IP: {}", server_ip);

    // Spawn mDNS discovery thread.
    //
    // Registers _alvr._tcp.local. via mDNS on first successful call.
    // The ServiceDaemon re-announces automatically; subsequent loop iterations
    // are no-ops unless the first registration failed (e.g. WiFi not yet up).
    {
        let discovery_config = config.clone();
        thread::Builder::new()
            .name("alvr-discovery".to_string())
            .spawn(move || {
                let discovery_client = AlvrClient::new(discovery_config);
                let mut iteration = 0_u64;
                loop {
                    if let Err(err) = discovery_client.announce() {
                        warn!("mDNS announce #{iteration} failed: {err:#}");
                    }
                    iteration = iteration.wrapping_add(1);
                    thread::sleep(Duration::from_secs(5));
                }
            })
            .context("spawn ALVR discovery thread")?;
    }

    set_server_status(format!("Waiting for server at {}", server_ip));

    info!("entering Pimax runtime probe");
    let probe = crate::pimax::probe();
    info!("Pimax probe completed: {}", probe.summary());

    Ok(())
}
