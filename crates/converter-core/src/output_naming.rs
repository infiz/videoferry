use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Container;

#[must_use]
pub fn conversion_output_path(input: &Path, container: Container) -> PathBuf {
    input.with_extension(container.extension())
}

#[must_use]
pub fn stabilized_output_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("video");
    let extension = input
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mp4");
    input.with_file_name(format!("{stem}_stabilized.{extension}"))
}

#[must_use]
pub fn trim_output_path(input: &Path, start: Duration, end: Duration) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("video");
    let extension = input
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("mkv");
    input.with_file_name(format!(
        "{stem}_{}_{}.{}",
        compact_time(start),
        compact_time(end),
        extension
    ))
}

fn compact_time(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    format!("{hours:02}{minutes:02}{seconds:02}")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{conversion_output_path, stabilized_output_path, trim_output_path};
    use crate::Container;

    #[test]
    fn matches_python_trim_output_naming() {
        assert_eq!(
            trim_output_path(
                Path::new("episode.mkv"),
                Duration::from_secs(65),
                Duration::from_secs(3_661),
            ),
            PathBuf::from("episode_000105_010101.mkv")
        );
    }

    #[test]
    fn conversion_replaces_the_source_extension() {
        assert_eq!(
            conversion_output_path(Path::new("episode.mov"), Container::Matroska),
            PathBuf::from("episode.mkv")
        );
        assert_eq!(
            conversion_output_path(Path::new("camera.mkv"), Container::Mp4),
            PathBuf::from("camera.mp4")
        );
    }

    #[test]
    fn stabilization_keeps_the_source_extension() {
        assert_eq!(
            stabilized_output_path(Path::new("clip.mov")),
            PathBuf::from("clip_stabilized.mov")
        );
    }
}
