use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodecType {
    #[default]
    H264,
    Hevc,
    AV1,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediacodecPropType {
    Float,
    Int32,
    Int64,
    String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediacodecProperty {
    pub ty: MediacodecPropType,
    pub value: String,
}

#[derive(Clone, Default, PartialEq)]
pub struct VideoDecoderConfig {
    pub codec: CodecType,
    pub force_software_decoder: bool,
    pub max_buffering_frames: f32,
    pub buffering_history_weight: f32,
    pub options: Vec<(String, MediacodecProperty)>,
    pub config_buffer: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

#[derive(Default)]
pub struct VideoDecoderSink {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSink,
}

impl VideoDecoderSink {
    #[allow(unused_variables)]
    pub fn push_nal(&mut self, timestamp: Duration, nal: &[u8]) -> bool {
        #[cfg(target_os = "android")]
        {
            self.inner.push_frame_nal(timestamp, nal).unwrap_or(false)
        }
        #[cfg(not(target_os = "android"))]
        false
    }
}

pub struct VideoDecoderSource {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSource,
}

impl VideoDecoderSource {
    pub fn get_frame(&mut self) -> Option<(Duration, *mut std::ffi::c_void)> {
        #[cfg(target_os = "android")]
        {
            self.inner.dequeue_frame()
        }
        #[cfg(not(target_os = "android"))]
        None
    }
}

#[allow(unused_variables)]
pub fn create_decoder(
    config: VideoDecoderConfig,
    report_frame_decoded: impl Fn(anyhow::Result<Duration>) + Send + Sync + 'static,
) -> (VideoDecoderSink, VideoDecoderSource) {
    #[cfg(target_os = "android")]
    {
        let (sink, source) = android::video_decoder_split(
            config.clone(),
            config.config_buffer,
            report_frame_decoded,
        )
        .unwrap();

        (
            VideoDecoderSink { inner: sink },
            VideoDecoderSource { inner: source },
        )
    }
    #[cfg(not(target_os = "android"))]
    (VideoDecoderSink {}, VideoDecoderSource {})
}

#[cfg(target_os = "android")]
mod android;
