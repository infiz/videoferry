#![deny(unsafe_op_in_unsafe_fn)]

use std::path::Path;

use videoferry_core::{
    ConversionControl, ConversionEvent, ConversionRequest, EncoderCapabilities, EngineError,
    MediaEngine, MediaInfo,
};

mod cpu_limit;
pub use cpu_limit::ProcessCpuLimiter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoThumbnail {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[cfg(feature = "native-ffmpeg")]
mod audio;
#[cfg(feature = "native-ffmpeg")]
mod chapters;
#[cfg(feature = "native-ffmpeg")]
mod mux;
#[cfg(feature = "native-ffmpeg")]
mod native;
#[cfg(feature = "native-ffmpeg")]
mod progress;
#[cfg(feature = "native-ffmpeg")]
mod remux;
#[cfg(feature = "native-ffmpeg")]
mod slideshow;
#[cfg(feature = "native-ffmpeg")]
mod slideshow_audio;
#[cfg(feature = "native-ffmpeg")]
mod slideshow_collage;
#[cfg(feature = "native-ffmpeg")]
mod stabilize;
#[cfg(feature = "native-ffmpeg")]
mod subtitle;
#[cfg(feature = "native-ffmpeg")]
mod transcode;
#[cfg(feature = "native-ffmpeg")]
pub use native::NativeEngine;

/// Placeholder used until the direct libav* implementation is enabled.
///
/// This deliberately fails instead of falling back to an `FFmpeg` subprocess.
#[derive(Debug, Default)]
pub struct UnavailableEngine;

impl MediaEngine for UnavailableEngine {
    fn version_summary(&self) -> Result<String, EngineError> {
        Err(unavailable())
    }

    fn capabilities(&self) -> Result<EncoderCapabilities, EngineError> {
        Err(unavailable())
    }

    fn probe(&self, _path: &Path) -> Result<MediaInfo, EngineError> {
        Err(unavailable())
    }

    fn convert(
        &self,
        _request: &ConversionRequest,
        _control: &ConversionControl,
        _emit: &mut dyn FnMut(ConversionEvent),
    ) -> Result<(), EngineError> {
        Err(unavailable())
    }
}

fn unavailable() -> EngineError {
    EngineError::Unavailable(
        "direct FFmpeg support has not been compiled into this milestone".to_owned(),
    )
}
