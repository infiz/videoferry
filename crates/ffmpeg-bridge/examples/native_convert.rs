use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use videoferry_core::{
    ContentMode, ConversionControl, ConversionEvent, ConversionRequest, Encoder, FpsPolicy,
    MediaEngine, MetadataPolicy,
};
use videoferry_ffmpeg::NativeEngine;
use videoferry_presets::default_settings;

#[expect(
    clippy::too_many_lines,
    reason = "the diagnostic runner keeps all environment overrides visible together"
)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let input = PathBuf::from(arguments.next().ok_or("missing input path")?);
    let output = PathBuf::from(arguments.next().ok_or("missing output path")?);

    let mode = match std::env::var("VIDEOFERRY_MODE").as_deref() {
        Ok("tv") | Err(_) => ContentMode::Tv,
        Ok("animation") => ContentMode::Animation,
        Ok("camera") => ContentMode::CameraVideos,
        Ok("stabilize") => ContentMode::Stabilize,
        Ok("slideshow") => ContentMode::PhotoSlideshow,
        Ok("trim") => ContentMode::Trim,
        Ok(name) => return Err(format!("unknown VIDEOFERRY_MODE: {name}").into()),
    };
    let encoder = match std::env::var("VIDEOFERRY_ENCODER").as_deref() {
        Ok(name) => match name {
            "x265" => Encoder::X265,
            "x264" => Encoder::X264,
            "svtav1" => Encoder::SvtAv1,
            "hevc_nvenc" => Encoder::HevcNvenc,
            "h264_nvenc" => Encoder::H264Nvenc,
            "av1_nvenc" => Encoder::Av1Nvenc,
            "h264_videotoolbox" => Encoder::H264VideoToolbox,
            "hevc_videotoolbox" => Encoder::HevcVideoToolbox,
            "av1_videotoolbox" => Encoder::Av1VideoToolbox,
            _ => return Err(format!("unknown VIDEOFERRY_ENCODER: {name}").into()),
        },
        Err(_) => Encoder::X265,
    };
    let mut settings = default_settings(mode, encoder);
    // Shared-lowest FPS is a queue/folder policy. This single-file diagnostic
    // runner deliberately preserves the source rate unless explicitly set.
    settings.fps = FpsPolicy::Source;
    if let Ok(value) = std::env::var("VIDEOFERRY_FPS") {
        settings.fps = if value == "shared-lowest" {
            FpsPolicy::SharedLowest
        } else {
            FpsPolicy::Exact(value.parse()?)
        };
    }
    if let Ok(value) = std::env::var("VIDEOFERRY_QUALITY") {
        settings.quality = Some(value.parse()?);
    }
    if let Ok(value) = std::env::var("VIDEOFERRY_SPEED") {
        settings.speed_preset = Some(value);
    }
    if let Some(path) = std::env::var_os("VIDEOFERRY_CAMERA_LUT") {
        settings.mode = ContentMode::CameraVideos;
        settings.camera_lut_path = Some(PathBuf::from(path));
        settings.fps = FpsPolicy::Source;
        settings.metadata = MetadataPolicy::Preserve;
        settings.quality = Some(match settings.encoder {
            Encoder::X265 => 18.0,
            Encoder::X264 => 23.0,
            Encoder::SvtAv1 => 24.0,
            _ => return Err("camera LUT example requires a software encoder".into()),
        });
    }
    if let Ok(strength) = std::env::var("VIDEOFERRY_STABILIZE") {
        settings.mode = ContentMode::Stabilize;
        settings.stabilize_strength = strength;
        settings.fps = FpsPolicy::Source;
        settings.metadata = MetadataPolicy::Preserve;
    }
    if let Ok(resolution) = std::env::var("VIDEOFERRY_SLIDESHOW") {
        let (width, height) = resolution
            .split_once('x')
            .ok_or("VIDEOFERRY_SLIDESHOW must be WIDTHxHEIGHT")?;
        settings.mode = ContentMode::PhotoSlideshow;
        settings.slideshow_resolution = (width.parse()?, height.parse()?);
        settings.slideshow_fps =
            std::env::var("VIDEOFERRY_SLIDESHOW_FPS").map_or(Ok(30), |value| value.parse())?;
        settings.photo_interval = Duration::from_secs_f64(
            std::env::var("VIDEOFERRY_SLIDESHOW_INTERVAL")
                .map_or(Ok(4.0), |value| value.parse())?,
        );
        if let Some(paths) = std::env::var_os("VIDEOFERRY_SLIDESHOW_AUDIO") {
            settings.slideshow_audio_paths = std::env::split_paths(&paths).collect();
        }
        settings.slideshow_collage = std::env::var_os("VIDEOFERRY_SLIDESHOW_COLLAGE").is_some();
    }
    if let Some(start) = arguments.next() {
        let end = arguments.next().ok_or("trim start requires trim end")?;
        settings.mode = ContentMode::Trim;
        settings.trim_start = Some(Duration::from_secs(start.to_string_lossy().parse()?));
        settings.trim_end = Some(Duration::from_secs(end.to_string_lossy().parse()?));
    }
    if arguments.next().is_some() {
        return Err("expected input, output, and optional trim start/end seconds".into());
    }
    let request = ConversionRequest {
        input,
        output,
        settings,
    };
    let control = Arc::new(ConversionControl::new());
    control.set_preview_enabled(std::env::var_os("VIDEOFERRY_PREVIEW").is_some());
    if let Ok(value) = std::env::var("VIDEOFERRY_CANCEL_AFTER_MS") {
        let delay = Duration::from_millis(value.parse()?);
        let cancellation = Arc::clone(&control);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            cancellation.stop_current();
        });
    }
    NativeEngine::new()?.convert(&request, control.as_ref(), &mut |event| match event {
        ConversionEvent::Preview(preview) => {
            eprintln!(
                "Preview({}x{}, {} RGBA bytes)",
                preview.width,
                preview.height,
                preview.rgba.len()
            );
        }
        event => eprintln!("{event:?}"),
    })?;
    Ok(())
}
