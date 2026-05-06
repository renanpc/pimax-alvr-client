//! Host-test tuning shim.
//!
//! The real tune server is Android-only. Host tests only need stable defaults
//! for pure protocol/config logic that shares the same data structures.
pub const EYE_RENDER_SCALE_DEFAULT: f32 = 1.0;
pub const FOV_SCALE_DEFAULT: f32 = 0.95;

pub fn ipd_scale() -> f32 {
    crate::client::ALVR_IPD_SCALE_DEFAULT
}

pub fn eye_render_scale() -> f32 {
    EYE_RENDER_SCALE_DEFAULT
}

pub fn fov_scale() -> f32 {
    FOV_SCALE_DEFAULT
}

pub fn get_server_ip() -> String {
    String::new()
}
