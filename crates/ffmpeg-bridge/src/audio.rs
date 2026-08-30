use ffmpeg_next as ffmpeg;
use ffmpeg_next::Rescale;
use videoferry_core::EngineError;

use crate::mux::write_interleaved;

pub(super) struct AudioTranscoder {
    output_index: usize,
    decoder: ffmpeg::decoder::Audio,
    encoder: ffmpeg::encoder::Audio,
    filter: ffmpeg::filter::Graph,
    encoder_time_base: ffmpeg::Rational,
}

trait EngineResultExt<T> {
    fn engine(self) -> Result<T, EngineError>;
}

impl<T> EngineResultExt<T> for Result<T, ffmpeg::Error> {
    fn engine(self) -> Result<T, EngineError> {
        self.map_err(|error| EngineError::Failed(error.to_string()))
    }
}

impl AudioTranscoder {
    pub(super) fn new(
        input: &ffmpeg::Stream<'_>,
        output: &mut ffmpeg::format::context::Output,
        bit_rate: u64,
        preserve_metadata: bool,
    ) -> Result<Self, EngineError> {
        let mut decoder = ffmpeg::codec::context::Context::from_parameters(input.parameters())
            .engine()?
            .decoder()
            .audio()
            .engine()?;
        decoder.set_packet_time_base(input.time_base());
        let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::AC3)
            .ok_or_else(|| EngineError::Unavailable("AC-3 encoder is not available".to_owned()))?;
        let audio_codec = codec.audio().engine()?;
        let sample_rate = select_sample_rate(audio_codec, decoder.rate());
        let channel_layout = audio_codec.channel_layouts().map_or_else(
            || decoder.channel_layout(),
            |layouts| layouts.best(i32::from(decoder.channels())),
        );
        let sample_format = audio_codec
            .formats()
            .and_then(|mut formats| formats.next())
            .ok_or_else(|| EngineError::Unavailable("AC-3 sample format is unknown".to_owned()))?;
        let encoder_time_base = ffmpeg::Rational(
            1,
            i32::try_from(sample_rate).map_err(|_| {
                EngineError::Unsupported("audio sample rate is too high".to_owned())
            })?,
        );
        let global_header = output
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);
        let mut output_stream = output.add_stream(Some(codec)).engine()?;
        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .engine()?;
        encoder.set_rate(encoder_time_base.denominator());
        encoder.set_channel_layout(channel_layout);
        encoder.set_format(sample_format);
        encoder.set_bit_rate(
            usize::try_from(bit_rate)
                .map_err(|_| EngineError::Unsupported("audio bit rate is too high".to_owned()))?,
        );
        encoder.set_time_base(encoder_time_base);
        if global_header {
            encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        }
        let encoder = encoder.open_as(codec).engine()?;
        output_stream.set_parameters(&encoder);
        output_stream.set_time_base(encoder_time_base);
        if preserve_metadata {
            output_stream.set_metadata(input.metadata().to_owned());
        }
        unsafe {
            (*output_stream.parameters().as_mut_ptr()).codec_tag = 0;
            (*output_stream.as_mut_ptr()).disposition = input.disposition().bits();
        }
        let filter = audio_filter(&decoder, &encoder, input.time_base())?;

        Ok(Self {
            output_index: output_stream.index(),
            decoder,
            encoder,
            filter,
            encoder_time_base,
        })
    }

    pub(super) fn output_index(&self) -> usize {
        self.output_index
    }

    pub(super) fn process_packet(
        &mut self,
        packet: &ffmpeg::Packet,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
    ) -> Result<(), EngineError> {
        self.decoder
            .send_packet(packet)
            .map_err(|error| EngineError::Failed(format!("sending an audio packet: {error}")))?;
        self.drain_decoder(output, output_time_base)
    }

    pub(super) fn finish(
        &mut self,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
    ) -> Result<(), EngineError> {
        self.decoder.send_eof().map_err(|error| {
            EngineError::Failed(format!("finishing the audio decoder: {error}"))
        })?;
        self.drain_decoder(output, output_time_base)?;
        self.filter
            .get("in")
            .ok_or_else(|| EngineError::Failed("audio filter input disappeared".to_owned()))?
            .source()
            .flush()
            .engine()?;
        self.drain_filter(output, output_time_base)?;
        self.encoder.send_eof().map_err(|error| {
            EngineError::Failed(format!("finishing the audio encoder: {error}"))
        })?;
        self.drain_encoder(output, output_time_base)
    }

    fn drain_decoder(
        &mut self,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
    ) -> Result<(), EngineError> {
        let mut frame = ffmpeg::frame::Audio::empty();
        loop {
            match self.decoder.receive_frame(&mut frame) {
                Ok(()) => {
                    let timestamp = frame.timestamp();
                    frame.set_pts(timestamp);
                    self.filter
                        .get("in")
                        .ok_or_else(|| {
                            EngineError::Failed("audio filter input disappeared".to_owned())
                        })?
                        .source()
                        .add(&frame)
                        .engine()?;
                    self.drain_filter(output, output_time_base)?;
                }
                Err(error) if is_again_or_eof(error) => break,
                Err(error) => return Err(EngineError::Failed(error.to_string())),
            }
        }
        Ok(())
    }

    fn drain_filter(
        &mut self,
        output: &mut ffmpeg::format::context::Output,
        output_time_base: ffmpeg::Rational,
    ) -> Result<(), EngineError> {
        let mut frame = ffmpeg::frame::Audio::empty();
        loop {
            let mut output_context = self
                .filter
                .get("out")
                .ok_or_else(|| EngineError::Failed("audio filter output disappeared".to_owned()))?;
            let mut sink = output_context.sink();
            let filter_time_base = sink.time_base();
            let result = sink.frame(&mut frame);
            match result {
                Ok(()) => {
                    let timestamp = frame
                        .timestamp()
                        .map(|pts| pts.rescale(filter_time_base, self.encoder_time_base));
                    frame.set_pts(timestamp);
                    self.encoder.send_frame(&frame).engine()?;
                    self.drain_encoder(output, output_time_base)?;
                }
                Err(error) if is_again_or_eof(error) => break,
                Err(error) => return Err(EngineError::Failed(error.to_string())),
            }
        }
        Ok(())
    }

    fn drain_encoder(
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
                Err(error) => return Err(EngineError::Failed(error.to_string())),
            }
        }
        Ok(())
    }
}

fn audio_filter(
    decoder: &ffmpeg::decoder::Audio,
    encoder: &ffmpeg::encoder::Audio,
    input_time_base: ffmpeg::Rational,
) -> Result<ffmpeg::filter::Graph, EngineError> {
    let mut filter = ffmpeg::filter::Graph::new();
    let arguments = format!(
        "time_base={input_time_base}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
        decoder.rate(),
        decoder.format().name(),
        decoder.channel_layout().bits()
    );
    filter
        .add(
            &ffmpeg::filter::find("abuffer").ok_or_else(|| {
                EngineError::Unavailable("abuffer filter is unavailable".to_owned())
            })?,
            "in",
            &arguments,
        )
        .engine()?;
    filter
        .add(
            &ffmpeg::filter::find("abuffersink").ok_or_else(|| {
                EngineError::Unavailable("abuffersink filter is unavailable".to_owned())
            })?,
            "out",
            "",
        )
        .engine()?;
    let conversion = format!(
        "aformat=sample_fmts={}:sample_rates={}:channel_layouts=0x{:x}",
        encoder.format().name(),
        encoder.rate(),
        encoder.channel_layout().bits()
    );
    filter
        .output("in", 0)
        .engine()?
        .input("out", 0)
        .engine()?
        .parse(&conversion)
        .engine()?;
    filter.validate().engine()?;
    if !encoder.codec().is_some_and(|codec| {
        codec
            .capabilities()
            .contains(ffmpeg::codec::capabilities::Capabilities::VARIABLE_FRAME_SIZE)
    }) {
        filter
            .get("out")
            .ok_or_else(|| EngineError::Failed("audio filter output disappeared".to_owned()))?
            .sink()
            .set_frame_size(encoder.frame_size());
    }
    Ok(filter)
}

fn select_sample_rate(codec: ffmpeg::codec::Audio, source: u32) -> u32 {
    codec.rates().map_or(source, |rates| {
        rates
            .filter_map(|rate| u32::try_from(rate).ok())
            .min_by_key(|rate| rate.abs_diff(source))
            .unwrap_or(source)
    })
}

fn is_again_or_eof(error: ffmpeg::Error) -> bool {
    error == ffmpeg::Error::Eof
        || error
            == ffmpeg::Error::Other {
                errno: ffmpeg::error::EAGAIN,
            }
}
