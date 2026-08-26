#[cfg(feature = "native-ffmpeg")]
use std::path::PathBuf;

#[cfg(feature = "native-ffmpeg")]
use videoferry_core::{
    ContentMode, ConversionControl, ConversionRequest, Encoder, FpsPolicy, MediaEngine,
    QueueSettings,
};
#[cfg(feature = "native-ffmpeg")]
use videoferry_ffmpeg::NativeEngine;

#[cfg(feature = "native-ffmpeg")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: transcode <input> <output> [x265|x264|svtav1]")?;
    let output = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .ok_or("usage: transcode <input> <output> [x265|x264|svtav1]")?;
    let encoder = match std::env::args().nth(3).as_deref().unwrap_or("x265") {
        "x265" => Encoder::X265,
        "x264" => Encoder::X264,
        "svtav1" => Encoder::SvtAv1,
        value => return Err(format!("unknown encoder: {value}").into()),
    };
    let fps = match std::env::args().nth(4).as_deref() {
        None | Some("source") => FpsPolicy::Source,
        Some("shared") => FpsPolicy::SharedLowest,
        Some(value) => FpsPolicy::Exact(value.parse::<f64>()?),
    };
    let speed_preset = if encoder == Encoder::SvtAv1 {
        "6"
    } else {
        "medium"
    };
    let settings = QueueSettings {
        mode: ContentMode::Tv,
        encoder,
        fps,
        quality: Some(28.0),
        speed_preset: Some(speed_preset.to_owned()),
        ..QueueSettings::default()
    };

    NativeEngine::new()?.convert(
        &ConversionRequest {
            input,
            output,
            settings,
        },
        &ConversionControl::new(),
        &mut |event| println!("{event:?}"),
    )?;
    Ok(())
}

#[cfg(not(feature = "native-ffmpeg"))]
fn main() {
    eprintln!("enable the native-ffmpeg feature");
}
