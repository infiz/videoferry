use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ffmpeg::Rescale;
use ffmpeg_next as ffmpeg;
use videoferry_core::{
    ControlDecision, ConversionControl, ConversionEvent, ConversionProgress, EngineError,
};

use crate::progress::{ProgressMetadata, ProgressPhase, phase_progress};

static TRANSFORM_SEQUENCE: AtomicU64 = AtomicU64::new(1);

pub(super) struct StabilizationPlan {
    filter: String,
    transform: Option<TransformFile>,
}

struct TransformFile(PathBuf);

#[derive(Clone, Copy)]
struct AnalysisProgress {
    metadata: ProgressMetadata,
    started_at: Instant,
}

impl Drop for TransformFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl StabilizationPlan {
    pub(super) fn filter(&self) -> &str {
        &self.filter
    }

    pub(super) fn is_two_pass(&self) -> bool {
        self.transform.is_some()
    }
}

pub(super) fn prepare(
    input_path: &Path,
    video_index: usize,
    strength: &str,
    progress: ProgressMetadata,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<StabilizationPlan, EngineError> {
    if ffmpeg::filter::find("vidstabdetect").is_some()
        && ffmpeg::filter::find("vidstabtransform").is_some()
    {
        let transform = TransformFile(transform_path());
        emit(ConversionEvent::Warning(
            "Stabilization analysis pass started".to_owned(),
        ));
        run_detection(
            input_path,
            video_index,
            &transform.0,
            detection_options(strength),
            progress,
            control,
            emit,
        )?;
        if std::fs::metadata(&transform.0).map_or(true, |metadata| metadata.len() == 0) {
            return Err(EngineError::Failed(
                "vidstab analysis did not produce transform data".to_owned(),
            ));
        }
        return Ok(StabilizationPlan {
            filter: format!(
                "vidstabtransform=input='{}':{}",
                filter_path(&transform.0),
                transform_options(strength)
            ),
            transform: Some(transform),
        });
    }
    if ffmpeg::filter::find("deshake").is_some() {
        emit(ConversionEvent::Warning(
            "vidstab is unavailable; using the deshake fallback".to_owned(),
        ));
        return Ok(StabilizationPlan {
            filter: deshake_options(strength).to_owned(),
            transform: None,
        });
    }
    Err(EngineError::Unavailable(
        "stabilization requires vidstabdetect/vidstabtransform or deshake".to_owned(),
    ))
}

fn run_detection(
    input_path: &Path,
    video_index: usize,
    transform_path: &Path,
    options: &str,
    progress: ProgressMetadata,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<(), EngineError> {
    let mut input = ffmpeg::format::input(input_path).map_err(ffmpeg_failure)?;
    let stream = input
        .stream(video_index)
        .ok_or_else(|| EngineError::InvalidMedia("video stream disappeared".to_owned()))?;
    let input_time_base = stream.time_base();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .and_then(|context| context.decoder().video())
        .map_err(ffmpeg_failure)?;
    decoder.set_packet_time_base(input_time_base);
    let expression = format!(
        "vidstabdetect={options}:fileformat=ascii:result='{}'",
        filter_path(transform_path)
    );
    let mut graph = detection_graph(&decoder, input_time_base, &expression)?;
    let mut frames = 0_u64;
    let mut last_progress = None;
    let analysis_progress = AnalysisProgress {
        metadata: progress,
        started_at: Instant::now(),
    };

    for (stream, packet) in input.packets() {
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        if stream.index() != video_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(ffmpeg_failure)?;
        frames = frames.saturating_add(drain_decoder(
            &mut decoder,
            &mut graph,
            input_time_base,
            analysis_progress,
            frames,
            &mut last_progress,
            emit,
        )?);
    }
    decoder.send_eof().map_err(ffmpeg_failure)?;
    frames = frames.saturating_add(drain_decoder(
        &mut decoder,
        &mut graph,
        input_time_base,
        analysis_progress,
        frames,
        &mut last_progress,
        emit,
    )?);
    graph
        .get("in")
        .ok_or_else(|| EngineError::Failed("stabilization input disappeared".to_owned()))?
        .source()
        .flush()
        .map_err(ffmpeg_failure)?;
    drain_sink(&mut graph)?;
    if let Some(total) = progress.total {
        let (frames_per_second, speed) =
            analysis_rates(frames, total, analysis_progress.started_at.elapsed());
        emit(ConversionEvent::Progress(ConversionProgress {
            overall: phase_progress(progress, frames, total, ProgressPhase::FirstHalf),
            completed: total,
            total: Some(total),
            frames: Some(frames),
            total_frames: progress.total_frames,
            target_fps: progress.target_fps,
            frames_per_second,
            speed,
            output_bytes: None,
        }));
    }
    Ok(())
}

fn drain_decoder(
    decoder: &mut ffmpeg::decoder::Video,
    graph: &mut ffmpeg::filter::Graph,
    time_base: ffmpeg::Rational,
    analysis_progress: AnalysisProgress,
    prior_frames: u64,
    last_progress: &mut Option<Duration>,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<u64, EngineError> {
    let mut frame = ffmpeg::frame::Video::empty();
    let mut drained = 0_u64;
    loop {
        match decoder.receive_frame(&mut frame) {
            Ok(()) => {
                let timestamp = frame.timestamp();
                frame.set_pts(timestamp);
                graph
                    .get("in")
                    .ok_or_else(|| {
                        EngineError::Failed("stabilization input disappeared".to_owned())
                    })?
                    .source()
                    .add(&frame)
                    .map_err(ffmpeg_failure)?;
                drain_sink(graph)?;
                drained = drained.saturating_add(1);
                if let Some(timestamp) = timestamp {
                    emit_analysis_progress(
                        timestamp,
                        time_base,
                        analysis_progress,
                        prior_frames.saturating_add(drained),
                        last_progress,
                        emit,
                    );
                }
            }
            Err(error) if is_again_or_eof(error) => break,
            Err(error) => return Err(ffmpeg_failure(error)),
        }
    }
    Ok(drained)
}

fn drain_sink(graph: &mut ffmpeg::filter::Graph) -> Result<(), EngineError> {
    let mut frame = ffmpeg::frame::Video::empty();
    loop {
        let result = graph
            .get("out")
            .ok_or_else(|| EngineError::Failed("stabilization output disappeared".to_owned()))?
            .sink()
            .frame(&mut frame);
        match result {
            Ok(()) => {}
            Err(error) if is_again_or_eof(error) => return Ok(()),
            Err(error) => return Err(ffmpeg_failure(error)),
        }
    }
}

fn detection_graph(
    decoder: &ffmpeg::decoder::Video,
    time_base: ffmpeg::Rational,
    expression: &str,
) -> Result<ffmpeg::filter::Graph, EngineError> {
    let mut graph = ffmpeg::filter::Graph::new();
    let aspect = decoder.aspect_ratio();
    let aspect = if aspect.numerator() > 0 && aspect.denominator() > 0 {
        aspect
    } else {
        ffmpeg::Rational(1, 1)
    };
    let arguments = format!(
        "video_size={}x{}:pix_fmt={}:time_base={time_base}:pixel_aspect={aspect}:colorspace={}:range={}",
        decoder.width(),
        decoder.height(),
        ffmpeg::ffi::AVPixelFormat::from(decoder.format()) as i32,
        ffmpeg::ffi::AVColorSpace::from(decoder.color_space()) as i32,
        ffmpeg::ffi::AVColorRange::from(decoder.color_range()) as i32
    );
    graph
        .add(
            &ffmpeg::filter::find("buffer").ok_or_else(|| {
                EngineError::Unavailable("buffer filter is unavailable".to_owned())
            })?,
            "in",
            &arguments,
        )
        .map_err(ffmpeg_failure)?;
    graph
        .add(
            &ffmpeg::filter::find("buffersink").ok_or_else(|| {
                EngineError::Unavailable("buffersink filter is unavailable".to_owned())
            })?,
            "out",
            "",
        )
        .map_err(ffmpeg_failure)?;
    graph
        .output("in", 0)
        .map_err(ffmpeg_failure)?
        .input("out", 0)
        .map_err(ffmpeg_failure)?
        .parse(expression)
        .map_err(ffmpeg_failure)?;
    graph.validate().map_err(ffmpeg_failure)?;
    Ok(graph)
}

fn emit_analysis_progress(
    timestamp: i64,
    time_base: ffmpeg::Rational,
    analysis_progress: AnalysisProgress,
    frames: u64,
    last_progress: &mut Option<Duration>,
    emit: &mut dyn FnMut(ConversionEvent),
) {
    let micros = timestamp.rescale(time_base, ffmpeg::Rational(1, 1_000_000));
    let media_time = Duration::from_micros(u64::try_from(micros.max(0)).unwrap_or(u64::MAX));
    let completed = media_time;
    if last_progress.is_some_and(|last| completed.saturating_sub(last) < Duration::from_millis(250))
    {
        return;
    }
    let (frames_per_second, speed) =
        analysis_rates(frames, media_time, analysis_progress.started_at.elapsed());
    emit(ConversionEvent::Progress(ConversionProgress {
        overall: phase_progress(
            analysis_progress.metadata,
            frames,
            media_time,
            ProgressPhase::FirstHalf,
        ),
        completed,
        total: analysis_progress.metadata.total,
        frames: Some(frames),
        total_frames: analysis_progress.metadata.total_frames,
        target_fps: analysis_progress.metadata.target_fps,
        frames_per_second,
        speed,
        output_bytes: None,
    }));
    *last_progress = Some(completed);
}

fn analysis_rates(
    frames: u64,
    media_time: Duration,
    elapsed: Duration,
) -> (Option<f64>, Option<f64>) {
    let elapsed = elapsed.as_secs_f64();
    if elapsed <= 0.0 {
        return (None, None);
    }
    let frames_per_second = u32::try_from(frames)
        .ok()
        .map(|frames| f64::from(frames) / elapsed);
    let speed = Some(media_time.as_secs_f64() / elapsed);
    (frames_per_second, speed)
}

fn detection_options(strength: &str) -> &'static str {
    match strength {
        "Gentle" => "shakiness=4:accuracy=8",
        "Steady" => "shakiness=8:accuracy=12:stepsize=8:mincontrast=0.25",
        "Strong" => "shakiness=10:accuracy=15:stepsize=6:mincontrast=0.25",
        "Maximum" => "shakiness=10:accuracy=15:stepsize=4:mincontrast=0.2",
        _ => "shakiness=6:accuracy=10",
    }
}

fn transform_options(strength: &str) -> &'static str {
    match strength {
        "Gentle" => "smoothing=15:optzoom=1:zoomspeed=0.05:interpol=bicubic",
        "Steady" => "smoothing=35:optzoom=1:zoomspeed=0.05:interpol=bicubic",
        "Strong" => "smoothing=45:optzoom=1:zoomspeed=0.05:interpol=bicubic",
        "Maximum" => "smoothing=60:optzoom=1:zoomspeed=0.05:interpol=bicubic",
        _ => "smoothing=25:optzoom=1:zoomspeed=0.05:interpol=bicubic",
    }
}

fn deshake_options(strength: &str) -> &'static str {
    match strength {
        "Gentle" => "deshake=rx=16:ry=16:blocksize=16:edge=mirror",
        "Steady" => "deshake=rx=32:ry=32:blocksize=8:edge=mirror",
        "Strong" => "deshake=rx=48:ry=48:blocksize=8:edge=mirror",
        "Maximum" => "deshake=rx=64:ry=64:blocksize=8:edge=mirror",
        _ => "deshake=rx=16:ry=16:blocksize=8:edge=mirror",
    }
}

fn transform_path() -> PathBuf {
    let sequence = TRANSFORM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "videoferry-transform-{}-{sequence}.trf",
        std::process::id()
    ))
}

fn filter_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace(':', "\\:")
        .replace('\'', "\\'")
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{analysis_rates, deshake_options, detection_options, transform_options};

    #[test]
    fn analysis_progress_reports_python_compatible_fps_and_speed() {
        let (fps, speed) = analysis_rates(120, Duration::from_secs(4), Duration::from_secs(2));
        assert_eq!(fps, Some(60.0));
        assert_eq!(speed, Some(2.0));
        assert_eq!(
            analysis_rates(0, Duration::ZERO, Duration::ZERO),
            (None, None)
        );
    }

    #[test]
    fn matches_python_balanced_stabilization_options() {
        assert_eq!(detection_options("Balanced"), "shakiness=6:accuracy=10");
        assert_eq!(
            transform_options("Balanced"),
            "smoothing=25:optzoom=1:zoomspeed=0.05:interpol=bicubic"
        );
        assert_eq!(
            deshake_options("Balanced"),
            "deshake=rx=16:ry=16:blocksize=8:edge=mirror"
        );
    }

    #[test]
    fn matches_all_python_stabilization_strengths() {
        assert_eq!(detection_options("Gentle"), "shakiness=4:accuracy=8");
        assert_eq!(
            detection_options("Steady"),
            "shakiness=8:accuracy=12:stepsize=8:mincontrast=0.25"
        );
        assert_eq!(
            detection_options("Strong"),
            "shakiness=10:accuracy=15:stepsize=6:mincontrast=0.25"
        );
        assert_eq!(
            detection_options("Maximum"),
            "shakiness=10:accuracy=15:stepsize=4:mincontrast=0.2"
        );
        assert_eq!(detection_options("invalid"), detection_options("Balanced"));
        assert_eq!(
            transform_options("Maximum"),
            "smoothing=60:optzoom=1:zoomspeed=0.05:interpol=bicubic"
        );
        assert_eq!(
            deshake_options("Strong"),
            "deshake=rx=48:ry=48:blocksize=8:edge=mirror"
        );
    }
}
