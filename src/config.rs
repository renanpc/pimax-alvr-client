/// Configuration management for the Pimax ALVR client.
///
/// # Overview
///
/// This module handles persistent storage of client settings. Configuration is stored
/// as JSON in a platform-specific location:
///
/// - **Android**: `/sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json`
/// - **Windows**: `%LOCALAPPDATA%\PimaxALVR\client.json` (or `%APPDATA%` as fallback)
/// - **Linux**: `$XDG_CONFIG_HOME/PimaxALVR/client.json` (or `~/.config` as fallback)
///
/// # Configuration Fields
///
/// The config serves two purposes:
/// 1. **Identity**: Client name and protocol version for ALVR handshake
/// 2. **Settings**: User preferences that persist across app restarts
///
/// ## Network Settings
///
/// - `discovery_port`: UDP port for ALVR discovery broadcasts (default: 9943)
/// - `stream_port`: TCP/UDP port for ALVR streaming (default: 9944)
/// - `last_server_ip`: Last connected ALVR server IP (for auto-reconnect)
///
/// ## Tuning Settings
///
/// These mirror the values in `tune.rs` and are loaded at startup:
///
/// - `convergence_shift_ndc`: Stereo convergence correction (default: 0.248)
/// - `ipd_scale`: IPD scale factor (default: 1.0)
/// - `color_black_crush`: Black level adjustment (default: 0.072)
/// - `color_gain`: Contrast gain (default: 1.22)
///
/// # Versioning
///
/// The config tracks which version of the client created it:
/// - `version_string`: Current ALVR protocol version
/// - `generated_for_version`: Version when client_name was generated
///
/// This allows migration logic if the config format changes.
///
/// # Thread Safety
///
/// Config is loaded at startup and saved when settings change. The file is
/// read/write on background threads to avoid blocking the render loop.
/// Concurrent access is handled by the OS file system.

use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::protocol::{ProtocolId, DISCOVERY_PORT, STREAM_PORT};

/// Android package name for the Pimax ALVR client app.
///
/// This is used to construct the config path on Android devices.
/// The path follows Android's app-specific storage convention:
/// `/sdcard/Android/data/{PACKAGE_NAME}/files/`
pub const ANDROID_PACKAGE_NAME: &str = "com.pimax.alvr.client";

/// ALVR protocol version string.
///
/// This must match the version expected by the ALVR server for successful handshake.
/// Format: "YY.MM.PATCH" (e.g., "20.14.1")
///
/// # Version Compatibility
///
/// ALVR uses a protocol ID derived from the version string. If client and server
/// versions don't match, the handshake may fail or features may be disabled.
pub const ALVR_PROTOCOL_VERSION: &str = "20.14.1";

/// Client configuration structure.
///
/// This struct is serialized to/from JSON for persistent storage.
/// All fields are public for easy access throughout the codebase.
///
/// # Serde Attributes
///
/// - `Serialize`: Convert to JSON for saving
/// - `Deserialize`: Parse from JSON when loading
/// - `Clone`: Allow passing by value
/// - `Debug`: Enable logging with {:?} format
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientConfig {
    /// Human-readable client identifier.
    ///
    /// Sent to ALVR server during handshake. Typically derived from hostname
    /// (e.g., "pimax-crystal-og" or "GAMING-PC").
    ///
    /// Max length: 32 characters (ALVR protocol limit)
    pub client_name: String,

    /// ALVR protocol version string.
    ///
    /// Must match `ALVR_PROTOCOL_VERSION` for compatibility.
    /// Updated automatically if mismatch detected.
    pub version_string: String,

    /// Version when the client_name was generated.
    ///
    /// Used to detect when the client identity needs regeneration
    /// (e.g., after version upgrade).
    pub generated_for_version: Option<String>,

    /// UDP port for ALVR discovery broadcasts.
    ///
    /// Default: 9943
    /// The client listens on this port for discovery packets from servers.
    pub discovery_port: u16,

    /// TCP/UDP port for ALVR video streaming.
    ///
    /// Default: 9944
    /// - TCP: Control messages and configuration
    /// - UDP: Video stream shards
    pub stream_port: u16,

    /// Last successfully connected ALVR server IP.
    ///
    /// Used for:
    /// - Auto-reconnect on app startup
    /// - Pre-populating the server IP field in settings UI
    ///
    /// Stored as `Option` to handle configs created before this field existed.
    pub last_server_ip: Option<String>,

    /// Convergence shift for stereo alignment.
    ///
    /// Corrects the Pimax headset's built-in divergent warp.
    /// Range: 0.0 to 0.5 (typical: ~0.248)
    ///
    /// See `tune.rs` for detailed explanation of the convergence shift mechanism.
    pub convergence_shift_ndc: Option<f32>,

    /// IPD (Interpupillary Distance) scale factor.
    ///
    /// Controls stereo separation strength from ALVR.
    /// Range: 0.0 to 2.0 (1.0 = full physical IPD)
    ///
    /// See `tune.rs` for detailed explanation of IPD blending.
    pub ipd_scale: Option<f32>,

    /// Color black crush adjustment.
    ///
    /// Raises black level to compensate for display characteristics.
    /// Range: 0.0 to 0.3 (typical: 0.072)
    ///
    /// See `tune.rs` for detailed explanation of color correction.
    pub color_black_crush: Option<f32>,

    /// Color gain (contrast) adjustment.
    ///
    /// Amplifies contrast by multiplying color values.
    /// Range: 0.5 to 2.0 (typical: 1.22)
    ///
    /// See `tune.rs` for detailed explanation of color correction.
    pub color_gain: Option<f32>,
}

impl Default for ClientConfig {
    /// Create a new config with default values.
    ///
    /// # Defaults
    ///
    /// - `client_name`: Derived from hostname (or "pimax-crystal-og" fallback)
    /// - `version_string`: Current ALVR_PROTOCOL_VERSION
    /// - `generated_for_version`: None (will be set on first save)
    /// - `discovery_port`: 9943 (ALVR standard)
    /// - `stream_port`: 9944 (ALVR standard)
    /// - `last_server_ip`: None (user must configure)
    /// - Tuning settings: Default values from video_receiver.rs and client.rs
    fn default() -> Self {
        let client_name = default_client_name();
        Self {
            client_name,
            version_string: ALVR_PROTOCOL_VERSION.to_string(),
            generated_for_version: None,
            discovery_port: DISCOVERY_PORT,
            stream_port: STREAM_PORT,
            last_server_ip: None,
            // Default tuning values from their respective modules
            convergence_shift_ndc: Some(crate::video_receiver::PIMAX_BLIT_CONVERGENCE_SHIFT_NDC_DEFAULT),
            ipd_scale: Some(crate::client::ALVR_IPD_SCALE_DEFAULT),
            color_black_crush: Some(crate::video_receiver::COLOR_BLACK_CRUSH_DEFAULT),
            color_gain: Some(crate::video_receiver::COLOR_GAIN_DEFAULT),
        }
    }
}

impl ClientConfig {
    /// Compute the ALVR protocol ID from the version string.
    ///
    /// The protocol ID is a 64-bit hash of the version string,
    /// used during handshake to verify client/server compatibility.
    ///
    /// # Returns
    ///
    /// `ProtocolId` struct wrapping the computed hash
    pub fn protocol_id(&self) -> ProtocolId {
        ProtocolId::from_version(&self.version_string)
    }

    /// Ensure the config has a fresh identity for the current version.
    ///
    /// This is called after loading to verify the config is compatible
    /// with the running client version.
    ///
    /// # Logic
    ///
    /// 1. Update `version_string` if it doesn't match current protocol
    /// 2. Regenerate `client_name` if it was generated for a different version
    ///
    /// # Why This Exists
    ///
    /// When the ALVR protocol changes, old configs may have incompatible
    /// settings. This method ensures the identity fields are always current.
    pub fn ensure_fresh_identity(&mut self) {
        // Always use current protocol version
        if self.version_string != ALVR_PROTOCOL_VERSION {
            self.version_string = ALVR_PROTOCOL_VERSION.to_string();
        }

        // Regenerate client name if version mismatch
        let current = self.version_string.clone();
        if self.generated_for_version.as_deref() != Some(current.as_str()) {
            self.client_name = default_client_name();
            self.generated_for_version = Some(current);
        }
    }

    /// Load config from file, or create default if missing.
    ///
    /// # Arguments
    ///
    /// * `path`: Path to the config JSON file
    ///
    /// # Returns
    ///
    /// - `Ok(config)`: Successfully loaded or created
    /// - `Err(e)`: File I/O error or JSON parse error
    ///
    /// # Side Effects
    ///
    /// If the config file doesn't exist, a new one is created with defaults.
    /// The new file is saved immediately to establish the default config.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            // Load existing config
            let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let config = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            Ok(config)
        } else {
            // Create and save default config
            let config = Self::default();
            config.save(path)?;
            Ok(config)
        }
    }

    /// Save config to file.
    ///
    /// # Arguments
    ///
    /// * `path`: Path to write the config JSON file
    ///
    /// # Returns
    ///
    /// - `Ok(())`: Successfully saved
    /// - `Err(e)`: Directory creation or file write error
    ///
    /// # Format
    ///
    /// JSON with pretty-printing (2-space indent) and trailing newline:
    /// ```json
    /// {
    ///   "client_name": "pimax-crystal-og",
    ///   "version_string": "20.14.1",
    ///   ...
    /// }
    /// ```
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }

        // Serialize to pretty JSON
        let serialized = serde_json::to_vec_pretty(self)?;
        let mut file =
            fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
        file.write_all(&serialized)
            .with_context(|| format!("write {}", path.display()))?;
        file.write_all(b"\n")
            .with_context(|| format!("finalize {}", path.display()))?;
        Ok(())
    }
}

/// Get the default config path for the current platform.
///
/// # Platform Paths
///
/// - **Windows**: `%LOCALAPPDATA%\PimaxALVR\client.json`
///   (falls back to `%APPDATA%` if LOCALAPPDATA not set)
/// - **Android**: `/sdcard/Android/data/com.pimax.alvr.client/files/PimaxALVR/client.json`
/// - **Linux/macOS**: `$XDG_CONFIG_HOME/PimaxALVR/client.json`
///   (falls back to `~/.config` if XDG_CONFIG_HOME not set)
///
/// # Returns
///
/// Absolute path to the config file location
pub fn default_config_path() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        // Windows: Use LOCALAPPDATA for application data
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| env::var_os("APPDATA").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
    } else if cfg!(target_os = "android") {
        // Android: Use app-specific external storage
        // This path is accessible via ADB and the app has full read/write access
        PathBuf::from(format!("/sdcard/Android/data/{ANDROID_PACKAGE_NAME}/files"))
    } else {
        // Linux/macOS: Use XDG config directory
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
    };

    base.join("PimaxALVR").join("client.json")
}

/// Generate a client name from the system hostname.
///
/// # Sanitization
///
/// The hostname is cleaned to ensure compatibility with ALVR protocol:
/// - Only alphanumeric, hyphen, and underscore characters allowed
/// - Truncated to 32 characters (protocol limit)
/// - Falls back to "pimax-crystal-og" if result is empty
///
/// # Environment Variables
///
/// - Windows: `COMPUTERNAME`
/// - Unix-like: `HOSTNAME`
///
/// # Returns
///
/// Sanitized hostname suitable for ALVR client identity
fn default_client_name() -> String {
    let hostname = env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "pimax-crystal-og".to_string());

    // Remove any characters that might break the protocol
    let mut cleaned = hostname
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
        .collect::<String>();
    if cleaned.is_empty() {
        cleaned = "pimax-crystal-og".to_string();
    }
    cleaned.truncate(32);
    cleaned
}

/// Load the default config from the standard path.
///
/// Convenience wrapper around `ClientConfig::load_or_create` that
/// uses the platform-specific default path.
///
/// # Returns
///
/// - `Ok(config)`: Successfully loaded or created
/// - `Err(e)`: I/O or parse error
pub fn load_default_config() -> Result<ClientConfig> {
    let mut config = ClientConfig::load_or_create(default_config_path())?;
    config.ensure_fresh_identity();
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that default config has all required fields populated.
    #[test]
    fn default_config_has_required_fields() {
        let config = ClientConfig::default();
        assert!(!config.client_name.is_empty());
        assert_eq!(config.discovery_port, DISCOVERY_PORT);
        assert_eq!(config.stream_port, STREAM_PORT);
    }

    /// Verify that config can be saved and loaded without data loss.
    #[test]
    fn config_save_and_load_round_trips() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("client.json");
        let mut config = ClientConfig::default();
        config.client_name = "pimax-test".to_string();
        config.last_server_ip = Some("192.168.1.5".to_string());
        config.save(&path)?;

        let loaded = ClientConfig::load_or_create(&path)?;
        assert_eq!(loaded.client_name, "pimax-test");
        assert_eq!(loaded.last_server_ip.as_deref(), Some("192.168.1.5"));
        Ok(())
    }
}
