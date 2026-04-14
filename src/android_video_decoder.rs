//! Minimal Android MediaCodec bridge for reconstructed ALVR video NALs.
//!
//! This is a pragmatic CPU-readback path: it proves the ALVR stream is decodable
//! and reuses the already-working RGBA texture upload path. A later pass should
//! replace this with an ImageReader/AHardwareBuffer zero-copy path.

use std::{
    cmp,
    collections::VecDeque,
    ffi::{CStr, CString},
    ptr,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc,
    },
    thread,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use log::{info, warn};
use ndk_sys as ffi;
use parking_lot::Mutex;

use crate::video_receiver::{self, AlvrVideoReceiver};

const DECODER_QUEUE_DEPTH: usize = 8;
const DECODER_INPUT_TIMEOUT_US: i64 = 2_000;
const DECODER_OUTPUT_TIMEOUT_US: i64 = 1_000;
const DECODER_IDLE_TIMEOUT: Duration = Duration::from_millis(4);
const DECODER_CONFIG_WIDTH: i32 = 512;
const DECODER_CONFIG_HEIGHT: i32 = 1024;
const DECODER_MAX_RGBA_DIMENSION: usize = 1024;
const DECODER_LOG_EVERY_FRAME: u64 = 120;
const COLOR_FORMAT_YUV420_PLANAR: i32 = 19;
const COLOR_FORMAT_YUV420_SEMIPLANAR: i32 = 21;
const COLOR_FORMAT_YUV420_FLEXIBLE: i32 = 0x7F42_0888;
const COLOR_FORMAT_QCOM_YUV420_SEMIPLANAR32M: i32 = 0x7FA3_0C04;

pub struct AlvrAndroidVideoDecoder {
    receiver: Arc<AlvrVideoReceiver>,
    sender: Mutex<Option<SyncSender<DecoderCommand>>>,
    dropped_before_ready: AtomicU64,
    dropped_after_ready: AtomicU64,
}

impl AlvrAndroidVideoDecoder {
    pub fn new() -> Self {
        Self {
            receiver: video_receiver::get_video_receiver(),
            sender: Mutex::new(None),
            dropped_before_ready: AtomicU64::new(0),
            dropped_after_ready: AtomicU64::new(0),
        }
    }

    pub fn configure(
        &self,
        mime_type: &'static str,
        codec_label: &str,
        config_buffer: Vec<u8>,
    ) -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(DECODER_QUEUE_DEPTH);
        if let Some(old_sender) = self.sender.lock().replace(sender) {
            let _ = old_sender.try_send(DecoderCommand::Stop);
        }

        let video_receiver = Arc::clone(&self.receiver);
        let codec_label = codec_label.to_string();
        thread::Builder::new()
            .name("alvr-mediacodec-decoder".to_string())
            .spawn(move || {
                if let Err(err) = run_decoder_thread(
                    receiver,
                    video_receiver,
                    mime_type,
                    codec_label,
                    config_buffer,
                ) {
                    warn!("ALVR MediaCodec decoder thread exited with error: {err:#}");
                }
            })
            .context("spawn ALVR MediaCodec decoder thread")?;

        Ok(())
    }

    pub fn push_nal(&self, timestamp_ns: u64, is_idr: bool, data: Vec<u8>) {
        let Some(sender) = self.sender.lock().as_ref().cloned() else {
            let dropped = self.dropped_before_ready.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped <= 5 || dropped % DECODER_LOG_EVERY_FRAME == 0 {
                warn!(
                    "dropping ALVR video NAL before decoder is configured: dropped={} bytes={} is_idr={}",
                    dropped,
                    data.len(),
                    is_idr
                );
            }
            return;
        };

        match sender.try_send(DecoderCommand::Nal {
            timestamp_ns,
            is_idr,
            data,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(DecoderCommand::Nal { data, is_idr, .. })) => {
                let dropped = self.dropped_after_ready.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped <= 5 || dropped % DECODER_LOG_EVERY_FRAME == 0 {
                    warn!(
                        "dropping ALVR video NAL because decoder queue is full: dropped={} bytes={} is_idr={}",
                        dropped,
                        data.len(),
                        is_idr
                    );
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                let dropped = self.dropped_after_ready.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped <= 5 || dropped % DECODER_LOG_EVERY_FRAME == 0 {
                    warn!("dropping ALVR video NAL because decoder thread is disconnected");
                }
            }
            Err(TrySendError::Full(DecoderCommand::Stop)) => {}
        }
    }
}

impl Default for AlvrAndroidVideoDecoder {
    fn default() -> Self {
        Self::new()
    }
}

enum DecoderCommand {
    Nal {
        timestamp_ns: u64,
        is_idr: bool,
        data: Vec<u8>,
    },
    Stop,
}

fn run_decoder_thread(
    receiver: Receiver<DecoderCommand>,
    video_receiver: Arc<AlvrVideoReceiver>,
    mime_type: &'static str,
    codec_label: String,
    config_buffer: Vec<u8>,
) -> Result<()> {
    let mut decoder = RawMediaDecoder::create(mime_type, &config_buffer)
        .with_context(|| format!("create MediaCodec decoder for {codec_label}"))?;
    let mut queued_nals = 0_u64;
    let mut decoded_frames = 0_u64;
    let mut fallback_pts_us = 0_u64;

    info!(
        "ALVR MediaCodec decoder configured: codec={} mime={} config_bytes={}",
        codec_label,
        mime_type,
        config_buffer.len()
    );

    loop {
        match receiver.recv_timeout(DECODER_IDLE_TIMEOUT) {
            Ok(DecoderCommand::Nal {
                timestamp_ns,
                is_idr,
                data,
            }) => {
                let pts_us = if timestamp_ns == 0 {
                    fallback_pts_us = fallback_pts_us.saturating_add(11_111);
                    fallback_pts_us
                } else {
                    timestamp_ns / 1_000
                };

                if decoder.queue_nal(&data, pts_us, timestamp_ns)? {
                    queued_nals = queued_nals.wrapping_add(1);
                    if queued_nals <= 5 || is_idr || queued_nals % DECODER_LOG_EVERY_FRAME == 0 {
                        info!(
                            "queued ALVR NAL into MediaCodec: queued={} bytes={} is_idr={} pts_us={}",
                            queued_nals,
                            data.len(),
                            is_idr,
                            pts_us
                        );
                    }
                } else if is_idr {
                    warn!(
                        "MediaCodec input buffer unavailable while queueing IDR NAL: bytes={}",
                        data.len()
                    );
                }
            }
            Ok(DecoderCommand::Stop) => {
                info!("ALVR MediaCodec decoder received stop command");
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                info!("ALVR MediaCodec decoder input channel disconnected");
                break;
            }
        }

        decoder.drain_output(video_receiver.as_ref(), &mut decoded_frames)?;
    }

    decoder.stop();
    Ok(())
}

/// AImageReader wrapper for zero-copy MediaCodec Surface output.
/// MediaCodec renders into this reader's buffers; we acquire AHardwareBuffers
/// and pass them through the GPU upload pipeline without any CPU readback.
struct ImageReaderHandle {
    reader: *mut ffi::AImageReader,
}

impl ImageReaderHandle {
    fn new_for_decoder_surface(width: i32, height: i32, max_images: i32) -> Result<Self> {
        let gpu_usage = ffi::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE.0
            | ffi::AHardwareBuffer_UsageFlags::AHARDWAREBUFFER_USAGE_GPU_COLOR_OUTPUT.0;
        let mut last_error = None;
        for (label, format, usage) in [
            (
                "PRIVATE_WITH_GPU_USAGE",
                ffi::AIMAGE_FORMATS::AIMAGE_FORMAT_PRIVATE.0 as i32,
                Some(gpu_usage),
            ),
            (
                "PRIVATE",
                ffi::AIMAGE_FORMATS::AIMAGE_FORMAT_PRIVATE.0 as i32,
                None,
            ),
            (
                "RGBA_8888",
                ffi::AIMAGE_FORMATS::AIMAGE_FORMAT_RGBA_8888.0 as i32,
                None,
            ),
        ] {
            let attempt = match usage {
                Some(usage) => Self::new_with_usage(width, height, format, usage, max_images),
                None => Self::new(width, height, format, max_images),
            };
            match attempt {
                Ok(reader) => {
                    info!(
                        "created AImageReader for MediaCodec Surface output: {}x{} format={}({}) max_images={}",
                        width, height, label, format, max_images
                    );
                    return Ok(reader);
                }
                Err(err) => {
                    warn!(
                        "AImageReader_new failed for decoder surface format {}({}): {err:#}",
                        label, format
                    );
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no AImageReader formats were tried")))
    }

    fn new(width: i32, height: i32, format: i32, max_images: i32) -> Result<Self> {
        let mut reader: *mut ffi::AImageReader = ptr::null_mut();
        let status = unsafe {
            ffi::AImageReader_new(
                width,
                height,
                format,
                max_images,
                &mut reader as *mut *mut ffi::AImageReader,
            )
        };
        if status != ffi::media_status_t::AMEDIA_OK {
            bail!("AImageReader_new failed: status={}", status.0);
        }
        if reader.is_null() {
            bail!(
                "AImageReader_new returned null for {}x{} format={}",
                width,
                height,
                format
            );
        }
        Ok(Self { reader })
    }

    fn new_with_usage(
        width: i32,
        height: i32,
        format: i32,
        usage: u64,
        max_images: i32,
    ) -> Result<Self> {
        let mut reader: *mut ffi::AImageReader = ptr::null_mut();
        let status = unsafe {
            ffi::AImageReader_newWithUsage(
                width,
                height,
                format,
                usage,
                max_images,
                &mut reader as *mut *mut ffi::AImageReader,
            )
        };
        if status != ffi::media_status_t::AMEDIA_OK {
            bail!(
                "AImageReader_newWithUsage failed: status={} usage=0x{usage:x}",
                status.0
            );
        }
        if reader.is_null() {
            bail!(
                "AImageReader_newWithUsage returned null for {}x{} format={} usage=0x{usage:x}",
                width,
                height,
                format
            );
        }
        Ok(Self { reader })
    }

    fn get_window(&self) -> Result<*mut ffi::ANativeWindow> {
        let mut window: *mut ffi::ANativeWindow = ptr::null_mut();
        let status = unsafe {
            ffi::AImageReader_getWindow(self.reader, &mut window as *mut *mut ffi::ANativeWindow)
        };
        check_media_status(status, "AImageReader_getWindow")?;
        Ok(window)
    }

    /// Acquire the latest decoded image. Returns null if no new buffer is available
    /// yet — that is not an error, callers should return and try again.
    fn acquire_latest_image(&self) -> Result<*mut ffi::AImage> {
        let mut image: *mut ffi::AImage = ptr::null_mut();
        let status = unsafe {
            ffi::AImageReader_acquireLatestImage(self.reader, &mut image as *mut *mut ffi::AImage)
        };
        if status.0 == ffi::media_status_t::AMEDIA_IMGREADER_NO_BUFFER_AVAILABLE.0 {
            return Ok(ptr::null_mut());
        }
        check_media_status(status, "AImageReader_acquireLatestImage")?;
        Ok(image)
    }

    fn get_hardware_buffer(image: *mut ffi::AImage) -> Result<*mut ffi::AHardwareBuffer> {
        let mut buffer: *mut ffi::AHardwareBuffer = ptr::null_mut();
        let status = unsafe {
            ffi::AImage_getHardwareBuffer(image, &mut buffer as *mut *mut ffi::AHardwareBuffer)
        };
        check_media_status(status, "AImage_getHardwareBuffer")?;
        Ok(buffer)
    }

    fn get_image_size(image: *mut ffi::AImage) -> Result<(u32, u32)> {
        let mut width = 0_i32;
        let mut height = 0_i32;
        let width_status = unsafe { ffi::AImage_getWidth(image, &mut width) };
        check_media_status(width_status, "AImage_getWidth")?;
        let height_status = unsafe { ffi::AImage_getHeight(image, &mut height) };
        check_media_status(height_status, "AImage_getHeight")?;
        if width <= 0 || height <= 0 {
            bail!("AImage reported invalid dimensions {width}x{height}");
        }
        Ok((width as u32, height as u32))
    }

    fn delete_image(image: *mut ffi::AImage) {
        if !image.is_null() {
            unsafe { ffi::AImage_delete(image) };
        }
    }

    fn delete(&mut self) {
        if !self.reader.is_null() {
            unsafe { ffi::AImageReader_delete(self.reader) };
            self.reader = ptr::null_mut();
        }
    }
}

impl Drop for ImageReaderHandle {
    fn drop(&mut self) {
        self.delete();
    }
}

struct RawMediaDecoder {
    codec: *mut ffi::AMediaCodec,
    output_format: DecoderOutputFormat,
    queued_timestamps: VecDeque<(u64, u64)>,
    image_reader: Option<ImageReaderHandle>,
}

impl RawMediaDecoder {
    fn create(mime_type: &str, config_buffer: &[u8]) -> Result<Self> {
        let mut last_error = None;
        for codec_name in [None, software_decoder_name(mime_type)] {
            match Self::try_create(mime_type, config_buffer, codec_name) {
                Ok(decoder) => return Ok(decoder),
                Err(err) => {
                    let label = codec_name.unwrap_or("<default>");
                    warn!("MediaCodec decoder attempt failed for {label}: {err:#}");
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no decoder attempts were available")))
    }

    fn try_create(
        mime_type: &str,
        config_buffer: &[u8],
        codec_name: Option<&'static str>,
    ) -> Result<Self> {
        let codec = unsafe {
            match codec_name {
                Some(name) => {
                    let name = CString::new(name).context("codec name contains NUL")?;
                    ffi::AMediaCodec_createCodecByName(name.as_ptr())
                }
                None => {
                    let mime = CString::new(mime_type).context("mime type contains NUL")?;
                    ffi::AMediaCodec_createDecoderByType(mime.as_ptr())
                }
            }
        };
        if codec.is_null() {
            bail!(
                "AMediaCodec_create{} returned null",
                if codec_name.is_some() {
                    "CodecByName"
                } else {
                    "DecoderByType"
                }
            );
        }

        let mut decoder = Self {
            codec,
            output_format: DecoderOutputFormat::default(),
            queued_timestamps: VecDeque::new(),
            image_reader: None,
        };

        let configure_result = decoder.configure_and_start(mime_type, config_buffer);
        if let Err(err) = configure_result {
            return Err(err);
        }

        let label = codec_name.unwrap_or("<default>");
        info!("created MediaCodec decoder using {label}");
        Ok(decoder)
    }

    fn configure_and_start(&mut self, mime_type: &str, config_buffer: &[u8]) -> Result<()> {
        let width = DECODER_CONFIG_WIDTH;
        let height = DECODER_CONFIG_HEIGHT;

        // Create AImageReader for zero-copy Surface output. PRIVATE lets the
        // decoder choose a GPU-native layout; RGBA_8888 is kept as a fallback.
        self.image_reader = Some(
            ImageReaderHandle::new_for_decoder_surface(width, height, DECODER_QUEUE_DEPTH as i32)
                .context("create AImageReader")?,
        );

        let surface = self
            .image_reader
            .as_ref()
            .context("AImageReader was not retained")?
            .get_window()
            .context("get AImageReader window")?;

        let format = MediaFormatHandle::new().context("create MediaCodec format")?;
        format.set_str("mime", mime_type)?;
        format.set_i32("width", width)?;
        format.set_i32("height", height)?;
        format.set_i32("max-input-size", 8 * 1024 * 1024)?;
        format.set_i32("color-format", COLOR_FORMAT_YUV420_FLEXIBLE)?;
        if !config_buffer.is_empty() {
            format.set_buffer("csd-0", config_buffer)?;
        }

        info!(
            "configuring MediaCodec with AImageReader surface: {}",
            format.to_string_lossy()
        );
        let status = unsafe {
            ffi::AMediaCodec_configure(self.codec, format.ptr, surface, ptr::null_mut(), 0)
        };
        check_media_status(status, "AMediaCodec_configure (surface)")?;

        let status = unsafe { ffi::AMediaCodec_start(self.codec) };
        check_media_status(status, "AMediaCodec_start")?;

        self.refresh_output_format("initial output format")?;
        Ok(())
    }

    fn queue_nal(&mut self, data: &[u8], pts_us: u64, timestamp_ns: u64) -> Result<bool> {
        let index =
            unsafe { ffi::AMediaCodec_dequeueInputBuffer(self.codec, DECODER_INPUT_TIMEOUT_US) };
        if index == ffi::AMEDIACODEC_INFO_TRY_AGAIN_LATER as ffi::ssize_t {
            return Ok(false);
        }
        if index < 0 {
            bail!("AMediaCodec_dequeueInputBuffer returned {index}");
        }

        let mut input_capacity = 0 as ffi::size_t;
        let input = unsafe {
            ffi::AMediaCodec_getInputBuffer(
                self.codec,
                index as ffi::size_t,
                &mut input_capacity as *mut ffi::size_t,
            )
        };
        if input.is_null() {
            bail!("AMediaCodec_getInputBuffer returned null for index {index}");
        }
        let input_capacity = input_capacity as usize;
        if data.len() > input_capacity {
            bail!(
                "ALVR NAL too large for MediaCodec input buffer: got {} bytes, capacity {}",
                data.len(),
                input_capacity
            );
        }

        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), input, data.len());
        }

        let status = unsafe {
            ffi::AMediaCodec_queueInputBuffer(
                self.codec,
                index as ffi::size_t,
                0,
                data.len() as ffi::size_t,
                pts_us,
                0,
            )
        };
        check_media_status(status, "AMediaCodec_queueInputBuffer")?;
        self.queued_timestamps
            .push_back((pts_us, timestamp_ns.max(pts_us.saturating_mul(1_000))));
        while self.queued_timestamps.len() > 1024 {
            self.queued_timestamps.pop_front();
        }
        Ok(true)
    }

    fn drain_output(
        &mut self,
        video_receiver: &AlvrVideoReceiver,
        decoded_frames: &mut u64,
    ) -> Result<()> {
        for _ in 0..8 {
            // Drain any pending MediaCodec output buffer notifications
            let mut info: ffi::AMediaCodecBufferInfo = unsafe { std::mem::zeroed() };
            let index = unsafe {
                ffi::AMediaCodec_dequeueOutputBuffer(
                    self.codec,
                    &mut info as *mut ffi::AMediaCodecBufferInfo,
                    DECODER_OUTPUT_TIMEOUT_US,
                )
            };

            if index == ffi::AMEDIACODEC_INFO_TRY_AGAIN_LATER as ffi::ssize_t {
                // No output ready yet — fall through to try ImageReader
            } else if index == ffi::AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED as ffi::ssize_t {
                self.refresh_output_format("output format changed")?;
                continue;
            } else if index == ffi::AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED as ffi::ssize_t {
                info!("MediaCodec output buffers changed");
                continue;
            } else if index >= 0 {
                // Surface output only becomes visible to AImageReader when rendered here.
                let release_index = index as ffi::size_t;
                self.release_output(release_index, true)?;
            } else {
                warn!("AMediaCodec_dequeueOutputBuffer returned unexpected event {index}");
                continue;
            }

            // Acquire the decoded image from AImageReader
            // Do this in a limited scope so the image_reader borrow ends before
            // we call mutable self methods below
            let image = {
                let image_reader = match self.image_reader.as_ref() {
                    Some(r) => r,
                    None => bail!("drain_output called but no image_reader configured"),
                };
                match image_reader.acquire_latest_image() {
                    Ok(img) if img.is_null() => return Ok(()),
                    Ok(img) => img,
                    Err(err) => {
                        warn!("AImageReader_acquireLatestImage failed: {err:#}");
                        continue;
                    }
                }
            };

            // Get AHardwareBuffer pointer from the image
            let buffer_ptr = match ImageReaderHandle::get_hardware_buffer(image) {
                Ok(buffer) if !buffer.is_null() => buffer as usize,
                Ok(_) => {
                    warn!("AImage_getHardwareBuffer returned null buffer");
                    ImageReaderHandle::delete_image(image);
                    continue;
                }
                Err(err) => {
                    warn!("AImage_getHardwareBuffer failed: {err:#}");
                    ImageReaderHandle::delete_image(image);
                    continue;
                }
            };

            let hardware_buffer_lease =
                match video_receiver::HardwareBufferLease::acquire(buffer_ptr) {
                    Some(lease) => lease,
                    None => {
                        warn!("failed to acquire AHardwareBuffer lease for decoded frame");
                        ImageReaderHandle::delete_image(image);
                        continue;
                    }
                };

            *decoded_frames = decoded_frames.wrapping_add(1);

            let timestamp_ns = if info.presentationTimeUs > 0 {
                self.take_queued_timestamp_ns(info.presentationTimeUs as u64)
                    .unwrap_or_else(|| (info.presentationTimeUs as u64).saturating_mul(1_000))
            } else {
                decoded_frames.saturating_mul(11_111_111)
            };

            crate::client::report_alvr_frame_decoded(Duration::from_nanos(timestamp_ns));

            let (width, height) = match ImageReaderHandle::get_image_size(image) {
                Ok(size) => size,
                Err(err) => {
                    warn!("failed to query decoded AImage size: {err:#}");
                    ImageReaderHandle::delete_image(image);
                    continue;
                }
            };
            let row_pitch = width.saturating_mul(4);

            ImageReaderHandle::delete_image(image);

            video_receiver::push_video_frame(
                video_receiver,
                timestamp_ns,
                buffer_ptr,
                width,
                height,
                row_pitch,
                Some(hardware_buffer_lease),
            );

            if *decoded_frames <= 5 || *decoded_frames % DECODER_LOG_EVERY_FRAME == 0 {
                info!(
                    "decoded ALVR frame via MediaCodec Surface: frames={} size={}x{} timestamp_ns={}",
                    *decoded_frames,
                    width,
                    height,
                    timestamp_ns
                );
            }
        }

        Ok(())
    }

    fn refresh_output_format(&mut self, reason: &str) -> Result<()> {
        let Some(format) = MediaFormatHandle::from_codec_output(self.codec) else {
            warn!("MediaCodec {reason}: AMediaCodec_getOutputFormat returned null");
            return Ok(());
        };

        let new_format = DecoderOutputFormat::from_media_format(&format);
        info!(
            "MediaCodec {reason}: {} parsed={new_format:?}",
            format.to_string_lossy()
        );

        // Check for resolution changes — requires recreating the AImageReader
        if (new_format.width != self.output_format.width
            || new_format.height != self.output_format.height)
            && self.output_format.width > 0
            && self.output_format.height > 0
        {
            info!(
                "resolution changed from {}x{} to {}x{} — flagging AImageReader reconfigure",
                self.output_format.width,
                self.output_format.height,
                new_format.width,
                new_format.height
            );
            self.replace_image_reader_surface(new_format.width as i32, new_format.height as i32)
                .context("replace AImageReader after MediaCodec resolution change")?;
        }

        self.output_format = new_format;
        Ok(())
    }

    fn replace_image_reader_surface(&mut self, width: i32, height: i32) -> Result<()> {
        let image_reader =
            ImageReaderHandle::new_for_decoder_surface(width, height, DECODER_QUEUE_DEPTH as i32)
                .with_context(|| format!("create replacement AImageReader for {width}x{height}"))?;
        let surface = image_reader
            .get_window()
            .context("get replacement AImageReader window")?;
        let status = unsafe { ffi::AMediaCodec_setOutputSurface(self.codec, surface) };
        check_media_status(status, "AMediaCodec_setOutputSurface")?;
        let old_reader = self.image_reader.replace(image_reader);
        drop(old_reader);
        info!(
            "replaced MediaCodec output surface with AImageReader sized {}x{}",
            width, height
        );
        Ok(())
    }

    fn take_queued_timestamp_ns(&mut self, pts_us: u64) -> Option<u64> {
        let index = self
            .queued_timestamps
            .iter()
            .position(|(queued_pts_us, _)| *queued_pts_us == pts_us)?;
        self.queued_timestamps
            .remove(index)
            .map(|(_, timestamp_ns)| timestamp_ns)
    }

    fn release_output(&mut self, index: ffi::size_t, render_to_surface: bool) -> Result<()> {
        let status =
            unsafe { ffi::AMediaCodec_releaseOutputBuffer(self.codec, index, render_to_surface) };
        check_media_status(status, "AMediaCodec_releaseOutputBuffer")
    }

    fn stop(&mut self) {
        let image_reader = self.image_reader.take();

        unsafe {
            let stop_status = ffi::AMediaCodec_stop(self.codec);
            if stop_status != ffi::media_status_t::AMEDIA_OK {
                warn!(
                    "AMediaCodec_stop failed during shutdown: status={}",
                    stop_status.0
                );
            }
        }

        drop(image_reader);
    }
}

impl Drop for RawMediaDecoder {
    fn drop(&mut self) {
        unsafe {
            let status = ffi::AMediaCodec_delete(self.codec);
            if status != ffi::media_status_t::AMEDIA_OK {
                warn!("AMediaCodec_delete failed: status={}", status.0);
            }
        }
    }
}

struct MediaFormatHandle {
    ptr: *mut ffi::AMediaFormat,
}

impl MediaFormatHandle {
    fn new() -> Result<Self> {
        let ptr = unsafe { ffi::AMediaFormat_new() };
        if ptr.is_null() {
            bail!("AMediaFormat_new returned null");
        }
        Ok(Self { ptr })
    }

    fn from_codec_output(codec: *mut ffi::AMediaCodec) -> Option<Self> {
        let ptr = unsafe { ffi::AMediaCodec_getOutputFormat(codec) };
        if ptr.is_null() {
            None
        } else {
            Some(Self { ptr })
        }
    }

    fn get_i32(&self, key: &str) -> Option<i32> {
        let key = CString::new(key).ok()?;
        let mut value = 0_i32;
        unsafe { ffi::AMediaFormat_getInt32(self.ptr, key.as_ptr(), &mut value as *mut i32) }
            .then_some(value)
    }

    fn set_i32(&self, key: &str, value: i32) -> Result<()> {
        let key = CString::new(key).with_context(|| format!("format key contains NUL: {key:?}"))?;
        unsafe {
            ffi::AMediaFormat_setInt32(self.ptr, key.as_ptr(), value);
        }
        Ok(())
    }

    fn set_str(&self, key: &str, value: &str) -> Result<()> {
        let key = CString::new(key).with_context(|| format!("format key contains NUL: {key:?}"))?;
        let value =
            CString::new(value).with_context(|| format!("format value contains NUL: {value:?}"))?;
        unsafe {
            ffi::AMediaFormat_setString(self.ptr, key.as_ptr(), value.as_ptr());
        }
        Ok(())
    }

    fn set_buffer(&self, key: &str, value: &[u8]) -> Result<()> {
        let key = CString::new(key).with_context(|| format!("format key contains NUL: {key:?}"))?;
        unsafe {
            ffi::AMediaFormat_setBuffer(
                self.ptr,
                key.as_ptr(),
                value.as_ptr().cast(),
                value.len() as ffi::size_t,
            );
        }
        Ok(())
    }

    fn to_string_lossy(&self) -> String {
        let ptr = unsafe { ffi::AMediaFormat_toString(self.ptr) };
        if ptr.is_null() {
            "<null AMediaFormat string>".to_string()
        } else {
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned()
        }
    }
}

impl Drop for MediaFormatHandle {
    fn drop(&mut self) {
        unsafe {
            let status = ffi::AMediaFormat_delete(self.ptr);
            if status != ffi::media_status_t::AMEDIA_OK {
                warn!("AMediaFormat_delete failed: status={}", status.0);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DecoderOutputFormat {
    width: usize,
    height: usize,
    stride: usize,
    slice_height: usize,
    color_format: i32,
}

impl DecoderOutputFormat {
    fn from_media_format(format: &MediaFormatHandle) -> Self {
        let mut parsed = Self {
            width: format
                .get_i32("width")
                .unwrap_or(DECODER_CONFIG_WIDTH)
                .max(1) as usize,
            height: format
                .get_i32("height")
                .unwrap_or(DECODER_CONFIG_HEIGHT)
                .max(1) as usize,
            stride: 0,
            slice_height: 0,
            color_format: format
                .get_i32("color-format")
                .unwrap_or(COLOR_FORMAT_YUV420_FLEXIBLE),
        };
        parsed.stride = format
            .get_i32("stride")
            .filter(|value| *value > 0)
            .map(|value| value as usize)
            .unwrap_or(parsed.width);
        parsed.slice_height = format
            .get_i32("slice-height")
            .filter(|value| *value > 0)
            .map(|value| value as usize)
            .unwrap_or(parsed.height);
        parsed
    }
}

impl Default for DecoderOutputFormat {
    fn default() -> Self {
        Self {
            width: DECODER_CONFIG_WIDTH as usize,
            height: DECODER_CONFIG_HEIGHT as usize,
            stride: DECODER_CONFIG_WIDTH as usize,
            slice_height: DECODER_CONFIG_HEIGHT as usize,
            color_format: COLOR_FORMAT_YUV420_FLEXIBLE,
        }
    }
}

struct RgbaFrame {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

fn decode_yuv420_to_rgba(data: &[u8], format: DecoderOutputFormat) -> Result<RgbaFrame> {
    if format.width == 0 || format.height == 0 {
        bail!(
            "invalid MediaCodec output dimensions {}x{}",
            format.width,
            format.height
        );
    }

    let scale = cmp::max(
        1,
        cmp::max(format.width, format.height).div_ceil(DECODER_MAX_RGBA_DIMENSION),
    );
    let out_width = cmp::max(1, format.width / scale);
    let out_height = cmp::max(1, format.height / scale);
    let mut rgba = vec![0_u8; out_width * out_height * 4];

    match format.color_format {
        COLOR_FORMAT_YUV420_PLANAR => {
            convert_planar_yuv420(data, format, scale, out_width, out_height, &mut rgba)?
        }
        COLOR_FORMAT_YUV420_SEMIPLANAR
        | COLOR_FORMAT_YUV420_FLEXIBLE
        | COLOR_FORMAT_QCOM_YUV420_SEMIPLANAR32M => {
            convert_semiplanar_yuv420(data, format, scale, out_width, out_height, &mut rgba)?
        }
        other => {
            bail!("unsupported MediaCodec CPU color format {other}");
        }
    }

    Ok(RgbaFrame {
        width: out_width,
        height: out_height,
        rgba,
    })
}

fn convert_planar_yuv420(
    data: &[u8],
    format: DecoderOutputFormat,
    scale: usize,
    out_width: usize,
    out_height: usize,
    rgba: &mut [u8],
) -> Result<()> {
    let y_stride = format.stride.max(format.width);
    let y_rows = format.slice_height.max(format.height);
    let chroma_stride = cmp::max(1, y_stride / 2);
    let chroma_rows = cmp::max(1, y_rows / 2);
    let y_plane_size = y_stride
        .checked_mul(y_rows)
        .context("Y plane size overflow")?;
    let chroma_plane_size = chroma_stride
        .checked_mul(chroma_rows)
        .context("chroma plane size overflow")?;
    let u_offset = y_plane_size;
    let v_offset = u_offset
        .checked_add(chroma_plane_size)
        .context("V plane offset overflow")?;

    if data.len() < v_offset + chroma_plane_size {
        bail!(
            "planar YUV output too small: got {} bytes, need at least {}",
            data.len(),
            v_offset + chroma_plane_size
        );
    }

    for out_y in 0..out_height {
        let src_y = (out_y * scale).min(format.height - 1);
        for out_x in 0..out_width {
            let src_x = (out_x * scale).min(format.width - 1);
            let y = data[src_y * y_stride + src_x];
            let chroma_index = (src_y / 2) * chroma_stride + (src_x / 2);
            let u = data[u_offset + chroma_index];
            let v = data[v_offset + chroma_index];
            write_rgba_pixel(rgba, out_y * out_width + out_x, y, u, v);
        }
    }

    Ok(())
}

fn convert_semiplanar_yuv420(
    data: &[u8],
    format: DecoderOutputFormat,
    scale: usize,
    out_width: usize,
    out_height: usize,
    rgba: &mut [u8],
) -> Result<()> {
    let y_stride = format.stride.max(format.width);
    let y_rows = format.slice_height.max(format.height);
    let uv_stride = y_stride;
    let uv_rows = cmp::max(1, y_rows / 2);
    let y_plane_size = y_stride
        .checked_mul(y_rows)
        .context("Y plane size overflow")?;
    let uv_plane_size = uv_stride
        .checked_mul(uv_rows)
        .context("UV plane size overflow")?;
    let uv_offset = y_plane_size;

    if data.len() < uv_offset + uv_plane_size {
        bail!(
            "semiplanar YUV output too small: got {} bytes, need at least {}",
            data.len(),
            uv_offset + uv_plane_size
        );
    }

    for out_y in 0..out_height {
        let src_y = (out_y * scale).min(format.height - 1);
        for out_x in 0..out_width {
            let src_x = (out_x * scale).min(format.width - 1);
            let y = data[src_y * y_stride + src_x];
            let uv_index = (src_y / 2) * uv_stride + (src_x / 2) * 2;
            let u = data[uv_offset + uv_index];
            let v = data[uv_offset + uv_index + 1];
            write_rgba_pixel(rgba, out_y * out_width + out_x, y, u, v);
        }
    }

    Ok(())
}

fn write_rgba_pixel(rgba: &mut [u8], pixel_index: usize, y: u8, u: u8, v: u8) {
    let c = i32::from(y).saturating_sub(16).max(0);
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let r = clamp_u8((298 * c + 409 * e + 128) >> 8);
    let g = clamp_u8((298 * c - 100 * d - 208 * e + 128) >> 8);
    let b = clamp_u8((298 * c + 516 * d + 128) >> 8);
    let offset = pixel_index * 4;
    rgba[offset] = r;
    rgba[offset + 1] = g;
    rgba[offset + 2] = b;
    rgba[offset + 3] = 255;
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

fn software_decoder_name(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "video/avc" => Some("OMX.google.h264.decoder"),
        "video/hevc" => Some("OMX.google.hevc.decoder"),
        _ => None,
    }
}

fn check_media_status(status: ffi::media_status_t, action: &str) -> Result<()> {
    if status == ffi::media_status_t::AMEDIA_OK {
        Ok(())
    } else {
        bail!("{action} failed with media_status={}", status.0)
    }
}
