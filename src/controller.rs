//! Controller Input Management for Pimax ALVR Client
//!
//! # Overview
//!
//! This module manages VR controller state and translates it into the ALVR
//! protocol format. Controller data flows through three stages:
//!
//! 1. **Native Pimax polling** (`pimax.rs`) uses `sxrControllerStartTracking`
//!    / `sxrControllerGetState` and normalizes the Crystal controller bitmask,
//!    axes, and pose into this module. A disabled Java `InputDevice` fallback is
//!    still present in `VrRenderActivity` for diagnostics only.
//! 2. **This module** caches the latest state in a global `Mutex` and provides
//!    converters that produce ALVR-compatible `ButtonEntry` and `DeviceMotion`
//!    values.
//! 3. **`client.rs`** reads the cached state each frame to populate `Tracking`
//!    (controller poses via UDP) and `ClientControlPacket::Buttons` (button
//!    state via TCP).
//!
//! # Button Mapping
//!
//! The native Pimax runtime reports buttons as a vendor bitmask. `pimax.rs`
//! maps those vendor bits into the compact ALVR bit layout documented below.
//! This module maps that normalized bit layout to OpenXR interaction profile
//! path strings, then hashes each path to the `u64` IDs expected by the ALVR
//! server and SteamVR bindings.
//!
//! # Thread Safety
//!
//! `LATEST_CONTROLLER_STATE` is protected by a `std::sync::Mutex`. The native
//! Pimax poller writes at 50 Hz; the Rust tracking thread reads at 90 Hz.
//! Contention is minimal because both hold the lock for microseconds.

use std::time::Instant;
use std::{collections::HashSet, sync::Mutex};

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

/// OpenXR interaction profile used for the Crystal controller surface.
pub const OCULUS_TOUCH_PROFILE_PATH: &str = "/interaction_profiles/oculus/touch_controller";

/// Active interaction profile packet payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ActiveInteractionProfile {
    pub device_id: u64,
    pub profile_id: u64,
    pub input_ids: HashSet<u64>,
}

// ---------------------------------------------------------------------------
// Controller state
// ---------------------------------------------------------------------------

/// Raw state of a single controller, updated from JNI callbacks.
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

    let previous_state = match hand {
        Hand::Left => snapshot.left.as_ref(),
        Hand::Right => snapshot.right.as_ref(),
    };
    let state = merge_controller_update(previous_state, state);

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

/// Normalized Pimax button bitmask → OpenXR path mapping.
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

const ANALOG_INPUT_SUFFIXES: &[&str] = &[
    "input/trigger/value",
    "input/squeeze/value",
    "input/thumbstick/x",
    "input/thumbstick/y",
];

/// Build the full OpenXR path for a hand + suffix, e.g.,
/// "/user/hand/left" + "input/x/click" → "/user/hand/left/input/x/click".
fn button_path_id(hand_path: &str, suffix: &str) -> u64 {
    let full = format!("{hand_path}/{suffix}");
    hash_string(&full)
}

fn suffix_for_hand(mapping: &ButtonBitMap, hand_path: &str) -> &'static str {
    if hand_path == LEFT_HAND_PATH {
        mapping.left_suffix
    } else {
        mapping.right_suffix
    }
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
/// Disconnected hands are emitted as neutral values while at least one hand is
/// still connected, so stale pressed state gets cleared on the server.
pub fn build_button_entries(snapshot: &ControllerSnapshot) -> Vec<ButtonEntry> {
    let mut entries = Vec::with_capacity(32);
    let any_connected = snapshot.left.as_ref().is_some_and(is_fresh)
        || snapshot.right.as_ref().is_some_and(is_fresh);

    for (hand_state, hand_path) in [
        (&snapshot.left, LEFT_HAND_PATH),
        (&snapshot.right, RIGHT_HAND_PATH),
    ] {
        let state = match hand_state {
            Some(s) if is_fresh(s) => Some(s),
            _ if any_connected => None,
            _ => continue,
        };

        push_hand_button_entries(&mut entries, hand_path, state);
    }

    entries
}

pub fn format_button_entries_for_hand(entries: &[ButtonEntry], hand_path: &str) -> String {
    let mut parts = Vec::new();

    for mapping in BUTTON_PRESS_MAP {
        let suffix = suffix_for_hand(mapping, hand_path);
        if suffix.is_empty() {
            continue;
        }
        let value = format_button_entry_value(entries, button_path_id(hand_path, suffix));
        parts.push(format!("{hand_path}/{suffix}={value}"));
    }

    for suffix in ANALOG_INPUT_SUFFIXES {
        let value = format_button_entry_value(entries, button_path_id(hand_path, suffix));
        parts.push(format!("{hand_path}/{suffix}={value}"));
    }

    parts.join(" ")
}

fn format_button_entry_value(entries: &[ButtonEntry], path_id: u64) -> String {
    entries
        .iter()
        .find(|entry| entry.path_id == path_id)
        .map(|entry| match entry.value {
            ButtonValue::Binary(value) => value.to_string(),
            ButtonValue::Scalar(value) => format!("{value:.3}"),
        })
        .unwrap_or_else(|| "missing".to_string())
}

fn push_hand_button_entries(
    entries: &mut Vec<ButtonEntry>,
    hand_path: &str,
    state: Option<&SingleControllerState>,
) {
    // Digital buttons (press)
    for mapping in BUTTON_PRESS_MAP {
        let suffix = suffix_for_hand(mapping, hand_path);
        if suffix.is_empty() {
            continue;
        }
        let pressed = state
            .map(|state| (state.buttons_pressed >> mapping.bit) & 1 != 0)
            .unwrap_or(false);
        entries.push(ButtonEntry {
            path_id: button_path_id(hand_path, suffix),
            value: ButtonValue::Binary(pressed),
        });
    }

    // Digital buttons (touch)
    for mapping in BUTTON_TOUCH_MAP {
        let suffix = suffix_for_hand(mapping, hand_path);
        if suffix.is_empty() {
            continue;
        }
        let touched = state
            .map(|state| (state.buttons_touched >> mapping.bit) & 1 != 0)
            .unwrap_or(false);
        entries.push(ButtonEntry {
            path_id: button_path_id(hand_path, suffix),
            value: ButtonValue::Binary(touched),
        });
    }

    // Analog axes
    let trigger = state.map(|state| state.trigger).unwrap_or(0.0);
    let grip = state.map(|state| state.grip).unwrap_or(0.0);
    let thumbstick_x = state.map(|state| state.thumbstick_x).unwrap_or(0.0);
    let thumbstick_y = state.map(|state| state.thumbstick_y).unwrap_or(0.0);

    entries.push(ButtonEntry {
        path_id: button_path_id(hand_path, "input/trigger/value"),
        value: ButtonValue::Scalar(trigger),
    });
    entries.push(ButtonEntry {
        path_id: button_path_id(hand_path, "input/squeeze/value"),
        value: ButtonValue::Scalar(grip),
    });
    entries.push(ButtonEntry {
        path_id: button_path_id(hand_path, "input/thumbstick/x"),
        value: ButtonValue::Scalar(thumbstick_x),
    });
    entries.push(ButtonEntry {
        path_id: button_path_id(hand_path, "input/thumbstick/y"),
        value: ButtonValue::Scalar(thumbstick_y),
    });
}

fn supported_input_ids_for_hand(hand_path: &str) -> HashSet<u64> {
    let mut ids = HashSet::new();

    for mapping in BUTTON_PRESS_MAP {
        let suffix = suffix_for_hand(mapping, hand_path);
        if !suffix.is_empty() {
            ids.insert(button_path_id(hand_path, suffix));
        }
    }

    for mapping in BUTTON_TOUCH_MAP {
        let suffix = suffix_for_hand(mapping, hand_path);
        if !suffix.is_empty() {
            ids.insert(button_path_id(hand_path, suffix));
        }
    }

    for suffix in ANALOG_INPUT_SUFFIXES {
        ids.insert(button_path_id(hand_path, suffix));
    }

    ids
}

/// Build ALVR active interaction profiles for connected controllers.
///
/// ALVR server currently keeps a single button mapping manager and rebuilds it
/// from the last received ActiveInteractionProfile packet. Send one combined
/// input set so both hands stay mapped at the same time.
pub fn build_active_interaction_profiles(
    snapshot: &ControllerSnapshot,
) -> Vec<ActiveInteractionProfile> {
    let profile_id = hash_string(OCULUS_TOUCH_PROFILE_PATH);
    let mut input_ids = HashSet::new();
    let mut first_fresh_hand_path = None;

    for (hand_state, hand_path) in [
        (&snapshot.left, LEFT_HAND_PATH),
        (&snapshot.right, RIGHT_HAND_PATH),
    ] {
        let Some(_state) = hand_state.as_ref().filter(|state| is_fresh(state)) else {
            continue;
        };

        first_fresh_hand_path.get_or_insert(hand_path);
        input_ids.extend(supported_input_ids_for_hand(hand_path));
    }

    first_fresh_hand_path.map_or_else(Vec::new, |hand_path| {
        vec![ActiveInteractionProfile {
            device_id: hash_string(hand_path),
            profile_id,
            input_ids,
        }]
    })
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

        let Some(motion) = state.motion else {
            continue;
        };

        motions.push((hash_string(hand_path), motion));
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
        let bytes = bincode::serde::encode_to_vec(&entry, bincode::config::standard()).unwrap();
        let (decoded, _): (ButtonEntry, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
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
        let bytes = bincode::serde::encode_to_vec(&entry, bincode::config::standard()).unwrap();
        let (decoded, _): (ButtonEntry, usize) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
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
    fn build_button_entries_maps_left_x_y_and_right_a_b() {
        let snapshot = ControllerSnapshot {
            left: Some(SingleControllerState {
                connected: true,
                handle: 1,
                motion: None,
                buttons_pressed: 0x30,
                buttons_touched: 0,
                trigger: 0.0,
                grip: 0.0,
                thumbstick_x: 0.0,
                thumbstick_y: 0.0,
                battery_percent: 100,
                last_updated: Instant::now(),
            }),
            right: Some(SingleControllerState {
                connected: true,
                handle: 2,
                motion: None,
                buttons_pressed: 0x30,
                buttons_touched: 0,
                trigger: 0.0,
                grip: 0.0,
                thumbstick_x: 0.0,
                thumbstick_y: 0.0,
                battery_percent: 100,
                last_updated: Instant::now(),
            }),
        };

        let entries = build_button_entries(&snapshot);

        let left_x = entries
            .iter()
            .find(|entry| entry.path_id == button_path_id(LEFT_HAND_PATH, "input/x/click"))
            .expect("left x entry");
        let left_y = entries
            .iter()
            .find(|entry| entry.path_id == button_path_id(LEFT_HAND_PATH, "input/y/click"))
            .expect("left y entry");
        let right_a = entries
            .iter()
            .find(|entry| entry.path_id == button_path_id(RIGHT_HAND_PATH, "input/a/click"))
            .expect("right a entry");
        let right_b = entries
            .iter()
            .find(|entry| entry.path_id == button_path_id(RIGHT_HAND_PATH, "input/b/click"))
            .expect("right b entry");

        assert!(matches!(left_x.value, ButtonValue::Binary(true)));
        assert!(matches!(left_y.value, ButtonValue::Binary(true)));
        assert!(matches!(right_a.value, ButtonValue::Binary(true)));
        assert!(matches!(right_b.value, ButtonValue::Binary(true)));
    }

    #[test]
    fn build_button_entries_clears_missing_hand() {
        let snapshot = ControllerSnapshot {
            left: Some(SingleControllerState {
                connected: true,
                handle: 1,
                motion: None,
                buttons_pressed: 0x01,
                buttons_touched: 0x01,
                trigger: 0.8,
                grip: 0.5,
                thumbstick_x: 0.25,
                thumbstick_y: -0.25,
                battery_percent: 75,
                last_updated: Instant::now(),
            }),
            right: None,
        };

        let entries = build_button_entries(&snapshot);
        let right_trigger = button_path_id(RIGHT_HAND_PATH, "input/trigger/value");
        let right_trigger_entry = entries.iter().find(|entry| entry.path_id == right_trigger);
        assert!(right_trigger_entry.is_some());
        match &right_trigger_entry.unwrap().value {
            ButtonValue::Scalar(v) => assert_eq!(*v, 0.0),
            _ => panic!("expected Scalar for missing hand trigger"),
        }
    }

    #[test]
    fn build_button_entries_preserves_right_thumbstick_x() {
        let snapshot = ControllerSnapshot {
            left: None,
            right: Some(SingleControllerState {
                connected: true,
                handle: 2,
                motion: None,
                buttons_pressed: 0,
                buttons_touched: 0,
                trigger: 0.0,
                grip: 0.0,
                thumbstick_x: 0.5,
                thumbstick_y: 0.0,
                battery_percent: 100,
                last_updated: Instant::now(),
            }),
        };

        let entries = build_button_entries(&snapshot);
        let right_thumbstick_x = button_path_id(RIGHT_HAND_PATH, "input/thumbstick/x");
        let entry = entries
            .iter()
            .find(|entry| entry.path_id == right_thumbstick_x)
            .expect("right thumbstick X entry");
        match &entry.value {
            ButtonValue::Scalar(v) => assert_eq!(*v, 0.5),
            _ => panic!("expected Scalar for right thumbstick X"),
        }
    }

    #[test]
    fn update_controller_state_overwrites_buttons_and_touches() {
        let initial = SingleControllerState {
            connected: true,
            handle: 9,
            motion: Some(DeviceMotion::default()),
            buttons_pressed: 0x03,
            buttons_touched: 0x03,
            trigger: 0.0,
            grip: 0.0,
            thumbstick_x: 0.0,
            thumbstick_y: 0.0,
            battery_percent: 100,
            last_updated: Instant::now(),
        };
        update_controller_state(Hand::Left, initial);

        update_controller_state(
            Hand::Left,
            SingleControllerState {
                connected: true,
                handle: 9,
                motion: None,
                buttons_pressed: 0,
                buttons_touched: 0,
                trigger: 0.0,
                grip: 0.0,
                thumbstick_x: 0.0,
                thumbstick_y: 0.0,
                battery_percent: 100,
                last_updated: Instant::now(),
            },
        );

        let snapshot = latest_controller_state();
        let left = snapshot.left.expect("left controller state");
        assert_eq!(left.buttons_pressed, 0x00);
        assert_eq!(left.buttons_touched, 0x00);
        assert!(left.motion.is_some());
    }

    #[test]
    fn build_device_motions_empty_when_no_controllers() {
        let snapshot = ControllerSnapshot::default();
        assert!(build_controller_device_motions(&snapshot).is_empty());
    }

    #[test]
    fn build_device_motions_skips_controller_without_motion() {
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
        assert!(motions.is_empty());
    }

    #[test]
    fn build_device_motions_includes_connected_controller_with_motion() {
        let snapshot = ControllerSnapshot {
            left: Some(SingleControllerState {
                connected: true,
                handle: 1,
                motion: Some(DeviceMotion::default()),
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

    #[test]
    fn build_active_interaction_profiles_includes_supported_inputs() {
        let snapshot = ControllerSnapshot {
            left: Some(SingleControllerState {
                connected: true,
                handle: 7,
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

        let profiles = build_active_interaction_profiles(&snapshot);
        assert_eq!(profiles.len(), 1);
        let profile = &profiles[0];
        assert_eq!(profile.device_id, hash_string(LEFT_HAND_PATH));
        assert_eq!(profile.profile_id, hash_string(OCULUS_TOUCH_PROFILE_PATH));
        assert!(profile
            .input_ids
            .contains(&button_path_id(LEFT_HAND_PATH, "input/x/click")));
        assert!(profile
            .input_ids
            .contains(&button_path_id(LEFT_HAND_PATH, "input/trigger/value")));
        assert!(profile
            .input_ids
            .contains(&button_path_id(LEFT_HAND_PATH, "input/thumbstick/y")));
    }

    #[test]
    fn build_active_interaction_profiles_combines_both_hands() {
        let fresh_state = || SingleControllerState {
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
        };
        let snapshot = ControllerSnapshot {
            left: Some(fresh_state()),
            right: Some(SingleControllerState {
                handle: 2,
                ..fresh_state()
            }),
        };

        let profiles = build_active_interaction_profiles(&snapshot);
        assert_eq!(profiles.len(), 1);
        let profile = &profiles[0];
        assert!(profile
            .input_ids
            .contains(&button_path_id(LEFT_HAND_PATH, "input/x/click")));
        assert!(profile
            .input_ids
            .contains(&button_path_id(LEFT_HAND_PATH, "input/thumbstick/x")));
        assert!(profile
            .input_ids
            .contains(&button_path_id(RIGHT_HAND_PATH, "input/a/click")));
        assert!(profile
            .input_ids
            .contains(&button_path_id(RIGHT_HAND_PATH, "input/thumbstick/x")));
    }

    #[test]
    fn merge_controller_update_preserves_thumbstick_sign_changes() {
        let previous = SingleControllerState {
            connected: true,
            handle: 1,
            motion: None,
            buttons_pressed: 0,
            buttons_touched: 0,
            trigger: 0.0,
            grip: 0.0,
            thumbstick_x: -1.0,
            thumbstick_y: 1.0,
            battery_percent: 100,
            last_updated: Instant::now(),
        };
        let incoming = SingleControllerState {
            thumbstick_x: 1.0,
            thumbstick_y: -1.0,
            ..previous.clone()
        };

        let merged = merge_controller_update(Some(&previous), incoming);

        assert_eq!(merged.thumbstick_x, 1.0);
        assert_eq!(merged.thumbstick_y, -1.0);
    }
}

fn merge_controller_update(
    previous_state: Option<&SingleControllerState>,
    mut state: SingleControllerState,
) -> SingleControllerState {
    if let (Some(previous_state), Some(incoming_motion)) = (previous_state, state.motion) {
        state.motion = Some(apply_controller_position_deadzone(
            previous_state.motion,
            incoming_motion,
        ));
    } else if state.motion.is_none() {
        state.motion = previous_state.and_then(|previous_state| previous_state.motion);
    }

    state
}

fn apply_controller_position_deadzone(
    previous_motion: Option<DeviceMotion>,
    incoming_motion: DeviceMotion,
) -> DeviceMotion {
    let Some(previous_motion) = previous_motion else {
        return incoming_motion;
    };

    let deadzone = crate::tune::controller_position_deadzone().max(0.0);
    let previous_position = previous_motion.pose.position;
    let incoming_position = incoming_motion.pose.position;
    let stabilized_position = if (incoming_position - previous_position).length() < deadzone {
        previous_position
    } else {
        incoming_position
    };

    DeviceMotion {
        pose: Pose {
            orientation: incoming_motion.pose.orientation,
            position: stabilized_position,
        },
        linear_velocity: incoming_motion.linear_velocity,
        angular_velocity: incoming_motion.angular_velocity,
    }
}
