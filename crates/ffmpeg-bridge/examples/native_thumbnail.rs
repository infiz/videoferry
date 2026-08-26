use std::error::Error;
use std::path::PathBuf;

use videoferry_ffmpeg::NativeEngine;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next().ok_or("expected a photo path")?;
    let engine = NativeEngine::new()?;
    let preview = if first == "--collage" {
        let paths = arguments.map(PathBuf::from).collect::<Vec<_>>();
        if paths.is_empty() {
            return Err("expected collage photo paths".into());
        }
        let groups = engine.slideshow_review_groups(&paths, true)?;
        println!("collage groups={}", groups.len());
        engine.slideshow_review_thumbnail(&groups[0], true, 640, 360)?
    } else {
        engine.photo_thumbnail(&PathBuf::from(first), 640, 360)?
    };
    println!(
        "{}x{} RGBA bytes={}",
        preview.width,
        preview.height,
        preview.rgba.len()
    );
    Ok(())
}
