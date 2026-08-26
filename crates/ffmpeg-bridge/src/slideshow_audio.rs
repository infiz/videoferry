use std::path::{Path, PathBuf};
use std::time::Duration;

use ffmpeg_next as ffmpeg;
use videoferry_core::{ControlDecision, ConversionControl, EngineError};

use crate::mux::write_interleaved;

const SAMPLE_RATE: u32 = 48_000;
const AUDIO_BIT_RATE: usize = 192_000;
const TRACK_PADDING: Duration = Duration::from_millis(1_500);

pub(super) struct AudioProgram {
    left: Vec<f32>,
    right: Vec<f32>,
    target_samples: usize,
    fade_start: usize,
    fade_samples: usize,
}

pub(super) struct AudioEncoder {
    encoder: ffmpeg::encoder::Audio,
    output_index: usize,
    time_base: ffmpeg::Rational,
}

pub(super) fn prepare(
    paths: &[PathBuf],
    duration: Duration,
    control: &ConversionControl,
) -> Result<Option<AudioProgram>, EngineError> {
    let paths = paths
        .iter()
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(None);
    }
    let mut left = Vec::new();
    let mut right = Vec::new();
    let padding = duration_samples(TRACK_PADDING)?;
    for path in paths {
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        let (track_left, track_right) = decode_track(path)?;
        if track_left.is_empty() {
            return Err(EngineError::InvalidMedia(format!(
                "audio track contains no samples: {}",
                path.display()
            )));
        }
        left.extend(track_left);
        right.extend(track_right);
        left.resize(left.len().saturating_add(padding), 0.0);
        right.resize(right.len().saturating_add(padding), 0.0);
    }
    let target_samples = duration_samples(duration)?;
    let fade_duration = Duration::from_secs_f64((duration.as_secs_f64() / 2.0).clamp(0.1, 5.0));
    let fade_samples = duration_samples(fade_duration)?.min(target_samples);
    Ok(Some(AudioProgram {
        left,
        right,
        target_samples,
        fade_start: target_samples.saturating_sub(fade_samples),
        fade_samples,
    }))
}

impl AudioEncoder {
    pub(super) fn new(output: &mut ffmpeg::format::context::Output) -> Result<Self, EngineError> {
        let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::AAC)
            .ok_or_else(|| EngineError::Unavailable("AAC encoder is unavailable".to_owned()))?;
        let audio_codec = codec.audio().map_err(ffmpeg_failure)?;
        let sample_format = ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar);
        if audio_codec
            .formats()
            .is_none_or(|mut formats| !formats.any(|format| format == sample_format))
        {
            return Err(EngineError::Unavailable(
                "AAC encoder does not support planar float samples".to_owned(),
            ));
        }
        let rate = i32::try_from(SAMPLE_RATE).map_err(integer_failure)?;
        if audio_codec
            .rates()
            .is_some_and(|mut rates| !rates.any(|candidate| candidate == rate))
        {
            return Err(EngineError::Unavailable(
                "AAC encoder does not support 48 kHz".to_owned(),
            ));
        }
        let time_base = ffmpeg::Rational(1, rate);
        let global_header = output
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);
        let mut context = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(ffmpeg_failure)?;
        context.set_rate(rate);
        context.set_channel_layout(ffmpeg::ChannelLayout::STEREO);
        context.set_format(sample_format);
        context.set_bit_rate(AUDIO_BIT_RATE);
        context.set_time_base(time_base);
        if global_header {
            context.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        }
        let encoder = context.open_as(codec).map_err(ffmpeg_failure)?;
        let output_index = output.nb_streams() as usize;
        {
            let mut stream = output.add_stream(Some(codec)).map_err(ffmpeg_failure)?;
            stream.set_parameters(&encoder);
            stream.set_time_base(time_base);
            unsafe {
                (*stream.parameters().as_mut_ptr()).codec_tag = 0;
            }
        }
        Ok(Self {
            encoder,
            output_index,
            time_base,
        })
    }

    pub(super) const fn output_index(&self) -> usize {
        self.output_index
    }

    pub(super) fn encode(
        &mut self,
        program: &AudioProgram,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
        control: &ConversionControl,
    ) -> Result<(), EngineError> {
        let frame_size = usize::try_from(self.encoder.frame_size())
            .map_err(integer_failure)?
            .max(1);
        let mut offset = 0_usize;
        while offset < program.target_samples {
            if control.checkpoint() != ControlDecision::Continue {
                return Err(EngineError::Cancelled);
            }
            let samples = frame_size.min(program.target_samples - offset);
            let mut frame = ffmpeg::frame::Audio::new(
                self.encoder.format(),
                samples,
                ffmpeg::ChannelLayout::STEREO,
            );
            frame.set_rate(SAMPLE_RATE);
            frame.set_pts(Some(i64::try_from(offset).map_err(integer_failure)?));
            fill_frame(&mut frame, program, offset);
            self.encoder.send_frame(&frame).map_err(ffmpeg_failure)?;
            self.drain(output, output_time_base)?;
            offset = offset.saturating_add(samples);
        }
        self.encoder.send_eof().map_err(ffmpeg_failure)?;
        self.drain(output, output_time_base)
    }

    fn drain(
        &mut self,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
    ) -> Result<(), EngineError> {
        let mut packet = ffmpeg::Packet::empty();
        loop {
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    packet.set_stream(self.output_index);
                    packet.rescale_ts(self.time_base, output_time_base);
                    packet.set_position(-1);
                    write_interleaved(&mut packet, output)?;
                }
                Err(error) if is_again_or_eof(error) => return Ok(()),
                Err(error) => return Err(ffmpeg_failure(error)),
            }
        }
    }
}

fn fill_frame(frame: &mut ffmpeg::frame::Audio, program: &AudioProgram, offset: usize) {
    for channel in 0..2 {
        let destination = frame.plane_mut::<f32>(channel);
        for (relative, value) in destination.iter_mut().enumerate() {
            let absolute = offset.saturating_add(relative);
            *value = program_sample(program, channel, absolute);
        }
    }
}

fn program_sample(program: &AudioProgram, channel: usize, absolute: usize) -> f32 {
    let source = if channel == 0 {
        &program.left
    } else {
        &program.right
    };
    let sequence_index = absolute % source.len();
    let gain = if absolute < program.fade_start || program.fade_samples == 0 {
        1.0
    } else {
        let remaining = program.target_samples.saturating_sub(absolute);
        let units = remaining.saturating_mul(usize::from(u16::MAX)) / program.fade_samples;
        f32::from(u16::try_from(units).unwrap_or(u16::MAX)) / f32::from(u16::MAX)
    };
    source[sequence_index] * gain.clamp(0.0, 1.0)
}

fn decode_track(path: &Path) -> Result<(Vec<f32>, Vec<f32>), EngineError> {
    let mut input = ffmpeg::format::input(path)
        .map_err(|error| EngineError::InvalidMedia(format!("{}: {error}", path.display())))?;
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Audio)
        .ok_or_else(|| EngineError::InvalidMedia(format!("no audio stream: {}", path.display())))?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .and_then(|context| context.decoder().audio())
        .map_err(ffmpeg_failure)?;
    decoder.set_packet_time_base(time_base);
    if decoder.channel_layout().is_empty() {
        decoder.set_channel_layout(ffmpeg::ChannelLayout::default(i32::from(
            decoder.channels(),
        )));
    }
    let mut graph = audio_filter(&decoder, time_base)?;
    let mut left = Vec::new();
    let mut right = Vec::new();
    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(ffmpeg_failure)?;
        drain_decoder(&mut decoder, &mut graph, &mut left, &mut right)?;
    }
    decoder.send_eof().map_err(ffmpeg_failure)?;
    drain_decoder(&mut decoder, &mut graph, &mut left, &mut right)?;
    graph
        .get("in")
        .ok_or_else(|| EngineError::Failed("slideshow audio input disappeared".to_owned()))?
        .source()
        .flush()
        .map_err(ffmpeg_failure)?;
    drain_filter(&mut graph, &mut left, &mut right)?;
    Ok((left, right))
}

fn drain_decoder(
    decoder: &mut ffmpeg::decoder::Audio,
    graph: &mut ffmpeg::filter::Graph,
    left: &mut Vec<f32>,
    right: &mut Vec<f32>,
) -> Result<(), EngineError> {
    let mut frame = ffmpeg::frame::Audio::empty();
    loop {
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                graph
                    .get("in")
                    .ok_or_else(|| {
                        EngineError::Failed("slideshow audio input disappeared".to_owned())
                    })?
                    .source()
                    .add(&frame)
                    .map_err(ffmpeg_failure)?;
                drain_filter(graph, left, right)?;
            }
            Err(error) if is_again_or_eof(error) => return Ok(()),
            Err(error) => return Err(ffmpeg_failure(error)),
        }
    }
}

fn drain_filter(
    graph: &mut ffmpeg::filter::Graph,
    left: &mut Vec<f32>,
    right: &mut Vec<f32>,
) -> Result<(), EngineError> {
    let mut frame = ffmpeg::frame::Audio::empty();
    loop {
        let result = graph
            .get("out")
            .ok_or_else(|| EngineError::Failed("slideshow audio output disappeared".to_owned()))?
            .sink()
            .frame(&mut frame);
        match result {
            Ok(()) => {
                left.extend_from_slice(frame.plane::<f32>(0));
                right.extend_from_slice(frame.plane::<f32>(1));
            }
            Err(error) if is_again_or_eof(error) => return Ok(()),
            Err(error) => return Err(ffmpeg_failure(error)),
        }
    }
}

fn audio_filter(
    decoder: &ffmpeg::decoder::Audio,
    time_base: ffmpeg::Rational,
) -> Result<ffmpeg::filter::Graph, EngineError> {
    let mut graph = ffmpeg::filter::Graph::new();
    let arguments = format!(
        "time_base={time_base}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        decoder.rate(),
        decoder.format().name(),
        decoder.channel_layout().bits()
    );
    graph
        .add(
            &ffmpeg::filter::find("abuffer").ok_or_else(|| {
                EngineError::Unavailable("abuffer filter is unavailable".to_owned())
            })?,
            "in",
            &arguments,
        )
        .map_err(ffmpeg_failure)?;
    graph
        .add(
            &ffmpeg::filter::find("abuffersink").ok_or_else(|| {
                EngineError::Unavailable("abuffersink filter is unavailable".to_owned())
            })?,
            "out",
            "",
        )
        .map_err(ffmpeg_failure)?;
    let conversion =
        format!("aformat=sample_fmts=fltp:sample_rates={SAMPLE_RATE}:channel_layouts=stereo");
    graph
        .output("in", 0)
        .map_err(ffmpeg_failure)?
        .input("out", 0)
        .map_err(ffmpeg_failure)?
        .parse(&conversion)
        .map_err(ffmpeg_failure)?;
    graph.validate().map_err(ffmpeg_failure)?;
    Ok(graph)
}

fn duration_samples(duration: Duration) -> Result<usize, EngineError> {
    let samples = duration
        .as_nanos()
        .saturating_mul(u128::from(SAMPLE_RATE))
        .saturating_add(500_000_000)
        / 1_000_000_000;
    usize::try_from(samples).map_err(integer_failure)
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

fn integer_failure(error: std::num::TryFromIntError) -> EngineError {
    EngineError::Unsupported(format!("numeric audio setting is out of range: {error}"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{AudioProgram, duration_samples, program_sample};

    #[test]
    fn converts_python_audio_padding_to_samples() {
        assert_eq!(
            duration_samples(Duration::from_millis(1_500)).unwrap(),
            72_000
        );
    }

    #[test]
    fn loops_the_sequence_and_fades_the_tail() {
        let program = AudioProgram {
            left: vec![1.0, 0.5],
            right: vec![0.25, 0.75],
            target_samples: 6,
            fade_start: 4,
            fade_samples: 2,
        };
        assert!((program_sample(&program, 0, 0) - 1.0).abs() < f32::EPSILON);
        assert!((program_sample(&program, 0, 2) - 1.0).abs() < f32::EPSILON);
        assert!((program_sample(&program, 1, 3) - 0.75).abs() < f32::EPSILON);
        assert!((program_sample(&program, 0, 4) - 1.0).abs() < f32::EPSILON);
        assert!((program_sample(&program, 0, 5) - 0.25).abs() < 0.001);
    }
}
