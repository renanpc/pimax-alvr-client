/// Runtime tuning parameters and settings UI for the Pimax ALVR client.
///
/// # Architecture
///
/// This module provides a browser-based settings interface accessible at `http://<headset-ip>:7878/`.
/// The settings are adjustable in real-time while the VR application is running, allowing for
/// immediate visual feedback when tuning parameters.
///
/// ## Thread Safety Model
///
/// All tuning values are stored as `AtomicU32` (with f32 bits stored via `to_bits()/from_bits()`).
/// This design allows:
/// - **Lock-free reads** from the render thread (critical for maintaining 90fps+)
/// - **Simple writes** from the HTTP server thread
/// - No mutex contention between the render loop and settings updates
///
/// The HTTP server runs on a separate thread and processes incoming browser requests.
/// When a slider changes, the new value is stored atomically and immediately visible
/// to the render thread on the next frame.
///
/// ## Persistence
///
/// Settings are persisted to `/sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json`.
/// On startup, the last saved values are loaded. If no config exists, defaults are used.
///
/// # Usage
///
/// 1. Put on the headset
/// 2. Note the IP address shown in the ALVR dashboard (e.g., 192.168.0.213)
/// 3. Open `http://<headset-ip>:7878/` in any browser on the same Wi-Fi network
/// 4. Adjust sliders and see changes immediately
/// 5. Settings are auto-saved and restored on next launch
use std::{
    io::{BufRead, BufReader, Write},
    net::{IpAddr, TcpListener},
    sync::{
        atomic::{AtomicU32, Ordering},
        LazyLock, Mutex,
    },
    thread,
};

use log::{info, warn};

// =============================================================================
// Atomic Storage for Tuning Parameters
// =============================================================================
//
// These statics hold the current tuning values as atomics. The render thread
// reads these every frame, so lock-free access is critical for performance.
//
// Why AtomicU32 instead of AtomicF32?
// - std::sync::atomic doesn't have AtomicF32
// - We store f32 bits as u32 and convert on read/write
// - Ordering::Relaxed is sufficient because:
//   - Only one thread writes (HTTP server)
//   - Render thread just needs eventual consistency
//   - No ordering requirements with other operations

/// Convergence shift in NDC (Normalized Device Coordinates) units.
///
/// This corrects for the Pimax headset's built-in divergent warp in the compositor.
/// The Pimax hardware applies approximately 0.248 NDC of divergent warp per eye,
/// which causes double vision when receiving stereo content from ALVR.
///
/// By pre-shifting the blit output convergently (left eye +shift, right eye -shift),
/// we cancel out the compositor's warp, resulting in properly aligned stereo.
///
/// Range: 0.0 to 0.5 (typical value: ~0.248)
static CONVERGENCE_SHIFT_NDC: AtomicU32 = AtomicU32::new(0);

/// IPD (Interpupillary Distance) scale factor for ALVR stereo rendering.
///
/// This controls how much of the physical IPD is used when rendering stereo views.
/// - 0.0 = Monoscopic (both eyes see the same image)
/// - 1.0 = Full physical IPD from headset sensors
/// - Values > 1.0 exaggerate stereo separation
///
/// Why this exists:
/// The Pimax Crystal has its own stereo rendering in the compositor. ALVR also
/// renders stereo. If both contribute full stereo, the result is excessive separation
/// causing eye strain. This scale lets you blend between Pimax-only stereo (0.0)
/// and full ALVR stereo (1.0).
///
/// Range: 0.0 to 2.0 (typical value: 1.0)
static IPD_SCALE: AtomicU32 = AtomicU32::new(0);

/// Color black crush adjustment for BT.709 color space.
///
/// Black crush raises the black level, making dark areas slightly brighter.
/// This compensates for the headset's display characteristics or personal preference.
///
/// How it works:
/// In the fragment shader, black crush is applied as: `color = max(color - black_crush, 0.0)`
/// A value of 0.072 means pixels below 7.2% brightness are lifted.
///
/// Range: 0.0 to 0.3 (typical value: 0.072)
static COLOR_BLACK_CRUSH: AtomicU32 = AtomicU32::new(0);

/// Color gain (contrast) adjustment for BT.709 color space.
///
/// Gain amplifies the contrast by multiplying color values above the black level.
/// Higher values = more contrast, lower values = flatter image.
///
/// How it works:
/// In the fragment shader: `color = (color - black_crush) * gain`
/// Applied after black crush to maintain the adjusted black level.
///
/// Range: 0.5 to 2.0 (typical value: 1.22)
static COLOR_GAIN: AtomicU32 = AtomicU32::new(0);

// =============================================================================
// Server Connection State
// =============================================================================
//
// These store the ALVR server connection state, managed separately from the
// tuning parameters. The server IP is persistent (saved to config), while
// status and discovered servers are runtime-only.

/// The configured ALVR server IP address.
///
/// This is the IP of the PC running ALVR Server. The client attempts to connect
/// to this IP on startup and when changed via the web UI.
///
/// Why LazyLock<Mutex<String>>?
/// - LazyLock: Can't initialize Mutex<String> in const context
/// - Mutex: Multiple threads access (HTTP thread writes, render thread may read)
/// - String: IP can change via user input
static SERVER_IP: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new(String::new()));

/// Human-readable connection status for display in the web UI.
///
/// Examples: "Not connected", "Connecting...", "Connected", "Connection failed: ..."
static SERVER_STATUS: LazyLock<Mutex<String>> =
    LazyLock::new(|| Mutex::new(String::from("Not connected")));

/// List of ALVR servers discovered via UDP broadcast.
///
/// Each entry is (hostname, IP address). Populated when user clicks "Scan for Servers"
/// in the web UI. Discovery uses ALVR's protocol on port 9943.
static DISCOVERED_SERVERS: LazyLock<Mutex<Vec<(String, String)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// Load an f32 value from an AtomicU32.
///
/// The atomic stores the raw bits of the f32. This converts back to f32.
fn load(atom: &AtomicU32) -> f32 {
    f32::from_bits(atom.load(Ordering::Relaxed))
}

/// Store an f32 value to an AtomicU32.
///
/// The f32 is converted to raw bits for atomic storage.
fn store(atom: &AtomicU32, v: f32) {
    atom.store(v.to_bits(), Ordering::Relaxed);
}

// =============================================================================
// Initialization (call once at startup)
// =============================================================================

/// Initialize the tuning module with default values.
///
/// This function:
/// 1. Loads saved settings from config (if available)
/// 2. Falls back to provided defaults
/// 3. Initializes atomic storage
/// 4. Starts the HTTP server thread
/// 5. Loads the configured server IP
///
/// # Arguments
///
/// * `convergence_shift_ndc` - Default convergence shift (typically 0.248)
/// * `ipd_scale` - Default IPD scale (typically 1.0)
/// * `color_black_crush` - Default black crush (typically 0.072)
/// * `color_gain` - Default gain (typically 1.22)
///
/// # Called From
///
/// `android::run_inner()` during application startup
pub fn init(convergence_shift_ndc: f32, ipd_scale: f32, color_black_crush: f32, color_gain: f32) {
    // Load tuning settings from config, or use defaults if not available
    let config_path = crate::config::default_config_path();
    let config = crate::config::ClientConfig::load_or_create(&config_path).ok();

    // Extract each setting with fallback to default
    // This pattern allows adding new settings without breaking old configs
    let cs = config
        .as_ref()
        .and_then(|c| c.convergence_shift_ndc)
        .unwrap_or(convergence_shift_ndc);
    let is = config
        .as_ref()
        .and_then(|c| c.ipd_scale)
        .unwrap_or(ipd_scale);
    let bc = config
        .as_ref()
        .and_then(|c| c.color_black_crush)
        .unwrap_or(color_black_crush);
    let cg = config
        .as_ref()
        .and_then(|c| c.color_gain)
        .unwrap_or(color_gain);

    // Store in atomics for render thread access
    store(&CONVERGENCE_SHIFT_NDC, cs);
    store(&IPD_SCALE, is);
    store(&COLOR_BLACK_CRUSH, bc);
    store(&COLOR_GAIN, cg);

    info!("tune: loaded settings from config: convergence_shift_ndc={:.4}, ipd_scale={:.4}, color_black_crush={:.4}, color_gain={:.4}", cs, is, bc, cg);

    // Load server IP from config
    let initial_server_ip = config
        .as_ref()
        .and_then(|c| c.last_server_ip.clone())
        .unwrap_or_else(|| String::from("192.168.50.220"));
    *SERVER_IP.lock().unwrap() = initial_server_ip;
    *SERVER_STATUS.lock().unwrap() = String::from("Not connected - configure below");

    // Start HTTP server on background thread
    // This thread runs for the lifetime of the app
    thread::spawn(run_http_server);
    info!("tune: runtime config HTTP server starting on :7878");
}

// =============================================================================
// Getters (called from render thread every frame)
// =============================================================================
//
// These functions are called by the render thread on every frame to get
// the current tuning values. They must be fast (lock-free) and safe to
// call from the render hot path.

/// Get the current convergence shift value.
///
/// # Called From
///
/// `video_receiver::blit()` - applied per-eye during the blit shader
#[inline]
pub fn convergence_shift_ndc() -> f32 {
    load(&CONVERGENCE_SHIFT_NDC)
}

/// Get the current IPD scale factor.
///
/// # Called From
///
/// `client::update_alvr_views_config_from_pimax()` - applied when building ViewsConfig
#[inline]
pub fn ipd_scale() -> f32 {
    load(&IPD_SCALE)
}

/// Get the current color black crush value.
///
/// # Called From
///
/// `video_receiver::blit()` - applied in fragment shader for color correction
#[inline]
pub fn color_black_crush() -> f32 {
    load(&COLOR_BLACK_CRUSH)
}

/// Get the current color gain value.
///
/// # Called From
///
/// `video_receiver::blit()` - applied in fragment shader for color correction
#[inline]
pub fn color_gain() -> f32 {
    load(&COLOR_GAIN)
}

// =============================================================================
// Server Management
// =============================================================================

/// Set the ALVR server IP address and attempt to connect.
///
/// This is called when the user enters an IP in the web UI and clicks "Connect".
/// The IP is:
/// 1. Stored in memory (visible to other parts of the app)
/// 2. Persisted to config file
/// 3. Used to initiate an immediate connection attempt
///
/// # Threading
///
/// Connection attempt runs on a background thread to avoid blocking the HTTP server.
pub fn set_server_ip(ip: String) {
    info!("tune: server IP set to {ip}");
    *SERVER_IP.lock().unwrap() = ip.clone();
    save_server_ip_to_config(&ip);

    // Try to connect immediately on a background thread
    std::thread::spawn(move || {
        try_connect_to_server(&ip);
    });
}

/// Attempt to connect to an ALVR server at the given IP.
///
/// This creates a new tokio runtime (blocking call from std::thread) and
/// attempts to establish a connection using the ALVR protocol.
///
/// # Connection Flow
///
/// 1. Parse IP address
/// 2. Load config for ports and client identity
/// 3. Create tokio runtime
/// 4. Call `client.connect()` which:
///    - Opens TCP connection to port 9944 (control)
///    - Opens UDP socket to port 9944 (video stream)
///    - Performs ALVR handshake
/// 5. On success: forget the session handle (keeps connection alive)
/// 6. On failure: update status with error message
///
/// # Why std::mem::forget?
///
/// The SessionHandle is deliberately leaked to keep the connection alive.
/// When SessionHandle is dropped, it closes the sockets. We want the
/// connection to persist for the lifetime of the app.
fn try_connect_to_server(ip: &str) {
    use std::net::IpAddr;

    set_server_status(format!("Connecting to {}...", ip));

    if let Ok(ip_addr) = ip.parse::<IpAddr>() {
        let config_path = crate::config::default_config_path();
        let config = match crate::config::ClientConfig::load_or_create(&config_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("tune: failed to load config for connection: {e}");
                set_server_status("Failed to load config".to_string());
                return;
            }
        };

        // Create a tokio runtime for the async connection
        // We're in a std::thread context, so we need our own runtime
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                warn!("tune: failed to create runtime for connection: {e}");
                set_server_status("Failed to create runtime".to_string());
                return;
            }
        };

        let client = crate::AlvrClient::new(config);
        match rt.block_on(client.connect(ip_addr)) {
            Ok(session) => {
                info!("tune: successfully connected to ALVR server at {}", ip);
                set_server_status("Connected".to_string());
                // Deliberately leak the session to keep connection alive
                std::mem::forget(session);
            }
            Err(err) => {
                warn!("tune: connection to {} failed: {err:#}", ip);
                set_server_status(format!("Connection failed: {}", err));
            }
        }
    } else {
        set_server_status(format!("Invalid IP: {}", ip));
    }
}

/// Get the currently configured server IP.
///
/// # Returns
///
/// The IP address string, or "192.168.1.100" if not configured
pub fn get_server_ip() -> String {
    (*SERVER_IP.lock().unwrap()).clone()
}

/// Persist the server IP to the config file.
///
/// This ensures the IP survives app restarts.
fn save_server_ip_to_config(ip: &str) {
    let config_path = crate::config::default_config_path();
    if let Ok(mut config) = crate::config::ClientConfig::load_or_create(&config_path) {
        config.last_server_ip = Some(ip.to_string());
        if let Err(e) = config.save(&config_path) {
            warn!("tune: failed to save config with server IP: {e}");
        } else {
            info!("tune: saved server IP {ip} to config");
        }
    }
}

/// Set the human-readable connection status.
///
/// This is displayed in the web UI to show connection progress/errors.
pub fn set_server_status(status: String) {
    *SERVER_STATUS.lock().unwrap() = status;
}

/// Get the current connection status.
///
/// # Returns
///
/// Status string like "Connected", "Connecting...", "Connection failed: ..."
pub fn get_server_status() -> String {
    (*SERVER_STATUS.lock().unwrap()).clone()
}

/// Add a discovered ALVR server to the list.
///
/// # Arguments
///
/// * `hostname` - The server's hostname (from ALVR discovery response)
/// * `ip` - The server's IP address
///
/// # Deduplication
///
/// Servers are deduplicated by both hostname and IP to avoid duplicates
/// from multiple discovery broadcasts.
pub fn add_discovered_server(hostname: String, ip: String) {
    let mut servers = DISCOVERED_SERVERS.lock().unwrap();
    if !servers.iter().any(|(h, i)| h == &hostname || i == &ip) {
        info!("tune: discovered server {} at {}", hostname, ip);
        servers.push((hostname, ip));
    }
}

/// Get the list of discovered servers.
///
/// # Returns
///
/// Vec of (hostname, IP) tuples
pub fn get_discovered_servers() -> Vec<(String, String)> {
    (*DISCOVERED_SERVERS.lock().unwrap()).clone()
}

/// Clear the discovered servers list.
///
/// Called before starting a new discovery scan.
pub fn clear_discovered_servers() {
    DISCOVERED_SERVERS.lock().unwrap().clear();
}

// =============================================================================
// HTTP Server
// =============================================================================
//
// A minimal HTTP server implemented with std::net::TcpListener.
// No external dependencies - just string parsing and formatting.
//
// Endpoints:
// - GET /           - Settings HTML page
// - GET /set?...    - Set a parameter (query string)
// - GET /values     - Get current values as JSON
// - GET /servers    - Get discovered servers as JSON

/// Run the HTTP server on port 7878.
///
/// This function runs in a loop for the lifetime of the app.
/// It handles one request at a time (synchronous, no async).
///
/// # Request Format
///
/// Simple HTTP/1.1 parsing:
/// - Read request line (e.g., "GET /set?ipd_scale=1.0 HTTP/1.1")
/// - Drain headers (ignore them)
/// - Route based on path
///
/// # Response Format
///
/// Minimal HTTP responses:
/// - Status line
/// - Content-Type header
/// - Content-Length header
/// - CORS header (Access-Control-Allow-Origin: *)
/// - Body
fn run_http_server() {
    let listener = match TcpListener::bind("0.0.0.0:7878") {
        Ok(l) => l,
        Err(e) => {
            warn!("tune: failed to bind HTTP server on :7878 — {e}");
            return;
        }
    };
    info!("tune: HTTP server listening on 0.0.0.0:7878");

    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            continue;
        };

        // Drain headers (read until empty line)
        loop {
            let mut h = String::new();
            match reader.read_line(&mut h) {
                Ok(n) if n <= 2 => break, // Empty line (CRLF or LF)
                Ok(_) => {}
                Err(_) => break,
            }
        }

        // Extract path from request line
        let path = request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();

        // Route request
        if let Some(query) = path.strip_prefix("/set?") {
            handle_set(query);
            let body = r#"{"ok":true}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
        } else if path == "/values" {
            // Return current tuning values as JSON
            let body = format!(
                r#"{{"convergence_shift_ndc":{:.4},"ipd_scale":{:.4},"color_black_crush":{:.4},"color_gain":{:.4},"server_ip":"{}","server_status":"{}"}}"#,
                convergence_shift_ndc(),
                ipd_scale(),
                color_black_crush(),
                color_gain(),
                get_server_ip(),
                get_server_status()
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
        } else if path == "/servers" {
            // Return discovered servers as JSON array
            let servers = get_discovered_servers();
            let servers_json: Vec<_> = servers
                .iter()
                .map(|(h, i)| format!(r#"{{"hostname":"{}","ip":"{}"}}"#, h, i))
                .collect();
            let body = format!(r#"{{"servers":[{}]}}"#, servers_json.join(","));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\n\r\n{}",
                body.len(), body
            );
        } else {
            // Serve the settings HTML page
            let html = build_html();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                html.len(),
                html
            );
        }
    }
}

/// Handle /set?... requests to change tuning parameters.
///
/// # Query String Format
///
/// Multiple parameters can be set in one request:
/// `convergence_shift_ndc=0.248&ipd_scale=1.0&color_gain=1.22`
///
/// # Parameter Types
///
/// - Float values: convergence_shift_ndc, ipd_scale, color_black_crush, color_gain
/// - String values: server_ip
/// - Commands: discover_servers (triggers action, no value)
///
/// # Threading
///
/// Each parameter update:
/// 1. Stores new value in atomic (immediate effect)
/// 2. Saves to config file (persistence)
/// 3. Logs the change
fn handle_set(query: &str) {
    for part in query.split('&') {
        let mut kv = part.splitn(2, '=');
        let key = kv.next().unwrap_or("").trim();
        let value = kv.next().unwrap_or("").trim();

        // Handle float parameters (tuning sliders)
        if let Ok(val) = value.parse::<f32>() {
            if val.is_finite() {
                match key {
                    "convergence_shift_ndc" => {
                        let clamped = val.clamp(0.0, 0.5);
                        store(&CONVERGENCE_SHIFT_NDC, clamped);
                        info!("tune: convergence_shift_ndc = {clamped:.4}");
                        save_tuning_settings();
                    }
                    "ipd_scale" => {
                        let clamped = val.clamp(0.0, 2.0);
                        store(&IPD_SCALE, clamped);
                        info!("tune: ipd_scale = {clamped:.4}");
                        // Notify client module to recompute ViewsConfig
                        crate::client::notify_ipd_scale_changed();
                        save_tuning_settings();
                    }
                    "color_black_crush" => {
                        let clamped = val.clamp(0.0, 0.3);
                        store(&COLOR_BLACK_CRUSH, clamped);
                        info!("tune: color_black_crush = {clamped:.4}");
                        save_tuning_settings();
                    }
                    "color_gain" => {
                        let clamped = val.clamp(0.5, 2.0);
                        store(&COLOR_GAIN, clamped);
                        info!("tune: color_gain = {clamped:.4}");
                        save_tuning_settings();
                    }
                    _ => {}
                }
            }
        } else {
            // Handle string parameters and commands
            match key {
                "server_ip" => {
                    set_server_ip(value.to_string());
                }
                "discover_servers" => {
                    // Trigger server discovery on background thread
                    std::thread::spawn(|| {
                        discover_servers_http();
                    });
                }
                _ => {}
            }
        }
    }
}

/// Save all current tuning settings to the config file.
///
/// Called after each slider change to ensure settings persist
/// across app restarts.
///
/// # Note
///
/// This is synchronous file I/O, but it's called infrequently
/// (only when user adjusts sliders), so performance is not critical.
fn save_tuning_settings() {
    let config_path = crate::config::default_config_path();
    if let Ok(mut config) = crate::config::ClientConfig::load_or_create(&config_path) {
        config.convergence_shift_ndc = Some(convergence_shift_ndc());
        config.ipd_scale = Some(ipd_scale());
        config.color_black_crush = Some(color_black_crush());
        config.color_gain = Some(color_gain());
        if let Err(e) = config.save(&config_path) {
            warn!("tune: failed to save tuning settings: {e}");
        } else {
            info!("tune: saved tuning settings to config");
        }
    }
}

// =============================================================================
// Server Discovery
// =============================================================================
//
// ALVR uses a simple UDP broadcast protocol for server discovery:
// 1. Client sends "ALVR\0...DISCOVERY" to 255.255.255.255:9943
// 2. Servers respond with their hostname and IP
// 3. Client collects responses for 3 seconds
//
// This allows automatic discovery without manual IP entry.

/// Discover ALVR servers on the local network via UDP broadcast.
///
/// # Protocol
///
/// 1. Bind a UDP socket to any available port
/// 2. Enable broadcast
/// 3. Send discovery packet 3 times (redundancy)
/// 4. Listen for responses for 3 seconds
/// 5. Parse responses and add to discovered servers list
///
/// # Discovery Packet Format
///
/// ```text
/// Bytes 0-3:   "ALVR" magic
/// Bytes 4-17:  Zero padding
/// Bytes 18-49: Hostname (32 bytes, null-terminated)
/// ```
///
/// # Called From
///
/// `handle_set()` when user clicks "Scan for Servers" in web UI
fn discover_servers_http() {
    use std::net::{SocketAddr, UdpSocket};
    use std::time::Duration;

    info!("tune: starting server discovery...");
    clear_discovered_servers();
    set_server_status("Discovering...".to_string());

    // Bind UDP socket
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            info!("tune: discovery failed to bind: {e}");
            set_server_status("Discovery failed".to_string());
            return;
        }
    };
    socket.set_broadcast(true).ok();
    socket.set_read_timeout(Some(Duration::from_secs(1))).ok();

    // Send discovery broadcast
    let discovery_packet = b"ALVR\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0DISCOVERY";
    let broadcast_addr: SocketAddr = "255.255.255.255:9943".parse().unwrap();

    // Send 3 times for redundancy
    for _ in 0..3 {
        socket.send_to(discovery_packet, broadcast_addr).ok();
        std::thread::sleep(Duration::from_millis(200));
    }

    // Listen for responses
    let mut buf = [0u8; 1024];
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if let Ok((len, addr)) = socket.recv_from(&mut buf) {
            if len > 18 {
                // Extract hostname from response (bytes 18-49)
                let hostname = String::from_utf8_lossy(&buf[18..(18 + 32).min(len)])
                    .trim_end_matches('\0')
                    .to_string();
                let ip = addr.ip().to_string();
                add_discovered_server(hostname.clone(), ip.clone());
            }
        }
    }

    let count = get_discovered_servers().len();
    set_server_status(format!("Found {} server(s)", count));
    info!("tune: discovery complete - {} server(s) found", count);
}

// =============================================================================
// HTML Settings Page
// =============================================================================
//
// The settings page is a single HTML file with embedded CSS and JavaScript.
// It's generated dynamically with current values injected into the template.
//
// Features:
// - Sliders for each tuning parameter with live value display
// - Server IP input with Connect button
// - Server discovery with "Scan for Servers" button
// - Debounced slider updates (80ms) to reduce network chatter
// - Status feedback for all actions

/// Generate the HTML settings page.
///
/// This function builds a complete HTML document with:
/// - Embedded CSS for dark theme styling
/// - Current tuning values pre-populated
/// - JavaScript for interactive controls
/// - Server discovery and connection UI
///
/// # Design Decisions
///
/// - Monospace font for technical appearance
/// - Dark background (#111) to reduce eye strain in VR
/// - High contrast colors (#7cf, #fa0) for visibility
/// - Simple layout optimized for mobile browsers
/// - No external dependencies (all inline)
fn build_html() -> String {
    let servers = get_discovered_servers();
    let servers_html: String = servers.iter().map(|(hostname, ip)| {
        format!(
            r#"<div class="server-item"><button class="server-btn" onclick="selectServer('{}')">{}</button><span class="server-ip">{}</span></div>"#,
            ip, hostname, ip
        )
    }).collect();

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Pimax ALVR Settings</title>
<style>
  body {{ font-family: monospace; max-width: 600px; margin: 40px auto; padding: 0 16px; background: #111; color: #eee; }}
  h1 {{ font-size: 1.2em; color: #7cf; margin-bottom: 4px; }}
  .subtitle {{ color: #888; font-size: .85em; margin-bottom: 24px; }}
  h2 {{ font-size: 1em; color: #fa0; margin-top: 28px; margin-bottom: 12px; border-bottom: 1px solid #333; padding-bottom: 6px; }}
  .param {{ margin-bottom: 16px; }}
  label {{ display: flex; justify-content: space-between; margin-bottom: 4px; }}
  label span {{ color: #fa0; }}
  input[type=range] {{ width: 100%; accent-color: #7cf; }}
  input[type=text] {{ width: 100%; padding: 8px; background: #222; border: 1px solid #444; color: #eee; font-family: monospace; font-size: 0.9em; }}
  .desc {{ font-size: .8em; color: #888; margin-top: 2px; }}
  .btn {{ background: #357; border: none; color: #fff; padding: 8px 16px; cursor: pointer; font-family: monospace; margin-top: 8px; }}
  .btn:hover {{ background: #468; }}
  #status {{ margin-top: 20px; color: #4c4; font-size: .85em; height: 1.2em; }}
  .server-list {{ margin-top: 12px; }}
  .server-item {{ display: flex; align-items: center; gap: 12px; padding: 6px 0; border-bottom: 1px solid #222; }}
  .server-btn {{ background: #246; border: 1px solid #468; color: #8cf; padding: 4px 10px; cursor: pointer; font-family: monospace; font-size: 0.85em; }}
  .server-btn:hover {{ background: #357; }}
  .server-ip {{ color: #888; font-size: 0.85em; }}
  .server-status {{ color: #fa0; font-size: 0.9em; margin-top: 8px; }}
</style>
</head>
<body>
<h1>Pimax ALVR — Settings</h1>
<div class="subtitle">Changes take effect immediately. Refresh page to load current values.</div>

<h2>Server Connection</h2>
<div class="param">
  <label>Server IP Address</label>
  <input type="text" id="server_ip" value="{current_server_ip}" placeholder="e.g., 192.168.1.100">
  <button class="btn" onclick="setServerIp()">Connect</button>
</div>
<div class="param">
  <button class="btn" onclick="discoverServers()">Scan for Servers</button>
  <div id="server_status" class="server-status">{server_status}</div>
  <div id="server_list" class="server-list">{servers_html}</div>
</div>

<h2>Video Tuning</h2>
<div class="param">
  <label>Convergence shift (NDC) <span id="v_cs">{cs:.4}</span></label>
  <input type="range" id="convergence_shift_ndc" min="0" max="0.5" step="0.004" value="{cs:.4}">
  <div class="desc">Pre-shift to cancel Pimax compositor divergent warp. Default 0.248.</div>
</div>

<div class="param">
  <label>IPD scale <span id="v_is">{is:.4}</span></label>
  <input type="range" id="ipd_scale" min="0" max="1.5" step="0.01" value="{is:.4}">
  <div class="desc">ALVR stereo strength. 0 = monoscopic, 1.0 = full physical IPD.</div>
</div>

<div class="param">
  <label>Color black crush <span id="v_bc">{bc:.4}</span></label>
  <input type="range" id="color_black_crush" min="0" max="0.3" step="0.002" value="{bc:.4}">
  <div class="desc">BT.709 black level. Default 0.072. Higher = more black crush.</div>
</div>

<div class="param">
  <label>Color gain <span id="v_cg">{cg:.4}</span></label>
  <input type="range" id="color_gain" min="0.5" max="2.0" step="0.01" value="{cg:.4}">
  <div class="desc">BT.709 contrast gain. Default 1.22. Higher = more contrast.</div>
</div>

<div id="status"></div>

<script>
const tuningIds = ['convergence_shift_ndc','ipd_scale','color_black_crush','color_gain'];
const tuningLabels = {{'convergence_shift_ndc':'v_cs','ipd_scale':'v_is','color_black_crush':'v_bc','color_gain':'v_cg'}};
let debounce = {{}};

// Tuning sliders with debouncing
tuningIds.forEach(id => {{
  const el = document.getElementById(id);
  el.addEventListener('input', () => {{
    document.getElementById(tuningLabels[id]).textContent = parseFloat(el.value).toFixed(4);
    clearTimeout(debounce[id]);
    debounce[id] = setTimeout(() => {{
      fetch('/set?' + id + '=' + el.value)
        .then(() => {{ document.getElementById('status').textContent = id + ' = ' + el.value + ' ✓'; }})
        .catch(() => {{ document.getElementById('status').textContent = 'send failed'; }});
    }}, 80);
  }});
}});

function refreshServerStatus() {{
  fetch('/values')
    .then(r => r.json())
    .then(data => {{
      document.getElementById('server_status').textContent = data.server_status;
    }})
    .catch(() => {{}});
}}

// Server IP connection
function setServerIp() {{
  const ip = document.getElementById('server_ip').value.trim();
  if (ip) {{
    document.getElementById('server_status').textContent = 'Connecting to ' + ip + '...';
    fetch('/set?server_ip=' + encodeURIComponent(ip))
      .then(() => {{
        document.getElementById('status').textContent = 'Server IP set to ' + ip + ' ✓';
        refreshServerStatus();
      }})
      .catch(() => {{ document.getElementById('status').textContent = 'Failed to set server IP'; }});
  }}
}}

// Server discovery
function discoverServers() {{
  document.getElementById('server_status').textContent = 'Discovering...';
  fetch('/set?discover_servers=1')
    .then(() => {{ setTimeout(loadServers, 3500); }})
    .catch(() => {{ document.getElementById('server_status').textContent = 'Discovery failed'; }});
}}

function loadServers() {{
  fetch('/servers')
    .then(r => r.json())
    .then(data => {{
      const list = document.getElementById('server_list');
      list.innerHTML = data.servers.map(s =>
        '<div class="server-item"><button class="server-btn" onclick="selectServer(\'' + s.ip + '\')">' + s.hostname + '</button><span class="server-ip">' + s.ip + '</span></div>'
      ).join('');
      document.getElementById('server_status').textContent = 'Found ' + data.servers.length + ' server(s)';
    }})
    .catch(() => {{ document.getElementById('server_status').textContent = 'Failed to load servers'; }});
}}

function selectServer(ip) {{
  document.getElementById('server_ip').value = ip;
  setServerIp();
}}

// Load servers on page load
loadServers();
refreshServerStatus();
setInterval(refreshServerStatus, 1500);
</script>
</body>
</html>
"#,
        cs = convergence_shift_ndc(),
        is = ipd_scale(),
        bc = color_black_crush(),
        cg = color_gain(),
        current_server_ip = get_server_ip(),
        server_status = get_server_status(),
        servers_html = servers_html,
    )
}
