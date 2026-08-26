#![forbid(unsafe_code)]

mod control;
mod engine;
mod model;
mod output_naming;
mod queue;
mod stream_policy;

pub use control::{ControlDecision, ConversionControl};
pub use engine::{EngineError, MediaEngine};
pub use model::{
    AudioPolicy, ColorCharacteristics, Container, ContentMode, ConversionEvent, ConversionPreview,
    ConversionProgress, ConversionRequest, Encoder, EncoderCapabilities, FpsPolicy, MediaInfo,
    MediaStream, MetadataPolicy, ProgressRatio, QueueSettings, StreamKind,
};
pub use output_naming::{conversion_output_path, stabilized_output_path, trim_output_path};
pub use queue::{Queue, QueueError, QueueStatus, QueueTask};
pub use stream_policy::{
    AudioStreamAction, PlannedAudioStream, PlannedSubtitleStream, SkippedStream, StreamPlan,
    StreamSkipReason, SubtitleStreamAction, build_stream_plan, build_stream_plan_with_tolerance,
};
