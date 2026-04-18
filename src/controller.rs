//! Controller Input Management for Pimax ALVR Client
//!
//! # Overview
//!
//! This module manages VR controller state and translates it into the ALVR
//! protocol format. Controller data flows through three stages:
//!
//! 1. **Java polling** (`VrRenderActivity.ControllerPoller`) queries the Pimax
//!    runtime at ~30 Hz and calls JNI to push raw state into Rust.
//! 2. **This module** caches the latest state in a global `Mutex` and provides
//!    converters that produce ALVR-compatible `ButtonEntry` and `DeviceMotion`
//!    values.
//! 3. **`client.rs`** reads the cached state each frame to populate `Tracking`
//!    (controller poses via UDP) and `ClientControlPacket::Buttons` (button
//!    state via TCP).
//!
//! # Button Mapping
//!
//! The Pimax runtime reports buttons as a bitmask from `ControllerQueryInt`.
//! Each bit is mapped to an OpenXR interaction profile path string, which is
//! then hashed to a `u64` path ID via `protocol::hash_string`. The ALVR server
//! uses these path IDs to drive SteamVR input bindings.
//!
//! The bit-to-path mapping is based on the Qualcomm SVR / Pimax controller
//! layout. Initial deployment should enable diagnostic logging to verify the
//! mapping against physical hardware.
//!
//! # Thread Safety
//!
//! `LATEST_CONTROLLER_STATE` is protected by a `std::sync::Mutex`. The Java
//! poller thread writes at 30 Hz; the Rust tracking thread reads at 90 Hz.
//! Contention is minimal because both hold the lock for microseconds.

use std::sync::Mutex;
use std::time::Instant;

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::client::{DeviceMotion, Pose};
use crate::protocol::hash_string;

// ---------------------------------------------------------------------------
// ALVR protocol types
// ---------------------------------------------------------------------------

/// Button value sent to the ALVR server. Matches the ALVR v20 protocol.
///
/// - `Binary(bool)`: digital press (click/touch)
/// - `Scalar(f32)`: analog axis (trigger, grip, thumbstick)
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ButtonValue {
    Binary(bool),
    Scalar(f32),
}

/// A single button/axis entry sent in `ClientControlPacket::Buttons`.
///
/// `path_id` is `hash_string(openxr_path)` — e.g.,
/// `hash_string("/user/hand/left/input/x/click")`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ButtonEntry {
    pub path_id: u64,
    pub value: ButtonValue,
}

// ---------------------------------------------------------------------------
// Controller hand identifier
// ---------------------------------------------------------------------------

/// Left or right hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hand {
    Left = 0,
    Right = 1,
}

// ---------------------------------------------------------------------------
// Controller state
// ---------------------------------------------------------------------------

/// Raw state of a single controller, updated from the Java polling thread.
#[derive(Clone, Debug)]
pub struct SingleControllerState {
    pub connected: bool,
    pub handle: i32,
    /// Latest controller pose/velocity when provided by the native runtime.
    pub(crate) motion: Option<DeviceMotion>,
    /// Bitmask of currently pressed buttons.
    pub buttons_pressed: u32,
    /// Bitmask of currently touched buttons (capacitive).
    pub buttons_touched: u32,
    /// Trigger analog value (0.0–1.0).
    pub trigger: f32,
    /// Grip analog value (0.0–1.0).
    pub grip: f32,
    /// Thumbstick X axis (-1.0 to 1.0).
    pub thumbstick_x: f32,
    /// Thumbstick Y axis (-1.0 to 1.0).
    pub thumbstick_y: f32,
    /// Battery percentage (0–100).
    pub battery_percent: u8,
    /// Monotonic timestamp of last update.
    pub last_updated: Instant,
}

/// Snapshot of both controllers.
#[derive(Clone, Debug, Default)]
pub struct ControllerSnapshot {
    pub left: Option<SingleControllerState>,
    pub right: Option<SingleControllerState>,
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static LATEST_CONTROLLER_STATE: Mutex<ControllerSnapshot> = Mutex::new(ControllerSnapshot {
    left: None,
    right: None,
});

/// How many state updates have been received (for throttled logging).
static CONTROLLER_UPDATE_COUNT: Mutex<[u64; 2]> = Mutex::new([0, 0]);

/// Max age before a controller is treated as disconnected.
const STALE_THRESHOLD_MS: u128 = 500;

/// Push an updated state for one hand. Called from JNI.
pub fn update_controller_state(hand: Hand, state: SingleControllerState) {
    let mut snapshot = match LATEST_CONTROLLER_STATE.lock() {
        Ok(s) => s,
        Err(_) => {
            warn!("controller state mutex poisoned on update");
            return;
        }
    };

    // Throttled diagnostic logging
    if let Ok(mut counts) = CONTROLLER_UPDATE_COUNT.lock() {
        let idx = hand as usize;
        counts[idx] = counts[idx].wrapping_add(1);
        let count = counts[idx];
        if count <= 5 || count % 3600 == 0 {
            info!(
                "controller state update: hand={:?} count={} handle={} buttons=0x{:08x} touch=0x{:08x} trigger={:.2} grip={:.2} stick=({:.2},{:.2}) battery={}",
                hand, count, state.handle,
                state.buttons_pressed, state.buttons_touched,
                state.trigger, state.grip,
                state.thumbstick_x, state.thumbstick_y,
                state.battery_percent
            );
        }
    }

    match hand {
        Hand::Left => snapshot.left = Some(state),
        Hand::Right => snapshot.right = Some(state),
    }
}

/// Mark a controller as connected or disconnected. Called from JNI.
pub fn update_controller_connection(hand: Hand, connected: bool) {
    let mut snapshot = match LATEST_CONTROLLER_STATE.lock() {
        Ok(s) => s,
        Err(_) => {
            warn!("controller state mutex poisoned on connection update");
            return;
        }
    };

    info!(
        "controller connection change: hand={:?} connected={}",
        hand, connected
    );

    if !connected {
        match hand {
            Hand::Left => snapshot.left = None,
            Hand::Right => snapshot.right = None,
        }
    }
}

/// Read the latest controller snapshot. Called from tracking/control threads.
pub fn latest_controller_state() -> ControllerSnapshot {
    match LATEST_CONTROLLER_STATE.lock() {
        Ok(s) => s.clone(),
        Err(_) => {
            warn!("controller state mutex poisoned on read");
            ControllerSnapshot::default()
        }
    }
}

// ---------------------------------------------------------------------------
// ALVR device path constants
// ---------------------------------------------------------------------------

pub const LEFT_HAND_PATH: &str = "/user/hand/left";
pub const RIGHT_HAND_PATH: &str = "/user/hand/right";

// ---------------------------------------------------------------------------
// Button bitmask → ALVR path mapping
// ---------------------------------------------------------------------------

/// A mapping from a single bit in the Pimax bitmask to an ALVR button path.
struct ButtonBitMap {
    /// Bit position in the bitmask (0 = LSB).
    bit: u32,
    /// OpenXR path suffix for the left hand (e.g., "input/x/click").
    left_suffix: &'static str,
    /// OpenXR path suffix for the right hand (e.g., "input/a/click").
    right_suffix: &'static str,
}

/// Pimax button bitmask → OpenXR path mapping.
///
/// These bit positions are based on the Qualcomm SVR controller layout.
/// Verify against actual hardware output using the diagnostic logs, then
/// adjust as needed.
///
/// Bit assignments after native Pimax normalization:
///   0 = trigger click
///   1 = thumbstick click
///   2 = menu
///   3 = grip/squeeze click
///   4 = X / A face button
///   5 = Y / B face button
///   6 = system (reserved; currently not emitted by native pxrapi)
const BUTTON_PRESS_MAP: &[ButtonBitMap] = &[
    ButtonBitMap {
        bit: 0,
        left_suffix: "input/trigger/click",
        right_suffix: "input/trigger/click",
    },
    ButtonBitMap {
        bit: 1,
        left_suffix: "input/thumbstick/click",
        right_suffix: "input/thumbstick/click",
    },
    ButtonBitMap {
        bit: 2,
        left_suffix: "input/menu/click",
        right_suffix: "input/menu/click",
    },
    ButtonBitMap {
        bit: 3,
        left_suffix: "input/squeeze/click",
        right_suffix: "input/squeeze/click",
    },
    ButtonBitMap {
        bit: 4,
        left_suffix: "input/x/click",
        right_suffix: "input/a/click",
    },
    ButtonBitMap {
        bit: 5,
        left_suffix: "input/y/click",
        right_suffix: "input/b/click",
    },
];

/// Touch bitmask → OpenXR touch path mapping (capacitive sensors).
/// Same bit layout as press map but with /touch suffix.
const BUTTON_TOUCH_MAP: &[ButtonBitMap] = &[
    ButtonBitMap {
        bit: 0,
        left_suffix: "input/trigger/touch",
        right_suffix: "input/trigger/touch",
    },
    ButtonBitMap {
        bit: 1,
        left_suffix: "input/thumbstick/touch",
        right_suffix: "input/thumbstick/touch",
    },
    ButtonBitMap {
        bit: 3,
        left_suffix: "input/squeeze/touch",
        right_suffix: "input/squeeze/touch",
    },
    ButtonBitMap {
        bit: 4,
        left_suffix: "input/x/touch",
        right_suffix: "input/a/touch",
    },
    ButtonBitMap {
        bit: 5,
        left_suffix: "input/y/touch",
        right_suffix: "input/b/touch",
    },
];

/// Build the full OpenXR path for a hand + suffix, e.g.,
/// "/user/hand/left" + "input/x/click" → "/user/hand/left/input/x/click".
fn button_path_id(hand_path: &str, suffix: &str) -> u64 {
    let full = format!("{hand_path}/{suffix}");
    hash_string(&full)
}

// ---------------------------------------------------------------------------
// Converters: controller state → ALVR packets
// ---------------------------------------------------------------------------

/// Check whether a controller state is still fresh.
fn is_fresh(state: &SingleControllerState) -> bool {
    state.connected && state.last_updated.elapsed().as_millis() < STALE_THRESHOLD_MS
}

/// Build ALVR `ButtonEntry` values from the current controller snapshot.
///
/// Returns an empty Vec when no controllers are connected.
pub fn build_button_entries(snapshot: &ControllerSnapshot) -> Vec<ButtonEntry> {
    let mut entries = Vec::with_capacity(32);

    for (hand_state, hand_path) in [
        (&snapshot.left, LEFT_HAND_PATH),
        (&snapshot.right, RIGHT_HAND_PATH),
    ] {
        let state = match hand_state {
            Some(s) if is_fresh(s) => s,
            _ => continue,
        };

        let is_left = hand_path == LEFT_HAND_PATH;

        // Digital buttons (press)
        for mapping in BUTTON_PRESS_MAP {
            let suffix = if is_left {
                mapping.left_suffix
            } else {
                mapping.right_suffix
            };
            if suffix.is_empty() {
                continue;
            }
            let pressed = (state.buttons_pressed >> mapping.bit) & 1 != 0;
            entries.push(ButtonEntry {
                path_id: button_path_id(hand_path, suffix),
                value: ButtonValue::Binary(pressed),
            });
        }

        // Digital buttons (touch)
        for mapping in BUTTON_TOUCH_MAP {
            let suffix = if is_left {
                mapping.left_suffix
            } else {
                mapping.right_suffix
            };
            if suffix.is_empty() {
                continue;
            }
            let touched = (state.buttons_touched >> mapping.bit) & 1 != 0;
            entries.push(ButtonEntry {
                path_id: button_path_id(hand_path, suffix),
                value: ButtonValue::Binary(touched),
            });
        }

        // Analog axes
        entries.push(ButtonEntry {
            path_id: button_path_id(hand_path, "input/trigger/value"),
            value: ButtonValue::Scalar(state.trigger),
        });
        entries.push(ButtonEntry {
            path_id: button_path_id(hand_path, "input/squeeze/value"),
            value: ButtonValue::Scalar(state.grip),
        });
        entries.push(ButtonEntry {
            path_id: button_path_id(hand_path, "input/thumbstick/x"),
            value: ButtonValue::Scalar(state.thumbstick_x),
        });
        entries.push(ButtonEntry {
            path_id: button_path_id(hand_path, "input/thumbstick/y"),
            value: ButtonValue::Scalar(state.thumbstick_y),
        });
    }

    entries
}

/// Build `DeviceMotion` entries for connected controllers.
pub(crate) fn build_controller_device_motions(
    snapshot: &ControllerSnapshot,
) -> Vec<(u64, DeviceMotion)> {
    let mut motions = Vec::with_capacity(2);

    for (hand_state, hand_path) in [
        (&snapshot.left, LEFT_HAND_PATH),
        (&snapshot.right, RIGHT_HAND_PATH),
    ] {
        let state = match hand_state {
            Some(s) if is_fresh(s) => s,
            _ => continue,
        };

        motions.push((
            hash_string(hand_path),
            state.motion.unwrap_or(DeviceMotion {
                pose: Pose::default(),
                linear_velocity: glam::Vec3::ZERO,
                angular_velocity: glam::Vec3::ZERO,
            }),
        ));
    }

    motions
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_entry_round_trips_through_bincode() {
        let entry = ButtonEntry {
            path_id: hash_string("/user/hand/left/input/x/click"),
            value: ButtonValue::Binary(true),
        };
        let bytes = bincode::serialize(&entry).unwrap();
        let decoded: ButtonEntry = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.path_id, entry.path_id);
        match decoded.value {
            ButtonValue::Binary(v) => assert!(v),
            _ => panic!("expected Binary"),
        }
    }

    #[test]
    fn scalar_entry_round_trips_through_bincode() {
        let entry = ButtonEntry {
            path_id: hash_string("/user/hand/right/input/trigger/value"),
            value: ButtonValue::Scalar(0.75),
        };
        let bytes = bincode::serialize(&entry).unwrap();
        let decoded: ButtonEntry = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.path_id, entry.path_id);
        match decoded.value {
            ButtonValue::Scalar(v) => assert!((v - 0.75).abs() < f32::EPSILON),
            _ => panic!("expected Scalar"),
        }
    }

    #[test]
    fn build_button_entries_empty_when_no_controllers() {
        let snapshot = ControllerSnapshot::default();
        assert!(build_button_entries(&snapshot).is_empty());
    }

    #[test]
    fn build_button_entries_populates_for_connected_controller() {
        let snapshot = ControllerSnapshot {
            left: Some(SingleControllerState {
                connected: true,
                handle: 1,
                motion: None,
                buttons_pressed: 0x01,
                buttons_touched: 0x00,
                trigger: 0.8,
                grip: 0.0,
                thumbstick_x: 0.5,
                thumbstick_y: -0.3,
                battery_percent: 75,
                last_updated: Instant::now(),
            }),
            right: None,
        };
        let entries = build_button_entries(&snapshot);
        // Should have press entries + touch entries + 4 analog axes
        assert!(!entries.is_empty());

        // Verify trigger scalar is present
        let trigger_path = button_path_id(LEFT_HAND_PATH, "input/trigger/value");
        let trigger_entry = entries.iter().find(|e| e.path_id == trigger_path);
        assert!(trigger_entry.is_some());
        match &trigger_entry.unwrap().value {
            ButtonValue::Scalar(v) => assert!((v - 0.8).abs() < f32::EPSILON),
            _ => panic!("expected Scalar for trigger"),
        }
    }

    #[test]
    fn build_device_motions_empty_when_no_controllers() {
        let snapshot = ControllerSnapshot::default();
        assert!(build_controller_device_motions(&snapshot).is_empty());
    }

    #[test]
    fn build_device_motions_includes_connected_controller() {
        let snapshot = ControllerSnapshot {
            left: Some(SingleControllerState {
                connected: true,
                handle: 1,
                motion: None,
                buttons_pressed: 0,
                buttons_touched: 0,
                trigger: 0.0,
                grip: 0.0,
                thumbstick_x: 0.0,
                thumbstick_y: 0.0,
                battery_percent: 100,
                last_updated: Instant::now(),
            }),
            right: None,
        };
        let motions = build_controller_device_motions(&snapshot);
        assert_eq!(motions.len(), 1);
        assert_eq!(motions[0].0, hash_string(LEFT_HAND_PATH));
    }

    #[test]
    fn path_id_is_deterministic() {
        let a = button_path_id(LEFT_HAND_PATH, "input/x/click");
        let b = button_path_id(LEFT_HAND_PATH, "input/x/click");
        assert_eq!(a, b);
        // Left and right should differ
        let c = button_path_id(RIGHT_HAND_PATH, "input/a/click");
        assert_ne!(a, c);
    }
}
