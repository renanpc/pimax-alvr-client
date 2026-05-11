use super::{CodecType, MediacodecPropType, VideoDecoderConfig};
use anyhow::{anyhow, bail, Context, Result};
use log::{error, info, warn};
use ndk::{
    hardware_buffer::HardwareBufferUsage,
    media::{
        image_reader::{Image, ImageFormat, ImageReader},
        media_codec::{MediaCodec, MediaCodecDirection, MediaFormat},
        NdkMediaError,
    },
};
use ndk_sys as ffi;
use parking_lot::{Condvar, Mutex};
use std::{
    collections::VecDeque,
    ffi::c_void,
    ops::Deref,
    ptr,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Weak},
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::video_receiver::bt709_limited_rgb;

// Match upstream ALVR's default ImageReader queue budget for MediaCodec output.
const MAX_BUFFERING_FRAMES: usize = 10;
// Long-term, stay aligned with upstream ALVR's MediaCodec -> ImageReader
// architecture: PRIVATE decoder output backed by GPU-sampled hardware buffers.
// The CPU staging path remains available for targeted diagnostics, but it is
// not stable enough on Crystal OG to be the default runtime architecture.
const USE_CPU_STAGING_DECODER: bool = false;
const CPU_STAGING_IMAGE_FORMAT: ImageFormat = ImageFormat::YUV_420_888;
const CPU_STAGING_MAX_IMAGES: i32 = 3;
static STAGED_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
static QUEUE_OVERFLOW_COUNT: AtomicU64 = AtomicU64::new(0);
static DEQUEUE_FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
static DEQUEUE_EMPTY_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug)]
enum AliasedChromaOrder {
    Uv,
    Vu,
}

fn neutralize_zero_chroma(u: &mut u8, v: &mut u8) {
    if *u == 0 && *v == 0 {
        *u = 128;
        *v = 128;
    }
}

struct FakeThreadSafe<T>(T);
unsafe impl<T> Send for FakeThreadSafe<T> {}
unsafe impl<T> Sync for FakeThreadSafe<T> {}

impl<T> Deref for FakeThreadSafe<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

type SharedMediaCodec = Arc<FakeThreadSafe<MediaCodec>>;

#[derive(Default)]
pub struct VideoDecoderSink {
    inner: Arc<Mutex<Option<SharedMediaCodec>>>,
}

unsafe impl Send for VideoDecoderSink {}

impl VideoDecoderSink {
    pub fn push_frame_nal(&mut self, timestamp: Duration, data: &[u8]) -> Result<bool> {
        let guard = self.inner.lock();
        let Some(decoder) = &*guard else {
            return Ok(false);
        };

        match decoder.dequeue_input_buffer(Duration::ZERO) {
            Ok(Some(mut buffer)) => {
                unsafe {
                    ptr::copy_nonoverlapping(
                        data.as_ptr(),
                        buffer.buffer_mut().as_mut_ptr().cast(),
                        data.len(),
                    )
                };

                // Keep nanosecond precision here to match ALVR's decoder path.
                decoder.queue_input_buffer(buffer, 0, data.len(), timestamp.as_nanos() as _, 0)?;

                Ok(true)
            }
            Ok(None) => Ok(false),
            Err(e) => bail!("{e}"),
        }
    }
}

struct QueuedImage {
    timestamp: Duration,
    image: Image,
    in_use: bool,
}
unsafe impl Send for QueuedImage {}

pub struct VideoDecoderSource {
    running: Arc<RelaxedAtomic>,
    dequeue_thread: Option<JoinHandle<()>>,
    image_queue: Arc<Mutex<VecDeque<QueuedImage>>>,
    config: VideoDecoderConfig,
    buffering_running_average: f32,
}

unsafe impl Send for VideoDecoderSource {}

impl VideoDecoderSource {
    pub fn dequeue_frame(&mut self) -> Option<(Duration, *mut c_void)> {
        let mut image_queue_lock = self.image_queue.lock();
        let queue_len_before = image_queue_lock.len();

        if let Some(queued_image) = image_queue_lock.front() {
            if queued_image.in_use {
                image_queue_lock.pop_front();
            }
        }

        while image_queue_lock.len() > 1 {
            image_queue_lock.pop_front();
        }

        self.buffering_running_average = self.buffering_running_average
            * self.config.buffering_history_weight
            + image_queue_lock.len() as f32 * (1. - self.config.buffering_history_weight);

        if let Some(queued_image) = image_queue_lock.front_mut() {
            queued_image.in_use = true;
            let timestamp = queued_image.timestamp;
            match queued_image.image.get_hardware_buffer() {
                Ok(buffer) => {
                    let count = DEQUEUE_FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    if count <= 5 || count % 120 == 0 {
                        info!(
                            "upstream decoder source handed frame {count}: queue_len_before={} queue_len_after={} buffering_avg={:.2} timestamp_ns={}",
                            queue_len_before,
                            image_queue_lock.len(),
                            self.buffering_running_average,
                            timestamp.as_nanos()
                        );
                    }

                    Some((timestamp, buffer.as_ptr().cast()))
                }
                Err(err) => {
                    warn!("failed to obtain AHardwareBuffer from decoded image: {err}");
                    None
                }
            }
        } else {
            let empty_count = DEQUEUE_EMPTY_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if empty_count <= 5 || empty_count % 600 == 0 {
                info!(
                    "upstream decoder source had no frame ready: queue_len_before={} buffering_avg={:.2} empties={empty_count}",
                    queue_len_before,
                    self.buffering_running_average
                );
            }
            None
        }
    }
}

impl Drop for VideoDecoderSource {
    fn drop(&mut self) {
        self.running.set(false);
        self.dequeue_thread.take().map(|t| t.join());
    }
}

fn mime_for_codec(codec: CodecType) -> &'static str {
    match codec {
        CodecType::H264 => "video/avc",
        CodecType::Hevc => "video/hevc",
        CodecType::AV1 => "video/av01",
    }
}

fn decoder_setup(
    codec_type: CodecType,
    is_software: bool,
    format: &MediaFormat,
    image_reader: &ImageReader,
) -> Result<MediaCodec> {
    let decoder = if is_software {
        let sw_codec_name = match codec_type {
            CodecType::H264 => "OMX.google.h264.decoder",
            CodecType::Hevc => "OMX.google.hevc.decoder",
            CodecType::AV1 => bail!("AV1 is not supported for software decoding"),
        };
        MediaCodec::from_codec_name(&sw_codec_name)
            .ok_or(anyhow!("no such codec: {}", &sw_codec_name))?
    } else {
        let mime = mime_for_codec(codec_type);
        MediaCodec::from_decoder_type(&mime)
            .ok_or(anyhow!("unable to find decoder for mime type: {}", &mime))?
    };
    decoder
        .configure(
            &format,
            Some(&image_reader.get_window()?),
            MediaCodecDirection::Decoder,
        )
        .with_context(|| format!("failed to configure decoder"))?;

    decoder
        .start()
        .with_context(|| format!("failed to start decoder"))?;

    Ok(decoder)
}

fn stage_rgba_image(image: &Image, timestamp: Duration) -> Result<bool> {
    let width = image.get_width()? as usize;
    let height = image.get_height()? as usize;
    let plane_count = image.get_number_of_planes()? as usize;
    if plane_count < 1 {
        bail!("RGBA image has no planes");
    }

    let row_stride = image.get_plane_row_stride(0)? as usize;
    let pixel_stride = image.get_plane_pixel_stride(0).unwrap_or(4) as usize;
    let plane = image.get_plane_data(0)?;
    let packed_stride = width
        .checked_mul(4)
        .context("RGBA packed stride overflow")?;
    let mut rgba = vec![
        0_u8;
        packed_stride
            .checked_mul(height)
            .context("RGBA frame size overflow")?
    ];

    for row in 0..height {
        let src_row_start = row
            .checked_mul(row_stride)
            .context("RGBA source row offset overflow")?;
        let dst_row_start = row
            .checked_mul(packed_stride)
            .context("RGBA destination row offset overflow")?;

        if pixel_stride == 4 {
            let src_row_end = src_row_start
                .checked_add(packed_stride)
                .context("RGBA source row end overflow")?;
            rgba[dst_row_start..dst_row_start + packed_stride]
                .copy_from_slice(&plane[src_row_start..src_row_end]);
        } else {
            for col in 0..width {
                let src_pixel_start = src_row_start
                    .checked_add(
                        col.checked_mul(pixel_stride)
                            .context("RGBA source pixel offset overflow")?,
                    )
                    .context("RGBA source pixel start overflow")?;
                let dst_pixel_start = dst_row_start
                    .checked_add(
                        col.checked_mul(4)
                            .context("RGBA destination pixel overflow")?,
                    )
                    .context("RGBA destination pixel start overflow")?;
                rgba[dst_pixel_start..dst_pixel_start + 4]
                    .copy_from_slice(&plane[src_pixel_start..src_pixel_start + 4]);
            }
        }
    }

    let receiver = crate::video_receiver::get_video_receiver();
    crate::video_receiver::push_rgba_frame(
        receiver.as_ref(),
        timestamp.as_nanos() as u64,
        width as u32,
        height as u32,
        rgba,
    );

    Ok(true)
}

fn push_staged_nv12_as_rgba(
    timestamp: Duration,
    width: usize,
    height: usize,
    nv12: Vec<u8>,
) -> Result<()> {
    let uv_width = (width + 1) / 2;
    let y_plane_len = width
        .checked_mul(height)
        .context("staged NV12 luma plane size overflow")?;
    let rgba_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("staged RGBA frame size overflow")?;
    let mut rgba = vec![0_u8; rgba_len];

    for row in 0..height {
        let y_row_start = row
            .checked_mul(width)
            .context("staged NV12 RGBA row overflow")?;
        let uv_row_start = y_plane_len
            + (row / 2)
                .checked_mul(uv_width)
                .and_then(|offset| offset.checked_mul(2))
                .context("staged NV12 chroma row overflow")?;
        for col in 0..width {
            let y = nv12[y_row_start + col];
            let uv_index = uv_row_start + (col / 2) * 2;
            let mut u = nv12[uv_index];
            let mut v = nv12[uv_index + 1];
            neutralize_zero_chroma(&mut u, &mut v);
            let rgb = bt709_limited_rgb(y, u, v);
            let dst = (y_row_start + col) * 4;
            rgba[dst] = rgb[0];
            rgba[dst + 1] = rgb[1];
            rgba[dst + 2] = rgb[2];
            rgba[dst + 3] = 255;
        }
    }

    let receiver = crate::video_receiver::get_video_receiver();
    crate::video_receiver::push_rgba_frame(
        receiver.as_ref(),
        timestamp.as_nanos() as u64,
        width as u32,
        height as u32,
        rgba,
    );

    Ok(())
}

fn stage_yuv420_image_as_nv12(image: &Image, timestamp: Duration) -> Result<bool> {
    let image_width = image.get_width()? as usize;
    let image_height = image.get_height()? as usize;

    let image_plane_count = image.get_number_of_planes()? as usize;
    if image_plane_count >= 3 {
        let y_plane = image.get_plane_data(0)?;
        let u_plane = image.get_plane_data(1)?;
        let v_plane = image.get_plane_data(2)?;
        let y_row_stride = image.get_plane_row_stride(0)? as usize;
        let u_row_stride = image.get_plane_row_stride(1)? as usize;
        let v_row_stride = image.get_plane_row_stride(2)? as usize;
        let y_pixel_stride = image.get_plane_pixel_stride(0).unwrap_or(1) as usize;
        let u_pixel_stride = image.get_plane_pixel_stride(1).unwrap_or(1) as usize;
        let v_pixel_stride = image.get_plane_pixel_stride(2).unwrap_or(1) as usize;

        let crop = image.get_crop_rect().ok();
        let mut crop_left = 0usize;
        let mut crop_top = 0usize;
        let mut width = image_width;
        let mut height = image_height;
        if let Some(crop) = crop {
            let right = crop.right.max(crop.left) as usize;
            let bottom = crop.bottom.max(crop.top) as usize;
            let left = crop.left.max(0) as usize;
            let top = crop.top.max(0) as usize;
            let crop_width = right.saturating_sub(left);
            let crop_height = bottom.saturating_sub(top);
            if crop_width > 0 && crop_height > 0 && right <= image_width && bottom <= image_height {
                crop_left = left;
                crop_top = top;
                width = crop_width;
                height = crop_height;
            }
        }

        let y_plane_len = width
            .checked_mul(height)
            .context("NV12 Y plane size overflow")?;
        let uv_width = (width + 1) / 2;
        let uv_height = (height + 1) / 2;
        let uv_plane_len = uv_width
            .checked_mul(uv_height)
            .and_then(|samples| samples.checked_mul(2))
            .context("NV12 UV plane size overflow")?;
        let mut nv12 = vec![0_u8; y_plane_len + uv_plane_len];

        for row in 0..height {
            let y_row_start = crop_top
                .checked_add(row)
                .context("Y plane crop row overflow")?
                .checked_mul(y_row_stride)
                .and_then(|offset| {
                    crop_left
                        .checked_mul(y_pixel_stride)
                        .and_then(|x_offset| offset.checked_add(x_offset))
                })
                .context("Y plane source row offset overflow")?;

            let y_dst_row_start = row
                .checked_mul(width)
                .context("NV12 Y destination row offset overflow")?;
            if y_pixel_stride == 1 {
                let src_end = y_row_start
                    .checked_add(width)
                    .context("Y plane packed source row end overflow")?;
                nv12[y_dst_row_start..y_dst_row_start + width]
                    .copy_from_slice(&y_plane[y_row_start..src_end]);
            } else {
                for col in 0..width {
                    let y_index = y_row_start
                        .checked_add(
                            col.checked_mul(y_pixel_stride)
                                .context("Y plane pixel offset overflow")?,
                        )
                        .context("Y plane source index overflow")?;
                    nv12[y_dst_row_start + col] = y_plane.get(y_index).copied().unwrap_or(16);
                }
            }
        }

        for row in 0..uv_height {
            let chroma_row = crop_top / 2 + row;
            let u_row_start = chroma_row
                .checked_mul(u_row_stride)
                .and_then(|offset| {
                    (crop_left / 2)
                        .checked_mul(u_pixel_stride)
                        .and_then(|x_offset| offset.checked_add(x_offset))
                })
                .context("U plane source row offset overflow")?;
            let v_row_start = chroma_row
                .checked_mul(v_row_stride)
                .and_then(|offset| {
                    (crop_left / 2)
                        .checked_mul(v_pixel_stride)
                        .and_then(|x_offset| offset.checked_add(x_offset))
                })
                .context("V plane source row offset overflow")?;

            let uv_dst_row_start = y_plane_len
                + row
                    .checked_mul(uv_width)
                    .and_then(|offset| offset.checked_mul(2))
                    .context("NV12 UV destination row offset overflow")?;
            for col in 0..uv_width {
                let u_index = u_row_start
                    .checked_add(
                        col.checked_mul(u_pixel_stride)
                            .context("U plane pixel offset overflow")?,
                    )
                    .context("U plane source index overflow")?;
                let v_index = v_row_start
                    .checked_add(
                        col.checked_mul(v_pixel_stride)
                            .context("V plane pixel offset overflow")?,
                    )
                    .context("V plane source index overflow")?;

                let mut u = u_plane.get(u_index).copied().unwrap_or(128);
                let mut v = v_plane.get(v_index).copied().unwrap_or(128);
                neutralize_zero_chroma(&mut u, &mut v);
                let dst = uv_dst_row_start + col * 2;
                nv12[dst] = u;
                nv12[dst + 1] = v;
            }
        }

        push_staged_nv12_as_rgba(timestamp, width, height, nv12)?;

        let count = STAGED_FRAME_COUNT.load(Ordering::Relaxed) + 1;
        if count <= 5 {
            let u_ptr = u_plane.as_ptr() as usize;
            let v_ptr = v_plane.as_ptr() as usize;
            let chroma_alias =
                if u_pixel_stride == 2 && v_pixel_stride == 2 && u_row_stride == v_row_stride {
                    if u_ptr.checked_add(1) == Some(v_ptr) {
                        Some(AliasedChromaOrder::Uv)
                    } else if v_ptr.checked_add(1) == Some(u_ptr) {
                        Some(AliasedChromaOrder::Vu)
                    } else {
                        None
                    }
                } else {
                    None
                };
            let y_samples: Vec<u8> = y_plane
                .iter()
                .step_by(y_row_stride)
                .take(8)
                .copied()
                .collect();
            let uv_samples: Vec<u8> = u_plane
                .iter()
                .step_by(u_row_stride)
                .take(8)
                .copied()
                .collect();
            info!(
                "YUV staging via image planes: image={}x{} staged={}x{} strides=[{},{},{}] pixel_strides=[{},{},{}] lens=[{},{},{}] alias={:?} y_samples={:?} uv_samples={:?}",
                image_width,
                image_height,
                width,
                height,
                y_row_stride,
                u_row_stride,
                v_row_stride,
                y_pixel_stride,
                u_pixel_stride,
                v_pixel_stride,
                y_plane.len(),
                u_plane.len(),
                v_plane.len(),
                chroma_alias,
                y_samples,
                uv_samples,
            );
        }

        return Ok(true);
    }

    let crop = image.get_crop_rect().ok();
    let mut crop_left = 0usize;
    let mut crop_top = 0usize;
    let mut width = image_width;
    let mut height = image_height;
    if let Some(crop) = crop {
        let right = crop.right.max(crop.left) as usize;
        let bottom = crop.bottom.max(crop.top) as usize;
        let left = crop.left.max(0) as usize;
        let top = crop.top.max(0) as usize;
        let crop_width = right.saturating_sub(left);
        let crop_height = bottom.saturating_sub(top);
        if crop_width > 0 && crop_height > 0 && right <= image_width && bottom <= image_height {
            crop_left = left;
            crop_top = top;
            width = crop_width;
            height = crop_height;
        }
    }

    let y_row_stride = image.get_plane_row_stride(0)? as usize;
    let y_pixel_stride = image.get_plane_pixel_stride(0).unwrap_or(1) as usize;
    let plane0 = image
        .get_plane_data(0)
        .context("decoded YUV image missing plane 0 data")?;
    let plane0_len = plane0.len();
    let y_len = y_row_stride
        .checked_mul(image_height)
        .context("Y plane length overflow")?;
    let uv_row_stride = y_row_stride;
    let uv_len = uv_row_stride
        .checked_mul((image_height + 1) / 2)
        .context("UV plane length overflow")?;
    let has_contiguous_uv = plane0_len >= y_len.saturating_add(uv_len);
    let y_plane = &plane0[..plane0_len.min(y_len)];
    let uv_plane = has_contiguous_uv.then(|| &plane0[y_len..y_len + uv_len]);

    if STAGED_FRAME_COUNT.load(Ordering::Relaxed) < 5 {
        warn!(
            "YUV_420_888 image reported only {image_plane_count} Java-visible planes; plane0_len={plane0_len} y_stride={y_row_stride} y_px_stride={y_pixel_stride} crop={}x{}+{},{} {}",
            width,
            height,
            crop_left,
            crop_top,
            if has_contiguous_uv {
                "using contiguous NV12-style fallback staging"
            } else {
                "falling back to grayscale Y-only staging"
            }
        );
    }

    let rgba_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("RGBA staging size overflow")?;
    let mut rgba = vec![0_u8; rgba_len];
    for row in 0..height {
        let src_row_start = crop_top
            .checked_add(row)
            .context("Y plane crop row overflow")?
            .checked_mul(y_row_stride)
            .and_then(|offset| {
                crop_left
                    .checked_mul(y_pixel_stride)
                    .and_then(|x_offset| offset.checked_add(x_offset))
            })
            .context("Y plane source row offset overflow")?;
        let dst_row_start = row
            .checked_mul(width)
            .context("Y plane destination row offset overflow")?;
        for col in 0..width {
            let y_index = src_row_start
                .checked_add(
                    col.checked_mul(y_pixel_stride)
                        .context("Y plane pixel offset overflow")?,
                )
                .context("Y plane source index overflow")?;
            let y = y_plane.get(y_index).copied().unwrap_or(16);
            let (mut u, mut v) = if let Some(uv_plane) = uv_plane {
                let uv_row_start = (row / 2)
                    .checked_mul(uv_row_stride)
                    .context("NV12 chroma row overflow")?;
                let uv_index = uv_row_start
                    .checked_add(
                        (col / 2)
                            .checked_mul(2)
                            .context("NV12 chroma pixel offset overflow")?,
                    )
                    .context("NV12 chroma source index overflow")?;
                (
                    uv_plane.get(uv_index).copied().unwrap_or(128),
                    uv_plane.get(uv_index + 1).copied().unwrap_or(128),
                )
            } else {
                (128, 128)
            };
            neutralize_zero_chroma(&mut u, &mut v);
            let rgb = bt709_limited_rgb(y, u, v);
            let dst = (dst_row_start + col) * 4;
            rgba[dst] = rgb[0];
            rgba[dst + 1] = rgb[1];
            rgba[dst + 2] = rgb[2];
            rgba[dst + 3] = 255;
        }
    }

    let receiver = crate::video_receiver::get_video_receiver();
    crate::video_receiver::push_rgba_frame(
        receiver.as_ref(),
        timestamp.as_nanos() as u64,
        width as u32,
        height as u32,
        rgba,
    );

    let count = STAGED_FRAME_COUNT.load(Ordering::Relaxed) + 1;
    if count <= 5 {
        let center_x = width / 2;
        let center_y = height / 2;
        let left_x = width / 4;
        let right_x = width.saturating_mul(3) / 4;
        let sample_nv12 = |sample_x: usize, sample_y: usize| -> ([u8; 3], [u8; 3]) {
            let sample_x = sample_x.min(width.saturating_sub(1));
            let sample_y = sample_y.min(height.saturating_sub(1));
            let y_index = sample_y
                .checked_mul(y_row_stride)
                .and_then(|offset| {
                    crop_left
                        .checked_mul(y_pixel_stride)
                        .and_then(|x_offset| offset.checked_add(x_offset))
                })
                .and_then(|offset| offset.checked_add(sample_x * y_pixel_stride))
                .unwrap_or(0);
            let y = y_plane.get(y_index).copied().unwrap_or(16);
            let (mut u, mut v) = if let Some(uv_plane) = uv_plane {
                let uv_index = (sample_y / 2) * uv_row_stride + (sample_x / 2) * 2;
                (
                    uv_plane.get(uv_index).copied().unwrap_or(128),
                    uv_plane.get(uv_index + 1).copied().unwrap_or(128),
                )
            } else {
                (128, 128)
            };
            neutralize_zero_chroma(&mut u, &mut v);
            ([y, u, v], bt709_limited_rgb(y, u, v))
        };
        let left_sample = sample_nv12(left_x, center_y);
        let center_sample = sample_nv12(center_x, center_y);
        let right_sample = sample_nv12(right_x, center_y);
        info!(
            "YUV staging fallback: image={}x{} staged={}x{} y_stride={} y_px_stride={} uv_stride={} plane_lens=[{},{}] left_yuv=[{},{},{}] left_rgb={:?} center_yuv=[{},{},{}] center_rgb={:?} right_yuv=[{},{},{}] right_rgb={:?}",
            image_width,
            image_height,
            width,
            height,
            y_row_stride,
            y_pixel_stride,
            uv_row_stride,
            y_plane.len(),
            uv_plane.map(|plane| plane.len()).unwrap_or(0),
            left_sample.0[0],
            left_sample.0[1],
            left_sample.0[2],
            left_sample.1,
            center_sample.0[0],
            center_sample.0[1],
            center_sample.0[2],
            center_sample.1,
            right_sample.0[0],
            right_sample.0[1],
            right_sample.0[2],
            right_sample.1,
        );
    }

    Ok(true)
}

fn stage_decoded_image(image: &Image, timestamp: Duration) -> Result<bool> {
    match image.get_format()? {
        ImageFormat::YUV_420_888 => stage_yuv420_image_as_nv12(image, timestamp),
        ImageFormat::RGBA_8888 | ImageFormat::RGBX_8888 => stage_rgba_image(image, timestamp),
        ImageFormat::PRIVATE => Ok(false),
        other => {
            warn!("decoded image format {other:?} is not staged to the legacy renderer");
            Ok(false)
        }
    }
}

fn decoder_lifecycle(
    config: VideoDecoderConfig,
    csd_0: Vec<u8>,
    frame_result_callback: Weak<dyn Fn(Result<Duration>) + Send + Sync + 'static>,
    running: Arc<RelaxedAtomic>,
    decoder_sink: Arc<Mutex<Option<SharedMediaCodec>>>,
    decoder_ready_notifier: Arc<Condvar>,
    image_queue: Arc<Mutex<VecDeque<QueuedImage>>>,
    image_reader: &mut ImageReader,
) -> Result<()> {
    let mime = mime_for_codec(config.codec);

    let format = MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", config.width);
    format.set_i32("height", config.height);
    format.set_buffer("csd-0", &csd_0);

    for (key, prop) in &config.options {
        let maybe_error = match prop.ty {
            MediacodecPropType::Float => prop
                .value
                .parse::<f32>()
                .map(|value| format.set_f32(key, value))
                .map_err(|e| anyhow!("{e}")),
            MediacodecPropType::Int32 => prop
                .value
                .parse::<i32>()
                .map(|value| format.set_i32(key, value))
                .map_err(|e| anyhow!("{e}")),
            MediacodecPropType::Int64 => prop
                .value
                .parse::<i64>()
                .map(|value| format.set_i64(key, value))
                .map_err(|e| anyhow!("{e}")),
            MediacodecPropType::String => Ok(format.set_str(key, &prop.value)),
        };

        if let Err(e) = maybe_error {
            error!("Failed to set property {key} to {}: {e}", prop.value);
        }
    }

    info!("Using AMediaCodec format:{} ", format);

    image_reader.set_image_listener(Box::new({
        let image_queue = Arc::clone(&image_queue);
        let frame_result_callback = frame_result_callback.clone();
        move |image_reader| {
            if USE_CPU_STAGING_DECODER {
                match image_reader.acquire_latest_image() {
                    Ok(Some(image)) => {
                        let timestamp = match image.get_timestamp() {
                            Ok(timestamp) => Duration::from_nanos(timestamp as u64),
                            Err(e) => {
                                error!("ImageReader timestamp error: {e}");
                                return;
                            }
                        };

                        if let Some(callback) = frame_result_callback.upgrade() {
                            callback(Ok(timestamp));
                        }

                        match stage_decoded_image(&image, timestamp) {
                            Ok(true) => {
                                let count = STAGED_FRAME_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                                if count <= 5 || count % 120 == 0 {
                                    info!(
                                        "staged decoded frame {} via {:?}: {}x{} timestamp_ns={}",
                                        count,
                                        image.get_format().unwrap_or(ImageFormat::PRIVATE),
                                        image.get_width().unwrap_or_default(),
                                        image.get_height().unwrap_or_default(),
                                        timestamp.as_nanos()
                                    );
                                }
                            }
                            Ok(false) => {}
                            Err(e) => {
                                error!("failed to stage decoded image: {e:#}");
                            }
                        }
                    }
                    Ok(None) => {
                        warn!("ImageReader callback found no latest image available");
                    }
                    Err(e) => {
                        error!("ImageReader latest-image error: {e}");
                    }
                }

                return;
            }

            let mut image_queue_lock = image_queue.lock();
            let mut dropped = 0usize;
            if image_queue_lock.front().map(|queued| queued.in_use).unwrap_or(false) {
                while image_queue_lock.len() > 1 {
                    image_queue_lock.pop_back();
                    dropped += 1;
                }
            } else {
                dropped = image_queue_lock.len();
                image_queue_lock.clear();
            }

            if dropped > 0 {
                let overflow_count = QUEUE_OVERFLOW_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if overflow_count <= 5 || overflow_count % 120 == 0 {
                    warn!(
                        "Video frame queue trimmed, dropped {dropped} stale frame(s) (count={overflow_count})"
                    );
                }
            }

            match image_reader.acquire_next_image() {
                Ok(Some(image)) => {
                    let timestamp = match image.get_timestamp() {
                        Ok(timestamp) => Duration::from_nanos(timestamp as u64),
                        Err(e) => {
                            error!("ImageReader timestamp error: {e}");
                            return;
                        }
                    };

                    if let Some(callback) = frame_result_callback.upgrade() {
                        callback(Ok(timestamp));
                    }

                    image_queue_lock.push_back(QueuedImage {
                        timestamp,
                        image,
                        in_use: false,
                    });
                }
                Ok(None) => {
                    warn!("ImageReader callback found no image available");
                }
                Err(e) => {
                    error!("ImageReader error: {e}");
                    image_queue_lock.clear();
                }
            }
        }
    }))?;

    image_reader.set_buffer_removed_listener(Box::new(|_, _| ()))?;

    let decoder = if config.force_software_decoder {
        decoder_setup(config.codec, true, &format, &image_reader)?
    } else {
        match decoder_setup(config.codec, false, &format, &image_reader) {
            Ok(d) => d,
            Err(e) => {
                error!("Attempting software fallback due to error in default decoder: {e:#}");
                decoder_setup(config.codec, true, &format, &image_reader)?
            }
        }
    };

    let mut decoder = Some(Arc::new(FakeThreadSafe(decoder)));

    {
        let mut decoder_lock = decoder_sink.lock();
        *decoder_lock = Some(Arc::clone(decoder.as_ref().unwrap()));
        decoder_ready_notifier.notify_one();
    }

    let mut consecutive_errors = 0_u32;

    while running.value() {
        match decoder
            .as_ref()
            .unwrap()
            .dequeue_output_buffer(Duration::from_millis(1))
        {
            Ok(Some(buffer)) => {
                consecutive_errors = 0;
                // For ImageReader-based decoding we release immediately.
                // release_output_buffer_at_time can fail on some devices when
                // the surface is an ImageReader, causing subsequent
                // dequeue_output_buffer calls to return ErrorUnknown.
                if let Err(e) = decoder
                    .as_ref()
                    .unwrap()
                    .release_output_buffer(buffer, true)
                {
                    error!("Decoder release error: {e}");
                    consecutive_errors += 1;
                }
            }
            Ok(None) => {
                consecutive_errors = 0;
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(NdkMediaError::UnknownResult(status))
                if status.0 == ffi::AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED as i32 =>
            {
                consecutive_errors = 0;
                info!("Decoder output format changed");
                continue;
            }
            Err(NdkMediaError::UnknownResult(status))
                if status.0 == ffi::AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED as i32 =>
            {
                consecutive_errors = 0;
                info!("Decoder output buffers changed");
                continue;
            }
            Err(e) => {
                consecutive_errors += 1;
                // Log decoder errors but do not kill the thread; transient
                // stalls (e.g. surface back-pressure) recover automatically.
                error!("Decoder dequeue error: {e} (consecutive={consecutive_errors})");

                if consecutive_errors >= 5 {
                    warn!("Decoder persistent error, stopping decoder thread to avoid unsafe MediaCodec surface reset");
                    // Remove decoder from sink so the render thread doesn't
                    // race with NAL pushes while we tear down.
                    decoder_sink.lock().take();
                    if let Some(ref d) = decoder {
                        let _ = d.stop();
                    }
                    decoder = None;
                    image_queue.lock().clear();
                    break;
                }

                thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
    }

    decoder_sink.lock().take();
    if let Some(d) = decoder.take() {
        d.stop()?;
    }

    Ok(())
}

pub fn video_decoder_split(
    config: VideoDecoderConfig,
    csd_0: Vec<u8>,
    frame_result_callback: impl Fn(Result<Duration>) + Send + Sync + 'static,
) -> Result<(VideoDecoderSink, VideoDecoderSource)> {
    let running = Arc::new(RelaxedAtomic::new(true));
    let decoder_sink = Arc::new(Mutex::new(None::<SharedMediaCodec>));
    let decoder_ready_notifier = Arc::new(Condvar::new());
    let image_queue = Arc::new(Mutex::new(VecDeque::<QueuedImage>::new()));

    let dequeue_thread = thread::spawn({
        let config = config.clone();
        let running = Arc::clone(&running);
        let decoder_sink = Arc::clone(&decoder_sink);
        let decoder_ready_notifier = Arc::clone(&decoder_ready_notifier);
        let image_queue = Arc::clone(&image_queue);
        move || {
            let mut image_reader = match ImageReader::new_with_usage(
                if USE_CPU_STAGING_DECODER {
                    config.width
                } else {
                    1
                },
                if USE_CPU_STAGING_DECODER {
                    config.height
                } else {
                    1
                },
                if USE_CPU_STAGING_DECODER {
                    CPU_STAGING_IMAGE_FORMAT
                } else {
                    ImageFormat::PRIVATE
                },
                HardwareBufferUsage(ffi::AHardwareBuffer_UsageFlags(
                    if USE_CPU_STAGING_DECODER {
                        HardwareBufferUsage::CPU_READ_OFTEN.0 .0
                    } else {
                        HardwareBufferUsage::GPU_SAMPLED_IMAGE.0 .0
                    },
                )),
                if USE_CPU_STAGING_DECODER {
                    CPU_STAGING_MAX_IMAGES
                } else {
                    MAX_BUFFERING_FRAMES as i32
                },
            ) {
                Ok(reader) => reader,
                Err(e) => {
                    frame_result_callback(Err(anyhow!("{e}")));
                    return;
                }
            };

            let frame_result_callback: Arc<dyn Fn(Result<Duration>) + Send + Sync + 'static> =
                Arc::new(frame_result_callback);

            if let Err(e) = decoder_lifecycle(
                config,
                csd_0,
                Arc::downgrade(&frame_result_callback),
                running,
                decoder_sink,
                decoder_ready_notifier,
                Arc::clone(&image_queue),
                &mut image_reader,
            ) {
                frame_result_callback(Err(e));
            }

            image_queue.lock().clear();
            // On this Qualcomm/Android 10 runtime, AImageReader_delete can
            // segfault after MediaCodec enters ErrorUnknown. The process is
            // long-lived and creates one decoder reader, so leaking it is safer
            // than taking down the VR session during teardown.
            std::mem::forget(image_reader);
        }
    });

    {
        let mut decoder_lock = decoder_sink.lock();
        if decoder_lock.is_none() {
            decoder_ready_notifier.wait(&mut decoder_lock);
        }
    }

    let sink = VideoDecoderSink {
        inner: decoder_sink,
    };
    let source = VideoDecoderSource {
        running,
        dequeue_thread: Some(dequeue_thread),
        image_queue,
        config,
        buffering_running_average: 0.0,
    };

    Ok((sink, source))
}

#[derive(Default)]
struct RelaxedAtomic(std::sync::atomic::AtomicBool);

impl RelaxedAtomic {
    pub const fn new(initial_value: bool) -> Self {
        Self(std::sync::atomic::AtomicBool::new(initial_value))
    }

    pub fn value(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set(&self, value: bool) {
        self.0.store(value, std::sync::atomic::Ordering::Relaxed);
    }
}
