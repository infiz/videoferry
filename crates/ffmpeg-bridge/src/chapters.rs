use std::time::Duration;

use ffmpeg::Rescale;
use ffmpeg_next as ffmpeg;
use videoferry_core::EngineError;

pub(super) fn copy_chapters(
    input: &ffmpeg::format::context::Input,
    output: &mut ffmpeg::format::context::Output,
    trim_range: Option<(Duration, Duration)>,
    preserve_metadata: bool,
) -> Result<(), EngineError> {
    let trim_start = trim_range
        .map(|(start, _)| duration_micros(start))
        .transpose()?
        .unwrap_or(0);
    let trim_end = trim_range
        .map(|(_, end)| duration_micros(end))
        .transpose()?;
    let microseconds = ffmpeg::Rational(1, 1_000_000);

    for chapter in input.chapters() {
        let chapter_start = chapter.start().rescale(chapter.time_base(), microseconds);
        let chapter_end = chapter.end().rescale(chapter.time_base(), microseconds);
        let clipped_start = chapter_start.max(trim_start);
        let clipped_end = trim_end.map_or(chapter_end, |end| chapter_end.min(end));
        if clipped_end <= clipped_start {
            continue;
        }
        let metadata = preserve_metadata.then(|| {
            chapter
                .metadata()
                .iter()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect::<Vec<_>>()
        });
        let title = metadata
            .as_ref()
            .and_then(|entries| entries.iter().find(|(key, _)| key == "title"))
            .map_or("", |(_, value)| value.as_str());
        let mut copied = output
            .add_chapter(
                chapter.id(),
                microseconds,
                clipped_start.saturating_sub(trim_start),
                clipped_end.saturating_sub(trim_start),
                title,
            )
            .map_err(ffmpeg_failure)?;
        if let Some(metadata) = metadata {
            for (key, value) in metadata {
                copied.set_metadata(key, value);
            }
        }
    }
    Ok(())
}

fn duration_micros(duration: Duration) -> Result<i64, EngineError> {
    i64::try_from(duration.as_micros())
        .map_err(|_| EngineError::Unsupported("chapter trim range is too large".to_owned()))
}

fn ffmpeg_failure(error: ffmpeg::Error) -> EngineError {
    EngineError::Failed(format!("copying chapters: {error}"))
}
