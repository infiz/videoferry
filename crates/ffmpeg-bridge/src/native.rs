use std::collections::BTreeMap;
use std::ffi::CStr;
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;
use std::time::Duration;

use ffmpeg_next as ffmpeg;
use videoferry_core::{
    AudioStreamAction, ColorCharacteristics, Container, ContentMode, ConversionControl,
    ConversionEvent, ConversionRequest, Encoder, EncoderCapabilities, EngineError, FpsPolicy,
    MediaEngine, MediaInfo, MediaStream, StreamKind, StreamPlan, SubtitleStreamAction,
    build_stream_plan,
};

use crate::progress::ProgressMetadata;
use crate::{remux, slideshow, transcode};

static INITIALIZED: OnceLock<Result<(), String>> = OnceLock::new();

struct PendingOutput {
    partial: remux::PartialOutput,
    expected_duration: Option<Duration>,
    expected_frame_rate: Option<f64>,
    expected_codec: Option<&'static str>,
}

const REQUIRED_FILTERS: &[&str] = &[
    "abuffer",
    "abuffersink",
    "afade",
    "aloop",
    "atrim",
    "buffer",
    "buffersink",
    "concat",
    "format",
    "fps",
    "lut3d",
    "pad",
    "scale",
    "setpts",
    "tpad",
    "xfade",
];
const PINNED_FFMPEG_RELEASE: &str = "9.0.1";
const PINNED_LIBRARY_VERSIONS: &[(&str, u32, (u32, u32, u32))] = &[
    ("libavformat", 63, (63, 1, 101)),
    ("libavcodec", 63, (63, 1, 101)),
    ("libavfilter", 12, (12, 1, 101)),
    ("libavutil", 61, (61, 1, 101)),
];
const REQUIRED_ENCODERS: &[&str] = &[
    "aac",
    "ac3",
    "libsvtav1",
    "libx264",
    "libx265",
    "mov_text",
    "srt",
];

#[derive(Debug, Default)]
pub struct NativeEngine;

impl NativeEngine {
    /// Initializes the process-wide `FFmpeg` libraries.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Unavailable`] when `FFmpeg` initialization fails.
    pub fn new() -> Result<Self, EngineError> {
        ensure_initialized()?;
        Ok(Self)
    }

    /// Verifies the exact packaged runtime and every non-hardware component
    /// needed by the desktop workflows.
    ///
    /// Hardware encoders are intentionally reported but not required because
    /// their availability depends on the current machine.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError::Unavailable`] when the loaded libraries are not
    /// the pinned release or a required codec, filter, or muxer is missing.
    pub fn verify_packaged_runtime(&self) -> Result<String, EngineError> {
        ensure_initialized()?;
        validate_pinned_runtime().map_err(EngineError::Unavailable)?;
        let capabilities = self.capabilities()?;
        let stabilization = if filters_available(&["vidstabdetect", "vidstabtransform"]) {
            "vidstabdetect+vidstabtransform"
        } else {
            "deshake"
        };
        Ok([
            "runtime=ok".to_owned(),
            format!("engine={}", self.version_summary()?),
            format!("required_encoders={}", REQUIRED_ENCODERS.join(",")),
            format!("stabilization={stabilization}"),
            "muxers=matroska,mp4".to_owned(),
            format!(
                "hardware_devices={}",
                capabilities.hardware_devices.join(",")
            ),
            format!("available_encoders={}", capabilities.encoders.join(",")),
        ]
        .join("\n"))
    }

    /// Decodes an oriented photo preview directly through `FFmpeg`.
    ///
    /// # Errors
    ///
    /// Returns an error when the photo cannot be decoded or scaled.
    pub fn photo_thumbnail(
        &self,
        path: &Path,
        maximum_width: u32,
        maximum_height: u32,
    ) -> Result<crate::PhotoThumbnail, EngineError> {
        ensure_initialized()?;
        slideshow::photo_thumbnail(path, maximum_width, maximum_height)
    }

    /// Returns the exact ordered photo groups used as slideshow slides.
    ///
    /// # Errors
    ///
    /// Returns an error when an image cannot be probed.
    pub fn slideshow_review_groups(
        &self,
        image_paths: &[std::path::PathBuf],
        collage: bool,
    ) -> Result<Vec<Vec<std::path::PathBuf>>, EngineError> {
        ensure_initialized()?;
        slideshow::review_groups(image_paths, collage)
    }

    /// Renders one slideshow review group directly through `FFmpeg`.
    ///
    /// # Errors
    ///
    /// Returns an error when a photo cannot be decoded or composited.
    pub fn slideshow_review_thumbnail(
        &self,
        image_paths: &[std::path::PathBuf],
        collage: bool,
        width: u32,
        height: u32,
    ) -> Result<crate::PhotoThumbnail, EngineError> {
        ensure_initialized()?;
        slideshow::review_thumbnail(image_paths, collage, width, height)
    }

    /// Detects the software encoder signature embedded in the primary video
    /// stream, matching the names reported by Python's `MediaInfo` dependency.
    ///
    /// # Errors
    ///
    /// Returns an error when the input cannot be opened or has no video stream.
    pub fn encoded_library_name(&self, path: &Path) -> Result<Option<&'static str>, EngineError> {
        ensure_initialized()?;
        let mut input = ffmpeg::format::input(path)
            .map_err(|error| EngineError::InvalidMedia(format!("{}: {error}", path.display())))?;
        let video_index = input
            .streams()
            .find(|stream| stream.parameters().medium() == ffmpeg::media::Type::Video)
            .map(|stream| stream.index())
            .ok_or_else(|| EngineError::InvalidMedia("no video stream found".to_owned()))?;

        for (_, value) in input.metadata().iter() {
            if let Some(name) = encoded_library_marker(value.as_bytes()) {
                return Ok(Some(name));
            }
        }
        if let Some(stream) = input.stream(video_index) {
            for (_, value) in stream.metadata().iter() {
                if let Some(name) = encoded_library_marker(value.as_bytes()) {
                    return Ok(Some(name));
                }
            }
        }

        let mut scanned_bytes = 0_usize;
        for (stream, packet) in input.packets().take(256) {
            if stream.index() != video_index {
                continue;
            }
            let Some(data) = packet.data() else {
                continue;
            };
            scanned_bytes = scanned_bytes.saturating_add(data.len());
            if let Some(name) = encoded_library_marker(data) {
                return Ok(Some(name));
            }
            if scanned_bytes >= 8 * 1024 * 1024 {
                break;
            }
        }
        Ok(None)
    }
}

fn encoded_library_marker(bytes: &[u8]) -> Option<&'static str> {
    let lower = bytes.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    let contains = |needle: &[u8]| lower.windows(needle.len()).any(|window| window == needle);
    if contains(b"libsvtav1") || contains(b"svt-av1") || contains(b"svt_av1") {
        Some("libsvtav1")
    } else if contains(b"libx265") || contains(b"x265 (build") || contains(b"x265 -") {
        Some("x265")
    } else if contains(b"libx264") || contains(b"x264 - core") || contains(b"x264 -") {
        Some("x264")
    } else {
        None
    }
}

impl MediaEngine for NativeEngine {
    fn version_summary(&self) -> Result<String, EngineError> {
        ensure_initialized()?;
        let release = runtime_release();
        Ok(format!(
            "FFmpeg {release}; libavformat {}; libavcodec {}; libavfilter {}; libavutil {}; {}",
            unpack_version(ffmpeg::format::version()),
            unpack_version(ffmpeg::codec::version()),
            unpack_version(ffmpeg::filter::version()),
            unpack_version(ffmpeg::util::version()),
            ffmpeg::util::license(),
        ))
    }

    fn capabilities(&self) -> Result<EncoderCapabilities, EngineError> {
        ensure_initialized()?;
        let hardware_devices = available_hardware_devices();
        let encoders = Encoder::ALL
            .into_iter()
            .map(Encoder::library_name)
            .filter(|name| ffmpeg::encoder::find_by_name(name).is_some())
            .filter(|name| encoder_is_usable(name, &hardware_devices))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let filters = REQUIRED_FILTERS
            .iter()
            .copied()
            .filter(|name| ffmpeg::filter::find(name).is_some())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let muxers = ["matroska", "mp4"]
            .into_iter()
            .filter(|name| output_format_available(name))
            .map(str::to_owned)
            .collect::<Vec<_>>();

        Ok(EncoderCapabilities {
            encoders,
            filters,
            muxers,
            hardware_devices,
        })
    }

    fn probe(&self, path: &Path) -> Result<MediaInfo, EngineError> {
        ensure_initialized()?;
        let input = ffmpeg::format::input(path)
            .map_err(|error| EngineError::InvalidMedia(format!("{}: {error}", path.display())))?;
        let streams = input
            .streams()
            .map(|stream| stream_info(&stream))
            .collect::<Vec<_>>();
        let primary_video = streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Video && !stream.is_attached_picture);

        Ok(MediaInfo {
            path: path.to_path_buf(),
            container_name: input.format().name().to_owned(),
            duration: microseconds_to_duration(input.duration()),
            file_size: std::fs::metadata(path).ok().map(|metadata| metadata.len()),
            bit_rate: positive_u64(input.bit_rate()),
            width: primary_video.and_then(|stream| stream.width),
            height: primary_video.and_then(|stream| stream.height),
            frame_rate: primary_video.and_then(|stream| stream.frame_rate),
            streams,
            metadata: dictionary_to_map(&input.metadata()),
        })
    }

    fn convert(
        &self,
        request: &ConversionRequest,
        control: &ConversionControl,
        emit: &mut dyn FnMut(ConversionEvent),
    ) -> Result<(), EngineError> {
        ensure_initialized()?;
        if request.output.exists() {
            return Err(EngineError::Failed(format!(
                "output already exists: {}",
                request.output.display()
            )));
        }
        if request.settings.mode == ContentMode::PhotoSlideshow {
            return convert_slideshow(self, request, control, emit);
        }
        let container = container_from_path(&request.output)?;
        let source = self.probe(&request.input)?;
        let plan = build_stream_plan(&source, container);
        if plan.video_input_index.is_none() {
            return Err(EngineError::InvalidMedia(
                "no video stream found".to_owned(),
            ));
        }
        for skipped in &plan.skipped {
            emit(ConversionEvent::Warning(format!(
                "skipping {:?} stream {}: {:?}",
                skipped.kind, skipped.input_index, skipped.reason
            )));
        }

        emit(ConversionEvent::Started {
            input: request.input.clone(),
            output: request.output.clone(),
        });
        let pending = write_output(self, request, &source, &plan, control, emit)?;
        let output_info = self.probe(pending.partial.path())?;
        validate_output(
            &source,
            &output_info,
            &plan,
            pending.expected_duration,
            pending.expected_frame_rate,
            pending.expected_codec,
        )?;
        pending.partial.commit(&request.output)?;
        emit(ConversionEvent::Completed {
            output: request.output.clone(),
        });
        Ok(())
    }
}

fn convert_slideshow(
    engine: &NativeEngine,
    request: &ConversionRequest,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<(), EngineError> {
    if container_from_path(&request.output)? != Container::Mp4 {
        return Err(EngineError::Unsupported(
            "Photo slideshow output must be MP4".to_owned(),
        ));
    }
    emit(ConversionEvent::Started {
        input: request.input.clone(),
        output: request.output.clone(),
    });
    let pending = slideshow::write(
        &request.input,
        &request.output,
        &request.settings,
        control,
        emit,
    )?;
    let decoded_frames = slideshow::decoded_frame_count(pending.partial.path())?;
    if decoded_frames != pending.frame_count {
        return Err(EngineError::Failed(format!(
            "slideshow decoded {decoded_frames} frames; expected {}",
            pending.frame_count
        )));
    }
    let output = engine.probe(pending.partial.path())?;
    validate_slideshow_output(
        &output,
        pending.duration,
        pending.frame_rate,
        pending.codec_name,
        request.settings.slideshow_resolution,
        pending.has_audio,
    )?;
    pending.partial.commit(&request.output)?;
    emit(ConversionEvent::Completed {
        output: request.output.clone(),
    });
    Ok(())
}

fn validate_slideshow_output(
    output: &MediaInfo,
    expected_duration: Duration,
    expected_frame_rate: f64,
    expected_codec: &str,
    expected_resolution: (u32, u32),
    expected_audio: bool,
) -> Result<(), EngineError> {
    if output.file_size.unwrap_or(0) == 0 {
        return Err(EngineError::Failed(
            "validated slideshow is empty".to_owned(),
        ));
    }
    let video = output
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Video && !stream.is_attached_picture)
        .ok_or_else(|| EngineError::Failed("slideshow has no video stream".to_owned()))?;
    if video.codec_name.as_deref() != Some(expected_codec) {
        return Err(EngineError::Failed(format!(
            "slideshow codec does not match expected {expected_codec}"
        )));
    }
    if video
        .frame_rate
        .is_none_or(|value| (value - expected_frame_rate).abs() > 0.01)
    {
        return Err(EngineError::Failed(format!(
            "slideshow frame rate does not match expected {expected_frame_rate}"
        )));
    }
    if (video.width, video.height) != (Some(expected_resolution.0), Some(expected_resolution.1)) {
        return Err(EngineError::Failed(format!(
            "slideshow resolution does not match {}x{}",
            expected_resolution.0, expected_resolution.1
        )));
    }
    let audio = output
        .streams
        .iter()
        .filter(|stream| stream.kind == StreamKind::Audio)
        .collect::<Vec<_>>();
    if audio.len() != usize::from(expected_audio)
        || expected_audio && audio[0].codec_name.as_deref() != Some("aac")
    {
        return Err(EngineError::Failed(
            "slideshow AAC stream layout does not match the selected audio".to_owned(),
        ));
    }
    if let Some(duration) = output.duration
        && duration.abs_diff(expected_duration) > Duration::from_secs(1)
    {
        return Err(EngineError::Failed(format!(
            "slideshow duration differs from expected by {:.3}s",
            duration.abs_diff(expected_duration).as_secs_f64()
        )));
    }
    Ok(())
}

fn write_output(
    engine: &NativeEngine,
    request: &ConversionRequest,
    source: &MediaInfo,
    plan: &StreamPlan,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<PendingOutput, EngineError> {
    if request.settings.mode == ContentMode::Trim {
        let (start, duration) = trim_window(source, &request.settings)?;
        let target_fps = source.frame_rate;
        return Ok(PendingOutput {
            partial: remux::write_trimmed_copy(
                &request.input,
                &request.output,
                plan,
                remux::TrimSpec {
                    start,
                    metadata_policy: request.settings.metadata,
                    progress: ProgressMetadata {
                        total: Some(duration),
                        total_frames: estimated_total_frames(Some(duration), target_fps),
                        target_fps,
                    },
                },
                control,
                emit,
            )?,
            expected_duration: Some(duration),
            expected_frame_rate: None,
            expected_codec: None,
        });
    }
    let target_frame_rate = resolve_target_frame_rate(engine, request)?;
    let target_fps = target_frame_rate.map(f64::from).or(source.frame_rate);
    let total_frames = estimated_total_frames(source.duration, target_fps).or_else(|| {
        plan.video_input_index.and_then(|index| {
            source
                .streams
                .iter()
                .find(|stream| stream.index == index)
                .and_then(|stream| stream.frame_count)
        })
    });
    let expected_codec = match request.settings.encoder {
        Encoder::X265 | Encoder::HevcNvenc | Encoder::HevcVideoToolbox => "hevc",
        Encoder::X264 | Encoder::H264Nvenc | Encoder::H264VideoToolbox => "h264",
        Encoder::SvtAv1 | Encoder::Av1Nvenc | Encoder::Av1VideoToolbox => "av1",
    };
    Ok(PendingOutput {
        partial: transcode::write_video_transcode(
            &request.input,
            &request.output,
            plan,
            transcode::TranscodeSpec {
                settings: &request.settings,
                target_frame_rate,
                progress: ProgressMetadata {
                    total: source.duration,
                    total_frames,
                    target_fps,
                },
            },
            control,
            emit,
        )?,
        expected_duration: source.duration,
        expected_frame_rate: target_frame_rate.map(f64::from),
        expected_codec: Some(expected_codec),
    })
}

fn estimated_total_frames(duration: Option<Duration>, fps: Option<f64>) -> Option<u64> {
    let value = duration?.as_secs_f64() * fps?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    format!("{value:.0}")
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
}

fn resolve_target_frame_rate(
    engine: &NativeEngine,
    request: &ConversionRequest,
) -> Result<Option<ffmpeg::Rational>, EngineError> {
    match request.settings.fps {
        FpsPolicy::Source => Ok(None),
        FpsPolicy::Exact(value) => fps_rational(value).map(Some),
        FpsPolicy::SharedLowest => {
            let parent = request.input.parent().unwrap_or_else(|| Path::new("."));
            let mut lowest = None::<f64>;
            let entries = std::fs::read_dir(parent).map_err(|error| {
                EngineError::Failed(format!("cannot read {}: {error}", parent.display()))
            })?;
            for entry in entries.flatten() {
                let path = entry.path();
                if !is_video_path(&path) {
                    continue;
                }
                if let Ok(media) = engine.probe(&path)
                    && let Some(frame_rate) = media.frame_rate
                {
                    lowest = Some(lowest.map_or(frame_rate, |current| current.min(frame_rate)));
                }
            }
            fps_rational(lowest.ok_or_else(|| {
                EngineError::InvalidMedia("no sibling video frame rate is available".to_owned())
            })?)
            .map(Some)
        }
    }
}

fn is_video_path(path: &Path) -> bool {
    path.is_file()
        && !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("._") || name.contains(".videoferry-partial-"))
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                ["mkv", "mp4", "mov", "avi", "wmv", "flv", "rm", "rmvb"]
                    .iter()
                    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
            })
}

fn fps_rational(value: f64) -> Result<ffmpeg::Rational, EngineError> {
    if !value.is_finite() || value <= 0.0 || value > f64::from(i32::MAX) {
        return Err(EngineError::Unsupported(format!(
            "invalid frame rate: {value}"
        )));
    }
    for denominator in [1_i32, 1_001, 1_000] {
        let numerator = (value * f64::from(denominator)).round();
        if numerator > 0.0
            && numerator <= f64::from(i32::MAX)
            && (numerator / f64::from(denominator) - value).abs() < 0.000_1
        {
            return Ok(ffmpeg::Rational(rounded_i32(numerator)?, denominator));
        }
    }
    let denominator = 100_000_i32;
    let numerator = (value * f64::from(denominator)).round();
    if numerator <= 0.0 || numerator > f64::from(i32::MAX) {
        return Err(EngineError::Unsupported(format!(
            "frame rate cannot be represented: {value}"
        )));
    }
    Ok(ffmpeg::Rational(rounded_i32(numerator)?, denominator))
}

fn rounded_i32(value: f64) -> Result<i32, EngineError> {
    format!("{value:.0}").parse::<i32>().map_err(|_| {
        EngineError::Unsupported(format!(
            "value cannot be represented as an integer: {value}"
        ))
    })
}

fn trim_window(
    source: &MediaInfo,
    settings: &videoferry_core::QueueSettings,
) -> Result<(Duration, Duration), EngineError> {
    let start = settings.trim_start.unwrap_or(Duration::ZERO);
    let end = settings
        .trim_end
        .ok_or_else(|| EngineError::Unsupported("Trim mode requires an end time".to_owned()))?;
    if end < start {
        return Err(EngineError::Unsupported(
            "trim end must be at or after trim start".to_owned(),
        ));
    }
    let inclusive_duration = end
        .saturating_sub(start)
        .saturating_add(Duration::from_secs(1));
    let available_duration = source
        .duration
        .and_then(|duration| duration.checked_sub(start))
        .unwrap_or(inclusive_duration);
    Ok((start, inclusive_duration.min(available_duration)))
}

fn container_from_path(path: &Path) -> Result<Container, EngineError> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mkv") => Ok(Container::Matroska),
        Some("mp4" | "mov") => Ok(Container::Mp4),
        _ => Err(EngineError::Unsupported(format!(
            "unsupported output container: {}",
            path.display()
        ))),
    }
}

fn validate_output(
    source: &MediaInfo,
    output: &MediaInfo,
    plan: &videoferry_core::StreamPlan,
    expected_duration: Option<Duration>,
    expected_frame_rate: Option<f64>,
    expected_codec: Option<&str>,
) -> Result<(), EngineError> {
    if output.file_size.unwrap_or(0) == 0 {
        return Err(EngineError::Failed("validated output is empty".to_owned()));
    }
    if let (Some(actual_duration), Some(expected_duration)) = (output.duration, expected_duration) {
        let difference = actual_duration.abs_diff(expected_duration);
        if difference > Duration::from_secs(2) {
            return Err(EngineError::Failed(format!(
                "output duration differs from expected duration by {:.3}s",
                difference.as_secs_f64()
            )));
        }
    }
    let output_video = output
        .streams
        .iter()
        .filter(|stream| stream.kind == StreamKind::Video && !stream.is_attached_picture)
        .count();
    let primary_video = output
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Video && !stream.is_attached_picture);
    if let (Some(actual), Some(expected)) = (
        primary_video.and_then(|stream| stream.frame_rate),
        expected_frame_rate,
    ) && (actual - expected).abs() > 0.01
    {
        return Err(EngineError::Failed(format!(
            "output frame rate {actual:.3} does not match expected {expected:.3}"
        )));
    }
    if let Some(expected) = expected_codec
        && primary_video.and_then(|stream| stream.codec_name.as_deref()) != Some(expected)
    {
        return Err(EngineError::Failed(format!(
            "output video codec does not match expected {expected}"
        )));
    }
    let output_audio = output
        .streams
        .iter()
        .filter(|stream| stream.kind == StreamKind::Audio)
        .count();
    let output_subtitles = output
        .streams
        .iter()
        .filter(|stream| stream.kind == StreamKind::Subtitle)
        .count();
    let output_attachments = output
        .streams
        .iter()
        .filter(|stream| stream.kind == StreamKind::Attachment)
        .count();
    if output_video != 1
        || output_audio != plan.audio.len()
        || output_subtitles != plan.subtitles.len()
        || output_attachments != plan.attachments.len()
    {
        return Err(EngineError::Failed(format!(
            "output stream layout mismatch: video={output_video}, audio={output_audio}, subtitles={output_subtitles}, attachments={output_attachments}"
        )));
    }
    validate_transcoded_stream_codecs(output, plan)?;
    if source.path == output.path {
        return Err(EngineError::Failed(
            "output validation resolved to the input path".to_owned(),
        ));
    }
    Ok(())
}

fn validate_transcoded_stream_codecs(
    output: &MediaInfo,
    plan: &StreamPlan,
) -> Result<(), EngineError> {
    let audio = output
        .streams
        .iter()
        .filter(|stream| stream.kind == StreamKind::Audio)
        .collect::<Vec<_>>();
    for (stream, planned) in audio.iter().zip(&plan.audio) {
        if matches!(planned.action, AudioStreamAction::TranscodeAc3 { .. })
            && stream.codec_name.as_deref() != Some("ac3")
        {
            return Err(EngineError::Failed(
                "DTS replacement stream is not AC-3".to_owned(),
            ));
        }
    }
    let subtitles = output
        .streams
        .iter()
        .filter(|stream| stream.kind == StreamKind::Subtitle)
        .collect::<Vec<_>>();
    for (stream, planned) in subtitles.iter().zip(&plan.subtitles) {
        let expected = match planned.action {
            SubtitleStreamAction::TranscodeSrt => Some("subrip"),
            SubtitleStreamAction::TranscodeMovText => Some("mov_text"),
            SubtitleStreamAction::Copy => None,
        };
        if let Some(expected) = expected
            && stream.codec_name.as_deref() != Some(expected)
        {
            return Err(EngineError::Failed(format!(
                "subtitle stream is not encoded as {expected}"
            )));
        }
    }
    Ok(())
}

fn ensure_initialized() -> Result<(), EngineError> {
    match INITIALIZED.get_or_init(|| {
        ffmpeg::init().map_err(|error| error.to_string())?;
        validate_library_majors()
    }) {
        Ok(()) => Ok(()),
        Err(message) => Err(EngineError::Unavailable(message.clone())),
    }
}

fn validate_library_majors() -> Result<(), String> {
    for (name, version, expected_major) in
        runtime_library_versions()
            .into_iter()
            .map(|(name, version)| {
                let expected_major = PINNED_LIBRARY_VERSIONS
                    .iter()
                    .find_map(|(candidate, major, _)| (*candidate == name).then_some(*major))
                    .expect("each runtime library has a pinned major");
                (name, version, expected_major)
            })
    {
        let actual_major = version >> 16;
        if actual_major != expected_major {
            return Err(format!(
                "{name} major {actual_major} is incompatible; expected {expected_major}"
            ));
        }
    }
    Ok(())
}

fn validate_pinned_runtime() -> Result<(), String> {
    let release = runtime_release();
    if !release.starts_with(PINNED_FFMPEG_RELEASE) {
        return Err(format!(
            "FFmpeg release {release} is incompatible; expected {PINNED_FFMPEG_RELEASE}"
        ));
    }
    for ((name, version), (expected_name, _, expected)) in runtime_library_versions()
        .into_iter()
        .zip(PINNED_LIBRARY_VERSIONS.iter().copied())
    {
        debug_assert_eq!(name, expected_name);
        let actual = version_components(version);
        if actual != expected {
            return Err(format!(
                "{name} {}.{}.{} is incompatible; expected {}.{}.{}",
                actual.0, actual.1, actual.2, expected.0, expected.1, expected.2
            ));
        }
    }
    let missing_encoders = REQUIRED_ENCODERS
        .iter()
        .copied()
        .filter(|name| ffmpeg::encoder::find_by_name(name).is_none())
        .collect::<Vec<_>>();
    if !missing_encoders.is_empty() {
        return Err(format!(
            "required encoders are missing: {}",
            missing_encoders.join(", ")
        ));
    }
    let missing_filters = REQUIRED_FILTERS
        .iter()
        .copied()
        .filter(|name| ffmpeg::filter::find(name).is_none())
        .collect::<Vec<_>>();
    if !missing_filters.is_empty() {
        return Err(format!(
            "required filters are missing: {}",
            missing_filters.join(", ")
        ));
    }
    if !filters_available(&["vidstabdetect", "vidstabtransform"])
        && !filters_available(&["deshake"])
    {
        return Err("stabilization requires vidstabdetect/vidstabtransform or deshake".to_owned());
    }
    let missing_muxers = ["matroska", "mp4"]
        .into_iter()
        .filter(|name| !output_format_available(name))
        .collect::<Vec<_>>();
    if !missing_muxers.is_empty() {
        return Err(format!(
            "required muxers are missing: {}",
            missing_muxers.join(", ")
        ));
    }
    if !ffmpeg::util::license().to_ascii_lowercase().contains("gpl") {
        return Err("the packaged FFmpeg runtime does not report a GPL license".to_owned());
    }
    Ok(())
}

fn runtime_release() -> String {
    unsafe {
        let pointer = ffmpeg::ffi::av_version_info();
        if pointer.is_null() {
            "unknown".to_owned()
        } else {
            CStr::from_ptr(pointer).to_string_lossy().into_owned()
        }
    }
}

fn runtime_library_versions() -> [(&'static str, u32); 4] {
    [
        ("libavformat", ffmpeg::format::version()),
        ("libavcodec", ffmpeg::codec::version()),
        ("libavfilter", ffmpeg::filter::version()),
        ("libavutil", ffmpeg::util::version()),
    ]
}

const fn version_components(version: u32) -> (u32, u32, u32) {
    (version >> 16, (version >> 8) & 0xff, version & 0xff)
}

fn filters_available(names: &[&str]) -> bool {
    names
        .iter()
        .all(|name| ffmpeg::filter::find(name).is_some())
}

fn stream_info(stream: &ffmpeg::Stream<'_>) -> MediaStream {
    let parameters = stream.parameters();
    let metadata = dictionary_to_map(&stream.metadata());
    let kind = match parameters.medium() {
        ffmpeg::media::Type::Video => StreamKind::Video,
        ffmpeg::media::Type::Audio => StreamKind::Audio,
        ffmpeg::media::Type::Data => StreamKind::Data,
        ffmpeg::media::Type::Subtitle => StreamKind::Subtitle,
        ffmpeg::media::Type::Attachment => StreamKind::Attachment,
        ffmpeg::media::Type::Unknown => StreamKind::Unknown,
    };
    let codec_name = match parameters.id().name() {
        "none" | "unknown" => None,
        name => Some(name.to_owned()),
    };
    let disposition = stream.disposition();
    let tagged_frame_count = metadata
        .get("NUMBER_OF_FRAMES")
        .and_then(|value| value.parse().ok());
    let frame_count = semantic_frame_count(
        codec_name.as_deref(),
        positive_u64(stream.frames()),
        tagged_frame_count,
    );
    let raw = unsafe { &*parameters.as_ptr() };

    MediaStream {
        index: stream.index(),
        kind,
        codec_name,
        codec_profile: ffi_string(unsafe {
            ffmpeg::ffi::avcodec_profile_name(raw.codec_id, raw.profile)
        }),
        codec_level: (raw.level >= 0).then_some(raw.level),
        bit_depth: positive_u32(raw.bits_per_raw_sample)
            .or_else(|| positive_u32(raw.bits_per_coded_sample)),
        bit_rate: positive_u64(parameters.bit_rate()),
        duration: metadata_duration(&metadata)
            .or_else(|| stream_duration(stream.duration(), stream.time_base())),
        frame_count,
        frame_rate: rational_to_positive_f64(stream.rate())
            .or_else(|| rational_to_positive_f64(stream.avg_frame_rate())),
        width: positive_u32(raw.width),
        height: positive_u32(raw.height),
        sample_rate: positive_u32(raw.sample_rate),
        channels: positive_u32(raw.ch_layout.nb_channels),
        language: metadata.get("language").cloned(),
        is_default: disposition.contains(ffmpeg::format::stream::Disposition::DEFAULT),
        is_forced: disposition.contains(ffmpeg::format::stream::Disposition::FORCED),
        is_attached_picture: disposition
            .contains(ffmpeg::format::stream::Disposition::ATTACHED_PIC),
        color: ColorCharacteristics {
            range: ffi_string(unsafe { ffmpeg::ffi::av_color_range_name(raw.color_range) }),
            primaries: ffi_string(unsafe {
                ffmpeg::ffi::av_color_primaries_name(raw.color_primaries)
            }),
            transfer: ffi_string(unsafe { ffmpeg::ffi::av_color_transfer_name(raw.color_trc) }),
            space: ffi_string(unsafe { ffmpeg::ffi::av_color_space_name(raw.color_space) }),
            chroma_location: ffi_string(unsafe {
                ffmpeg::ffi::av_chroma_location_name(raw.chroma_location)
            }),
        },
        metadata,
    }
}

fn ffi_string(pointer: *const std::ffi::c_char) -> Option<String> {
    (!pointer.is_null()).then(|| {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    })
}

fn output_format_available(name: &str) -> bool {
    let Ok(name) = std::ffi::CString::new(name) else {
        return false;
    };
    !unsafe { ffmpeg::ffi::av_guess_format(name.as_ptr(), ptr::null(), ptr::null()) }.is_null()
}

fn available_hardware_devices() -> Vec<String> {
    let mut available = Vec::new();
    let mut device_type = ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE;

    loop {
        device_type = unsafe { ffmpeg::ffi::av_hwdevice_iterate_types(device_type) };
        if device_type == ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_NONE {
            break;
        }

        let relevant = matches!(
            device_type,
            ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA
                | ffmpeg::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VIDEOTOOLBOX
        );
        if !relevant {
            continue;
        }

        let mut context = ptr::null_mut();
        let status = unsafe {
            ffmpeg::ffi::av_hwdevice_ctx_create(
                &raw mut context,
                device_type,
                ptr::null(),
                ptr::null_mut(),
                0,
            )
        };
        if status >= 0 {
            let name = unsafe { ffmpeg::ffi::av_hwdevice_get_type_name(device_type) };
            if !name.is_null() {
                available.push(
                    unsafe { CStr::from_ptr(name) }
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        if !context.is_null() {
            unsafe { ffmpeg::ffi::av_buffer_unref(&raw mut context) };
        }
    }

    available
}

fn encoder_is_usable(name: &str, hardware_devices: &[String]) -> bool {
    let Some(required_device) = hardware_device_for_encoder(name) else {
        return true;
    };
    hardware_devices
        .iter()
        .any(|device| device == required_device)
        && hardware_encoder_probe(name)
}

fn hardware_device_for_encoder(name: &str) -> Option<&'static str> {
    if name.ends_with("_nvenc") {
        Some("cuda")
    } else if name.ends_with("_videotoolbox") {
        Some("videotoolbox")
    } else {
        None
    }
}

fn hardware_encoder_probe(name: &str) -> bool {
    const WIDTH: u32 = 320;
    const HEIGHT: u32 = 180;
    let Some(codec) = ffmpeg::encoder::find_by_name(name) else {
        return false;
    };
    let Ok(mut encoder) = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
    else {
        return false;
    };
    encoder.set_width(WIDTH);
    encoder.set_height(HEIGHT);
    encoder.set_format(ffmpeg::format::Pixel::YUV420P);
    encoder.set_time_base(ffmpeg::Rational(1, 30));
    encoder.set_frame_rate(Some(ffmpeg::Rational(30, 1)));
    encoder.set_bit_rate(1_000_000);
    encoder.set_gop(30);
    encoder.set_max_b_frames(0);
    let mut options = ffmpeg::Dictionary::new();
    if name.ends_with("_nvenc") {
        options.set("preset", "p4");
    } else if name.ends_with("_videotoolbox") {
        options.set("allow_sw", "0");
        options.set("realtime", "1");
    }
    let Ok(mut encoder) = encoder.open_with(options) else {
        return false;
    };
    let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, WIDTH, HEIGHT);
    frame.data_mut(0).fill(16);
    frame.data_mut(1).fill(128);
    frame.data_mut(2).fill(128);
    frame.set_pts(Some(0));
    if encoder.send_frame(&frame).is_err() || encoder.send_eof().is_err() {
        return false;
    }
    let mut packet = ffmpeg::Packet::empty();
    for _ in 0..4 {
        match encoder.receive_packet(&mut packet) {
            Ok(()) => return packet.size() > 0,
            Err(ffmpeg::Error::Other {
                errno: ffmpeg::error::EAGAIN,
            }) => {}
            Err(_) => return false,
        }
    }
    false
}

fn metadata_duration(metadata: &BTreeMap<String, String>) -> Option<Duration> {
    metadata
        .get("DURATION")
        .or_else(|| metadata.get("duration"))
        .and_then(|value| parse_duration(value))
}

fn parse_duration(value: &str) -> Option<Duration> {
    let parts = value.split(':').collect::<Vec<_>>();
    let seconds = match parts.as_slice() {
        [hours, minutes, seconds] => {
            let hours = hours.parse::<u32>().ok()?;
            let minutes = minutes.parse::<u32>().ok()?;
            if minutes >= 60 {
                return None;
            }
            f64::from(hours) * 3_600.0 + f64::from(minutes) * 60.0 + seconds.parse::<f64>().ok()?
        }
        [seconds] => seconds.parse::<f64>().ok()?,
        _ => return None,
    };
    positive_seconds_to_duration(seconds)
}

fn dictionary_to_map(dictionary: &ffmpeg::DictionaryRef<'_>) -> BTreeMap<String, String> {
    dictionary
        .iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn stream_duration(value: i64, time_base: ffmpeg::Rational) -> Option<Duration> {
    if value <= 0 || time_base.numerator() <= 0 || time_base.denominator() <= 0 {
        return None;
    }
    let ticks = u128::try_from(value).ok()?;
    let numerator = u128::try_from(time_base.numerator()).ok()?;
    let denominator = u128::try_from(time_base.denominator()).ok()?;
    let total_nanos = ticks
        .checked_mul(numerator)?
        .checked_mul(1_000_000_000)?
        .checked_div(denominator)?;
    let seconds = u64::try_from(total_nanos / 1_000_000_000).ok()?;
    let nanos = u32::try_from(total_nanos % 1_000_000_000).ok()?;
    Some(Duration::new(seconds, nanos))
}

fn microseconds_to_duration(value: i64) -> Option<Duration> {
    if value <= 0 {
        None
    } else {
        u64::try_from(value).ok().map(Duration::from_micros)
    }
}

fn positive_seconds_to_duration(value: f64) -> Option<Duration> {
    if value.is_finite() && value > 0.0 {
        Some(Duration::from_secs_f64(value))
    } else {
        None
    }
}

fn rational_to_positive_f64(value: ffmpeg::Rational) -> Option<f64> {
    if value.numerator() <= 0 || value.denominator() <= 0 {
        None
    } else {
        let result = f64::from(value);
        result.is_finite().then_some(result)
    }
}

fn positive_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok().filter(|value| *value > 0)
}

fn positive_u32(value: i32) -> Option<u32> {
    u32::try_from(value).ok().filter(|value| *value > 0)
}

fn semantic_frame_count(
    codec_name: Option<&str>,
    container_frame_count: Option<u64>,
    tagged_frame_count: Option<u64>,
) -> Option<u64> {
    tagged_frame_count.or_else(|| {
        container_frame_count.map(|count| {
            if codec_name == Some("mov_text") {
                // MP4 timed text stores a clearing sample after each visible
                // cue. ffprobe reports both as frames, while Matroska's
                // NUMBER_OF_FRAMES tag and the Python sparse-stream policy
                // count only visible subtitle cues.
                count.div_ceil(2)
            } else {
                count
            }
        })
    })
}

fn unpack_version(value: u32) -> String {
    format!("{}.{}.{}", value >> 16, (value >> 8) & 0xff, value & 0xff)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ffmpeg_next::Rational;

    use super::{
        encoded_library_marker, estimated_total_frames, fps_rational, hardware_device_for_encoder,
        parse_duration, positive_seconds_to_duration, semantic_frame_count, stream_duration,
        unpack_version,
    };

    #[test]
    fn unpacks_ffmpeg_library_versions() {
        assert_eq!(unpack_version((62 << 16) | (11 << 8) | 0x65), "62.11.101");
    }

    #[test]
    fn maps_hardware_encoders_to_their_runtime_devices() {
        assert_eq!(hardware_device_for_encoder("hevc_nvenc"), Some("cuda"));
        assert_eq!(
            hardware_device_for_encoder("av1_videotoolbox"),
            Some("videotoolbox")
        );
        assert_eq!(hardware_device_for_encoder("libx265"), None);
    }

    #[test]
    fn rescales_stream_duration() {
        assert_eq!(
            stream_duration(90_000, Rational(1, 90_000)),
            Some(Duration::from_secs(1))
        );
        assert_eq!(stream_duration(-1, Rational(1, 1_000)), None);
    }

    #[test]
    fn rejects_invalid_seconds() {
        assert_eq!(positive_seconds_to_duration(f64::NAN), None);
        assert_eq!(positive_seconds_to_duration(0.0), None);
    }

    #[test]
    fn parses_ffmpeg_metadata_durations() {
        assert_eq!(
            parse_duration("00:02:01.750000000"),
            Some(Duration::from_millis(121_750))
        );
        assert_eq!(parse_duration("1.25"), Some(Duration::from_millis(1_250)));
        assert_eq!(parse_duration("00:75:00"), None);
        assert_eq!(parse_duration("unknown"), None);
    }

    #[test]
    fn estimates_python_compatible_progress_frame_totals() {
        assert_eq!(
            estimated_total_frames(Some(Duration::from_secs(100)), Some(29.97)),
            Some(2_997)
        );
        assert_eq!(
            estimated_total_frames(Some(Duration::from_millis(500)), Some(5.0)),
            Some(2)
        );
        assert_eq!(estimated_total_frames(None, Some(30.0)), None);
        assert_eq!(
            estimated_total_frames(Some(Duration::from_secs(1)), Some(f64::NAN)),
            None
        );
    }

    #[test]
    fn represents_common_frame_rates_without_drift() {
        assert_eq!(
            fps_rational(30_000.0 / 1_001.0).unwrap(),
            Rational(30_000, 1_001)
        );
        assert_eq!(fps_rational(15.0).unwrap(), Rational(15, 1));
        assert!(fps_rational(0.0).is_err());
    }

    #[test]
    fn mov_text_frame_counts_exclude_mp4_clearing_samples() {
        assert_eq!(
            semantic_frame_count(Some("mov_text"), Some(2), None),
            Some(1)
        );
        assert_eq!(
            semantic_frame_count(Some("mov_text"), Some(4), None),
            Some(2)
        );
        assert_eq!(semantic_frame_count(Some("subrip"), None, Some(1)), Some(1));
    }

    #[test]
    fn detects_python_mediainfo_encoder_names_from_metadata_or_sei() {
        assert_eq!(
            encoded_library_marker(b"Lavc63.25.100 libx265"),
            Some("x265")
        );
        assert_eq!(
            encoded_library_marker(b"x264 - core 165 r3222"),
            Some("x264")
        );
        assert_eq!(
            encoded_library_marker(b"Svt-Av1 Encoder Lib v4.0"),
            Some("libsvtav1")
        );
        assert_eq!(encoded_library_marker(b"videotoolbox"), None);
    }
}
