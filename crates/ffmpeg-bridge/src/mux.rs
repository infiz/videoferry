use ffmpeg_next as ffmpeg;
use videoferry_core::EngineError;

#[cfg(feature = "test-fault-injection")]
use std::sync::OnceLock;
#[cfg(feature = "test-fault-injection")]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(feature = "test-fault-injection")]
const DISK_FULL_AFTER_WRITES_ENV: &str = "VIDEOFERRY_TEST_DISK_FULL_AFTER_MUX_WRITES";

#[cfg(feature = "test-fault-injection")]
enum WriteFault {
    Disabled,
    Invalid(String),
    Remaining(AtomicU64),
}

#[cfg(feature = "test-fault-injection")]
static WRITE_FAULT: OnceLock<WriteFault> = OnceLock::new();

pub(super) fn write_interleaved(
    packet: &mut ffmpeg::Packet,
    output: &mut ffmpeg::format::context::Output,
) -> Result<(), EngineError> {
    #[cfg(feature = "test-fault-injection")]
    maybe_fail_for_disk_full()?;
    packet
        .write_interleaved(output)
        .map_err(|error| EngineError::Failed(error.to_string()))
}

/// Supplies the conventional layout for copied audio that reports only a
/// channel count. Some camera MP4 files legitimately store stereo PCM this
/// way, but `FFmpeg`'s MP4 muxer requires a named layout when creating a new
/// file. This changes stream metadata only; the audio packets remain copied.
pub(super) fn normalize_copied_audio_channel_layout(stream: &mut ffmpeg::StreamMut<'_>) {
    unsafe {
        let parameters = &mut *stream.parameters().as_mut_ptr();
        if parameters.codec_type == ffmpeg::ffi::AVMediaType::AVMEDIA_TYPE_AUDIO
            && parameters.ch_layout.order == ffmpeg::ffi::AVChannelOrder::AV_CHANNEL_ORDER_UNSPEC
            && parameters.ch_layout.nb_channels > 0
        {
            ffmpeg::ffi::av_channel_layout_default(
                &raw mut parameters.ch_layout,
                parameters.ch_layout.nb_channels,
            );
        }
    }
}

#[cfg(feature = "test-fault-injection")]
fn maybe_fail_for_disk_full() -> Result<(), EngineError> {
    let fault = WRITE_FAULT.get_or_init(|| match std::env::var(DISK_FULL_AFTER_WRITES_ENV) {
        Err(std::env::VarError::NotPresent) => WriteFault::Disabled,
        Err(error) => WriteFault::Invalid(error.to_string()),
        Ok(value) => match value.parse::<u64>() {
            Ok(writes) => WriteFault::Remaining(AtomicU64::new(writes)),
            Err(error) => WriteFault::Invalid(error.to_string()),
        },
    });
    match fault {
        WriteFault::Disabled => Ok(()),
        WriteFault::Invalid(error) => Err(EngineError::Failed(format!(
            "invalid {DISK_FULL_AFTER_WRITES_ENV}: {error}"
        ))),
        WriteFault::Remaining(remaining) => remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |writes| {
                writes.checked_sub(1)
            })
            .map(|_| ())
            .map_err(|_| {
                EngineError::Failed(
                    "No space left on device (simulated test-only mux failure)".to_owned(),
                )
            }),
    }
}

#[cfg(all(test, feature = "test-fault-injection"))]
mod tests {
    use super::DISK_FULL_AFTER_WRITES_ENV;

    #[test]
    fn fault_injection_environment_name_is_explicitly_test_only() {
        assert!(DISK_FULL_AFTER_WRITES_ENV.contains("_TEST_"));
    }
}
