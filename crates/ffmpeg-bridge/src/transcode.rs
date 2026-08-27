use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use ffmpeg::Rescale;
use ffmpeg_next as ffmpeg;
use videoferry_core::{
    AudioStreamAction, ControlDecision, ConversionControl, ConversionEvent, ConversionPreview,
    ConversionProgress, EngineError, MetadataPolicy, QueueSettings, StreamPlan,
    SubtitleStreamAction,
};

use crate::audio::AudioTranscoder;
use crate::chapters::copy_chapters;
use crate::mux::write_interleaved;
use crate::progress::{ProgressMetadata, ProgressPhase, phase_progress};
use crate::remux::PartialOutput;
use crate::stabilize::{self, StabilizationPlan};
use crate::subtitle::SubtitleTranscoder;

const PREVIEW_INTERVAL: Duration = Duration::from_secs(1);

struct PreviewRenderer {
    source_format: ffmpeg::format::Pixel,
    source_width: u32,
    source_height: u32,
    width: u32,
    height: u32,
    scaler: ffmpeg::software::scaling::Context,
    rgba: ffmpeg::frame::Video,
}

impl PreviewRenderer {
    fn new(frame: &ffmpeg::frame::Video, width: u32, height: u32) -> Result<Self, EngineError> {
        let source_format = frame.format();
        let source_width = frame.width();
        let source_height = frame.height();
        let scaler = ffmpeg::software::scaling::Context::get(
            source_format,
            source_width,
            source_height,
            ffmpeg::format::Pixel::RGBA,
            width,
            height,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )
        .map_err(ffmpeg_failure)?;
        Ok(Self {
            source_format,
            source_width,
            source_height,
            width,
            height,
            scaler,
            rgba: ffmpeg::frame::Video::new(ffmpeg::format::Pixel::RGBA, width, height),
        })
    }

    fn matches(&self, frame: &ffmpeg::frame::Video, width: u32, height: u32) -> bool {
        self.source_format == frame.format()
            && self.source_width == frame.width()
            && self.source_height == frame.height()
            && self.width == width
            && self.height == height
    }

    fn render(&mut self, frame: &ffmpeg::frame::Video) -> Result<ConversionPreview, EngineError> {
        self.scaler
            .run(frame, &mut self.rgba)
            .map_err(ffmpeg_failure)?;
        let row_bytes = usize::try_from(self.width)
            .map_err(|error| EngineError::Unsupported(error.to_string()))?
            .checked_mul(4)
            .ok_or_else(|| EngineError::Unsupported("preview frame is too wide".to_owned()))?;
        let rows = usize::try_from(self.height)
            .map_err(|error| EngineError::Unsupported(error.to_string()))?;
        let mut pixels = Vec::with_capacity(row_bytes.saturating_mul(rows));
        for row in 0..rows {
            let start = row.saturating_mul(self.rgba.stride(0));
            pixels.extend_from_slice(&self.rgba.data(0)[start..start + row_bytes]);
        }
        Ok(ConversionPreview {
            width: self.width,
            height: self.height,
            rgba: pixels.into(),
        })
    }
}

struct VideoTranscoder {
    decoder: ffmpeg::decoder::Video,
    encoder: ffmpeg::encoder::Video,
    filter: ffmpeg::filter::Graph,
    input_time_base: ffmpeg::Rational,
    encoder_time_base: ffmpeg::Rational,
    target_frame_rate: Option<ffmpeg::Rational>,
    progress: ProgressMetadata,
    output_index: usize,
    frames: u64,
    last_progress: Option<Duration>,
    last_preview: Option<Instant>,
    preview_renderer: Option<PreviewRenderer>,
    first_target_pts: Option<i64>,
    next_output_pts: i64,
    started_at: Instant,
    two_pass_progress: bool,
}

struct OutputSetup {
    transcoder: VideoTranscoder,
    audio_transcoders: Vec<AudioTranscoder>,
    audio_mapping: Vec<Option<usize>>,
    subtitle_transcoders: Vec<SubtitleTranscoder>,
    subtitle_mapping: Vec<Option<usize>>,
    mapping: Vec<Option<usize>>,
    input_time_bases: Vec<ffmpeg::Rational>,
}

#[derive(Clone, Copy)]
pub(super) struct TranscodeSpec<'a> {
    pub(super) settings: &'a QueueSettings,
    pub(super) target_frame_rate: Option<ffmpeg::Rational>,
    pub(super) progress: ProgressMetadata,
}

pub(super) fn write_video_transcode(
    input_path: &Path,
    destination: &Path,
    plan: &StreamPlan,
    spec: TranscodeSpec<'_>,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<PartialOutput, EngineError> {
    let partial = PartialOutput::new(destination)?;
    let stabilization = stabilization_plan(input_path, plan, spec, control, emit)?;
    let mut input = ffmpeg::format::input(input_path)
        .map_err(|error| EngineError::InvalidMedia(format!("{}: {error}", input_path.display())))?;
    let mut output = ffmpeg::format::output(partial.path()).map_err(ffmpeg_failure)?;

    let mut setup = create_output_streams(&input, &mut output, plan, spec, stabilization.as_ref())?;
    copy_container_context(&input, &mut output, spec.settings.metadata)?;
    output
        .write_header()
        .map_err(|error| EngineError::Failed(format!("writing output header: {error}")))?;
    let output_time_bases = output
        .streams()
        .map(|stream| stream.time_base())
        .collect::<Vec<_>>();

    for (stream, mut packet) in input.packets() {
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        let input_index = stream.index();
        let Some(output_index) = setup.mapping[input_index] else {
            continue;
        };
        if Some(input_index) == plan.video_input_index {
            setup
                .transcoder
                .decoder
                .send_packet(&packet)
                .map_err(ffmpeg_failure)?;
            setup.transcoder.drain_frames(
                &mut output,
                output_time_bases[output_index],
                spec.progress.total,
                partial.path(),
                control,
                emit,
            )?;
        } else if let Some(audio_index) = setup.audio_mapping[input_index] {
            let audio = &mut setup.audio_transcoders[audio_index];
            audio.process_packet(
                &packet,
                &mut output,
                output_time_bases[audio.output_index()],
            )?;
        } else if let Some(subtitle_index) = setup.subtitle_mapping[input_index] {
            let subtitle = &mut setup.subtitle_transcoders[subtitle_index];
            subtitle.process_packet(
                &packet,
                &mut output,
                output_time_bases[subtitle.output_index()],
            )?;
        } else {
            packet.rescale_ts(
                setup.input_time_bases[input_index],
                output_time_bases[output_index],
            );
            packet.set_position(-1);
            packet.set_stream(output_index);
            write_interleaved(&mut packet, &mut output)?;
        }
    }

    setup
        .transcoder
        .decoder
        .send_eof()
        .map_err(ffmpeg_failure)?;
    setup.transcoder.drain_frames(
        &mut output,
        output_time_bases[setup.transcoder.output_index],
        spec.progress.total,
        partial.path(),
        control,
        emit,
    )?;
    setup.transcoder.finish_filter(
        &mut output,
        output_time_bases[setup.transcoder.output_index],
    )?;
    setup
        .transcoder
        .encoder
        .send_eof()
        .map_err(ffmpeg_failure)?;
    setup.transcoder.drain_packets(
        &mut output,
        output_time_bases[setup.transcoder.output_index],
    )?;
    for audio in &mut setup.audio_transcoders {
        audio.finish(&mut output, output_time_bases[audio.output_index()])?;
    }
    output.write_trailer().map_err(ffmpeg_failure)?;
    drop(output);
    Ok(partial)
}

fn copy_container_context(
    input: &ffmpeg::format::context::Input,
    output: &mut ffmpeg::format::context::Output,
    metadata_policy: MetadataPolicy,
) -> Result<(), EngineError> {
    if metadata_policy == MetadataPolicy::Preserve {
        output.set_metadata(input.metadata().to_owned());
    }
    copy_chapters(
        input,
        output,
        None,
        metadata_policy == MetadataPolicy::Preserve,
    )
}

fn stabilization_plan(
    input_path: &Path,
    plan: &StreamPlan,
    spec: TranscodeSpec<'_>,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<Option<StabilizationPlan>, EngineError> {
    if spec.settings.mode != videoferry_core::ContentMode::Stabilize {
        return Ok(None);
    }
    let video_index = plan
        .video_input_index
        .ok_or_else(|| EngineError::InvalidMedia("no video stream found".to_owned()))?;
    stabilize::prepare(
        input_path,
        video_index,
        &spec.settings.stabilize_strength,
        spec.progress,
        control,
        emit,
    )
    .map(Some)
}

fn create_output_streams(
    input: &ffmpeg::format::context::Input,
    output: &mut ffmpeg::format::context::Output,
    plan: &StreamPlan,
    spec: TranscodeSpec<'_>,
    stabilization: Option<&StabilizationPlan>,
) -> Result<OutputSetup, EngineError> {
    let selected = selected_streams(plan);
    let stream_count = usize::try_from(input.nb_streams())
        .map_err(|_| EngineError::InvalidMedia("too many input streams".to_owned()))?;
    let mut mapping = vec![None; stream_count];
    let mut audio_mapping = vec![None; stream_count];
    let mut subtitle_mapping = vec![None; stream_count];
    let mut input_time_bases = vec![ffmpeg::Rational(0, 1); stream_count];
    let mut transcoder = None;
    let mut audio_transcoders = Vec::new();
    let mut subtitle_transcoders = Vec::new();

    for input_stream in input.streams() {
        let input_index = input_stream.index();
        if !selected.contains(&input_index) {
            continue;
        }
        input_time_bases[input_index] = input_stream.time_base();
        let output_index = output.nb_streams() as usize;
        if Some(input_index) == plan.video_input_index {
            transcoder = Some(VideoTranscoder::new(
                &input_stream,
                output,
                output_index,
                spec.settings,
                spec.target_frame_rate,
                spec.progress,
                stabilization,
            )?);
        } else if let Some(AudioStreamAction::TranscodeAc3 { bit_rate }) = plan
            .audio
            .iter()
            .find(|audio| audio.input_index == input_index)
            .map(|audio| &audio.action)
        {
            let audio_index = audio_transcoders.len();
            audio_transcoders.push(AudioTranscoder::new(
                &input_stream,
                output,
                *bit_rate,
                spec.settings.metadata == MetadataPolicy::Preserve,
            )?);
            audio_mapping[input_index] = Some(audio_index);
        } else if let Some(action) = plan
            .subtitles
            .iter()
            .find(|subtitle| subtitle.input_index == input_index)
            .map(|subtitle| &subtitle.action)
            .filter(|action| **action != SubtitleStreamAction::Copy)
        {
            let subtitle_index = subtitle_transcoders.len();
            subtitle_transcoders.push(SubtitleTranscoder::new(
                &input_stream,
                output,
                action,
                spec.settings.metadata == MetadataPolicy::Preserve,
            )?);
            subtitle_mapping[input_index] = Some(subtitle_index);
        } else {
            let preserve_stream_metadata = spec.settings.metadata == MetadataPolicy::Preserve
                || input_stream.parameters().medium() == ffmpeg::media::Type::Attachment;
            add_copy_stream(&input_stream, output, preserve_stream_metadata)?;
        }
        mapping[input_index] = Some(output_index);
    }

    Ok(OutputSetup {
        transcoder: transcoder
            .ok_or_else(|| EngineError::InvalidMedia("no video stream found".to_owned()))?,
        audio_transcoders,
        audio_mapping,
        subtitle_transcoders,
        subtitle_mapping,
        mapping,
        input_time_bases,
    })
}

fn add_copy_stream(
    input: &ffmpeg::Stream<'_>,
    output: &mut ffmpeg::format::context::Output,
    preserve_metadata: bool,
) -> Result<(), EngineError> {
    let mut stream = output
        .add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))
        .map_err(ffmpeg_failure)?;
    stream.set_parameters(input.parameters());
    stream.set_time_base(input.time_base());
    if preserve_metadata {
        stream.set_metadata(input.metadata().to_owned());
    }
    unsafe {
        (*stream.parameters().as_mut_ptr()).codec_tag = 0;
        (*stream.as_mut_ptr()).disposition = input.disposition().bits();
    }
    Ok(())
}

impl VideoTranscoder {
    fn new(
        input: &ffmpeg::Stream<'_>,
        output: &mut ffmpeg::format::context::Output,
        output_index: usize,
        settings: &QueueSettings,
        target_frame_rate: Option<ffmpeg::Rational>,
        progress: ProgressMetadata,
        stabilization: Option<&StabilizationPlan>,
    ) -> Result<Self, EngineError> {
        let decoder = ffmpeg::codec::context::Context::from_parameters(input.parameters())
            .and_then(|context| context.decoder().video())
            .map_err(ffmpeg_failure)?;
        let codec =
            ffmpeg::encoder::find_by_name(settings.encoder.library_name()).ok_or_else(|| {
                EngineError::Unavailable(format!(
                    "encoder {} is not available",
                    settings.encoder.library_name()
                ))
            })?;
        let global_header = output
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);
        let needs_hvc1_tag = needs_hvc1_tag(settings.encoder, output.format().name());
        let output_format = output_pixel_format(
            codec.video().map_err(ffmpeg_failure)?,
            decoder.format(),
            settings,
        )?;
        let mut output_stream = output.add_stream(Some(codec)).map_err(ffmpeg_failure)?;
        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(ffmpeg_failure)?;
        let width = (decoder.width() + 1) & !1;
        let height = (decoder.height() + 1) & !1;
        let stream_frame_rate = input.rate();
        let nominal_frame_rate =
            nominal_frame_rate(target_frame_rate, decoder.frame_rate(), stream_frame_rate);
        let encoder_time_base = nominal_frame_rate.map_or_else(
            || input.time_base(),
            |rate| ffmpeg::Rational(rate.denominator(), rate.numerator()),
        );
        encoder.set_width(width);
        encoder.set_height(height);
        encoder.set_aspect_ratio(decoder.aspect_ratio());
        encoder.set_format(output_format);
        encoder.set_time_base(encoder_time_base);
        encoder.set_frame_rate(nominal_frame_rate);
        encoder.set_colorspace(decoder.color_space());
        encoder.set_color_range(effective_color_range(decoder.color_range()));
        encoder.set_color_primaries(decoder.color_primaries());
        encoder.set_color_transfer_characteristic(decoder.color_transfer_characteristic());
        if global_header {
            encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        }
        let encoder = encoder
            .open_with(encoder_options(settings))
            .map_err(ffmpeg_failure)?;
        output_stream.set_parameters(&encoder);
        if needs_hvc1_tag {
            unsafe {
                (*output_stream.parameters().as_mut_ptr()).codec_tag = u32::from_le_bytes(*b"hvc1");
            }
        }
        output_stream.set_time_base(encoder_time_base);
        if let Some(frame_rate) = nominal_frame_rate {
            output_stream.set_rate(frame_rate);
            output_stream.set_avg_frame_rate(frame_rate);
        }
        if settings.metadata == MetadataPolicy::Preserve {
            output_stream.set_metadata(input.metadata().to_owned());
        }
        unsafe {
            (*output_stream.as_mut_ptr()).disposition = input.disposition().bits();
        }
        let filter = video_filter(
            &decoder,
            input.time_base(),
            output_format,
            settings,
            stabilization.map(StabilizationPlan::filter),
        )?;
        Ok(Self {
            decoder,
            encoder,
            filter,
            input_time_base: input.time_base(),
            encoder_time_base,
            target_frame_rate,
            progress,
            output_index,
            frames: 0,
            last_progress: None,
            last_preview: None,
            preview_renderer: None,
            first_target_pts: None,
            next_output_pts: 0,
            started_at: Instant::now(),
            two_pass_progress: stabilization.is_some_and(StabilizationPlan::is_two_pass),
        })
    }

    fn drain_frames(
        &mut self,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
        total: Option<Duration>,
        partial_path: &Path,
        control: &ConversionControl,
        emit: &mut dyn FnMut(ConversionEvent),
    ) -> Result<(), EngineError> {
        let mut decoded = ffmpeg::frame::Video::empty();
        loop {
            match self.decoder.receive_frame(&mut decoded) {
                Ok(()) => {
                    let timestamp = decoded.timestamp();
                    decoded.set_pts(timestamp);
                    self.emit_preview(&decoded, control, emit);
                    self.filter
                        .get("in")
                        .ok_or_else(|| {
                            EngineError::Failed("video filter input disappeared".to_owned())
                        })?
                        .source()
                        .add(&decoded)
                        .map_err(ffmpeg_failure)?;
                    self.drain_filtered(output, output_time_base)?;
                    self.emit_progress(timestamp, total, partial_path, emit);
                }
                Err(error) if is_again_or_eof(error) => break,
                Err(error) => return Err(ffmpeg_failure(error)),
            }
        }
        Ok(())
    }

    fn emit_preview(
        &mut self,
        frame: &ffmpeg::frame::Video,
        control: &ConversionControl,
        emit: &mut dyn FnMut(ConversionEvent),
    ) {
        if !control.preview_enabled() {
            return;
        }
        let now = Instant::now();
        if !preview_is_due(self.last_preview, now) {
            return;
        }
        if let Ok(preview) = preview_frame(frame, 480, 270, &mut self.preview_renderer) {
            emit(ConversionEvent::Preview(preview));
            self.last_preview = Some(now);
        }
    }

    fn finish_filter(
        &mut self,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
    ) -> Result<(), EngineError> {
        self.filter
            .get("in")
            .ok_or_else(|| EngineError::Failed("video filter input disappeared".to_owned()))?
            .source()
            .flush()
            .map_err(ffmpeg_failure)?;
        self.drain_filtered(output, output_time_base)
    }

    fn drain_filtered(
        &mut self,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
    ) -> Result<(), EngineError> {
        let mut converted = ffmpeg::frame::Video::empty();
        loop {
            let result = self
                .filter
                .get("out")
                .ok_or_else(|| EngineError::Failed("video filter output disappeared".to_owned()))?
                .sink()
                .frame(&mut converted);
            match result {
                Ok(()) => {
                    converted.set_kind(ffmpeg::picture::Type::None);
                    let timestamp = converted.timestamp();
                    self.send_converted_frames(&mut converted, timestamp)?;
                    self.drain_packets(output, output_time_base)?;
                }
                Err(error) if is_again_or_eof(error) => break,
                Err(error) => return Err(ffmpeg_failure(error)),
            }
        }
        Ok(())
    }

    fn drain_packets(
        &mut self,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
    ) -> Result<(), EngineError> {
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(self.output_index);
                    packet.rescale_ts(self.encoder_time_base, output_time_base);
                    packet.set_position(-1);
                    write_interleaved(&mut packet, output)?;
                }
                Err(error) if is_again_or_eof(error) => break,
                Err(error) => return Err(ffmpeg_failure(error)),
            }
        }
        Ok(())
    }

    fn send_converted_frames(
        &mut self,
        converted: &mut ffmpeg::frame::Video,
        source_timestamp: Option<i64>,
    ) -> Result<(), EngineError> {
        if self.target_frame_rate.is_none() {
            converted.set_pts(source_timestamp.map(|timestamp| {
                timestamp.rescale_with(
                    self.input_time_base,
                    self.encoder_time_base,
                    ffmpeg::Rounding::NearInfinity,
                )
            }));
            self.encoder.send_frame(converted).map_err(ffmpeg_failure)?;
            self.frames = self.frames.saturating_add(1);
            return Ok(());
        }

        let Some(source_timestamp) = source_timestamp else {
            return Ok(());
        };
        let target_pts = source_timestamp.rescale_with(
            self.input_time_base,
            self.encoder_time_base,
            ffmpeg::Rounding::Down,
        );
        let first_target_pts = *self.first_target_pts.get_or_insert(target_pts);
        let target_pts = target_pts.saturating_sub(first_target_pts).max(0);
        while self.next_output_pts <= target_pts {
            converted.set_pts(Some(self.next_output_pts));
            self.encoder.send_frame(converted).map_err(ffmpeg_failure)?;
            self.frames = self.frames.saturating_add(1);
            self.next_output_pts = self.next_output_pts.saturating_add(1);
        }
        Ok(())
    }

    fn emit_progress(
        &mut self,
        timestamp: Option<i64>,
        total: Option<Duration>,
        partial_path: &Path,
        emit: &mut dyn FnMut(ConversionEvent),
    ) {
        let Some(timestamp) = timestamp else {
            return;
        };
        let micros = timestamp.rescale(self.input_time_base, ffmpeg::Rational(1, 1_000_000));
        let media_time = Duration::from_micros(u64::try_from(micros.max(0)).unwrap_or(u64::MAX));
        let completed = media_time;
        if self
            .last_progress
            .is_some_and(|last| completed.saturating_sub(last) < Duration::from_millis(250))
        {
            return;
        }
        emit(ConversionEvent::Progress(ConversionProgress {
            overall: self
                .two_pass_progress
                .then(|| {
                    phase_progress(
                        self.progress,
                        self.frames,
                        media_time,
                        ProgressPhase::SecondHalf,
                    )
                })
                .flatten(),
            completed,
            total,
            frames: Some(self.frames),
            total_frames: self.progress.total_frames,
            target_fps: self.progress.target_fps,
            frames_per_second: u32::try_from(self.frames)
                .ok()
                .and_then(|frames| rate(f64::from(frames), self.started_at.elapsed())),
            speed: rate(media_time.as_secs_f64(), self.started_at.elapsed()),
            output_bytes: std::fs::metadata(partial_path)
                .ok()
                .map(|metadata| metadata.len()),
        }));
        self.last_progress = Some(completed);
    }
}

fn preview_frame(
    frame: &ffmpeg::frame::Video,
    maximum_width: u32,
    maximum_height: u32,
    renderer: &mut Option<PreviewRenderer>,
) -> Result<ConversionPreview, EngineError> {
    let (width, height) =
        fitted_dimensions(frame.width(), frame.height(), maximum_width, maximum_height);
    if renderer
        .as_ref()
        .is_none_or(|renderer| !renderer.matches(frame, width, height))
    {
        *renderer = Some(PreviewRenderer::new(frame, width, height)?);
    }
    renderer
        .as_mut()
        .ok_or_else(|| EngineError::Unavailable("preview renderer is unavailable".to_owned()))?
        .render(frame)
}

fn preview_is_due(last_preview: Option<Instant>, now: Instant) -> bool {
    last_preview.is_none_or(|last| now.saturating_duration_since(last) >= PREVIEW_INTERVAL)
}

fn fitted_dimensions(
    width: u32,
    height: u32,
    maximum_width: u32,
    maximum_height: u32,
) -> (u32, u32) {
    if width == 0 || height == 0 || maximum_width == 0 || maximum_height == 0 {
        return (1, 1);
    }
    let by_width = u64::from(maximum_width) * u64::from(height);
    let by_height = u64::from(maximum_height) * u64::from(width);
    if by_width <= by_height {
        (
            maximum_width,
            u32::try_from(u64::from(height) * u64::from(maximum_width) / u64::from(width))
                .unwrap_or(maximum_height)
                .max(1),
        )
    } else {
        (
            u32::try_from(u64::from(width) * u64::from(maximum_height) / u64::from(height))
                .unwrap_or(maximum_width)
                .max(1),
            maximum_height,
        )
    }
}

fn video_filter(
    decoder: &ffmpeg::decoder::Video,
    input_time_base: ffmpeg::Rational,
    output_format: ffmpeg::format::Pixel,
    settings: &QueueSettings,
    stabilization_filter: Option<&str>,
) -> Result<ffmpeg::filter::Graph, EngineError> {
    let mut filter = ffmpeg::filter::Graph::new();
    let aspect_ratio = decoder.aspect_ratio();
    let aspect_ratio = if aspect_ratio.numerator() > 0 && aspect_ratio.denominator() > 0 {
        aspect_ratio
    } else {
        ffmpeg::Rational(1, 1)
    };
    let arguments = format!(
        "video_size={}x{}:pix_fmt={}:time_base={input_time_base}:pixel_aspect={aspect_ratio}:colorspace={}:range={}",
        decoder.width(),
        decoder.height(),
        ffmpeg::ffi::AVPixelFormat::from(decoder.format()) as i32,
        ffmpeg::ffi::AVColorSpace::from(decoder.color_space()) as i32,
        ffmpeg::ffi::AVColorRange::from(effective_color_range(decoder.color_range())) as i32
    );
    filter
        .add(
            &ffmpeg::filter::find("buffer").ok_or_else(|| {
                EngineError::Unavailable("buffer filter is unavailable".to_owned())
            })?,
            "in",
            &arguments,
        )
        .map_err(ffmpeg_failure)?;
    filter
        .add(
            &ffmpeg::filter::find("buffersink").ok_or_else(|| {
                EngineError::Unavailable("buffersink filter is unavailable".to_owned())
            })?,
            "out",
            "",
        )
        .map_err(ffmpeg_failure)?;
    let output_format_name = output_format
        .descriptor()
        .ok_or_else(|| EngineError::Unsupported("unknown output pixel format".to_owned()))?
        .name();
    let chain = video_filter_chain(output_format_name, settings, stabilization_filter)?;
    filter
        .output("in", 0)
        .map_err(ffmpeg_failure)?
        .input("out", 0)
        .map_err(ffmpeg_failure)?
        .parse(&chain)
        .map_err(ffmpeg_failure)?;
    filter.validate().map_err(ffmpeg_failure)?;
    Ok(filter)
}

const fn effective_color_range(range: ffmpeg::color::Range) -> ffmpeg::color::Range {
    if matches!(range, ffmpeg::color::Range::Unspecified) {
        // FFmpeg's CLI encoders default ordinary YUV video to limited/MPEG
        // range. Make that default explicit because the direct API otherwise
        // leaves x264's bitstream VUI range unspecified.
        ffmpeg::color::Range::MPEG
    } else {
        range
    }
}

fn output_pixel_format(
    codec: ffmpeg::codec::Video,
    source: ffmpeg::format::Pixel,
    settings: &QueueSettings,
) -> Result<ffmpeg::format::Pixel, EngineError> {
    let preferred = if settings.camera_lut_path.is_some() {
        match settings.encoder {
            videoferry_core::Encoder::X265 => ffmpeg::format::Pixel::YUV420P10LE,
            _ => ffmpeg::format::Pixel::YUV420P,
        }
    } else {
        source
    };
    if codec
        .formats()
        .is_some_and(|mut formats| formats.any(|format| format == preferred))
    {
        return Ok(preferred);
    }
    if settings.camera_lut_path.is_some() {
        return Err(EngineError::Unsupported(format!(
            "encoder {} cannot accept the pixel format required by the DJI LUT",
            settings.encoder.library_name()
        )));
    }
    if codec
        .formats()
        .is_some_and(|mut formats| formats.any(|format| format == ffmpeg::format::Pixel::YUV420P))
    {
        Ok(ffmpeg::format::Pixel::YUV420P)
    } else {
        Err(EngineError::Unsupported(format!(
            "encoder {} has no supported software pixel format",
            settings.encoder.library_name()
        )))
    }
}

fn video_filter_chain(
    output_format_name: &str,
    settings: &QueueSettings,
    stabilization_filter: Option<&str>,
) -> Result<String, EngineError> {
    let mut filters = Vec::new();
    if let Some(stabilization_filter) = stabilization_filter {
        filters.push(stabilization_filter.to_owned());
    }
    if let Some(path) = &settings.camera_lut_path {
        if !path.is_file() {
            return Err(EngineError::Unavailable(format!(
                "DJI LUT does not exist: {}",
                path.display()
            )));
        }
        filters.push(format!("lut3d='{}'", ffmpeg_filter_path(path)));
        filters.push("eq=brightness=-0.02:saturation=0.90".to_owned());
    }
    filters.push(format!("format=pix_fmts={output_format_name}"));
    filters.push("pad=ceil(iw/2)*2:ceil(ih/2)*2".to_owned());
    Ok(filters.join(","))
}

fn ffmpeg_filter_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

fn encoder_options(settings: &QueueSettings) -> ffmpeg::Dictionary<'static> {
    let mut options = ffmpeg::Dictionary::new();
    if let Some(quality) = settings.quality {
        options.set("crf", &format!("{quality:.0}"));
    }
    if let Some(preset) = &settings.speed_preset {
        options.set("preset", preset);
    }
    if settings.mode == videoferry_core::ContentMode::Animation
        && matches!(
            settings.encoder,
            videoferry_core::Encoder::X264 | videoferry_core::Encoder::X265
        )
    {
        options.set("tune", "animation");
    }
    options
}

fn needs_hvc1_tag(encoder: videoferry_core::Encoder, output_format: &str) -> bool {
    encoder.is_hevc() && output_format.split(',').any(|name| name == "mp4")
}

fn selected_streams(plan: &StreamPlan) -> BTreeSet<usize> {
    plan.video_input_index
        .into_iter()
        .chain(plan.audio.iter().map(|stream| stream.input_index))
        .chain(plan.subtitles.iter().map(|stream| stream.input_index))
        .chain(plan.attachments.iter().copied())
        .collect()
}

fn is_again_or_eof(error: ffmpeg::Error) -> bool {
    error == ffmpeg::Error::Eof
        || error
            == ffmpeg::Error::Other {
                errno: ffmpeg::error::EAGAIN,
            }
}

fn ffmpeg_failure(error: ffmpeg::Error) -> EngineError {
    EngineError::Failed(error.to_string())
}

fn rate(value: f64, elapsed: Duration) -> Option<f64> {
    let elapsed = elapsed.as_secs_f64();
    (elapsed > 0.0).then_some(value / elapsed)
}

fn nominal_frame_rate(
    requested: Option<ffmpeg::Rational>,
    decoder: Option<ffmpeg::Rational>,
    stream: ffmpeg::Rational,
) -> Option<ffmpeg::Rational> {
    requested
        .or(decoder)
        .or_else(|| (stream.numerator() > 0 && stream.denominator() > 0).then_some(stream))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use videoferry_core::{Encoder, QueueSettings};

    use super::{
        effective_color_range, ffmpeg_filter_path, fitted_dimensions, needs_hvc1_tag,
        nominal_frame_rate, preview_is_due, video_filter_chain,
    };

    #[test]
    fn escapes_windows_paths_for_filter_graphs() {
        assert_eq!(
            ffmpeg_filter_path(Path::new("C:\\LUTs\\DJI's.cube")),
            "C\\:/LUTs/DJI\\'s.cube"
        );
    }

    #[test]
    fn stabilization_runs_before_format_and_padding() {
        let settings = QueueSettings::default();
        assert_eq!(
            video_filter_chain(
                "yuv420p",
                &settings,
                Some("vidstabtransform=input='transform.trf'"),
            )
            .unwrap(),
            "vidstabtransform=input='transform.trf',format=pix_fmts=yuv420p,pad=ceil(iw/2)*2:ceil(ih/2)*2"
        );
    }

    #[test]
    fn all_hevc_encoders_use_the_apple_compatible_mp4_tag() {
        for encoder in [Encoder::X265, Encoder::HevcNvenc, Encoder::HevcVideoToolbox] {
            assert!(needs_hvc1_tag(encoder, "mov,mp4,m4a,3gp,3g2,mj2"));
            assert!(!needs_hvc1_tag(encoder, "matroska,webm"));
        }
        assert!(!needs_hvc1_tag(Encoder::H264VideoToolbox, "mp4"));
    }

    #[test]
    fn preview_dimensions_fit_landscape_and_portrait_frames() {
        assert_eq!(fitted_dimensions(1920, 1080, 480, 270), (480, 270));
        assert_eq!(fitted_dimensions(1080, 1920, 480, 270), (151, 270));
        assert_eq!(fitted_dimensions(640, 480, 480, 270), (360, 270));
        assert_eq!(fitted_dimensions(0, 0, 480, 270), (1, 1));
    }

    #[test]
    fn preview_cadence_uses_wall_clock_time() {
        let now = Instant::now();
        assert!(preview_is_due(None, now));
        assert!(!preview_is_due(Some(now), now + Duration::from_millis(999)));
        assert!(preview_is_due(Some(now), now + Duration::from_secs(1)));
    }

    #[test]
    fn source_timing_falls_back_to_the_stream_frame_rate() {
        let stream_rate = ffmpeg_next::Rational(24, 1);
        assert_eq!(
            nominal_frame_rate(None, None, stream_rate),
            Some(stream_rate)
        );
        assert_eq!(
            nominal_frame_rate(
                Some(ffmpeg_next::Rational(30, 1)),
                Some(ffmpeg_next::Rational(25, 1)),
                stream_rate,
            ),
            Some(ffmpeg_next::Rational(30, 1))
        );
        assert_eq!(
            nominal_frame_rate(None, None, ffmpeg_next::Rational(0, 0)),
            None
        );
    }

    #[test]
    fn unspecified_yuv_range_matches_ffmpeg_cli_limited_range() {
        assert_eq!(
            effective_color_range(ffmpeg_next::color::Range::Unspecified),
            ffmpeg_next::color::Range::MPEG
        );
        assert_eq!(
            effective_color_range(ffmpeg_next::color::Range::JPEG),
            ffmpeg_next::color::Range::JPEG
        );
    }
}
