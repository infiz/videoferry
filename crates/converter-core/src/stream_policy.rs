use std::time::Duration;

use crate::{Container, MediaInfo, MediaStream, StreamKind};

const DEFAULT_DURATION_TOLERANCE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStreamAction {
    Copy,
    TranscodeAc3 { bit_rate: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedAudioStream {
    pub input_index: usize,
    pub output_ordinal: usize,
    pub action: AudioStreamAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleStreamAction {
    Copy,
    TranscodeSrt,
    TranscodeMovText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSubtitleStream {
    pub input_index: usize,
    pub output_ordinal: usize,
    pub action: SubtitleStreamAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamSkipReason {
    UnknownCodec,
    SparseSubtitle {
        frame_count: u64,
        duration: Duration,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedStream {
    pub input_index: usize,
    pub kind: StreamKind,
    pub reason: StreamSkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamPlan {
    pub video_input_index: Option<usize>,
    pub audio: Vec<PlannedAudioStream>,
    pub subtitles: Vec<PlannedSubtitleStream>,
    pub attachments: Vec<usize>,
    pub skipped: Vec<SkippedStream>,
}

#[must_use]
pub fn build_stream_plan(media: &MediaInfo, container: Container) -> StreamPlan {
    build_stream_plan_with_tolerance(media, container, DEFAULT_DURATION_TOLERANCE)
}

#[must_use]
pub fn build_stream_plan_with_tolerance(
    media: &MediaInfo,
    container: Container,
    duration_tolerance: Duration,
) -> StreamPlan {
    let mut plan = StreamPlan {
        video_input_index: media
            .streams
            .iter()
            .find(|stream| stream.kind == StreamKind::Video && !stream.is_attached_picture)
            .map(|stream| stream.index),
        ..StreamPlan::default()
    };

    for stream in &media.streams {
        match stream.kind {
            StreamKind::Audio => plan_audio(stream, container, &mut plan),
            StreamKind::Subtitle => plan_subtitle(
                stream,
                media.duration,
                duration_tolerance,
                container,
                &mut plan,
            ),
            StreamKind::Attachment if container == Container::Matroska => {
                plan.attachments.push(stream.index);
            }
            _ => {}
        }
    }

    plan
}

fn plan_audio(stream: &MediaStream, container: Container, plan: &mut StreamPlan) {
    let Some(codec) = valid_codec_name(stream) else {
        plan.skipped.push(SkippedStream {
            input_index: stream.index,
            kind: StreamKind::Audio,
            reason: StreamSkipReason::UnknownCodec,
        });
        return;
    };

    let action = if container == Container::Matroska && codec.eq_ignore_ascii_case("dts") {
        AudioStreamAction::TranscodeAc3 { bit_rate: 640_000 }
    } else {
        AudioStreamAction::Copy
    };
    plan.audio.push(PlannedAudioStream {
        input_index: stream.index,
        output_ordinal: plan.audio.len(),
        action,
    });
}

fn plan_subtitle(
    stream: &MediaStream,
    source_duration: Option<Duration>,
    duration_tolerance: Duration,
    container: Container,
    plan: &mut StreamPlan,
) {
    let Some(codec) = valid_codec_name(stream) else {
        plan.skipped.push(SkippedStream {
            input_index: stream.index,
            kind: StreamKind::Subtitle,
            reason: StreamSkipReason::UnknownCodec,
        });
        return;
    };

    if let (Some(frame_count), Some(duration), Some(source_duration)) =
        (stream.frame_count, stream.duration, source_duration)
        && frame_count <= 1
        && duration.saturating_add(duration_tolerance) < source_duration
    {
        plan.skipped.push(SkippedStream {
            input_index: stream.index,
            kind: StreamKind::Subtitle,
            reason: StreamSkipReason::SparseSubtitle {
                frame_count,
                duration,
            },
        });
        return;
    }

    let action = match container {
        Container::Matroska if codec.eq_ignore_ascii_case("mov_text") => {
            SubtitleStreamAction::TranscodeSrt
        }
        Container::Mp4
            if ["subrip", "srt", "ass", "ssa", "webvtt"]
                .iter()
                .any(|candidate| codec.eq_ignore_ascii_case(candidate)) =>
        {
            SubtitleStreamAction::TranscodeMovText
        }
        _ => SubtitleStreamAction::Copy,
    };
    plan.subtitles.push(PlannedSubtitleStream {
        input_index: stream.index,
        output_ordinal: plan.subtitles.len(),
        action,
    });
}

fn valid_codec_name(stream: &MediaStream) -> Option<&str> {
    stream.codec_name.as_deref().filter(|name| {
        !name.is_empty()
            && !name.eq_ignore_ascii_case("unknown")
            && !name.eq_ignore_ascii_case("none")
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    fn media(streams: Vec<MediaStream>, duration: Duration) -> MediaInfo {
        MediaInfo {
            path: PathBuf::from("fixture.mkv"),
            container_name: "matroska".to_owned(),
            duration: Some(duration),
            file_size: None,
            bit_rate: None,
            width: None,
            height: None,
            frame_rate: None,
            streams,
            metadata: BTreeMap::new(),
        }
    }

    fn stream(index: usize, kind: StreamKind, codec: Option<&str>) -> MediaStream {
        MediaStream {
            index,
            kind,
            codec_name: codec.map(str::to_owned),
            codec_profile: None,
            codec_level: None,
            bit_depth: None,
            bit_rate: None,
            duration: None,
            frame_count: None,
            frame_rate: None,
            width: None,
            height: None,
            sample_rate: None,
            channels: None,
            language: None,
            is_default: false,
            is_forced: false,
            is_attached_picture: false,
            color: crate::ColorCharacteristics {
                range: None,
                primaries: None,
                transfer: None,
                space: None,
                chroma_location: None,
            },
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn copies_valid_audio_and_transcodes_dts_for_matroska() {
        let source = media(
            vec![
                stream(0, StreamKind::Video, Some("h264")),
                stream(1, StreamKind::Audio, Some("aac")),
                stream(3, StreamKind::Audio, Some("dts")),
                stream(4, StreamKind::Audio, Some("unknown")),
            ],
            Duration::from_secs(60),
        );

        let plan = build_stream_plan(&source, Container::Matroska);

        assert_eq!(plan.video_input_index, Some(0));
        assert_eq!(
            plan.audio,
            vec![
                PlannedAudioStream {
                    input_index: 1,
                    output_ordinal: 0,
                    action: AudioStreamAction::Copy,
                },
                PlannedAudioStream {
                    input_index: 3,
                    output_ordinal: 1,
                    action: AudioStreamAction::TranscodeAc3 { bit_rate: 640_000 },
                },
            ]
        );
        assert_eq!(
            plan.skipped,
            vec![SkippedStream {
                input_index: 4,
                kind: StreamKind::Audio,
                reason: StreamSkipReason::UnknownCodec,
            }]
        );
    }

    #[test]
    fn converts_mov_text_and_keeps_other_subtitles_for_matroska() {
        let source = media(
            vec![
                stream(2, StreamKind::Subtitle, Some("mov_text")),
                stream(4, StreamKind::Subtitle, Some("subrip")),
            ],
            Duration::from_secs(60),
        );

        let plan = build_stream_plan(&source, Container::Matroska);

        assert_eq!(
            plan.subtitles,
            vec![
                PlannedSubtitleStream {
                    input_index: 2,
                    output_ordinal: 0,
                    action: SubtitleStreamAction::TranscodeSrt,
                },
                PlannedSubtitleStream {
                    input_index: 4,
                    output_ordinal: 1,
                    action: SubtitleStreamAction::Copy,
                },
            ]
        );
    }

    #[test]
    fn rejects_a_one_frame_subtitle_that_ends_early() {
        let mut sparse = stream(3, StreamKind::Subtitle, Some("subrip"));
        sparse.frame_count = Some(1);
        sparse.duration = Some(Duration::from_millis(4_200));
        let source = media(vec![sparse], Duration::from_millis(2_628_906));

        let plan = build_stream_plan(&source, Container::Matroska);

        assert!(plan.subtitles.is_empty());
        assert_eq!(
            plan.skipped,
            vec![SkippedStream {
                input_index: 3,
                kind: StreamKind::Subtitle,
                reason: StreamSkipReason::SparseSubtitle {
                    frame_count: 1,
                    duration: Duration::from_millis(4_200),
                },
            }]
        );
    }

    #[test]
    fn marks_text_subtitles_for_mp4_conversion() {
        let source = media(
            vec![stream(2, StreamKind::Subtitle, Some("subrip"))],
            Duration::from_secs(60),
        );

        assert_eq!(
            build_stream_plan(&source, Container::Mp4).subtitles,
            vec![PlannedSubtitleStream {
                input_index: 2,
                output_ordinal: 0,
                action: SubtitleStreamAction::TranscodeMovText,
            }]
        );
    }

    #[test]
    fn ignores_cover_art_when_selecting_the_primary_video() {
        let mut cover = stream(0, StreamKind::Video, Some("mjpeg"));
        cover.is_attached_picture = true;
        let source = media(
            vec![cover, stream(1, StreamKind::Video, Some("hevc"))],
            Duration::from_secs(1),
        );

        assert_eq!(
            build_stream_plan(&source, Container::Matroska).video_input_index,
            Some(1)
        );
    }

    #[test]
    fn matroska_keeps_attachments_but_all_outputs_exclude_data_streams() {
        let source = media(
            vec![
                stream(0, StreamKind::Video, Some("h264")),
                stream(1, StreamKind::Attachment, Some("ttf")),
                stream(2, StreamKind::Data, Some("bin_data")),
            ],
            Duration::from_secs(1),
        );

        assert_eq!(
            build_stream_plan(&source, Container::Matroska).attachments,
            [1]
        );
        assert!(
            build_stream_plan(&source, Container::Mp4)
                .attachments
                .is_empty()
        );
    }
}
