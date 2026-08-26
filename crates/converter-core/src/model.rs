use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentMode {
    Tv,
    Animation,
    CameraVideos,
    Stabilize,
    Trim,
    PhotoSlideshow,
}

impl ContentMode {
    pub const ALL: [Self; 6] = [
        Self::Tv,
        Self::Animation,
        Self::CameraVideos,
        Self::Stabilize,
        Self::Trim,
        Self::PhotoSlideshow,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tv => "TV",
            Self::Animation => "Animation",
            Self::CameraVideos => "Camera videos",
            Self::Stabilize => "Stabilize",
            Self::Trim => "Trim",
            Self::PhotoSlideshow => "Photo slideshow",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoder {
    X265,
    X264,
    SvtAv1,
    HevcNvenc,
    Av1Nvenc,
    H264Nvenc,
    H264VideoToolbox,
    HevcVideoToolbox,
    Av1VideoToolbox,
}

impl Encoder {
    pub const ALL: [Self; 9] = [
        Self::X265,
        Self::X264,
        Self::SvtAv1,
        Self::HevcNvenc,
        Self::Av1Nvenc,
        Self::H264Nvenc,
        Self::H264VideoToolbox,
        Self::HevcVideoToolbox,
        Self::Av1VideoToolbox,
    ];

    #[must_use]
    pub const fn library_name(self) -> &'static str {
        match self {
            Self::X265 => "libx265",
            Self::X264 => "libx264",
            Self::SvtAv1 => "libsvtav1",
            Self::HevcNvenc => "hevc_nvenc",
            Self::Av1Nvenc => "av1_nvenc",
            Self::H264Nvenc => "h264_nvenc",
            Self::H264VideoToolbox => "h264_videotoolbox",
            Self::HevcVideoToolbox => "hevc_videotoolbox",
            Self::Av1VideoToolbox => "av1_videotoolbox",
        }
    }

    /// Name used by the Python application in its UI and persisted JSON.
    #[must_use]
    pub const fn user_name(self) -> &'static str {
        match self {
            Self::X265 => "x265",
            Self::X264 => "x264",
            _ => self.library_name(),
        }
    }

    #[must_use]
    pub fn from_library_name(value: &str) -> Option<Self> {
        match value {
            "x265" => return Some(Self::X265),
            "x264" => return Some(Self::X264),
            _ => {}
        }
        Self::ALL
            .into_iter()
            .find(|encoder| encoder.library_name() == value)
    }

    #[must_use]
    pub const fn is_hevc(self) -> bool {
        matches!(self, Self::X265 | Self::HevcNvenc | Self::HevcVideoToolbox)
    }

    #[must_use]
    pub const fn is_nvenc(self) -> bool {
        matches!(self, Self::HevcNvenc | Self::Av1Nvenc | Self::H264Nvenc)
    }

    #[must_use]
    pub const fn is_videotoolbox(self) -> bool {
        matches!(
            self,
            Self::H264VideoToolbox | Self::HevcVideoToolbox | Self::Av1VideoToolbox
        )
    }

    #[must_use]
    pub const fn is_hardware(self) -> bool {
        self.is_nvenc() || self.is_videotoolbox()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Matroska,
    Mp4,
}

impl Container {
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Matroska => "mkv",
            Self::Mp4 => "mp4",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FpsPolicy {
    SharedLowest,
    Source,
    Exact(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioPolicy {
    CopyValid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataPolicy {
    Preserve,
    Remove,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueueSettings {
    pub mode: ContentMode,
    pub encoder: Encoder,
    pub fps: FpsPolicy,
    pub quality: Option<f32>,
    pub speed_preset: Option<String>,
    pub stabilize_strength: String,
    pub trim_start: Option<Duration>,
    pub trim_end: Option<Duration>,
    pub apply_lut: bool,
    pub camera_lut_path: Option<PathBuf>,
    pub photo_interval: Duration,
    pub slideshow_resolution: (u32, u32),
    pub slideshow_fps: u32,
    pub slideshow_collage: bool,
    pub slideshow_audio_paths: Vec<PathBuf>,
    pub slideshow_image_paths: Vec<PathBuf>,
    pub slideshow_review_image_paths: Vec<PathBuf>,
    pub audio: AudioPolicy,
    pub metadata: MetadataPolicy,
}

impl Default for QueueSettings {
    fn default() -> Self {
        Self {
            mode: ContentMode::Tv,
            encoder: Encoder::X265,
            fps: FpsPolicy::SharedLowest,
            quality: Some(28.0),
            speed_preset: Some("medium".to_owned()),
            stabilize_strength: "Balanced".to_owned(),
            trim_start: None,
            trim_end: None,
            apply_lut: true,
            camera_lut_path: None,
            photo_interval: Duration::from_secs(4),
            slideshow_resolution: (1920, 1080),
            slideshow_fps: 30,
            slideshow_collage: false,
            slideshow_audio_paths: Vec::new(),
            slideshow_image_paths: Vec::new(),
            slideshow_review_image_paths: Vec::new(),
            audio: AudioPolicy::CopyValid,
            metadata: MetadataPolicy::Remove,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    Attachment,
    Data,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColorCharacteristics {
    pub range: Option<String>,
    pub primaries: Option<String>,
    pub transfer: Option<String>,
    pub space: Option<String>,
    pub chroma_location: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaStream {
    pub index: usize,
    pub kind: StreamKind,
    pub codec_name: Option<String>,
    pub codec_profile: Option<String>,
    pub codec_level: Option<i32>,
    pub bit_depth: Option<u32>,
    pub bit_rate: Option<u64>,
    pub duration: Option<Duration>,
    pub frame_count: Option<u64>,
    pub frame_rate: Option<f64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub language: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
    pub is_attached_picture: bool,
    pub color: ColorCharacteristics,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub path: PathBuf,
    pub container_name: String,
    pub duration: Option<Duration>,
    pub file_size: Option<u64>,
    pub bit_rate: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frame_rate: Option<f64>,
    pub streams: Vec<MediaStream>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EncoderCapabilities {
    pub encoders: Vec<String>,
    pub filters: Vec<String>,
    pub muxers: Vec<String>,
    pub hardware_devices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConversionRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    pub settings: QueueSettings,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConversionProgress {
    /// Overall work completed across every conversion phase.
    ///
    /// This is distinct from `completed`, which is the media timestamp in the
    /// currently active `FFmpeg` pass. Multi-pass workflows use this ratio to
    /// keep their progress bar monotonic without falsifying the displayed
    /// media time.
    pub overall: Option<ProgressRatio>,
    pub completed: Duration,
    pub total: Option<Duration>,
    pub frames: Option<u64>,
    pub total_frames: Option<u64>,
    pub target_fps: Option<f64>,
    pub frames_per_second: Option<f64>,
    pub speed: Option<f64>,
    pub output_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressRatio {
    pub completed: u128,
    pub total: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversionPreview {
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<[u8]>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConversionEvent {
    Started { input: PathBuf, output: PathBuf },
    Progress(ConversionProgress),
    Preview(ConversionPreview),
    Warning(String),
    Completed { output: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::Encoder;

    #[test]
    fn encoder_library_names_round_trip() {
        for encoder in Encoder::ALL {
            assert_eq!(
                Encoder::from_library_name(encoder.library_name()),
                Some(encoder)
            );
        }
        assert_eq!(Encoder::from_library_name("unknown_encoder"), None);
        assert_eq!(Encoder::from_library_name("x265"), Some(Encoder::X265));
        assert_eq!(Encoder::from_library_name("x264"), Some(Encoder::X264));
        assert_eq!(Encoder::X265.user_name(), "x265");
        assert_eq!(Encoder::X264.user_name(), "x264");
    }

    #[test]
    fn hardware_encoder_families_are_classified_explicitly() {
        assert!(Encoder::HevcNvenc.is_hevc());
        assert!(Encoder::HevcVideoToolbox.is_hevc());
        assert!(Encoder::Av1Nvenc.is_nvenc());
        assert!(Encoder::Av1VideoToolbox.is_videotoolbox());
        assert!(!Encoder::X265.is_hardware());
        assert!(Encoder::H264VideoToolbox.is_hardware());
    }
}
