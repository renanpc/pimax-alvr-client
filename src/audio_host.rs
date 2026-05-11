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

use anyhow::{bail, Result};
use log::info;
use parking_lot::Mutex;

use crate::audio_common::push_game_audio_payload;

#[derive(Clone, Copy, Debug)]
pub struct AudioBufferingConfig {
    pub average_buffering_ms: u64,
    pub batch_ms: u64,
}

pub use crate::audio_common::AUDIO_STREAM_ID;

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
    batch_frames_count: usize,
    average_buffer_frames_count: usize,
}

impl GameAudioOutput {
    pub fn sample_queue(&self) -> Arc<Mutex<VecDeque<f32>>> {
        Arc::clone(&self.sample_queue)
    }

    pub fn push_payload(&self, payload: &[u8]) {
        push_game_audio_payload(
            &self.sample_queue,
            payload,
            self.average_buffer_frames_count,
            self.batch_frames_count,
        );
    }

    pub fn shutdown(&self) {}
}

pub struct MicrophoneCapture;

impl MicrophoneCapture {
    pub fn shutdown(&self) {}
}

pub fn start_game_audio_output(
    sample_rate: u32,
    buffering: AudioBufferingConfig,
) -> Result<GameAudioOutput> {
    if sample_rate < 8_000 {
        bail!("invalid game audio sample rate: {sample_rate}");
    }

    let batch_frames_count = (sample_rate as usize * buffering.batch_ms as usize / 1000).max(1);
    let average_buffer_frames_count =
        (sample_rate as usize * buffering.average_buffering_ms as usize / 1000).max(1);

    Ok(GameAudioOutput {
        sample_queue: Arc::new(Mutex::new(VecDeque::new())),
        batch_frames_count,
        average_buffer_frames_count,
    })
}

pub fn start_microphone_capture(
    _sample_rate: u32,
    _max_packet_size: usize,
    _socket: UdpSocket,
) -> Result<MicrophoneCapture> {
    if _sample_rate < 8_000 {
        bail!("invalid microphone sample rate: {_sample_rate}");
    }

    Ok(MicrophoneCapture)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_game_audio_output_starts_without_microphone_permission() {
        set_microphone_permission_granted(false);

        let output = start_game_audio_output(
            48_000,
            AudioBufferingConfig {
                average_buffering_ms: 10,
                batch_ms: 10,
            },
        );

        assert!(output.is_ok());
    }

    #[test]
    fn host_game_audio_output_buffers_payloads_like_android_path() {
        let output = start_game_audio_output(
            48_000,
            AudioBufferingConfig {
                average_buffering_ms: 1,
                batch_ms: 1,
            },
        )
        .unwrap();
        let mut payload = Vec::new();
        payload.extend_from_slice(&i16::MAX.to_ne_bytes());
        payload.extend_from_slice(&0_i16.to_ne_bytes());

        output.push_payload(&payload);

        let queue = output.sample_queue();
        let queue = queue.lock();
        assert_eq!(queue.len(), 2);
        assert!((queue[0] - 1.0).abs() < f32::EPSILON);
        assert_eq!(queue[1], 0.0);
    }

    #[test]
    fn host_audio_rejects_invalid_sample_rates() {
        let buffering = AudioBufferingConfig {
            average_buffering_ms: 10,
            batch_ms: 10,
        };
        assert!(start_game_audio_output(7_999, buffering).is_err());

        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(start_microphone_capture(7_999, 1200, socket).is_err());
    }
}
