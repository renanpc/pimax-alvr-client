#[cfg(target_os = "android")]
pub mod android;
#[cfg(target_os = "android")]
pub mod android_video_decoder;
pub mod client;
pub mod config;
#[cfg(target_os = "android")]
pub mod tune;
#[cfg(target_os = "android")]
pub mod pimax;
pub mod protocol;
#[cfg(target_os = "android")]
pub mod video_receiver;

pub use client::{AlvrClient, DiscoveredStreamer, SessionHandle};
pub use config::ClientConfig;
pub use protocol::{DiscoveryPacket, ProtocolId};

#[cfg(target_os = "android")]
#[ndk_glue::main]
pub fn main() {
    android::run();
}
