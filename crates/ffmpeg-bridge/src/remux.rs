use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ffmpeg::Rescale;
use ffmpeg_next as ffmpeg;
use videoferry_core::{
    AudioStreamAction, ControlDecision, ConversionControl, ConversionEvent, ConversionProgress,
    EngineError, MetadataPolicy, StreamPlan, SubtitleStreamAction,
};

use crate::audio::AudioTranscoder;
use crate::chapters::copy_chapters;
use crate::mux::write_interleaved;
use crate::progress::ProgressMetadata;
use crate::subtitle::SubtitleTranscoder;

static PARTIAL_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

struct PacketCopy<'a> {
    plan: &'a StreamPlan,
    mapping: &'a [Option<usize>],
    audio_mapping: &'a [Option<usize>],
    audio_transcoders: &'a mut [AudioTranscoder],
    subtitle_mapping: &'a [Option<usize>],
    subtitle_transcoders: &'a mut [SubtitleTranscoder],
    input_time_bases: &'a [ffmpeg::Rational],
    decode_delays: &'a [i64],
    start_micros: i64,
    end_micros: i64,
    duration: Duration,
    progress: ProgressMetadata,
    started_at: Instant,
    partial_path: &'a Path,
    control: &'a ConversionControl,
    emit: &'a mut dyn FnMut(ConversionEvent),
}

struct StreamMapping {
    output_indices: Vec<Option<usize>>,
    audio_mapping: Vec<Option<usize>>,
    audio_transcoders: Vec<AudioTranscoder>,
    subtitle_mapping: Vec<Option<usize>>,
    subtitle_transcoders: Vec<SubtitleTranscoder>,
    input_time_bases: Vec<ffmpeg::Rational>,
    decode_delays: Vec<i64>,
}

pub(super) struct PartialOutput {
    path: PathBuf,
    armed: bool,
}

#[derive(Clone, Copy)]
pub(super) struct TrimSpec {
    pub(super) start: Duration,
    pub(super) metadata_policy: MetadataPolicy,
    pub(super) progress: ProgressMetadata,
}

impl PartialOutput {
    pub(super) fn new(destination: &Path) -> Result<Self, EngineError> {
        Ok(Self {
            path: partial_path(destination)?,
            armed: true,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn commit(mut self, destination: &Path) -> Result<(), EngineError> {
        if destination.exists() {
            return Err(EngineError::Failed(format!(
                "output already exists: {}",
                destination.display()
            )));
        }
        std::fs::hard_link(&self.path, destination).map_err(|error| {
            EngineError::Failed(format!(
                "could not publish validated output {}: {error}",
                destination.display()
            ))
        })?;
        std::fs::remove_file(&self.path).map_err(|error| {
            EngineError::Failed(format!(
                "output was published but temporary link {} could not be removed: {error}",
                self.path.display()
            ))
        })?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for PartialOutput {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(super) fn write_trimmed_copy(
    input_path: &Path,
    destination: &Path,
    plan: &StreamPlan,
    spec: TrimSpec,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<PartialOutput, EngineError> {
    let duration = spec.progress.total.ok_or_else(|| {
        EngineError::Unsupported("trim progress requires a known duration".to_owned())
    })?;
    let partial = PartialOutput::new(destination)?;
    let mut input = ffmpeg::format::input(input_path)
        .map_err(|error| EngineError::InvalidMedia(format!("{}: {error}", input_path.display())))?;
    let mut output = ffmpeg::format::output(partial.path()).map_err(ffmpeg_failure)?;
    unsafe {
        (*output.as_mut_ptr()).avoid_negative_ts = ffmpeg::ffi::AVFMT_AVOID_NEG_TS_MAKE_ZERO;
    }

    let mut mapping = add_output_streams(&input, &mut output, plan, spec.metadata_policy)?;

    if spec.metadata_policy == MetadataPolicy::Preserve {
        output.set_metadata(input.metadata().to_owned());
    }
    copy_chapters(
        &input,
        &mut output,
        Some((spec.start, spec.start.saturating_add(duration))),
        spec.metadata_policy == MetadataPolicy::Preserve,
    )?;
    output.write_header().map_err(ffmpeg_failure)?;

    let start_micros = duration_to_micros(spec.start)?;
    let duration_micros = duration_to_micros(duration)?;
    let end_micros = start_micros
        .checked_add(duration_micros)
        .ok_or_else(|| EngineError::Unsupported("trim range is too large".to_owned()))?;
    if start_micros > 0 {
        input
            .seek(start_micros, ..start_micros)
            .map_err(ffmpeg_failure)?;
    }

    let started_at = Instant::now();
    let video_packets = PacketCopy {
        plan,
        mapping: &mapping.output_indices,
        audio_mapping: &mapping.audio_mapping,
        audio_transcoders: &mut mapping.audio_transcoders,
        subtitle_mapping: &mapping.subtitle_mapping,
        subtitle_transcoders: &mut mapping.subtitle_transcoders,
        input_time_bases: &mapping.input_time_bases,
        decode_delays: &mapping.decode_delays,
        start_micros,
        end_micros,
        duration,
        progress: spec.progress,
        started_at,
        partial_path: partial.path(),
        control,
        emit,
    }
    .run(&mut input, &mut output)?;

    for audio in &mut mapping.audio_transcoders {
        let output_time_base = output
            .stream(audio.output_index())
            .ok_or_else(|| EngineError::Failed("output audio stream disappeared".to_owned()))?
            .time_base();
        audio.finish(&mut output, output_time_base)?;
    }

    output.write_trailer().map_err(ffmpeg_failure)?;
    drop(output);
    emit(ConversionEvent::Progress(ConversionProgress {
        overall: None,
        completed: duration,
        total: Some(duration),
        frames: Some(video_packets),
        total_frames: spec.progress.total_frames,
        target_fps: spec.progress.target_fps,
        frames_per_second: u32::try_from(video_packets)
            .ok()
            .and_then(|frames| rate(f64::from(frames), started_at.elapsed())),
        speed: rate(duration.as_secs_f64(), started_at.elapsed()),
        output_bytes: std::fs::metadata(partial.path())
            .ok()
            .map(|metadata| metadata.len()),
    }));
    Ok(partial)
}

impl PacketCopy<'_> {
    fn run(
        &mut self,
        input: &mut ffmpeg::format::context::Input,
        output: &mut ffmpeg::format::context::Output,
    ) -> Result<u64, EngineError> {
        let mut video_packets = 0_u64;
        let mut last_progress = None;
        let mut next_dts = vec![None; self.mapping.len()];
        for (input_stream, mut packet) in input.packets() {
            match self.control.checkpoint() {
                ControlDecision::Continue => {}
                ControlDecision::StopCurrent | ControlDecision::StopAll => {
                    return Err(EngineError::Cancelled);
                }
            }

            let input_index = input_stream.index();
            let Some(output_index) = self.mapping[input_index] else {
                continue;
            };
            let input_time_base = self.input_time_bases[input_index];
            synthesize_missing_timestamps(
                &mut packet,
                self.decode_delays[input_index],
                &mut next_dts[input_index],
            );
            let packet_timestamp = if Some(input_index) == self.plan.video_input_index {
                packet.dts().or_else(|| packet.pts())
            } else {
                packet.pts().or_else(|| packet.dts())
            };
            let packet_micros = packet_timestamp.map(|timestamp| {
                timestamp.rescale(input_time_base, ffmpeg::Rational(1, 1_000_000))
            });
            if packet_micros.is_some_and(|timestamp| timestamp >= self.end_micros) {
                continue;
            }

            let timestamp_offset = self
                .start_micros
                .rescale(ffmpeg::Rational(1, 1_000_000), input_time_base);
            packet.set_pts(packet.pts().map(|timestamp| timestamp - timestamp_offset));
            packet.set_dts(packet.dts().map(|timestamp| timestamp - timestamp_offset));
            if let Some(audio_index) = self.audio_mapping[input_index] {
                let audio = &mut self.audio_transcoders[audio_index];
                let output_time_base = output
                    .stream(audio.output_index())
                    .ok_or_else(|| {
                        EngineError::Failed("output audio stream disappeared".to_owned())
                    })?
                    .time_base();
                audio.process_packet(&packet, output, output_time_base)?;
                continue;
            }
            if let Some(subtitle_index) = self.subtitle_mapping[input_index] {
                let subtitle = &mut self.subtitle_transcoders[subtitle_index];
                let output_time_base = output
                    .stream(subtitle.output_index())
                    .ok_or_else(|| {
                        EngineError::Failed("output subtitle stream disappeared".to_owned())
                    })?
                    .time_base();
                subtitle.process_packet(&packet, output, output_time_base)?;
                continue;
            }
            let output_time_base = output
                .stream(output_index)
                .ok_or_else(|| EngineError::Failed("output stream disappeared".to_owned()))?
                .time_base();
            packet.rescale_ts(input_time_base, output_time_base);
            packet.set_position(-1);
            packet.set_stream(output_index);
            write_interleaved(&mut packet, output)?;

            if Some(input_index) == self.plan.video_input_index {
                video_packets = video_packets.saturating_add(1);
            }
            if let Some(timestamp) = packet_micros {
                let completed_micros = timestamp.saturating_sub(self.start_micros).max(0);
                let completed =
                    Duration::from_micros(u64::try_from(completed_micros).unwrap_or(u64::MAX))
                        .min(self.duration);
                if last_progress
                    .is_none_or(|last| completed.saturating_sub(last) >= PROGRESS_INTERVAL)
                {
                    (self.emit)(ConversionEvent::Progress(ConversionProgress {
                        overall: None,
                        completed,
                        total: Some(self.duration),
                        frames: Some(video_packets),
                        total_frames: self.progress.total_frames,
                        target_fps: self.progress.target_fps,
                        frames_per_second: u32::try_from(video_packets)
                            .ok()
                            .and_then(|frames| rate(f64::from(frames), self.started_at.elapsed())),
                        speed: rate(completed.as_secs_f64(), self.started_at.elapsed()),
                        output_bytes: std::fs::metadata(self.partial_path)
                            .ok()
                            .map(|metadata| metadata.len()),
                    }));
                    last_progress = Some(completed);
                }
            }
        }

        Ok(video_packets)
    }
}

fn add_output_streams(
    input: &ffmpeg::format::context::Input,
    output: &mut ffmpeg::format::context::Output,
    plan: &StreamPlan,
    metadata_policy: MetadataPolicy,
) -> Result<StreamMapping, EngineError> {
    let included = selected_streams(plan);
    let stream_count = usize::try_from(input.nb_streams())
        .map_err(|_| EngineError::InvalidMedia("too many input streams".to_owned()))?;
    let mut mapping = vec![None; stream_count];
    let mut audio_mapping = vec![None; stream_count];
    let mut audio_transcoders = Vec::new();
    let mut subtitle_mapping = vec![None; stream_count];
    let mut subtitle_transcoders = Vec::new();
    let mut input_time_bases = vec![ffmpeg::Rational(0, 1); stream_count];
    let mut decode_delays = vec![0; stream_count];

    for input_stream in input.streams() {
        let input_index = input_stream.index();
        if !included.contains(&input_index) {
            continue;
        }
        input_time_bases[input_index] = input_stream.time_base();
        decode_delays[input_index] =
            unsafe { i64::from((*input_stream.parameters().as_ptr()).video_delay.max(0)) };
        if let Some(AudioStreamAction::TranscodeAc3 { bit_rate }) = plan
            .audio
            .iter()
            .find(|audio| audio.input_index == input_index)
            .map(|audio| &audio.action)
        {
            let audio_index = audio_transcoders.len();
            let audio = AudioTranscoder::new(
                &input_stream,
                output,
                *bit_rate,
                metadata_policy == MetadataPolicy::Preserve,
            )?;
            mapping[input_index] = Some(audio.output_index());
            audio_mapping[input_index] = Some(audio_index);
            audio_transcoders.push(audio);
        } else if let Some(action) = plan
            .subtitles
            .iter()
            .find(|subtitle| subtitle.input_index == input_index)
            .map(|subtitle| &subtitle.action)
            .filter(|action| **action != SubtitleStreamAction::Copy)
        {
            let subtitle_index = subtitle_transcoders.len();
            let subtitle = SubtitleTranscoder::new(
                &input_stream,
                output,
                action,
                metadata_policy == MetadataPolicy::Preserve,
            )?;
            mapping[input_index] = Some(subtitle.output_index());
            subtitle_mapping[input_index] = Some(subtitle_index);
            subtitle_transcoders.push(subtitle);
        } else {
            let mut output_stream = output
                .add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))
                .map_err(ffmpeg_failure)?;
            output_stream.set_parameters(input_stream.parameters());
            output_stream.set_time_base(input_stream.time_base());
            let rate = input_stream.rate();
            if positive_rational(rate) {
                output_stream.set_rate(rate);
            }
            let average_rate = input_stream.avg_frame_rate();
            if positive_rational(average_rate) {
                output_stream.set_avg_frame_rate(average_rate);
            }
            if metadata_policy == MetadataPolicy::Preserve
                || input_stream.parameters().medium() == ffmpeg::media::Type::Attachment
            {
                output_stream.set_metadata(input_stream.metadata().to_owned());
            }
            unsafe {
                (*output_stream.parameters().as_mut_ptr()).codec_tag = 0;
                (*output_stream.as_mut_ptr()).disposition = input_stream.disposition().bits();
            }
            mapping[input_index] = Some(output_stream.index());
        }
    }
    Ok(StreamMapping {
        output_indices: mapping,
        audio_mapping,
        audio_transcoders,
        subtitle_mapping,
        subtitle_transcoders,
        input_time_bases,
        decode_delays,
    })
}

fn synthesize_missing_timestamps(
    packet: &mut ffmpeg::Packet,
    decode_delay: i64,
    next_dts: &mut Option<i64>,
) {
    let packet_duration = packet.duration().max(1);
    let dts = packet.dts().unwrap_or_else(|| {
        next_dts.unwrap_or_else(|| {
            packet
                .pts()
                .unwrap_or(0)
                .saturating_sub(decode_delay.saturating_mul(packet_duration))
        })
    });
    packet.set_dts(Some(dts));
    if packet.pts().is_none() {
        packet.set_pts(Some(dts));
    }
    *next_dts = Some(dts.saturating_add(packet_duration));
}

fn selected_streams(plan: &StreamPlan) -> BTreeSet<usize> {
    plan.video_input_index
        .into_iter()
        .chain(plan.audio.iter().map(|stream| stream.input_index))
        .chain(plan.subtitles.iter().map(|stream| stream.input_index))
        .chain(plan.attachments.iter().copied())
        .collect()
}

fn partial_path(destination: &Path) -> Result<PathBuf, EngineError> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(EngineError::Failed(format!(
            "output directory does not exist: {}",
            parent.display()
        )));
    }
    let stem = destination
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    let extension = destination
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| EngineError::Unsupported("output needs a file extension".to_owned()))?;
    let sequence = PARTIAL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{stem}.videoferry-partial-{}-{sequence}.{extension}",
        std::process::id()
    )))
}

fn duration_to_micros(duration: Duration) -> Result<i64, EngineError> {
    i64::try_from(duration.as_micros())
        .map_err(|_| EngineError::Unsupported("duration exceeds FFmpeg limits".to_owned()))
}

fn ffmpeg_failure(error: ffmpeg::Error) -> EngineError {
    EngineError::Failed(error.to_string())
}

fn rate(value: f64, elapsed: Duration) -> Option<f64> {
    let elapsed = elapsed.as_secs_f64();
    (elapsed > 0.0).then_some(value / elapsed)
}

fn positive_rational(value: ffmpeg::Rational) -> bool {
    value.numerator() > 0 && value.denominator() > 0
}

#[cfg(test)]
mod tests {
    use super::synthesize_missing_timestamps;

    #[test]
    fn reconstructs_leading_decode_timestamps_for_reordered_video() {
        let mut next_dts = None;
        let mut first = ffmpeg_next::Packet::empty();
        first.set_pts(Some(0));
        first.set_duration(33);
        synthesize_missing_timestamps(&mut first, 2, &mut next_dts);
        assert_eq!(first.dts(), Some(-66));
        assert_eq!(next_dts, Some(-33));

        let mut second = ffmpeg_next::Packet::empty();
        second.set_pts(Some(100));
        second.set_duration(33);
        synthesize_missing_timestamps(&mut second, 2, &mut next_dts);
        assert_eq!(second.dts(), Some(-33));
        assert_eq!(next_dts, Some(0));
    }
}
