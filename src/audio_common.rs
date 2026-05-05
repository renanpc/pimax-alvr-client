use std::{collections::VecDeque, net::UdpSocket, sync::Arc};

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;

pub const AUDIO_STREAM_ID: u16 = 2;

pub fn frames_to_f32(payload: &[u8]) -> Vec<f32> {
    payload
        .chunks_exact(2)
        .map(|bytes| i16::from_ne_bytes([bytes[0], bytes[1]]) as f32 / i16::MAX as f32)
        .collect()
}

pub fn analyze_pcm16_level(payload: &[u8]) -> Option<(f32, f32)> {
    let mut peak = 0.0_f32;
    let mut sum_squares = 0.0_f32;
    let mut count = 0_u32;

    for sample in payload.chunks_exact(2) {
        let value = i16::from_ne_bytes([sample[0], sample[1]]) as f32 / i16::MAX as f32;
        peak = peak.max(value.abs());
        sum_squares += value * value;
        count += 1;
    }

    if count == 0 {
        None
    } else {
        Some((peak, (sum_squares / count as f32).sqrt()))
    }
}

pub fn push_game_audio_payload(
    sample_queue: &Arc<Mutex<VecDeque<f32>>>,
    payload: &[u8],
    average_buffer_frames_count: usize,
    batch_frames_count: usize,
) {
    let samples = frames_to_f32(payload);
    let mut queue = sample_queue.lock();
    queue.extend(samples);

    let max_samples = (2 * average_buffer_frames_count + batch_frames_count) * 2;
    if queue.len() > max_samples {
        let drain_count = queue.len() - max_samples;
        queue.drain(..drain_count);
    }
}

pub fn get_next_frame_batch(
    sample_buffer: &Arc<Mutex<VecDeque<f32>>>,
    channels_count: usize,
    batch_frames_count: usize,
) -> Vec<f32> {
    let mut sample_buffer = sample_buffer.lock();

    if sample_buffer.len() / channels_count >= batch_frames_count {
        sample_buffer
            .drain(0..batch_frames_count * channels_count)
            .collect::<Vec<_>>()
    } else {
        vec![0.0; batch_frames_count * channels_count]
    }
}

pub fn send_stream_payload(
    socket: &UdpSocket,
    stream_id: u16,
    packet_index: u32,
    payload: &[u8],
    max_packet_size: usize,
) -> Result<()> {
    let max_shard_size = max_packet_size.saturating_sub(14);
    if max_shard_size == 0 {
        bail!("invalid ALVR packet size: {max_packet_size}");
    }

    let shards_count = payload.len().div_ceil(max_shard_size).max(1);
    let mut buffer = Vec::with_capacity(14 + max_shard_size);

    for shard_index in 0..shards_count {
        let shard_start = shard_index * max_shard_size;
        let shard_end = usize::min(shard_start + max_shard_size, payload.len());
        let shard_payload = &payload[shard_start..shard_end];

        buffer.clear();
        buffer.resize(14, 0);
        buffer.extend_from_slice(shard_payload);
        buffer[0..2].copy_from_slice(&stream_id.to_le_bytes());
        buffer[2..6].copy_from_slice(&packet_index.to_le_bytes());
        buffer[6..10].copy_from_slice(&(shards_count as u32).to_le_bytes());
        buffer[10..14].copy_from_slice(&(shard_index as u32).to_le_bytes());

        let bytes_sent = socket.send(&buffer).context("send ALVR audio payload")?;
        if bytes_sent != buffer.len() {
            bail!(
                "short ALVR audio send: sent {bytes_sent} of {} bytes",
                buffer.len()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn frames_to_f32_ignores_incomplete_sample() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&i16::MAX.to_ne_bytes());
        payload.extend_from_slice(&i16::MIN.to_ne_bytes());
        payload.push(0xff);

        let samples = frames_to_f32(&payload);

        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 1.0).abs() < f32::EPSILON);
        assert!(samples[1] < -0.99);
    }

    #[test]
    fn push_game_audio_payload_trims_oldest_samples() {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let payload = (0_i16..12)
            .flat_map(|sample| sample.to_ne_bytes())
            .collect::<Vec<_>>();

        push_game_audio_payload(&queue, &payload, 2, 1);

        let queue = queue.lock();
        assert_eq!(queue.len(), 10);
        assert_eq!(queue.front().copied(), Some(2.0 / i16::MAX as f32));
    }

    #[test]
    fn get_next_frame_batch_returns_silence_when_underfilled() {
        let queue = Arc::new(Mutex::new(VecDeque::from([0.5, -0.5])));

        let batch = get_next_frame_batch(&queue, 2, 2);

        assert_eq!(batch, vec![0.0, 0.0, 0.0, 0.0]);
        assert_eq!(queue.lock().len(), 2);
    }

    #[test]
    fn send_stream_payload_shards_with_expected_headers() {
        let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.connect(receiver.local_addr().unwrap()).unwrap();

        send_stream_payload(&sender, 2, 42, &[0, 1, 2, 3, 4], 17).unwrap();

        for expected_shard in 0..2_u32 {
            let mut packet = [0_u8; 64];
            let len = receiver.recv(&mut packet).unwrap();
            assert_eq!(u16::from_le_bytes([packet[0], packet[1]]), 2);
            assert_eq!(
                u32::from_le_bytes([packet[2], packet[3], packet[4], packet[5]]),
                42
            );
            assert_eq!(
                u32::from_le_bytes([packet[6], packet[7], packet[8], packet[9]]),
                2
            );
            assert_eq!(
                u32::from_le_bytes([packet[10], packet[11], packet[12], packet[13]]),
                expected_shard
            );
            assert!(len <= 17);
        }
    }
}
