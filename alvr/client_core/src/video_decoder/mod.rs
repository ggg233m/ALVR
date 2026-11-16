#[cfg(target_os = "android")]
mod android;

use alvr_common::anyhow::Result;
use alvr_session::{CodecType, MediacodecProperty};
use std::time::Duration;

#[derive(Clone, Default, PartialEq)]
pub struct VideoDecoderConfig {
    pub codec: CodecType,
    pub force_software_decoder: bool,
    pub max_buffering_frames: f32,
    pub buffering_history_weight: f32,
    pub options: Vec<(String, MediacodecProperty)>,
    pub config_buffer: Vec<u8>,
    /// Size of one decoded eye view, in pixels.
    ///
    /// MediaCodec and the ImageReader need this up front to size their buffers.
    /// They used to be created with placeholders (512x1024 for the codec, 1x1
    /// for the reader), which makes the platform rebuild internal buffers once
    /// the first full-size frame arrives. Measured on device: decode latency
    /// dropped from 20-25 ms to 14 ms and total latency from 89 ms to 70 ms
    /// after supplying the real size.
    pub frame_size: [u32; 2],
}

pub struct VideoDecoderSink {
    #[cfg(target_os = "android")]
    inner: android::VideoDecoderSink,
}

impl VideoDecoderSink {
    // returns true if frame has been successfully enqueued
    #[allow(unused_variables)]
    pub fn push_nal(&mut self, timestamp: Duration, nal: &[u8]) -> bool {
        #[cfg(target_os = "android")]
        {
            alvr_common::show_err(self.inner.push_frame_nal(timestamp, nal)).unwrap_or(false)
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
    /// If a frame is available, return the timestamp and the AHardwareBuffer.
    pub fn get_frame(&mut self) -> Option<(Duration, *mut std::ffi::c_void)> {
        #[cfg(target_os = "android")]
        {
            self.inner.dequeue_frame()
        }
        #[cfg(not(target_os = "android"))]
        None
    }
}

// report_frame_decoded: (target_timestamp: Duration) -> ()
#[allow(unused_variables)]
pub fn create_decoder(
    config: VideoDecoderConfig,
    report_frame_decoded: impl Fn(Result<Duration>) + Send + Sync + 'static,
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
