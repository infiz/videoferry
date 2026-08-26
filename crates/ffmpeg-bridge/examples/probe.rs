use std::env;
use std::path::Path;

#[cfg(feature = "native-ffmpeg")]
use videoferry_core::{MediaEngine, StreamKind};

#[cfg(feature = "native-ffmpeg")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args().nth(1).ok_or("usage: probe <media-path>")?;
    let engine = videoferry_ffmpeg::NativeEngine::new()?;
    let media = engine.probe(Path::new(&path))?;
    let primary_video = media
        .streams
        .iter()
        .find(|stream| stream.kind == StreamKind::Video && !stream.is_attached_picture)
        .ok_or("media has no primary video stream")?;
    println!("{}", engine.version_summary()?);
    println!("Capabilities: {:#?}", engine.capabilities()?);
    println!(
        "PrimaryVideo: codec={} width={} height={} duration_ms={}",
        primary_video.codec_name.as_deref().unwrap_or("unknown"),
        primary_video.width.unwrap_or(0),
        primary_video.height.unwrap_or(0),
        media.duration.map_or(0, |duration| duration.as_millis())
    );
    println!("Media: {media:#?}");
    println!(
        "Encoded library: {:?}",
        engine.encoded_library_name(Path::new(&path))?
    );
    Ok(())
}

#[cfg(not(feature = "native-ffmpeg"))]
fn main() {
    let _ = env::args();
    let _ = Path::new("");
    eprintln!("rebuild this example with --features native-ffmpeg");
}
