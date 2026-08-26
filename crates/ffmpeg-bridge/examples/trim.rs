#[cfg(feature = "native-ffmpeg")]
use std::path::PathBuf;
#[cfg(feature = "native-ffmpeg")]
use std::time::Duration;

#[cfg(feature = "native-ffmpeg")]
use videoferry_core::{
    ContentMode, ConversionControl, ConversionRequest, MediaEngine, MetadataPolicy, QueueSettings,
};
#[cfg(feature = "native-ffmpeg")]
use videoferry_ffmpeg::NativeEngine;

#[cfg(feature = "native-ffmpeg")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: trim <input> <output>")?;
    let output = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .ok_or("usage: trim <input> <output>")?;
    let start = std::env::args()
        .nth(3)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(0);
    let end = std::env::args()
        .nth(4)
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(1);
    let settings = QueueSettings {
        mode: ContentMode::Trim,
        trim_start: Some(Duration::from_secs(start)),
        trim_end: Some(Duration::from_secs(end)),
        metadata: MetadataPolicy::Preserve,
        ..QueueSettings::default()
    };
    let engine = NativeEngine::new()?;
    engine.convert(
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
