#[path = "persistence.rs"]
mod persistence;
#[path = "platform_indicator.rs"]
mod platform_indicator;
#[path = "unicode_fonts.rs"]
mod unicode_fonts;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::platform_actions::{open_path_with_default_app, reveal_path_in_file_manager};
use eframe::egui;
use persistence::{AppPreferences, CompletedHistoryRow, CpuLimitLevel, StateStore};
use platform_indicator::PlatformIndicator;
use videoferry_core::{
    Container, ContentMode, ControlDecision, ConversionControl, ConversionEvent, ConversionPreview,
    ConversionProgress, ConversionRequest, Encoder, EngineError, FpsPolicy, MediaEngine, MediaInfo,
    Queue, QueueSettings, QueueStatus, QueueTask, StreamKind, build_stream_plan,
    conversion_output_path, stabilized_output_path, trim_output_path,
};
use videoferry_ffmpeg::{ProcessCpuLimiter, ProcessCpuSampler};
#[cfg(any(feature = "native-ffmpeg", test))]
use videoferry_presets::dji_camera_profile;
use videoferry_presets::{
    converted_directory_suffix, default_settings, descriptor, dji_camera_profiles,
};

/// Starts the temporary egui fallback frontend.
///
/// # Errors
///
/// Returns an [`eframe::Error`] if the native window or renderer cannot start.
pub fn run() -> eframe::Result<()> {
    if handle_runtime_verification() {
        return Ok(());
    }
    if let Err(error) = cleanup_stale_temporary_files() {
        eprintln!("unable to clean stale VideoFerry temporary files: {error}");
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("VideoFerry")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([820.0, 560.0]),
        ..Default::default()
    };

    eframe::run_native(
        "VideoFerry",
        options,
        Box::new(|creation_context| {
            unicode_fonts::install(&creation_context.egui_ctx);
            Ok(Box::<ConverterApp>::default())
        }),
    )
}

/// Performs the packaging-only runtime check before a GUI backend is started.
///
/// Both the Slint frontend and the temporary egui fallback use this so the
/// signed-package verification contract stays identical during migration.
#[must_use]
pub fn handle_runtime_verification() -> bool {
    if std::env::args_os()
        .skip(1)
        .any(|argument| argument == "--verify-runtime")
    {
        match packaged_runtime_report() {
            Ok(report) => write_runtime_report(&report),
            Err(error) => {
                write_runtime_report(&format!("runtime=error\nmessage={error}"));
                std::process::exit(2);
            }
        }
        return true;
    }
    false
}

fn write_runtime_report(report: &str) {
    if let Some(path) = std::env::var_os("VIDEOFERRY_RUNTIME_REPORT_PATH") {
        if let Err(error) = std::fs::write(&path, report) {
            eprintln!("unable to write runtime report: {error}");
            std::process::exit(3);
        }
    } else {
        println!("{report}");
    }
}

#[cfg(feature = "native-ffmpeg")]
fn packaged_runtime_report() -> Result<String, EngineError> {
    videoferry_ffmpeg::NativeEngine::new()?.verify_packaged_runtime()
}

#[cfg(not(feature = "native-ffmpeg"))]
fn packaged_runtime_report() -> Result<String, EngineError> {
    Err(EngineError::Unavailable(
        "direct FFmpeg support is not compiled in".to_owned(),
    ))
}

struct ConverterApp {
    queue: Queue,
    selected_id: Option<String>,
    selected_ids: HashSet<String>,
    selection_anchor_id: Option<String>,
    next_id: u64,
    mode: ContentMode,
    encoder: Encoder,
    trim_start_text: String,
    trim_end_text: String,
    stabilize_strength: String,
    apply_lut: bool,
    slideshow_interval_seconds: f32,
    slideshow_fps: u32,
    slideshow_resolution: String,
    slideshow_audio_paths: Vec<PathBuf>,
    slideshow_audio_selected: Option<usize>,
    slideshow_collage: bool,
    fps_mode: FpsUiMode,
    explicit_fps: f64,
    quality_crf: f32,
    speed_preset: String,
    engine_status: String,
    available_encoders: Vec<Encoder>,
    engine_discovery: EngineDiscoveryState,
    activity: String,
    completion_notice: String,
    progress: Option<ConversionProgress>,
    worker: Option<WorkerState>,
    photo_review: Option<PhotoReviewState>,
    photo_viewer: Option<PhotoViewerState>,
    review_worker: Option<ReviewWorkerState>,
    next_review_request_id: u64,
    state_store: StateStore,
    settings_by_mode: HashMap<ContentMode, QueueSettings>,
    encoding_settings: HashMap<(ContentMode, Encoder), EncodingUiSettings>,
    persisted_mode: ContentMode,
    resume_task_id: Option<String>,
    queue_run_state: QueueRunState,
    center_view: CenterView,
    completed_history: Vec<CompletedHistoryRow>,
    next_history_refresh: Instant,
    sleep: SleepState,
    cpu_limit: CpuLimitLevel,
    persisted_cpu_limit: CpuLimitLevel,
    cpu_limiter: ProcessCpuLimiter,
    cpu_sampler: ProcessCpuSampler,
    process_cpu_usage_percent: Option<f64>,
    next_cpu_usage_refresh: Instant,
    watched_folders: Vec<WatchedFolder>,
    folder_summaries: Vec<FolderQueueSummary>,
    task_run_failures: HashMap<String, HashMap<PathBuf, String>>,
    task_run_start_converted: HashMap<String, usize>,
    next_folder_refresh: Instant,
    task_name_edit: String,
    task_name_edit_id: Option<String>,
    task_file_error_dialog: Option<(String, String)>,
    activity_log: Vec<String>,
    logged_activity: String,
    frame_preview_enabled: bool,
    live_preview_texture: Option<egui::TextureHandle>,
    live_preview: Option<ConversionPreview>,
    active_source_info: Option<HistoryMediaInfo>,
    active_stream_info: Option<StreamCarryInfo>,
    active_camera_info: CameraRunInfo,
    close_state: CloseState,
    platform_indicator: PlatformIndicator,
}

#[derive(Debug, Clone, PartialEq)]
struct EncodingUiSettings {
    quality_crf: f32,
    speed_preset: String,
}

impl From<&QueueSettings> for EncodingUiSettings {
    fn from(settings: &QueueSettings) -> Self {
        Self {
            quality_crf: settings.quality.unwrap_or(28.0),
            speed_preset: settings.speed_preset.clone().unwrap_or_default(),
        }
    }
}

fn encoding_ui_settings_for(
    remembered: &HashMap<(ContentMode, Encoder), EncodingUiSettings>,
    mode: ContentMode,
    encoder: Encoder,
) -> EncodingUiSettings {
    remembered
        .get(&(mode, encoder))
        .cloned()
        .unwrap_or_else(|| EncodingUiSettings::from(&default_settings(mode, encoder)))
}

fn speed_options(encoder: Encoder) -> Vec<String> {
    if matches!(encoder, Encoder::X264 | Encoder::X265) {
        [
            "ultrafast",
            "superfast",
            "veryfast",
            "faster",
            "fast",
            "medium",
            "slow",
            "slower",
            "veryslow",
            "placebo",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    } else if encoder == Encoder::SvtAv1 {
        (0..=13).map(|value| value.to_string()).collect()
    } else if encoder.is_nvenc() {
        (1..=7).map(|value| format!("p{value}")).collect()
    } else {
        Vec::new()
    }
}

fn speed_option_labels(encoder: Encoder) -> Vec<String> {
    if encoder.is_nvenc() {
        [
            "P1 · Fastest",
            "P2 · Very fast",
            "P3 · Fast",
            "P4 · Medium (default)",
            "P5 · Quality",
            "P6 · High quality",
            "P7 · Highest quality",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    } else {
        speed_options(encoder)
    }
}

fn speed_help(encoder: Encoder) -> String {
    if encoder.is_nvenc() {
        "P1 is fastest, P4 is medium, and P7 gives the highest quality.".to_owned()
    } else {
        "Slower settings usually make a smaller file but take longer.".to_owned()
    }
}

struct ReviewPhoto {
    path: PathBuf,
    included: bool,
}

struct PhotoReviewState {
    task_id: String,
    photos: Vec<ReviewPhoto>,
    editable: bool,
    selected: Option<usize>,
    open: bool,
    preview_path: Option<PathBuf>,
    preview_texture: Option<egui::TextureHandle>,
    preview_error: Option<String>,
    preview_request_id: Option<u64>,
    thumbnail_textures: HashMap<PathBuf, egui::TextureHandle>,
    thumbnail_request_ids: HashMap<PathBuf, u64>,
    thumbnail_failures: HashSet<PathBuf>,
    tab: PhotoReviewTab,
    slide_groups: Vec<Vec<PathBuf>>,
    slide_selected: Option<usize>,
    slide_preview_paths: Vec<PathBuf>,
    slide_preview_texture: Option<egui::TextureHandle>,
    slide_error: Option<String>,
    slide_thumbnail_textures: HashMap<Vec<PathBuf>, egui::TextureHandle>,
    slide_thumbnail_request_ids: HashMap<Vec<PathBuf>, u64>,
    slide_thumbnail_failures: HashSet<Vec<PathBuf>>,
    slide_groups_request_id: Option<u64>,
    slide_preview_request_id: Option<u64>,
    slides_dirty: bool,
}

struct PhotoViewerState {
    path: PathBuf,
    open: bool,
    texture: Option<egui::TextureHandle>,
    error: Option<String>,
    request_id: Option<u64>,
    zoom: f32,
    fit_to_window: bool,
}

struct ReviewWorkerState {
    sender: mpsc::Sender<ReviewWorkerRequest>,
    receiver: mpsc::Receiver<ReviewWorkerResult>,
}

enum ReviewWorkerRequest {
    Photo {
        request_id: u64,
        target: ReviewPhotoTarget,
        path: PathBuf,
        maximum_width: u32,
        maximum_height: u32,
    },
    SlideGroups {
        request_id: u64,
        paths: Vec<PathBuf>,
        collage: bool,
    },
    SlidePreview {
        request_id: u64,
        target: ReviewSlideTarget,
        paths: Vec<PathBuf>,
        collage: bool,
        width: u32,
        height: u32,
    },
}

enum ReviewWorkerResult {
    Photo {
        request_id: u64,
        target: ReviewPhotoTarget,
        path: PathBuf,
        result: Result<videoferry_ffmpeg::PhotoThumbnail, EngineError>,
    },
    SlideGroups {
        request_id: u64,
        result: Result<Vec<Vec<PathBuf>>, EngineError>,
    },
    SlidePreview {
        request_id: u64,
        target: ReviewSlideTarget,
        paths: Vec<PathBuf>,
        result: Result<videoferry_ffmpeg::PhotoThumbnail, EngineError>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewPhotoTarget {
    Review,
    Viewer,
    Thumbnail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewSlideTarget {
    Selected,
    Thumbnail,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PhotoReviewTab {
    OriginalPhotos,
    Slides,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FpsUiMode {
    SharedLowest,
    Source,
    Explicit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueRunState {
    Idle,
    Running,
    RunningSelected,
    PauseAfterCurrent,
    PauseAfterCurrentSelected,
    PausedBetweenFiles,
    PausedBetweenFilesSelected,
    Stopping,
}

#[derive(Default, PartialEq, Eq)]
enum CloseState {
    #[default]
    Open,
    WaitingForWorker,
}

enum EngineDiscoveryState {
    NotStarted,
    Running(mpsc::Receiver<(String, Vec<Encoder>)>),
    Complete,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CenterView {
    Queue,
    CompletedHistory,
    Processes,
    Log,
    About,
}

#[derive(Clone, Copy)]
enum TaskTargetAction {
    MoveUp(usize),
    MoveDown(usize),
    Remove(usize),
}

const HISTORY_HEADERS: [&str; 14] = [
    "Target",
    "Mode",
    "LUT",
    "Started",
    "Finished",
    "Minutes",
    "Original",
    "New",
    "Resolution",
    "Original FPS",
    "Target FPS",
    "Encoder",
    "Quality (CRF)",
    "Preset",
];

impl FpsUiMode {
    const fn label(self) -> &'static str {
        match self {
            Self::SharedLowest => "Shared lowest in folder",
            Self::Source => "Keep source FPS",
            Self::Explicit => "Set explicit FPS",
        }
    }
}

enum WorkerUpdate {
    SourceInfo(HistoryMediaInfo, StreamCarryInfo),
    CameraInfo(CameraRunInfo),
    Event(ConversionEvent),
    Finished(Result<WorkerJobOutcome, EngineError>),
}

enum WorkerJobOutcome {
    Converted(Box<CompletedJob>),
    Skipped { reason: String },
}

struct CompletedJob {
    input: PathBuf,
    output: PathBuf,
    lut_name: Option<String>,
    start_time: String,
    end_time: String,
    process_minutes: String,
    original: HistoryMediaInfo,
    converted: HistoryMediaInfo,
}

#[derive(Clone, Default)]
struct HistoryMediaInfo {
    size: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<f64>,
    codec: Option<String>,
    duration: Option<Duration>,
}

#[derive(Clone, Default)]
struct StreamCarryInfo {
    audio_tracks: usize,
    audio_channels: Option<u32>,
    subtitle_tracks: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CameraRunInfo {
    model_name: Option<String>,
    lut_status: Option<String>,
}

struct SleepState {
    enabled: bool,
    persisted_enabled: bool,
    inhibitor: Option<keepawake::KeepAwake>,
}

#[derive(Clone)]
struct WatchedFolder {
    root: PathBuf,
    settings: QueueSettings,
    snapshot: Vec<(PathBuf, u64, Option<std::time::SystemTime>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FolderQueueSummary {
    root: PathBuf,
    mode: ContentMode,
    encoder: Encoder,
    folders: usize,
    files: usize,
    remaining: usize,
    converted: usize,
    failed: usize,
    active_status: Option<QueueStatus>,
}

#[derive(Default)]
struct TaskDisplayCounts {
    targets: usize,
    folders: usize,
    files: usize,
    remaining: usize,
    converted: usize,
    failed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryFileState {
    Remaining,
    Converted,
    Failed,
}

struct WorkerState {
    task_id: String,
    target: PathBuf,
    started_local: String,
    receiver: mpsc::Receiver<WorkerUpdate>,
    control: Arc<ConversionControl>,
    paused: bool,
    started_at: Instant,
    paused_at: Option<Instant>,
    paused_duration: Duration,
}

impl WorkerState {
    fn active_elapsed(&self) -> Duration {
        let current_pause = self.paused_at.map_or(Duration::ZERO, |at| at.elapsed());
        self.started_at
            .elapsed()
            .saturating_sub(self.paused_duration.saturating_add(current_pause))
    }
}

impl Default for ConverterApp {
    #[expect(
        clippy::too_many_lines,
        reason = "initialization keeps every recovered GUI field visible in one place"
    )]
    fn default() -> Self {
        let state_store = StateStore::system();
        let mut activity_messages = Vec::new();
        let preferences = match state_store.load_preferences() {
            Ok(Some(preferences)) => preferences,
            Ok(None) => AppPreferences::default(),
            Err(error) => {
                activity_messages.push(format!("Settings recovery skipped: {error}"));
                AppPreferences::default()
            }
        };
        let (queue, resume_task_id, next_id, task_run_failures) = match state_store.load_queue() {
            Ok(Some(loaded)) => {
                let count = loaded.queue.tasks().len();
                if count > 0 {
                    activity_messages.push(format!("Restored {count} queue item(s)"));
                }
                (
                    loaded.queue,
                    loaded.resume_task_id,
                    loaded.next_id,
                    loaded.task_run_failures,
                )
            }
            Ok(None) => (Queue::default(), None, 1, HashMap::new()),
            Err(error) => {
                activity_messages.push(format!("Queue recovery skipped: {error}"));
                (Queue::default(), None, 1, HashMap::new())
            }
        };
        let completed_history = match state_store.load_history() {
            Ok(rows) => rows,
            Err(error) => {
                activity_messages.push(format!("Completed history recovery skipped: {error}"));
                Vec::new()
            }
        };
        let mode = preferences.selected_mode;
        let prevent_system_sleep = preferences.prevent_system_sleep;
        let cpu_limit = preferences.cpu_limit;
        let cpu_limiter = ProcessCpuLimiter::new();
        let cpu_sampler = ProcessCpuSampler::new();
        let encoding_settings = preferences
            .settings_by_mode
            .values()
            .map(|settings| {
                (
                    (settings.mode, settings.encoder),
                    EncodingUiSettings::from(settings),
                )
            })
            .collect();
        let current = preferences
            .settings_by_mode
            .get(&mode)
            .cloned()
            .unwrap_or_else(|| default_settings(mode, Encoder::X265));
        let selected_id = queue.tasks().front().map(|task| task.id.clone());
        let selected_ids = selected_id.iter().cloned().collect();
        let watched_folders = restored_folder_watches(&queue);
        let folder_summaries = folder_queue_summaries(&watched_folders, &queue, &task_run_failures);
        Self {
            queue,
            selected_id,
            selected_ids,
            selection_anchor_id: None,
            next_id,
            mode,
            encoder: current.encoder,
            trim_start_text: format_trim_time(current.trim_start.unwrap_or_default()),
            trim_end_text: format_trim_time(current.trim_end.unwrap_or(Duration::from_secs(1))),
            stabilize_strength: current.stabilize_strength.clone(),
            apply_lut: current.apply_lut,
            slideshow_interval_seconds: current.photo_interval.as_secs_f32(),
            slideshow_fps: current.slideshow_fps,
            slideshow_resolution: if current.slideshow_resolution == (3840, 2160) {
                "4K".to_owned()
            } else {
                "1080p".to_owned()
            },
            slideshow_audio_paths: current.slideshow_audio_paths.clone(),
            slideshow_audio_selected: None,
            slideshow_collage: current.slideshow_collage,
            fps_mode: fps_ui_mode(current.fps),
            explicit_fps: explicit_fps(current.fps),
            quality_crf: current.quality.unwrap_or(28.0),
            speed_preset: current.speed_preset.clone().unwrap_or_default(),
            engine_status: "Initializing direct FFmpeg…".to_owned(),
            available_encoders: Vec::new(),
            engine_discovery: EngineDiscoveryState::NotStarted,
            activity: if activity_messages.is_empty() {
                "Ready".to_owned()
            } else {
                activity_messages.join(" · ")
            },
            completion_notice: String::new(),
            progress: None,
            worker: None,
            photo_review: None,
            photo_viewer: None,
            review_worker: None,
            next_review_request_id: 1,
            state_store,
            settings_by_mode: preferences.settings_by_mode,
            encoding_settings,
            persisted_mode: mode,
            resume_task_id,
            queue_run_state: QueueRunState::Idle,
            center_view: CenterView::Queue,
            completed_history,
            next_history_refresh: Instant::now() + Duration::from_secs(1),
            sleep: SleepState {
                enabled: prevent_system_sleep,
                persisted_enabled: prevent_system_sleep,
                inhibitor: None,
            },
            cpu_limit,
            persisted_cpu_limit: cpu_limit,
            cpu_limiter,
            cpu_sampler,
            process_cpu_usage_percent: None,
            next_cpu_usage_refresh: Instant::now(),
            watched_folders,
            folder_summaries,
            task_run_failures,
            task_run_start_converted: HashMap::new(),
            next_folder_refresh: Instant::now() + Duration::from_secs(2),
            task_name_edit: String::new(),
            task_name_edit_id: None,
            task_file_error_dialog: None,
            activity_log: Vec::new(),
            logged_activity: String::new(),
            frame_preview_enabled: false,
            live_preview_texture: None,
            live_preview: None,
            active_source_info: None,
            active_stream_info: None,
            active_camera_info: CameraRunInfo::default(),
            close_state: CloseState::Open,
            platform_indicator: PlatformIndicator::default(),
        }
    }
}

#[cfg(feature = "native-ffmpeg")]
fn engine_details() -> (String, Vec<Encoder>) {
    let Ok(engine) = videoferry_ffmpeg::NativeEngine::new() else {
        return ("Direct FFmpeg initialization failed".to_owned(), Vec::new());
    };
    let status = engine
        .version_summary()
        .unwrap_or_else(|error| error.to_string());
    let encoders = engine.capabilities().map_or_else(
        |_| Vec::new(),
        |capabilities| {
            Encoder::ALL
                .into_iter()
                .filter(|encoder| {
                    capabilities
                        .encoders
                        .iter()
                        .any(|name| name == encoder.library_name())
                })
                .collect()
        },
    );
    (status, encoders)
}

#[cfg(not(feature = "native-ffmpeg"))]
fn engine_details() -> (String, Vec<Encoder>) {
    let engine = videoferry_ffmpeg::UnavailableEngine;
    (
        engine
            .version_summary()
            .unwrap_or_else(|error| error.to_string()),
        Vec::new(),
    )
}

impl eframe::App for ConverterApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.handle_close_request(&context);
        self.poll_engine_discovery(&context);
        self.poll_worker(&context);
        self.refresh_process_cpu_usage();
        self.poll_review_worker(&context);
        if self.close_state == CloseState::WaitingForWorker && self.worker.is_none() {
            self.close_state = CloseState::Open;
            context.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.worker.is_none() && self.resume_task_id.take().is_some() {
            "Resuming persisted queue".clone_into(&mut self.activity);
            self.start_queue(&context);
        }
        self.handle_dropped_paths(&context);
        self.refresh_folder_watches();
        self.refresh_completed_history(&context);
        self.capture_activity_log(&context);
        let title = if self.worker.is_some() {
            "● VideoFerry [Converting]"
        } else {
            "VideoFerry"
        };
        self.platform_indicator
            .set_converting(self.worker.is_some());
        context.send_viewport_cmd(egui::ViewportCommand::Title(title.to_owned()));
        egui::Panel::top("toolbar").show(ui, |ui| self.show_toolbar(ui));
        egui::Panel::right("settings")
            .default_size(300.0)
            .show(ui, |ui| self.show_settings(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.show_status(ui));
        egui::CentralPanel::default().show(ui, |ui| self.show_center(ui));
        self.show_photo_review(&context);
        self.show_photo_viewer(&context);
        self.update_sleep_inhibitor();
        self.sync_preferences();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(worker) = &self.worker {
            worker.control.stop_all();
        }
        self.sleep.inhibitor = None;
        self.platform_indicator.set_converting(false);
        self.sync_preferences();
        let _ = self.state_store.save_queue(
            &self.queue,
            self.worker.is_some() || self.queue_run_state != QueueRunState::Idle,
            &self.task_run_failures,
        );
    }
}

impl ConverterApp {
    fn poll_engine_discovery(&mut self, context: &egui::Context) {
        if matches!(&self.engine_discovery, EngineDiscoveryState::NotStarted) {
            let (sender, receiver) = mpsc::channel();
            let repaint_context = context.clone();
            std::thread::spawn(move || {
                let _ = sender.send(engine_details());
                repaint_context.request_repaint();
            });
            self.engine_discovery = EngineDiscoveryState::Running(receiver);
        }
        let discovery = match &self.engine_discovery {
            EngineDiscoveryState::Running(receiver) => receiver.try_recv(),
            EngineDiscoveryState::NotStarted | EngineDiscoveryState::Complete => return,
        };
        match discovery {
            Ok((status, encoders)) => {
                self.engine_status = status;
                self.available_encoders = encoders;
                self.engine_discovery = EngineDiscoveryState::Complete;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                "Direct FFmpeg capability discovery stopped unexpectedly"
                    .clone_into(&mut self.engine_status);
                self.engine_discovery = EngineDiscoveryState::Complete;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn select_only(&mut self, id: Option<String>) {
        self.selected_ids.clear();
        if let Some(id) = id {
            self.selected_ids.insert(id.clone());
            self.selection_anchor_id = Some(id.clone());
            self.selected_id = Some(id);
        } else {
            self.selection_anchor_id = None;
            self.selected_id = None;
        }
    }

    fn selected_task_ids(&self) -> Vec<String> {
        self.queue
            .tasks()
            .iter()
            .filter(|task| self.selected_ids.contains(&task.id))
            .map(|task| task.id.clone())
            .collect()
    }

    fn apply_task_selection(&mut self, id: String, modifiers: egui::Modifiers) {
        let task_ids = self
            .queue
            .tasks()
            .iter()
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        update_extended_selection(
            &task_ids,
            id,
            modifiers,
            &mut self.selected_id,
            &mut self.selected_ids,
            &mut self.selection_anchor_id,
        );
    }

    fn handle_close_request(&mut self, context: &egui::Context) {
        if !context.input(|input| input.viewport().close_requested()) || self.worker.is_none() {
            return;
        }
        context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if let Some(worker) = &self.worker {
            worker.control.stop_all();
        }
        self.queue_run_state = QueueRunState::Stopping;
        self.close_state = CloseState::WaitingForWorker;
        "Stopping active conversion before closing".clone_into(&mut self.activity);
        self.persist_queue();
    }

    fn refresh_completed_history(&mut self, context: &egui::Context) {
        let now = Instant::now();
        if now < self.next_history_refresh {
            context.request_repaint_after(self.next_history_refresh - now);
            return;
        }
        self.next_history_refresh = now + Duration::from_secs(1);
        match self.state_store.load_history() {
            Ok(rows) => self.completed_history = rows,
            Err(error) => self.activity = format!("Unable to refresh completed history: {error}"),
        }
        context.request_repaint_after(Duration::from_secs(1));
    }

    fn capture_activity_log(&mut self, context: &egui::Context) {
        if self.activity.is_empty() || self.activity == self.logged_activity {
            return;
        }
        self.logged_activity.clone_from(&self.activity);
        let timestamp = jiff::Zoned::now().strftime("%H:%M:%S");
        self.activity_log
            .push(format!("[{timestamp}] {}", self.activity));
        if self.activity_log.len() > 2_000 {
            self.activity_log.drain(..1_000);
        }
        context.request_repaint();
    }

    fn refresh_folder_watches(&mut self) {
        let now = Instant::now();
        if now < self.next_folder_refresh {
            return;
        }
        self.next_folder_refresh = now + Duration::from_secs(2);
        let added = self.rescan_watched_folders(false);
        if added > 0 {
            self.activity = format!("Folder watch found new work in {added} queue task(s)");
            self.persist_queue();
        }
        self.refresh_folder_summaries();
    }

    fn refresh_process_cpu_usage(&mut self) {
        let now = Instant::now();
        if self.worker.is_none() {
            self.cpu_sampler.reset();
            self.process_cpu_usage_percent = None;
            self.next_cpu_usage_refresh = now;
            return;
        }
        if now < self.next_cpu_usage_refresh {
            return;
        }
        self.next_cpu_usage_refresh = now + Duration::from_secs(1);
        self.process_cpu_usage_percent = self.cpu_sampler.sample_percent();
    }

    fn refresh_folder_summaries(&mut self) {
        self.folder_summaries =
            folder_queue_summaries(&self.watched_folders, &self.queue, &self.task_run_failures);
    }

    fn rescan_watched_folders(&mut self, force: bool) -> usize {
        let existing = self
            .queue
            .tasks()
            .iter()
            .flat_map(|task| task.targets.iter().cloned())
            .collect::<HashSet<_>>();
        let active_roots = self.watched_folders.clone();
        let mut additions = Vec::new();
        let mut reactivated = 0_usize;
        for watch in active_roots {
            let snapshot = folder_snapshot_for_settings(&watch.root, &watch.settings);
            if !force && snapshot == watch.snapshot {
                continue;
            }
            if let Some(stored) = self
                .watched_folders
                .iter_mut()
                .find(|stored| stored.root == watch.root)
            {
                stored.snapshot = snapshot;
            }
            if watch.settings.mode == ContentMode::PhotoSlideshow {
                // A slideshow folder remains one queue task. Its image list is
                // collected from the folder when the task starts, matching the
                // Python app's rescan behavior instead of enqueueing each photo.
                continue;
            }
            let aggregate_task = self
                .queue
                .tasks()
                .iter()
                .find(|task| task.targets.iter().any(|target| target == &watch.root))
                .map(|task| (task.id.clone(), task.status));
            if let Some((task_id, status)) = aggregate_task {
                let has_remaining = self.queue.task(&task_id).is_some_and(|task| {
                    !remaining_task_inputs(task, self.task_run_failures.get(&task_id)).is_empty()
                });
                if status == QueueStatus::Completed && has_remaining {
                    let _ = self.queue.set_status(&task_id, QueueStatus::Pending);
                    let _ = self.queue.set_error(&task_id, None);
                    if let Some(task) = self.queue.task_mut(&task_id) {
                        task.complete_time.clear();
                    }
                    reactivated += 1;
                }
                continue;
            }
            for path in collect_video_files(&watch.root) {
                if !existing.contains(&path) && !should_skip_queue_source(&path, &watch.settings) {
                    additions.push((path, watch.settings.clone(), watch.root.clone()));
                }
            }
        }
        additions.sort_by(|left, right| left.0.cmp(&right.0));
        additions.dedup_by(|left, right| left.0 == right.0);
        let count = additions.len() + reactivated;
        for (path, settings, root) in additions {
            self.enqueue_media_file(path, settings, Some(root));
        }
        count
    }

    fn update_sleep_inhibitor(&mut self) {
        let should_inhibit = should_inhibit_sleep(
            self.sleep.enabled,
            self.worker.is_some(),
            self.worker.as_ref().is_some_and(|worker| worker.paused),
        );
        if should_inhibit && self.sleep.inhibitor.is_none() {
            match keepawake::Builder::default()
                .idle(true)
                .reason("VideoFerry is processing media")
                .app_name("VideoFerry")
                .app_reverse_domain("io.github.infiz.videoferry")
                .create()
            {
                Ok(inhibitor) => self.sleep.inhibitor = Some(inhibitor),
                Err(error) => {
                    self.sleep.enabled = false;
                    self.activity = format!("Unable to prevent system sleep: {error}");
                }
            }
        } else if !should_inhibit {
            self.sleep.inhibitor = None;
        }
    }

    fn handle_dropped_paths(&mut self, context: &egui::Context) {
        let paths = context.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_owned())
                .collect::<Vec<_>>()
        });
        if !paths.is_empty() {
            self.add_paths(paths);
        }
    }

    fn show_toolbar(&mut self, ui: &mut egui::Ui) {
        let selected_task_ids = self.selected_task_ids();
        let selected_count = selected_task_ids.len();
        let selected_status = self
            .selected_id
            .as_deref()
            .and_then(|id| self.queue.task(id))
            .map(|task| task.status);
        let selected_is_pending =
            selected_count == 1 && selected_status == Some(QueueStatus::Pending);
        let any_rerunnable = selected_task_ids.iter().any(|id| {
            self.queue.task(id).is_some_and(|task| {
                matches!(
                    task.status,
                    QueueStatus::Completed | QueueStatus::Failed | QueueStatus::Cancelled
                )
            })
        });
        ui.horizontal(|ui| {
            ui.heading("VideoFerry");
            ui.separator();
            if ui.button("Add files").clicked() {
                let dialog = if self.mode == ContentMode::PhotoSlideshow {
                    rfd::FileDialog::new().add_filter(
                        "Photo files",
                        &["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff"],
                    )
                } else {
                    rfd::FileDialog::new().add_filter(
                        "Video files",
                        &["mkv", "mp4", "mov", "avi", "wmv", "flv", "rm", "rmvb"],
                    )
                };
                if let Some(paths) = dialog.pick_files() {
                    self.add_paths(paths);
                }
            }
            if ui
                .add_enabled(
                    self.mode != ContentMode::Trim,
                    egui::Button::new("Add folder"),
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new().pick_folder()
            {
                self.add_paths(vec![path]);
            }
            self.show_queue_edit_controls(ui, selected_status, selected_count, any_rerunnable);
            let can_review = self.selected_id.as_deref().is_some_and(|id| {
                self.queue
                    .task(id)
                    .is_some_and(|task| task.settings.mode == ContentMode::PhotoSlideshow)
            }) && selected_count == 1
                && self.worker.is_none();
            if ui
                .add_enabled(can_review, egui::Button::new("Review photos"))
                .clicked()
            {
                self.open_photo_review(ui.ctx());
            }
            ui.separator();
            if ui
                .add_enabled(
                    self.worker.is_none()
                        && self.queue_run_state == QueueRunState::Idle
                        && self.queue.next_pending_id().is_some(),
                    egui::Button::new("Run queue"),
                )
                .clicked()
            {
                self.start_queue(ui.ctx());
            }
            if ui
                .add_enabled(
                    selected_is_pending
                        && self.worker.is_none()
                        && self.queue_run_state == QueueRunState::Idle,
                    egui::Button::new("Run selected"),
                )
                .clicked()
            {
                self.queue_run_state = QueueRunState::RunningSelected;
                let _ = self.start_selected(ui.ctx());
            }
            self.show_worker_controls(ui);
        });
    }

    fn show_queue_edit_controls(
        &mut self,
        ui: &mut egui::Ui,
        selected_status: Option<QueueStatus>,
        selected_count: usize,
        any_rerunnable: bool,
    ) {
        let pending = selected_count == 1 && selected_status == Some(QueueStatus::Pending);
        let selected = selected_count > 0;
        if ui
            .add_enabled(pending, egui::Button::new("Move up"))
            .clicked()
        {
            self.move_selected(-1);
        }
        if ui
            .add_enabled(pending, egui::Button::new("Move down"))
            .clicked()
        {
            self.move_selected(1);
        }
        if ui
            .add_enabled(selected, egui::Button::new("Remove selected"))
            .clicked()
        {
            self.remove_selected();
        }
        if ui
            .add_enabled(any_rerunnable, egui::Button::new("Retry / rerun selected"))
            .clicked()
        {
            self.rerun_selected();
        }
        if ui
            .add_enabled(
                self.worker.is_none() && self.queue_run_state == QueueRunState::Idle,
                egui::Button::new("Clear queue"),
            )
            .clicked()
        {
            self.clear_queue();
        }
    }

    fn show_worker_controls(&mut self, ui: &mut egui::Ui) {
        if self.show_paused_between_files_controls(ui) {
            return;
        }
        let mut worker_status = None;
        let mut stop_current = false;
        let mut stop_all = false;
        if let Some(worker) = &mut self.worker {
            let pause_label = if worker.paused { "Resume" } else { "Pause" };
            if ui.button(pause_label).clicked() {
                if worker.paused {
                    worker.control.resume();
                    if let Some(paused_at) = worker.paused_at.take() {
                        worker.paused_duration += paused_at.elapsed();
                    }
                } else {
                    worker.control.pause();
                    worker.paused_at = Some(Instant::now());
                }
                worker.paused = !worker.paused;
                worker_status = Some((
                    worker.task_id.clone(),
                    if worker.paused {
                        QueueStatus::Paused
                    } else {
                        QueueStatus::Running
                    },
                ));
            }
            let pause_scheduled = matches!(
                self.queue_run_state,
                QueueRunState::PauseAfterCurrent | QueueRunState::PauseAfterCurrentSelected
            );
            if ui
                .add_enabled(
                    matches!(
                        self.queue_run_state,
                        QueueRunState::Running
                            | QueueRunState::RunningSelected
                            | QueueRunState::PauseAfterCurrent
                            | QueueRunState::PauseAfterCurrentSelected
                    ),
                    egui::Button::new(if pause_scheduled {
                        "✓ Pause scheduled"
                    } else {
                        "Pause after this file"
                    })
                    .selected(pause_scheduled),
                )
                .clicked()
            {
                if pause_scheduled {
                    self.queue_run_state = match self.queue_run_state {
                        QueueRunState::PauseAfterCurrentSelected => QueueRunState::RunningSelected,
                        _ => QueueRunState::Running,
                    };
                    "Scheduled pause cancelled".clone_into(&mut self.activity);
                } else {
                    self.queue_run_state = if self.queue_run_state == QueueRunState::RunningSelected
                    {
                        QueueRunState::PauseAfterCurrentSelected
                    } else {
                        QueueRunState::PauseAfterCurrent
                    };
                    "Queue will pause after the current item".clone_into(&mut self.activity);
                }
            }
            if ui.button("Stop current").clicked() {
                worker.control.stop_current();
                stop_current = true;
            }
            if ui.button("Stop all").clicked() {
                worker.control.stop_all();
                stop_all = true;
            }
        }
        if let Some((task_id, status)) = worker_status {
            let _ = self.queue.set_status(&task_id, status);
            self.persist_queue();
        }
        if stop_all {
            self.queue_run_state = QueueRunState::Stopping;
            "Stopping the queue; the current item will remain pending"
                .clone_into(&mut self.activity);
            self.persist_queue();
        } else if stop_current {
            "Stopping the current item; queued work will continue".clone_into(&mut self.activity);
            self.persist_queue();
        }
    }

    fn show_paused_between_files_controls(&mut self, ui: &mut egui::Ui) -> bool {
        if !matches!(
            self.queue_run_state,
            QueueRunState::PausedBetweenFiles | QueueRunState::PausedBetweenFilesSelected
        ) {
            return false;
        }
        if ui.add(egui::Button::new("Resume").selected(true)).clicked() {
            if self.queue_run_state == QueueRunState::PausedBetweenFilesSelected {
                self.queue_run_state = QueueRunState::RunningSelected;
                "Task resumed".clone_into(&mut self.activity);
                self.persist_queue();
                let _ = self.start_selected(ui.ctx());
            } else {
                self.queue_run_state = QueueRunState::Running;
                "Queue resumed".clone_into(&mut self.activity);
                self.persist_queue();
                self.start_next_pending(ui.ctx());
            }
        }
        ui.add_enabled(false, egui::Button::new("Pause after this file"));
        if ui.button("Stop all").clicked() {
            self.queue_run_state = QueueRunState::Idle;
            "Queue stopped; queued files remain".clone_into(&mut self.activity);
            self.persist_queue();
        }
        true
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Conversion settings");
        ui.add_space(8.0);
        let old_mode = self.mode;
        egui::ComboBox::from_label("Workflow")
            .selected_text(self.mode.label())
            .show_ui(ui, |ui| {
                for mode in ContentMode::ALL {
                    ui.selectable_value(&mut self.mode, mode, mode.label());
                }
            });
        if self.mode != old_mode {
            self.remember_encoding_settings(old_mode, self.encoder);
            let previous = self.settings_from_ui(old_mode, self.encoder);
            self.settings_by_mode.insert(old_mode, previous);
            let next = self
                .settings_by_mode
                .get(&self.mode)
                .cloned()
                .unwrap_or_else(|| default_settings(self.mode, Encoder::X265));
            self.apply_settings_to_ui(&next);
            if self.mode == ContentMode::Trim {
                self.encoder = Encoder::X265;
            }
        }
        let old_encoder = self.encoder;
        let allowed_encoders = self.allowed_encoders();
        if !allowed_encoders.contains(&self.encoder) {
            self.encoder = allowed_encoders[0];
        }
        if workflow_shows_encoder(self.mode) {
            egui::ComboBox::from_label("Encoder")
                .selected_text(self.encoder.user_name())
                .show_ui(ui, |ui| {
                    for encoder in self.allowed_encoders() {
                        ui.selectable_value(&mut self.encoder, encoder, encoder.user_name());
                    }
                });
        }
        if self.encoder != old_encoder {
            self.remember_encoding_settings(self.mode, old_encoder);
            let next = encoding_ui_settings_for(&self.encoding_settings, self.mode, self.encoder);
            self.apply_quality_settings_to_ui(&next);
        }
        self.show_encoding_settings(ui);
        ui.label("Audio: Copy valid streams");
        ui.label(format!(
            "Metadata: {:?}",
            default_settings(self.mode, self.encoder).metadata
        ));
        if self.mode == ContentMode::Trim {
            ui.separator();
            ui.label("Trim range (MM:SS or HH:MM:SS, inclusive)");
            ui.horizontal(|ui| {
                let start_label = ui.label("Start");
                ui.add(egui::TextEdit::singleline(&mut self.trim_start_text).desired_width(82.0))
                    .labelled_by(start_label.id);
                let end_label = ui.label("End");
                ui.add(egui::TextEdit::singleline(&mut self.trim_end_text).desired_width(82.0))
                    .labelled_by(end_label.id);
            });
            if let Err(error) = self.trim_range_from_ui() {
                ui.colored_label(egui::Color32::RED, error);
            }
        } else if self.mode == ContentMode::CameraVideos && !self.encoder.is_hardware() {
            ui.separator();
            ui.checkbox(&mut self.apply_lut, "Apply matching DJI LUT");
            ui.weak("Provided LUT map:");
            for profile in dji_camera_profiles() {
                ui.weak(format!("{} → {}", profile.model_name, profile.lut_name));
            }
        } else if self.mode == ContentMode::Stabilize {
            ui.separator();
            egui::ComboBox::from_label("Strength")
                .selected_text(&self.stabilize_strength)
                .show_ui(ui, |ui| {
                    for strength in ["Gentle", "Balanced", "Steady", "Strong", "Maximum"] {
                        ui.selectable_value(
                            &mut self.stabilize_strength,
                            strength.to_owned(),
                            strength,
                        );
                    }
                });
            ui.weak("Uses native two-pass stabilization when available.");
        } else if self.mode == ContentMode::PhotoSlideshow {
            self.show_slideshow_settings(ui);
        }
        ui.add_space(16.0);
        ui.checkbox(
            &mut self.sleep.enabled,
            "Prevent system sleep while converting",
        );
        ui.weak("The display may still turn off normally.");
        ui.add_space(8.0);
        ui.weak("Drop files anywhere in this window to add them to the queue.");
    }

    fn show_encoding_settings(&mut self, ui: &mut egui::Ui) {
        if self.mode == ContentMode::Trim {
            return;
        }
        if workflow_shows_fps(self.mode) {
            if !descriptor(self.mode, self.encoder).share_lowest_fps
                && self.fps_mode == FpsUiMode::SharedLowest
            {
                self.fps_mode = FpsUiMode::Source;
            }
            egui::ComboBox::from_label("Frame rate")
                .selected_text(self.fps_mode.label())
                .show_ui(ui, |ui| {
                    if descriptor(self.mode, self.encoder).share_lowest_fps {
                        ui.selectable_value(
                            &mut self.fps_mode,
                            FpsUiMode::SharedLowest,
                            FpsUiMode::SharedLowest.label(),
                        );
                    }
                    ui.selectable_value(
                        &mut self.fps_mode,
                        FpsUiMode::Source,
                        FpsUiMode::Source.label(),
                    );
                    ui.selectable_value(
                        &mut self.fps_mode,
                        FpsUiMode::Explicit,
                        FpsUiMode::Explicit.label(),
                    );
                });
            if self.fps_mode == FpsUiMode::Explicit {
                ui.horizontal(|ui| {
                    let label = ui.label("FPS");
                    ui.add(
                        egui::DragValue::new(&mut self.explicit_fps)
                            .range(f64::MIN_POSITIVE..=f64::MAX)
                            .speed(0.001)
                            .max_decimals(3),
                    )
                    .labelled_by(label.id);
                });
            }
        }

        if matches!(
            self.encoder,
            Encoder::X264 | Encoder::X265 | Encoder::SvtAv1
        ) {
            let quality_scale = quality_scale(self.encoder);
            ui.horizontal(|ui| {
                let label = ui.label("Quality (CRF)");
                ui.add(
                    egui::DragValue::new(&mut self.quality_crf)
                        .range(quality_scale.minimum..=quality_scale.maximum)
                        .speed(f64::from(quality_scale.step)),
                )
                .labelled_by(label.id);
            });
        }
        if matches!(self.encoder, Encoder::X264 | Encoder::X265) {
            egui::ComboBox::from_label("Encoding speed")
                .selected_text(&self.speed_preset)
                .show_ui(ui, |ui| {
                    for preset in [
                        "ultrafast",
                        "superfast",
                        "veryfast",
                        "faster",
                        "fast",
                        "medium",
                        "slow",
                        "slower",
                        "veryslow",
                        "placebo",
                    ] {
                        ui.selectable_value(&mut self.speed_preset, preset.to_owned(), preset);
                    }
                });
        } else if self.encoder == Encoder::SvtAv1 {
            egui::ComboBox::from_label("Encoding speed")
                .selected_text(&self.speed_preset)
                .show_ui(ui, |ui| {
                    for preset in 0..=13 {
                        let preset = preset.to_string();
                        ui.selectable_value(&mut self.speed_preset, preset.clone(), preset);
                    }
                });
        } else if self.encoder.is_nvenc() {
            self.show_nvenc_preset_settings(ui);
        } else if self.encoder.is_videotoolbox() {
            ui.weak("Quality is managed by VideoToolbox.");
        }
    }

    fn show_nvenc_preset_settings(&mut self, ui: &mut egui::Ui) {
        let options = speed_options(self.encoder);
        let labels = speed_option_labels(self.encoder);
        let selected = options
            .iter()
            .position(|preset| preset == &self.speed_preset)
            .and_then(|index| labels.get(index).cloned())
            .unwrap_or_else(|| "P4 · Medium (default)".to_owned());
        egui::ComboBox::from_label("NVENC preset")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for (preset, label) in options.into_iter().zip(labels) {
                    ui.selectable_value(&mut self.speed_preset, preset, label);
                }
            });
        ui.weak(speed_help(self.encoder));
    }

    fn show_slideshow_settings(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        let interval_label = ui.label("Photo interval (seconds)");
        ui.add(
            egui::DragValue::new(&mut self.slideshow_interval_seconds)
                .range(f32::MIN_POSITIVE..=9.0e18_f32)
                .speed(0.1),
        )
        .labelled_by(interval_label.id);
        let fps_label = ui.label("Frames per second");
        ui.add(
            egui::DragValue::new(&mut self.slideshow_fps)
                .range(1..=u32::MAX)
                .speed(1),
        )
        .labelled_by(fps_label.id);
        egui::ComboBox::from_label("Resolution")
            .selected_text(&self.slideshow_resolution)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut self.slideshow_resolution,
                    "1080p".to_owned(),
                    "1080p (1920×1080)",
                );
                ui.selectable_value(
                    &mut self.slideshow_resolution,
                    "4K".to_owned(),
                    "4K (3840×2160)",
                );
            });
        ui.checkbox(
            &mut self.slideshow_collage,
            "Group portrait photos into collage slides",
        );
        self.show_slideshow_audio(ui);
        ui.weak("Output: slideshow.mp4 in the selected folder or beside the first image.");
        ui.weak("Select two or more photos, or add a folder recursively.");
    }

    fn show_slideshow_audio(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Add audio").clicked()
                && let Some(paths) = rfd::FileDialog::new()
                    .add_filter(
                        "Audio files",
                        &["mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "wma"],
                    )
                    .pick_files()
            {
                for path in paths {
                    if !self.slideshow_audio_paths.contains(&path) {
                        self.slideshow_audio_paths.push(path);
                    }
                }
                self.slideshow_audio_selected = self.slideshow_audio_paths.len().checked_sub(1);
            }
            if ui
                .add_enabled(
                    self.slideshow_audio_selected.is_some(),
                    egui::Button::new("Remove"),
                )
                .clicked()
                && let Some(index) = self.slideshow_audio_selected
            {
                self.slideshow_audio_paths.remove(index);
                self.slideshow_audio_selected = (!self.slideshow_audio_paths.is_empty())
                    .then(|| index.min(self.slideshow_audio_paths.len() - 1));
            }
            let can_move_up = self.slideshow_audio_selected.is_some_and(|index| index > 0);
            if ui
                .add_enabled(can_move_up, egui::Button::new("Up"))
                .clicked()
                && let Some(index) = self.slideshow_audio_selected
            {
                self.slideshow_audio_paths.swap(index, index - 1);
                self.slideshow_audio_selected = Some(index - 1);
            }
            let can_move_down = self
                .slideshow_audio_selected
                .is_some_and(|index| index + 1 < self.slideshow_audio_paths.len());
            if ui
                .add_enabled(can_move_down, egui::Button::new("Down"))
                .clicked()
                && let Some(index) = self.slideshow_audio_selected
            {
                self.slideshow_audio_paths.swap(index, index + 1);
                self.slideshow_audio_selected = Some(index + 1);
            }
            if ui
                .add_enabled(
                    !self.slideshow_audio_paths.is_empty(),
                    egui::Button::new("Clear"),
                )
                .clicked()
            {
                self.slideshow_audio_paths.clear();
                self.slideshow_audio_selected = None;
            }
        });
        if self.slideshow_audio_paths.is_empty() {
            ui.label("Audio: none");
        } else {
            ui.label("Audio order (concatenated, then looped):");
            for (index, path) in self.slideshow_audio_paths.iter().enumerate() {
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("Audio track");
                if ui
                    .selectable_label(self.slideshow_audio_selected == Some(index), name)
                    .clicked()
                {
                    self.slideshow_audio_selected = Some(index);
                }
            }
        }
    }

    fn open_photo_review(&mut self, context: &egui::Context) {
        let Some(task_id) = self.selected_id.clone() else {
            return;
        };
        let Some(task) = self.queue.task(&task_id).cloned() else {
            return;
        };
        if task.settings.mode != ContentMode::PhotoSlideshow {
            return;
        }
        let had_review_order = !task.settings.slideshow_review_image_paths.is_empty();
        let paths = if had_review_order {
            task.settings.slideshow_review_image_paths.clone()
        } else if !task.settings.slideshow_image_paths.is_empty() {
            task.settings.slideshow_image_paths.clone()
        } else {
            task.targets
                .first()
                .map_or_else(Vec::new, |root| collect_photo_files(root))
        };
        if paths.is_empty() {
            "No supported photos were found for this slideshow.".clone_into(&mut self.activity);
            return;
        }
        let selected = task
            .settings
            .slideshow_image_paths
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let display_counts = Self::task_display_counts(
            &task,
            &self.folder_summaries,
            self.task_run_failures.get(&task.id),
        );
        let editable = slideshow_review_is_editable(&task, display_counts.converted);
        let photos = paths
            .into_iter()
            .map(|path| ReviewPhoto {
                included: !had_review_order || selected.contains(&path),
                path,
            })
            .collect();
        self.photo_review = Some(PhotoReviewState {
            task_id,
            photos,
            editable,
            selected: Some(0),
            open: true,
            preview_path: None,
            preview_texture: None,
            preview_error: None,
            preview_request_id: None,
            thumbnail_textures: HashMap::new(),
            thumbnail_request_ids: HashMap::new(),
            thumbnail_failures: HashSet::new(),
            tab: PhotoReviewTab::OriginalPhotos,
            slide_groups: Vec::new(),
            slide_selected: None,
            slide_preview_paths: Vec::new(),
            slide_preview_texture: None,
            slide_error: None,
            slide_thumbnail_textures: HashMap::new(),
            slide_thumbnail_request_ids: HashMap::new(),
            slide_thumbnail_failures: HashSet::new(),
            slide_groups_request_id: None,
            slide_preview_request_id: None,
            slides_dirty: true,
        });
        self.refresh_photo_review_preview(context);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the review modal keeps its list actions and commit boundary together"
    )]
    fn show_photo_review(&mut self, context: &egui::Context) {
        let Some(review) = self.photo_review.as_mut() else {
            return;
        };
        let mut window_open = review.open;
        let mut cancel = false;
        let mut apply = None;
        let mut refresh_preview = false;
        let mut refresh_slide_groups = false;
        let mut refresh_slide_preview = false;
        let mut open_full_size = None;
        let mut schedule_thumbnails = Vec::new();
        let mut schedule_slide_thumbnails = Vec::new();
        egui::Window::new("Review slideshow photos")
            .open(&mut window_open)
            .default_size([760.0, 560.0])
            .min_size([560.0, 400.0])
            .show(context, |ui| {
                let included_count = review.photos.iter().filter(|photo| photo.included).count();
                ui.label(format!(
                    "{included_count} of {} photos included. The order below is the slideshow order.",
                    review.photos.len()
                ));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(review.editable, egui::Button::new("Include all"))
                        .clicked()
                    {
                        for photo in &mut review.photos {
                            photo.included = true;
                        }
                        review.slides_dirty = true;
                        review.slide_groups_request_id = None;
                    }
                    if ui
                        .add_enabled(review.editable, egui::Button::new("Exclude all"))
                        .clicked()
                    {
                        for photo in &mut review.photos {
                            photo.included = false;
                        }
                        review.slides_dirty = true;
                        review.slide_groups_request_id = None;
                    }
                    ui.separator();
                    let can_move_up =
                        review.editable && review.selected.is_some_and(|index| index > 0);
                    if ui
                        .add_enabled(can_move_up, egui::Button::new("Move earlier"))
                        .clicked()
                        && let Some(index) = review.selected
                    {
                        review.photos.swap(index, index - 1);
                        review.selected = Some(index - 1);
                        review.slides_dirty = true;
                        review.slide_groups_request_id = None;
                    }
                    let can_move_down = review.editable
                        && review
                        .selected
                        .is_some_and(|index| index + 1 < review.photos.len());
                    if ui
                        .add_enabled(can_move_down, egui::Button::new("Move later"))
                        .clicked()
                        && let Some(index) = review.selected
                    {
                        review.photos.swap(index, index + 1);
                        review.selected = Some(index + 1);
                        review.slides_dirty = true;
                        review.slide_groups_request_id = None;
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut review.tab,
                        PhotoReviewTab::OriginalPhotos,
                        "Original photos",
                    );
                    if ui
                        .selectable_value(&mut review.tab, PhotoReviewTab::Slides, "Slides")
                        .clicked()
                        && review.slides_dirty
                    {
                        refresh_slide_groups = true;
                    }
                });
                ui.separator();
                if review.tab == PhotoReviewTab::OriginalPhotos {
                    review_preview_panel(
                        ui,
                        review.preview_texture.as_ref(),
                        review.preview_error.as_deref(),
                        "Selected source photo preview",
                    );
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show_rows(ui, 68.0, review.photos.len(), |ui, visible_rows| {
                            let mut reorder = None;
                            for index in visible_rows {
                                let photo = &mut review.photos[index];
                                let drag_id = ui.make_persistent_id((
                                    "slideshow-review-photo",
                                    photo.path.as_os_str(),
                                ));
                                let (drop_zone, dropped) = ui.dnd_drop_zone::<usize, _>(
                                    egui::Frame::NONE,
                                    |ui| {
                                        ui.horizontal(|ui| {
                                            if review.editable {
                                                ui.dnd_drag_source(drag_id, index, |ui| {
                                                    ui.label("⠿").on_hover_text(
                                                        "Drag to change slideshow order",
                                                    );
                                                });
                                            } else {
                                                ui.weak("⠿").on_hover_text(
                                                    "Drag to change slideshow order",
                                                );
                                            }
                                                let name = photo
                                                    .path
                                                    .file_name()
                                                    .and_then(|value| value.to_str())
                                                    .unwrap_or("Photo");
                                                let include = ui
                                                    .add_enabled_ui(review.editable, |ui| {
                                                        ui.checkbox(&mut photo.included, "")
                                                    })
                                                    .inner;
                                                include.widget_info(|| {
                                                    egui::WidgetInfo::selected(
                                                        egui::WidgetType::Checkbox,
                                                        true,
                                                        photo.included,
                                                        format!("Include {name}"),
                                                    )
                                                });
                                                if include.changed() {
                                                    review.slides_dirty = true;
                                                    review.slide_groups_request_id = None;
                                                }
                                                let selected = review.selected == Some(index);
                                                let select_response = ui
                                                    .selectable_label(
                                                        selected,
                                                        format!("{:>4}. {name}", index + 1),
                                                    )
                                                    .on_hover_text(
                                                        photo.path.display().to_string(),
                                                    );
                                                if select_response.double_clicked() {
                                                    review.selected = Some(index);
                                                    open_full_size = Some(photo.path.clone());
                                                } else if select_response.clicked() {
                                                    review.selected = Some(index);
                                                    refresh_preview = true;
                                                }
                                                if let Some(texture) =
                                                    review.thumbnail_textures.get(&photo.path)
                                                {
                                                    let size = texture.size_vec2();
                                                    let scale = (96.0 / size.x)
                                                        .min(64.0 / size.y)
                                                        .min(1.0);
                                                    ui.add(
                                                        egui::Image::new(texture)
                                                            .fit_to_exact_size(size * scale)
                                                            .alt_text(format!(
                                                                "Thumbnail of {name}"
                                                            )),
                                                    );
                                                } else {
                                                    ui.allocate_ui_with_layout(
                                                        egui::vec2(96.0, 64.0),
                                                        egui::Layout::centered_and_justified(
                                                            egui::Direction::LeftToRight,
                                                        ),
                                                        |ui| {
                                                            if review
                                                                .thumbnail_failures
                                                                .contains(&photo.path)
                                                            {
                                                                ui.weak("Unavailable");
                                                            } else {
                                                                ui.spinner();
                                                            }
                                                        },
                                                    );
                                                    if !review
                                                        .thumbnail_request_ids
                                                        .contains_key(&photo.path)
                                                        && !review
                                                            .thumbnail_failures
                                                            .contains(&photo.path)
                                                    {
                                                        schedule_thumbnails
                                                            .push(photo.path.clone());
                                                    }
                                                }
                                        });
                                    },
                                );
                                if review.editable && let Some(source) = dropped {
                                    let insertion = context.pointer_interact_pos().map_or(
                                        index,
                                        |pointer| {
                                            if pointer.y < drop_zone.response.rect.center().y {
                                                index
                                            } else {
                                                index + 1
                                            }
                                        },
                                    );
                                    reorder = Some((*source, insertion));
                                }
                            }
                            if let Some((source, insertion)) = reorder
                                && let Some(new_index) =
                                    move_item_to_insertion(&mut review.photos, source, insertion)
                            {
                                review.selected = Some(new_index);
                                review.slides_dirty = true;
                                review.slide_groups_request_id = None;
                                refresh_preview = true;
                            }
                        });
                } else {
                    review_preview_panel(
                        ui,
                        review.slide_preview_texture.as_ref(),
                        review.slide_error.as_deref(),
                        "Selected generated slide preview",
                    );
                    ui.separator();
                    if review.slides_dirty {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Preparing slide groups…");
                        });
                        refresh_slide_groups = true;
                    } else {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show_rows(
                                ui,
                                76.0,
                                review.slide_groups.len(),
                                |ui, visible_rows| {
                                for index in visible_rows {
                                    let group = &review.slide_groups[index];
                                    let names = group
                                        .iter()
                                        .filter_map(|path| path.file_name()?.to_str())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let label = if group.len() > 1 {
                                        format!(
                                            "Slide {} · {} photos · {names}",
                                            index + 1,
                                            group.len()
                                        )
                                    } else {
                                        format!("Slide {} · {names}", index + 1)
                                    };
                                    ui.horizontal(|ui| {
                                        if let Some(texture) =
                                            review.slide_thumbnail_textures.get(group)
                                        {
                                            let size = texture.size_vec2();
                                            let scale = (128.0 / size.x)
                                                .min(72.0 / size.y)
                                                .min(1.0);
                                            ui.add(
                                                egui::Image::new(texture)
                                                    .fit_to_exact_size(size * scale)
                                                    .alt_text(format!(
                                                        "Preview of slide {}",
                                                        index + 1
                                                    )),
                                            );
                                        } else {
                                            ui.allocate_ui_with_layout(
                                                egui::vec2(128.0, 72.0),
                                                egui::Layout::centered_and_justified(
                                                    egui::Direction::LeftToRight,
                                                ),
                                                |ui| {
                                                    if review
                                                        .slide_thumbnail_failures
                                                        .contains(group)
                                                    {
                                                        ui.weak("Unavailable");
                                                    } else {
                                                        ui.spinner();
                                                    }
                                                },
                                            );
                                            if !review
                                                .slide_thumbnail_request_ids
                                                .contains_key(group)
                                                && !review
                                                    .slide_thumbnail_failures
                                                    .contains(group)
                                            {
                                                schedule_slide_thumbnails.push(group.clone());
                                            }
                                        }
                                        if ui
                                            .selectable_label(
                                                review.slide_selected == Some(index),
                                                label,
                                            )
                                            .clicked()
                                        {
                                            review.slide_selected = Some(index);
                                            refresh_slide_preview = true;
                                        }
                                    });
                                }
                            },
                            );
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    let selected_photo = review.selected.and_then(|index| {
                        review.photos.get(index).map(|photo| photo.path.clone())
                    });
                    if ui
                        .add_enabled(
                            review.tab == PhotoReviewTab::OriginalPhotos
                                && selected_photo.is_some(),
                            egui::Button::new("Open full size"),
                        )
                        .clicked()
                    {
                        open_full_size = selected_photo;
                    }
                    ui.separator();
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if review.editable && ui.button("Apply review").clicked() {
                        let selected_paths = review
                            .photos
                            .iter()
                            .filter(|photo| photo.included)
                            .map(|photo| photo.path.clone())
                            .collect::<Vec<_>>();
                        if selected_paths.len() < 2 {
                            "Keep at least two photos in the slideshow."
                                .clone_into(&mut self.activity);
                        } else {
                            let ordered_paths = review
                                .photos
                                .iter()
                                .map(|photo| photo.path.clone())
                                .collect::<Vec<_>>();
                            apply = Some((review.task_id.clone(), selected_paths, ordered_paths));
                        }
                    } else if !review.editable && ui.button("Close").clicked() {
                        cancel = true;
                    }
                });
            });
        review.open = window_open;
        if let Some((task_id, selected_paths, ordered_paths)) = apply {
            if let Some(task) = self.queue.task_mut(&task_id) {
                task.name = format!("Photo slideshow · {} photos", selected_paths.len());
                task.settings.slideshow_image_paths = selected_paths;
                task.settings.slideshow_review_image_paths = ordered_paths;
                "Applied slideshow photo review.".clone_into(&mut self.activity);
            }
            self.persist_queue();
            self.photo_review = None;
        } else if cancel || !window_open {
            self.photo_review = None;
        } else if refresh_slide_groups {
            self.refresh_photo_review_slide_groups(context);
        } else if refresh_slide_preview {
            self.refresh_photo_review_slide_preview(context);
        } else if refresh_preview {
            self.refresh_photo_review_preview(context);
        }
        if let Some(path) = open_full_size {
            self.open_photo_viewer(context, path);
        }
        self.schedule_review_thumbnails(context, schedule_thumbnails);
        self.schedule_review_slide_thumbnails(context, schedule_slide_thumbnails);
    }

    fn open_photo_viewer(&mut self, context: &egui::Context, path: PathBuf) {
        let request_id = self.next_review_request_id();
        self.photo_viewer = Some(PhotoViewerState {
            path: path.clone(),
            open: true,
            texture: None,
            error: None,
            request_id: Some(request_id),
            zoom: 1.0,
            fit_to_window: true,
        });
        self.send_review_request(
            context,
            ReviewWorkerRequest::Photo {
                request_id,
                target: ReviewPhotoTarget::Viewer,
                path,
                maximum_width: 4096,
                maximum_height: 4096,
            },
        );
    }

    fn show_photo_viewer(&mut self, context: &egui::Context) {
        let Some(viewer) = self.photo_viewer.as_mut() else {
            return;
        };
        let mut open = viewer.open;
        let title = viewer
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("Photo");
        egui::Window::new(title)
            .open(&mut open)
            .default_size([1000.0, 720.0])
            .min_size([640.0, 420.0])
            .show(context, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(viewer.path.display().to_string());
                    if ui.button("Zoom in").clicked() {
                        viewer.zoom = (viewer.zoom * 1.25).min(8.0);
                        viewer.fit_to_window = false;
                    }
                    if ui.button("Zoom out").clicked() {
                        viewer.zoom = (viewer.zoom * 0.8).max(0.05);
                        viewer.fit_to_window = false;
                    }
                    if ui.button("Fit").clicked() {
                        viewer.fit_to_window = true;
                    }
                    if ui.button("Actual size").clicked() {
                        viewer.zoom = 1.0;
                        viewer.fit_to_window = false;
                    }
                    ui.label(format!("{:.0}%", viewer.zoom * 100.0));
                });
                ui.separator();
                if let Some(texture) = viewer.texture.as_ref() {
                    let original = texture.size_vec2();
                    if viewer.fit_to_window {
                        viewer.zoom = (ui.available_width() / original.x)
                            .min(ui.available_height() / original.y)
                            .min(1.0);
                    }
                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let response = ui.add(
                                egui::Image::new(texture)
                                    .fit_to_exact_size(original * viewer.zoom)
                                    .alt_text(format!(
                                        "Full-size preview of {}",
                                        viewer.path.display()
                                    )),
                            );
                            if response.hovered() {
                                let wheel_delta = ui.input(|input| input.smooth_scroll_delta.y);
                                if wheel_delta != 0.0 {
                                    viewer.zoom = photo_zoom_after_wheel(viewer.zoom, wheel_delta);
                                    viewer.fit_to_window = false;
                                    ui.ctx().request_repaint();
                                }
                            }
                        });
                } else if let Some(error) = viewer.error.as_deref() {
                    ui.centered_and_justified(|ui| {
                        ui.label(format!("Unable to open photo: {error}"));
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.spinner();
                    });
                }
            });
        viewer.open = open;
        if !open {
            self.photo_viewer = None;
        }
    }

    fn next_review_request_id(&mut self) -> u64 {
        let request_id = self.next_review_request_id;
        self.next_review_request_id = self.next_review_request_id.wrapping_add(1).max(1);
        request_id
    }

    fn send_review_request(&mut self, context: &egui::Context, request: ReviewWorkerRequest) {
        let worker = self
            .review_worker
            .get_or_insert_with(|| spawn_review_worker(context.clone()));
        if worker.sender.send(request).is_err() {
            self.review_worker = None;
            "Photo preview worker stopped unexpectedly".clone_into(&mut self.activity);
        }
    }

    fn schedule_review_thumbnails(&mut self, context: &egui::Context, paths: Vec<PathBuf>) {
        for path in paths {
            let should_schedule = self.photo_review.as_ref().is_some_and(|review| {
                !review.thumbnail_textures.contains_key(&path)
                    && !review.thumbnail_request_ids.contains_key(&path)
                    && !review.thumbnail_failures.contains(&path)
            });
            if !should_schedule {
                continue;
            }
            let request_id = self.next_review_request_id();
            let Some(review) = self.photo_review.as_mut() else {
                return;
            };
            review
                .thumbnail_request_ids
                .insert(path.clone(), request_id);
            self.send_review_request(
                context,
                ReviewWorkerRequest::Photo {
                    request_id,
                    target: ReviewPhotoTarget::Thumbnail,
                    path,
                    maximum_width: 96,
                    maximum_height: 64,
                },
            );
        }
    }

    fn schedule_review_slide_thumbnails(
        &mut self,
        context: &egui::Context,
        groups: Vec<Vec<PathBuf>>,
    ) {
        let collage = self.photo_review.as_ref().is_some_and(|review| {
            self.queue
                .task(&review.task_id)
                .is_some_and(|task| task.settings.slideshow_collage)
        });
        for paths in groups {
            let should_schedule = self.photo_review.as_ref().is_some_and(|review| {
                !review.slide_thumbnail_textures.contains_key(&paths)
                    && !review.slide_thumbnail_request_ids.contains_key(&paths)
                    && !review.slide_thumbnail_failures.contains(&paths)
            });
            if !should_schedule {
                continue;
            }
            let request_id = self.next_review_request_id();
            let Some(review) = self.photo_review.as_mut() else {
                return;
            };
            review
                .slide_thumbnail_request_ids
                .insert(paths.clone(), request_id);
            self.send_review_request(
                context,
                ReviewWorkerRequest::SlidePreview {
                    request_id,
                    target: ReviewSlideTarget::Thumbnail,
                    paths,
                    collage,
                    width: 128,
                    height: 72,
                },
            );
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "typed preview results are validated and applied at one GUI-thread boundary"
    )]
    fn poll_review_worker(&mut self, context: &egui::Context) {
        let results = self.review_worker.as_ref().map_or_else(Vec::new, |worker| {
            worker.receiver.try_iter().collect::<Vec<_>>()
        });
        for result in results {
            let mut refresh_slide_preview = false;
            match result {
                ReviewWorkerResult::Photo {
                    request_id,
                    target: ReviewPhotoTarget::Review,
                    path,
                    result,
                } => {
                    let Some(review) = self.photo_review.as_mut() else {
                        continue;
                    };
                    if review.preview_request_id != Some(request_id)
                        || review.preview_path.as_ref() != Some(&path)
                    {
                        continue;
                    }
                    review.preview_request_id = None;
                    match result {
                        Ok(preview) => {
                            review.preview_texture = Some(photo_texture(
                                context,
                                format!("review-photo:{}", path.display()),
                                &preview,
                            ));
                            review.preview_error = None;
                        }
                        Err(error) => {
                            review.preview_texture = None;
                            review.preview_error = Some(error.to_string());
                        }
                    }
                }
                ReviewWorkerResult::Photo {
                    request_id,
                    target: ReviewPhotoTarget::Viewer,
                    path,
                    result,
                } => {
                    let Some(viewer) = self.photo_viewer.as_mut() else {
                        continue;
                    };
                    if viewer.request_id != Some(request_id) || viewer.path != path {
                        continue;
                    }
                    viewer.request_id = None;
                    match result {
                        Ok(preview) => {
                            viewer.texture = Some(photo_texture(
                                context,
                                format!("full-photo:{}", path.display()),
                                &preview,
                            ));
                            viewer.error = None;
                        }
                        Err(error) => {
                            viewer.texture = None;
                            viewer.error = Some(error.to_string());
                        }
                    }
                }
                ReviewWorkerResult::Photo {
                    request_id,
                    target: ReviewPhotoTarget::Thumbnail,
                    path,
                    result,
                } => {
                    let Some(review) = self.photo_review.as_mut() else {
                        continue;
                    };
                    if review.thumbnail_request_ids.get(&path) != Some(&request_id) {
                        continue;
                    }
                    review.thumbnail_request_ids.remove(&path);
                    match result {
                        Ok(preview) => {
                            review.thumbnail_textures.insert(
                                path.clone(),
                                photo_texture(
                                    context,
                                    format!("review-thumbnail:{}", path.display()),
                                    &preview,
                                ),
                            );
                            review.thumbnail_failures.remove(&path);
                        }
                        Err(_) => {
                            review.thumbnail_failures.insert(path);
                        }
                    }
                }
                ReviewWorkerResult::SlideGroups { request_id, result } => {
                    let Some(review) = self.photo_review.as_mut() else {
                        continue;
                    };
                    if review.slide_groups_request_id != Some(request_id) {
                        continue;
                    }
                    review.slide_groups_request_id = None;
                    review.slides_dirty = false;
                    review.slide_preview_paths.clear();
                    review.slide_preview_request_id = None;
                    review.slide_preview_texture = None;
                    review.slide_thumbnail_textures.clear();
                    review.slide_thumbnail_request_ids.clear();
                    review.slide_thumbnail_failures.clear();
                    match result {
                        Ok(groups) => {
                            review.slide_groups = groups;
                            review.slide_selected = (!review.slide_groups.is_empty()).then_some(0);
                            review.slide_error = None;
                            refresh_slide_preview = review.slide_selected.is_some();
                        }
                        Err(error) => {
                            review.slide_groups.clear();
                            review.slide_selected = None;
                            review.slide_error = Some(error.to_string());
                        }
                    }
                }
                ReviewWorkerResult::SlidePreview {
                    request_id,
                    target: ReviewSlideTarget::Selected,
                    paths,
                    result,
                } => {
                    let Some(review) = self.photo_review.as_mut() else {
                        continue;
                    };
                    if review.slide_preview_request_id != Some(request_id)
                        || review.slide_preview_paths != paths
                    {
                        continue;
                    }
                    review.slide_preview_request_id = None;
                    match result {
                        Ok(preview) => {
                            review.slide_preview_texture = Some(photo_texture(
                                context,
                                format!("review-slide:{request_id}"),
                                &preview,
                            ));
                            review.slide_error = None;
                        }
                        Err(error) => {
                            review.slide_preview_texture = None;
                            review.slide_error = Some(error.to_string());
                        }
                    }
                }
                ReviewWorkerResult::SlidePreview {
                    request_id,
                    target: ReviewSlideTarget::Thumbnail,
                    paths,
                    result,
                } => {
                    let Some(review) = self.photo_review.as_mut() else {
                        continue;
                    };
                    if review.slide_thumbnail_request_ids.get(&paths) != Some(&request_id) {
                        continue;
                    }
                    review.slide_thumbnail_request_ids.remove(&paths);
                    match result {
                        Ok(preview) => {
                            review.slide_thumbnail_textures.insert(
                                paths.clone(),
                                photo_texture(
                                    context,
                                    format!("review-slide-thumbnail:{request_id}"),
                                    &preview,
                                ),
                            );
                            review.slide_thumbnail_failures.remove(&paths);
                        }
                        Err(_) => {
                            review.slide_thumbnail_failures.insert(paths);
                        }
                    }
                }
            }
            if refresh_slide_preview {
                self.refresh_photo_review_slide_preview(context);
            }
        }
    }

    fn refresh_photo_review_preview(&mut self, context: &egui::Context) {
        let Some(path) = self.photo_review.as_ref().and_then(|review| {
            review
                .selected
                .and_then(|index| review.photos.get(index))
                .map(|photo| photo.path.clone())
        }) else {
            return;
        };
        if self.photo_review.as_ref().is_some_and(|review| {
            review.preview_path.as_ref() == Some(&path)
                && (review.preview_request_id.is_some()
                    || review.preview_texture.is_some()
                    || review.preview_error.is_some())
        }) {
            return;
        }
        let request_id = self.next_review_request_id();
        let Some(review) = self.photo_review.as_mut() else {
            return;
        };
        review.preview_path = Some(path.clone());
        review.preview_request_id = Some(request_id);
        review.preview_texture = None;
        review.preview_error = None;
        self.send_review_request(
            context,
            ReviewWorkerRequest::Photo {
                request_id,
                target: ReviewPhotoTarget::Review,
                path,
                maximum_width: 640,
                maximum_height: 360,
            },
        );
    }

    fn refresh_photo_review_slide_groups(&mut self, context: &egui::Context) {
        if self
            .photo_review
            .as_ref()
            .is_some_and(|review| review.slide_groups_request_id.is_some())
        {
            return;
        }
        let Some((task_id, paths)) = self.photo_review.as_ref().map(|review| {
            (
                review.task_id.clone(),
                review
                    .photos
                    .iter()
                    .filter(|photo| photo.included)
                    .map(|photo| photo.path.clone())
                    .collect::<Vec<_>>(),
            )
        }) else {
            return;
        };
        let collage = self
            .queue
            .task(&task_id)
            .is_some_and(|task| task.settings.slideshow_collage);
        let request_id = self.next_review_request_id();
        let Some(review) = self.photo_review.as_mut() else {
            return;
        };
        review.slide_groups_request_id = Some(request_id);
        review.slide_preview_paths.clear();
        review.slide_preview_request_id = None;
        review.slide_preview_texture = None;
        review.slide_error = None;
        self.send_review_request(
            context,
            ReviewWorkerRequest::SlideGroups {
                request_id,
                paths,
                collage,
            },
        );
    }

    fn refresh_photo_review_slide_preview(&mut self, context: &egui::Context) {
        let Some((task_id, paths)) = self.photo_review.as_ref().and_then(|review| {
            let index = review.slide_selected?;
            Some((
                review.task_id.clone(),
                review.slide_groups.get(index)?.clone(),
            ))
        }) else {
            return;
        };
        if self
            .photo_review
            .as_ref()
            .is_some_and(|review| review.slide_preview_paths == paths)
        {
            return;
        }
        let collage = self
            .queue
            .task(&task_id)
            .is_some_and(|task| task.settings.slideshow_collage);
        let request_id = self.next_review_request_id();
        let Some(review) = self.photo_review.as_mut() else {
            return;
        };
        review.slide_preview_paths.clone_from(&paths);
        review.slide_preview_request_id = Some(request_id);
        review.slide_preview_texture = None;
        review.slide_error = None;
        self.send_review_request(
            context,
            ReviewWorkerRequest::SlidePreview {
                request_id,
                target: ReviewSlideTarget::Selected,
                paths,
                collage,
                width: 640,
                height: 360,
            },
        );
    }

    fn show_status(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                ui.strong("Engine status:");
                ui.label(&self.engine_status);
                ui.separator();
                ui.label(&self.activity);
            });
            ui.horizontal(|ui| {
                if ui
                    .checkbox(&mut self.frame_preview_enabled, "Frame preview")
                    .changed()
                {
                    if let Some(worker) = &self.worker {
                        worker
                            .control
                            .set_preview_enabled(self.frame_preview_enabled);
                    }
                    if !self.frame_preview_enabled {
                        self.live_preview_texture = None;
                        self.live_preview = None;
                    }
                }
                if self.frame_preview_enabled {
                    if let Some(texture) = &self.live_preview_texture {
                        let size = texture.size_vec2();
                        let scale = (240.0 / size.x).min(135.0 / size.y).min(1.0);
                        ui.add(
                            egui::Image::new((texture.id(), size * scale))
                                .alt_text("Live conversion frame preview"),
                        );
                    } else {
                        ui.weak("No preview yet");
                    }
                }
            });
            self.show_source_metrics(ui);
            let Some(progress) = &self.progress else {
                return;
            };
            let mode = self
                .worker
                .as_ref()
                .and_then(|worker| self.queue.task(&worker.task_id))
                .map(|task| task.settings.mode);
            let fraction = progress_fraction(progress, mode).unwrap_or(0.0);
            ui.add(
                egui::ProgressBar::new(fraction)
                    .show_percentage()
                    .animate(self.worker.is_some()),
            );
            self.show_progress_metrics(ui, progress);
        });
    }

    fn show_source_metrics(&self, ui: &mut egui::Ui) {
        let Some(worker) = &self.worker else {
            return;
        };
        let source = &worker.target;
        ui.horizontal_wrapped(|ui| {
            metric(
                ui,
                "Folder",
                &source
                    .parent()
                    .map_or_else(|| "-".to_owned(), |path| path.display().to_string()),
            );
            metric(
                ui,
                "File",
                &source
                    .file_name()
                    .map_or_else(|| "-".to_owned(), |name| name.to_string_lossy().into()),
            );
            if let Some(info) = &self.active_source_info {
                metric(ui, "Original size", &format_size_mb(info.size));
                metric(
                    ui,
                    "Resolution",
                    &format_resolution(info.width, info.height),
                );
                metric(ui, "Original FPS", &format_fps(info.fps));
            }
            if let Some(task) = self.queue.task(&worker.task_id) {
                metric(
                    ui,
                    "Target FPS",
                    &target_fps_status(self.progress.as_ref(), task.settings.fps),
                );
                let counts = Self::task_display_counts(
                    task,
                    &self.folder_summaries,
                    self.task_run_failures.get(&task.id),
                );
                if let Some((index, total)) =
                    active_item_position(task, &counts, self.progress.as_ref())
                {
                    metric(ui, "File #", &format!("{index}/{total}"));
                }
            }
            metric(
                ui,
                "Camera Model",
                self.active_camera_info.model_name.as_deref().unwrap_or("-"),
            );
            metric(
                ui,
                "Applying LUT",
                self.active_camera_info.lut_status.as_deref().unwrap_or("-"),
            );
        });
    }

    fn show_progress_metrics(&self, ui: &mut egui::Ui, progress: &ConversionProgress) {
        let active_settings = self
            .worker
            .as_ref()
            .and_then(|worker| self.queue.task(&worker.task_id))
            .map(|task| &task.settings);
        ui.horizontal_wrapped(|ui| {
            let elapsed = self
                .worker
                .as_ref()
                .map_or(Duration::ZERO, WorkerState::active_elapsed);
            metric(ui, "Spent", &format_clock(elapsed));
            let remaining = estimated_remaining(
                elapsed,
                progress_fraction(progress, active_settings.map(|settings| settings.mode)),
            );
            metric(
                ui,
                "Remaining",
                &remaining.map_or_else(|| "-".to_owned(), format_clock),
            );
            metric(
                ui,
                "Frame",
                &format_frame_progress(
                    progress,
                    active_settings,
                    self.active_source_info.as_ref().and_then(|info| info.fps),
                ),
            );
            metric(
                ui,
                "Time",
                &format_progress_time(progress, active_settings.map(|settings| settings.mode)),
            );
            metric(
                ui,
                "Current FPS",
                &progress
                    .frames_per_second
                    .map_or_else(|| "-".to_owned(), |value| format!("{value:.1}")),
            );
            metric(
                ui,
                "Speed",
                &progress
                    .speed
                    .map_or_else(|| "-".to_owned(), |value| format!("{value:.2}x")),
            );
            metric(
                ui,
                "Current File Size",
                &format_size_mb(progress.output_bytes),
            );
            metric(
                ui,
                "Estimated size",
                &format_size_mb(estimated_output_bytes(progress)),
            );
            metric(
                ui,
                "Approx size/min",
                &format_size_mb(output_bytes_per_minute(progress)),
            );
        });
    }

    fn show_queue(&mut self, ui: &mut egui::Ui) {
        ui.heading("Conversion queue");
        ui.separator();
        if self.queue.tasks().is_empty() && self.folder_summaries.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("Add files or folders to begin building the queue.");
            });
            return;
        }

        self.show_folder_summaries(ui);
        if self.queue.tasks().is_empty() {
            return;
        }

        let mut selection_action = None;
        egui::ScrollArea::both()
            .id_salt("file-task-table-scroll")
            .max_height(240.0)
            .show(ui, |ui| {
                egui::Grid::new("file-task-grid")
                    .striped(true)
                    .show(ui, |ui| {
                        for heading in [
                            "Task name",
                            "Convert mode",
                            "Status",
                            "Targets",
                            "Folders",
                            "Files",
                            "Remaining",
                            "Converted",
                            "Failed",
                            "Queued time",
                            "Complete time",
                        ] {
                            ui.strong(heading);
                        }
                        ui.end_row();
                        for task in self.queue.tasks() {
                            let counts = Self::task_display_counts(
                                task,
                                &self.folder_summaries,
                                self.task_run_failures.get(&task.id),
                            );
                            let selected = self.selected_ids.contains(&task.id);
                            if ui.selectable_label(selected, &task.name).clicked() {
                                selection_action =
                                    Some((task.id.clone(), ui.input(|input| input.modifiers)));
                            }
                            ui.label(format!(
                                "{} · {}",
                                task.settings.mode.label(),
                                task.settings.encoder.user_name()
                            ));
                            ui.label(format!("{:?}", task.status));
                            ui.label(counts.targets.to_string());
                            ui.label(counts.folders.to_string());
                            ui.label(counts.files.to_string());
                            ui.label(counts.remaining.to_string());
                            ui.label(counts.converted.to_string());
                            ui.label(counts.failed.to_string());
                            ui.label(if task.queued_time.is_empty() {
                                "-"
                            } else {
                                &task.queued_time
                            });
                            ui.label(if task.complete_time.is_empty() {
                                "-"
                            } else {
                                &task.complete_time
                            });
                            ui.end_row();
                        }
                    });
            });
        if let Some((id, modifiers)) = selection_action {
            self.apply_task_selection(id, modifiers);
        }
        self.show_selected_task_details(ui);
    }

    fn show_folder_summaries(&self, ui: &mut egui::Ui) {
        let legacy_summaries = self
            .folder_summaries
            .iter()
            .filter(|summary| {
                !self
                    .queue
                    .tasks()
                    .iter()
                    .any(|task| task.targets.iter().any(|target| target == &summary.root))
            })
            .collect::<Vec<_>>();
        if legacy_summaries.is_empty() {
            return;
        }
        ui.strong("Watched folder summary");
        egui::ScrollArea::horizontal()
            .id_salt("folder-summary-scroll")
            .show(ui, |ui| {
                egui::Grid::new("folder-summary-grid")
                    .striped(true)
                    .show(ui, |ui| {
                        for heading in [
                            "Folder",
                            "Convert mode",
                            "Status",
                            "Targets",
                            "Folders",
                            "Files",
                            "Remaining",
                            "Converted",
                            "Failed",
                        ] {
                            ui.strong(heading);
                        }
                        ui.end_row();
                        for summary in legacy_summaries {
                            ui.label(summary.root.display().to_string());
                            ui.label(format!(
                                "{} · {}",
                                summary.mode.label(),
                                summary.encoder.user_name()
                            ));
                            ui.label(folder_summary_status(summary));
                            ui.label("1");
                            ui.label(summary.folders.to_string());
                            ui.label(summary.files.to_string());
                            ui.label(summary.remaining.to_string());
                            ui.label(summary.converted.to_string());
                            ui.label(summary.failed.to_string());
                            ui.end_row();
                        }
                    });
            });
        ui.separator();
        ui.strong("File tasks");
    }

    fn task_display_counts(
        task: &QueueTask,
        folder_summaries: &[FolderQueueSummary],
        failures: Option<&HashMap<PathBuf, String>>,
    ) -> TaskDisplayCounts {
        if task.settings.mode == ContentMode::PhotoSlideshow {
            let valid = task.settings.slideshow_image_paths.len() >= 2
                || task
                    .targets
                    .iter()
                    .any(|target| target.is_dir() && directory_has_two_photos(target));
            let completed = usize::from(valid && task.status == QueueStatus::Completed);
            return TaskDisplayCounts {
                targets: task.targets.len(),
                folders: task.targets.iter().filter(|path| path.is_dir()).count(),
                files: usize::from(valid),
                remaining: usize::from(valid && completed == 0),
                converted: completed,
                failed: 0,
            };
        }

        let mut counts = TaskDisplayCounts {
            targets: task.targets.len(),
            ..TaskDisplayCounts::default()
        };
        for target in &task.targets {
            if target.is_dir() {
                if let Some(summary) = folder_summaries
                    .iter()
                    .find(|summary| summary.root == *target)
                {
                    counts.folders += summary.folders;
                    counts.remaining += summary.remaining;
                    counts.converted += summary.converted;
                    counts.failed += summary.failed;
                } else {
                    counts.folders += 1;
                }
                continue;
            }
            if !has_video_filename(target) {
                continue;
            }
            if failures.is_some_and(|failures| failures.contains_key(target)) {
                counts.failed += 1;
                continue;
            }
            if task.skipped_paths.contains(target) {
                counts.converted += 1;
                continue;
            }
            match task.status {
                QueueStatus::Completed => counts.converted += 1,
                QueueStatus::Failed | QueueStatus::Cancelled => counts.failed += 1,
                QueueStatus::Pending | QueueStatus::Running | QueueStatus::Paused => {
                    if should_skip_queue_source(target, &task.settings) {
                        counts.converted += 1;
                    } else {
                        counts.remaining += 1;
                    }
                }
            }
        }
        counts.files = counts.remaining + counts.converted + counts.failed;
        counts
    }

    fn show_selected_task_details(&mut self, ui: &mut egui::Ui) {
        let Some(id) = self.selected_id.clone() else {
            return;
        };
        let Some(task) = self.queue.task(&id).cloned() else {
            return;
        };
        if self.task_name_edit_id.as_deref() != Some(&id) {
            self.task_name_edit.clone_from(&task.name);
            self.task_name_edit_id = Some(id.clone());
        }
        let single_selected = self.selected_ids.len() == 1;
        let name_editable = single_selected
            && self
                .worker
                .as_ref()
                .is_none_or(|worker| worker.task_id != id);
        let counts = Self::task_display_counts(
            &task,
            &self.folder_summaries,
            self.task_run_failures.get(&task.id),
        );
        let settings_editable =
            name_editable && task.status != QueueStatus::Completed && counts.converted == 0;
        ui.separator();
        ui.heading("Selected task");
        ui.horizontal(|ui| {
            let label = ui.label("Name");
            ui.add_enabled(
                name_editable,
                egui::TextEdit::singleline(&mut self.task_name_edit).desired_width(260.0),
            )
            .labelled_by(label.id);
            if ui
                .add_enabled(
                    name_editable && !self.task_name_edit.trim().is_empty(),
                    egui::Button::new("Save name"),
                )
                .clicked()
            {
                if let Some(stored) = self.queue.task_mut(&id) {
                    self.task_name_edit.trim().clone_into(&mut stored.name);
                }
                self.persist_queue();
            }
            if ui
                .add_enabled(single_selected, egui::Button::new("Load settings"))
                .clicked()
            {
                self.mode = task.settings.mode;
                self.apply_settings_to_ui(&task.settings);
            }
            if ui
                .add_enabled(
                    settings_editable,
                    egui::Button::new("Apply current settings"),
                )
                .clicked()
            {
                let settings = self.validated_current_settings();
                if let Err(error) = settings {
                    self.activity = error;
                } else {
                    let settings = settings.expect("validated above");
                    if let Err(error) = validate_task_targets(&settings, &task.targets) {
                        self.activity = error;
                        return;
                    }
                    let mut watch_roots = Vec::new();
                    if let Some(stored) = self.queue.task_mut(&id) {
                        stored.settings = settings.clone();
                        rebuild_slideshow_task_images(stored);
                        watch_roots
                            .extend(stored.targets.iter().filter(|path| path.is_dir()).cloned());
                    }
                    for root in watch_roots {
                        self.register_folder_watch(root, settings.clone());
                    }
                    self.task_run_failures.remove(&id);
                    self.task_run_start_converted.remove(&id);
                    if let Some(task) = self.queue.task_mut(&id) {
                        task.skipped_paths.clear();
                    }
                    "Updated queued task settings".clone_into(&mut self.activity);
                    self.persist_queue();
                    self.refresh_folder_summaries();
                }
            }
        });
        ui.label(format!(
            "{} · {} · {:?} · {:?}",
            task.settings.mode.label(),
            task.settings.encoder.user_name(),
            task.settings.fps,
            task.status
        ));
        self.show_task_targets(ui, &id, &task, settings_editable);
        self.show_selected_task_files(ui, &task);
        if let Some(error) = &task.error {
            ui.colored_label(egui::Color32::RED, error);
        }
    }

    fn show_selected_task_files(&mut self, ui: &mut egui::Ui, task: &QueueTask) {
        ui.strong("Files");
        let mut error_to_show = None;
        egui::ScrollArea::horizontal().show(ui, |ui| {
            egui::Grid::new("selected-task-files-grid")
                .striped(true)
                .show(ui, |ui| {
                    for heading in [
                        "File",
                        "Status",
                        "Started",
                        "Completed",
                        "Conversion time",
                        "Original size",
                        "New size",
                        "Original FPS",
                        "New FPS",
                        "Codec",
                        "Duration",
                        "Error",
                    ] {
                        ui.strong(heading);
                    }
                    ui.end_row();
                    for file in selected_task_files(
                        task,
                        &self.completed_history,
                        self.task_run_failures.get(&task.id),
                    ) {
                        for value in [
                            &file.path,
                            &file.status,
                            &file.started_time,
                            &file.completed_time,
                            &file.conversion_time,
                            &file.original_size,
                            &file.new_size,
                            &file.original_fps,
                            &file.new_fps,
                            &file.codec,
                            &file.duration,
                        ] {
                            ui.label(value);
                        }
                        if file.error_detail.is_empty() {
                            ui.label("-");
                        } else if ui.button("Show error").clicked() {
                            error_to_show = Some((file.path, file.error_detail));
                        }
                        ui.end_row();
                    }
                });
        });
        if let Some(error) = error_to_show {
            self.task_file_error_dialog = Some(error);
        }
        if let Some((path, error)) = self.task_file_error_dialog.clone() {
            let mut open = true;
            let mut close_clicked = false;
            egui::Window::new("Conversion error")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .show(ui.ctx(), |ui| {
                    ui.strong(path);
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .max_height(320.0)
                        .show(ui, |ui| {
                            ui.label(&error);
                        });
                    ui.horizontal(|ui| {
                        if ui.button("Copy error").clicked() {
                            ui.ctx().copy_text(error.clone());
                        }
                        if ui.button("Close").clicked() {
                            close_clicked = true;
                        }
                    });
                });
            if !open || close_clicked {
                self.task_file_error_dialog = None;
            }
        }
    }

    fn show_task_targets(&mut self, ui: &mut egui::Ui, id: &str, task: &QueueTask, editable: bool) {
        ui.strong("Targets");
        let mut action = None;
        for (index, target) in task.targets.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(target.display().to_string());
                if ui
                    .add_enabled(editable && index > 0, egui::Button::new("↑"))
                    .on_hover_text("Move target up")
                    .clicked()
                {
                    action = Some(TaskTargetAction::MoveUp(index));
                }
                if ui
                    .add_enabled(
                        editable && index + 1 < task.targets.len(),
                        egui::Button::new("↓"),
                    )
                    .on_hover_text("Move target down")
                    .clicked()
                {
                    action = Some(TaskTargetAction::MoveDown(index));
                }
                if ui
                    .add_enabled(
                        editable && task.targets.len() > 1,
                        egui::Button::new("Remove"),
                    )
                    .clicked()
                {
                    action = Some(TaskTargetAction::Remove(index));
                }
            });
        }
        if let Some(action) = action {
            self.apply_task_target_action(id, action);
        }
        let mut add_files = false;
        let mut add_folder = false;
        ui.horizontal(|ui| {
            add_files = ui
                .add_enabled(
                    editable
                        && (task.settings.mode != ContentMode::Trim || task.targets.is_empty()),
                    egui::Button::new("Add target files"),
                )
                .clicked();
            add_folder = ui
                .add_enabled(
                    editable && task.settings.mode != ContentMode::Trim,
                    egui::Button::new("Add target folder"),
                )
                .clicked();
        });
        if add_files {
            let dialog = if task.settings.mode == ContentMode::PhotoSlideshow {
                rfd::FileDialog::new().add_filter(
                    "Photo files",
                    &["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff"],
                )
            } else {
                rfd::FileDialog::new().add_filter(
                    "Video files",
                    &["mkv", "mp4", "mov", "avi", "wmv", "flv", "rm", "rmvb"],
                )
            };
            if let Some(paths) = dialog.pick_files() {
                self.add_targets_to_task(id, paths);
            }
        }
        if add_folder && let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.add_targets_to_task(id, vec![path]);
        }
    }

    fn apply_task_target_action(&mut self, id: &str, action: TaskTargetAction) {
        let Some(mut candidate) = self.queue.task(id).cloned() else {
            return;
        };
        match action {
            TaskTargetAction::MoveUp(index) if index > 0 && index < candidate.targets.len() => {
                candidate.targets.swap(index, index - 1);
            }
            TaskTargetAction::MoveDown(index) if index + 1 < candidate.targets.len() => {
                candidate.targets.swap(index, index + 1);
            }
            TaskTargetAction::Remove(index) if candidate.targets.len() > 1 => {
                candidate.targets.remove(index);
            }
            _ => return,
        }
        rebuild_slideshow_task_images(&mut candidate);
        if let Err(error) = validate_task_targets(&candidate.settings, &candidate.targets) {
            self.activity = error;
            return;
        }
        if let Some(task) = self.queue.task_mut(id) {
            *task = candidate;
        }
        self.task_run_failures.remove(id);
        self.task_run_start_converted.remove(id);
        if let Some(task) = self.queue.task_mut(id) {
            task.skipped_paths.clear();
        }
        "Updated queued task targets".clone_into(&mut self.activity);
        self.prune_unused_folder_watches();
        self.persist_queue();
        self.refresh_folder_summaries();
    }

    fn add_targets_to_task(&mut self, id: &str, paths: Vec<PathBuf>) {
        let Some(snapshot) = self.queue.task(id).cloned() else {
            return;
        };
        let occupied = self
            .queue
            .tasks()
            .iter()
            .filter(|task| task.id != id)
            .flat_map(|task| task.targets.iter().cloned())
            .collect::<HashSet<_>>();
        let mut accepted = Vec::new();
        let mut skipped = 0_usize;
        for path in paths {
            let valid = if path.is_dir() {
                snapshot.settings.mode != ContentMode::Trim
                    && (snapshot.settings.mode != ContentMode::PhotoSlideshow
                        || !collect_photo_files(&path).is_empty())
            } else if snapshot.settings.mode == ContentMode::PhotoSlideshow {
                is_photo_path(&path)
            } else {
                is_video_path(&path)
            };
            if !valid
                || occupied.contains(&path)
                || snapshot.targets.contains(&path)
                || accepted.contains(&path)
            {
                skipped += 1;
                continue;
            }
            accepted.push(path);
        }
        if accepted.is_empty() {
            self.activity = if skipped > 0 {
                "No new valid targets were selected".to_owned()
            } else {
                "No targets were selected".to_owned()
            };
            return;
        }

        let new_watches = accepted
            .iter()
            .filter(|path| path.is_dir())
            .cloned()
            .collect::<Vec<_>>();
        let mut candidate = snapshot;
        candidate.targets.extend(accepted);
        rebuild_slideshow_task_images(&mut candidate);
        if let Err(error) = validate_task_targets(&candidate.settings, &candidate.targets) {
            self.activity = error;
            return;
        }
        let watch_settings = candidate.settings.clone();
        if let Some(task) = self.queue.task_mut(id) {
            *task = candidate;
        }
        self.task_run_failures.remove(id);
        self.task_run_start_converted.remove(id);
        if let Some(task) = self.queue.task_mut(id) {
            task.skipped_paths.clear();
        }
        for root in new_watches {
            self.register_folder_watch(root, watch_settings.clone());
        }
        self.activity = if skipped > 0 {
            format!("Added task targets; skipped {skipped} duplicate or unsupported path(s)")
        } else {
            "Added task targets".to_owned()
        };
        self.persist_queue();
        self.refresh_folder_summaries();
    }

    fn prune_unused_folder_watches(&mut self) {
        let referenced = self
            .queue
            .tasks()
            .iter()
            .flat_map(|task| {
                task.targets
                    .iter()
                    .cloned()
                    .chain(task.source_root.iter().cloned())
            })
            .collect::<HashSet<_>>();
        self.watched_folders
            .retain(|watch| referenced.contains(&watch.root));
    }

    fn show_center(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.center_view, CenterView::Queue, "Conversion queue");
            ui.selectable_value(
                &mut self.center_view,
                CenterView::CompletedHistory,
                format!("Completed history ({})", self.completed_history.len()),
            );
            ui.selectable_value(&mut self.center_view, CenterView::Processes, "Processes");
            ui.selectable_value(&mut self.center_view, CenterView::Log, "Activity log");
            ui.selectable_value(&mut self.center_view, CenterView::About, "About");
        });
        ui.separator();
        match self.center_view {
            CenterView::Queue => self.show_queue(ui),
            CenterView::CompletedHistory => self.show_completed_history(ui),
            CenterView::Processes => self.show_processes(ui),
            CenterView::Log => self.show_activity_log(ui),
            CenterView::About => self.show_about(ui),
        }
    }

    fn show_processes(&self, ui: &mut egui::Ui) {
        ui.heading("Processes");
        ui.weak("Direct FFmpeg runs inside this application; no ffmpeg or ffprobe child process is launched.");
        ui.add_space(6.0);
        egui::Grid::new("process-grid")
            .striped(true)
            .show(ui, |ui| {
                for heading in ["Type", "PID", "Started", "Run time", "Target"] {
                    ui.strong(heading);
                }
                ui.end_row();
                if let Some(worker) = &self.worker {
                    ui.label(if worker.paused {
                        "Native FFmpeg worker (paused)"
                    } else {
                        "Native FFmpeg worker"
                    });
                    ui.label(std::process::id().to_string());
                    ui.label(&worker.started_local);
                    ui.label(format_clock(worker.active_elapsed()));
                    ui.label(worker.target.display().to_string());
                    ui.end_row();
                }
            });
        if self.worker.is_none() {
            ui.add_space(8.0);
            ui.weak("No active media worker.");
        } else {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
    }

    fn show_about(&self, ui: &mut egui::Ui) {
        ui.heading("VideoFerry");
        ui.label(format!("Application version {}", env!("CARGO_PKG_VERSION")));
        ui.label(format!(
            "Rust language level {} (toolchain pinned to 1.98.0)",
            env!("CARGO_PKG_RUST_VERSION")
        ));
        ui.add_space(8.0);
        ui.strong("Native media engine");
        ui.label(&self.engine_status);
        ui.label("FFmpeg and FFmpeg-derived libraries are bound in-process; ffmpeg and ffprobe executables are not used.");
        ui.add_space(8.0);
        ui.strong("Licensing");
        ui.label("The active FFmpeg runtime reports its license in the engine line above. The Windows FFmpeg 9.0.1 distribution is GPL-3.0-or-later. The application repository license must be finalized before publishing these Rust crates.");
        ui.add_space(8.0);
        egui::CollapsingHeader::new("Embedded engine manifest")
            .default_open(false)
            .show(ui, |ui| {
                ui.monospace(include_str!("../../../engine-manifest.toml"));
            });
    }

    fn show_activity_log(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Activity log");
            if ui
                .add_enabled(!self.activity_log.is_empty(), egui::Button::new("Copy"))
                .clicked()
            {
                ui.ctx().copy_text(self.activity_log.join("\n"));
            }
            if ui
                .add_enabled(!self.activity_log.is_empty(), egui::Button::new("Clear"))
                .clicked()
            {
                self.activity_log.clear();
            }
        });
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.activity_log {
                    ui.monospace(line);
                }
            });
    }

    fn show_completed_history(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Completed history");
            if ui
                .add_enabled(
                    !self.completed_history.is_empty(),
                    egui::Button::new("Clear history"),
                )
                .clicked()
            {
                match self.state_store.clear_history() {
                    Ok(()) => {
                        self.completed_history.clear();
                        "Completed history cleared".clone_into(&mut self.activity);
                    }
                    Err(error) => self.activity = format!("Unable to clear history: {error}"),
                }
            }
        });
        if self.completed_history.is_empty() {
            ui.centered_and_justified(|ui| ui.label("Completed conversions will appear here."));
            return;
        }
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("completed-history-grid")
                .striped(true)
                .show(ui, |ui| {
                    for heading in HISTORY_HEADERS {
                        ui.strong(heading);
                    }
                    ui.end_row();
                    for row in &self.completed_history {
                        for value in row.display_columns() {
                            ui.label(value);
                        }
                        ui.end_row();
                    }
                });
        });
    }

    fn add_paths(&mut self, mut paths: Vec<PathBuf>) {
        if self.mode == ContentMode::PhotoSlideshow {
            self.add_slideshow_paths(&paths);
            return;
        }
        let mut skipped = 0_usize;
        if self.mode == ContentMode::Trim {
            if let Err(error) = self.trim_range_from_ui() {
                self.activity = error;
                return;
            }
            if self.worker.is_some() {
                "Stop the active conversion before adding a Trim task"
                    .clone_into(&mut self.activity);
                return;
            }
            let (selected, extra_video_count) = select_trim_source(&paths);
            skipped = extra_video_count;
            let Some(selected) = selected else {
                "Trim accepts one video file, not a folder.".clone_into(&mut self.activity);
                return;
            };
            self.clear_queue();
            paths = vec![selected];
        }
        let settings = self.current_settings();
        let occupied = queue_admission_occupied_paths(&self.queue, &self.watched_folders);
        let (targets, collection_skipped) =
            collect_new_video_task_targets(paths, &settings, &occupied);
        skipped += collection_skipped;
        let target_count = targets.len();
        let watch_roots = targets
            .iter()
            .filter(|path| path.is_dir())
            .cloned()
            .collect::<Vec<_>>();
        let mut added = false;
        if let Some(first_target) = targets.first() {
            let id = format!("task-{}", self.next_id);
            self.next_id += 1;
            let name = default_task_name(&settings, first_target);
            let source_root =
                (targets.len() == 1 && first_target.is_dir()).then(|| first_target.clone());
            let mut task = QueueTask::new(&id, name, targets, settings.clone());
            task.source_root = source_root;
            task.queued_time = formatted_local_time();
            if let Err(error) = self.queue.add(task) {
                self.activity = error.to_string();
            } else {
                self.select_only(Some(id));
                added = true;
            }
        }
        if added {
            self.activity = if skipped > 0 {
                format!(
                    "Added one queue task with {target_count} target(s); skipped {skipped} duplicate, unsupported, or completed target(s)"
                )
            } else {
                format!("Added one queue task with {target_count} target(s)")
            };
            self.persist_queue();
        } else if skipped > 0 {
            self.activity =
                format!("No remaining work; skipped {skipped} duplicate or completed item(s)");
        }
        if added {
            for root in watch_roots {
                self.register_folder_watch(root, settings.clone());
            }
        }
        self.refresh_folder_summaries();
    }

    fn register_folder_watch(&mut self, root: PathBuf, settings: QueueSettings) {
        let snapshot = folder_snapshot_for_settings(&root, &settings);
        if let Some(watch) = self
            .watched_folders
            .iter_mut()
            .find(|watch| watch.root == root)
        {
            watch.settings = settings;
            watch.snapshot = snapshot;
        } else {
            self.watched_folders.push(WatchedFolder {
                root,
                settings,
                snapshot,
            });
        }
    }

    fn enqueue_media_file(
        &mut self,
        target: PathBuf,
        settings: QueueSettings,
        source_root: Option<PathBuf>,
    ) {
        let id = format!("task-{}", self.next_id);
        self.next_id += 1;
        let name = default_task_name(&settings, &target);
        let mut task = QueueTask::new(&id, name, vec![target], settings);
        task.source_root = source_root;
        task.queued_time = formatted_local_time();
        if let Err(error) = self.queue.add(task) {
            self.activity = error.to_string();
        } else {
            self.select_only(Some(id));
        }
    }

    fn add_slideshow_paths(&mut self, paths: &[PathBuf]) {
        let mut selected_images = paths
            .iter()
            .filter(|path| is_photo_path(path))
            .cloned()
            .collect::<Vec<_>>();
        let mut seen_images = HashSet::new();
        selected_images.retain(|path| seen_images.insert(path.clone()));
        let mut folders = paths
            .iter()
            .filter(|path| path.is_dir())
            .cloned()
            .collect::<Vec<_>>();
        folders.sort();
        folders.dedup();
        if selected_images.is_empty() && folders.is_empty() {
            "Select at least two photos or one photo folder.".clone_into(&mut self.activity);
            return;
        }
        let mut added = 0_usize;
        let mut skipped = 0_usize;
        let mut occupied = queued_target_set(&self.queue);
        if !selected_images.is_empty() && selected_images.len() < 2 {
            skipped += 1;
        } else if let Some(input) = selected_images.first().cloned() {
            if occupied.contains(&input) {
                skipped += 1;
            } else {
                let id = format!("task-{}", self.next_id);
                self.next_id += 1;
                let mut settings = self.current_settings();
                settings.slideshow_image_paths.clone_from(&selected_images);
                settings
                    .slideshow_review_image_paths
                    .clone_from(&selected_images);
                let name = format!("Photo slideshow · {} photos", selected_images.len());
                let queue_input = input.clone();
                let mut task = QueueTask::new(&id, name, vec![input], settings);
                task.queued_time = formatted_local_time();
                if let Err(error) = self.queue.add(task) {
                    self.activity = error.to_string();
                } else {
                    occupied.insert(queue_input);
                    self.select_only(Some(id));
                    added += 1;
                }
            }
        }

        for root in folders {
            if occupied.contains(&root) || !directory_has_two_photos(&root) {
                skipped += 1;
                continue;
            }
            let id = format!("task-{}", self.next_id);
            self.next_id += 1;
            let mut settings = self.current_settings();
            settings.slideshow_image_paths.clear();
            settings.slideshow_review_image_paths.clear();
            let name = format!(
                "Photo slideshow · {}",
                root.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("folder")
            );
            let mut task = QueueTask::new(&id, name, vec![root.clone()], settings.clone());
            task.queued_time = formatted_local_time();
            if let Err(error) = self.queue.add(task) {
                self.activity = error.to_string();
            } else {
                occupied.insert(root.clone());
                self.select_only(Some(id));
                self.register_folder_watch(root, settings);
                added += 1;
            }
        }

        if added > 0 {
            self.activity = if skipped > 0 {
                format!("Added {added} photo slideshow(s); skipped {skipped} invalid selection(s)")
            } else {
                format!("Added {added} photo slideshow(s)")
            };
            self.persist_queue();
            self.refresh_folder_summaries();
        } else if skipped > 0 {
            "Photo slideshow requires at least two images.".clone_into(&mut self.activity);
        }
    }

    fn allowed_encoders(&self) -> Vec<Encoder> {
        let encoders = if self.mode == ContentMode::Trim {
            vec![Encoder::X265]
        } else {
            self.available_encoders.clone()
        };
        if encoders.is_empty() {
            vec![Encoder::X265]
        } else {
            encoders
        }
    }

    fn current_settings(&self) -> QueueSettings {
        self.settings_from_ui(self.mode, self.encoder)
    }

    fn validated_current_settings(&self) -> Result<QueueSettings, String> {
        if self.mode == ContentMode::Trim {
            self.trim_range_from_ui()?;
        }
        Ok(self.current_settings())
    }

    fn trim_range_from_ui(&self) -> Result<(Duration, Duration), String> {
        let start = parse_trim_time(&self.trim_start_text)
            .map_err(|error| format!("Invalid start time: {error}"))?;
        let end = parse_trim_time(&self.trim_end_text)
            .map_err(|error| format!("Invalid end time: {error}"))?;
        if end < start {
            return Err("Trim end time must be later than or equal to start time".to_owned());
        }
        Ok((start, end))
    }

    fn settings_from_ui(&self, mode: ContentMode, encoder: Encoder) -> QueueSettings {
        let mut settings = self
            .settings_by_mode
            .get(&mode)
            .filter(|settings| settings.encoder == encoder)
            .cloned()
            .unwrap_or_else(|| default_settings(mode, encoder));
        settings.mode = mode;
        settings.encoder = encoder;
        settings.fps = if matches!(mode, ContentMode::Trim | ContentMode::PhotoSlideshow) {
            FpsPolicy::Source
        } else {
            match self.fps_mode {
                FpsUiMode::SharedLowest => FpsPolicy::SharedLowest,
                FpsUiMode::Source => FpsPolicy::Source,
                FpsUiMode::Explicit => FpsPolicy::Exact(self.explicit_fps.max(f64::MIN_POSITIVE)),
            }
        };
        if matches!(encoder, Encoder::X264 | Encoder::X265 | Encoder::SvtAv1) {
            settings.quality = Some(normalized_quality(encoder, self.quality_crf));
            settings.speed_preset = Some(self.speed_preset.clone());
        } else if encoder.is_nvenc() {
            settings.quality = None;
            settings.speed_preset = Some(self.speed_preset.clone());
        } else {
            settings.quality = None;
            settings.speed_preset = None;
        }
        if mode == ContentMode::Trim {
            if let Ok((start, end)) = self.trim_range_from_ui() {
                settings.trim_start = Some(start);
                settings.trim_end = Some(end);
            }
        } else if mode == ContentMode::CameraVideos && !encoder.is_hardware() {
            settings.apply_lut = self.apply_lut;
        } else if mode == ContentMode::Stabilize {
            settings
                .stabilize_strength
                .clone_from(&self.stabilize_strength);
        } else if mode == ContentMode::PhotoSlideshow {
            settings.photo_interval = Duration::from_secs_f32(self.slideshow_interval_seconds);
            settings.slideshow_fps = self.slideshow_fps;
            settings.slideshow_resolution = if self.slideshow_resolution == "4K" {
                (3840, 2160)
            } else {
                (1920, 1080)
            };
            settings
                .slideshow_audio_paths
                .clone_from(&self.slideshow_audio_paths);
            settings.slideshow_collage = self.slideshow_collage;
        }
        settings
    }

    fn apply_settings_to_ui(&mut self, settings: &QueueSettings) {
        self.apply_encoding_settings_to_ui(settings);
        self.trim_start_text = format_trim_time(settings.trim_start.unwrap_or_default());
        self.trim_end_text = format_trim_time(settings.trim_end.unwrap_or(Duration::from_secs(1)));
        self.stabilize_strength
            .clone_from(&settings.stabilize_strength);
        self.apply_lut = settings.apply_lut;
        self.slideshow_interval_seconds = settings.photo_interval.as_secs_f32();
        self.slideshow_fps = settings.slideshow_fps;
        self.slideshow_resolution = if settings.slideshow_resolution == (3840, 2160) {
            "4K".to_owned()
        } else {
            "1080p".to_owned()
        };
        self.slideshow_audio_paths
            .clone_from(&settings.slideshow_audio_paths);
        self.slideshow_collage = settings.slideshow_collage;
    }

    fn apply_encoding_settings_to_ui(&mut self, settings: &QueueSettings) {
        self.encoder = settings.encoder;
        self.fps_mode = fps_ui_mode(settings.fps);
        self.explicit_fps = explicit_fps(settings.fps);
        self.apply_quality_settings_to_ui(&EncodingUiSettings::from(settings));
    }

    fn apply_quality_settings_to_ui(&mut self, settings: &EncodingUiSettings) {
        self.quality_crf = normalized_quality(self.encoder, settings.quality_crf);
        self.speed_preset.clone_from(&settings.speed_preset);
    }

    fn remember_encoding_settings(&mut self, mode: ContentMode, encoder: Encoder) {
        self.encoding_settings.insert(
            (mode, encoder),
            EncodingUiSettings {
                quality_crf: self.quality_crf,
                speed_preset: self.speed_preset.clone(),
            },
        );
    }

    fn sync_preferences(&mut self) {
        self.remember_encoding_settings(self.mode, self.encoder);
        let settings = self.current_settings();
        let settings_changed = self.settings_by_mode.get(&self.mode) != Some(&settings);
        if settings_changed {
            self.settings_by_mode.insert(self.mode, settings);
        }
        if settings_changed
            || self.persisted_mode != self.mode
            || self.sleep.persisted_enabled != self.sleep.enabled
            || self.persisted_cpu_limit != self.cpu_limit
        {
            let preferences = AppPreferences {
                selected_mode: self.mode,
                settings_by_mode: self.settings_by_mode.clone(),
                prevent_system_sleep: self.sleep.enabled,
                cpu_limit: self.cpu_limit,
            };
            match self.state_store.save_preferences(&preferences) {
                Ok(()) => {
                    self.persisted_mode = self.mode;
                    self.sleep.persisted_enabled = self.sleep.enabled;
                    self.persisted_cpu_limit = self.cpu_limit;
                }
                Err(error) => self.activity = format!("Unable to save settings: {error}"),
            }
        }
    }

    fn cpu_thread_limit(&self) -> usize {
        self.cpu_limit
            .thread_limit(self.cpu_limiter.available_threads())
    }

    fn cpu_limit_summary(&self) -> String {
        let threads = self.cpu_thread_limit();
        let available = self.cpu_limiter.available_threads();
        let unit = if available == 1 { "thread" } else { "threads" };
        format!(
            "{} · {}% · {threads} of {available} CPU {unit}",
            self.cpu_limit.label(),
            self.cpu_limit.percent()
        )
    }

    fn start_selected(&mut self, context: &egui::Context) -> bool {
        let Some(id) = self.selected_id.clone() else {
            return false;
        };
        let Some(task) = self.queue.task(&id).cloned() else {
            "Selected queue item no longer exists.".clone_into(&mut self.activity);
            return false;
        };
        if !matches!(
            task.settings.mode,
            ContentMode::Trim
                | ContentMode::Tv
                | ContentMode::Animation
                | ContentMode::CameraVideos
                | ContentMode::Stabilize
                | ContentMode::PhotoSlideshow
        ) {
            "This workflow is visible for parity planning but is not executable yet."
                .clone_into(&mut self.activity);
            return false;
        }
        let failed_inputs = self.task_run_failures.get(&id);
        let input = if task.settings.mode == ContentMode::PhotoSlideshow {
            task.targets.first().cloned()
        } else {
            remaining_task_inputs(&task, failed_inputs)
                .into_iter()
                .next()
        };
        let Some(input) = input else {
            let _ = self.queue.set_status(&id, QueueStatus::Completed);
            "No remaining work was found in the selected task".clone_into(&mut self.activity);
            self.persist_queue();
            return false;
        };
        let output = if task.settings.mode == ContentMode::Trim {
            let Some(start) = task.settings.trim_start else {
                "Trim start is missing.".clone_into(&mut self.activity);
                return false;
            };
            let Some(end) = task.settings.trim_end else {
                "Trim end is missing.".clone_into(&mut self.activity);
                return false;
            };
            if end < start {
                "Trim end must be at or after start.".clone_into(&mut self.activity);
                return false;
            }
            trim_output_path(&input, start, end)
        } else if task.settings.mode == ContentMode::Stabilize {
            stabilized_output_path(&input)
        } else if task.settings.mode == ContentMode::PhotoSlideshow {
            next_slideshow_output_path(&input)
        } else {
            conversion_output_path(
                &input,
                descriptor(task.settings.mode, task.settings.encoder).container,
            )
        };
        let request = ConversionRequest {
            input,
            output,
            settings: task.settings.clone(),
        };
        let converted_before_run = Self::task_display_counts(
            &task,
            &self.folder_summaries,
            self.task_run_failures.get(&id),
        )
        .converted;
        self.task_run_start_converted
            .entry(id.clone())
            .or_insert(converted_before_run);
        let _ = self.queue.set_status(&id, QueueStatus::Running);
        let run_error = task_run_failure_summary(self.task_run_failures.get(&id));
        let _ = self.queue.set_error(&id, run_error);
        self.progress = None;
        let cpu_thread_limit = self.cpu_thread_limit();
        let cpu_limit_error = self.cpu_limiter.set_thread_limit(cpu_thread_limit).err();
        self.activity = cpu_limit_error.map_or_else(
            || format!("Running {}", task.name),
            |error| format!("Running {}; live CPU limit unavailable: {error}", task.name),
        );
        self.live_preview_texture = None;
        self.live_preview = None;
        self.active_source_info = None;
        self.active_stream_info = None;
        self.active_camera_info = CameraRunInfo::default();
        self.worker = Some(spawn_worker(
            id,
            request,
            context.clone(),
            self.frame_preview_enabled,
            cpu_thread_limit,
        ));
        self.update_sleep_inhibitor();
        self.persist_queue();
        true
    }

    fn start_queue(&mut self, context: &egui::Context) {
        self.completion_notice.clear();
        let pending_ids = self
            .queue
            .tasks()
            .iter()
            .filter(|task| task.status == QueueStatus::Pending)
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        for id in pending_ids {
            self.task_run_start_converted.remove(&id);
        }
        self.queue_run_state = QueueRunState::Running;
        self.start_next_pending(context);
    }

    fn start_next_pending(&mut self, context: &egui::Context) {
        while let Some(id) = self.queue.next_pending_id().map(str::to_owned) {
            self.select_only(Some(id.clone()));
            if self.start_selected(context) {
                return;
            }
            if self.queue.task(&id).map(|task| task.status) == Some(QueueStatus::Completed) {
                continue;
            }
            let message = self.activity.clone();
            let _ = self.queue.set_status(&id, QueueStatus::Failed);
            let _ = self.queue.set_error(&id, Some(message));
        }
        if self.rescan_watched_folders(true) > 0 {
            self.start_next_pending(context);
            return;
        }
        self.queue_run_state = QueueRunState::Idle;
        self.completion_notice = self.queue_completion_summary();
        self.activity.clone_from(&self.completion_notice);
        self.persist_queue();
    }

    fn queue_completion_summary(&self) -> String {
        let mut previous = 0_usize;
        let mut this_run = 0_usize;
        let mut failed = 0_usize;
        let mut remaining = 0_usize;
        for task in self.queue.tasks() {
            let counts = Self::task_display_counts(
                task,
                &self.folder_summaries,
                self.task_run_failures.get(&task.id),
            );
            let before = self
                .task_run_start_converted
                .get(&task.id)
                .copied()
                .unwrap_or(counts.converted)
                .min(counts.files);
            previous = previous.saturating_add(before);
            this_run = this_run.saturating_add(counts.converted.saturating_sub(before));
            failed = failed.saturating_add(counts.failed);
            remaining = remaining.saturating_add(counts.remaining);
        }
        format!(
            "Queue complete — {this_run} completed this run, {previous} already complete, {failed} failed, {remaining} remaining"
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "worker completion atomically updates per-file history and aggregate task state"
    )]
    fn poll_worker(&mut self, context: &egui::Context) {
        let Some(worker) = &self.worker else {
            return;
        };
        let task_id = worker.task_id.clone();
        let worker_target = worker.target.clone();
        let worker_mode = self.queue.task(&task_id).map(|task| task.settings.mode);
        let updates = worker.receiver.try_iter().collect::<Vec<_>>();
        let mut finished = false;
        let mut continue_task = false;
        for update in updates {
            match update {
                WorkerUpdate::SourceInfo(info, streams) => {
                    self.active_source_info = Some(info);
                    self.active_stream_info = Some(streams);
                }
                WorkerUpdate::CameraInfo(info) => self.active_camera_info = info,
                WorkerUpdate::Event(event) => self.handle_conversion_event(context, event),
                WorkerUpdate::Finished(result) => {
                    finished = true;
                    match result {
                        Ok(WorkerJobOutcome::Converted(job)) => {
                            self.activity = format!("Created {}", job.output.display());
                            if let Some(task) = self.queue.task(&task_id) {
                                let history_row = completed_history_row(task, &job);
                                match self.state_store.append_history(history_row) {
                                    Ok(()) => match self.state_store.load_history() {
                                        Ok(rows) => self.completed_history = rows,
                                        Err(error) => {
                                            self.activity = format!(
                                                "Created {}; unable to refresh history: {error}",
                                                job.output.display()
                                            );
                                        }
                                    },
                                    Err(error) => {
                                        self.activity = format!(
                                            "Created {}; unable to save history: {error}",
                                            job.output.display()
                                        );
                                    }
                                }
                            }
                            let has_remaining = self.queue.task(&task_id).is_some_and(|task| {
                                task_is_aggregate(task)
                                    && !remaining_task_inputs(
                                        task,
                                        self.task_run_failures.get(&task_id),
                                    )
                                    .is_empty()
                            });
                            if has_remaining {
                                let _ = self.queue.set_status(&task_id, QueueStatus::Pending);
                                let run_error =
                                    task_run_failure_summary(self.task_run_failures.get(&task_id));
                                let _ = self.queue.set_error(&task_id, run_error);
                                continue_task = true;
                            } else {
                                let _ = self.queue.set_status(&task_id, QueueStatus::Completed);
                                let failed_count =
                                    self.task_run_failures.get(&task_id).map_or(0, HashMap::len);
                                if failed_count > 0 {
                                    let _ = self.queue.set_error(
                                        &task_id,
                                        Some(format!("{failed_count} file(s) failed")),
                                    );
                                }
                                if let Some(task) = self.queue.task_mut(&task_id) {
                                    task.complete_time.clone_from(&job.end_time);
                                }
                            }
                        }
                        Ok(WorkerJobOutcome::Skipped { reason }) => {
                            if let Some(task) = self.queue.task_mut(&task_id)
                                && !task.skipped_paths.contains(&worker_target)
                            {
                                task.skipped_paths.push(worker_target.clone());
                            }
                            let has_remaining = self.queue.task(&task_id).is_some_and(|task| {
                                task_is_aggregate(task)
                                    && !remaining_task_inputs(
                                        task,
                                        self.task_run_failures.get(&task_id),
                                    )
                                    .is_empty()
                            });
                            if has_remaining {
                                let _ = self.queue.set_status(&task_id, QueueStatus::Pending);
                                continue_task = true;
                            } else {
                                let _ = self.queue.set_status(&task_id, QueueStatus::Completed);
                                let failed_count =
                                    self.task_run_failures.get(&task_id).map_or(0, HashMap::len);
                                if failed_count > 0 {
                                    let _ = self.queue.set_error(
                                        &task_id,
                                        Some(format!("{failed_count} file(s) failed")),
                                    );
                                }
                                if let Some(task) = self.queue.task_mut(&task_id) {
                                    task.complete_time = formatted_local_time();
                                }
                            }
                            self.activity = if has_remaining {
                                format!("Skipped already-encoded file; continuing task: {reason}")
                            } else {
                                format!("Skipped already-encoded file: {reason}")
                            };
                        }
                        Err(EngineError::Cancelled) => {
                            if self.queue_run_state == QueueRunState::Stopping {
                                let _ = self.queue.set_status(&task_id, QueueStatus::Pending);
                                let _ = self.queue.set_error(&task_id, None);
                                "Queue stopped; the interrupted item is pending"
                                    .clone_into(&mut self.activity);
                            } else if self.queue.task(&task_id).is_some() {
                                let _ = self.queue.set_status(&task_id, QueueStatus::Cancelled);
                                "Conversion cancelled.".clone_into(&mut self.activity);
                            } else {
                                "Removed item stopped".clone_into(&mut self.activity);
                            }
                        }
                        Err(error)
                            if worker_mode
                                .is_some_and(|mode| worker_error_is_queue_fatal(mode, &error)) =>
                        {
                            let message = error.to_string();
                            let _ = self.queue.set_status(&task_id, QueueStatus::Pending);
                            let _ = self.queue.set_error(&task_id, Some(message.clone()));
                            self.queue_run_state = QueueRunState::Idle;
                            self.activity = message;
                        }
                        Err(error) => {
                            let message = error.to_string();
                            let aggregate =
                                self.queue.task(&task_id).is_some_and(task_is_aggregate);
                            self.task_run_failures
                                .entry(task_id.clone())
                                .or_default()
                                .insert(worker_target.clone(), message);
                            let has_remaining = aggregate
                                && self.queue.task(&task_id).is_some_and(|task| {
                                    !remaining_task_inputs(
                                        task,
                                        self.task_run_failures.get(&task_id),
                                    )
                                    .is_empty()
                                });
                            if has_remaining {
                                let _ = self.queue.set_status(&task_id, QueueStatus::Pending);
                                continue_task = true;
                            } else {
                                let failed_count =
                                    self.task_run_failures.get(&task_id).map_or(0, HashMap::len);
                                let _ = self.queue.set_status(&task_id, QueueStatus::Completed);
                                let _ = self.queue.set_error(
                                    &task_id,
                                    Some(format!("{failed_count} file(s) failed")),
                                );
                                if let Some(task) = self.queue.task_mut(&task_id) {
                                    task.complete_time = formatted_local_time();
                                }
                            }
                            self.activity = if has_remaining {
                                format!(
                                    "Skipped failed file {}; continuing the task",
                                    worker_target.display()
                                )
                            } else {
                                format!(
                                    "Task finished with failed file {}",
                                    worker_target.display()
                                )
                            };
                        }
                    }
                }
            }
        }
        if finished {
            self.worker = None;
            self.progress = None;
            self.live_preview_texture = None;
            self.live_preview = None;
            self.active_source_info = None;
            self.active_stream_info = None;
            self.active_camera_info = CameraRunInfo::default();
            match self.queue_run_state {
                QueueRunState::PauseAfterCurrent
                    if continue_task || self.queue.next_pending_id().is_some() =>
                {
                    self.queue_run_state = QueueRunState::PausedBetweenFiles;
                    "Queue paused after the current item".clone_into(&mut self.activity);
                }
                QueueRunState::PauseAfterCurrentSelected if continue_task => {
                    self.queue_run_state = QueueRunState::PausedBetweenFilesSelected;
                    "Queue paused after the current item".clone_into(&mut self.activity);
                }
                QueueRunState::PauseAfterCurrent
                | QueueRunState::PauseAfterCurrentSelected
                | QueueRunState::Stopping => self.queue_run_state = QueueRunState::Idle,
                _ => {}
            }
            if let Some(task) = self.queue.task(&task_id).cloned() {
                let rename_messages = rename_completed_task_directories(&task);
                if !rename_messages.is_empty() {
                    self.activity = format!("{} · {}", self.activity, rename_messages.join(" · "));
                }
            }
            self.persist_queue();
            self.refresh_folder_summaries();
            if continue_task
                && !matches!(
                    self.queue_run_state,
                    QueueRunState::PauseAfterCurrent
                        | QueueRunState::PauseAfterCurrentSelected
                        | QueueRunState::PausedBetweenFiles
                        | QueueRunState::PausedBetweenFilesSelected
                        | QueueRunState::Stopping
                )
            {
                self.select_only(Some(task_id));
                if self.start_selected(context) {
                    return;
                }
            }
            if self.queue_run_state == QueueRunState::Running {
                self.start_next_pending(context);
            } else if self.queue_run_state == QueueRunState::RunningSelected {
                self.queue_run_state = QueueRunState::Idle;
                self.persist_queue();
            }
        }
    }

    fn handle_conversion_event(&mut self, context: &egui::Context, event: ConversionEvent) {
        match event {
            ConversionEvent::Started { input, .. } => {
                self.activity = format!("Converting {}", input.display());
            }
            ConversionEvent::Progress(progress) => self.progress = Some(progress),
            ConversionEvent::Preview(preview) => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [preview.width as usize, preview.height as usize],
                    &preview.rgba,
                );
                self.live_preview_texture = Some(context.load_texture(
                    "live-conversion-preview",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
                self.live_preview = Some(preview);
            }
            ConversionEvent::Warning(message) => self.activity = message,
            ConversionEvent::Completed { output } => {
                self.activity = format!("Validated {}", output.display());
            }
        }
    }

    fn move_selected(&mut self, delta: isize) {
        if let Some(id) = self.selected_id.clone() {
            if self.queue.task(&id).map(|task| task.status) != Some(QueueStatus::Pending) {
                "Only pending items can be reordered".clone_into(&mut self.activity);
                return;
            }
            match self.queue.move_by(&id, delta) {
                Ok(()) => self.persist_queue(),
                Err(error) => self.activity = error.to_string(),
            }
        }
    }

    fn move_task_to(&mut self, source: usize, target: usize) {
        let Some(task) = self.queue.tasks().get(source) else {
            return;
        };
        if task.status != QueueStatus::Pending {
            "Only pending tasks can be reordered".clone_into(&mut self.activity);
            return;
        }
        let id = task.id.clone();
        match self.queue.move_to(&id, target) {
            Ok(()) => {
                self.select_only(Some(id));
                "Task order updated".clone_into(&mut self.activity);
                self.persist_queue();
            }
            Err(error) => self.activity = error.to_string(),
        }
    }

    fn remove_selected(&mut self) {
        let selected = self.selected_task_ids();
        if selected.is_empty() {
            return;
        }
        let active_id = self.worker.as_ref().map(|worker| worker.task_id.clone());
        let removing_active = active_id
            .as_ref()
            .is_some_and(|active| selected.contains(active));
        let mut removed = 0_usize;
        for id in selected {
            if self.queue.remove(&id).is_ok() {
                self.task_run_failures.remove(&id);
                self.task_run_start_converted.remove(&id);
                removed += 1;
            }
        }
        if removing_active && let Some(worker) = &self.worker {
            worker.control.stop_current();
        }
        let next = self.queue.tasks().front().map(|task| task.id.clone());
        self.select_only(next);
        self.activity = if removing_active {
            format!("Removed {removed} queue item(s) and stopped the active item")
        } else {
            format!("Removed {removed} queue item(s)")
        };
        self.prune_unused_folder_watches();
        self.persist_queue();
        self.refresh_folder_summaries();
    }

    fn rerun_selected(&mut self) {
        let selected = self.selected_task_ids();
        if selected.is_empty() {
            return;
        }
        let mut reset = 0_usize;
        let mut no_remaining = 0_usize;
        let mut skipped = 0_usize;
        for id in selected {
            let Some(task) = self.queue.task(&id).cloned() else {
                continue;
            };
            let can_reset = match task.status {
                QueueStatus::Failed | QueueStatus::Cancelled => true,
                QueueStatus::Completed => task_has_remaining_work(&task),
                QueueStatus::Pending | QueueStatus::Running | QueueStatus::Paused => false,
            };
            if !can_reset {
                if task.status == QueueStatus::Completed {
                    no_remaining += 1;
                } else {
                    skipped += 1;
                }
                continue;
            }
            let _ = self.queue.set_status(&id, QueueStatus::Pending);
            let _ = self.queue.set_error(&id, None);
            if let Some(task) = self.queue.task_mut(&id) {
                task.complete_time.clear();
            }
            self.task_run_failures.remove(&id);
            self.task_run_start_converted.remove(&id);
            reset += 1;
        }
        self.activity = if reset > 0 {
            format!(
                "{reset} queue item(s) are ready to run again; {no_remaining} had no remaining work; {skipped} skipped"
            )
        } else if no_remaining > 0 {
            format!("{no_remaining} completed item(s) had no remaining work")
        } else {
            "Only completed, failed, or cancelled items can be rerun".to_owned()
        };
        self.persist_queue();
        self.refresh_folder_summaries();
    }

    fn clear_queue(&mut self) {
        if self.worker.is_some() {
            "Stop the active conversion before clearing the queue".clone_into(&mut self.activity);
            return;
        }
        self.queue_run_state = QueueRunState::Idle;
        self.queue.clear();
        self.task_run_failures.clear();
        self.task_run_start_converted.clear();
        self.watched_folders.clear();
        self.folder_summaries.clear();
        self.select_only(None);
        "Queue cleared".clone_into(&mut self.activity);
        self.persist_queue();
    }

    fn persist_queue(&mut self) {
        if let Err(error) = self.state_store.save_queue(
            &self.queue,
            self.worker.is_some() || self.queue_run_state != QueueRunState::Idle,
            &self.task_run_failures,
        ) {
            self.activity = format!("Unable to save queue: {error}");
        }
    }
}

/// Temporary presentation bridge used by the Slint frontend. The conversion
/// controller remains the source of truth while the old egui presentation is
/// retired incrementally.
#[derive(Default)]
pub struct SlintController {
    app: ConverterApp,
    context: egui::Context,
    task_draft: Option<SlintTaskDraft>,
    history_filter: String,
}

#[derive(Clone, Debug)]
struct SlintTaskDraft {
    settings: QueueSettings,
    targets: Vec<PathBuf>,
}

fn task_draft_output_summary(draft: &SlintTaskDraft) -> String {
    let Some(first) = draft.targets.first() else {
        return "Add media to see the exact output and original-file safety plan.".to_owned();
    };
    let mode = draft.settings.mode;
    let output = if mode == ContentMode::Trim {
        trim_output_path(
            first,
            draft.settings.trim_start.unwrap_or_default(),
            draft.settings.trim_end.unwrap_or_default(),
        )
        .display()
        .to_string()
    } else if mode == ContentMode::Stabilize {
        if first.is_dir() {
            format!("{} (one *_stabilized file per source)", first.display())
        } else {
            stabilized_output_path(first).display().to_string()
        }
    } else if mode == ContentMode::PhotoSlideshow {
        next_slideshow_output_path(first).display().to_string()
    } else if first.is_dir() {
        let extension = descriptor(mode, draft.settings.encoder)
            .container
            .extension();
        format!("{} (one .{extension} file per source)", first.display())
    } else {
        conversion_output_path(first, descriptor(mode, draft.settings.encoder).container)
            .display()
            .to_string()
    };
    let source_size = first
        .metadata()
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map_or_else(
            || "Folder size is checked while the task is scanned.".to_owned(),
            |metadata| {
                format!(
                    "Selected source size: {} MB.",
                    format_size_mb(Some(metadata.len()))
                )
            },
        );
    let safety = if matches!(
        mode,
        ContentMode::Tv | ContentMode::Animation | ContentMode::CameraVideos
    ) {
        "After the new file is validated, the original is moved into an original folder; failed conversions leave it in place."
    } else {
        "The source remains in place; the result is published only after validation."
    };
    format!("Output: {output}\n{safety}\n{source_size}")
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SlintTaskSnapshot {
    pub title: String,
    pub subtitle: String,
    pub status: String,
    pub completed_before: f32,
    pub completed_this_run: f32,
    pub current_item_progress: f32,
    pub has_progress: bool,
    pub progress_label: String,
    pub progress_summary: String,
    pub completed_before_label: String,
    pub completed_this_run_label: String,
    pub remaining_label: String,
    pub selected: bool,
    pub active: bool,
    pub can_run: bool,
    pub can_retry: bool,
    pub can_reorder: bool,
    pub error_detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlintTaskFileSnapshot {
    pub title: String,
    pub path: String,
    pub status: String,
    pub started_time: String,
    pub completed_time: String,
    pub conversion_time: String,
    pub original_size: String,
    pub new_size: String,
    pub original_fps: String,
    pub new_fps: String,
    pub codec: String,
    pub duration: String,
    pub error_detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SlintHistorySnapshot {
    pub title: String,
    pub path: String,
    pub subtitle: String,
    pub detail: String,
    pub configuration: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlintLiveStatusSnapshot {
    pub position: String,
    pub duration: String,
    pub frames: String,
    pub conversion_fps: String,
    pub conversion_speed: String,
    pub original_fps: String,
    pub target_fps: String,
    pub encoder: String,
    pub quality: String,
    pub preset: String,
    pub carried_audio: String,
    pub carried_subtitles: String,
    pub spent: String,
    pub estimated_total: String,
    pub remaining: String,
    pub app_cpu: String,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SlintSettingsSnapshot {
    pub mode_index: usize,
    pub mode_labels: Vec<String>,
    pub encoder_index: usize,
    pub encoder_labels: Vec<String>,
    pub show_encoder: bool,
    pub fps_index: usize,
    pub fps_labels: Vec<String>,
    pub show_fps: bool,
    pub explicit_fps: f32,
    pub show_explicit_fps: bool,
    pub quality: f32,
    pub show_quality: bool,
    pub quality_level: String,
    pub quality_guide_labels: Vec<String>,
    pub quality_minimum: f32,
    pub quality_maximum: f32,
    pub quality_step: f32,
    pub speed: String,
    pub speed_labels: Vec<String>,
    pub speed_help: String,
    pub speed_index: usize,
    pub show_speed: bool,
    pub prevent_sleep: bool,
    pub trim_start: String,
    pub trim_end: String,
    pub apply_lut: bool,
    pub stabilize_index: usize,
    pub slideshow_interval: f32,
    pub slideshow_fps: i32,
    pub slideshow_resolution_index: usize,
    pub slideshow_collage: bool,
    pub slideshow_audio_labels: Vec<String>,
    pub slideshow_audio_selected: i32,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct SlintAppSnapshot {
    pub tasks: Vec<SlintTaskSnapshot>,
    pub selected_task_title: String,
    pub selected_task_files: Vec<SlintTaskFileSnapshot>,
    pub selected_task_error_detail: String,
    pub history: Vec<SlintHistorySnapshot>,
    pub settings: SlintSettingsSnapshot,
    pub activity: String,
    pub engine_status: String,
    pub active_title: String,
    pub active_detail: String,
    pub live_status: SlintLiveStatusSnapshot,
    pub cpu_limit_index: usize,
    pub cpu_limit_summary: String,
    pub cpu_usage: String,
    pub progress: f32,
    pub is_running: bool,
    pub is_paused: bool,
    pub paused_between_files: bool,
    pub pending_count: usize,
    pub completed_count: usize,
    pub attention_count: usize,
    pub history_total_count: usize,
    pub pause_after_current: bool,
    pub completion_notice: String,
    pub selected_index: i32,
    pub selected_can_move: bool,
    pub preview_enabled: bool,
    pub live_preview: Option<ConversionPreview>,
    pub task_draft_targets: Vec<String>,
    pub task_draft_summary: String,
    pub task_draft_output_summary: String,
}

fn displayed_task_settings(
    app: &ConverterApp,
    paused_between_files: bool,
    editing_task: bool,
) -> Option<&QueueSettings> {
    if editing_task {
        return None;
    }
    app.worker
        .as_ref()
        .and_then(|worker| app.queue.task(&worker.task_id))
        .or_else(|| {
            paused_between_files
                .then_some(app.selected_id.as_deref())
                .flatten()
                .and_then(|id| app.queue.task(id))
        })
        .map(|task| &task.settings)
}

fn task_file_title(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn unavailable_task_file(path: &std::path::Path, status: &str) -> SlintTaskFileSnapshot {
    let path = path.display().to_string();
    SlintTaskFileSnapshot {
        title: task_file_title(&path),
        path,
        status: status.to_owned(),
        started_time: "-".to_owned(),
        completed_time: "-".to_owned(),
        conversion_time: "-".to_owned(),
        original_size: "-".to_owned(),
        new_size: "-".to_owned(),
        original_fps: "-".to_owned(),
        new_fps: "-".to_owned(),
        codec: "-".to_owned(),
        duration: "-".to_owned(),
        error_detail: String::new(),
    }
}

fn completed_task_file(row: &CompletedHistoryRow) -> SlintTaskFileSnapshot {
    let columns = row.columns();
    let path = columns[0].clone();
    let codec = match (row.original_codec(), row.converted_codec()) {
        (Some(original), Some(converted)) if original != converted => {
            format!("{original} → {converted}")
        }
        (_, Some(converted)) => converted.to_owned(),
        (Some(original), None) => original.to_owned(),
        (None, None) => columns[11].clone(),
    };
    SlintTaskFileSnapshot {
        title: task_file_title(&path),
        path,
        status: "Completed".to_owned(),
        started_time: columns[3].clone(),
        completed_time: columns[4].clone(),
        conversion_time: if columns[5] == "-" {
            "-".to_owned()
        } else {
            format!("{} min", columns[5])
        },
        original_size: columns[6].clone(),
        new_size: columns[7].clone(),
        original_fps: columns[9].clone(),
        new_fps: columns[10].clone(),
        codec,
        duration: row.duration().unwrap_or("-").to_owned(),
        error_detail: String::new(),
    }
}

fn path_components_lowercase(path: &std::path::Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

fn path_is_within(path: &std::path::Path, root: &std::path::Path) -> bool {
    let path = path_components_lowercase(path);
    let root = path_components_lowercase(root);
    path.starts_with(&root)
}

fn history_path_matches_target(path: &std::path::Path, target: &std::path::Path) -> bool {
    if target.is_dir() || !has_video_filename(target) {
        return path_is_within(path, target);
    }
    let path_parent = path.parent().map(path_components_lowercase);
    let target_parent = target.parent().map(path_components_lowercase);
    let path_stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_lowercase());
    let target_stem = target
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_lowercase());
    path_parent == target_parent && path_stem == target_stem
}

fn history_row_matches_task(row: &CompletedHistoryRow, task: &QueueTask) -> bool {
    if let Some(task_id) = row.task_id() {
        return task_id == task.id;
    }
    let columns = row.columns();
    let expected_encoder = if task.settings.mode == ContentMode::Trim {
        "Stream copy"
    } else {
        task.settings.encoder.user_name()
    };
    if columns[1] != task.settings.mode.label() || columns[11] != expected_encoder {
        return false;
    }
    let history_path = std::path::Path::new(row.input_path().unwrap_or(&columns[0]));
    task.source_root
        .iter()
        .chain(&task.targets)
        .any(|target| history_path_matches_target(history_path, target))
}

fn completed_directory_video_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|value| value.to_str());
                if !matches!(name, Some("original" | "chs")) {
                    pending.push(path);
                }
            } else if is_video_path(&path) {
                files.push(path);
            }
        }
    }
    files
}

fn folder_task_detail_files(
    root: &std::path::Path,
    settings: &QueueSettings,
) -> Vec<(PathBuf, bool)> {
    if !root.is_dir() {
        return renamed_conversion_directory(root, settings).map_or_else(Vec::new, |renamed| {
            completed_directory_video_files(&renamed)
                .into_iter()
                .map(|path| (path, true))
                .collect()
        });
    }
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|value| value.to_str());
                if name == Some("original") {
                    continue;
                }
                if is_skipped_conversion_directory(&path) {
                    if settings.mode != ContentMode::CameraVideos {
                        files.extend(
                            collect_all_video_files(&path)
                                .into_iter()
                                .map(|path| (path, true)),
                        );
                    }
                } else {
                    pending.push(path);
                }
            } else if is_video_path(&path) {
                let completed = should_skip_queue_source(&path, settings);
                files.push((path, completed));
            }
        }
    }
    files
}

fn task_detail_files(task: &QueueTask) -> Vec<(PathBuf, bool)> {
    let mut files = task
        .targets
        .iter()
        .flat_map(|target| {
            if target.is_dir() || !has_video_filename(target) {
                folder_task_detail_files(target, &task.settings)
            } else {
                vec![(
                    target.clone(),
                    task.status == QueueStatus::Completed
                        || task.skipped_paths.contains(target)
                        || should_skip_queue_source(target, &task.settings),
                )]
            }
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|(path, _)| full_path_natural_key(path));
    files
}

fn selected_task_files(
    task: &QueueTask,
    history: &[CompletedHistoryRow],
    failures: Option<&HashMap<PathBuf, String>>,
) -> Vec<SlintTaskFileSnapshot> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for row in history
        .iter()
        .filter(|row| history_row_matches_task(row, task))
    {
        let file = completed_task_file(row);
        seen.insert(file.path.to_lowercase());
        if let Some(input_path) = row.input_path() {
            seen.insert(input_path.to_lowercase());
        }
        files.push(file);
    }

    for path in &task.skipped_paths {
        if seen.insert(path.display().to_string().to_lowercase()) {
            files.push(unavailable_task_file(path, "Completed"));
        }
    }

    if task.settings.mode != ContentMode::PhotoSlideshow {
        for (path, completed) in task_detail_files(task) {
            if seen.insert(path.display().to_string().to_lowercase()) {
                let status = if completed || task.status == QueueStatus::Completed {
                    "Completed"
                } else {
                    "Queued"
                };
                files.push(unavailable_task_file(&path, status));
            }
        }
    }

    for path in task
        .targets
        .iter()
        .filter(|path| has_video_filename(path) || is_photo_path(path))
    {
        if seen.insert(path.display().to_string().to_lowercase()) {
            let status = if task.status == QueueStatus::Completed {
                "Completed"
            } else {
                "Queued"
            };
            files.push(unavailable_task_file(path, status));
        }
    }

    if files.is_empty() {
        for path in &task.targets {
            if seen.insert(path.display().to_string().to_lowercase()) {
                let status = if task.status == QueueStatus::Completed {
                    "Completed"
                } else {
                    "Queued"
                };
                files.push(unavailable_task_file(path, status));
            }
        }
    }

    if let Some(failures) = failures {
        let failures = failures
            .iter()
            .map(|(path, error)| (summary_path_key(path), (path, error)))
            .collect::<HashMap<_, _>>();
        for file in &mut files {
            if let Some((_, error)) =
                failures.get(&summary_path_key(std::path::Path::new(&file.path)))
            {
                "Failed".clone_into(&mut file.status);
                file.error_detail.clone_from(error);
            }
        }
        for (key, (path, error)) in failures {
            if !files
                .iter()
                .any(|file| summary_path_key(std::path::Path::new(&file.path)) == key)
            {
                let mut file = unavailable_task_file(path, "Failed");
                file.error_detail.clone_from(error);
                files.push(file);
            }
        }
    }
    files.sort_by_key(|file| full_path_natural_key(std::path::Path::new(&file.path)));
    files
}

#[derive(Clone, Copy)]
struct QualityScale {
    minimum: f32,
    maximum: f32,
    step: f32,
    bands: &'static [(u8, u8, &'static str)],
}

fn quality_scale(encoder: Encoder) -> QualityScale {
    // The limits match VideoFerry's pinned FFmpeg build; the UI intentionally uses whole numbers.
    // The friendly bands use each encoder's documented default or recommended starting point.
    match encoder {
        Encoder::X264 => QualityScale {
            minimum: 0.0,
            maximum: 51.0,
            step: 1.0,
            bands: &[
                (0, 17, "Max detail"),
                (18, 22, "High detail"),
                (23, 27, "Balanced"),
                (28, 35, "Smaller file"),
                (36, 51, "Smallest file"),
            ],
        },
        Encoder::X265 => QualityScale {
            minimum: 0.0,
            maximum: 51.0,
            step: 1.0,
            bands: &[
                (0, 17, "Max detail"),
                (18, 25, "High detail"),
                (26, 31, "Balanced"),
                (32, 39, "Smaller file"),
                (40, 51, "Smallest file"),
            ],
        },
        Encoder::SvtAv1 => QualityScale {
            minimum: 0.0,
            maximum: 63.0,
            step: 1.0,
            bands: &[
                (0, 19, "Max detail"),
                (20, 27, "High detail"),
                (28, 35, "Balanced"),
                (36, 47, "Smaller file"),
                (48, 63, "Smallest file"),
            ],
        },
        _ => QualityScale {
            minimum: 0.0,
            maximum: 63.0,
            step: 1.0,
            bands: &[],
        },
    }
}

fn normalized_quality(encoder: Encoder, quality: f32) -> f32 {
    let scale = quality_scale(encoder);
    let clamped = quality.clamp(scale.minimum, scale.maximum);
    let steps = ((clamped - scale.minimum) / scale.step).round();
    (scale.minimum + steps * scale.step).clamp(scale.minimum, scale.maximum)
}

fn quality_guidance(encoder: Encoder, quality: f32) -> (String, Vec<String>) {
    let bands = quality_scale(encoder).bands;
    let level = bands
        .iter()
        .find(|(_, end, _)| quality <= f32::from(*end))
        .map_or("Smallest file", |(_, _, label)| *label)
        .to_owned();
    let labels = bands
        .iter()
        .map(|(start, end, label)| format!("{start}\u{2013}{end}\n{label}"))
        .collect();
    (level, labels)
}

#[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
fn slint_settings_snapshot(
    app: &ConverterApp,
    task_settings: Option<&QueueSettings>,
) -> SlintSettingsSnapshot {
    let mode = task_settings.map_or(app.mode, |settings| settings.mode);
    let encoder = task_settings.map_or(app.encoder, |settings| settings.encoder);
    let fps_mode = task_settings.map_or(app.fps_mode, |settings| fps_ui_mode(settings.fps));
    let explicit_fps =
        task_settings.map_or(app.explicit_fps, |settings| explicit_fps(settings.fps));
    let quality = task_settings
        .and_then(|settings| settings.quality)
        .unwrap_or(app.quality_crf);
    let quality_scale = quality_scale(encoder);
    let quality = normalized_quality(encoder, quality);
    let (quality_level, quality_guide_labels) = quality_guidance(encoder, quality);
    let speed = task_settings.map_or_else(
        || app.speed_preset.clone(),
        |settings| settings.speed_preset.clone().unwrap_or_default(),
    );
    let mut encoders = if mode == ContentMode::Trim {
        vec![Encoder::X265]
    } else {
        app.available_encoders.clone()
    };
    if encoders.is_empty() {
        encoders.push(Encoder::X265);
    }
    if !encoders.contains(&encoder) {
        encoders.push(encoder);
    }
    let fps_modes = fps_modes_for(mode, encoder);
    let speed_options = speed_options(encoder);
    let speed_labels = speed_option_labels(encoder);
    let audio_paths = task_settings.map_or(app.slideshow_audio_paths.as_slice(), |settings| {
        settings.slideshow_audio_paths.as_slice()
    });
    SlintSettingsSnapshot {
        mode_index: ContentMode::ALL
            .iter()
            .position(|candidate| *candidate == mode)
            .unwrap_or_default(),
        mode_labels: ContentMode::ALL
            .into_iter()
            .map(|candidate| candidate.label().to_owned())
            .collect(),
        encoder_index: encoders
            .iter()
            .position(|candidate| *candidate == encoder)
            .unwrap_or_default(),
        encoder_labels: encoders
            .into_iter()
            .map(|candidate| candidate.user_name().to_owned())
            .collect(),
        show_encoder: workflow_shows_encoder(mode),
        fps_index: fps_modes
            .iter()
            .position(|candidate| *candidate == fps_mode)
            .unwrap_or_default(),
        fps_labels: fps_modes
            .into_iter()
            .map(|candidate| candidate.label().to_owned())
            .collect(),
        show_fps: workflow_shows_fps(mode),
        explicit_fps: explicit_fps as f32,
        show_explicit_fps: fps_mode == FpsUiMode::Explicit,
        quality,
        show_quality: matches!(encoder, Encoder::X264 | Encoder::X265 | Encoder::SvtAv1)
            && mode != ContentMode::Trim,
        quality_level,
        quality_guide_labels,
        quality_minimum: quality_scale.minimum,
        quality_maximum: quality_scale.maximum,
        quality_step: quality_scale.step,
        speed_index: speed_options
            .iter()
            .position(|candidate| candidate == &speed)
            .unwrap_or_default(),
        speed,
        speed_labels,
        speed_help: speed_help(encoder),
        show_speed: (matches!(encoder, Encoder::X264 | Encoder::X265 | Encoder::SvtAv1)
            || encoder.is_nvenc())
            && mode != ContentMode::Trim,
        prevent_sleep: app.sleep.enabled,
        trim_start: task_settings.map_or_else(
            || app.trim_start_text.clone(),
            |settings| format_trim_time(settings.trim_start.unwrap_or_default()),
        ),
        trim_end: task_settings.map_or_else(
            || app.trim_end_text.clone(),
            |settings| format_trim_time(settings.trim_end.unwrap_or(Duration::from_secs(1))),
        ),
        apply_lut: task_settings.map_or(app.apply_lut, |settings| settings.apply_lut),
        stabilize_index: ["Gentle", "Balanced", "Steady", "Strong", "Maximum"]
            .iter()
            .position(|strength| {
                *strength
                    == task_settings.map_or(app.stabilize_strength.as_str(), |settings| {
                        settings.stabilize_strength.as_str()
                    })
            })
            .unwrap_or(1),
        slideshow_interval: task_settings.map_or(app.slideshow_interval_seconds, |settings| {
            settings.photo_interval.as_secs_f32()
        }),
        slideshow_fps: i32::try_from(
            task_settings.map_or(app.slideshow_fps, |settings| settings.slideshow_fps),
        )
        .unwrap_or(i32::MAX),
        slideshow_resolution_index: usize::from(
            task_settings.map_or(app.slideshow_resolution == "4K", |settings| {
                settings.slideshow_resolution == (3840, 2160)
            }),
        ),
        slideshow_collage: task_settings
            .map_or(app.slideshow_collage, |settings| settings.slideshow_collage),
        slideshow_audio_labels: audio_paths
            .iter()
            .map(|path| {
                path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                )
            })
            .collect(),
        slideshow_audio_selected: if task_settings.is_some() {
            -1
        } else {
            app.slideshow_audio_selected
                .and_then(|index| i32::try_from(index).ok())
                .unwrap_or(-1)
        },
    }
}

impl SlintController {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tick(&mut self) {
        self.app.poll_engine_discovery(&self.context);
        self.app.poll_worker(&self.context);
        self.app.refresh_process_cpu_usage();
        if self.app.worker.is_none() && self.app.resume_task_id.take().is_some() {
            "Resuming persisted queue".clone_into(&mut self.app.activity);
            self.app.start_queue(&self.context);
        }
        self.app.refresh_folder_watches();
        self.app.refresh_completed_history(&self.context);
        self.app.capture_activity_log(&self.context);
        self.app.update_sleep_inhibitor();
        self.app.sync_preferences();
        self.app
            .platform_indicator
            .set_converting(self.app.worker.is_some());
    }

    pub fn begin_task_draft(&mut self) {
        self.task_draft = Some(SlintTaskDraft {
            settings: self.app.current_settings(),
            targets: Vec::new(),
        });
        "Choose how this task should be converted".clone_into(&mut self.app.activity);
    }

    pub fn prepare_task_draft(&mut self) -> bool {
        let settings = match self.app.validated_current_settings() {
            Ok(settings) => settings,
            Err(error) => {
                self.app.activity = error;
                return false;
            }
        };
        let targets = self
            .task_draft
            .take()
            .map_or_else(Vec::new, |draft| draft.targets);
        self.task_draft = Some(SlintTaskDraft { settings, targets });
        "Configuration saved — add files or folders".clone_into(&mut self.app.activity);
        true
    }

    pub fn cancel_task_draft(&mut self) {
        self.task_draft = None;
        "Task creation cancelled".clone_into(&mut self.app.activity);
    }

    pub fn add_task_draft_paths(&mut self, paths: Vec<PathBuf>) {
        if self.task_draft.is_none() && !self.prepare_task_draft() {
            return;
        }
        let occupied = queue_admission_occupied_paths(&self.app.queue, &self.app.watched_folders);
        let Some(draft) = self.task_draft.as_mut() else {
            return;
        };
        let mut added = 0_usize;
        let mut skipped = 0_usize;
        for path in paths {
            let supported = match draft.settings.mode {
                ContentMode::Trim => is_video_path(&path),
                ContentMode::PhotoSlideshow => path.is_dir() || is_photo_path(&path),
                ContentMode::Tv
                | ContentMode::Animation
                | ContentMode::CameraVideos
                | ContentMode::Stabilize => path.is_dir() || is_video_path(&path),
            };
            let trim_full = draft.settings.mode == ContentMode::Trim && !draft.targets.is_empty();
            if !supported || trim_full || occupied.contains(&path) || draft.targets.contains(&path)
            {
                skipped += 1;
                continue;
            }
            draft.targets.push(path);
            added += 1;
        }
        self.app.activity = if skipped > 0 {
            format!("Added {added} target(s); skipped {skipped} unsupported or duplicate item(s)")
        } else {
            format!("Added {added} target(s) to the new task")
        };
    }

    pub fn move_task_draft_target(&mut self, index: usize, delta: isize) -> Option<usize> {
        let draft = self.task_draft.as_mut()?;
        let destination = index.checked_add_signed(delta)?;
        if index >= draft.targets.len() || destination >= draft.targets.len() {
            return None;
        }
        draft.targets.swap(index, destination);
        Some(destination)
    }

    pub fn remove_task_draft_target(&mut self, index: usize) -> bool {
        let Some(draft) = self.task_draft.as_mut() else {
            return false;
        };
        if index >= draft.targets.len() {
            return false;
        }
        draft.targets.remove(index);
        "Removed target from the new task".clone_into(&mut self.app.activity);
        true
    }

    pub fn create_task_from_draft(&mut self, requested_name: &str) -> bool {
        let Some(mut draft) = self.task_draft.take() else {
            "Choose the task configuration first".clone_into(&mut self.app.activity);
            return false;
        };
        let occupied = queue_admission_occupied_paths(&self.app.queue, &self.app.watched_folders);
        let original_targets = draft.targets.clone();
        let (targets, skipped) = if draft.settings.mode == ContentMode::PhotoSlideshow {
            let mut seen = HashSet::new();
            let mut skipped = 0_usize;
            let targets = draft
                .targets
                .into_iter()
                .filter(|path| {
                    let keep = !occupied.contains(path) && seen.insert(path.clone());
                    if !keep {
                        skipped += 1;
                    }
                    keep
                })
                .collect::<Vec<_>>();
            let mut images = targets
                .iter()
                .flat_map(|target| collect_photo_files(target))
                .collect::<Vec<_>>();
            images.sort_by_key(|path| slideshow_natural_key(path));
            images.dedup();
            draft.settings.slideshow_image_paths.clone_from(&images);
            draft.settings.slideshow_review_image_paths = images;
            (targets, skipped)
        } else {
            collect_new_video_task_targets(draft.targets, &draft.settings, &occupied)
        };

        if let Err(error) = validate_task_targets(&draft.settings, &targets) {
            draft.targets = original_targets;
            self.task_draft = Some(draft);
            self.app.activity = error;
            return false;
        }

        let Some(first_target) = targets.first().cloned() else {
            "Add at least one target path.".clone_into(&mut self.app.activity);
            self.task_draft = Some(SlintTaskDraft {
                settings: draft.settings,
                targets: original_targets,
            });
            return false;
        };
        let name = if requested_name.trim().is_empty() {
            default_task_name(&draft.settings, &first_target)
        } else {
            requested_name.trim().to_owned()
        };
        let watch_roots = targets
            .iter()
            .filter(|path| path.is_dir())
            .cloned()
            .collect::<Vec<_>>();
        let id = format!("task-{}", self.app.next_id);
        self.app.next_id += 1;
        let source_root = (targets.len() == 1 && first_target.is_dir()).then_some(first_target);
        let mut task = QueueTask::new(&id, name, targets, draft.settings.clone());
        task.source_root = source_root;
        task.queued_time = formatted_local_time();
        if let Err(error) = self.app.queue.add(task) {
            self.app.activity = error.to_string();
            self.task_draft = Some(SlintTaskDraft {
                settings: draft.settings,
                targets: original_targets,
            });
            return false;
        }
        self.app.select_only(Some(id));
        for root in watch_roots {
            self.app.register_folder_watch(root, draft.settings.clone());
        }
        self.app.persist_queue();
        self.app.refresh_folder_summaries();
        self.app.activity = if skipped > 0 {
            format!("Task created; skipped {skipped} duplicate target(s)")
        } else {
            "Task created and added to the queue".to_owned()
        };
        true
    }

    pub fn select_task(&mut self, index: usize) {
        let id = self
            .app
            .queue
            .tasks()
            .get(index)
            .map(|task| task.id.clone());
        self.app.select_only(id);
    }

    pub fn start_queue(&mut self) {
        if self.app.worker.is_none() {
            self.app.start_queue(&self.context);
        }
    }

    pub fn start_selected(&mut self) {
        if self.app.worker.is_none() {
            self.app.queue_run_state = QueueRunState::RunningSelected;
            self.app.start_selected(&self.context);
        }
    }

    pub fn toggle_pause(&mut self) {
        if self.app.queue_run_state == QueueRunState::PausedBetweenFiles {
            self.app.queue_run_state = QueueRunState::Running;
            "Queue resumed".clone_into(&mut self.app.activity);
            self.app.persist_queue();
            self.app.start_next_pending(&self.context);
            self.app.update_sleep_inhibitor();
            return;
        }
        if self.app.queue_run_state == QueueRunState::PausedBetweenFilesSelected {
            self.app.queue_run_state = QueueRunState::RunningSelected;
            "Task resumed".clone_into(&mut self.app.activity);
            self.app.persist_queue();
            self.app.start_selected(&self.context);
            self.app.update_sleep_inhibitor();
            return;
        }
        let Some(worker) = &mut self.app.worker else {
            return;
        };
        let status = if worker.paused {
            worker.control.resume();
            if let Some(paused_at) = worker.paused_at.take() {
                worker.paused_duration += paused_at.elapsed();
            }
            QueueStatus::Running
        } else {
            worker.control.pause();
            worker.paused_at = Some(Instant::now());
            QueueStatus::Paused
        };
        worker.paused = !worker.paused;
        let task_id = worker.task_id.clone();
        let _ = self.app.queue.set_status(&task_id, status);
        self.app.activity = if worker.paused {
            "Conversion paused".to_owned()
        } else {
            "Conversion resumed".to_owned()
        };
        self.app.update_sleep_inhibitor();
        self.app.persist_queue();
    }

    pub fn set_live_preview(&mut self, enabled: bool) {
        self.app.frame_preview_enabled = enabled;
        if let Some(worker) = &self.app.worker {
            worker.control.set_preview_enabled(enabled);
        }
        if !enabled {
            self.app.live_preview_texture = None;
            self.app.live_preview = None;
        }
    }

    pub fn set_cpu_limit(&mut self, index: usize) {
        let Some(cpu_limit) = CpuLimitLevel::from_index(index) else {
            return;
        };
        if cpu_limit == self.app.cpu_limit {
            return;
        }
        self.app.cpu_limit = cpu_limit;
        let threads = self.app.cpu_thread_limit();
        let active = self.app.worker.is_some();
        if let Some(worker) = &self.app.worker {
            worker.control.set_cpu_thread_limit(threads);
        }
        self.app.activity = match active
            .then(|| self.app.cpu_limiter.set_thread_limit(threads))
            .transpose()
        {
            Ok(Some(())) => format!("CPU limit set to {}", self.app.cpu_limit_summary()),
            Ok(None) => format!("CPU limit saved: {}", self.app.cpu_limit_summary()),
            Err(error) => {
                format!("CPU limit saved for the next file; live adjustment unavailable: {error}")
            }
        };
        self.app.sync_preferences();
    }

    pub fn pause_after_current(&mut self) {
        if self.app.worker.is_none() {
            return;
        }
        match self.app.queue_run_state {
            QueueRunState::PauseAfterCurrent => {
                self.app.queue_run_state = QueueRunState::Running;
                "Scheduled pause cancelled".clone_into(&mut self.app.activity);
            }
            QueueRunState::PauseAfterCurrentSelected => {
                self.app.queue_run_state = QueueRunState::RunningSelected;
                "Scheduled pause cancelled".clone_into(&mut self.app.activity);
            }
            QueueRunState::Running => {
                self.app.queue_run_state = QueueRunState::PauseAfterCurrent;
                "Queue will pause after this video".clone_into(&mut self.app.activity);
            }
            QueueRunState::RunningSelected | QueueRunState::Idle => {
                self.app.queue_run_state = QueueRunState::PauseAfterCurrentSelected;
                "Task will pause after this video".clone_into(&mut self.app.activity);
            }
            QueueRunState::PausedBetweenFiles
            | QueueRunState::PausedBetweenFilesSelected
            | QueueRunState::Stopping => {}
        }
        self.app.persist_queue();
    }

    pub fn stop_current(&mut self) {
        if let Some(worker) = &self.app.worker {
            worker.control.stop_current();
            "Stopping this video; the rest of the queue will continue"
                .clone_into(&mut self.app.activity);
            self.app.persist_queue();
        }
    }

    pub fn stop_all(&mut self) {
        if matches!(
            self.app.queue_run_state,
            QueueRunState::PausedBetweenFiles | QueueRunState::PausedBetweenFilesSelected
        ) {
            self.app.queue_run_state = QueueRunState::Idle;
            "Queue stopped; queued files remain".clone_into(&mut self.app.activity);
            self.app.persist_queue();
            return;
        }
        if let Some(worker) = &self.app.worker {
            worker.control.stop_all();
            self.app.queue_run_state = QueueRunState::Stopping;
            "Stopping conversion safely".clone_into(&mut self.app.activity);
            self.app.persist_queue();
        }
    }

    pub fn move_selected(&mut self, delta: isize) {
        self.app.move_selected(delta);
    }

    pub fn move_task_to(&mut self, source: usize, target: usize) {
        self.app.move_task_to(source, target);
    }

    pub fn remove_selected(&mut self) {
        self.app.remove_selected();
    }

    pub fn retry_selected(&mut self) {
        self.app.rerun_selected();
    }

    pub fn clear_queue(&mut self) {
        self.app.clear_queue();
    }

    pub fn clear_history(&mut self) {
        match self.app.state_store.clear_history() {
            Ok(()) => {
                self.app.completed_history.clear();
                "Finished history cleared".clone_into(&mut self.app.activity);
            }
            Err(error) => {
                self.app.activity = format!("Unable to clear history: {error}");
            }
        }
    }

    pub fn open_history_item(&mut self, path: &str) {
        let path = PathBuf::from(path);
        if !path.is_file() {
            self.app.activity = format!("Converted file was not found: {}", path.display());
            return;
        }
        match open_path_with_default_app(&path) {
            Ok(()) => self.app.activity = format!("Opening {}", path.display()),
            Err(error) => {
                self.app.activity = format!("Unable to open {}: {error}", path.display());
            }
        }
    }

    pub fn reveal_history_item(&mut self, path: &str) {
        let path = PathBuf::from(path);
        if !path.exists() {
            self.app.activity = format!("Converted file was not found: {}", path.display());
            return;
        }
        match reveal_path_in_file_manager(&path) {
            Ok(()) => self.app.activity = format!("Showing {}", path.display()),
            Err(error) => {
                self.app.activity = format!("Unable to show {}: {error}", path.display());
            }
        }
    }

    pub fn set_history_filter(&mut self, filter: String) {
        self.history_filter = filter;
    }

    pub fn report_clipboard_result(&mut self, label: &str, error: Option<&str>) {
        self.app.activity = error.map_or_else(
            || format!("Copied {label}"),
            |error| format!("Unable to copy {label}: {error}"),
        );
    }

    pub fn dismiss_completion(&mut self) {
        self.app.completion_notice.clear();
    }

    pub fn set_mode(&mut self, index: usize) {
        let Some(mode) = ContentMode::ALL.get(index).copied() else {
            return;
        };
        let old_mode = self.app.mode;
        if mode == old_mode {
            return;
        }
        self.app
            .remember_encoding_settings(old_mode, self.app.encoder);
        let previous = self.app.settings_from_ui(old_mode, self.app.encoder);
        self.app.settings_by_mode.insert(old_mode, previous);
        self.app.mode = mode;
        let next = self
            .app
            .settings_by_mode
            .get(&mode)
            .cloned()
            .unwrap_or_else(|| default_settings(mode, Encoder::X265));
        self.app.apply_settings_to_ui(&next);
        if mode == ContentMode::Trim {
            self.app.encoder = Encoder::X265;
        }
        self.app.sync_preferences();
    }

    pub fn set_encoder(&mut self, index: usize) {
        let encoders = self.app.allowed_encoders();
        let Some(encoder) = encoders.get(index).copied() else {
            return;
        };
        if encoder == self.app.encoder {
            return;
        }
        let previous = self.app.encoder;
        self.app.remember_encoding_settings(self.app.mode, previous);
        self.app.encoder = encoder;
        let next =
            encoding_ui_settings_for(&self.app.encoding_settings, self.app.mode, self.app.encoder);
        self.app.apply_quality_settings_to_ui(&next);
        self.app.sync_preferences();
    }

    pub fn set_quality(&mut self, quality: f32) {
        self.app.quality_crf = normalized_quality(self.app.encoder, quality);
        self.app.sync_preferences();
    }

    pub fn set_fps_mode(&mut self, index: usize) {
        let modes = self.fps_modes();
        let Some(mode) = modes.get(index).copied() else {
            return;
        };
        self.app.fps_mode = mode;
        self.app.sync_preferences();
    }

    pub fn set_explicit_fps(&mut self, fps: f32) {
        if fps.is_finite() && fps > 0.0 {
            self.app.explicit_fps = f64::from(fps);
            self.app.sync_preferences();
        }
    }

    pub fn set_trim_start(&mut self, value: String) {
        self.app.trim_start_text = value;
        self.app.sync_preferences();
    }

    pub fn set_trim_end(&mut self, value: String) {
        self.app.trim_end_text = value;
        self.app.sync_preferences();
    }

    pub fn set_apply_lut(&mut self, enabled: bool) {
        self.app.apply_lut = enabled;
        self.app.sync_preferences();
    }

    pub fn set_stabilize_strength(&mut self, index: usize) {
        const STRENGTHS: [&str; 5] = ["Gentle", "Balanced", "Steady", "Strong", "Maximum"];
        let Some(strength) = STRENGTHS.get(index) else {
            return;
        };
        (*strength).clone_into(&mut self.app.stabilize_strength);
        self.app.sync_preferences();
    }

    pub fn set_slideshow_interval(&mut self, seconds: f32) {
        if seconds.is_finite() && seconds > 0.0 {
            self.app.slideshow_interval_seconds = seconds;
            self.app.sync_preferences();
        }
    }

    pub fn set_slideshow_fps(&mut self, fps: i32) {
        if let Ok(fps) = u32::try_from(fps.max(1)) {
            self.app.slideshow_fps = fps;
            self.app.sync_preferences();
        }
    }

    pub fn set_slideshow_resolution(&mut self, index: usize) {
        let Some(resolution) = ["1080p", "4K"].get(index) else {
            return;
        };
        (*resolution).clone_into(&mut self.app.slideshow_resolution);
        self.app.sync_preferences();
    }

    pub fn set_slideshow_collage(&mut self, enabled: bool) {
        self.app.slideshow_collage = enabled;
        self.app.sync_preferences();
    }

    pub fn add_slideshow_audio(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            if !self.app.slideshow_audio_paths.contains(&path) {
                self.app.slideshow_audio_paths.push(path);
            }
        }
        self.app.slideshow_audio_selected = self.app.slideshow_audio_paths.len().checked_sub(1);
        self.app.sync_preferences();
    }

    pub fn select_slideshow_audio(&mut self, index: usize) {
        if index < self.app.slideshow_audio_paths.len() {
            self.app.slideshow_audio_selected = Some(index);
        }
    }

    pub fn move_slideshow_audio(&mut self, delta: isize) {
        let Some(index) = self.app.slideshow_audio_selected else {
            return;
        };
        let Some(next) = index.checked_add_signed(delta) else {
            return;
        };
        if next >= self.app.slideshow_audio_paths.len() {
            return;
        }
        self.app.slideshow_audio_paths.swap(index, next);
        self.app.slideshow_audio_selected = Some(next);
        self.app.sync_preferences();
    }

    pub fn remove_slideshow_audio(&mut self) {
        let Some(index) = self.app.slideshow_audio_selected else {
            return;
        };
        self.app.slideshow_audio_paths.remove(index);
        self.app.slideshow_audio_selected = (!self.app.slideshow_audio_paths.is_empty())
            .then(|| index.min(self.app.slideshow_audio_paths.len() - 1));
        self.app.sync_preferences();
    }

    pub fn clear_slideshow_audio(&mut self) {
        self.app.slideshow_audio_paths.clear();
        self.app.slideshow_audio_selected = None;
        self.app.sync_preferences();
    }

    pub fn set_prevent_sleep(&mut self, enabled: bool) {
        self.app.sleep.enabled = enabled;
        self.app.update_sleep_inhibitor();
        self.app.sync_preferences();
    }

    pub fn set_speed(&mut self, index: usize) {
        let options = speed_options(self.app.encoder);
        let Some(speed) = options.get(index) else {
            return;
        };
        self.app.speed_preset.clone_from(speed);
        self.app.sync_preferences();
    }

    pub fn shutdown(&mut self) {
        if let Some(worker) = &self.app.worker {
            worker.control.stop_all();
        }
        self.app.sleep.inhibitor = None;
        self.app.platform_indicator.set_converting(false);
        self.app.sync_preferences();
        let _ = self.app.state_store.save_queue(
            &self.app.queue,
            self.app.worker.is_some() || self.app.queue_run_state != QueueRunState::Idle,
            &self.app.task_run_failures,
        );
    }

    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::too_many_lines)]
    pub fn snapshot(&self) -> SlintAppSnapshot {
        let paused_between_files = matches!(
            self.app.queue_run_state,
            QueueRunState::PausedBetweenFiles | QueueRunState::PausedBetweenFilesSelected
        );
        let active_id = self
            .app
            .worker
            .as_ref()
            .map(|worker| worker.task_id.as_str());
        let active_mode = active_id
            .and_then(|id| self.app.queue.task(id))
            .map(|task| task.settings.mode);
        let active_progress = self
            .app
            .progress
            .as_ref()
            .and_then(|progress| progress_fraction(progress, active_mode))
            .unwrap_or(0.0);
        let tasks = self
            .app
            .queue
            .tasks()
            .iter()
            .map(|task| {
                let counts = ConverterApp::task_display_counts(
                    task,
                    &self.app.folder_summaries,
                    self.app.task_run_failures.get(&task.id),
                );
                let status = friendly_status(task.status, task.error.as_deref());
                let count_text = if counts.files > 0 {
                    format!(
                        "{} video{}",
                        counts.files,
                        if counts.files == 1 { "" } else { "s" }
                    )
                } else if counts.targets > 0 {
                    format!(
                        "{} item{}",
                        counts.targets,
                        if counts.targets == 1 { "" } else { "s" }
                    )
                } else {
                    "Waiting for media".to_owned()
                };
                let task_progress = task_progress_segments(
                    &counts,
                    self.app.task_run_start_converted.get(&task.id).copied(),
                    (active_id == Some(task.id.as_str())).then_some(active_progress),
                );
                let completed_before_count = self
                    .app
                    .task_run_start_converted
                    .get(&task.id)
                    .copied()
                    .unwrap_or(counts.converted)
                    .min(counts.files);
                let completed_this_run_count = counts
                    .converted
                    .saturating_sub(completed_before_count)
                    .min(counts.files.saturating_sub(completed_before_count));
                let unfinished_count = counts.remaining;
                let completed_count = counts.converted.min(counts.files);
                let progress_percent = ((task_progress.completed_before
                    + task_progress.completed_this_run)
                    * 100.0)
                    .round();
                let can_retry = matches!(task.status, QueueStatus::Failed | QueueStatus::Cancelled)
                    || (task.status == QueueStatus::Completed && task_has_remaining_work(task));
                let failure_suffix = if counts.failed == 0 {
                    String::new()
                } else {
                    format!(", {} failed", counts.failed)
                };
                let progress_summary = format!(
                    "{completed_before_count} completed previously, {completed_this_run_count} completed this run, {unfinished_count} unfinished{failure_suffix}"
                );
                SlintTaskSnapshot {
                    title: task.name.clone(),
                    subtitle: format!(
                        "{}  ·  {}  ·  {count_text}",
                        task.settings.mode.label(),
                        task.settings.encoder.user_name()
                    ),
                    status,
                    completed_before: task_progress.completed_before,
                    completed_this_run: task_progress.completed_this_run,
                    current_item_progress: task_progress.current_item,
                    has_progress: counts.files > 0,
                    progress_label: if counts.failed == 0 {
                        format!(
                            "{completed_count} / {} completed  ·  {progress_percent:.0}% overall",
                            counts.files
                        )
                    } else {
                        format!(
                            "{completed_count} / {} completed  ·  {} failed  ·  {progress_percent:.0}% overall",
                            counts.files, counts.failed
                        )
                    },
                    progress_summary,
                    completed_before_label: format!("Previous {completed_before_count}"),
                    completed_this_run_label: format!("This run {completed_this_run_count}"),
                    remaining_label: if counts.failed == 0 {
                        format!("Remaining {unfinished_count}")
                    } else {
                        format!("Remaining {unfinished_count} · Failed {}", counts.failed)
                    },
                    selected: self.app.selected_id.as_deref() == Some(task.id.as_str()),
                    active: active_id == Some(task.id.as_str()),
                    can_run: task.status == QueueStatus::Pending || can_retry,
                    can_retry,
                    can_reorder: task.status == QueueStatus::Pending,
                    error_detail: task.error.clone().unwrap_or_default(),
                }
            })
            .collect::<Vec<_>>();
        let attention_count = tasks
            .iter()
            .filter(|task| !task.error_detail.is_empty())
            .count();
        let history_filter = self.history_filter.trim().to_lowercase();
        let history = self
            .app
            .completed_history
            .iter()
            .filter(|row| {
                history_filter.is_empty()
                    || row
                        .columns()
                        .iter()
                        .any(|value| value.to_lowercase().contains(&history_filter))
            })
            .map(|row| {
                let columns = row.columns();
                let path = columns[0].clone();
                let title = std::path::Path::new(&path).file_name().map_or_else(
                    || path.clone(),
                    |name| name.to_string_lossy().into_owned(),
                );
                SlintHistorySnapshot {
                    title,
                    path,
                    subtitle: format!("{}  ·  {}", columns[1], columns[8]),
                    detail: format!("{} min  ·  {} → {}", columns[5], columns[6], columns[7]),
                    configuration: format!(
                        "Original FPS {}  ·  Target FPS {}  ·  Encoder {}  ·  Quality CRF {}  ·  Preset {}",
                        columns[9], columns[10], columns[11], columns[12], columns[13]
                    ),
                }
            })
            .collect::<Vec<_>>();
        let selected_index = self
            .app
            .selected_id
            .as_ref()
            .and_then(|id| {
                self.app
                    .queue
                    .tasks()
                    .iter()
                    .position(|task| &task.id == id)
            })
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1);
        let selected_can_move = self.app.selected_id.as_ref().is_some_and(|id| {
            self.app
                .queue
                .task(id)
                .is_some_and(|task| task.status == QueueStatus::Pending)
        });
        let (selected_task_title, selected_task_files) = self
            .app
            .selected_id
            .as_deref()
            .and_then(|id| self.app.queue.task(id))
            .map_or_else(
                || (String::new(), Vec::new()),
                |task| {
                    (
                        task.name.clone(),
                        selected_task_files(
                            task,
                            &self.app.completed_history,
                            self.app.task_run_failures.get(&task.id),
                        ),
                    )
                },
            );
        let selected_task_error_detail = selected_task_files
            .iter()
            .filter(|file| !file.error_detail.is_empty())
            .map(|file| format!("{}\n{}", file.path, file.error_detail))
            .collect::<Vec<_>>()
            .join("\n\n");
        let active_title = if paused_between_files {
            "Queue paused".to_owned()
        } else {
            self.app
                .worker
                .as_ref()
                .and_then(|worker| self.app.queue.task(&worker.task_id))
                .map_or_else(|| "Ready when you are".to_owned(), |task| task.name.clone())
        };
        let active_detail = if paused_between_files {
            "Current file completed. Resume to start the next queued file.".to_owned()
        } else {
            self.app.worker.as_ref().map_or_else(
                || self.app.activity.clone(),
                |worker| worker.target.display().to_string(),
            )
        };
        let live_status = slint_live_status(&self.app);
        let task_settings =
            displayed_task_settings(&self.app, paused_between_files, self.task_draft.is_some());
        let settings = slint_settings_snapshot(&self.app, task_settings);
        let (task_draft_targets, task_draft_summary, task_draft_output_summary) =
            self.task_draft.as_ref().map_or_else(
                || (Vec::new(), String::new(), String::new()),
                |draft| {
                    (
                        draft
                            .targets
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect(),
                        format!(
                            "{}  ·  {}",
                            draft.settings.mode.label(),
                            draft.settings.encoder.user_name()
                        ),
                        task_draft_output_summary(draft),
                    )
                },
            );
        SlintAppSnapshot {
            pending_count: self
                .app
                .queue
                .tasks()
                .iter()
                .filter(|task| task.status == QueueStatus::Pending)
                .count(),
            completed_count: self
                .app
                .queue
                .tasks()
                .iter()
                .filter(|task| task.status == QueueStatus::Completed)
                .count(),
            attention_count,
            history_total_count: self.app.completed_history.len(),
            pause_after_current: matches!(
                self.app.queue_run_state,
                QueueRunState::PauseAfterCurrent | QueueRunState::PauseAfterCurrentSelected
            ),
            completion_notice: self.app.completion_notice.clone(),
            tasks,
            selected_task_title,
            selected_task_files,
            selected_task_error_detail,
            history,
            settings,
            activity: self.app.activity.clone(),
            engine_status: self.app.engine_status.clone(),
            active_title,
            active_detail,
            live_status,
            cpu_limit_index: self.app.cpu_limit.index(),
            cpu_limit_summary: self.app.cpu_limit_summary(),
            cpu_usage: self
                .app
                .process_cpu_usage_percent
                .map_or_else(|| "-".to_owned(), |value| format!("{value:.0}%")),
            progress: active_progress,
            is_running: self.app.worker.is_some() || paused_between_files,
            is_paused: paused_between_files
                || self.app.worker.as_ref().is_some_and(|worker| worker.paused),
            paused_between_files,
            selected_index,
            selected_can_move,
            preview_enabled: self.app.frame_preview_enabled && !paused_between_files,
            live_preview: self.app.live_preview.clone(),
            task_draft_targets,
            task_draft_summary,
            task_draft_output_summary,
        }
    }

    fn fps_modes(&self) -> Vec<FpsUiMode> {
        fps_modes_for(self.app.mode, self.app.encoder)
    }
}

fn fps_modes_for(mode: ContentMode, encoder: Encoder) -> Vec<FpsUiMode> {
    if descriptor(mode, encoder).share_lowest_fps {
        vec![
            FpsUiMode::SharedLowest,
            FpsUiMode::Source,
            FpsUiMode::Explicit,
        ]
    } else {
        vec![FpsUiMode::Source, FpsUiMode::Explicit]
    }
}

fn slint_live_status(app: &ConverterApp) -> SlintLiveStatusSnapshot {
    let Some(worker) = app.worker.as_ref() else {
        return SlintLiveStatusSnapshot::default();
    };
    let Some(task) = app.queue.task(&worker.task_id) else {
        return SlintLiveStatusSnapshot::default();
    };
    let progress = app.progress.as_ref();
    let spent = worker.active_elapsed();
    let fraction =
        progress.and_then(|progress| progress_fraction(progress, Some(task.settings.mode)));
    let remaining = estimated_remaining(spent, fraction);
    let estimated_total = remaining.map(|remaining| spent.saturating_add(remaining));
    let original_fps = app.active_source_info.as_ref().and_then(|info| info.fps);
    let (encoder, quality, preset) = if task.settings.mode == ContentMode::Trim {
        ("Stream copy".to_owned(), "-".to_owned(), "-".to_owned())
    } else {
        (
            task.settings.encoder.user_name().to_owned(),
            task.settings
                .quality
                .map(f64::from)
                .map_or_else(|| "-".to_owned(), format_python_general),
            task.settings
                .speed_preset
                .clone()
                .unwrap_or_else(|| "-".to_owned()),
        )
    };

    SlintLiveStatusSnapshot {
        position: progress.map_or_else(|| "-".to_owned(), |value| format_clock(value.completed)),
        duration: progress
            .and_then(|value| value.total)
            .map_or_else(|| "-".to_owned(), format_clock),
        frames: progress.map_or_else(
            || "-".to_owned(),
            |value| format_frame_progress(value, Some(&task.settings), original_fps),
        ),
        conversion_fps: progress
            .and_then(|value| value.frames_per_second)
            .map_or_else(|| "-".to_owned(), |value| format!("{value:.1}")),
        conversion_speed: progress
            .and_then(|value| value.speed)
            .map_or_else(|| "-".to_owned(), |value| format!("{value:.2}x")),
        original_fps: format_fps(original_fps),
        target_fps: target_fps_status_with_source(progress, task.settings.fps, original_fps),
        encoder,
        quality,
        preset,
        carried_audio: format_carried_audio(app.active_stream_info.as_ref()),
        carried_subtitles: format_carried_subtitles(app.active_stream_info.as_ref()),
        spent: format_clock(spent),
        estimated_total: estimated_total.map_or_else(|| "-".to_owned(), format_clock),
        remaining: remaining.map_or_else(|| "-".to_owned(), format_clock),
        app_cpu: app
            .process_cpu_usage_percent
            .map_or_else(|| "-".to_owned(), |value| format!("{value:.0}%")),
    }
}

fn friendly_status(status: QueueStatus, error: Option<&str>) -> String {
    match status {
        QueueStatus::Pending => "Ready".to_owned(),
        QueueStatus::Running => "Converting".to_owned(),
        QueueStatus::Paused => "Paused".to_owned(),
        QueueStatus::Completed if error.is_some() => "Finished with issues".to_owned(),
        QueueStatus::Completed => "Finished".to_owned(),
        QueueStatus::Failed => "Needs attention".to_owned(),
        QueueStatus::Cancelled => "Stopped".to_owned(),
    }
}

fn active_item_position(
    task: &QueueTask,
    counts: &TaskDisplayCounts,
    progress: Option<&ConversionProgress>,
) -> Option<(usize, usize)> {
    if task.settings.mode == ContentMode::PhotoSlideshow {
        let total = task.settings.slideshow_image_paths.len();
        if total == 0 {
            return None;
        }
        let index = progress
            .and_then(|progress| {
                let duration = progress.total.filter(|duration| !duration.is_zero())?;
                let numerator = progress
                    .completed
                    .min(duration)
                    .as_nanos()
                    .saturating_mul(u128::try_from(total).unwrap_or(u128::MAX));
                let offset = numerator / duration.as_nanos();
                Some(
                    usize::try_from(offset)
                        .unwrap_or(total)
                        .saturating_add(1)
                        .min(total),
                )
            })
            .unwrap_or(1);
        return Some((index, total));
    }
    (counts.files > 0).then(|| {
        let index = counts
            .converted
            .saturating_add(counts.failed)
            .saturating_add(1)
            .min(counts.files);
        (index, counts.files)
    })
}

fn review_preview_panel(
    ui: &mut egui::Ui,
    texture: Option<&egui::TextureHandle>,
    error: Option<&str>,
    alt_text: &str,
) {
    ui.group(|ui| {
        ui.set_min_height(210.0);
        ui.set_width(ui.available_width());
        if let Some(texture) = texture {
            let original = texture.size_vec2();
            let scale = (ui.available_width() / original.x)
                .min(190.0 / original.y)
                .min(1.0);
            ui.vertical_centered(|ui| {
                ui.add(
                    egui::Image::new(texture)
                        .fit_to_exact_size(original * scale)
                        .alt_text(alt_text),
                );
            });
        } else if let Some(error) = error {
            ui.centered_and_justified(|ui| {
                ui.weak(format!("Preview unavailable: {error}"));
            });
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
            });
        }
    });
}

fn photo_texture(
    context: &egui::Context,
    name: String,
    preview: &videoferry_ffmpeg::PhotoThumbnail,
) -> egui::TextureHandle {
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [preview.width as usize, preview.height as usize],
        &preview.rgba,
    );
    context.load_texture(name, image, egui::TextureOptions::LINEAR)
}

fn format_trim_time(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = total_seconds % 3600 / 60;
    let seconds = total_seconds % 60;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

fn parse_trim_time(value: &str) -> Result<Duration, &'static str> {
    let value = value.trim();
    let parts = value.split(':').collect::<Vec<_>>();
    if !matches!(parts.len(), 2 | 3)
        || parts
            .iter()
            .any(|part| part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err("use MM:SS or HH:MM:SS");
    }
    let values = parts
        .iter()
        .map(|part| part.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "use MM:SS or HH:MM:SS")?;
    let (hours, minutes, seconds) = match values.as_slice() {
        [minutes, seconds] => (0, *minutes, *seconds),
        [hours, minutes, seconds] => (*hours, *minutes, *seconds),
        _ => unreachable!("part count was validated"),
    };
    if minutes >= 60 || seconds >= 60 {
        return Err("minutes and seconds must be between 00 and 59");
    }
    let total_seconds = hours
        .checked_mul(3600)
        .and_then(|value| {
            minutes
                .checked_mul(60)
                .and_then(|minutes| value.checked_add(minutes))
        })
        .and_then(|value| value.checked_add(seconds))
        .ok_or("time is too large")?;
    Ok(Duration::from_secs(total_seconds))
}

const fn fps_ui_mode(policy: FpsPolicy) -> FpsUiMode {
    match policy {
        FpsPolicy::SharedLowest => FpsUiMode::SharedLowest,
        FpsPolicy::Source => FpsUiMode::Source,
        FpsPolicy::Exact(_) => FpsUiMode::Explicit,
    }
}

fn explicit_fps(policy: FpsPolicy) -> f64 {
    match policy {
        FpsPolicy::Exact(value) if value.is_finite() && value > 0.0 => value,
        _ => 30.0,
    }
}

const fn workflow_shows_encoder(mode: ContentMode) -> bool {
    !matches!(mode, ContentMode::Trim)
}

const fn workflow_shows_fps(mode: ContentMode) -> bool {
    !matches!(
        mode,
        ContentMode::Trim | ContentMode::Stabilize | ContentMode::PhotoSlideshow
    )
}

fn photo_zoom_after_wheel(current: f32, wheel_delta: f32) -> f32 {
    let multiplier = if wheel_delta > 0.0 { 1.25 } else { 0.8 };
    (current * multiplier).clamp(0.05, 8.0)
}

fn worker_error_is_queue_fatal(mode: ContentMode, error: &EngineError) -> bool {
    mode == ContentMode::PhotoSlideshow || matches!(error, EngineError::Unavailable(_))
}

fn worker_error_should_retry(mode: ContentMode, error: &EngineError) -> bool {
    mode != ContentMode::PhotoSlideshow
        && matches!(
            error,
            EngineError::Unsupported(_) | EngineError::InvalidMedia(_) | EngineError::Failed(_)
        )
}

fn worker_error_is_locked_input(error: &EngineError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("winerror 32")
        || message.contains("being used by another process")
        || message.contains("sharing violation")
}

fn task_run_failure_summary(failures: Option<&HashMap<PathBuf, String>>) -> Option<String> {
    let failed_count = failures.map_or(0, HashMap::len);
    (failed_count > 0).then(|| {
        format!(
            "{failed_count} file{} failed in this run; conversion is continuing",
            if failed_count == 1 { "" } else { "s" }
        )
    })
}

fn spawn_worker(
    task_id: String,
    request: ConversionRequest,
    context: egui::Context,
    preview_enabled: bool,
    cpu_thread_limit: usize,
) -> WorkerState {
    let (sender, receiver) = mpsc::channel();
    let control = Arc::new(ConversionControl::new());
    control.set_preview_enabled(preview_enabled);
    control.set_cpu_thread_limit(cpu_thread_limit);
    let worker_control = Arc::clone(&control);
    let worker_task_id = task_id.clone();
    let target = request.input.clone();
    let started_local = formatted_local_time();
    std::thread::spawn(move || {
        let event_sender = sender.clone();
        let event_context = context.clone();
        let mut emit = |event| {
            let _ = event_sender.send(WorkerUpdate::Event(event));
            event_context.request_repaint();
        };
        let result = (|| {
            wait_until_file_stable(&request.input, &worker_control, &mut emit)?;
            let _ = sender.send(WorkerUpdate::CameraInfo(camera_run_info(&request)));
            let started = Instant::now();
            let start_time = formatted_local_time();
            let (original, streams) = active_source_media_info(&request.input);
            let _ = sender.send(WorkerUpdate::SourceInfo(original.clone(), streams));
            context.request_repaint();
            let outcome =
                execute_job_with_retry(&worker_task_id, &request, &worker_control, &mut emit)?;
            Ok(match outcome {
                JobExecution::Converted { output, lut_name } => {
                    WorkerJobOutcome::Converted(Box::new(CompletedJob {
                        input: request.input.clone(),
                        converted: history_media_info(&output),
                        output,
                        lut_name,
                        start_time,
                        end_time: formatted_local_time(),
                        process_minutes: format!("{:.2}", started.elapsed().as_secs_f64() / 60.0),
                        original,
                    }))
                }
                JobExecution::Skipped { reason } => WorkerJobOutcome::Skipped { reason },
            })
        })();
        let _ = sender.send(WorkerUpdate::Finished(result));
        context.request_repaint();
    });
    WorkerState {
        task_id,
        target,
        started_local,
        receiver,
        control,
        paused: false,
        started_at: Instant::now(),
        paused_at: None,
        paused_duration: Duration::ZERO,
    }
}

fn spawn_review_worker(context: egui::Context) -> ReviewWorkerState {
    let (request_sender, request_receiver) = mpsc::channel();
    let (result_sender, result_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut pending = VecDeque::new();
        loop {
            if pending.is_empty() {
                let Ok(request) = request_receiver.recv() else {
                    break;
                };
                pending.push_back(request);
            }
            pending.extend(request_receiver.try_iter());
            let next_index = pending
                .iter()
                .position(review_request_is_priority)
                .unwrap_or_default();
            let Some(request) = pending.remove(next_index) else {
                continue;
            };
            let result = execute_review_request(request);
            if result_sender.send(result).is_err() {
                break;
            }
            context.request_repaint();
        }
    });
    ReviewWorkerState {
        sender: request_sender,
        receiver: result_receiver,
    }
}

const fn review_request_is_priority(request: &ReviewWorkerRequest) -> bool {
    !matches!(
        request,
        ReviewWorkerRequest::Photo {
            target: ReviewPhotoTarget::Thumbnail,
            ..
        } | ReviewWorkerRequest::SlidePreview {
            target: ReviewSlideTarget::Thumbnail,
            ..
        }
    )
}

#[cfg(feature = "native-ffmpeg")]
fn execute_review_request(request: ReviewWorkerRequest) -> ReviewWorkerResult {
    match request {
        ReviewWorkerRequest::Photo {
            request_id,
            target,
            path,
            maximum_width,
            maximum_height,
        } => ReviewWorkerResult::Photo {
            request_id,
            target,
            result: videoferry_ffmpeg::NativeEngine::new()
                .and_then(|engine| engine.photo_thumbnail(&path, maximum_width, maximum_height)),
            path,
        },
        ReviewWorkerRequest::SlideGroups {
            request_id,
            paths,
            collage,
        } => ReviewWorkerResult::SlideGroups {
            request_id,
            result: videoferry_ffmpeg::NativeEngine::new()
                .and_then(|engine| engine.slideshow_review_groups(&paths, collage)),
        },
        ReviewWorkerRequest::SlidePreview {
            request_id,
            target,
            paths,
            collage,
            width,
            height,
        } => ReviewWorkerResult::SlidePreview {
            request_id,
            target,
            result: videoferry_ffmpeg::NativeEngine::new().and_then(|engine| {
                engine.slideshow_review_thumbnail(&paths, collage, width, height)
            }),
            paths,
        },
    }
}

#[cfg(not(feature = "native-ffmpeg"))]
fn execute_review_request(request: ReviewWorkerRequest) -> ReviewWorkerResult {
    let unavailable =
        || EngineError::Unavailable("direct FFmpeg previews are not compiled in".to_owned());
    match request {
        ReviewWorkerRequest::Photo {
            request_id,
            target,
            path,
            maximum_width,
            maximum_height,
        } => {
            let _ = (maximum_width, maximum_height);
            ReviewWorkerResult::Photo {
                request_id,
                target,
                path,
                result: Err(unavailable()),
            }
        }
        ReviewWorkerRequest::SlideGroups {
            request_id,
            paths,
            collage,
        } => ReviewWorkerResult::SlideGroups {
            request_id,
            result: if collage {
                Err(unavailable())
            } else {
                Ok(paths.into_iter().map(|path| vec![path]).collect())
            },
        },
        ReviewWorkerRequest::SlidePreview {
            request_id,
            target,
            paths,
            collage,
            width,
            height,
        } => {
            let _ = (collage, width, height);
            ReviewWorkerResult::SlidePreview {
                request_id,
                target,
                paths,
                result: Err(unavailable()),
            }
        }
    }
}

fn metric(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.strong(format!("{label}:"));
    ui.label(value);
    ui.separator();
}

fn format_clock(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = total % 3600 / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn format_ffmpeg_progress_clock(duration: Duration) -> String {
    let total_centiseconds = duration.as_millis() / 10;
    let centiseconds = total_centiseconds % 100;
    let total_seconds = total_centiseconds / 100;
    let hours = total_seconds / 3_600;
    let minutes = total_seconds % 3_600 / 60;
    let seconds = total_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{centiseconds:02}")
}

fn format_progress_time(progress: &ConversionProgress, mode: Option<ContentMode>) -> String {
    let completed = if mode == Some(ContentMode::PhotoSlideshow) {
        format_clock(progress.completed)
    } else {
        format_ffmpeg_progress_clock(progress.completed)
    };
    format!(
        "{}/{}",
        completed,
        progress.total.map_or_else(|| "-".to_owned(), format_clock)
    )
}

fn format_frame_progress(
    progress: &ConversionProgress,
    settings: Option<&QueueSettings>,
    source_fps: Option<f64>,
) -> String {
    let Some(current) = progress.frames else {
        return "-".to_owned();
    };
    if let Some(total) = progress.total_frames.filter(|total| *total > 0) {
        return format!("{current}/{total}");
    }
    let effective_fps = settings.and_then(|settings| {
        if settings.mode == ContentMode::PhotoSlideshow {
            return Some(f64::from(settings.slideshow_fps));
        }
        match settings.fps {
            FpsPolicy::Source => source_fps,
            FpsPolicy::Exact(value) => Some(value),
            FpsPolicy::SharedLowest => None,
        }
    });
    let total_frames = progress
        .total
        .zip(effective_fps)
        .and_then(|(duration, fps)| {
            let value = duration.as_secs_f64() * fps;
            if !value.is_finite() || value <= 0.0 {
                return None;
            }
            format!("{value:.0}").parse::<u64>().ok()
        })
        .filter(|total| *total > 0);
    total_frames.map_or_else(
        || format!("{current}/?"),
        |total| format!("{current}/{total}"),
    )
}

fn format_carried_audio(info: Option<&StreamCarryInfo>) -> String {
    let Some(info) = info else {
        return "-".to_owned();
    };
    let tracks = format_track_count(info.audio_tracks);
    if info.audio_tracks == 0 {
        return tracks;
    }
    info.audio_channels.map_or_else(
        || format!("{tracks}, ? ch"),
        |channels| format!("{tracks}, {channels} ch"),
    )
}

fn format_carried_subtitles(info: Option<&StreamCarryInfo>) -> String {
    info.map_or_else(
        || "-".to_owned(),
        |info| format_track_count(info.subtitle_tracks),
    )
}

fn format_track_count(count: usize) -> String {
    format!("{count} track{}", if count == 1 { "" } else { "s" })
}

fn progress_fraction(progress: &ConversionProgress, mode: Option<ContentMode>) -> Option<f32> {
    if let Some(overall) = progress.overall {
        return ratio_fraction(overall.completed, overall.total);
    }
    if mode != Some(ContentMode::Stabilize)
        && let Some((frames, total_frames)) = progress.frames.zip(progress.total_frames)
        && total_frames > 0
    {
        return ratio_fraction(u128::from(frames), u128::from(total_frames));
    }
    ratio_fraction(progress.completed.as_nanos(), progress.total?.as_nanos())
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct TaskProgressSegments {
    completed_before: f32,
    completed_this_run: f32,
    current_item: f32,
}

#[allow(
    clippy::cast_precision_loss,
    reason = "GUI progress ratios intentionally reduce bounded file counts to f32"
)]
fn task_progress_segments(
    counts: &TaskDisplayCounts,
    converted_before_run: Option<usize>,
    current_item_progress: Option<f32>,
) -> TaskProgressSegments {
    if counts.files == 0 {
        return TaskProgressSegments::default();
    }
    let completed_before_count = converted_before_run
        .unwrap_or(counts.converted)
        .min(counts.files);
    let completed_this_run_count = counts
        .converted
        .saturating_sub(completed_before_count)
        .min(counts.files.saturating_sub(completed_before_count));
    let total = counts.files as f32;
    let current_item = current_item_progress
        .filter(|_| counts.converted < counts.files)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0)
        / total;
    TaskProgressSegments {
        completed_before: completed_before_count as f32 / total,
        completed_this_run: completed_this_run_count as f32 / total,
        current_item,
    }
}

fn ratio_fraction(numerator: u128, denominator: u128) -> Option<f32> {
    if denominator == 0 {
        return None;
    }
    let basis_points = numerator.min(denominator).saturating_mul(10_000) / denominator;
    let basis_points = u16::try_from(basis_points).ok()?;
    Some(f32::from(basis_points) / 10_000.0)
}

fn estimated_remaining(spent: Duration, fraction: Option<f32>) -> Option<Duration> {
    let fraction = fraction.filter(|fraction| fraction.is_finite() && *fraction > 0.0)?;
    let fraction = f64::from(fraction);
    let seconds = spent.as_secs_f64() * (1.0 - fraction).max(0.0) / fraction;
    seconds
        .is_finite()
        .then(|| Duration::from_secs_f64(seconds))
}

fn formatted_local_time() -> String {
    jiff::Zoned::now().strftime("%Y-%m-%d %H:%M:%S").to_string()
}

const fn should_inhibit_sleep(enabled: bool, worker_active: bool, paused: bool) -> bool {
    enabled && worker_active && !paused
}

fn queued_target_set(queue: &Queue) -> HashSet<PathBuf> {
    queue
        .tasks()
        .iter()
        .flat_map(|task| task.targets.iter().cloned())
        .collect()
}

fn update_extended_selection(
    task_ids: &[String],
    id: String,
    modifiers: egui::Modifiers,
    primary: &mut Option<String>,
    selected: &mut HashSet<String>,
    anchor: &mut Option<String>,
) {
    if modifiers.shift
        && let Some(anchor_id) = anchor.as_ref()
        && let (Some(anchor_index), Some(clicked_index)) = (
            task_ids.iter().position(|candidate| candidate == anchor_id),
            task_ids.iter().position(|candidate| candidate == &id),
        )
    {
        if !modifiers.command {
            selected.clear();
        }
        let (start, end) = if anchor_index <= clicked_index {
            (anchor_index, clicked_index)
        } else {
            (clicked_index, anchor_index)
        };
        selected.extend(task_ids[start..=end].iter().cloned());
        *primary = Some(id);
        return;
    }
    if modifiers.command {
        if !selected.remove(&id) {
            selected.insert(id.clone());
            *primary = Some(id.clone());
        } else if primary.as_deref() == Some(&id) {
            *primary = task_ids
                .iter()
                .find(|candidate| selected.contains(*candidate))
                .cloned();
        }
        *anchor = Some(id);
        return;
    }
    selected.clear();
    selected.insert(id.clone());
    *anchor = Some(id.clone());
    *primary = Some(id);
}

fn move_item_to_insertion<T>(items: &mut Vec<T>, source: usize, insertion: usize) -> Option<usize> {
    if source >= items.len() || insertion > items.len() {
        return None;
    }
    let item = items.remove(source);
    let destination = if source < insertion {
        insertion.saturating_sub(1)
    } else {
        insertion
    }
    .min(items.len());
    items.insert(destination, item);
    Some(destination)
}

const fn slideshow_review_is_editable(task: &QueueTask, converted_count: usize) -> bool {
    !matches!(
        task.status,
        QueueStatus::Completed | QueueStatus::Running | QueueStatus::Paused
    ) && converted_count == 0
}

fn select_trim_source(paths: &[PathBuf]) -> (Option<PathBuf>, usize) {
    let mut videos = paths.iter().filter(|path| is_video_path(path));
    let selected = videos.next().cloned();
    (selected, videos.count())
}

fn collect_new_video_task_targets(
    paths: Vec<PathBuf>,
    settings: &QueueSettings,
    occupied: &HashSet<PathBuf>,
) -> (Vec<PathBuf>, usize) {
    let mut targets = Vec::new();
    let mut seen = HashSet::new();
    let mut skipped = 0_usize;
    for path in paths {
        let supported = path.is_dir() || is_video_path(&path);
        let already_complete = !path.is_dir() && should_skip_queue_source(&path, settings);
        if !supported || already_complete || occupied.contains(&path) || !seen.insert(path.clone())
        {
            skipped += 1;
            continue;
        }
        targets.push(path);
    }
    (targets, skipped)
}

fn queue_admission_occupied_paths(
    queue: &Queue,
    watched_folders: &[WatchedFolder],
) -> HashSet<PathBuf> {
    queued_target_set(queue)
        .into_iter()
        .chain(watched_folders.iter().map(|watch| watch.root.clone()))
        .collect()
}

fn default_task_name(settings: &QueueSettings, target: &std::path::Path) -> String {
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("Untitled");
    format!("{} - {name}", settings.mode.label())
}

fn validate_task_targets(settings: &QueueSettings, targets: &[PathBuf]) -> Result<(), String> {
    if targets.is_empty() {
        return Err("Add at least one target path.".to_owned());
    }
    for target in targets {
        if !target.exists() {
            return Err(format!("Path does not exist: {}", target.display()));
        }
        match settings.mode {
            ContentMode::Trim if target.is_dir() => {
                return Err(format!(
                    "Trim mode does not support folders: {}",
                    target.display()
                ));
            }
            ContentMode::Trim if !is_video_path(target) => {
                return Err(format!(
                    "Trim mode requires a video file: {}",
                    target.display()
                ));
            }
            ContentMode::PhotoSlideshow if target.is_file() && !is_photo_path(target) => {
                return Err(format!(
                    "Photo slideshow requires image files: {}",
                    target.display()
                ));
            }
            ContentMode::PhotoSlideshow
                if target.is_dir() && collect_photo_files(target).is_empty() =>
            {
                return Err(format!(
                    "Folder contains no supported images: {}",
                    target.display()
                ));
            }
            ContentMode::Tv
            | ContentMode::Animation
            | ContentMode::CameraVideos
            | ContentMode::Stabilize
                if target.is_file() && !is_video_path(target) =>
            {
                return Err(format!(
                    "Video task requires media files: {}",
                    target.display()
                ));
            }
            _ => {}
        }
    }
    if settings.mode == ContentMode::Trim && targets.len() != 1 {
        return Err("Trim mode requires exactly one video file.".to_owned());
    }
    if settings.mode == ContentMode::PhotoSlideshow {
        let image_count = if settings.slideshow_image_paths.is_empty() {
            targets
                .iter()
                .flat_map(|target| collect_photo_files(target))
                .collect::<HashSet<_>>()
                .len()
        } else {
            settings
                .slideshow_image_paths
                .iter()
                .filter(|path| is_photo_path(path))
                .collect::<HashSet<_>>()
                .len()
        };
        if image_count < 2 {
            return Err("Photo slideshow requires at least two images.".to_owned());
        }
    }
    Ok(())
}

fn history_media_info(path: &std::path::Path) -> HistoryMediaInfo {
    #[cfg(feature = "native-ffmpeg")]
    if path.is_file()
        && let Ok(engine) = videoferry_ffmpeg::NativeEngine::new()
        && let Ok(info) = engine.probe(path)
    {
        return HistoryMediaInfo::from(&info);
    }
    HistoryMediaInfo {
        size: std::fs::metadata(path).ok().map(|metadata| metadata.len()),
        ..HistoryMediaInfo::default()
    }
}

impl From<&MediaInfo> for HistoryMediaInfo {
    fn from(info: &MediaInfo) -> Self {
        Self {
            size: info.file_size,
            width: info.width,
            height: info.height,
            fps: info.frame_rate,
            codec: info
                .streams
                .iter()
                .find(|stream| stream.kind == StreamKind::Video && !stream.is_attached_picture)
                .and_then(|stream| stream.codec_name.clone()),
            duration: info.duration,
        }
    }
}

fn active_source_media_info(path: &std::path::Path) -> (HistoryMediaInfo, StreamCarryInfo) {
    #[cfg(feature = "native-ffmpeg")]
    if path.is_file()
        && let Ok(engine) = videoferry_ffmpeg::NativeEngine::new()
        && let Ok(info) = engine.probe(path)
    {
        return (HistoryMediaInfo::from(&info), StreamCarryInfo::from(&info));
    }
    (
        HistoryMediaInfo {
            size: std::fs::metadata(path).ok().map(|metadata| metadata.len()),
            ..HistoryMediaInfo::default()
        },
        StreamCarryInfo::default(),
    )
}

impl From<&MediaInfo> for StreamCarryInfo {
    fn from(info: &MediaInfo) -> Self {
        // Audio and subtitle inclusion is identical for the two release
        // containers; only the selected copy/transcode action differs.
        let plan = build_stream_plan(info, Container::Matroska);
        let carried_audio_channels = plan.audio.iter().try_fold(0_u32, |total, planned| {
            let channels = info
                .streams
                .iter()
                .find(|stream| stream.index == planned.input_index)
                .and_then(|stream| stream.channels)?;
            total.checked_add(channels)
        });
        Self {
            audio_tracks: plan.audio.len(),
            audio_channels: carried_audio_channels,
            subtitle_tracks: plan.subtitles.len(),
        }
    }
}

fn completed_history_row(task: &QueueTask, job: &CompletedJob) -> CompletedHistoryRow {
    let (encoder, quality, preset) = if task.settings.mode == ContentMode::Trim {
        ("Stream copy".to_owned(), "-".to_owned(), "-".to_owned())
    } else {
        (
            task.settings.encoder.user_name().to_owned(),
            task.settings
                .quality
                .map(f64::from)
                .map_or_else(|| "-".to_owned(), format_python_general),
            task.settings
                .speed_preset
                .clone()
                .unwrap_or_else(|| "-".to_owned()),
        )
    };
    CompletedHistoryRow::new([
        job.output.display().to_string(),
        task.settings.mode.label().to_owned(),
        job.lut_name.clone().unwrap_or_else(|| "-".to_owned()),
        job.start_time.clone(),
        job.end_time.clone(),
        job.process_minutes.clone(),
        format_size_mb(job.original.size),
        format_size_mb(job.converted.size),
        format_resolution(job.original.width, job.original.height),
        format_fps(job.original.fps),
        format_fps(job.converted.fps),
        encoder,
        quality,
        preset,
        task.id.clone(),
        job.input.display().to_string(),
        job.original.codec.clone().unwrap_or_else(|| "-".to_owned()),
        job.converted
            .codec
            .clone()
            .unwrap_or_else(|| "-".to_owned()),
        job.original
            .duration
            .or(job.converted.duration)
            .map_or_else(|| "-".to_owned(), format_clock),
    ])
}

fn format_size_mb(size: Option<u64>) -> String {
    size.map_or_else(
        || "-".to_owned(),
        |bytes| {
            let scaled = u128::from(bytes).saturating_mul(100);
            let divisor = 1_048_576_u128;
            let mut hundredths = scaled / divisor;
            let remainder = scaled % divisor;
            if remainder.saturating_mul(2) > divisor
                || (remainder.saturating_mul(2) == divisor && hundredths % 2 == 1)
            {
                hundredths = hundredths.saturating_add(1);
            }
            format!("{}.{:02} MB", hundredths / 100, hundredths % 100)
        },
    )
}

fn format_resolution(width: Option<u32>, height: Option<u32>) -> String {
    match (width, height) {
        (Some(width), Some(height)) if width > 0 && height > 0 => format!("{width}x{height}"),
        _ => "-".to_owned(),
    }
}

fn format_fps(fps: Option<f64>) -> String {
    fps.filter(|value| value.is_finite() && *value > 0.0)
        .map_or_else(|| "-".to_owned(), format_python_general)
}

fn format_python_general(value: f64) -> String {
    let mut normalized = value.abs();
    let mut exponent = 0_i32;
    if normalized >= 1.0 {
        while normalized >= 10.0 {
            normalized /= 10.0;
            exponent += 1;
        }
    } else {
        while normalized < 1.0 {
            normalized *= 10.0;
            exponent -= 1;
        }
    }
    if !(-4..6).contains(&exponent) {
        let scientific = format!("{value:.5e}");
        let (mantissa, exponent) = scientific
            .split_once('e')
            .unwrap_or((scientific.as_str(), "0"));
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let exponent = exponent.parse::<i32>().unwrap_or_default();
        return format!("{mantissa}e{exponent:+03}");
    }
    let decimals = usize::try_from((5 - exponent).max(0)).unwrap_or_default();
    let fixed = format!("{value:.decimals$}");
    if fixed.contains('.') {
        fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        fixed
    }
}

fn fps_policy_status(policy: FpsPolicy) -> String {
    match policy {
        FpsPolicy::SharedLowest => "Shared lowest in folder".to_owned(),
        FpsPolicy::Source => "Original".to_owned(),
        FpsPolicy::Exact(value) => value.to_string(),
    }
}

fn target_fps_status(progress: Option<&ConversionProgress>, policy: FpsPolicy) -> String {
    progress
        .and_then(|progress| progress.target_fps)
        .map_or_else(|| fps_policy_status(policy), |fps| format_fps(Some(fps)))
}

fn target_fps_status_with_source(
    progress: Option<&ConversionProgress>,
    policy: FpsPolicy,
    source_fps: Option<f64>,
) -> String {
    if let Some(target_fps) = progress.and_then(|progress| progress.target_fps) {
        return format_fps(Some(target_fps));
    }
    match policy {
        FpsPolicy::Source => {
            source_fps.map_or_else(|| "Original".to_owned(), |fps| format_fps(Some(fps)))
        }
        FpsPolicy::Exact(fps) => format_fps(Some(fps)),
        FpsPolicy::SharedLowest => "Shared lowest in folder".to_owned(),
    }
}

fn estimated_output_bytes(progress: &ConversionProgress) -> Option<u64> {
    let bytes = progress.output_bytes?;
    let completed = progress.completed.as_nanos();
    let total = progress.total?.as_nanos();
    if completed == 0 || total == 0 {
        return None;
    }
    let estimate = u128::from(bytes).saturating_mul(total) / completed;
    Some(u64::try_from(estimate).unwrap_or(u64::MAX))
}

fn output_bytes_per_minute(progress: &ConversionProgress) -> Option<u64> {
    let bytes = progress.output_bytes?;
    let completed = progress.completed.as_nanos();
    if completed == 0 {
        return None;
    }
    let one_minute = Duration::from_secs(60).as_nanos();
    let rate = u128::from(bytes).saturating_mul(one_minute) / completed;
    Some(u64::try_from(rate).unwrap_or(u64::MAX))
}

enum JobExecution {
    Converted {
        output: PathBuf,
        lut_name: Option<String>,
    },
    Skipped {
        reason: String,
    },
}

fn execute_job_with_retry(
    task_id: &str,
    request: &ConversionRequest,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<JobExecution, EngineError> {
    const RETRY_DELAY: Duration = Duration::from_secs(3);

    run_with_retry(request.settings.mode, control, RETRY_DELAY, emit, |emit| {
        if let Some(reason) = camera_source_skip_reason(request)? {
            return Ok(JobExecution::Skipped { reason });
        }
        let prepared = prepare_request(request, emit)?;
        let lut_name = prepared
            .settings
            .camera_lut_path
            .as_deref()
            .and_then(std::path::Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_owned);
        execute_prepared_job(task_id, &prepared, control, emit)
            .map(|output| JobExecution::Converted { output, lut_name })
    })
}

fn run_with_retry<T>(
    mode: ContentMode,
    control: &ConversionControl,
    retry_delay: Duration,
    emit: &mut dyn FnMut(ConversionEvent),
    mut operation: impl FnMut(&mut dyn FnMut(ConversionEvent)) -> Result<T, EngineError>,
) -> Result<T, EngineError> {
    const ATTEMPTS: usize = 2;

    let mut attempt = 1;
    loop {
        match operation(emit) {
            Ok(output) => return Ok(output),
            Err(error)
                if mode != ContentMode::PhotoSlideshow && worker_error_is_locked_input(&error) =>
            {
                emit(ConversionEvent::Warning(format!(
                    "Locked input detected: {error}; waiting to retry"
                )));
                wait_for_retry(retry_delay.min(Duration::from_secs(1)), control)?;
            }
            Err(error) if attempt < ATTEMPTS && worker_error_should_retry(mode, &error) => {
                emit(ConversionEvent::Warning(format!(
                    "Conversion attempt {attempt} failed: {error}; retrying in {} seconds",
                    retry_delay.as_secs()
                )));
                wait_for_retry(retry_delay, control)?;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn wait_for_retry(delay: Duration, control: &ConversionControl) -> Result<(), EngineError> {
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        std::thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        );
    }
    Ok(())
}

fn execute_prepared_job(
    task_id: &str,
    request: &ConversionRequest,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<PathBuf, EngineError> {
    if matches!(
        request.settings.mode,
        ContentMode::Trim | ContentMode::Stabilize | ContentMode::PhotoSlideshow
    ) {
        let staging = staging_path(&request.output, task_id)?;
        if staging.exists() {
            return Err(EngineError::Failed(format!(
                "staging output already exists: {}",
                staging.display()
            )));
        }
        let staged_request = ConversionRequest {
            input: request.input.clone(),
            output: staging.clone(),
            settings: request.settings.clone(),
        };
        if let Err(error) = execute_conversion(&staged_request, control, emit) {
            let _ = std::fs::remove_file(&staging);
            return Err(error);
        }
        publish_specialized_output(
            &staging,
            &request.output,
            task_id,
            request.settings.mode == ContentMode::Trim,
        )?;
        return Ok(request.output.clone());
    }
    execute_with_backup(task_id, request, control, emit)
}

fn publish_specialized_output(
    staging: &std::path::Path,
    output: &std::path::Path,
    task_id: &str,
    replace_existing: bool,
) -> Result<(), EngineError> {
    if !output.exists() {
        return match copy_new_file(staging, output) {
            Ok(()) => {
                let _ = std::fs::remove_file(staging);
                Ok(())
            }
            Err(error) => {
                let _ = std::fs::remove_file(staging);
                Err(io_failure("publishing output", &error))
            }
        };
    }
    if !replace_existing {
        let _ = std::fs::remove_file(staging);
        return Err(EngineError::Failed(format!(
            "publishing output: output already exists: {}",
            output.display()
        )));
    }

    let previous = replacement_backup_path(output, task_id)?;
    if previous.exists() {
        let _ = std::fs::remove_file(staging);
        return Err(EngineError::Failed(format!(
            "replacement backup already exists: {}",
            previous.display()
        )));
    }
    if let Err(error) = copy_new_file(output, &previous) {
        let _ = std::fs::remove_file(staging);
        return Err(io_failure("preserving previous output", &error));
    }
    if let Err(error) = std::fs::remove_file(output) {
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::remove_file(staging);
        return Err(io_failure("removing previous output", &error));
    }
    if let Err(publish_error) = copy_new_file(staging, output) {
        if let Err(rollback_error) = copy_new_file(&previous, output) {
            return Err(EngineError::Failed(format!(
                "publishing replacement failed ({publish_error}); previous-output rollback also failed ({rollback_error}). Recover the previous output from {} and the replacement from {}",
                previous.display(),
                staging.display()
            )));
        }
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::remove_file(staging);
        return Err(io_failure("publishing replacement output", &publish_error));
    }
    let _ = std::fs::remove_file(staging);
    let _ = std::fs::remove_file(&previous);
    Ok(())
}

fn replacement_backup_path(
    output: &std::path::Path,
    task_id: &str,
) -> Result<PathBuf, EngineError> {
    let name = output
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| EngineError::Failed("output filename is missing".to_owned()))?;
    system_temporary_file_path(&format!(
        "{name}.videoferry-previous-{}-{task_id}",
        std::process::id()
    ))
}

#[cfg(any(feature = "native-ffmpeg", test))]
fn camera_run_info_from_media(settings: &QueueSettings, media: &MediaInfo) -> CameraRunInfo {
    let Some(profile) = dji_camera_profile(media) else {
        return CameraRunInfo::default();
    };
    let lut_status = (settings.mode == ContentMode::CameraVideos).then(|| {
        if settings.apply_lut && !settings.encoder.is_hardware() {
            profile.lut_name.to_owned()
        } else {
            "Disabled".to_owned()
        }
    });
    CameraRunInfo {
        model_name: Some(profile.model_name.to_owned()),
        lut_status,
    }
}

#[cfg(feature = "native-ffmpeg")]
fn camera_run_info(request: &ConversionRequest) -> CameraRunInfo {
    let Ok(engine) = videoferry_ffmpeg::NativeEngine::new() else {
        return CameraRunInfo::default();
    };
    let Ok(media) = engine.probe(&request.input) else {
        return CameraRunInfo::default();
    };
    camera_run_info_from_media(&request.settings, &media)
}

#[cfg(not(feature = "native-ffmpeg"))]
fn camera_run_info(_request: &ConversionRequest) -> CameraRunInfo {
    CameraRunInfo::default()
}

#[cfg(feature = "native-ffmpeg")]
fn camera_source_skip_reason(request: &ConversionRequest) -> Result<Option<String>, EngineError> {
    if !descriptor(request.settings.mode, request.settings.encoder).ignore_existing_encoded_source {
        return Ok(None);
    }
    let engine = videoferry_ffmpeg::NativeEngine::new()?;
    let media = engine.probe(&request.input)?;
    let Some(encoded_library) = engine.encoded_library_name(&request.input)? else {
        return Ok(None);
    };
    let expected_library = python_existing_library_name(request.settings.encoder);
    if encoded_library != expected_library {
        return Ok(None);
    }
    let Some(source_fps) = media.frame_rate else {
        return Ok(None);
    };
    let shared_lowest = match request.settings.fps {
        FpsPolicy::SharedLowest => {
            let parent = request
                .input
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            std::fs::read_dir(parent)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    is_video_path(&path)
                        .then(|| engine.probe(&path).ok()?.frame_rate)
                        .flatten()
                })
                .reduce(f64::min)
        }
        FpsPolicy::Source | FpsPolicy::Exact(_) => None,
    };
    let fps_matches = encoded_source_matches_fps(source_fps, request.settings.fps, shared_lowest);
    Ok(fps_matches.then(|| {
        format!(
            "{} already uses {encoded_library} at {source_fps} FPS",
            request.input.display()
        )
    }))
}

#[cfg(not(feature = "native-ffmpeg"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "keeps the native and portable worker contracts identical"
)]
fn camera_source_skip_reason(_request: &ConversionRequest) -> Result<Option<String>, EngineError> {
    Ok(None)
}

#[cfg(any(feature = "native-ffmpeg", test))]
const fn python_existing_library_name(encoder: Encoder) -> &'static str {
    match encoder {
        Encoder::X264 => "x264",
        Encoder::SvtAv1 => "libsvtav1",
        Encoder::X265
        | Encoder::HevcNvenc
        | Encoder::Av1Nvenc
        | Encoder::H264Nvenc
        | Encoder::H264VideoToolbox
        | Encoder::HevcVideoToolbox
        | Encoder::Av1VideoToolbox => "x265",
    }
}

#[cfg(any(feature = "native-ffmpeg", test))]
fn encoded_source_matches_fps(
    source_fps: f64,
    policy: FpsPolicy,
    shared_lowest: Option<f64>,
) -> bool {
    match policy {
        FpsPolicy::Source => true,
        FpsPolicy::Exact(target) => (source_fps - target).abs() < 0.01,
        FpsPolicy::SharedLowest => {
            shared_lowest.is_some_and(|target| (source_fps - target).abs() < 0.01)
        }
    }
}

#[cfg(feature = "native-ffmpeg")]
fn prepare_request(
    request: &ConversionRequest,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<ConversionRequest, EngineError> {
    let mut prepared = request.clone();
    prepared.settings.camera_lut_path = None;
    if prepared.settings.mode != ContentMode::CameraVideos
        || !prepared.settings.apply_lut
        || prepared.settings.encoder.is_hardware()
    {
        return Ok(prepared);
    }
    let engine = videoferry_ffmpeg::NativeEngine::new()?;
    let media = engine.probe(&prepared.input)?;
    let Some(profile) = dji_camera_profile(&media) else {
        emit(ConversionEvent::Warning(
            "DJI LUT skipped: camera metadata did not match Action 6 or Pocket 3".to_owned(),
        ));
        return Ok(prepared);
    };
    let lut_path = resolve_lut_path(profile.lut_name).ok_or_else(|| {
        EngineError::Unavailable(format!("matching DJI LUT is missing: {}", profile.lut_name))
    })?;
    emit(ConversionEvent::Warning(format!(
        "Applying {} LUT ({})",
        profile.model_name,
        lut_path.display()
    )));
    prepared.settings.camera_lut_path = Some(lut_path);
    Ok(prepared)
}

#[cfg(not(feature = "native-ffmpeg"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "keeps the native and portable worker contracts identical"
)]
fn prepare_request(
    request: &ConversionRequest,
    _emit: &mut dyn FnMut(ConversionEvent),
) -> Result<ConversionRequest, EngineError> {
    let mut prepared = request.clone();
    prepared.settings.camera_lut_path = None;
    Ok(prepared)
}

#[cfg(feature = "native-ffmpeg")]
fn resolve_lut_path(name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(directory) = std::env::var_os("VIDEOFERRY_LUT_DIR") {
        candidates.push(PathBuf::from(directory).join(name));
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("lut").join("dji").join(name));
        candidates.push(
            directory
                .join("resources")
                .join("lut")
                .join("dji")
                .join(name),
        );
        if let Some(contents_directory) = directory.parent() {
            candidates.push(
                contents_directory
                    .join("Resources")
                    .join("lut")
                    .join("dji")
                    .join(name),
            );
        }
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/lut/dji")
            .join(name),
    );
    if let Ok(directory) = std::env::current_dir() {
        candidates.push(directory.join("lut").join("dji").join(name));
        candidates.push(directory.join("../lut").join("dji").join(name));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .and_then(|path| path.canonicalize().ok())
}

fn execute_with_backup(
    task_id: &str,
    request: &ConversionRequest,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<PathBuf, EngineError> {
    let final_output = &request.output;
    let backup = backup_path(&request.input)?;
    if backup.exists() {
        return Err(EngineError::Failed(format!(
            "original backup already exists: {}",
            backup.display()
        )));
    }
    let staging = staging_path(final_output, task_id)?;
    if staging.exists() {
        return Err(EngineError::Failed(format!(
            "staging output already exists: {}",
            staging.display()
        )));
    }
    let staged_request = ConversionRequest {
        input: request.input.clone(),
        output: staging.clone(),
        settings: request.settings.clone(),
    };
    if let Err(error) = execute_conversion(&staged_request, control, emit) {
        let _ = std::fs::remove_file(&staging);
        return Err(error);
    }
    finalize_conversion_replacing_output(&request.input, &staging, final_output, &backup, task_id)?;
    Ok(final_output.clone())
}

fn finalize_conversion_replacing_output(
    input: &std::path::Path,
    staging: &std::path::Path,
    output: &std::path::Path,
    backup: &std::path::Path,
    task_id: &str,
) -> Result<(), EngineError> {
    if output == input || !output.exists() {
        return finalize_conversion(input, staging, output, backup);
    }

    let previous = replacement_backup_path(output, task_id)?;
    if previous.exists() {
        let _ = std::fs::remove_file(staging);
        return Err(EngineError::Failed(format!(
            "replacement backup already exists: {}",
            previous.display()
        )));
    }
    if let Err(error) = copy_new_file(output, &previous) {
        let _ = std::fs::remove_file(staging);
        return Err(io_failure("preserving previous output", &error));
    }
    if let Err(error) = std::fs::remove_file(output) {
        let _ = std::fs::remove_file(&previous);
        let _ = std::fs::remove_file(staging);
        return Err(io_failure("removing previous output", &error));
    }
    match finalize_conversion(input, staging, output, backup) {
        Ok(()) => {
            let _ = std::fs::remove_file(&previous);
            Ok(())
        }
        Err(error) => match copy_new_file(&previous, output) {
            Ok(()) => {
                let _ = std::fs::remove_file(&previous);
                Err(error)
            }
            Err(rollback_error) => Err(EngineError::Failed(format!(
                "{error}; previous-output rollback also failed ({rollback_error}). Recover the previous output from {}",
                previous.display()
            ))),
        },
    }
}

fn finalize_conversion(
    input: &std::path::Path,
    staging: &std::path::Path,
    output: &std::path::Path,
    backup: &std::path::Path,
) -> Result<(), EngineError> {
    if output != input && output.exists() {
        let _ = std::fs::remove_file(staging);
        return Err(EngineError::Failed(format!(
            "publishing converted output: output already exists: {}",
            output.display()
        )));
    }
    let original_dir = backup
        .parent()
        .ok_or_else(|| EngineError::Failed("backup directory is missing".to_owned()))?;
    if let Err(error) = std::fs::create_dir_all(original_dir) {
        let _ = std::fs::remove_file(staging);
        return Err(io_failure("creating original backup directory", &error));
    }
    if let Err(error) = std::fs::rename(input, backup) {
        let _ = std::fs::remove_file(staging);
        return Err(io_failure("moving source into original", &error));
    }
    if let Err(publish_error) = copy_new_file(staging, output) {
        if let Err(rollback_error) = std::fs::rename(backup, input) {
            return Err(EngineError::Failed(format!(
                "publishing output failed ({publish_error}); source rollback also failed ({rollback_error}). Recover source from {} and converted file from {}",
                backup.display(),
                staging.display()
            )));
        }
        let _ = std::fs::remove_file(staging);
        return Err(io_failure("publishing converted output", &publish_error));
    }
    let _ = std::fs::remove_file(staging);
    Ok(())
}

fn copy_new_file(source: &std::path::Path, destination: &std::path::Path) -> std::io::Result<()> {
    let mut input = std::fs::File::open(source)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = std::io::copy(&mut input, &mut output).and_then(|_| output.sync_all());
    drop(output);
    if result.is_err() {
        let _ = std::fs::remove_file(destination);
    }
    result
}

fn backup_path(input: &std::path::Path) -> Result<PathBuf, EngineError> {
    let parent = input
        .parent()
        .ok_or_else(|| EngineError::Failed("source directory is missing".to_owned()))?;
    let name = input
        .file_name()
        .ok_or_else(|| EngineError::Failed("source filename is missing".to_owned()))?;
    Ok(parent.join("original").join(name))
}

fn task_has_remaining_work(task: &QueueTask) -> bool {
    if task_is_aggregate(task) {
        return !remaining_task_inputs(task, None).is_empty();
    }
    let Some(input) = task.targets.first() else {
        return false;
    };
    if task.skipped_paths.contains(input) {
        return false;
    }
    match task.settings.mode {
        ContentMode::PhotoSlideshow => true,
        ContentMode::Trim => task.settings.trim_start.is_some() && task.settings.trim_end.is_some(),
        _ => !should_skip_queue_source(input, &task.settings),
    }
}

fn should_skip_queue_source(path: &std::path::Path, settings: &QueueSettings) -> bool {
    if settings.mode == ContentMode::Stabilize {
        return is_stabilized_export(path) || stabilized_output_path(path).exists();
    }
    if has_original_backup_stem(path) {
        return true;
    }
    false
}

fn has_original_backup_stem(path: &std::path::Path) -> bool {
    let Some(stem) = path.file_stem() else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    std::fs::read_dir(parent.join("original"))
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| entry.path().file_stem() == Some(stem))
}

fn wait_until_file_stable(
    path: &std::path::Path,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<(), EngineError> {
    if !path.is_file() {
        return Ok(());
    }
    let mut warned = false;
    loop {
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        let before = std::fs::metadata(path).map_err(|error| {
            EngineError::Failed(format!(
                "reading source metadata for {}: {error}",
                path.display()
            ))
        })?;
        std::thread::sleep(Duration::from_secs(1));
        if control.checkpoint() != ControlDecision::Continue {
            return Err(EngineError::Cancelled);
        }
        let after = std::fs::metadata(path).map_err(|error| {
            EngineError::Failed(format!(
                "reading source metadata for {}: {error}",
                path.display()
            ))
        })?;
        let stable = before.len() == after.len() && before.modified().ok() == after.modified().ok();
        if stable {
            return Ok(());
        }
        if !warned {
            emit(ConversionEvent::Warning(format!(
                "Waiting for {} to finish copying",
                path.display()
            )));
            warned = true;
        }
    }
}

fn staging_path(output: &std::path::Path, task_id: &str) -> Result<PathBuf, EngineError> {
    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("output");
    let extension = output
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| EngineError::Failed("output extension is missing".to_owned()))?;
    system_temporary_file_path(&format!(
        "{stem}.videoferry-stage-{}-{task_id}.{extension}",
        std::process::id()
    ))
}

fn system_temporary_file_path(name: &str) -> Result<PathBuf, EngineError> {
    let directory = videoferry_temporary_directory();
    std::fs::create_dir_all(&directory)
        .map_err(|error| io_failure("creating system temporary directory", &error))?;
    Ok(directory.join(name))
}

fn videoferry_temporary_directory() -> PathBuf {
    std::env::temp_dir().join("VideoFerry")
}

/// Removes temporary conversion artifacts left by earlier app processes.
///
/// Files owned by another process that is still running are preserved.
///
/// # Errors
///
/// Returns an error when the temporary directory cannot be read or a stale
/// `VideoFerry` file cannot be removed.
pub fn cleanup_stale_temporary_files() -> Result<usize, EngineError> {
    let directory = videoferry_temporary_directory();
    cleanup_temporary_directory(&directory, std::process::id(), process_is_running)
        .map_err(|error| io_failure("cleaning stale system temporary files", &error))
}

fn cleanup_temporary_directory(
    directory: &std::path::Path,
    current_process_id: u32,
    mut process_is_running: impl FnMut(u32) -> bool,
) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut active_processes = HashMap::<u32, bool>::new();
    let mut removed = 0_usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some(owner_process_id) = temporary_file_process_id(name) else {
            continue;
        };
        if owner_process_id != current_process_id
            && *active_processes
                .entry(owner_process_id)
                .or_insert_with(|| process_is_running(owner_process_id))
        {
            continue;
        }
        std::fs::remove_file(path)?;
        removed = removed.saturating_add(1);
    }
    Ok(removed)
}

fn temporary_file_process_id(name: &str) -> Option<u32> {
    [
        ".videoferry-stage-",
        ".videoferry-previous-",
        ".videoferry-partial-",
    ]
    .into_iter()
    .find_map(|marker| {
        let (_, suffix) = name.split_once(marker)?;
        suffix.split('-').next()?.parse().ok()
    })
}

#[cfg(target_os = "windows")]
fn process_is_running(process_id: u32) -> bool {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let filter = format!("PID eq {process_id}");
    std::process::Command::new("tasklist.exe")
        .args(["/FI", filter.as_str(), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_or(true, |output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .contains(&format!("\",\"{process_id}\","))
        })
}

#[cfg(target_os = "linux")]
fn process_is_running(process_id: u32) -> bool {
    std::path::Path::new("/proc")
        .join(process_id.to_string())
        .exists()
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn process_is_running(process_id: u32) -> bool {
    let process_id = process_id.to_string();
    std::process::Command::new("ps")
        .args(["-p", process_id.as_str(), "-o", "pid="])
        .output()
        .map_or(true, |output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == process_id
        })
}

fn collect_video_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if !is_skipped_conversion_directory(&path) {
                    pending.push(path);
                }
            } else if is_video_path(&path) {
                files.push(path);
            }
        }
    }
    files.sort_by_key(|path| full_path_natural_key(path));
    files
}

fn expanded_task_targets(
    task: &QueueTask,
    already_queued: &HashSet<PathBuf>,
) -> Vec<(PathBuf, Option<PathBuf>)> {
    let roots = task
        .targets
        .iter()
        .filter(|path| path.is_dir())
        .cloned()
        .collect::<Vec<_>>();
    let mut files = task
        .targets
        .iter()
        .flat_map(|path| {
            if path.is_dir() {
                collect_video_files(path)
            } else {
                vec![path.clone()]
            }
        })
        .filter(|path| {
            !already_queued.contains(path) && !should_skip_queue_source(path, &task.settings)
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|path| full_path_natural_key(path));
    files.dedup();
    files
        .into_iter()
        .map(|path| {
            let root = roots.iter().find(|root| path.starts_with(root)).cloned();
            (path, root)
        })
        .collect()
}

fn remaining_task_inputs(
    task: &QueueTask,
    failures: Option<&HashMap<PathBuf, String>>,
) -> Vec<PathBuf> {
    expanded_task_targets(task, &HashSet::new())
        .into_iter()
        .map(|(path, _)| path)
        .filter(|path| failures.is_none_or(|failures| !failures.contains_key(path)))
        .filter(|path| !task.skipped_paths.contains(path))
        .collect()
}

fn task_is_aggregate(task: &QueueTask) -> bool {
    task.settings.mode != ContentMode::PhotoSlideshow
        && (task.targets.len() > 1 || task.targets.iter().any(|target| target.is_dir()))
}

fn folder_snapshot_for_settings(
    root: &std::path::Path,
    settings: &QueueSettings,
) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
    let files = if settings.mode == ContentMode::PhotoSlideshow {
        collect_photo_files(root)
    } else {
        collect_video_files(root)
    };
    files
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            Some((relative, metadata.len(), metadata.modified().ok()))
        })
        .collect()
}

fn folder_queue_summaries(
    watches: &[WatchedFolder],
    queue: &Queue,
    failures: &HashMap<String, HashMap<PathBuf, String>>,
) -> Vec<FolderQueueSummary> {
    let mut summaries = watches
        .iter()
        .map(|watch| folder_queue_summary_with_outcomes(watch, queue, failures))
        .collect::<Vec<_>>();
    summaries.sort_by_key(|summary| summary.root.to_string_lossy().to_ascii_lowercase());
    summaries
}

#[cfg(test)]
fn folder_queue_summary(watch: &WatchedFolder, queue: &Queue) -> FolderQueueSummary {
    folder_queue_summary_with_outcomes(watch, queue, &HashMap::new())
}

#[cfg(test)]
fn folder_queue_summary_with_failures(
    watch: &WatchedFolder,
    queue: &Queue,
    failures: &HashMap<String, HashMap<PathBuf, String>>,
) -> FolderQueueSummary {
    folder_queue_summary_with_outcomes(watch, queue, failures)
}

fn folder_queue_summary_with_outcomes(
    watch: &WatchedFolder,
    queue: &Queue,
    failures: &HashMap<String, HashMap<PathBuf, String>>,
) -> FolderQueueSummary {
    if watch.settings.mode == ContentMode::PhotoSlideshow {
        let task = queue.tasks().iter().find(|task| {
            task.settings.mode == ContentMode::PhotoSlideshow
                && task.targets.iter().any(|target| target == &watch.root)
        });
        let valid = task.is_some_and(|task| {
            task.settings.slideshow_image_paths.len() >= 2 || directory_has_two_photos(&watch.root)
        });
        let completed =
            usize::from(valid && task.is_some_and(|task| task.status == QueueStatus::Completed));
        return FolderQueueSummary {
            root: watch.root.clone(),
            mode: watch.settings.mode,
            encoder: watch.settings.encoder,
            folders: usize::from(watch.root.is_dir()),
            files: usize::from(valid),
            remaining: usize::from(valid && completed == 0),
            converted: completed,
            failed: 0,
            active_status: task
                .map(|task| task.status)
                .filter(|status| matches!(status, QueueStatus::Running | QueueStatus::Paused)),
        };
    }
    let mut states = HashMap::<String, SummaryFileState>::new();
    let mut active_status = None;
    for task in queue.tasks().iter().filter(|task| {
        task.source_root.as_deref() == Some(&watch.root)
            || task.targets.iter().any(|target| target == &watch.root)
    }) {
        if matches!(task.status, QueueStatus::Running | QueueStatus::Paused) {
            active_status = Some(task.status);
        }
        let state = match task.status {
            QueueStatus::Completed => SummaryFileState::Converted,
            QueueStatus::Failed | QueueStatus::Cancelled => SummaryFileState::Failed,
            QueueStatus::Pending | QueueStatus::Running | QueueStatus::Paused => {
                SummaryFileState::Remaining
            }
        };
        for target in &task.targets {
            if has_video_filename(target) {
                states.insert(summary_path_key(target), state);
            }
        }
        if let Some(task_failures) = failures.get(&task.id) {
            for failed_path in task_failures.keys() {
                states.insert(summary_path_key(failed_path), SummaryFileState::Failed);
            }
        }
        for skipped_path in &task.skipped_paths {
            states.insert(summary_path_key(skipped_path), SummaryFileState::Converted);
        }
    }

    let folders = scan_folder_summary(watch, &mut states);
    let remaining = states
        .values()
        .filter(|state| **state == SummaryFileState::Remaining)
        .count();
    let converted = states
        .values()
        .filter(|state| **state == SummaryFileState::Converted)
        .count();
    let failed = states
        .values()
        .filter(|state| **state == SummaryFileState::Failed)
        .count();
    FolderQueueSummary {
        root: watch.root.clone(),
        mode: watch.settings.mode,
        encoder: watch.settings.encoder,
        folders,
        files: states.len(),
        remaining,
        converted,
        failed,
        active_status,
    }
}

fn scan_folder_summary(
    watch: &WatchedFolder,
    states: &mut HashMap<String, SummaryFileState>,
) -> usize {
    if !watch.root.is_dir() {
        if let Some(renamed) = renamed_conversion_directory(&watch.root, &watch.settings) {
            return scan_completed_directory(&renamed, states);
        }
        return 0;
    }
    let mut folders = 0_usize;
    let mut pending = vec![watch.root.clone()];
    while let Some(directory) = pending.pop() {
        folders = folders.saturating_add(1);
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|value| value.to_str());
                if name == Some("original") {
                    // A backup proves completion only when its matching public
                    // output is present. The public-file branch below checks
                    // the backup stem and records that output exactly once.
                } else if is_skipped_conversion_directory(&path) {
                    if watch.settings.mode != ContentMode::CameraVideos {
                        for converted in collect_all_video_files(&path) {
                            states
                                .entry(format!("converted-dir:{}", summary_path_key(&converted)))
                                .or_insert(SummaryFileState::Converted);
                        }
                    }
                } else {
                    pending.push(path);
                }
            } else if is_video_path(&path) {
                let exact_key = summary_path_key(&path);
                let converted = should_skip_queue_source(&path, &watch.settings);
                states
                    .entry(exact_key)
                    .and_modify(|state| {
                        if converted && *state == SummaryFileState::Remaining {
                            *state = SummaryFileState::Converted;
                        }
                    })
                    .or_insert(if converted {
                        SummaryFileState::Converted
                    } else {
                        SummaryFileState::Remaining
                    });
            }
        }
    }
    folders
}

fn renamed_conversion_directory(
    original: &std::path::Path,
    settings: &QueueSettings,
) -> Option<PathBuf> {
    let name = original.file_name()?.to_str()?;
    let mut suffixes = Vec::with_capacity(CONVERTED_DIRECTORY_SUFFIXES.len());
    if let Some(suffix) = converted_directory_suffix(settings.mode, settings.encoder) {
        suffixes.push(suffix.trim_start());
    }
    for suffix in CONVERTED_DIRECTORY_SUFFIXES {
        if !suffixes.contains(&suffix) {
            suffixes.push(suffix);
        }
    }
    suffixes
        .into_iter()
        .map(|suffix| original.with_file_name(format!("{name} {suffix}")))
        .find(|candidate| candidate.is_dir())
}

fn scan_completed_directory(
    root: &std::path::Path,
    states: &mut HashMap<String, SummaryFileState>,
) -> usize {
    let mut folders = 0_usize;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        folders = folders.saturating_add(1);
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|value| value.to_str());
                if !matches!(name, Some("original" | "chs")) {
                    pending.push(path);
                }
            } else if is_video_path(&path) {
                states.insert(
                    format!("converted-dir:{}", summary_path_key(&path)),
                    SummaryFileState::Converted,
                );
            }
        }
    }
    folders
}

fn collect_all_video_files(root: &std::path::Path) -> Vec<PathBuf> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if is_video_path(&path) {
                files.push(path);
            }
        }
    }
    files
}

fn summary_path_key(path: &std::path::Path) -> String {
    path.to_string_lossy().to_ascii_lowercase()
}

fn folder_summary_status(summary: &FolderQueueSummary) -> &'static str {
    match summary.active_status {
        Some(QueueStatus::Running) => "Running",
        Some(QueueStatus::Paused) => "Paused",
        _ if summary.failed > 0 && summary.remaining == 0 => "Failed",
        _ if summary.failed > 0 => "Needs attention",
        _ if summary.remaining > 0 => "Pending",
        _ if summary.files > 0 => "Completed",
        _ => "Empty",
    }
}

fn restored_folder_watches(queue: &Queue) -> Vec<WatchedFolder> {
    let mut watches = Vec::<WatchedFolder>::new();
    for task in queue.tasks() {
        let roots = task
            .source_root
            .iter()
            .cloned()
            .chain(task.targets.iter().filter(|path| path.is_dir()).cloned());
        for root in roots {
            if watches.iter().any(|watch| watch.root == root) {
                continue;
            }
            watches.push(WatchedFolder {
                // Force one recovery scan so files copied while the app was closed
                // are added even though they were absent from the persisted queue.
                snapshot: Vec::new(),
                root,
                settings: task.settings.clone(),
            });
        }
    }
    watches
}

fn rename_completed_task_directories(task: &QueueTask) -> Vec<String> {
    let Some(suffix) = converted_directory_suffix(task.settings.mode, task.settings.encoder) else {
        return Vec::new();
    };
    let mut directories = Vec::new();
    for target in &task.targets {
        if target.is_dir() {
            collect_rename_directories(target, &mut directories);
        } else if let Some(parent) = target.parent() {
            directories.push(parent.to_path_buf());
        }
    }
    let mut seen = HashSet::new();
    directories.retain(|path| seen.insert(path.clone()));
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

    let mut messages = Vec::new();
    for directory in directories {
        let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("0_") || name.ends_with(suffix) || !directory.is_dir() {
            continue;
        }
        if !directory_tree_is_conversion_complete(&directory) {
            continue;
        }
        let renamed = directory.with_file_name(format!("{name}{suffix}"));
        if renamed.exists() {
            messages.push(format!(
                "Unable to rename completed folder {}: {} already exists",
                directory.display(),
                renamed.display()
            ));
            continue;
        }
        match std::fs::rename(&directory, &renamed) {
            Ok(()) => messages.push(format!(
                "Renamed completed folder {} to {}",
                directory.display(),
                renamed.display()
            )),
            Err(error) => messages.push(format!(
                "Unable to rename completed folder {}: {error}",
                directory.display()
            )),
        }
    }
    messages
}

fn directory_tree_is_conversion_complete(directory: &std::path::Path) -> bool {
    if direct_video_count(directory) != direct_video_count(&directory.join("original")) {
        return false;
    }
    std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && !is_skipped_conversion_directory(path))
        .all(|path| directory_tree_is_conversion_complete(&path))
}

fn collect_rename_directories(root: &std::path::Path, directories: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    directories.push(root.to_path_buf());
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && !is_skipped_conversion_directory(&path) {
            collect_rename_directories(&path, directories);
        }
    }
}

fn direct_video_count(directory: &std::path::Path) -> usize {
    std::fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| is_video_path(&entry.path()))
        .count()
}

const CONVERTED_DIRECTORY_SUFFIXES: [&str; 9] = [
    "(x265)",
    "(x264)",
    "(libsvtav1)",
    "(hevc_nvenc)",
    "(av1_nvenc)",
    "(h264_nvenc)",
    "(h264_videotoolbox)",
    "(hevc_videotoolbox)",
    "(av1_videotoolbox)",
];

fn is_skipped_conversion_directory(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == "original"
        || name == "chs"
        || CONVERTED_DIRECTORY_SUFFIXES
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

fn is_video_path(path: &std::path::Path) -> bool {
    path.is_file() && has_video_filename(path)
}

fn has_video_filename(path: &std::path::Path) -> bool {
    !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("._"))
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                ["mkv", "mp4", "mov", "avi", "wmv", "flv", "rm", "rmvb"]
                    .iter()
                    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
            })
}

fn collect_photo_files(root: &std::path::Path) -> Vec<PathBuf> {
    if is_photo_path(root) {
        return vec![root.to_path_buf()];
    }
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if is_photo_path(&path) {
                files.push(path);
            }
        }
    }
    files.sort_by_key(|path| slideshow_natural_key(path));
    files
}

fn directory_has_two_photos(root: &std::path::Path) -> bool {
    let mut pending = vec![root.to_path_buf()];
    let mut count = 0_u8;
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if is_photo_path(&path) {
                count += 1;
                if count == 2 {
                    return true;
                }
            }
        }
    }
    false
}

fn rebuild_slideshow_task_images(task: &mut QueueTask) {
    if task.settings.mode != ContentMode::PhotoSlideshow {
        return;
    }
    let mut images = task
        .targets
        .iter()
        .flat_map(|target| collect_photo_files(target))
        .collect::<Vec<_>>();
    images.sort_by_key(|path| slideshow_natural_key(path));
    images.dedup();
    task.settings.slideshow_image_paths.clone_from(&images);
    task.settings.slideshow_review_image_paths = images;
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SlideshowNaturalPart {
    Number(u128, usize),
    Text(String),
}

fn slideshow_natural_key(path: &std::path::Path) -> Vec<SlideshowNaturalPart> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let mut parts = natural_key(name);
    parts.push(SlideshowNaturalPart::Text(
        path.to_string_lossy().to_ascii_lowercase(),
    ));
    parts
}

fn full_path_natural_key(path: &std::path::Path) -> Vec<SlideshowNaturalPart> {
    natural_key(&path.to_string_lossy())
}

fn natural_key(value: &str) -> Vec<SlideshowNaturalPart> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut digits = None;
    for character in value.to_lowercase().chars() {
        let is_digit = character.is_ascii_digit();
        if digits.is_some_and(|value| value != is_digit) {
            parts.push(slideshow_natural_part(&current, digits.unwrap_or(false)));
            current.clear();
        }
        digits = Some(is_digit);
        current.push(character);
    }
    if !current.is_empty() {
        parts.push(slideshow_natural_part(&current, digits.unwrap_or(false)));
    }
    parts
}

fn slideshow_natural_part(value: &str, digits: bool) -> SlideshowNaturalPart {
    if digits {
        SlideshowNaturalPart::Number(value.parse().unwrap_or(u128::MAX), value.len())
    } else {
        SlideshowNaturalPart::Text(value.to_owned())
    }
}

fn is_photo_path(path: &std::path::Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                ["jpg", "jpeg", "png", "webp", "bmp", "tif", "tiff"]
                    .iter()
                    .any(|candidate| extension.eq_ignore_ascii_case(candidate))
            })
}

fn next_slideshow_output_path(input: &std::path::Path) -> PathBuf {
    let directory = if input.is_dir() {
        input
    } else {
        input.parent().unwrap_or_else(|| std::path::Path::new("."))
    };
    let first = directory.join("slideshow.mp4");
    if !first.exists() {
        return first;
    }
    for suffix in 1_u64.. {
        let candidate = directory.join(format!("slideshow_{suffix}.mp4"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("the slideshow suffix space is not finite")
}

fn is_stabilized_export(path: &std::path::Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
        return false;
    };
    if stem.ends_with("_stabilized") {
        return true;
    }
    let Some((prefix, suffix)) = stem.rsplit_once("_stabilized_") else {
        return false;
    };
    !prefix.is_empty() && !suffix.is_empty() && suffix.chars().all(|value| value.is_ascii_digit())
}

fn io_failure(operation: &str, error: &std::io::Error) -> EngineError {
    EngineError::Failed(format!("{operation}: {error}"))
}

#[cfg(feature = "native-ffmpeg")]
fn execute_conversion(
    request: &ConversionRequest,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<(), EngineError> {
    videoferry_ffmpeg::NativeEngine::new()?.convert(request, control, emit)
}

#[cfg(not(feature = "native-ffmpeg"))]
fn execute_conversion(
    request: &ConversionRequest,
    control: &ConversionControl,
    emit: &mut dyn FnMut(ConversionEvent),
) -> Result<(), EngineError> {
    videoferry_ffmpeg::UnavailableEngine.convert(request, control, emit)
}

#[cfg(test)]
mod tests {
    use videoferry_core::ProgressRatio;

    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        CompletedHistoryRow, CompletedJob, ConverterApp, EncodingUiSettings, FolderQueueSummary,
        FpsUiMode, HistoryMediaInfo, QueueRunState, ReviewPhotoTarget, ReviewSlideTarget,
        ReviewWorkerRequest, SlintController, SlintTaskDraft, StreamCarryInfo, TaskDisplayCounts,
        WatchedFolder, active_item_position, camera_run_info_from_media,
        cleanup_temporary_directory, collect_new_video_task_targets, collect_video_files,
        completed_history_row, default_task_name, displayed_task_settings,
        encoded_source_matches_fps, encoding_ui_settings_for, estimated_output_bytes,
        estimated_remaining, expanded_task_targets, explicit_fps, finalize_conversion,
        finalize_conversion_replacing_output, folder_queue_summary,
        folder_queue_summary_with_failures, folder_snapshot_for_settings, folder_summary_status,
        format_carried_audio, format_carried_subtitles, format_clock, format_ffmpeg_progress_clock,
        format_fps, format_frame_progress, format_progress_time, format_size_mb, format_trim_time,
        fps_policy_status, fps_ui_mode, is_skipped_conversion_directory, is_stabilized_export,
        is_video_path, move_item_to_insertion, next_slideshow_output_path, output_bytes_per_minute,
        parse_trim_time, photo_zoom_after_wheel, prepare_request, progress_fraction,
        publish_specialized_output, python_existing_library_name, queue_admission_occupied_paths,
        queued_target_set, rebuild_slideshow_task_images, remaining_task_inputs,
        rename_completed_task_directories, replacement_backup_path, restored_folder_watches,
        review_request_is_priority, run_with_retry, select_trim_source, selected_task_files,
        should_inhibit_sleep, should_skip_queue_source, slideshow_natural_key,
        slideshow_review_is_editable, slint_settings_snapshot, staging_path, target_fps_status,
        target_fps_status_with_source, task_draft_output_summary, task_progress_segments,
        task_run_failure_summary, temporary_file_process_id, update_extended_selection,
        validate_task_targets, wait_for_retry, worker_error_is_locked_input,
        worker_error_is_queue_fatal, worker_error_should_retry, workflow_shows_encoder,
        workflow_shows_fps,
    };
    #[cfg(feature = "native-ffmpeg")]
    use super::{packaged_runtime_report, resolve_lut_path};
    use eframe::egui;
    use videoferry_core::{
        ContentMode, ConversionControl, ConversionEvent, ConversionProgress, Encoder, EngineError,
        FpsPolicy, MediaInfo, Queue, QueueStatus, QueueTask,
    };
    use videoferry_presets::default_settings;

    #[cfg(feature = "native-ffmpeg")]
    #[test]
    fn packaged_runtime_verification_reports_the_pinned_direct_engine() {
        let report = packaged_runtime_report().unwrap();
        assert!(report.starts_with("runtime=ok\nengine=FFmpeg 9.0.1"));
        assert!(
            report.contains("required_encoders=aac,ac3,libsvtav1,libx264,libx265,mov_text,srt")
        );
        assert!(report.contains("muxers=matroska,mp4"));
        assert!(report.contains("stabilization="));
    }

    #[test]
    fn finalization_backs_up_the_source_before_publication() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "videoferry-lifecycle-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("test directory");
        let source = root.join("episode.mkv");
        let staging = root.join(".episode.stage.mkv");
        let backup = root.join("original").join("episode.mkv");
        std::fs::write(&source, b"source").expect("source fixture");
        std::fs::write(&staging, b"converted").expect("output fixture");

        finalize_conversion(&source, &staging, &source, &backup).expect("finalize");

        assert_eq!(std::fs::read(&source).unwrap(), b"converted");
        assert_eq!(std::fs::read(&backup).unwrap(), b"source");
        assert!(!staging.exists());
        std::fs::remove_dir_all(&root).expect("remove test directory");
    }

    #[test]
    fn task_builder_explains_output_and_original_file_safety() {
        let root = temporary_test_directory("task-builder-safety");
        let source = root.join("camera.mp4");
        std::fs::write(&source, b"camera source").unwrap();
        let draft = SlintTaskDraft {
            settings: default_settings(ContentMode::CameraVideos, Encoder::X265),
            targets: vec![source],
        };

        let summary = task_draft_output_summary(&draft);

        assert!(summary.contains("Output:"));
        assert!(summary.contains("camera.mp4"));
        assert!(summary.contains("the original is moved into an original folder"));
        assert!(summary.contains("Selected source size:"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn task_builder_keeps_specialized_workflow_sources_in_place() {
        let root = temporary_test_directory("task-builder-stabilize-safety");
        let source = root.join("clip.mp4");
        std::fs::write(&source, b"camera source").unwrap();
        let draft = SlintTaskDraft {
            settings: default_settings(ContentMode::Stabilize, Encoder::X265),
            targets: vec![source],
        };

        let summary = task_draft_output_summary(&draft);

        assert!(summary.contains("clip_stabilized.mp4"));
        assert!(summary.contains("The source remains in place"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finalization_rolls_source_back_when_publication_fails() {
        let root = temporary_test_directory("publish-rollback");
        let source = root.join("episode.mp4");
        let output = root.join("missing-parent").join("episode.mkv");
        let staging = root.join(".episode.stage.mkv");
        let backup = root.join("original").join("episode.mp4");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&staging, b"converted").unwrap();

        let error = finalize_conversion(&source, &staging, &output, &backup).unwrap_err();

        assert!(error.to_string().contains("publishing converted output"));
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
        assert!(!output.exists());
        assert!(!backup.exists());
        assert!(!staging.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finalization_never_overwrites_a_late_output() {
        let root = temporary_test_directory("publish-no-clobber");
        let source = root.join("episode.mp4");
        let output = root.join("episode.mkv");
        let staging = root.join(".episode.stage.mkv");
        let backup = root.join("original").join("episode.mp4");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&staging, b"converted").unwrap();
        std::fs::write(&output, b"late output").unwrap();

        let error = finalize_conversion(&source, &staging, &output, &backup).unwrap_err();

        assert!(error.to_string().contains("output already exists"));
        assert_eq!(std::fs::read(&source).unwrap(), b"source");
        assert_eq!(std::fs::read(&output).unwrap(), b"late output");
        assert!(!backup.exists());
        assert!(!staging.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_conversion_replaces_an_existing_target_like_python() {
        let root = temporary_test_directory("replace-existing-target");
        let source = root.join("episode.mov");
        let output = root.join("episode.mkv");
        let staging = root.join(".episode.stage.mkv");
        let backup = root.join("original").join("episode.mov");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(&output, b"previous output").unwrap();
        std::fs::write(&staging, b"converted").unwrap();

        finalize_conversion_replacing_output(&source, &staging, &output, &backup, "task").unwrap();

        assert_eq!(std::fs::read(&output).unwrap(), b"converted");
        assert_eq!(std::fs::read(&backup).unwrap(), b"source");
        assert!(!staging.exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn temporary_publication_files_use_the_system_temp_directory() {
        let output_root = temporary_test_directory("staging-location");
        let output = output_root.join("episode.mkv");
        let path = staging_path(&output, "task-7").unwrap();
        let previous = replacement_backup_path(&output, "task-7").unwrap();
        let expected_parent = std::env::temp_dir().join("VideoFerry");

        assert_eq!(path.parent(), Some(expected_parent.as_path()));
        assert_eq!(previous.parent(), Some(expected_parent.as_path()));
        assert_ne!(path.parent(), output.parent());
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("mkv")
        );
        assert!(
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.contains("task-7"))
        );
        std::fs::remove_dir_all(output_root).unwrap();
    }

    #[test]
    fn specialized_publication_preserves_collisions_except_for_python_trim_replacement() {
        let root = temporary_test_directory("specialized-publication");
        let output = root.join("clip.mp4");
        let staging = root.join(".clip.stage.mp4");
        std::fs::write(&output, b"existing").unwrap();
        std::fs::write(&staging, b"stabilized").unwrap();

        let error = publish_specialized_output(&staging, &output, "task", false).unwrap_err();
        assert!(error.to_string().contains("output already exists"));
        assert_eq!(std::fs::read(&output).unwrap(), b"existing");
        assert!(!staging.exists());

        std::fs::write(&staging, b"trimmed").unwrap();
        publish_specialized_output(&staging, &output, "task", true).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"trimmed");
        assert!(!staging.exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detects_python_style_stabilized_exports() {
        assert!(is_stabilized_export(std::path::Path::new(
            "clip_stabilized.mp4"
        )));
        assert!(is_stabilized_export(std::path::Path::new(
            "clip_stabilized_2.mp4"
        )));
        assert!(!is_stabilized_export(std::path::Path::new(
            "clip_stabilized_final.mp4"
        )));
    }

    #[test]
    fn slideshow_output_starts_with_the_python_default_name() {
        let path = next_slideshow_output_path(std::path::Path::new("missing-photo-directory"));
        assert_eq!(path, std::path::Path::new("slideshow.mp4"));
    }

    #[test]
    fn review_uses_python_style_natural_photo_order() {
        let mut paths = [
            std::path::PathBuf::from("album/photo10.jpg"),
            std::path::PathBuf::from("album/photo2.jpg"),
        ];
        paths.sort_by_key(|path| slideshow_natural_key(path));
        assert_eq!(paths[0], std::path::Path::new("album/photo2.jpg"));
    }

    #[test]
    fn slideshow_review_drag_reorders_at_row_boundaries() {
        let mut photos = vec!["one", "two", "three", "four"];

        assert_eq!(move_item_to_insertion(&mut photos, 0, 3), Some(2));
        assert_eq!(photos, ["two", "three", "one", "four"]);
        assert_eq!(move_item_to_insertion(&mut photos, 3, 1), Some(1));
        assert_eq!(photos, ["two", "four", "three", "one"]);
        assert_eq!(move_item_to_insertion(&mut photos, 8, 0), None);
    }

    #[test]
    fn selected_review_previews_take_priority_over_row_thumbnails() {
        let thumbnail = ReviewWorkerRequest::Photo {
            request_id: 1,
            target: ReviewPhotoTarget::Thumbnail,
            path: "photo.jpg".into(),
            maximum_width: 96,
            maximum_height: 64,
        };
        let selected = ReviewWorkerRequest::Photo {
            request_id: 2,
            target: ReviewPhotoTarget::Review,
            path: "photo.jpg".into(),
            maximum_width: 640,
            maximum_height: 360,
        };
        let slide_thumbnail = ReviewWorkerRequest::SlidePreview {
            request_id: 3,
            target: ReviewSlideTarget::Thumbnail,
            paths: vec!["photo.jpg".into()],
            collage: false,
            width: 128,
            height: 72,
        };
        let selected_slide = ReviewWorkerRequest::SlidePreview {
            request_id: 4,
            target: ReviewSlideTarget::Selected,
            paths: vec!["photo.jpg".into()],
            collage: false,
            width: 640,
            height: 360,
        };

        assert!(!review_request_is_priority(&thumbnail));
        assert!(review_request_is_priority(&selected));
        assert!(!review_request_is_priority(&slide_thumbnail));
        assert!(review_request_is_priority(&selected_slide));
    }

    #[test]
    fn completed_or_partially_converted_slideshow_reviews_are_read_only() {
        let settings = default_settings(ContentMode::PhotoSlideshow, Encoder::X265);
        let mut task = QueueTask::new("slides", "Slides", vec!["album".into()], settings);

        assert!(slideshow_review_is_editable(&task, 0));
        task.status = QueueStatus::Failed;
        assert!(slideshow_review_is_editable(&task, 0));
        assert!(!slideshow_review_is_editable(&task, 1));
        task.status = QueueStatus::Completed;
        assert!(!slideshow_review_is_editable(&task, 0));
    }

    #[test]
    fn editing_slideshow_targets_invalidates_and_rebuilds_review_order() {
        let root = temporary_test_directory("slideshow-target-edit");
        let first = root.join("photo2.jpg");
        let second = root.join("photo10.jpg");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let mut settings = default_settings(ContentMode::PhotoSlideshow, Encoder::X265);
        settings.slideshow_image_paths = vec![second.clone()];
        settings.slideshow_review_image_paths = vec![second.clone()];
        let mut task = QueueTask::new("slides", "Slides", vec![root.clone()], settings);

        rebuild_slideshow_task_images(&mut task);

        assert_eq!(task.settings.slideshow_image_paths, [first, second]);
        assert_eq!(
            task.settings.slideshow_review_image_paths,
            task.settings.slideshow_image_paths
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restored_python_folder_target_remains_an_aggregate_watched_task() {
        let root = temporary_test_directory("restored-folder-target");
        let mut queue = Queue::default();
        queue
            .add(QueueTask::new(
                "folder",
                "Imported Python folder",
                vec![root.clone()],
                default_settings(ContentMode::Tv, Encoder::X265),
            ))
            .unwrap();

        let watches = restored_folder_watches(&queue);

        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].root, root);
        assert_eq!(queue.tasks().len(), 1);
        assert!(queue.tasks()[0].targets[0].is_dir());
        std::fs::remove_dir_all(&watches[0].root).unwrap();
    }

    #[test]
    fn restored_slideshow_folder_keeps_a_photo_aware_watch() {
        let root = temporary_test_directory("restored-slideshow-folder");
        std::fs::write(root.join("photo1.jpg"), b"one").unwrap();
        std::fs::write(root.join("photo2.png"), b"two").unwrap();
        std::fs::write(root.join("ignore.mp4"), b"video").unwrap();
        let settings = default_settings(ContentMode::PhotoSlideshow, Encoder::X265);
        let mut queue = Queue::default();
        queue
            .add(QueueTask::new(
                "slides",
                "Imported Python slideshow folder",
                vec![root.clone()],
                settings.clone(),
            ))
            .unwrap();

        let watches = restored_folder_watches(&queue);
        let snapshot = folder_snapshot_for_settings(&root, &settings);

        assert_eq!(watches.len(), 1);
        assert_eq!(watches[0].root, root);
        assert_eq!(watches[0].settings.mode, ContentMode::PhotoSlideshow);
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.iter().all(|(path, _, _)| {
            matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("jpg" | "png")
            )
        }));
        std::fs::remove_dir_all(&watches[0].root).unwrap();
    }

    #[test]
    fn slideshow_folder_summary_remains_one_job_when_photos_change() {
        let root = temporary_test_directory("slideshow-folder-summary");
        std::fs::write(root.join("photo1.jpg"), b"one").unwrap();
        std::fs::write(root.join("photo2.jpg"), b"two").unwrap();
        let settings = default_settings(ContentMode::PhotoSlideshow, Encoder::X265);
        let mut queue = Queue::default();
        queue
            .add(QueueTask::new(
                "slides",
                "Slideshow folder",
                vec![root.clone()],
                settings.clone(),
            ))
            .unwrap();
        let watch = WatchedFolder {
            root: root.clone(),
            settings,
            snapshot: Vec::new(),
        };

        let summary = folder_queue_summary(&watch, &queue);

        assert_eq!(summary.folders, 1);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.remaining, 1);
        assert_eq!(summary.converted, 0);
        assert_eq!(summary.failed, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fps_controls_preserve_python_policy_meaning() {
        assert!(matches!(
            fps_ui_mode(FpsPolicy::SharedLowest),
            FpsUiMode::SharedLowest
        ));
        assert!(matches!(fps_ui_mode(FpsPolicy::Source), FpsUiMode::Source));
        assert!(matches!(
            fps_ui_mode(FpsPolicy::Exact(23.976)),
            FpsUiMode::Explicit
        ));
        assert!((explicit_fps(FpsPolicy::Exact(23.976)) - 23.976).abs() < f64::EPSILON);
        assert!((explicit_fps(FpsPolicy::Source) - 30.0).abs() < f64::EPSILON);
        assert_eq!(
            fps_policy_status(FpsPolicy::SharedLowest),
            "Shared lowest in folder"
        );
        assert_eq!(fps_policy_status(FpsPolicy::Source), "Original");
        assert_eq!(fps_policy_status(FpsPolicy::Exact(23.976)), "23.976");
    }

    #[test]
    fn running_task_settings_keep_the_normal_layout_and_select_its_preset() {
        let app = ConverterApp {
            available_encoders: vec![Encoder::X265, Encoder::SvtAv1],
            ..ConverterApp::default()
        };
        let mut settings = default_settings(ContentMode::Animation, Encoder::SvtAv1);
        settings.fps = FpsPolicy::Exact(23.976);
        settings.quality = Some(31.0);
        settings.speed_preset = Some("10".to_owned());

        let snapshot = slint_settings_snapshot(&app, Some(&settings));

        assert_eq!(snapshot.mode_index, 1);
        assert_eq!(snapshot.encoder_labels[snapshot.encoder_index], "libsvtav1");
        assert!(snapshot.show_encoder);
        assert!(snapshot.show_fps);
        assert!(snapshot.show_explicit_fps);
        assert!((snapshot.quality - 31.0).abs() < f32::EPSILON);
        assert_eq!(snapshot.speed, "10");
        assert_eq!(snapshot.speed_labels[snapshot.speed_index], "10");
    }

    #[test]
    fn quality_guides_are_specific_to_each_software_codec() {
        let app = ConverterApp {
            available_encoders: vec![Encoder::X264, Encoder::X265, Encoder::SvtAv1],
            ..ConverterApp::default()
        };
        let x264 = slint_settings_snapshot(
            &app,
            Some(&default_settings(ContentMode::Tv, Encoder::X264)),
        );
        let x265 = slint_settings_snapshot(
            &app,
            Some(&default_settings(ContentMode::Tv, Encoder::X265)),
        );
        let svt_av1 = slint_settings_snapshot(
            &app,
            Some(&default_settings(ContentMode::Tv, Encoder::SvtAv1)),
        );

        assert!((x264.quality_maximum - 51.0).abs() < f32::EPSILON);
        assert!((x265.quality_maximum - 51.0).abs() < f32::EPSILON);
        assert!((svt_av1.quality_maximum - 63.0).abs() < f32::EPSILON);
        assert!((x264.quality_minimum - 0.0).abs() < f32::EPSILON);
        assert!((x265.quality_minimum - 0.0).abs() < f32::EPSILON);
        assert!((svt_av1.quality_minimum - 0.0).abs() < f32::EPSILON);
        assert!((x264.quality_step - 1.0).abs() < f32::EPSILON);
        assert!((x265.quality_step - 1.0).abs() < f32::EPSILON);
        assert!((svt_av1.quality_step - 1.0).abs() < f32::EPSILON);
        assert_eq!(x264.quality_level, "Balanced");
        assert_eq!(x265.quality_level, "Balanced");
        assert_eq!(svt_av1.quality_level, "Balanced");
        assert_eq!(x264.quality_guide_labels[2], "23\u{2013}27\nBalanced");
        assert_eq!(x265.quality_guide_labels[2], "26\u{2013}31\nBalanced");
        assert_eq!(svt_av1.quality_guide_labels[2], "28\u{2013}35\nBalanced");
        assert!((super::normalized_quality(Encoder::X264, 51.4) - 51.0).abs() < f32::EPSILON);
        assert!((super::normalized_quality(Encoder::X265, 21.3) - 21.0).abs() < f32::EPSILON);
        assert!((super::normalized_quality(Encoder::SvtAv1, 35.5) - 36.0).abs() < f32::EPSILON);
    }

    #[test]
    fn nvidia_preset_labels_explain_the_speed_and_quality_tradeoff() {
        let labels = super::speed_option_labels(Encoder::H264Nvenc);

        assert_eq!(labels[0], "P1 · Fastest");
        assert_eq!(labels[3], "P4 · Medium (default)");
        assert_eq!(labels[6], "P7 · Highest quality");
        assert_eq!(
            super::speed_help(Encoder::H264Nvenc),
            "P1 is fastest, P4 is medium, and P7 gives the highest quality."
        );
    }

    #[test]
    fn new_task_codec_quality_overrides_the_active_task_display() {
        let active_task = QueueTask::new(
            "active",
            "Active",
            vec!["active.mp4".into()],
            default_settings(ContentMode::Tv, Encoder::X265),
        );
        let mut queue = Queue::default();
        queue.add(active_task).unwrap();
        let app = ConverterApp {
            queue,
            selected_id: Some("active".to_owned()),
            mode: ContentMode::Tv,
            encoder: Encoder::SvtAv1,
            quality_crf: 35.0,
            available_encoders: vec![Encoder::X265, Encoder::SvtAv1],
            queue_run_state: QueueRunState::PausedBetweenFiles,
            ..ConverterApp::default()
        };

        let active_settings = displayed_task_settings(&app, true, false);
        let active_snapshot = slint_settings_snapshot(&app, active_settings);
        assert_eq!(
            active_snapshot.encoder_labels[active_snapshot.encoder_index],
            "x265"
        );

        let editing_settings = displayed_task_settings(&app, true, true);
        let editing_snapshot = slint_settings_snapshot(&app, editing_settings);
        assert_eq!(
            editing_snapshot.encoder_labels[editing_snapshot.encoder_index],
            "libsvtav1"
        );
        assert!((editing_snapshot.quality - 35.0).abs() < f32::EPSILON);
        assert_eq!(
            editing_snapshot.quality_guide_labels[2],
            "28\u{2013}35\nBalanced"
        );
    }

    #[test]
    fn quality_and_preset_memory_is_independent_per_mode_and_encoder() {
        let remembered = HashMap::from([
            (
                (ContentMode::Tv, Encoder::X265),
                EncodingUiSettings {
                    quality_crf: 21.0,
                    speed_preset: "slow".to_owned(),
                },
            ),
            (
                (ContentMode::Tv, Encoder::X264),
                EncodingUiSettings {
                    quality_crf: 19.0,
                    speed_preset: "fast".to_owned(),
                },
            ),
        ]);

        assert_eq!(
            encoding_ui_settings_for(&remembered, ContentMode::Tv, Encoder::X265),
            remembered[&(ContentMode::Tv, Encoder::X265)]
        );
        assert_eq!(
            encoding_ui_settings_for(&remembered, ContentMode::Tv, Encoder::X264),
            remembered[&(ContentMode::Tv, Encoder::X264)]
        );
        assert_eq!(
            encoding_ui_settings_for(&remembered, ContentMode::Animation, Encoder::SvtAv1),
            EncodingUiSettings::from(&default_settings(ContentMode::Animation, Encoder::SvtAv1))
        );
    }

    #[test]
    fn camera_existing_encoder_and_fps_rules_match_python() {
        assert_eq!(python_existing_library_name(Encoder::X265), "x265");
        assert_eq!(python_existing_library_name(Encoder::X264), "x264");
        assert_eq!(python_existing_library_name(Encoder::SvtAv1), "libsvtav1");
        for hardware in [
            Encoder::HevcNvenc,
            Encoder::Av1Nvenc,
            Encoder::H264Nvenc,
            Encoder::H264VideoToolbox,
            Encoder::HevcVideoToolbox,
            Encoder::Av1VideoToolbox,
        ] {
            assert_eq!(python_existing_library_name(hardware), "x265");
        }

        assert!(encoded_source_matches_fps(29.97, FpsPolicy::Source, None));
        assert!(encoded_source_matches_fps(
            23.976,
            FpsPolicy::Exact(23.976),
            None
        ));
        assert!(!encoded_source_matches_fps(
            29.97,
            FpsPolicy::Exact(23.976),
            None
        ));
        assert!(encoded_source_matches_fps(
            23.976,
            FpsPolicy::SharedLowest,
            Some(23.976)
        ));
    }

    #[test]
    fn active_camera_metrics_match_python_lut_display_rules() {
        let media = MediaInfo {
            path: "DJI_0001.MP4".into(),
            container_name: "mov,mp4".to_owned(),
            duration: Some(Duration::from_secs(1)),
            file_size: Some(1),
            bit_rate: None,
            width: Some(3840),
            height: Some(2160),
            frame_rate: Some(59.94),
            streams: Vec::new(),
            metadata: std::collections::BTreeMap::from([(
                "encoder".to_owned(),
                "DJI OsmoAction6".to_owned(),
            )]),
        };

        let tv =
            camera_run_info_from_media(&default_settings(ContentMode::Tv, Encoder::X265), &media);
        assert_eq!(tv.model_name.as_deref(), Some("DJI OsmoAction6"));
        assert_eq!(tv.lut_status, None);

        let mut camera = default_settings(ContentMode::CameraVideos, Encoder::X265);
        let enabled = camera_run_info_from_media(&camera, &media);
        assert_eq!(enabled.model_name.as_deref(), Some("DJI OsmoAction6"));
        assert_eq!(enabled.lut_status.as_deref(), Some("action6.cube"));

        camera.apply_lut = false;
        assert_eq!(
            camera_run_info_from_media(&camera, &media)
                .lut_status
                .as_deref(),
            Some("Disabled")
        );

        let hardware = default_settings(ContentMode::CameraVideos, Encoder::HevcNvenc);
        assert_eq!(
            camera_run_info_from_media(&hardware, &media)
                .lut_status
                .as_deref(),
            Some("Disabled")
        );

        let mut unsupported = media;
        unsupported.metadata.clear();
        assert_eq!(
            camera_run_info_from_media(&camera, &unsupported),
            super::CameraRunInfo::default()
        );
    }

    #[test]
    fn completed_history_records_only_the_lut_actually_applied() {
        let task = QueueTask::new(
            "camera",
            "Camera",
            vec!["DJI_0001.MP4".into()],
            default_settings(ContentMode::CameraVideos, Encoder::X265),
        );
        let mut job = CompletedJob {
            input: "DJI_0001.MP4".into(),
            output: "DJI_0001.mp4".into(),
            lut_name: None,
            start_time: "start".to_owned(),
            end_time: "end".to_owned(),
            process_minutes: "1.00".to_owned(),
            original: HistoryMediaInfo {
                fps: Some(29.97),
                codec: Some("h264".to_owned()),
                duration: Some(Duration::from_secs(125)),
                ..HistoryMediaInfo::default()
            },
            converted: HistoryMediaInfo {
                fps: Some(23.976),
                codec: Some("hevc".to_owned()),
                ..HistoryMediaInfo::default()
            },
        };

        assert_eq!(completed_history_row(&task, &job).columns()[2], "-");
        let configuration = completed_history_row(&task, &job);
        assert_eq!(&configuration.columns()[9..=10], ["29.97", "23.976"]);
        assert_eq!(configuration.columns()[11], "x265");
        assert_ne!(configuration.columns()[12], "-");
        assert_ne!(configuration.columns()[13], "-");
        assert_eq!(configuration.task_id(), Some("camera"));
        assert_eq!(configuration.input_path(), Some("DJI_0001.MP4"));
        assert_eq!(configuration.original_codec(), Some("h264"));
        assert_eq!(configuration.converted_codec(), Some("hevc"));
        assert_eq!(configuration.duration(), Some("02:05"));
        job.lut_name = Some("DJI_Osmo_Action_6_D-Log_M_to_Rec.709.cube".to_owned());
        assert_eq!(
            completed_history_row(&task, &job).columns()[2],
            "DJI_Osmo_Action_6_D-Log_M_to_Rec.709.cube"
        );

        let trim = QueueTask::new(
            "trim",
            "Trim",
            vec!["clip.mp4".into()],
            default_settings(ContentMode::Trim, Encoder::X265),
        );
        let trim_configuration = completed_history_row(&trim, &job);
        assert_eq!(
            &trim_configuration.columns()[11..14],
            ["Stream copy", "-", "-"]
        );

        let completed_files = selected_task_files(&task, &[configuration], None);
        assert_eq!(completed_files.len(), 1);
        assert_eq!(completed_files[0].path, "DJI_0001.mp4");
        assert_eq!(completed_files[0].status, "Completed");
        assert_eq!(completed_files[0].conversion_time, "1.00 min");
        assert_eq!(completed_files[0].codec, "h264 → hevc");
        assert_eq!(completed_files[0].duration, "02:05");

        let queued = QueueTask::new(
            "queued",
            "Queued",
            vec!["waiting.mkv".into()],
            default_settings(ContentMode::Tv, Encoder::X265),
        );
        let queued_files = selected_task_files(&queued, &[], None);
        assert_eq!(queued_files.len(), 1);
        assert_eq!(queued_files[0].status, "Queued");
        assert_eq!(queued_files[0].started_time, "-");
        assert_eq!(queued_files[0].original_size, "-");
    }

    #[test]
    fn completed_task_details_replace_the_source_row_with_the_output_row() {
        let root = temporary_test_directory("completed-task-output-row");
        let input = root.join("episode.mkv");
        let output = root.join("episode.mp4");
        let original_directory = root.join("original");
        std::fs::create_dir(&original_directory).unwrap();
        std::fs::write(original_directory.join("episode.mkv"), b"source").unwrap();
        std::fs::write(&output, b"converted").unwrap();

        let task = QueueTask::new(
            "tv-folder",
            "TV folder",
            vec![root.clone()],
            default_settings(ContentMode::Tv, Encoder::X265),
        );
        let history = completed_history_row(
            &task,
            &CompletedJob {
                input,
                output: output.clone(),
                lut_name: None,
                start_time: "start".to_owned(),
                end_time: "end".to_owned(),
                process_minutes: "1.00".to_owned(),
                original: HistoryMediaInfo::default(),
                converted: HistoryMediaInfo::default(),
            },
        );

        let files = selected_task_files(&task, &[history], None);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, output.display().to_string());
        assert_eq!(files[0].title, "episode.mp4");
        assert_eq!(files[0].status, "Completed");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_completed_history_is_included_in_matching_folder_task_details() {
        let root = temporary_test_directory("legacy-completed-task-details");
        let series = root.join("series");
        let completed_path = series.join("episode-01.mkv");
        let pending_path = series.join("episode-02.mp4");
        let converted_directory = root.join("archive (x265)");
        let converted_path = converted_directory.join("episode-03.mkv");
        std::fs::create_dir_all(series.join("original")).unwrap();
        std::fs::create_dir_all(&converted_directory).unwrap();
        std::fs::write(&completed_path, b"converted").unwrap();
        std::fs::write(series.join("original").join("episode-01.mp4"), b"source").unwrap();
        std::fs::write(&pending_path, b"pending").unwrap();
        std::fs::write(&converted_path, b"converted directory").unwrap();
        let mut columns = std::array::from_fn(|_| "-".to_owned());
        columns[0] = completed_path.display().to_string();
        columns[1] = "TV".to_owned();
        columns[3] = "2026-08-25 22:57:17".to_owned();
        columns[4] = "2026-08-25 23:53:36".to_owned();
        columns[5] = "56.31".to_owned();
        columns[6] = "2025.38 MB".to_owned();
        columns[7] = "693.65 MB".to_owned();
        columns[9] = "25".to_owned();
        columns[10] = "25".to_owned();
        columns[11] = "x265".to_owned();
        columns[12] = "28".to_owned();
        columns[13] = "medium".to_owned();
        let legacy_row = CompletedHistoryRow::new(columns);
        let mut task = QueueTask::new(
            "folder-task",
            "TV folder",
            vec![root.clone()],
            default_settings(ContentMode::Tv, Encoder::X265),
        );
        task.source_root = Some(root.clone());
        let watch = WatchedFolder {
            root: root.clone(),
            settings: task.settings.clone(),
            snapshot: Vec::new(),
        };
        let mut queue = Queue::default();
        queue.add(task.clone()).unwrap();
        let summary = folder_queue_summary(&watch, &queue);

        let files = selected_task_files(&task, &[legacy_row], None);

        assert_eq!(files.len(), summary.files);
        assert_eq!(files.len(), 3);
        let completed = files
            .iter()
            .find(|file| file.path == completed_path.display().to_string())
            .unwrap();
        assert_eq!(completed.status, "Completed");
        assert_eq!(completed.conversion_time, "56.31 min");
        assert_eq!(completed.original_size, "2025.38 MB");
        assert_eq!(
            files
                .iter()
                .find(|file| file.path == pending_path.display().to_string())
                .unwrap()
                .status,
            "Queued"
        );
        assert_eq!(
            files
                .iter()
                .find(|file| file.path == converted_path.display().to_string())
                .unwrap()
                .status,
            "Completed"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn task_details_default_to_natural_full_path_order() {
        let season_ten_episode_one = PathBuf::from("season-10").join("episode-1.mp4");
        let season_two_episode_ten = PathBuf::from("season-2").join("episode-10.mp4");
        let season_two_episode_two = PathBuf::from("season-2").join("episode-2.mp4");
        let task = QueueTask::new(
            "natural-order",
            "Natural order",
            vec![
                season_ten_episode_one.clone(),
                season_two_episode_ten.clone(),
                season_two_episode_two.clone(),
            ],
            default_settings(ContentMode::Tv, Encoder::X265),
        );

        let files = selected_task_files(&task, &[], None);
        let paths = files
            .iter()
            .map(|file| PathBuf::from(&file.path))
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            [
                season_two_episode_two,
                season_two_episode_ten,
                season_ten_episode_one,
            ]
        );
    }

    #[test]
    fn failed_task_detail_file_exposes_its_conversion_error() {
        let failed_path = PathBuf::from("season").join("episode-02.mkv");
        let task = QueueTask::new(
            "failed-details",
            "Failed details",
            vec![failed_path.clone()],
            default_settings(ContentMode::Tv, Encoder::X265),
        );
        let failures = HashMap::from([(
            failed_path.clone(),
            "muxer rejected the copied audio channel layout".to_owned(),
        )]);

        let files = selected_task_files(&task, &[], Some(&failures));

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, failed_path.display().to_string());
        assert_eq!(files[0].status, "Failed");
        assert_eq!(
            files[0].error_detail,
            "muxer rejected the copied audio channel layout"
        );
    }

    #[test]
    fn prepared_requests_discard_stale_lut_paths_before_detection() {
        let mut settings = default_settings(ContentMode::Tv, Encoder::X265);
        settings.apply_lut = true;
        settings.camera_lut_path = Some("stale.cube".into());
        let request = videoferry_core::ConversionRequest {
            input: "episode.mkv".into(),
            output: "episode-output.mkv".into(),
            settings,
        };

        let prepared = prepare_request(&request, &mut |_| {}).unwrap();

        assert!(prepared.settings.camera_lut_path.is_none());
    }

    #[test]
    fn progress_size_estimates_match_python_metrics() {
        let progress = ConversionProgress {
            overall: None,
            completed: Duration::from_secs(30),
            total: Some(Duration::from_secs(120)),
            frames: None,
            total_frames: None,
            target_fps: None,
            frames_per_second: None,
            speed: Some(2.0),
            output_bytes: Some(10 * 1_048_576),
        };

        assert_eq!(estimated_output_bytes(&progress), Some(40 * 1_048_576));
        assert_eq!(output_bytes_per_minute(&progress), Some(20 * 1_048_576));
        assert_eq!(
            format_progress_time(&progress, Some(ContentMode::Tv)),
            "00:00:30.00/02:00"
        );
        assert_eq!(
            format_progress_time(&progress, Some(ContentMode::PhotoSlideshow)),
            "00:30/02:00"
        );

        let unknown_total = ConversionProgress {
            total: None,
            ..progress.clone()
        };
        assert_eq!(
            format_progress_time(&unknown_total, Some(ContentMode::CameraVideos)),
            "00:00:30.00/-"
        );
        assert_eq!(format_clock(Duration::from_secs(3_661)), "1:01:01");
        assert_eq!(
            format_ffmpeg_progress_clock(Duration::from_millis(3_661_999)),
            "01:01:01.99"
        );

        let frame_progress = ConversionProgress {
            frames: Some(120),
            ..progress
        };
        let source = default_settings(ContentMode::CameraVideos, Encoder::X265);
        assert_eq!(
            format_frame_progress(&frame_progress, Some(&source), Some(30.0)),
            "120/3600"
        );

        let mut explicit = source.clone();
        explicit.fps = FpsPolicy::Exact(23.976);
        assert_eq!(
            format_frame_progress(&frame_progress, Some(&explicit), Some(30.0)),
            "120/2877"
        );

        let slideshow = default_settings(ContentMode::PhotoSlideshow, Encoder::X265);
        assert_eq!(
            format_frame_progress(&frame_progress, Some(&slideshow), None),
            "120/3600"
        );

        let shared = default_settings(ContentMode::Tv, Encoder::X265);
        assert_eq!(
            format_frame_progress(&frame_progress, Some(&shared), Some(30.0)),
            "120/?"
        );

        let resolved_shared = ConversionProgress {
            total_frames: Some(2_997),
            target_fps: Some(24.975),
            ..frame_progress.clone()
        };
        assert_eq!(
            format_frame_progress(&resolved_shared, Some(&shared), Some(30.0)),
            "120/2997"
        );
        assert_eq!(
            target_fps_status(Some(&resolved_shared), FpsPolicy::SharedLowest),
            "24.975"
        );
        assert_eq!(
            target_fps_status(None, FpsPolicy::SharedLowest),
            "Shared lowest in folder"
        );
        assert_eq!(
            target_fps_status_with_source(None, FpsPolicy::Source, Some(29.97)),
            "29.97"
        );
        assert_eq!(
            target_fps_status_with_source(None, FpsPolicy::Exact(23.976), Some(29.97)),
            "23.976"
        );

        let no_frames = ConversionProgress {
            frames: None,
            ..frame_progress.clone()
        };
        assert_eq!(
            format_frame_progress(&no_frames, Some(&source), Some(30.0)),
            "-"
        );

        let unknown_duration = ConversionProgress {
            total: None,
            ..frame_progress
        };
        assert_eq!(
            format_frame_progress(&unknown_duration, Some(&source), Some(30.0)),
            "120/?"
        );
    }

    #[test]
    fn carried_stream_status_reports_tracks_and_audio_channels() {
        let info = StreamCarryInfo {
            audio_tracks: 2,
            audio_channels: Some(8),
            subtitle_tracks: 1,
        };

        assert_eq!(format_carried_audio(Some(&info)), "2 tracks, 8 ch");
        assert_eq!(format_carried_subtitles(Some(&info)), "1 track");
        assert_eq!(format_carried_audio(None), "-");
        assert_eq!(format_carried_subtitles(None), "-");

        let unknown_channels = StreamCarryInfo {
            audio_tracks: 1,
            audio_channels: None,
            ..StreamCarryInfo::default()
        };
        assert_eq!(
            format_carried_audio(Some(&unknown_channels)),
            "1 track, ? ch"
        );
        assert_eq!(
            format_carried_audio(Some(&StreamCarryInfo::default())),
            "0 tracks"
        );
    }

    #[test]
    fn progress_fraction_and_remaining_match_python_pause_aware_metrics() {
        let quarter_by_frames = ConversionProgress {
            overall: None,
            completed: Duration::from_secs(50),
            total: Some(Duration::from_secs(100)),
            frames: Some(25),
            total_frames: Some(100),
            target_fps: Some(25.0),
            frames_per_second: Some(10.0),
            speed: Some(0.4),
            output_bytes: None,
        };
        assert_eq!(
            progress_fraction(&quarter_by_frames, Some(ContentMode::Tv)),
            Some(0.25)
        );
        assert_eq!(
            progress_fraction(&quarter_by_frames, Some(ContentMode::Stabilize)),
            Some(0.5)
        );
        let second_stabilization_pass = ConversionProgress {
            overall: Some(ProgressRatio {
                completed: 125,
                total: 200,
            }),
            completed: Duration::from_secs(25),
            ..quarter_by_frames.clone()
        };
        assert_eq!(
            progress_fraction(&second_stabilization_pass, Some(ContentMode::Stabilize)),
            Some(0.625)
        );
        assert_eq!(
            format_progress_time(&second_stabilization_pass, Some(ContentMode::Stabilize)),
            "00:00:25.00/01:40"
        );
        assert_eq!(
            estimated_remaining(Duration::from_secs(30), Some(0.25)),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            estimated_remaining(Duration::from_secs(30), Some(0.0)),
            None
        );
        assert_eq!(
            estimated_remaining(Duration::from_secs(30), Some(1.0)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn task_progress_separates_previous_current_run_and_unfinished_work() {
        let counts = TaskDisplayCounts {
            files: 10,
            converted: 4,
            remaining: 6,
            ..TaskDisplayCounts::default()
        };

        let active = task_progress_segments(&counts, Some(2), Some(0.5));
        assert!((active.completed_before - 0.2).abs() < f32::EPSILON);
        assert!((active.completed_this_run - 0.2).abs() < f32::EPSILON);
        assert!((active.current_item - 0.05).abs() < f32::EPSILON);

        let not_started_here = task_progress_segments(&counts, None, None);
        assert!((not_started_here.completed_before - 0.4).abs() < f32::EPSILON);
        assert!(not_started_here.completed_this_run.abs() < f32::EPSILON);
        assert!(not_started_here.current_item.abs() < f32::EPSILON);

        let completed = task_progress_segments(
            &TaskDisplayCounts {
                files: 10,
                converted: 10,
                ..TaskDisplayCounts::default()
            },
            Some(2),
            Some(1.0),
        );
        assert!((completed.completed_before - 0.2).abs() < f32::EPSILON);
        assert!((completed.completed_this_run - 0.8).abs() < f32::EPSILON);
        assert!(completed.current_item.abs() < f32::EPSILON);
    }

    #[test]
    fn aggregate_failure_summary_survives_while_the_next_file_runs() {
        let mut failures = HashMap::new();
        failures.insert(PathBuf::from("episode-01.mp4"), "decode failed".to_owned());
        assert_eq!(
            task_run_failure_summary(Some(&failures)).as_deref(),
            Some("1 file failed in this run; conversion is continuing")
        );

        failures.insert(PathBuf::from("episode-02.mp4"), "input locked".to_owned());
        assert_eq!(
            task_run_failure_summary(Some(&failures)).as_deref(),
            Some("2 files failed in this run; conversion is continuing")
        );
        assert_eq!(task_run_failure_summary(None), None);
    }

    #[test]
    fn active_file_position_matches_python_video_and_slideshow_indexing() {
        let video = QueueTask::new(
            "aggregate",
            "Season",
            vec!["one.mkv".into(), "two.mkv".into(), "three.mkv".into()],
            default_settings(ContentMode::Tv, Encoder::X265),
        );
        let counts = TaskDisplayCounts {
            files: 4,
            converted: 1,
            failed: 1,
            remaining: 2,
            ..TaskDisplayCounts::default()
        };
        assert_eq!(active_item_position(&video, &counts, None), Some((3, 4)));

        let mut slideshow_settings = default_settings(ContentMode::PhotoSlideshow, Encoder::X265);
        slideshow_settings.slideshow_image_paths = (1..=10)
            .map(|index| format!("photo-{index}.jpg").into())
            .collect();
        let slideshow = QueueTask::new(
            "slideshow",
            "Photos",
            vec!["photos".into()],
            slideshow_settings,
        );
        assert_eq!(
            active_item_position(&slideshow, &TaskDisplayCounts::default(), None),
            Some((1, 10))
        );
        let halfway = ConversionProgress {
            overall: None,
            completed: Duration::from_secs(50),
            total: Some(Duration::from_secs(100)),
            frames: None,
            total_frames: None,
            target_fps: None,
            frames_per_second: None,
            speed: None,
            output_bytes: None,
        };
        assert_eq!(
            active_item_position(&slideshow, &TaskDisplayCounts::default(), Some(&halfway)),
            Some((6, 10))
        );
        let complete = ConversionProgress {
            completed: Duration::from_secs(100),
            ..halfway
        };
        assert_eq!(
            active_item_position(&slideshow, &TaskDisplayCounts::default(), Some(&complete)),
            Some((10, 10))
        );
    }

    #[test]
    fn completed_history_numbers_use_python_rounding_and_general_format() {
        assert_eq!(format_size_mb(Some(1_043_334)), "1.00 MB");
        assert_eq!(format_size_mb(None), "-");
        for (value, expected) in [
            (24_000.0 / 1_001.0, "23.976"),
            (30_000.0 / 1_001.0, "29.97"),
            (60_000.0 / 1_001.0, "59.9401"),
            (120.0, "120"),
            (0.00001, "1e-05"),
        ] {
            assert_eq!(format_fps(Some(value)), expected);
        }
        assert_eq!(format_fps(None), "-");
    }

    #[test]
    fn sleep_prevention_matches_python_running_pause_and_idle_rules() {
        assert!(should_inhibit_sleep(true, true, false));
        assert!(!should_inhibit_sleep(false, true, false));
        assert!(!should_inhibit_sleep(true, false, false));
        assert!(!should_inhibit_sleep(true, true, true));
    }

    #[test]
    fn scheduled_pause_becomes_a_one_shot_pause_between_files() {
        let mut controller = SlintController::new();
        controller.app.queue_run_state = QueueRunState::PausedBetweenFiles;

        let snapshot = controller.snapshot();

        assert!(snapshot.is_running);
        assert!(snapshot.is_paused);
        assert!(snapshot.paused_between_files);
        assert!(!snapshot.pause_after_current);
        assert_eq!(snapshot.active_title, "Queue paused");
    }

    #[test]
    fn original_backup_marks_a_source_as_completed() {
        let root = temporary_test_directory("completed-source");
        let source = root.join("episode.mkv");
        let backup = root.join("original").join("episode.mkv");
        std::fs::create_dir(root.join("original")).unwrap();
        std::fs::write(&source, b"converted").unwrap();
        std::fs::write(&backup, b"source").unwrap();

        assert!(should_skip_queue_source(
            &source,
            &default_settings(ContentMode::Tv, Encoder::X265)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_existing_target_does_not_hide_source_work() {
        let root = temporary_test_directory("existing-target-work");
        let source = root.join("episode.mov");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(root.join("episode.mkv"), b"previous output").unwrap();

        assert!(!should_skip_queue_source(
            &source,
            &default_settings(ContentMode::Tv, Encoder::X265)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recursive_collection_excludes_python_completed_folders() {
        let root = temporary_test_directory("completed-folder");
        let pending = root.join("pending");
        let completed = root.join("show (x265)");
        let chs = root.join("chs");
        std::fs::create_dir(&pending).unwrap();
        std::fs::create_dir(&completed).unwrap();
        std::fs::create_dir(&chs).unwrap();
        std::fs::write(pending.join("new.mkv"), b"new").unwrap();
        std::fs::write(completed.join("old.mkv"), b"old").unwrap();
        std::fs::write(chs.join("translated.mkv"), b"skip").unwrap();

        let files = collect_video_files(&root);

        assert_eq!(files, [pending.join("new.mkv")]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_tv_folders_receive_the_python_encoder_suffix() {
        let root = temporary_test_directory("completed-folder-rename");
        let show = root.join("Show");
        let original = show.join("original");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::write(show.join("episode.mkv"), b"converted").unwrap();
        std::fs::write(original.join("episode.mp4"), b"source").unwrap();
        let task = QueueTask::new(
            "show",
            "TV - Show",
            vec![show.join("episode.mp4")],
            default_settings(ContentMode::Tv, Encoder::X265),
        );

        let messages = rename_completed_task_directories(&task);

        let renamed = root.join("Show (x265)");
        assert!(renamed.is_dir());
        assert!(!show.exists());
        assert_eq!(messages.len(), 1);
        assert!(is_skipped_conversion_directory(&renamed));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn running_aggregate_task_renames_each_completed_subfolder() {
        let temporary = temporary_test_directory("running-aggregate-folder-rename");
        let root = temporary.join("Shows");
        let completed = root.join("Completed Show");
        let pending = root.join("Pending Show");
        std::fs::create_dir_all(completed.join("original")).unwrap();
        std::fs::create_dir_all(&pending).unwrap();
        std::fs::write(completed.join("episode.mkv"), b"converted").unwrap();
        std::fs::write(completed.join("original/episode.mp4"), b"source").unwrap();
        std::fs::write(pending.join("episode.mp4"), b"source").unwrap();
        let mut task = QueueTask::new(
            "shows",
            "TV - Shows",
            vec![root.clone()],
            default_settings(ContentMode::Tv, Encoder::X265),
        );
        task.status = QueueStatus::Running;

        let messages = rename_completed_task_directories(&task);

        assert!(root.join("Completed Show (x265)").is_dir());
        assert!(!completed.exists());
        assert!(root.is_dir());
        assert!(pending.is_dir());
        assert_eq!(messages.len(), 1);
        assert_eq!(task.status, QueueStatus::Running);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn completed_folder_rename_respects_zero_prefix_and_incomplete_backups() {
        let root = temporary_test_directory("folder-rename-exclusions");
        for name in ["0_Keep", "Incomplete"] {
            let directory = root.join(name);
            std::fs::create_dir_all(directory.join("original")).unwrap();
            std::fs::write(directory.join("episode.mkv"), b"video").unwrap();
        }
        std::fs::write(root.join("0_Keep/original/episode.mp4"), b"source").unwrap();
        let zero_task = QueueTask::new(
            "zero",
            "TV - zero",
            vec![root.join("0_Keep")],
            default_settings(ContentMode::Tv, Encoder::X265),
        );
        let incomplete_task = QueueTask::new(
            "incomplete",
            "TV - incomplete",
            vec![root.join("Incomplete")],
            default_settings(ContentMode::Tv, Encoder::X265),
        );

        assert!(rename_completed_task_directories(&zero_task).is_empty());
        assert!(rename_completed_task_directories(&incomplete_task).is_empty());
        assert!(root.join("0_Keep").is_dir());
        assert!(root.join("Incomplete").is_dir());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn video_folder_scheduler_uses_python_style_natural_order() {
        let root = temporary_test_directory("natural-video-order");
        let episode_ten = root.join("episode10.mkv");
        let episode_two = root.join("episode2.mkv");
        std::fs::write(&episode_ten, b"ten").unwrap();
        std::fs::write(&episode_two, b"two").unwrap();

        assert_eq!(collect_video_files(&root), [episode_two, episode_ten]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn video_folder_scheduler_naturally_sorts_the_entire_path() {
        let root = temporary_test_directory("natural-full-path-video-order");
        let season_ten = root.join("season10");
        let season_two = root.join("season2");
        std::fs::create_dir_all(&season_ten).unwrap();
        std::fs::create_dir_all(&season_two).unwrap();
        let season_ten_episode_one = season_ten.join("episode1.mkv");
        let season_two_episode_ten = season_two.join("episode10.mkv");
        let season_two_episode_two = season_two.join("episode2.mkv");
        std::fs::write(&season_ten_episode_one, b"one").unwrap();
        std::fs::write(&season_two_episode_ten, b"ten").unwrap();
        std::fs::write(&season_two_episode_two, b"two").unwrap();

        assert_eq!(
            collect_video_files(&root),
            [
                season_two_episode_two,
                season_two_episode_ten,
                season_ten_episode_one,
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn video_filter_ignores_macos_appledouble_files_like_python() {
        let root = temporary_test_directory("appledouble");
        let resource_fork = root.join("._episode.mp4");
        let video = root.join("episode.mp4");
        std::fs::write(&resource_fork, b"metadata").unwrap();
        std::fs::write(&video, b"video").unwrap();

        assert!(!is_video_path(&resource_fork));
        assert!(is_video_path(&video));
        assert_eq!(collect_video_files(&root), [video]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn expands_every_file_in_a_python_multi_target_task() {
        let root = temporary_test_directory("multi-target");
        let first = root.join("episode1.mkv");
        let second = root.join("episode2.mp4");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let task = QueueTask::new(
            "task-1",
            "Season",
            vec![second.clone(), first.clone(), first.clone()],
            default_settings(ContentMode::Tv, Encoder::X265),
        );

        let expanded = expanded_task_targets(&task, &HashSet::new());

        assert_eq!(
            expanded,
            vec![(first, None), (second, None)],
            "targets are naturally decomposed into deterministic per-file work"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_task_keeps_python_style_name_and_skips_failed_inputs_within_the_run() {
        let root = temporary_test_directory("aggregate-folder-inputs");
        let first = root.join("episode1.mkv");
        let second = root.join("episode2.mkv");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let settings = default_settings(ContentMode::Tv, Encoder::X265);
        let task = QueueTask::new(
            "season",
            default_task_name(&settings, &root),
            vec![root.clone()],
            settings,
        );
        let failures = HashMap::from([(first, "broken stream".to_owned())]);

        assert!(task.name.starts_with("TV - "));
        assert_eq!(remaining_task_inputs(&task, Some(&failures)), [second]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_targets_are_owned_across_tasks() {
        let first = std::path::PathBuf::from("season/episode1.mkv");
        let second = std::path::PathBuf::from("season/episode2.mkv");
        let mut queue = Queue::default();
        queue
            .add(QueueTask::new(
                "season",
                "Season",
                vec![first.clone(), second.clone()],
                default_settings(ContentMode::Tv, Encoder::X265),
            ))
            .unwrap();

        let occupied = queued_target_set(&queue);

        assert_eq!(occupied, HashSet::from([first, second]));
    }

    #[test]
    fn legacy_watched_roots_remain_owned_during_new_task_admission() {
        let target = std::path::PathBuf::from("season/episode1.mkv");
        let watched_root = std::path::PathBuf::from("season");
        let settings = default_settings(ContentMode::Tv, Encoder::X265);
        let mut queue = Queue::default();
        queue
            .add(QueueTask::new(
                "legacy-file",
                "Legacy file",
                vec![target.clone()],
                settings.clone(),
            ))
            .unwrap();
        let watches = [WatchedFolder {
            root: watched_root.clone(),
            settings,
            snapshot: Vec::new(),
        }];

        let occupied = queue_admission_occupied_paths(&queue, &watches);

        assert_eq!(occupied, HashSet::from([target, watched_root]));
    }

    #[test]
    fn extended_queue_selection_supports_toggle_and_ranges() {
        let tasks = ["a", "b", "c", "d"].map(str::to_owned);
        let mut primary = None;
        let mut selected = HashSet::new();
        let mut anchor = None;

        update_extended_selection(
            &tasks,
            "b".to_owned(),
            egui::Modifiers::NONE,
            &mut primary,
            &mut selected,
            &mut anchor,
        );
        update_extended_selection(
            &tasks,
            "d".to_owned(),
            egui::Modifiers {
                command: true,
                ..egui::Modifiers::NONE
            },
            &mut primary,
            &mut selected,
            &mut anchor,
        );
        assert_eq!(selected, HashSet::from(["b".to_owned(), "d".to_owned()]));

        update_extended_selection(
            &tasks,
            "a".to_owned(),
            egui::Modifiers::SHIFT,
            &mut primary,
            &mut selected,
            &mut anchor,
        );
        assert_eq!(selected, tasks.iter().cloned().collect());
        assert_eq!(primary.as_deref(), Some("a"));
    }

    #[test]
    fn trim_admission_keeps_only_the_first_video_file() {
        let root = temporary_test_directory("trim-admission");
        let first = root.join("first.mkv");
        let second = root.join("second.mp4");
        let folder = root.join("folder");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        std::fs::create_dir(&folder).unwrap();

        let (selected, skipped) = select_trim_source(&[folder, first.clone(), second]);

        assert_eq!(selected, Some(first));
        assert_eq!(skipped, 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn new_task_target_collection_keeps_batch_selection_in_one_ordered_task() {
        let root = temporary_test_directory("new-task-targets");
        let first = root.join("episode-1.mkv");
        let second = root.join("episode-2.mp4");
        let unsupported = root.join("notes.txt");
        let folder = root.join("season");
        for path in [&first, &second, &unsupported] {
            std::fs::write(path, b"fixture").unwrap();
        }
        std::fs::create_dir(&folder).unwrap();
        let settings = default_settings(ContentMode::Tv, Encoder::X265);
        let occupied = HashSet::from([second.clone()]);

        let (targets, skipped) = collect_new_video_task_targets(
            vec![
                first.clone(),
                unsupported,
                second,
                folder.clone(),
                first.clone(),
            ],
            &settings,
            &occupied,
        );

        assert_eq!(targets, [first, folder]);
        assert_eq!(skipped, 3);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workflow_setting_controls_match_the_python_dialog() {
        assert!(!workflow_shows_encoder(ContentMode::Trim));
        assert!(!workflow_shows_fps(ContentMode::Trim));
        assert!(!workflow_shows_fps(ContentMode::Stabilize));
        assert!(!workflow_shows_fps(ContentMode::PhotoSlideshow));

        for mode in [
            ContentMode::Tv,
            ContentMode::Animation,
            ContentMode::CameraVideos,
        ] {
            assert!(workflow_shows_encoder(mode));
            assert!(workflow_shows_fps(mode));
        }
        assert!(workflow_shows_encoder(ContentMode::Stabilize));
        assert!(workflow_shows_encoder(ContentMode::PhotoSlideshow));
    }

    #[test]
    fn trim_time_entry_matches_python_formats_and_validation() {
        assert_eq!(parse_trim_time("00:00"), Ok(Duration::ZERO));
        assert_eq!(parse_trim_time("01:05"), Ok(Duration::from_secs(65)));
        assert_eq!(parse_trim_time("01:02:03"), Ok(Duration::from_secs(3723)));
        assert_eq!(format_trim_time(Duration::from_secs(65)), "01:05");
        assert_eq!(format_trim_time(Duration::from_secs(3723)), "01:02:03");

        for invalid in ["", "1:05", "01:5", "60:00", "00:60", "1", "00:00:00:00"] {
            assert!(parse_trim_time(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn ordinary_video_failures_finish_the_task_but_slideshow_or_engine_loss_stops_queue() {
        assert!(worker_error_is_queue_fatal(
            ContentMode::Tv,
            &EngineError::Unavailable("missing libraries".to_owned())
        ));
        assert!(worker_error_is_queue_fatal(
            ContentMode::PhotoSlideshow,
            &EngineError::Failed("encode error".to_owned())
        ));
        for error in [
            EngineError::Unsupported("codec".to_owned()),
            EngineError::InvalidMedia("broken input".to_owned()),
            EngineError::Failed("encode error".to_owned()),
            EngineError::Cancelled,
        ] {
            assert!(!worker_error_is_queue_fatal(ContentMode::Tv, &error));
        }

        for error in [
            EngineError::Unsupported("codec".to_owned()),
            EngineError::InvalidMedia("broken input".to_owned()),
            EngineError::Failed("encode error".to_owned()),
        ] {
            assert!(worker_error_should_retry(ContentMode::Tv, &error));
            assert!(!worker_error_should_retry(
                ContentMode::PhotoSlideshow,
                &error
            ));
        }
        assert!(!worker_error_should_retry(
            ContentMode::Tv,
            &EngineError::Unavailable("missing libraries".to_owned())
        ));
        let control = ConversionControl::new();
        control.stop_current();
        assert_eq!(
            wait_for_retry(Duration::from_secs(1), &control),
            Err(EngineError::Cancelled)
        );

        let control = ConversionControl::new();
        let mut attempts = 0;
        let mut events = Vec::new();
        let result = run_with_retry(
            ContentMode::Tv,
            &control,
            Duration::ZERO,
            &mut |event| events.push(event),
            &mut |_emit: &mut dyn FnMut(ConversionEvent)| {
                attempts += 1;
                if attempts == 1 {
                    Err(EngineError::Failed("first attempt".to_owned()))
                } else {
                    Ok("converted")
                }
            },
        );
        assert_eq!(result, Ok("converted"));
        assert_eq!(attempts, 2);
        assert!(matches!(events.as_slice(), [ConversionEvent::Warning(_)]));

        attempts = 0;
        let result = run_with_retry(
            ContentMode::PhotoSlideshow,
            &control,
            Duration::ZERO,
            &mut |_| {},
            &mut |_emit: &mut dyn FnMut(ConversionEvent)| {
                attempts += 1;
                Err::<(), _>(EngineError::Failed("slideshow".to_owned()))
            },
        );
        assert!(result.is_err());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn sharing_violations_wait_without_consuming_the_ordinary_retry() {
        for message in [
            "[WinError 32] The process cannot access the file",
            "The file is being used by another process",
            "sharing violation while opening input",
        ] {
            assert!(worker_error_is_locked_input(&EngineError::Failed(
                message.to_owned()
            )));
        }
        assert!(!worker_error_is_locked_input(&EngineError::Failed(
            "Permission denied".to_owned()
        )));

        let control = ConversionControl::new();
        let mut attempts = 0;
        let result = run_with_retry(
            ContentMode::Tv,
            &control,
            Duration::ZERO,
            &mut |_| {},
            &mut |_emit: &mut dyn FnMut(ConversionEvent)| {
                attempts += 1;
                if attempts < 4 {
                    Err(EngineError::Failed(
                        "[WinError 32] source is being used by another process".to_owned(),
                    ))
                } else {
                    Ok("unlocked")
                }
            },
        );
        assert_eq!(result, Ok("unlocked"));
        assert_eq!(attempts, 4);
    }

    #[test]
    fn photo_viewer_wheel_zoom_matches_buttons_and_limits() {
        for (actual, expected) in [
            (photo_zoom_after_wheel(1.0, 1.0), 1.25),
            (photo_zoom_after_wheel(1.0, -1.0), 0.8),
            (photo_zoom_after_wheel(8.0, 1.0), 8.0),
            (photo_zoom_after_wheel(0.05, -1.0), 0.05),
        ] {
            assert!((actual - expected).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn task_setting_edits_validate_targets_for_the_new_workflow() {
        let root = temporary_test_directory("task-setting-validation");
        let video = root.join("episode.mkv");
        let photo_one = root.join("photo1.jpg");
        let photo_two = root.join("photo2.jpg");
        let empty_album = root.join("empty-album");
        std::fs::write(&video, b"video").unwrap();
        std::fs::write(&photo_one, b"one").unwrap();
        std::fs::write(&photo_two, b"two").unwrap();
        std::fs::create_dir(&empty_album).unwrap();

        let trim = default_settings(ContentMode::Trim, Encoder::X265);
        assert!(validate_task_targets(&trim, std::slice::from_ref(&video)).is_ok());
        assert!(validate_task_targets(&trim, &[video.clone(), video]).is_err());
        assert!(validate_task_targets(&trim, std::slice::from_ref(&root)).is_err());

        let slideshow = default_settings(ContentMode::PhotoSlideshow, Encoder::X265);
        assert!(validate_task_targets(&slideshow, std::slice::from_ref(&root)).is_ok());
        assert!(validate_task_targets(&slideshow, &[photo_one.clone(), photo_two]).is_ok());
        assert!(validate_task_targets(&slideshow, &[photo_one]).is_err());
        assert!(validate_task_targets(&slideshow, &[empty_album]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multi_target_queue_counts_reflect_filesystem_completion() {
        let root = temporary_test_directory("multi-target-counts");
        let original = root.join("original");
        std::fs::create_dir(&original).unwrap();
        let converted = root.join("episode1.mkv");
        let pending = root.join("episode2.mkv");
        std::fs::write(&converted, b"converted").unwrap();
        std::fs::write(original.join("episode1.mkv"), b"source").unwrap();
        std::fs::write(&pending, b"pending").unwrap();
        let task = QueueTask::new(
            "season",
            "Season",
            vec![converted, pending],
            default_settings(ContentMode::Tv, Encoder::X265),
        );

        let counts = ConverterApp::task_display_counts(&task, &[], None);

        assert_eq!(counts.targets, 2);
        assert_eq!(counts.files, 2);
        assert_eq!(counts.remaining, 1);
        assert_eq!(counts.converted, 1);
        assert_eq!(counts.failed, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_summary_matches_python_style_queue_counters() {
        let root = temporary_test_directory("folder-summary");
        let season = root.join("season");
        let original = season.join("original");
        std::fs::create_dir(&season).unwrap();
        std::fs::create_dir(&original).unwrap();
        let completed = season.join("episode1.mkv");
        let pending = season.join("episode2.mkv");
        let failed = season.join("episode3.mkv");
        std::fs::write(&completed, b"converted").unwrap();
        std::fs::write(original.join("episode1.mkv"), b"source").unwrap();
        std::fs::write(&pending, b"pending").unwrap();
        std::fs::write(&failed, b"failed").unwrap();
        let settings = default_settings(ContentMode::Tv, Encoder::X265);
        let mut queue = Queue::default();
        for (id, path, status) in [
            ("done", completed, QueueStatus::Completed),
            ("pending", pending, QueueStatus::Pending),
            ("failed", failed, QueueStatus::Failed),
        ] {
            let mut task = QueueTask::new(id, id, vec![path], settings.clone());
            task.source_root = Some(root.clone());
            task.status = status;
            queue.add(task).unwrap();
        }
        let watch = WatchedFolder {
            root: root.clone(),
            settings,
            snapshot: Vec::new(),
        };

        let summary = folder_queue_summary(&watch, &queue);

        assert_eq!(summary.folders, 2);
        assert_eq!(summary.files, 3);
        assert_eq!(summary.remaining, 1);
        assert_eq!(summary.converted, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(folder_summary_status(&summary), "Needs attention");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn aggregate_folder_task_reports_each_file_without_expanding_queue_rows() {
        let root = temporary_test_directory("aggregate-folder-summary");
        let first = root.join("episode1.mkv");
        let second = root.join("episode2.mkv");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let settings = default_settings(ContentMode::Tv, Encoder::X265);
        let mut task = QueueTask::new(
            "season",
            "TV - Season",
            vec![root.clone()],
            settings.clone(),
        );
        task.source_root = Some(root.clone());
        task.status = QueueStatus::Running;
        let mut queue = Queue::default();
        queue.add(task).unwrap();
        let watch = WatchedFolder {
            root: root.clone(),
            settings,
            snapshot: Vec::new(),
        };
        let failures = HashMap::from([(
            "season".to_owned(),
            HashMap::from([(first, "decode failed".to_owned())]),
        )]);

        let summary = folder_queue_summary_with_failures(&watch, &queue, &failures);

        assert_eq!(queue.tasks().len(), 1);
        assert_eq!(summary.files, 2);
        assert_eq!(summary.remaining, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.active_status, Some(QueueStatus::Running));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn aggregate_folder_summary_counts_public_outputs_with_original_backups() {
        let root = temporary_test_directory("aggregate-original-summary");
        let show = root.join("Show");
        let original = show.join("original");
        std::fs::create_dir_all(&original).unwrap();
        for episode in ["001", "002", "003"] {
            std::fs::write(show.join(format!("{episode}.mkv")), b"public").unwrap();
        }
        for episode in ["001", "002"] {
            std::fs::write(original.join(format!("{episode}.mp4")), b"source").unwrap();
        }
        std::fs::write(original.join("missing-output.mp4"), b"source").unwrap();
        let settings = default_settings(ContentMode::Tv, Encoder::X265);
        let mut task = QueueTask::new("show", "TV - Show", vec![root.clone()], settings.clone());
        task.source_root = Some(root.clone());
        task.status = QueueStatus::Running;
        let mut queue = Queue::default();
        queue.add(task).unwrap();
        let watch = WatchedFolder {
            root: root.clone(),
            settings,
            snapshot: Vec::new(),
        };

        let summary = folder_queue_summary(&watch, &queue);

        assert_eq!(summary.files, 3);
        assert_eq!(summary.converted, 2);
        assert_eq!(summary.remaining, 1);
        assert_eq!(summary.failed, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn already_encoded_camera_files_stay_completed_across_aggregate_rescans() {
        let root = temporary_test_directory("aggregate-camera-skip");
        let skipped = root.join("already-x265.mp4");
        let pending = root.join("new-camera.mp4");
        std::fs::write(&skipped, b"encoded").unwrap();
        std::fs::write(&pending, b"pending").unwrap();
        let settings = default_settings(ContentMode::CameraVideos, Encoder::X265);
        let mut task = QueueTask::new(
            "camera",
            "Camera videos - Album",
            vec![root.clone()],
            settings.clone(),
        );
        task.source_root = Some(root.clone());
        task.skipped_paths.push(skipped.clone());
        let mut queue = Queue::default();
        queue.add(task.clone()).unwrap();
        let watch = WatchedFolder {
            root: root.clone(),
            settings,
            snapshot: Vec::new(),
        };

        assert_eq!(remaining_task_inputs(&task, None), [pending]);
        let summary = folder_queue_summary(&watch, &queue);
        assert_eq!(summary.files, 2);
        assert_eq!(summary.converted, 1);
        assert_eq!(summary.remaining, 1);
        assert_eq!(summary.failed, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_summary_counts_preexisting_encoder_suffix_outputs() {
        let root = temporary_test_directory("folder-summary-suffix");
        let converted = root.join("show (x265)");
        std::fs::create_dir(&converted).unwrap();
        std::fs::write(converted.join("episode.mkv"), b"converted").unwrap();
        let watch = WatchedFolder {
            root: root.clone(),
            settings: default_settings(ContentMode::Tv, Encoder::X265),
            snapshot: Vec::new(),
        };

        let summary = folder_queue_summary(&watch, &Queue::default());

        assert_eq!(
            summary,
            FolderQueueSummary {
                root: root.clone(),
                mode: ContentMode::Tv,
                encoder: Encoder::X265,
                folders: 1,
                files: 1,
                remaining: 0,
                converted: 1,
                failed: 0,
                active_status: None,
            }
        );
        assert_eq!(folder_summary_status(&summary), "Completed");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folder_summary_counts_a_renamed_task_root_as_completed() {
        let parent = temporary_test_directory("folder-summary-renamed-root");
        let original_root = parent.join("Show");
        let converted_root = parent.join("Show (libsvtav1)");
        std::fs::create_dir_all(converted_root.join("Season 1")).unwrap();
        std::fs::create_dir_all(converted_root.join("original")).unwrap();
        std::fs::write(converted_root.join("episode1.mkv"), b"converted").unwrap();
        std::fs::write(
            converted_root.join("Season 1").join("episode2.mkv"),
            b"converted",
        )
        .unwrap();
        std::fs::write(
            converted_root.join("original").join("episode1.mp4"),
            b"source",
        )
        .unwrap();
        let watch = WatchedFolder {
            root: original_root.clone(),
            settings: default_settings(ContentMode::Tv, Encoder::X265),
            snapshot: Vec::new(),
        };

        let summary = folder_queue_summary(&watch, &Queue::default());

        assert_eq!(summary.root, original_root);
        assert_eq!(summary.folders, 2);
        assert_eq!(summary.files, 2);
        assert_eq!(summary.converted, 2);
        assert_eq!(summary.remaining, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(folder_summary_status(&summary), "Completed");
        std::fs::remove_dir_all(parent).unwrap();
    }

    fn temporary_test_directory(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("videoferry-{label}-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn temporary_file_names_report_their_owner_process() {
        assert_eq!(
            temporary_file_process_id("episode.videoferry-stage-123-task-7.mkv"),
            Some(123)
        );
        assert_eq!(
            temporary_file_process_id("episode.videoferry-previous-456-task-7.mkv"),
            Some(456)
        );
        assert_eq!(
            temporary_file_process_id("episode.videoferry-partial-789-7.mkv"),
            Some(789)
        );
        assert_eq!(temporary_file_process_id("notes.txt"), None);
    }

    #[test]
    fn startup_cleanup_preserves_active_and_unrelated_temporary_files() {
        let root = temporary_test_directory("startup-cleanup");
        let stale = root.join("episode.videoferry-stage-111-task-1.mkv");
        let active = root.join("episode.videoferry-previous-222-task-1.mkv");
        let current = root.join("episode.videoferry-partial-333-1.mkv");
        let unrelated = root.join("notes.txt");
        for path in [&stale, &active, &current, &unrelated] {
            std::fs::write(path, b"fixture").unwrap();
        }

        let removed = cleanup_temporary_directory(&root, 333, |process_id| process_id == 222)
            .expect("cleanup");

        assert_eq!(removed, 2);
        assert!(!stale.exists());
        assert!(active.exists());
        assert!(!current.exists());
        assert!(unrelated.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "native-ffmpeg")]
    #[test]
    fn development_lut_resolver_finds_bundled_camera_assets() {
        assert!(resolve_lut_path("action6.cube").is_some());
        assert!(resolve_lut_path("pocket3.cube").is_some());
    }
}
