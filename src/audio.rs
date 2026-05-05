use std::{
    collections::VecDeque,
    net::UdpSocket,
    slice,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{bail, Result};
use log::{info, warn};
use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioError, AudioFormat, AudioInputPreset,
    AudioPerformanceMode, AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
};
use parking_lot::Mutex;

use crate::audio_common::{
    analyze_pcm16_level, get_next_frame_batch, push_game_audio_payload, send_stream_payload,
};

#[derive(Clone, Copy, Debug)]
pub struct AudioBufferingConfig {
    pub average_buffering_ms: u64,
    pub batch_ms: u64,
}

pub use crate::audio_common::AUDIO_STREAM_ID;
const INPUT_SAMPLES_MAX_BUFFER_COUNT: usize = 20;
const INPUT_RECV_TIMEOUT: Duration = Duration::from_millis(20);

static NEGOTIATED_GAME_AUDIO_SAMPLE_RATE: AtomicU32 = AtomicU32::new(0);
static MICROPHONE_PERMISSION_GRANTED: AtomicBool = AtomicBool::new(false);

use std::sync::atomic::AtomicU32;

pub fn init() {
    info!("audio: initialized negotiation hooks");
}

pub fn set_negotiated_game_audio_sample_rate(sample_rate: u32) {
    NEGOTIATED_GAME_AUDIO_SAMPLE_RATE.store(sample_rate, Ordering::Relaxed);
    if sample_rate != 0 {
        info!("audio: negotiated game audio sample rate = {sample_rate} Hz");
    }
}

pub fn negotiated_game_audio_sample_rate() -> u32 {
    NEGOTIATED_GAME_AUDIO_SAMPLE_RATE.load(Ordering::Relaxed)
}

pub fn set_microphone_permission_granted(granted: bool) {
    MICROPHONE_PERMISSION_GRANTED.store(granted, Ordering::Relaxed);
    info!("audio: microphone permission granted={granted}");
}

#[derive(Clone)]
pub struct GameAudioOutput {
    sample_queue: Arc<Mutex<VecDeque<f32>>>,
    stop_requested: Arc<AtomicBool>,
    join_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
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

    pub fn shutdown(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        if let Some(join_handle) = self.join_handle.lock().take() {
            join_handle.join().ok();
        }
    }
}

pub struct MicrophoneCapture {
    stop_requested: Arc<AtomicBool>,
    join_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl MicrophoneCapture {
    pub fn shutdown(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        if let Some(join_handle) = self.join_handle.lock().take() {
            join_handle.join().ok();
        }
    }
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
    let sample_queue = Arc::new(Mutex::new(VecDeque::new()));
    let stop_requested = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None::<AudioError>));

    let thread_sample_queue = Arc::clone(&sample_queue);
    let thread_stop_requested = Arc::clone(&stop_requested);
    let thread_error = Arc::clone(&error);
    let join_handle = thread::spawn(move || {
        while !MICROPHONE_PERMISSION_GRANTED.load(Ordering::Relaxed)
            && !thread_stop_requested.load(Ordering::SeqCst)
        {
            thread::sleep(Duration::from_millis(100));
        }

        if thread_stop_requested.load(Ordering::SeqCst) {
            return;
        }

        let stream: AudioStream = match AudioStreamBuilder::new().and_then(|builder| {
            builder
                .direction(AudioDirection::Output)
                .channel_count(2)
                .sample_rate(sample_rate as _)
                .format(AudioFormat::PCM_Float)
                .frames_per_data_callback(batch_frames_count as _)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(AudioSharingMode::Shared)
                .data_callback(Box::new(move |_, data_ptr, frames_count| {
                    let frames_count = frames_count as usize;
                    let out_frames = unsafe {
                        slice::from_raw_parts_mut(data_ptr as *mut f32, frames_count * 2)
                    };
                    let samples = get_next_frame_batch(&thread_sample_queue, 2, frames_count);
                    out_frames.copy_from_slice(&samples);
                    AudioCallbackResult::Continue
                }))
                .error_callback(Box::new(move |_, e| *thread_error.lock() = Some(e)))
                .open_stream()
        }) {
            Ok(stream) => stream,
            Err(err) => {
                warn!("failed to create game audio output stream: {err:#}");
                return;
            }
        };

        if stream.get_channel_count() != 2
            || stream.get_sample_rate() != sample_rate as i32
            || !matches!(stream.get_format(), Ok(AudioFormat::PCM_Float))
            || stream.get_frames_per_data_callback() != Some(batch_frames_count as _)
        {
            warn!("game audio output stream negotiated unexpected configuration");
            return;
        }

        if let Err(err) = stream.request_start() {
            warn!("failed to start game audio output stream: {err:#}");
            return;
        }

        while !thread_stop_requested.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(50));
        }

        stream.request_stop().ok();
    });

    Ok(GameAudioOutput {
        sample_queue,
        stop_requested,
        join_handle: Arc::new(Mutex::new(Some(join_handle))),
        batch_frames_count,
        average_buffer_frames_count,
    })
}

pub fn start_microphone_capture(
    sample_rate: u32,
    max_packet_size: usize,
    socket: UdpSocket,
) -> Result<MicrophoneCapture> {
    if sample_rate < 8_000 {
        bail!("invalid microphone sample rate: {sample_rate}");
    }

    let stop_requested = Arc::new(AtomicBool::new(false));
    let (samples_sender, samples_receiver) =
        mpsc::sync_channel::<Vec<u8>>(INPUT_SAMPLES_MAX_BUFFER_COUNT);
    let error = Arc::new(Mutex::new(None::<AudioError>));
    let thread_stop_requested = Arc::clone(&stop_requested);
    let thread_error = Arc::clone(&error);

    let join_handle = thread::spawn(move || {
        let mut maybe_stream = None;
        for input_preset in [
            AudioInputPreset::Unprocessed,
            AudioInputPreset::Generic,
            AudioInputPreset::VoiceCommunication,
        ] {
            let samples_sender = samples_sender.clone();
            let thread_error = Arc::clone(&thread_error);
            let attempt = AudioStreamBuilder::new().and_then(|builder| {
                builder
                    .direction(AudioDirection::Input)
                    .usage(AudioUsage::Game)
                    .channel_count(1)
                    .sample_rate(sample_rate as _)
                    .format(AudioFormat::PCM_I16)
                    .input_preset(input_preset)
                    .performance_mode(AudioPerformanceMode::LowLatency)
                    .sharing_mode(AudioSharingMode::Shared)
                    .data_callback(Box::new(move |_, data_ptr, frames_count| {
                        let buffer_size = frames_count as usize * std::mem::size_of::<i16>();
                        let sample_buffer =
                            unsafe { slice::from_raw_parts(data_ptr as *const u8, buffer_size) }
                                .to_vec();
                        samples_sender.send(sample_buffer).ok();
                        AudioCallbackResult::Continue
                    }))
                    .error_callback(Box::new(move |_, e| *thread_error.lock() = Some(e)))
                    .open_stream()
            });

            match attempt {
                Ok(stream) => {
                    info!("microphone capture opened with preset {:?}", input_preset);
                    maybe_stream = Some(stream);
                    break;
                }
                Err(err) => {
                    warn!("microphone preset {:?} unavailable: {err:#}", input_preset);
                }
            }
        }

        let stream: AudioStream = match maybe_stream {
            Some(stream) => stream,
            None => {
                warn!("failed to create microphone capture stream with any input preset");
                return;
            }
        };

        if stream.get_channel_count() != 1
            || stream.get_sample_rate() != sample_rate as i32
            || !matches!(stream.get_format(), Ok(AudioFormat::PCM_I16))
        {
            warn!("microphone capture stream negotiated unexpected configuration");
            return;
        }

        if let Err(err) = stream.request_start() {
            warn!("failed to start microphone capture stream: {err:#}");
            return;
        }

        let mut packet_index = 0_u32;
        let mut captured_packets = 0_u64;
        while !thread_stop_requested.load(Ordering::SeqCst) {
            match samples_receiver.recv_timeout(INPUT_RECV_TIMEOUT) {
                Ok(sample_buffer) => {
                    let (peak, rms) = analyze_pcm16_level(&sample_buffer).unwrap_or((0.0, 0.0));
                    if let Err(err) = send_stream_payload(
                        &socket,
                        AUDIO_STREAM_ID,
                        packet_index,
                        &sample_buffer,
                        max_packet_size,
                    ) {
                        warn!("failed to send microphone packet: {err:#}");
                        break;
                    }
                    captured_packets = captured_packets.wrapping_add(1);
                    if captured_packets <= 5 || captured_packets % 50 == 0 || peak > 0.05 {
                        info!(
                            "sent ALVR microphone packet: packet_index={} payload_bytes={} peak={:.3} rms={:.3} captured_packets={}",
                            packet_index,
                            sample_buffer.len(),
                            peak,
                            rms,
                            captured_packets
                        );
                    }
                    packet_index = packet_index.wrapping_add(1);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        stream.request_stop().ok();
    });

    Ok(MicrophoneCapture {
        stop_requested,
        join_handle: Arc::new(Mutex::new(Some(join_handle))),
    })
}
