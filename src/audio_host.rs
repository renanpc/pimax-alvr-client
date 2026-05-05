//! Host-test audio shim.
//!
//! The production audio path depends on Android `ndk::audio` types. Host tests
//! only need the shared API surface so the crate compiles on desktop.

use std::{
    collections::VecDeque,
    net::UdpSocket,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
};

use anyhow::Result;
use log::info;
use parking_lot::Mutex;

#[derive(Clone, Copy, Debug)]
pub struct AudioBufferingConfig {
    pub average_buffering_ms: u64,
    pub batch_ms: u64,
}

pub const AUDIO_STREAM_ID: u16 = 2;

static NEGOTIATED_GAME_AUDIO_SAMPLE_RATE: AtomicU32 = AtomicU32::new(0);
static MICROPHONE_PERMISSION_GRANTED: AtomicBool = AtomicBool::new(false);

pub fn init() {
    info!("audio: host shim initialized");
}

pub fn set_negotiated_game_audio_sample_rate(sample_rate: u32) {
    NEGOTIATED_GAME_AUDIO_SAMPLE_RATE.store(sample_rate, Ordering::Relaxed);
}

pub fn negotiated_game_audio_sample_rate() -> u32 {
    NEGOTIATED_GAME_AUDIO_SAMPLE_RATE.load(Ordering::Relaxed)
}

pub fn set_microphone_permission_granted(granted: bool) {
    MICROPHONE_PERMISSION_GRANTED.store(granted, Ordering::Relaxed);
}

#[derive(Clone)]
pub struct GameAudioOutput {
    sample_queue: Arc<Mutex<VecDeque<f32>>>,
}

impl GameAudioOutput {
    pub fn sample_queue(&self) -> Arc<Mutex<VecDeque<f32>>> {
        Arc::clone(&self.sample_queue)
    }

    pub fn push_payload(&self, _payload: &[u8]) {}

    pub fn shutdown(&self) {}
}

pub struct MicrophoneCapture;

impl MicrophoneCapture {
    pub fn shutdown(&self) {}
}

pub fn start_game_audio_output(
    _sample_rate: u32,
    _buffering: AudioBufferingConfig,
) -> Result<GameAudioOutput> {
    Ok(GameAudioOutput {
        sample_queue: Arc::new(Mutex::new(VecDeque::new())),
    })
}

pub fn start_microphone_capture(
    _sample_rate: u32,
    _max_packet_size: usize,
    _socket: UdpSocket,
) -> Result<MicrophoneCapture> {
    Ok(MicrophoneCapture)
}
