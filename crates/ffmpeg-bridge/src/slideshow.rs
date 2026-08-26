use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ffmpeg::Rescale;
use ffmpeg_next as ffmpeg;
use sha2::{Digest, Sha512};
use videoferry_core::{
    ControlDecision, ConversionControl, ConversionEvent, ConversionProgress, EngineError,
    QueueSettings,
};

use crate::PhotoThumbnail;
use crate::mux::write_interleaved;
use crate::remux::PartialOutput;
use crate::slideshow_audio::{self, AudioEncoder};
use crate::slideshow_collage::{self, Cell, ImageSize};

const PHOTO_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff"];
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const MAX_CROSSFADE_SLIDESHOW_IMAGES: usize = 40;
const SLIDESHOW_CHUNK_IMAGE_COUNT: usize = 30;
const TRANSITIONS: [Transition; 20] = [
    Transition::Fade,
    Transition::FadeBlack,
    Transition::FadeWhite,
    Transition::WipeLeft,
    Transition::WipeRight,
    Transition::WipeUp,
    Transition::WipeDown,
    Transition::SlideLeft,
    Transition::SlideRight,
    Transition::SlideUp,
    Transition::SlideDown,
    Transition::CircleOpen,
    Transition::CircleClose,
    Transition::HorizontalOpen,
    Transition::HorizontalClose,
    Transition::VerticalOpen,
    Transition::VerticalClose,
    Transition::Dissolve,
    Transition::Pixelize,
    Transition::HorizontalBlur,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    Fade,
    FadeBlack,
    FadeWhite,
    WipeLeft,
    WipeRight,
    WipeUp,
    WipeDown,
    SlideLeft,
    SlideRight,
    SlideUp,
    SlideDown,
    CircleOpen,
    CircleClose,
    HorizontalOpen,
    HorizontalClose,
    VerticalOpen,
    VerticalClose,
    Dissolve,
    Pixelize,
    HorizontalBlur,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageOrientation {
    Normal,
    FlipHorizontal,
    Rotate180,
    FlipVertical,
    Transpose,
    RotateClockwise,
    Transverse,
    RotateCounterClockwise,
}

impl ImageOrientation {
    const fn filter_prefix(self) -> &'static str {
        match self {
            Self::Normal => "",
            Self::FlipHorizontal => "hflip,",
            Self::Rotate180 => "hflip,vflip,",
            Self::FlipVertical => "vflip,",
            Self::Transpose => "transpose=clock,hflip,",
            Self::RotateClockwise => "transpose=clock,",
            Self::Transverse => "transpose=clock,vflip,",
            Self::RotateCounterClockwise => "transpose=cclock,",
        }
    }

    const fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Transpose
                | Self::RotateClockwise
                | Self::Transverse
                | Self::RotateCounterClockwise
        )
    }
}

struct SlidePlan {
    groups: Vec<Vec<usize>>,
    sizes: Option<Vec<ImageSize>>,
    seed_paths: Vec<PathBuf>,
    width: u32,
    height: u32,
    collage: bool,
    python_chunk_transitions: bool,
}

impl SlidePlan {
    fn render(
        &self,
        image_paths: &[PathBuf],
        slide_index: usize,
    ) -> Result<ffmpeg::frame::Video, EngineError> {
        let group = &self.groups[slide_index];
        if !self.collage {
            return decode_slide(&image_paths[group[0]], self.width, self.height);
        }
        let sizes = self.sizes.as_ref().expect("collage plan has image sizes");
        let group_sizes = group.iter().map(|index| sizes[*index]).collect::<Vec<_>>();
        let cells = slideshow_collage::cells(&group_sizes, self.width, self.height);
        render_collage(
            image_paths,
            group,
            &group_sizes,
            &cells,
            self.width,
            self.height,
        )
    }
}

pub(super) struct SlideshowOutput {
    pub(super) partial: PartialOutput,
    pub(super) duration: Duration,
    pub(super) frame_rate: f64,
    pub(super) frame_count: u64,
    pub(super) codec_name: &'static str,
    pub(super) has_audio: bool,
}

#[expect(
    clippy::too_many_lines,
    reason = "keeps the slideshow resource lifecycle visible in one function"
)]
pub(super) fn write(
    input_path: &Path,
    destination: &Path,
    settings: &QueueSettings,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<SlideshowOutput, EngineError> {
    let image_paths = collect_image_paths(input_path, &settings.slideshow_image_paths)?;
    if image_paths.len() < 2 {
        return Err(EngineError::Unsupported(
            "Photo slideshow requires at least two images".to_owned(),
        ));
    }
    let fps = settings.slideshow_fps.max(1);
    let interval = settings.photo_interval.max(Duration::from_millis(100));
    let (width, height) = settings.slideshow_resolution;
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err(EngineError::Unsupported(
            "slideshow resolution must have positive even dimensions".to_owned(),
        ));
    }

    let slide_plan = plan_slides(
        &image_paths,
        width,
        height,
        settings.slideshow_collage,
        control,
        emit,
    )?;

    let slide_count = u32::try_from(slide_plan.groups.len()).map_err(integer_failure)?;
    let duration = interval
        .checked_mul(slide_count)
        .ok_or_else(|| EngineError::Unsupported("slideshow duration is out of range".to_owned()))?;
    let total_frames = frame_count(duration, fps)?;
    let audio_program =
        slideshow_audio::prepare(&settings.slideshow_audio_paths, duration, control)?;

    let partial = PartialOutput::new(destination)?;
    let mut output = ffmpeg::format::output(partial.path()).map_err(ffmpeg_failure)?;
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
    let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(ffmpeg_failure)?;
    let encoder_time_base = ffmpeg::Rational(1, i32::try_from(fps).map_err(integer_failure)?);
    encoder.set_width(width);
    encoder.set_height(height);
    encoder.set_format(ffmpeg::format::Pixel::YUV420P);
    encoder.set_time_base(encoder_time_base);
    encoder.set_frame_rate(Some(ffmpeg::Rational(
        i32::try_from(fps).map_err(integer_failure)?,
        1,
    )));
    encoder.set_aspect_ratio(ffmpeg::Rational(1, 1));
    encoder.set_gop(frame_count(
        interval.clamp(Duration::from_secs(1), Duration::from_secs(2)),
        fps,
    )?);
    encoder.set_max_b_frames(0);
    if global_header {
        encoder.set_flags(ffmpeg::codec::Flags::GLOBAL_HEADER);
    }
    let mut encoder = encoder
        .open_with(encoder_options(settings))
        .map_err(ffmpeg_failure)?;
    {
        let mut stream = output.add_stream(Some(codec)).map_err(ffmpeg_failure)?;
        stream.set_parameters(&encoder);
        stream.set_time_base(encoder_time_base);
        set_codec_tag(&mut stream, settings.encoder);
    }
    let mut audio_encoder = if audio_program.is_some() {
        Some(AudioEncoder::new(&mut output)?)
    } else {
        None
    };
    output
        .write_header()
        .map_err(|error| EngineError::Failed(format!("writing slideshow header: {error}")))?;
    let output_time_bases = output
        .streams()
        .map(|stream| stream.time_base())
        .collect::<Vec<_>>();
    let output_time_base = output_time_bases[0];
    let transition = transition_duration(interval);
    let interval_nanos = interval.as_nanos();
    let transition_nanos = transition.as_nanos();
    let transitions = slideshow_transition_sequence(
        &slide_plan.seed_paths,
        interval,
        slide_plan.python_chunk_transitions,
    );
    let started_at = Instant::now();
    let mut last_progress = None;
    let mut written_packets = 0_u32;
    let mut slide_cache = BTreeMap::new();
    // Submit one look-ahead duplicate so delayed still-image encoders release
    // the requested final frame. Only the requested number of ordered CFR
    // packets is muxed.
    for frame_index in 0..=total_frames {
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        let timestamp_nanos = u128::from(frame_index) * 1_000_000_000_u128 / u128::from(fps);
        let slide_index = usize::try_from(timestamp_nanos / interval_nanos)
            .map_err(integer_failure)?
            .min(slide_plan.groups.len().saturating_sub(1));
        prepare_slide_cache(
            &mut slide_cache,
            &slide_plan,
            &image_paths,
            slide_index,
            control,
        )?;
        let transition_elapsed = timestamp_nanos % interval_nanos;
        if slide_index > 0 && transition_elapsed < transition_nanos {
            let mut frame = blend_frames(
                &slide_cache[&(slide_index - 1)],
                &slide_cache[&slide_index],
                transition_elapsed,
                transition_nanos,
                transitions[slide_index - 1],
            );
            frame.set_pts(Some(i64::from(frame_index)));
            encoder.send_frame(&frame).map_err(ffmpeg_failure)?;
        } else {
            let mut frame = slide_cache[&slide_index].clone();
            frame.set_pts(Some(i64::from(frame_index)));
            encoder.send_frame(&frame).map_err(ffmpeg_failure)?;
        }
        drain_packets(
            &mut encoder,
            &mut output,
            encoder_time_base,
            output_time_base,
            &mut written_packets,
            total_frames,
        )?;
        if frame_index < total_frames {
            emit_progress(
                frame_index,
                fps,
                total_frames,
                duration,
                started_at,
                partial.path(),
                &mut last_progress,
                emit,
            );
        }
    }
    finish_packets(
        &mut encoder,
        &mut output,
        encoder_time_base,
        output_time_base,
        &mut written_packets,
        total_frames,
    )?;
    let stream_duration = i64::from(total_frames).rescale(encoder_time_base, output_time_base);
    if let Some(mut stream) = output.stream_mut(0) {
        unsafe {
            (*stream.as_mut_ptr()).duration = stream_duration;
            (*stream.as_mut_ptr()).nb_frames = i64::from(total_frames);
        }
    }
    if let (Some(audio_encoder), Some(audio_program)) =
        (audio_encoder.as_mut(), audio_program.as_ref())
    {
        audio_encoder.encode(
            audio_program,
            &mut output,
            output_time_bases[audio_encoder.output_index()],
            control,
        )?;
    }
    output.write_trailer().map_err(ffmpeg_failure)?;
    drop(output);

    Ok(SlideshowOutput {
        partial,
        duration,
        frame_rate: f64::from(fps),
        frame_count: u64::from(total_frames),
        codec_name: expected_codec(settings),
        has_audio: audio_program.is_some(),
    })
}

pub(super) fn decoded_frame_count(path: &Path) -> Result<u64, EngineError> {
    let mut input = ffmpeg::format::input(path).map_err(ffmpeg_failure)?;
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| EngineError::InvalidMedia("slideshow has no video stream".to_owned()))?;
    let stream_index = stream.index();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .and_then(|context| context.decoder().video())
        .map_err(ffmpeg_failure)?;
    let mut count = 0_u64;
    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(ffmpeg_failure)?;
        count = count.saturating_add(drain_frame_count(&mut decoder)?);
    }
    decoder.send_eof().map_err(ffmpeg_failure)?;
    count = count.saturating_add(drain_frame_count(&mut decoder)?);
    Ok(count)
}

pub(super) fn photo_thumbnail(
    path: &Path,
    maximum_width: u32,
    maximum_height: u32,
) -> Result<PhotoThumbnail, EngineError> {
    if maximum_width == 0 || maximum_height == 0 {
        return Err(EngineError::Unsupported(
            "photo preview dimensions must be positive".to_owned(),
        ));
    }
    let frame = decode_transformed(
        path,
        &format!(
            "scale={maximum_width}:{maximum_height}:force_original_aspect_ratio=decrease,setsar=1,format=pix_fmts=rgba"
        ),
    )?;
    rgba_thumbnail(&frame)
}

pub(super) fn review_groups(
    image_paths: &[PathBuf],
    collage: bool,
) -> Result<Vec<Vec<PathBuf>>, EngineError> {
    if !collage {
        return Ok(image_paths.iter().cloned().map(|path| vec![path]).collect());
    }
    let sizes = image_paths
        .iter()
        .map(|path| probe_image_size(path))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(slideshow_collage::groups(&sizes)
        .into_iter()
        .map(|group| {
            group
                .into_iter()
                .map(|index| image_paths[index].clone())
                .collect()
        })
        .collect())
}

pub(super) fn review_thumbnail(
    image_paths: &[PathBuf],
    collage: bool,
    width: u32,
    height: u32,
) -> Result<PhotoThumbnail, EngineError> {
    if image_paths.is_empty() || width < 2 || height < 2 {
        return Err(EngineError::Unsupported(
            "slideshow review preview requires photos and positive dimensions".to_owned(),
        ));
    }
    let width = width - width % 2;
    let height = height - height % 2;
    if !collage || image_paths.len() == 1 {
        return photo_thumbnail(&image_paths[0], width, height);
    }
    let sizes = image_paths
        .iter()
        .map(|path| probe_image_size(path))
        .collect::<Result<Vec<_>, _>>()?;
    let group = (0..image_paths.len()).collect::<Vec<_>>();
    let cells = slideshow_collage::cells(&sizes, width, height);
    let frame = render_collage(image_paths, &group, &sizes, &cells, width, height)?;
    let mut rgba_frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::RGBA, width, height);
    ffmpeg::software::scaling::Context::get(
        frame.format(),
        width,
        height,
        ffmpeg::format::Pixel::RGBA,
        width,
        height,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )
    .map_err(ffmpeg_failure)?
    .run(&frame, &mut rgba_frame)
    .map_err(ffmpeg_failure)?;
    rgba_thumbnail(&rgba_frame)
}

fn rgba_thumbnail(frame: &ffmpeg::frame::Video) -> Result<PhotoThumbnail, EngineError> {
    let row_bytes = usize::try_from(frame.width())
        .map_err(integer_failure)?
        .checked_mul(4)
        .ok_or_else(|| EngineError::Unsupported("photo preview is too wide".to_owned()))?;
    let height = usize::try_from(frame.height()).map_err(integer_failure)?;
    let mut rgba = Vec::with_capacity(row_bytes.saturating_mul(height));
    for row in 0..height {
        let start = row.saturating_mul(frame.stride(0));
        rgba.extend_from_slice(&frame.data(0)[start..start + row_bytes]);
    }
    Ok(PhotoThumbnail {
        width: frame.width(),
        height: frame.height(),
        rgba,
    })
}

fn drain_frame_count(decoder: &mut ffmpeg::decoder::Video) -> Result<u64, EngineError> {
    let mut count = 0_u64;
    let mut frame = ffmpeg::frame::Video::empty();
    loop {
        match decoder.receive_frame(&mut frame) {
            Ok(()) => count = count.saturating_add(1),
            Err(error) if is_again_or_eof(error) => return Ok(count),
            Err(error) => return Err(ffmpeg_failure(error)),
        }
    }
}

fn plan_slides(
    image_paths: &[PathBuf],
    width: u32,
    height: u32,
    collage: bool,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<SlidePlan, EngineError> {
    if !collage {
        emit(ConversionEvent::Warning(format!(
            "Preparing {} slideshow images with a bounded native decode cache",
            image_paths.len()
        )));
        return Ok(SlidePlan {
            groups: (0..image_paths.len()).map(|index| vec![index]).collect(),
            sizes: None,
            seed_paths: image_paths.to_vec(),
            width,
            height,
            collage,
            python_chunk_transitions: image_paths.len() > MAX_CROSSFADE_SLIDESHOW_IMAGES,
        });
    }

    let mut sizes = Vec::with_capacity(image_paths.len());
    for path in image_paths {
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        sizes.push(probe_image_size(path)?);
    }
    let groups = slideshow_collage::groups(&sizes);
    emit(ConversionEvent::Warning(format!(
        "Rendering {} photos into {} native collage slides",
        image_paths.len(),
        groups.len()
    )));
    let seed_paths = groups
        .iter()
        .map(|group| image_paths[group[0]].clone())
        .collect();
    Ok(SlidePlan {
        groups,
        sizes: Some(sizes),
        seed_paths,
        width,
        height,
        collage,
        python_chunk_transitions: false,
    })
}

fn prepare_slide_cache(
    cache: &mut BTreeMap<usize, ffmpeg::frame::Video>,
    plan: &SlidePlan,
    image_paths: &[PathBuf],
    current: usize,
    control: &ConversionControl,
) -> Result<(), EngineError> {
    let first = current.saturating_sub(1);
    cache.retain(|index, _| *index >= first && *index <= current);
    for index in first..=current {
        if cache.contains_key(&index) {
            continue;
        }
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        cache.insert(index, plan.render(image_paths, index)?);
    }
    debug_assert!(cache.len() <= 2);
    Ok(())
}

fn probe_image_size(path: &Path) -> Result<ImageSize, EngineError> {
    let (_, _, frame) = decode_image_frame(path)?;
    let (width, height) = if image_orientation(&frame).swaps_axes() {
        (frame.height(), frame.width())
    } else {
        (frame.width(), frame.height())
    };
    Ok(ImageSize {
        width: width.max(1),
        height: height.max(1),
    })
}

fn render_collage(
    paths: &[PathBuf],
    group: &[usize],
    group_sizes: &[ImageSize],
    cells: &[Cell],
    width: u32,
    height: u32,
) -> Result<ffmpeg::frame::Video, EngineError> {
    let mut slide = black_frame(width, height);
    let paste_cells = slideshow_collage::row_paste_cells(group_sizes, cells, width, height)
        .unwrap_or_else(|| cells.to_vec());
    for (index, cell) in group.iter().zip(paste_cells) {
        let fitted = decode_fitted(&paths[*index], cell.width, cell.height)?;
        copy_into(&fitted, &mut slide, cell);
    }
    Ok(slide)
}

fn black_frame(width: u32, height: u32) -> ffmpeg::frame::Video {
    let mut frame = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, width, height);
    frame.data_mut(0).fill(16);
    frame.data_mut(1).fill(128);
    frame.data_mut(2).fill(128);
    frame.set_pts(Some(0));
    frame
}

fn copy_into(source: &ffmpeg::frame::Video, destination: &mut ffmpeg::frame::Video, cell: Cell) {
    for plane in 0..3 {
        let divisor = if plane == 0 { 1 } else { 2 };
        let width = usize::try_from(cell.width / divisor).expect("cell width fits usize");
        let height = usize::try_from(cell.height / divisor).expect("cell height fits usize");
        let x = usize::try_from(cell.x / divisor).expect("cell x fits usize");
        let y = usize::try_from(cell.y / divisor).expect("cell y fits usize");
        let source_stride = source.stride(plane);
        let destination_stride = destination.stride(plane);
        let source_data = source.data(plane);
        let destination_data = destination.data_mut(plane);
        for row in 0..height {
            let source_start = row * source_stride;
            let destination_start = (y + row) * destination_stride + x;
            destination_data[destination_start..destination_start + width]
                .copy_from_slice(&source_data[source_start..source_start + width]);
        }
    }
}

fn decode_slide(path: &Path, width: u32, height: u32) -> Result<ffmpeg::frame::Video, EngineError> {
    decode_transformed(
        path,
        &format!(
            "scale={width}:{height}:force_original_aspect_ratio=decrease,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1,format=pix_fmts=yuv420p"
        ),
    )
}

fn decode_fitted(
    path: &Path,
    width: u32,
    height: u32,
) -> Result<ffmpeg::frame::Video, EngineError> {
    decode_transformed(
        path,
        &format!(
            "scale={width}:{height}:force_original_aspect_ratio=increase,crop={width}:{height},setsar=1,format=pix_fmts=yuv420p"
        ),
    )
}

fn decode_transformed(path: &Path, chain: &str) -> Result<ffmpeg::frame::Video, EngineError> {
    let (decoder, time_base, frame) = decode_image_frame(path)?;
    let orientation = image_orientation(&frame);
    let oriented_chain = format!("{}{chain}", orientation.filter_prefix());
    apply_image_filter(&decoder, &frame, time_base, &oriented_chain)
}

fn decode_image_frame(
    path: &Path,
) -> Result<
    (
        ffmpeg::decoder::Video,
        ffmpeg::Rational,
        ffmpeg::frame::Video,
    ),
    EngineError,
> {
    let mut input = ffmpeg::format::input(path)
        .map_err(|error| EngineError::InvalidMedia(format!("{}: {error}", path.display())))?;
    let stream = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| EngineError::InvalidMedia(format!("no image stream: {}", path.display())))?;
    let stream_index = stream.index();
    let time_base = stream.time_base();
    let mut decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .and_then(|context| context.decoder().video())
        .map_err(ffmpeg_failure)?;
    decoder.set_packet_time_base(time_base);
    for (stream, packet) in input.packets() {
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet).map_err(ffmpeg_failure)?;
        if let Some(frame) = receive_frame(&mut decoder)? {
            return Ok((decoder, time_base, frame));
        }
    }
    decoder.send_eof().map_err(ffmpeg_failure)?;
    receive_frame(&mut decoder)?
        .ok_or_else(|| {
            EngineError::InvalidMedia(format!("could not decode image: {}", path.display()))
        })
        .map(|frame| (decoder, time_base, frame))
}

fn image_orientation(frame: &ffmpeg::frame::Video) -> ImageOrientation {
    let Some(side_data) = frame.side_data(ffmpeg::util::frame::side_data::Type::DisplayMatrix)
    else {
        return ImageOrientation::Normal;
    };
    let matrix = side_data.data();
    let Some(a) = matrix_axis(matrix, 0) else {
        return ImageOrientation::Normal;
    };
    let Some(b) = matrix_axis(matrix, 1) else {
        return ImageOrientation::Normal;
    };
    let Some(d) = matrix_axis(matrix, 3) else {
        return ImageOrientation::Normal;
    };
    let Some(e) = matrix_axis(matrix, 4) else {
        return ImageOrientation::Normal;
    };
    orientation_from_axes(a, b, d, e)
}

fn matrix_axis(matrix: &[u8], index: usize) -> Option<i8> {
    let start = index.checked_mul(size_of::<i32>())?;
    let bytes: [u8; 4] = matrix
        .get(start..start + size_of::<i32>())?
        .try_into()
        .ok()?;
    let value = i32::from_ne_bytes(bytes);
    Some(match value {
        ..=-32_768 => -1,
        32_768.. => 1,
        _ => 0,
    })
}

const fn orientation_from_axes(a: i8, b: i8, d: i8, e: i8) -> ImageOrientation {
    match (a, b, d, e) {
        (-1, 0, 0, 1) => ImageOrientation::FlipHorizontal,
        (-1, 0, 0, -1) => ImageOrientation::Rotate180,
        (1, 0, 0, -1) => ImageOrientation::FlipVertical,
        (0, 1, 1, 0) => ImageOrientation::Transpose,
        (0, 1, -1, 0) => ImageOrientation::RotateClockwise,
        (0, -1, -1, 0) => ImageOrientation::Transverse,
        (0, -1, 1, 0) => ImageOrientation::RotateCounterClockwise,
        _ => ImageOrientation::Normal,
    }
}

fn receive_frame(
    decoder: &mut ffmpeg::decoder::Video,
) -> Result<Option<ffmpeg::frame::Video>, EngineError> {
    let mut frame = ffmpeg::frame::Video::empty();
    match decoder.receive_frame(&mut frame) {
        Ok(()) => Ok(Some(frame)),
        Err(error) if is_again_or_eof(error) => Ok(None),
        Err(error) => Err(ffmpeg_failure(error)),
    }
}

fn apply_image_filter(
    decoder: &ffmpeg::decoder::Video,
    frame: &ffmpeg::frame::Video,
    time_base: ffmpeg::Rational,
    chain: &str,
) -> Result<ffmpeg::frame::Video, EngineError> {
    let mut graph = ffmpeg::filter::Graph::new();
    let arguments = format!(
        "video_size={}x{}:pix_fmt={}:time_base={time_base}:pixel_aspect=1/1:colorspace={}:range={}",
        decoder.width(),
        decoder.height(),
        ffmpeg::ffi::AVPixelFormat::from(decoder.format()) as i32,
        ffmpeg::ffi::AVColorSpace::from(decoder.color_space()) as i32,
        ffmpeg::ffi::AVColorRange::from(decoder.color_range()) as i32,
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
        .parse(chain)
        .map_err(ffmpeg_failure)?;
    graph.validate().map_err(ffmpeg_failure)?;
    graph
        .get("in")
        .ok_or_else(|| EngineError::Failed("image filter input disappeared".to_owned()))?
        .source()
        .add(frame)
        .map_err(ffmpeg_failure)?;
    let mut output = ffmpeg::frame::Video::empty();
    graph
        .get("out")
        .ok_or_else(|| EngineError::Failed("image filter output disappeared".to_owned()))?
        .sink()
        .frame(&mut output)
        .map_err(ffmpeg_failure)?;
    output.set_pts(Some(0));
    Ok(output)
}

fn blend_frames(
    previous: &ffmpeg::frame::Video,
    next: &ffmpeg::frame::Video,
    progress: u128,
    total: u128,
    transition: Transition,
) -> ffmpeg::frame::Video {
    let mut output = ffmpeg::frame::Video::new(
        ffmpeg::format::Pixel::YUV420P,
        previous.width(),
        previous.height(),
    );
    let progress = progress.min(total);
    for plane in 0..3 {
        let plane_width = if plane == 0 {
            previous.width() as usize
        } else {
            previous.width().div_ceil(2) as usize
        };
        let plane_height = if plane == 0 {
            previous.height() as usize
        } else {
            previous.height().div_ceil(2) as usize
        };
        let previous_stride = previous.stride(plane);
        let next_stride = next.stride(plane);
        let output_stride = output.stride(plane);
        let previous_data = previous.data(plane);
        let next_data = next.data(plane);
        let output_data = output.data_mut(plane);
        for row in 0..plane_height {
            if transition == Transition::HorizontalBlur {
                horizontal_blur_row(
                    previous_data,
                    next_data,
                    previous_stride,
                    next_stride,
                    output_data,
                    output_stride,
                    row,
                    plane_width,
                    progress,
                    total,
                );
                continue;
            }
            for column in 0..plane_width {
                output_data[row * output_stride + column] = transition_value(
                    transition,
                    previous_data,
                    next_data,
                    previous_stride,
                    next_stride,
                    column,
                    row,
                    plane_width,
                    plane_height,
                    plane,
                    progress,
                    total,
                );
            }
        }
    }
    output
}

#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "single exhaustive sampler keeps transition semantics together"
)]
fn transition_value(
    transition: Transition,
    previous: &[u8],
    next: &[u8],
    previous_stride: usize,
    next_stride: usize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    plane: usize,
    progress: u128,
    total: u128,
) -> u8 {
    let old = previous[y * previous_stride + x];
    let new = next[y * next_stride + x];
    match transition {
        Transition::Fade => weighted(old, new, progress, total),
        Transition::Pixelize => {
            let (source_x, source_y) = pixelized_coordinate(x, y, width, height, progress, total);
            weighted_truncated(
                previous[source_y * previous_stride + source_x],
                next[source_y * next_stride + source_x],
                progress,
                total,
            )
        }
        Transition::HorizontalBlur => unreachable!("horizontal blur is rendered one row at a time"),
        Transition::FadeBlack | Transition::FadeWhite => {
            let neutral = if plane == 0 {
                if transition == Transition::FadeBlack {
                    16
                } else {
                    235
                }
            } else {
                128
            };
            if progress.saturating_mul(2) < total {
                weighted(old, neutral, progress.saturating_mul(2), total)
            } else {
                weighted(
                    neutral,
                    new,
                    progress.saturating_mul(2).saturating_sub(total),
                    total,
                )
            }
        }
        Transition::WipeLeft => choose(new, old, scaled(x, total) < scaled(width, progress)),
        Transition::WipeRight => choose(
            new,
            old,
            scaled(width.saturating_sub(x + 1), total) < scaled(width, progress),
        ),
        Transition::WipeUp => choose(new, old, scaled(y, total) < scaled(height, progress)),
        Transition::WipeDown => choose(
            new,
            old,
            scaled(height.saturating_sub(y + 1), total) < scaled(height, progress),
        ),
        Transition::SlideLeft => {
            let shift = usize::try_from(scaled(width, progress) / total).unwrap_or(width);
            let source_x = x.saturating_add(shift);
            if source_x < width {
                previous[y * previous_stride + source_x]
            } else {
                next[y * next_stride + source_x.saturating_sub(width)]
            }
        }
        Transition::SlideRight => {
            let shift = usize::try_from(scaled(width, progress) / total).unwrap_or(width);
            if x >= shift {
                previous[y * previous_stride + x - shift]
            } else {
                next[y * next_stride + width - shift + x]
            }
        }
        Transition::SlideUp => {
            let shift = usize::try_from(scaled(height, progress) / total).unwrap_or(height);
            let source_y = y.saturating_add(shift);
            if source_y < height {
                previous[source_y * previous_stride + x]
            } else {
                next[source_y.saturating_sub(height) * next_stride + x]
            }
        }
        Transition::SlideDown => {
            let shift = usize::try_from(scaled(height, progress) / total).unwrap_or(height);
            if y >= shift {
                previous[(y - shift) * previous_stride + x]
            } else {
                next[(height - shift + y) * next_stride + x]
            }
        }
        Transition::CircleOpen | Transition::CircleClose => {
            let dx = i128::try_from(x.saturating_mul(2)).unwrap_or(i128::MAX)
                - i128::try_from(width).unwrap_or(i128::MAX);
            let dy = i128::try_from(y.saturating_mul(2)).unwrap_or(i128::MAX)
                - i128::try_from(height).unwrap_or(i128::MAX);
            let distance =
                u128::try_from(dx.saturating_mul(dx) + dy.saturating_mul(dy)).unwrap_or(u128::MAX);
            let maximum = scaled(width, u128::try_from(width).unwrap_or(u128::MAX))
                .saturating_add(scaled(height, u128::try_from(height).unwrap_or(u128::MAX)));
            let radius = if transition == Transition::CircleOpen {
                progress
            } else {
                total.saturating_sub(progress)
            };
            let inside = distance.saturating_mul(total.saturating_mul(total))
                <= maximum.saturating_mul(radius.saturating_mul(radius));
            if transition == Transition::CircleOpen {
                choose(new, old, inside)
            } else {
                choose(old, new, inside)
            }
        }
        Transition::HorizontalOpen | Transition::HorizontalClose => {
            let distance = x.abs_diff(width / 2);
            let radius = if transition == Transition::HorizontalOpen {
                progress
            } else {
                total.saturating_sub(progress)
            };
            let inside = scaled(distance.saturating_mul(2), total) <= scaled(width, radius);
            if transition == Transition::HorizontalOpen {
                choose(new, old, inside)
            } else {
                choose(old, new, inside)
            }
        }
        Transition::VerticalOpen | Transition::VerticalClose => {
            let distance = y.abs_diff(height / 2);
            let radius = if transition == Transition::VerticalOpen {
                progress
            } else {
                total.saturating_sub(progress)
            };
            let inside = scaled(distance.saturating_mul(2), total) <= scaled(height, radius);
            if transition == Transition::VerticalOpen {
                choose(new, old, inside)
            } else {
                choose(old, new, inside)
            }
        }
        Transition::Dissolve => {
            let noise = (u128::from(hash_pixel(x, y, plane)) * total) / u128::from(u32::MAX);
            choose(new, old, noise <= progress)
        }
    }
}

fn weighted(old: u8, new: u8, progress: u128, total: u128) -> u8 {
    let progress = progress.min(total);
    let value =
        (u128::from(old) * (total - progress) + u128::from(new) * progress + total / 2) / total;
    u8::try_from(value).expect("a weighted pair of bytes remains a byte")
}

fn weighted_truncated(old: u8, new: u8, progress: u128, total: u128) -> u8 {
    let progress = progress.min(total);
    let value = (u128::from(old) * (total - progress) + u128::from(new) * progress) / total;
    u8::try_from(value).expect("a weighted pair of bytes remains a byte")
}

fn pixelized_coordinate(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    progress: u128,
    total: u128,
) -> (usize, usize) {
    let distance = progress.min(total.saturating_sub(progress.min(total)));
    let distance_steps = distance
        .saturating_mul(50)
        .saturating_add(total.saturating_sub(1))
        / total;
    if distance_steps == 0 {
        return (x, y);
    }
    let block_numerator =
        distance_steps.saturating_mul(u128::try_from(width.min(height)).unwrap_or(u128::MAX));
    (
        pixelized_axis(x, width, block_numerator),
        pixelized_axis(y, height, block_numerator),
    )
}

fn pixelized_axis(position: usize, length: usize, block_numerator: u128) -> usize {
    let quotient = u128::try_from(position)
        .unwrap_or(u128::MAX)
        .saturating_mul(500)
        / block_numerator;
    let centered = quotient
        .saturating_mul(2)
        .saturating_add(1)
        .saturating_mul(block_numerator)
        / 1_000;
    usize::try_from(centered)
        .unwrap_or(usize::MAX)
        .min(length.saturating_sub(1))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the row kernel receives explicit plane buffers and strides"
)]
fn horizontal_blur_row(
    previous: &[u8],
    next: &[u8],
    previous_stride: usize,
    next_stride: usize,
    output: &mut [u8],
    output_stride: usize,
    row: usize,
    width: usize,
    progress: u128,
    total: u128,
) {
    let progress = progress.min(total);
    let distance = progress.min(total - progress);
    let size = 1_usize.saturating_add(
        usize::try_from(
            u128::try_from(width / 2)
                .unwrap_or(u128::MAX)
                .saturating_mul(distance)
                .saturating_mul(2)
                / total,
        )
        .unwrap_or(width),
    );
    let size = size.min(width);
    let previous_row = &previous[row * previous_stride..row * previous_stride + width];
    let next_row = &next[row * next_stride..row * next_stride + width];
    let output_row = &mut output[row * output_stride..row * output_stride + width];
    let mut previous_sum = previous_row[..size]
        .iter()
        .map(|value| u64::from(*value))
        .sum::<u64>();
    let mut next_sum = next_row[..size]
        .iter()
        .map(|value| u64::from(*value))
        .sum::<u64>();
    let mut count = u128::try_from(size).expect("plane width fits u128");
    for x in 0..width {
        let numerator = u128::from(previous_sum)
            .saturating_mul(total - progress)
            .saturating_add(u128::from(next_sum).saturating_mul(progress));
        let value = numerator / count.saturating_mul(total);
        output_row[x] = u8::try_from(value).expect("an average of bytes remains a byte");
        if x + size < width {
            previous_sum =
                previous_sum + u64::from(previous_row[x + size]) - u64::from(previous_row[x]);
            next_sum = next_sum + u64::from(next_row[x + size]) - u64::from(next_row[x]);
        } else {
            previous_sum -= u64::from(previous_row[x]);
            next_sum -= u64::from(next_row[x]);
            count = count.saturating_sub(1);
        }
    }
}

const fn choose(selected: u8, alternate: u8, condition: bool) -> u8 {
    if condition { selected } else { alternate }
}

fn scaled(value: usize, scale: u128) -> u128 {
    u128::try_from(value)
        .unwrap_or(u128::MAX)
        .saturating_mul(scale)
}

fn hash_pixel(x: usize, y: usize, plane: usize) -> u32 {
    let mut value = u64::try_from(x)
        .unwrap_or(u64::MAX)
        .wrapping_mul(0x9E37_79B1);
    value ^= u64::try_from(y)
        .unwrap_or(u64::MAX)
        .wrapping_mul(0x85EB_CA77);
    value ^= u64::try_from(plane)
        .unwrap_or(u64::MAX)
        .wrapping_mul(0xC2B2_AE3D);
    value ^= value >> 16;
    u32::try_from(value & u64::from(u32::MAX)).expect("masked to 32 bits")
}

fn drain_packets(
    encoder: &mut ffmpeg::encoder::Video,
    output: &mut ffmpeg::format::context::Output,
    encoder_time_base: ffmpeg::Rational,
    output_time_base: ffmpeg::Rational,
    written_packets: &mut u32,
    maximum_packets: u32,
) -> Result<(), EngineError> {
    let mut packet = ffmpeg::Packet::empty();
    loop {
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {
                write_packet(
                    &mut packet,
                    output,
                    encoder_time_base,
                    output_time_base,
                    written_packets,
                    maximum_packets,
                )?;
            }
            Err(error) if is_again_or_eof(error) => return Ok(()),
            Err(error) => return Err(ffmpeg_failure(error)),
        }
    }
}

fn finish_packets(
    encoder: &mut ffmpeg::encoder::Video,
    output: &mut ffmpeg::format::context::Output,
    encoder_time_base: ffmpeg::Rational,
    output_time_base: ffmpeg::Rational,
    written_packets: &mut u32,
    maximum_packets: u32,
) -> Result<(), EngineError> {
    encoder.send_eof().map_err(ffmpeg_failure)?;
    let mut packet = ffmpeg::Packet::empty();
    let mut again_count = 0_u8;
    loop {
        match encoder.receive_packet(&mut packet) {
            Ok(()) => {
                again_count = 0;
                write_packet(
                    &mut packet,
                    output,
                    encoder_time_base,
                    output_time_base,
                    written_packets,
                    maximum_packets,
                )?;
            }
            Err(ffmpeg::Error::Eof) => return Ok(()),
            Err(error) if is_again_or_eof(error) && again_count < 8 => {
                again_count += 1;
                std::thread::yield_now();
            }
            Err(error) => return Err(ffmpeg_failure(error)),
        }
    }
}

fn write_packet(
    packet: &mut ffmpeg::Packet,
    output: &mut ffmpeg::format::context::Output,
    encoder_time_base: ffmpeg::Rational,
    output_time_base: ffmpeg::Rational,
    written_packets: &mut u32,
    maximum_packets: u32,
) -> Result<(), EngineError> {
    if *written_packets >= maximum_packets {
        return Ok(());
    }
    packet.set_stream(0);
    if packet.duration() <= 0 {
        packet.set_duration(1);
    }
    packet.rescale_ts(encoder_time_base, output_time_base);
    packet.set_position(-1);
    write_interleaved(packet, output)?;
    *written_packets = written_packets.saturating_add(1);
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "typed conversion progress payload"
)]
fn emit_progress(
    frame_index: u32,
    fps: u32,
    total_frames: u32,
    total: Duration,
    started_at: Instant,
    partial_path: &Path,
    last_progress: &mut Option<Duration>,
    emit: &mut dyn FnMut(ConversionEvent),
) {
    let completed = Duration::from_secs_f64(f64::from(frame_index + 1) / f64::from(fps));
    if last_progress.is_some_and(|last| completed.saturating_sub(last) < PROGRESS_INTERVAL)
        && frame_index + 1 != total_frames
    {
        return;
    }
    let elapsed = started_at.elapsed().as_secs_f64();
    emit(ConversionEvent::Progress(ConversionProgress {
        overall: None,
        completed: completed.min(total),
        total: Some(total),
        frames: Some(u64::from(frame_index + 1)),
        total_frames: Some(u64::from(total_frames)),
        target_fps: Some(f64::from(fps)),
        frames_per_second: (elapsed > 0.0).then_some(f64::from(frame_index + 1) / elapsed),
        speed: (elapsed > 0.0).then_some(completed.as_secs_f64() / elapsed),
        output_bytes: std::fs::metadata(partial_path)
            .ok()
            .map(|metadata| metadata.len()),
    }));
    *last_progress = Some(completed);
}

fn collect_image_paths(
    input_path: &Path,
    selected: &[PathBuf],
) -> Result<Vec<PathBuf>, EngineError> {
    let mut paths = if selected.is_empty() {
        if input_path.is_dir() {
            collect_directory_images(input_path)?
        } else if is_photo(input_path) {
            vec![input_path.to_path_buf()]
        } else {
            Vec::new()
        }
    } else {
        selected
            .iter()
            .filter(|path| path.is_file() && is_photo(path))
            .cloned()
            .collect()
    };
    paths.sort_by_key(|path| natural_key(path));
    paths.dedup();
    Ok(paths)
}

fn collect_directory_images(root: &Path) -> Result<Vec<PathBuf>, EngineError> {
    let mut pending = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            EngineError::Failed(format!("cannot read {}: {error}", directory.display()))
        })?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if is_photo(&path) {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

fn natural_key(path: &Path) -> Vec<NaturalPart> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut digits = None;
    for character in name.chars() {
        let is_digit = character.is_ascii_digit();
        if digits.is_some_and(|value| value != is_digit) {
            parts.push(natural_part(&current, digits.unwrap_or(false)));
            current.clear();
        }
        digits = Some(is_digit);
        current.push(character);
    }
    if !current.is_empty() {
        parts.push(natural_part(&current, digits.unwrap_or(false)));
    }
    parts.push(NaturalPart::Text(
        path.to_string_lossy().to_ascii_lowercase(),
    ));
    parts
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum NaturalPart {
    Number(u128, usize),
    Text(String),
}

fn natural_part(value: &str, digits: bool) -> NaturalPart {
    if digits {
        NaturalPart::Number(value.parse().unwrap_or(u128::MAX), value.len())
    } else {
        NaturalPart::Text(value.to_owned())
    }
}

fn transition_sequence(paths: &[PathBuf], interval: Duration) -> Vec<Transition> {
    let path_seed = paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\0");
    let seed = format!("{path_seed}\0{}", interval.as_secs_f64());
    let mut random = PythonRandom::from_string(&seed);
    (1..paths.len())
        .map(|_| TRANSITIONS[random.rand_below(TRANSITIONS.len())])
        .collect()
}

fn slideshow_transition_sequence(
    paths: &[PathBuf],
    interval: Duration,
    python_chunks: bool,
) -> Vec<Transition> {
    if !python_chunks {
        return transition_sequence(paths, interval);
    }
    let mut transitions = Vec::with_capacity(paths.len().saturating_sub(1));
    for range in image_chunk_ranges(paths.len()) {
        let seed_end = range.end.saturating_add(1).min(paths.len());
        transitions.extend(transition_sequence(&paths[range.start..seed_end], interval));
    }
    transitions
}

fn image_chunk_ranges(image_count: usize) -> Vec<Range<usize>> {
    let mut ranges = (0..image_count)
        .step_by(SLIDESHOW_CHUNK_IMAGE_COUNT)
        .map(|start| {
            start
                ..start
                    .saturating_add(SLIDESHOW_CHUNK_IMAGE_COUNT)
                    .min(image_count)
        })
        .collect::<Vec<_>>();
    if ranges.len() > 1
        && ranges
            .last()
            .is_some_and(|range| range.end - range.start == 1)
    {
        let last_end = ranges.pop().expect("checked final chunk").end;
        ranges.last_mut().expect("checked preceding chunk").end = last_end;
    }
    ranges
}

struct PythonRandom {
    state: [u32; 624],
    index: usize,
}

impl PythonRandom {
    fn from_string(seed: &str) -> Self {
        let mut bytes = seed.as_bytes().to_vec();
        bytes.extend(Sha512::digest(seed.as_bytes()));
        let mut key = Vec::with_capacity(bytes.len().div_ceil(4));
        let mut end = bytes.len();
        while end > 0 {
            let start = end.saturating_sub(4);
            let mut value = 0_u32;
            for byte in &bytes[start..end] {
                value = value.wrapping_shl(8) | u32::from(*byte);
            }
            key.push(value);
            end = start;
        }
        Self::from_key(&key)
    }

    fn from_key(key: &[u32]) -> Self {
        let mut random = Self {
            state: [0; 624],
            index: 624,
        };
        random.state[0] = 19_650_218;
        for index in 1..624 {
            random.state[index] = 1_812_433_253_u32
                .wrapping_mul(random.state[index - 1] ^ (random.state[index - 1] >> 30))
                .wrapping_add(u32::try_from(index).expect("MT index fits u32"));
        }
        let mut state_index = 1_usize;
        let mut key_index = 0_usize;
        for _ in 0..624.max(key.len()) {
            let previous = random.state[state_index - 1];
            random.state[state_index] = (random.state[state_index]
                ^ (previous ^ (previous >> 30)).wrapping_mul(1_664_525))
            .wrapping_add(key[key_index])
            .wrapping_add(u32::try_from(key_index).expect("seed index fits u32"));
            state_index += 1;
            key_index += 1;
            if state_index >= 624 {
                random.state[0] = random.state[623];
                state_index = 1;
            }
            if key_index >= key.len() {
                key_index = 0;
            }
        }
        for _ in 0..623 {
            let previous = random.state[state_index - 1];
            random.state[state_index] = (random.state[state_index]
                ^ (previous ^ (previous >> 30)).wrapping_mul(1_566_083_941))
            .wrapping_sub(u32::try_from(state_index).expect("MT index fits u32"));
            state_index += 1;
            if state_index >= 624 {
                random.state[0] = random.state[623];
                state_index = 1;
            }
        }
        random.state[0] = 0x8000_0000;
        random
    }

    fn rand_below(&mut self, upper: usize) -> usize {
        let bits = usize::BITS - upper.leading_zeros();
        loop {
            let value = usize::try_from(self.get_rand_bits(bits)).expect("random u32 fits usize");
            if value < upper {
                return value;
            }
        }
    }

    fn get_rand_bits(&mut self, bits: u32) -> u32 {
        debug_assert!((1..=32).contains(&bits));
        self.next_u32() >> (32 - bits)
    }

    fn next_u32(&mut self) -> u32 {
        if self.index >= 624 {
            self.twist();
        }
        let mut value = self.state[self.index];
        self.index += 1;
        value ^= value >> 11;
        value ^= (value << 7) & 0x9D2C_5680;
        value ^= (value << 15) & 0xEFC6_0000;
        value ^ (value >> 18)
    }

    fn twist(&mut self) {
        for index in 0..624 {
            let combined =
                (self.state[index] & 0x8000_0000) | (self.state[(index + 1) % 624] & 0x7FFF_FFFF);
            let mut twisted = combined >> 1;
            if combined & 1 != 0 {
                twisted ^= 0x9908_B0DF;
            }
            self.state[index] = self.state[(index + 397) % 624] ^ twisted;
        }
        self.index = 0;
    }
}

fn is_photo(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            PHOTO_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

fn transition_duration(interval: Duration) -> Duration {
    Duration::from_secs_f64((interval.as_secs_f64() / 2.0).clamp(0.2, 2.0))
}

fn frame_count(duration: Duration, fps: u32) -> Result<u32, EngineError> {
    let numerator = duration
        .as_nanos()
        .checked_mul(u128::from(fps))
        .ok_or_else(|| {
            EngineError::Unsupported("slideshow frame count is out of range".to_owned())
        })?;
    let frames = numerator.saturating_add(500_000_000) / 1_000_000_000;
    if frames == 0 {
        return Err(EngineError::Unsupported(
            "slideshow frame count is out of range".to_owned(),
        ));
    }
    u32::try_from(frames).map_err(integer_failure)
}

fn encoder_options(settings: &QueueSettings) -> ffmpeg::Dictionary<'static> {
    let mut options = ffmpeg::Dictionary::new();
    if let Some(quality) = settings.quality {
        options.set("crf", &format!("{quality:.2}"));
    }
    if let Some(preset) = &settings.speed_preset {
        options.set("preset", preset);
    }
    options
}

fn set_codec_tag(stream: &mut ffmpeg::StreamMut<'_>, encoder: videoferry_core::Encoder) {
    let tag = match encoder {
        videoferry_core::Encoder::X265
        | videoferry_core::Encoder::HevcNvenc
        | videoferry_core::Encoder::HevcVideoToolbox => Some(*b"hvc1"),
        videoferry_core::Encoder::X264
        | videoferry_core::Encoder::H264Nvenc
        | videoferry_core::Encoder::H264VideoToolbox => Some(*b"avc1"),
        _ => None,
    };
    if let Some(tag) = tag {
        unsafe {
            (*stream.parameters().as_mut_ptr()).codec_tag = u32::from_le_bytes(tag);
        }
    }
}

fn expected_codec(settings: &QueueSettings) -> &'static str {
    match settings.encoder {
        videoferry_core::Encoder::X265
        | videoferry_core::Encoder::HevcNvenc
        | videoferry_core::Encoder::HevcVideoToolbox => "hevc",
        videoferry_core::Encoder::X264
        | videoferry_core::Encoder::H264Nvenc
        | videoferry_core::Encoder::H264VideoToolbox => "h264",
        videoferry_core::Encoder::SvtAv1
        | videoferry_core::Encoder::Av1Nvenc
        | videoferry_core::Encoder::Av1VideoToolbox => "av1",
    }
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
    EngineError::Unsupported(format!("numeric setting is out of range: {error}"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{
        ImageOrientation, PythonRandom, frame_count, horizontal_blur_row, image_chunk_ranges,
        natural_key, orientation_from_axes, pixelized_coordinate, slideshow_transition_sequence,
        transition_duration, transition_sequence,
    };

    #[test]
    fn uses_python_compatible_transition_duration_bounds() {
        assert_eq!(
            transition_duration(Duration::from_millis(100)),
            Duration::from_millis(200)
        );
        assert_eq!(
            transition_duration(Duration::from_secs(3)),
            Duration::from_millis(1_500)
        );
        assert_eq!(
            transition_duration(Duration::from_secs(10)),
            Duration::from_secs(2)
        );
    }

    #[test]
    fn computes_the_expected_cfr_frame_count() {
        assert_eq!(frame_count(Duration::from_secs(8), 30).unwrap(), 240);
    }

    #[test]
    fn naturally_orders_numbered_images() {
        let mut paths = [PathBuf::from("photo10.jpg"), PathBuf::from("photo2.jpg")];
        paths.sort_by_key(|path| natural_key(path));
        assert_eq!(paths[0], Path::new("photo2.jpg"));
    }

    #[test]
    fn matches_python_string_seeded_transition_choices() {
        let mut random =
            PythonRandom::from_string(concat!("photo1.jpg\0photo2.jpg\0photo10.jpg\0", "4"));
        let actual = (0..8).map(|_| random.rand_below(20)).collect::<Vec<_>>();
        assert_eq!(actual, [11, 6, 10, 6, 3, 9, 1, 14]);
    }

    #[test]
    fn maps_all_exif_display_matrices_to_python_transforms() {
        let cases = [
            ((1, 0, 0, 1), ImageOrientation::Normal),
            ((-1, 0, 0, 1), ImageOrientation::FlipHorizontal),
            ((-1, 0, 0, -1), ImageOrientation::Rotate180),
            ((1, 0, 0, -1), ImageOrientation::FlipVertical),
            ((0, 1, 1, 0), ImageOrientation::Transpose),
            ((0, 1, -1, 0), ImageOrientation::RotateClockwise),
            ((0, -1, -1, 0), ImageOrientation::Transverse),
            ((0, -1, 1, 0), ImageOrientation::RotateCounterClockwise),
        ];
        for ((a, b, d, e), expected) in cases {
            assert_eq!(orientation_from_axes(a, b, d, e), expected);
        }
    }

    #[test]
    fn rotated_exif_orientations_swap_layout_axes() {
        assert!(ImageOrientation::Transpose.swaps_axes());
        assert!(ImageOrientation::RotateClockwise.swaps_axes());
        assert!(ImageOrientation::Transverse.swaps_axes());
        assert!(ImageOrientation::RotateCounterClockwise.swaps_axes());
        assert!(!ImageOrientation::Rotate180.swaps_axes());
    }

    #[test]
    fn matches_python_large_slideshow_chunk_boundaries() {
        assert_eq!(image_chunk_ranges(41), [0..30, 30..41]);
        assert_eq!(image_chunk_ranges(61), [0..30, 30..61]);
        assert_eq!(image_chunk_ranges(91), [0..30, 30..60, 60..91]);
    }

    #[test]
    fn seeds_large_slideshow_transitions_per_python_chunk() {
        let paths = (0..61)
            .map(|index| PathBuf::from(format!("photo{index}.jpg")))
            .collect::<Vec<_>>();
        let interval = Duration::from_secs(4);
        let actual = slideshow_transition_sequence(&paths, interval, true);
        let mut expected = transition_sequence(&paths[..31], interval);
        expected.extend(transition_sequence(&paths[30..], interval));
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 60);
    }

    #[test]
    fn pixelize_samples_ffmpeg_block_centers() {
        assert_eq!(pixelized_coordinate(0, 0, 100, 80, 50, 100), (2, 2));
        assert_eq!(pixelized_coordinate(3, 3, 100, 80, 50, 100), (2, 2));
        assert_eq!(pixelized_coordinate(4, 4, 100, 80, 50, 100), (6, 6));
        assert_eq!(pixelized_coordinate(99, 79, 100, 80, 50, 100), (98, 78));
        assert_eq!(pixelized_coordinate(12, 15, 100, 80, 0, 100), (12, 15));
    }

    #[test]
    fn horizontal_blur_matches_ffmpeg_sliding_window() {
        let previous = [0, 30, 60, 90];
        let next = [100, 130, 160, 190];
        let mut output = [0; 4];
        horizontal_blur_row(&previous, &next, 4, 4, &mut output, 4, 0, 4, 50, 100);
        assert_eq!(output, [80, 110, 125, 140]);
    }
}
