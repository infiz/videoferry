use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::Path;

use crate::{
    ConversionControl, ConversionEvent, ConversionRequest, EncoderCapabilities, MediaInfo,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    Unavailable(String),
    Unsupported(String),
    InvalidMedia(String),
    Cancelled,
    Failed(String),
}

impl Display for EngineError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "media engine unavailable: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported media: {message}"),
            Self::InvalidMedia(message) => write!(formatter, "invalid media: {message}"),
            Self::Cancelled => formatter.write_str("conversion cancelled"),
            Self::Failed(message) => write!(formatter, "conversion failed: {message}"),
        }
    }
}

impl Error for EngineError {}

pub trait MediaEngine: Send + Sync {
    /// Returns the exact native media-library versions in use.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Unavailable`] when the native libraries cannot be
    /// loaded or fail the supported-major-version guard.
    fn version_summary(&self) -> Result<String, EngineError>;

    /// Discovers encoders, filters, and hardware devices at runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when `FFmpeg` initialization or capability enumeration
    /// fails.
    fn capabilities(&self) -> Result<EncoderCapabilities, EngineError>;

    /// Reads container and stream metadata without using `ffprobe`.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be opened or contains invalid or
    /// unsupported media.
    fn probe(&self, path: &Path) -> Result<MediaInfo, EngineError>;

    /// Runs one direct-library conversion and emits typed progress events.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, unsupported media, invalid settings,
    /// native `FFmpeg` failures, or output validation failures.
    fn convert(
        &self,
        request: &ConversionRequest,
        control: &ConversionControl,
        emit: &mut dyn FnMut(ConversionEvent),
    ) -> Result<(), EngineError>;
}
