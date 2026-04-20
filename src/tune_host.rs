//! Host-test tuning shim.
//!
//! The real tune server is Android-only. Host tests only need stable defaults
//! for pure protocol/config logic that shares the same data structures.

use glam::Vec3;

pub const CONTROLLER_ROTATION_X_DEG_DEFAULT: f32 = 45.0;
pub const CONTROLLER_ROTATION_Y_DEG_DEFAULT: f32 = 0.0;
pub const CONTROLLER_ROTATION_Z_DEG_DEFAULT: f32 = 0.0;

pub fn ipd_scale() -> f32 {
    crate::client::ALVR_IPD_SCALE_DEFAULT
}

pub fn controller_rotation_deg() -> Vec3 {
    Vec3::new(
        CONTROLLER_ROTATION_X_DEG_DEFAULT,
        CONTROLLER_ROTATION_Y_DEG_DEFAULT,
        CONTROLLER_ROTATION_Z_DEG_DEFAULT,
    )
}
