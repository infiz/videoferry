use ffmpeg_next as ffmpeg;
use ffmpeg_next::Rescale;
use videoferry_core::{EngineError, SubtitleStreamAction};

use crate::mux::write_interleaved;

const ENCODE_BUFFER_SIZE: usize = 1_048_576;

pub(super) struct SubtitleTranscoder {
    output_index: usize,
    decoder: ffmpeg::decoder::Subtitle,
    encoder: ffmpeg::encoder::Subtitle,
    input_time_base: ffmpeg::Rational,
}

struct DecodedSubtitle(ffmpeg::Subtitle);

impl Drop for DecodedSubtitle {
    fn drop(&mut self) {
        unsafe { ffmpeg::ffi::avsubtitle_free(self.0.as_mut_ptr()) };
    }
}

impl SubtitleTranscoder {
    pub(super) fn new(
        input: &ffmpeg::Stream<'_>,
        output: &mut ffmpeg::format::context::Output,
        action: &SubtitleStreamAction,
        preserve_metadata: bool,
    ) -> Result<Self, EngineError> {
        let codec_id = match *action {
            SubtitleStreamAction::TranscodeSrt => ffmpeg::codec::Id::SUBRIP,
            SubtitleStreamAction::TranscodeMovText => ffmpeg::codec::Id::MOV_TEXT,
            SubtitleStreamAction::Copy => {
                return Err(EngineError::Failed(
                    "copy subtitle was sent to the transcoder".to_owned(),
                ));
            }
        };
        let codec = ffmpeg::encoder::find(codec_id).ok_or_else(|| {
            EngineError::Unavailable(format!("{} encoder is unavailable", codec_id.name()))
        })?;
        let input_time_base = input.time_base();
        let mut decoder = ffmpeg::codec::context::Context::from_parameters(input.parameters())
            .and_then(|context| context.decoder().subtitle())
            .map_err(|error| ffmpeg_failure_at("opening subtitle decoder", error))?;
        decoder.set_packet_time_base(input_time_base);
        let global_header = output
            .format()
            .flags()
            .contains(ffmpeg::format::Flags::GLOBAL_HEADER);
        let mut output_stream = output
            .add_stream(Some(codec))
            .map_err(|error| ffmpeg_failure_at("adding subtitle output stream", error))?;
        let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .subtitle()
            .map_err(|error| ffmpeg_failure_at("creating subtitle encoder", error))?;
        encoder.set_time_base(input_time_base);
        copy_subtitle_header(&decoder, &mut encoder)?;
        if global_header {
            encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
        }
        let encoder = encoder
            .open_as(codec)
            .map_err(|error| ffmpeg_failure_at("opening subtitle encoder", error))?;
        output_stream.set_parameters(&encoder);
        output_stream.set_time_base(input_time_base);
        if preserve_metadata {
            output_stream.set_metadata(input.metadata().to_owned());
        }
        unsafe {
            (*output_stream.parameters().as_mut_ptr()).codec_tag = 0;
            (*output_stream.as_mut_ptr()).disposition = input.disposition().bits();
        }

        Ok(Self {
            output_index: output_stream.index(),
            decoder,
            encoder,
            input_time_base,
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
        let mut subtitle = DecodedSubtitle(ffmpeg::Subtitle::new());
        if !self
            .decoder
            .decode(packet, &mut subtitle.0)
            .map_err(ffmpeg_failure)?
        {
            return Ok(());
        }
        let mut buffer = vec![0_u8; ENCODE_BUFFER_SIZE];
        let buffer_size = i32::try_from(buffer.len())
            .map_err(|_| EngineError::Failed("subtitle buffer is too large".to_owned()))?;
        let encoded_size = unsafe {
            ffmpeg::ffi::avcodec_encode_subtitle(
                self.encoder.as_mut_ptr(),
                buffer.as_mut_ptr(),
                buffer_size,
                subtitle.0.as_ptr(),
            )
        };
        if encoded_size < 0 {
            return Err(ffmpeg_failure(ffmpeg::Error::from(encoded_size)));
        }
        if encoded_size == 0 {
            return Ok(());
        }
        let encoded_size = usize::try_from(encoded_size)
            .map_err(|_| EngineError::Failed("invalid subtitle packet size".to_owned()))?;
        let mut encoded = ffmpeg::Packet::copy(&buffer[..encoded_size]);
        let pts = packet.pts().or_else(|| {
            subtitle
                .0
                .pts()
                .map(|value| value.rescale(ffmpeg::Rational(1, 1_000_000), self.input_time_base))
        });
        encoded.set_pts(pts);
        encoded.set_dts(pts);
        encoded.set_duration(subtitle_duration(packet, &subtitle.0, self.input_time_base));
        encoded.set_stream(self.output_index);
        encoded.rescale_ts(self.input_time_base, output_time_base);
        encoded.set_position(-1);
        write_interleaved(&mut encoded, output)
    }
}

fn subtitle_duration(
    packet: &ffmpeg::Packet,
    subtitle: &ffmpeg::Subtitle,
    time_base: ffmpeg::Rational,
) -> i64 {
    if packet.duration() > 0 {
        packet.duration()
    } else {
        i64::from(subtitle.end().saturating_sub(subtitle.start()))
            .rescale(ffmpeg::Rational(1, 1_000), time_base)
    }
}

fn ffmpeg_failure(error: ffmpeg::Error) -> EngineError {
    EngineError::Failed(error.to_string())
}

fn ffmpeg_failure_at(operation: &str, error: ffmpeg::Error) -> EngineError {
    EngineError::Failed(format!("{operation}: {error}"))
}

fn copy_subtitle_header(
    decoder: &ffmpeg::decoder::Subtitle,
    encoder: &mut ffmpeg::encoder::subtitle::Subtitle,
) -> Result<(), EngineError> {
    unsafe {
        let source = decoder.as_ptr();
        let size = (*source).subtitle_header_size;
        if size <= 0 || (*source).subtitle_header.is_null() {
            return Err(EngineError::InvalidMedia(
                "subtitle decoder did not provide the ASS header required for text conversion"
                    .to_owned(),
            ));
        }
        let allocation_size = usize::try_from(size)
            .ok()
            .and_then(|size| size.checked_add(ffmpeg::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize))
            .ok_or_else(|| EngineError::Failed("subtitle header is too large".to_owned()))?;
        let destination = ffmpeg::ffi::av_mallocz(allocation_size);
        if destination.is_null() {
            return Err(EngineError::Failed(
                "could not allocate subtitle header".to_owned(),
            ));
        }
        std::ptr::copy_nonoverlapping(
            (*source).subtitle_header,
            destination.cast::<u8>(),
            usize::try_from(size)
                .map_err(|_| EngineError::Failed("invalid subtitle header size".to_owned()))?,
        );
        let target = encoder.as_mut_ptr();
        (*target).subtitle_header = destination.cast::<u8>();
        (*target).subtitle_header_size = size;
    }
    Ok(())
}
