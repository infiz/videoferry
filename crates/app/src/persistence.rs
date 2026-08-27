use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use videoferry_core::{
    AudioPolicy, ContentMode, Encoder, FpsPolicy, MetadataPolicy, Queue, QueueSettings,
    QueueStatus, QueueTask,
};
use videoferry_presets::default_settings;

const STATE_DIRECTORY_NAME: &str = "VideoFerry";
const APP_SETTINGS_FILE: &str = "settings.json";
const QUEUE_STATE_FILE: &str = "queue.json";
const COMPLETED_HISTORY_FILE: &str = "completed_history.json";
const COMPLETED_HISTORY_LOCK_FILE: &str = "completed_history.lock";
const QUEUE_STATE_VERSION: u32 = 2;

const LEGACY_HISTORY_COLUMN_COUNT: usize = 14;
const HISTORY_COLUMN_COUNT: usize = 19;

fn normalized_quality(encoder: Encoder, quality: f32) -> f32 {
    let maximum = if matches!(encoder, Encoder::X264 | Encoder::X265) {
        51.0
    } else {
        63.0
    };
    quality.round().clamp(0.0, maximum)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletedHistoryRow([String; HISTORY_COLUMN_COUNT]);

impl CompletedHistoryRow {
    #[must_use]
    pub fn new(columns: [String; HISTORY_COLUMN_COUNT]) -> Self {
        Self(columns)
    }

    #[must_use]
    pub fn columns(&self) -> &[String; HISTORY_COLUMN_COUNT] {
        &self.0
    }

    #[must_use]
    pub fn display_columns(&self) -> &[String] {
        &self.0[..LEGACY_HISTORY_COLUMN_COUNT]
    }

    #[must_use]
    pub fn task_id(&self) -> Option<&str> {
        history_value(&self.0[14])
    }

    #[must_use]
    pub fn input_path(&self) -> Option<&str> {
        history_value(&self.0[15])
    }

    #[must_use]
    pub fn original_codec(&self) -> Option<&str> {
        history_value(&self.0[16])
    }

    #[must_use]
    pub fn converted_codec(&self) -> Option<&str> {
        history_value(&self.0[17])
    }

    #[must_use]
    pub fn duration(&self) -> Option<&str> {
        history_value(&self.0[18])
    }

    fn key(&self) -> String {
        format!("{}|{}|{}", self.0[0], self.0[2], self.0[3])
    }
}

#[derive(Debug)]
pub(crate) struct PersistenceError(String);

impl Display for PersistenceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for PersistenceError {}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AppPreferences {
    pub selected_mode: ContentMode,
    pub settings_by_mode: HashMap<ContentMode, QueueSettings>,
    pub prevent_system_sleep: bool,
}

impl Default for AppPreferences {
    fn default() -> Self {
        Self {
            selected_mode: ContentMode::Tv,
            settings_by_mode: ContentMode::ALL
                .into_iter()
                .map(|mode| (mode, default_settings(mode, Encoder::X265)))
                .collect(),
            prevent_system_sleep: true,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LoadedQueue {
    pub queue: Queue,
    pub resume_task_id: Option<String>,
    pub next_id: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct StateStore {
    root: PathBuf,
    legacy_root: Option<PathBuf>,
}

impl StateStore {
    #[must_use]
    pub fn system() -> Self {
        Self {
            root: app_data_directory(),
            legacy_root: std::env::current_dir().ok(),
        }
    }

    #[cfg(test)]
    fn at(root: PathBuf) -> Self {
        Self {
            root,
            legacy_root: None,
        }
    }

    pub fn load_preferences(&self) -> Result<Option<AppPreferences>, PersistenceError> {
        let Some(stored) = self.read_json::<StoredAppSettings>(APP_SETTINGS_FILE)? else {
            return Ok(None);
        };
        let mut preferences = AppPreferences {
            prevent_system_sleep: stored.prevent_system_sleep,
            ..AppPreferences::default()
        };
        for (mode_name, stored_settings) in stored.queue_settings_by_mode {
            let Some(mode) = parse_mode(&mode_name) else {
                continue;
            };
            let Some(settings) = stored_settings.into_settings() else {
                continue;
            };
            if settings.mode == mode {
                preferences.settings_by_mode.insert(mode, settings);
            }
        }
        Ok(Some(preferences))
    }

    pub fn save_preferences(&self, preferences: &AppPreferences) -> Result<(), PersistenceError> {
        let queue_settings_by_mode = ContentMode::ALL
            .into_iter()
            .filter_map(|mode| {
                preferences
                    .settings_by_mode
                    .get(&mode)
                    .map(|settings| (mode.label().to_owned(), StoredSettings::from(settings)))
            })
            .collect();
        self.atomic_write_json(
            APP_SETTINGS_FILE,
            &StoredAppSettings {
                queue_settings_by_mode,
                prevent_system_sleep: preferences.prevent_system_sleep,
            },
        )
    }

    pub fn load_queue(&self) -> Result<Option<LoadedQueue>, PersistenceError> {
        let Some(state) = self.read_json::<StoredQueueState>(QUEUE_STATE_FILE)? else {
            return Ok(None);
        };
        if state.version != QUEUE_STATE_VERSION {
            return Err(PersistenceError(format!(
                "unsupported queue state version {}; expected {QUEUE_STATE_VERSION}",
                state.version
            )));
        }
        let mut queue = Queue::default();
        let mut next_id = 1_u64;
        for (index, stored) in state.tasks.into_iter().enumerate() {
            let Some(settings) = stored.settings.and_then(StoredSettings::into_settings) else {
                continue;
            };
            if stored.target_paths.is_empty() {
                continue;
            }
            let fallback_id = format!("task-{}", index + 1);
            let id = stored
                .task_data_id
                .filter(|value| !value.is_empty())
                .unwrap_or(fallback_id);
            if let Some(number) = id
                .strip_prefix("task-")
                .and_then(|value| value.parse::<u64>().ok())
            {
                next_id = next_id.max(number.saturating_add(1));
            }
            let name = stored
                .name
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| {
                    stored
                        .target_paths
                        .first()
                        .and_then(|path| path.file_name())
                        .and_then(|value| value.to_str())
                        .unwrap_or("Media task")
                        .to_owned()
                });
            let mut task = QueueTask::new(id, name, stored.target_paths, settings);
            task.source_root = stored.source_root;
            task.queued_time = stored.queued_time;
            task.complete_time = stored.complete_time;
            task.skipped_paths = stored.skipped_paths;
            task.status = match stored.status.as_deref() {
                Some("completed") => QueueStatus::Completed,
                _ => QueueStatus::Pending,
            };
            task.error = stored.error;
            let _ = queue.add(task);
        }
        let resume_task_id = state
            .was_running
            .then(|| queue.next_pending_id().map(str::to_owned))
            .flatten();
        Ok(Some(LoadedQueue {
            queue,
            resume_task_id,
            next_id,
        }))
    }

    pub fn save_queue(&self, queue: &Queue, was_running: bool) -> Result<(), PersistenceError> {
        let tasks = queue
            .tasks()
            .iter()
            .map(|task| StoredQueueTask {
                name: Some(task.name.clone()),
                target_paths: task.targets.clone(),
                source_root: task.source_root.clone(),
                settings: Some(StoredSettings::from(&task.settings)),
                queued_time: task.queued_time.clone(),
                task_data_id: Some(task.id.clone()),
                status: Some(status_name(task.status).to_owned()),
                complete_time: task.complete_time.clone(),
                error: task.error.clone(),
                skipped_paths: task.skipped_paths.clone(),
            })
            .collect();
        self.atomic_write_json(
            QUEUE_STATE_FILE,
            &StoredQueueState {
                version: QUEUE_STATE_VERSION,
                was_running,
                tasks,
            },
        )
    }

    pub fn load_history(&self) -> Result<Vec<CompletedHistoryRow>, PersistenceError> {
        let Some(value) = self.read_json::<serde_json::Value>(COMPLETED_HISTORY_FILE)? else {
            return Ok(Vec::new());
        };
        let Some(rows) = value.as_array() else {
            return Ok(Vec::new());
        };
        Ok(rows
            .iter()
            .filter_map(|row| row.as_array())
            .filter_map(|row| normalize_history_row(row))
            .collect())
    }

    pub fn append_history(&self, row: CompletedHistoryRow) -> Result<(), PersistenceError> {
        self.with_history_lock(|| {
            let existing = self.load_history()?;
            let mut merged = Vec::with_capacity(existing.len().saturating_add(1));
            let mut seen = std::collections::HashSet::new();
            for candidate in std::iter::once(row).chain(existing) {
                if seen.insert(candidate.key()) {
                    merged.push(candidate);
                }
            }
            self.write_history(&merged)
        })
    }

    pub fn clear_history(&self) -> Result<(), PersistenceError> {
        self.with_history_lock(|| self.write_history(&[]))
    }

    fn write_history(&self, rows: &[CompletedHistoryRow]) -> Result<(), PersistenceError> {
        let values = rows.iter().map(|row| row.0.to_vec()).collect::<Vec<_>>();
        self.atomic_write_json(COMPLETED_HISTORY_FILE, &values)
    }

    fn with_history_lock<T>(
        &self,
        action: impl FnOnce() -> Result<T, PersistenceError>,
    ) -> Result<T, PersistenceError> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            PersistenceError(format!(
                "creating history directory {}: {error}",
                self.root.display()
            ))
        })?;
        let path = self.root.join(COMPLETED_HISTORY_LOCK_FILE);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| PersistenceError(format!("opening {}: {error}", path.display())))?;
        lock.lock()
            .map_err(|error| PersistenceError(format!("locking {}: {error}", path.display())))?;
        action()
    }

    fn read_json<T: for<'de> Deserialize<'de>>(
        &self,
        file_name: &str,
    ) -> Result<Option<T>, PersistenceError> {
        let mut last_error = None;
        for root in std::iter::once(&self.root).chain(self.legacy_root.as_ref()) {
            let path = root.join(file_name);
            for candidate in [path.clone(), backup_path(&path)] {
                if !candidate.is_file() {
                    continue;
                }
                match std::fs::read(&candidate)
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| {
                        serde_json::from_slice(&bytes).map_err(|error| error.to_string())
                    }) {
                    Ok(value) => return Ok(Some(value)),
                    Err(error) => {
                        last_error = Some(format!("reading {}: {error}", candidate.display()));
                    }
                }
            }
        }
        last_error.map_or(Ok(None), |error| Err(PersistenceError(error)))
    }

    fn atomic_write_json<T: Serialize>(
        &self,
        file_name: &str,
        value: &T,
    ) -> Result<(), PersistenceError> {
        std::fs::create_dir_all(&self.root).map_err(|error| {
            PersistenceError(format!(
                "creating settings directory {}: {error}",
                self.root.display()
            ))
        })?;
        let destination = self.root.join(file_name);
        let temporary = temporary_path(&destination);
        let backup = backup_path(&destination);
        let write_result = (|| {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)?;
            let mut writer = BufWriter::new(file);
            serde_json::to_writer_pretty(&mut writer, value).map_err(std::io::Error::other)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            writer.get_ref().sync_all()
        })();
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temporary);
            return Err(PersistenceError(format!(
                "writing {}: {error}",
                temporary.display()
            )));
        }

        if backup.exists() {
            std::fs::remove_file(&backup).map_err(|error| {
                PersistenceError(format!("removing stale {}: {error}", backup.display()))
            })?;
        }
        let had_destination = destination.exists();
        if had_destination {
            std::fs::rename(&destination, &backup).map_err(|error| {
                PersistenceError(format!(
                    "preparing replacement for {}: {error}",
                    destination.display()
                ))
            })?;
        }
        if let Err(error) = std::fs::rename(&temporary, &destination) {
            if had_destination {
                let _ = std::fs::rename(&backup, &destination);
            }
            let _ = std::fs::remove_file(&temporary);
            return Err(PersistenceError(format!(
                "publishing {}: {error}",
                destination.display()
            )));
        }
        if had_destination {
            let _ = std::fs::remove_file(backup);
        }
        Ok(())
    }
}

fn normalize_history_row(row: &[serde_json::Value]) -> Option<CompletedHistoryRow> {
    let mut normalized = row.iter().map(python_string).collect::<Vec<_>>();
    match normalized.len() {
        HISTORY_COLUMN_COUNT => {}
        LEGACY_HISTORY_COLUMN_COUNT => {
            normalized.extend(std::iter::repeat_n(
                "-".to_owned(),
                HISTORY_COLUMN_COUNT - LEGACY_HISTORY_COLUMN_COUNT,
            ));
        }
        11 => normalized.extend(["-".to_owned(), "-".to_owned(), "-".to_owned()]),
        10 => normalized.insert(2, "-".to_owned()),
        9 => {
            normalized.insert(2, "-".to_owned());
            normalized.insert(8, "-".to_owned());
        }
        8 => {
            normalized.insert(1, "-".to_owned());
            normalized.insert(2, "-".to_owned());
            normalized.insert(8, "-".to_owned());
        }
        _ => return None,
    }
    if normalized.len() == 11 {
        normalized.extend(["-".to_owned(), "-".to_owned(), "-".to_owned()]);
    }
    if normalized.len() == LEGACY_HISTORY_COLUMN_COUNT {
        normalized.extend(std::iter::repeat_n(
            "-".to_owned(),
            HISTORY_COLUMN_COUNT - LEGACY_HISTORY_COLUMN_COUNT,
        ));
    }
    normalized
        .into_iter()
        .collect::<Vec<_>>()
        .try_into()
        .ok()
        .map(CompletedHistoryRow)
}

fn history_value(value: &str) -> Option<&str> {
    (!value.is_empty() && value != "-" && value != "None").then_some(value)
}

fn python_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Null => "None".to_owned(),
        serde_json::Value::Bool(true) => "True".to_owned(),
        serde_json::Value::Bool(false) => "False".to_owned(),
        _ => value.to_string(),
    }
}

fn app_data_directory() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(root) = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("APPDATA"))
        {
            return PathBuf::from(root).join(STATE_DIRECTORY_NAME);
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(profile).join(".videoferry");
        }
    }
    #[cfg(not(target_os = "windows"))]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join(STATE_DIRECTORY_NAME);
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".videoferry")
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    PathBuf::from(name)
}

fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".bak");
    PathBuf::from(name)
}

fn status_name(status: QueueStatus) -> &'static str {
    match status {
        QueueStatus::Running | QueueStatus::Paused => "running",
        QueueStatus::Completed => "completed",
        QueueStatus::Pending | QueueStatus::Failed | QueueStatus::Cancelled => "pending",
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredAppSettings {
    #[serde(default)]
    queue_settings_by_mode: BTreeMap<String, StoredSettings>,
    #[serde(default = "default_true")]
    prevent_system_sleep: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredQueueState {
    version: u32,
    #[serde(default)]
    was_running: bool,
    tasks: Vec<StoredQueueTask>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredQueueTask {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    target_paths: Vec<PathBuf>,
    #[serde(default)]
    source_root: Option<PathBuf>,
    #[serde(default)]
    settings: Option<StoredSettings>,
    #[serde(default)]
    queued_time: String,
    #[serde(default)]
    task_data_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    complete_time: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    skipped_paths: Vec<PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredSettings {
    #[serde(default)]
    mode: String,
    #[serde(default)]
    encoder: String,
    #[serde(default)]
    fps_raw: String,
    #[serde(default)]
    target_fps: Option<f64>,
    #[serde(default)]
    share_lowest_fps: bool,
    #[serde(default)]
    quality_crf: String,
    #[serde(default)]
    quality_preset: String,
    #[serde(default)]
    stabilize_strength: String,
    #[serde(default)]
    trim_start: String,
    #[serde(default)]
    trim_end: String,
    #[serde(default)]
    apply_lut: Option<bool>,
    #[serde(default)]
    camera_lut_path: Option<PathBuf>,
    #[serde(default = "default_photo_interval")]
    photo_interval_seconds: f64,
    #[serde(default = "default_resolution_name")]
    slideshow_resolution: String,
    #[serde(default)]
    slideshow_width: Option<u32>,
    #[serde(default)]
    slideshow_height: Option<u32>,
    #[serde(default = "default_slideshow_fps")]
    slideshow_fps: u32,
    #[serde(default)]
    slideshow_collage_enabled: bool,
    #[serde(default)]
    slideshow_audio_paths: Vec<PathBuf>,
    #[serde(default)]
    slideshow_audio_path: Option<PathBuf>,
    #[serde(default)]
    slideshow_image_paths: Vec<PathBuf>,
    #[serde(default)]
    slideshow_review_image_paths: Vec<PathBuf>,
    #[serde(default)]
    metadata: Option<String>,
}

impl From<&QueueSettings> for StoredSettings {
    fn from(settings: &QueueSettings) -> Self {
        let (fps_raw, target_fps, share_lowest_fps) = match settings.fps {
            FpsPolicy::SharedLowest => ("__auto__".to_owned(), None, true),
            FpsPolicy::Source => ("None".to_owned(), None, false),
            FpsPolicy::Exact(value) => (value.to_string(), Some(value), false),
        };
        Self {
            mode: settings.mode.label().to_owned(),
            encoder: settings.encoder.user_name().to_owned(),
            fps_raw,
            target_fps,
            share_lowest_fps,
            quality_crf: settings.quality.map_or_else(String::new, |value| {
                normalized_quality(settings.encoder, value).to_string()
            }),
            quality_preset: settings.speed_preset.clone().unwrap_or_default(),
            stabilize_strength: settings.stabilize_strength.clone(),
            trim_start: settings
                .trim_start
                .map_or_else(String::new, format_duration),
            trim_end: settings.trim_end.map_or_else(String::new, format_duration),
            apply_lut: Some(settings.apply_lut),
            camera_lut_path: settings.camera_lut_path.clone(),
            photo_interval_seconds: settings.photo_interval.as_secs_f64(),
            slideshow_resolution: match settings.slideshow_resolution {
                (3840, 2160) => "4K".to_owned(),
                _ => "1080p".to_owned(),
            },
            slideshow_width: Some(settings.slideshow_resolution.0),
            slideshow_height: Some(settings.slideshow_resolution.1),
            slideshow_fps: settings.slideshow_fps,
            slideshow_collage_enabled: settings.slideshow_collage,
            slideshow_audio_paths: settings.slideshow_audio_paths.clone(),
            slideshow_audio_path: None,
            slideshow_image_paths: settings.slideshow_image_paths.clone(),
            slideshow_review_image_paths: settings.slideshow_review_image_paths.clone(),
            metadata: Some(
                match settings.metadata {
                    MetadataPolicy::Preserve => "preserve",
                    MetadataPolicy::Remove => "remove",
                }
                .to_owned(),
            ),
        }
    }
}

impl StoredSettings {
    fn into_settings(self) -> Option<QueueSettings> {
        let mode = parse_mode(&self.mode)?;
        let encoder = parse_encoder(&self.encoder)?;
        let mut settings = default_settings(mode, encoder);
        settings.fps = if self.share_lowest_fps || self.fps_raw == "__auto__" {
            FpsPolicy::SharedLowest
        } else if let Some(value) = self
            .target_fps
            .filter(|value| value.is_finite() && *value > 0.0)
        {
            FpsPolicy::Exact(value)
        } else if let Ok(value) = self.fps_raw.parse::<f64>() {
            if value.is_finite() && value > 0.0 {
                FpsPolicy::Exact(value)
            } else {
                FpsPolicy::Source
            }
        } else {
            FpsPolicy::Source
        };
        settings.quality = self
            .quality_crf
            .parse::<f32>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| normalized_quality(encoder, value));
        settings.speed_preset = (!self.quality_preset.is_empty()).then_some(self.quality_preset);
        if !self.stabilize_strength.is_empty() {
            settings.stabilize_strength = self.stabilize_strength;
        }
        settings.trim_start = parse_duration(&self.trim_start);
        settings.trim_end = parse_duration(&self.trim_end);
        settings.apply_lut = if mode == ContentMode::CameraVideos && !encoder.is_hardware() {
            self.apply_lut.unwrap_or(true)
        } else {
            false
        };
        settings.camera_lut_path = self.camera_lut_path;
        if self.photo_interval_seconds.is_finite() && self.photo_interval_seconds > 0.0 {
            settings.photo_interval = Duration::from_secs_f64(self.photo_interval_seconds);
        }
        settings.slideshow_resolution = match (self.slideshow_width, self.slideshow_height) {
            (Some(width), Some(height)) if width > 0 && height > 0 => (width, height),
            _ if self.slideshow_resolution == "4K" => (3840, 2160),
            _ => (1920, 1080),
        };
        if self.slideshow_fps > 0 {
            settings.slideshow_fps = self.slideshow_fps;
        }
        settings.slideshow_collage = self.slideshow_collage_enabled;
        settings.slideshow_audio_paths = self
            .slideshow_audio_paths
            .into_iter()
            .filter(|path| path.is_file())
            .collect();
        if settings.slideshow_audio_paths.is_empty()
            && let Some(path) = self.slideshow_audio_path.filter(|path| path.is_file())
        {
            settings.slideshow_audio_paths.push(path);
        }
        settings.slideshow_image_paths = self.slideshow_image_paths;
        settings.slideshow_review_image_paths = self.slideshow_review_image_paths;
        settings.audio = AudioPolicy::CopyValid;
        settings.metadata = match self.metadata.as_deref() {
            Some("preserve") => MetadataPolicy::Preserve,
            Some("remove") => MetadataPolicy::Remove,
            _ => settings.metadata,
        };
        Some(settings)
    }
}

fn parse_mode(value: &str) -> Option<ContentMode> {
    ContentMode::ALL
        .into_iter()
        .find(|mode| mode.label() == value)
}

fn parse_encoder(value: &str) -> Option<Encoder> {
    Encoder::from_library_name(value)
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let hours = total / 3600;
    let minutes = total % 3600 / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn parse_duration(value: &str) -> Option<Duration> {
    if value.is_empty() {
        return None;
    }
    let parts = value
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [seconds] => *seconds,
        [minutes, seconds] if *seconds < 60 => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => hours
            .checked_mul(3600)?
            .checked_add(minutes.checked_mul(60)?)?
            .checked_add(*seconds)?,
        _ => return None,
    };
    Some(Duration::from_secs(seconds))
}

const fn default_photo_interval() -> f64 {
    4.0
}

fn default_resolution_name() -> String {
    "1080p".to_owned()
}

const fn default_slideshow_fps() -> u32 {
    30
}

const fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        AppPreferences, COMPLETED_HISTORY_FILE, CompletedHistoryRow, QUEUE_STATE_FILE, StateStore,
        StoredSettings, backup_path, temporary_path,
    };
    use videoferry_core::{
        ContentMode, Encoder, FpsPolicy, MetadataPolicy, Queue, QueueSettings, QueueStatus,
        QueueTask,
    };

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "videoferry-persistence-{label}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir(&path).expect("test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn detailed_settings(root: &std::path::Path) -> QueueSettings {
        let first_audio = root.join("one.flac");
        let second_audio = root.join("two.m4a");
        std::fs::write(&first_audio, b"audio").unwrap();
        std::fs::write(&second_audio, b"audio").unwrap();
        QueueSettings {
            mode: ContentMode::PhotoSlideshow,
            encoder: Encoder::SvtAv1,
            fps: FpsPolicy::Exact(23.976),
            quality: Some(32.0),
            speed_preset: Some("6".to_owned()),
            stabilize_strength: "Strong".to_owned(),
            trim_start: Some(std::time::Duration::from_secs(65)),
            trim_end: Some(std::time::Duration::from_secs(3723)),
            apply_lut: false,
            camera_lut_path: Some("lut/camera.cube".into()),
            photo_interval: std::time::Duration::from_millis(2750),
            slideshow_resolution: (1280, 720),
            slideshow_fps: 24,
            slideshow_collage: true,
            slideshow_audio_paths: vec![first_audio, second_audio],
            slideshow_image_paths: vec!["photos/2.jpg".into(), "photos/1.jpg".into()],
            slideshow_review_image_paths: vec!["photos/1.jpg".into(), "photos/2.jpg".into()],
            metadata: MetadataPolicy::Preserve,
            ..QueueSettings::default()
        }
    }

    #[test]
    fn rust_state_files_are_isolated_from_python() {
        assert_eq!(super::STATE_DIRECTORY_NAME, "VideoFerry");
        assert_ne!(super::APP_SETTINGS_FILE, "video_converter_settings.json");
        assert_ne!(QUEUE_STATE_FILE, "video_converter_queue.json");
        assert_ne!(
            COMPLETED_HISTORY_FILE,
            "video_converter_completed_history.json"
        );
    }

    fn history_row(path: &str, start: &str, new_size: &str) -> CompletedHistoryRow {
        CompletedHistoryRow::new([
            path.to_owned(),
            "TV".to_owned(),
            "-".to_owned(),
            start.to_owned(),
            "2026-08-25 12:01:00".to_owned(),
            "1.00".to_owned(),
            "20.00 MB".to_owned(),
            new_size.to_owned(),
            "1920x1080".to_owned(),
            "30".to_owned(),
            "24".to_owned(),
            "x265".to_owned(),
            "28".to_owned(),
            "medium".to_owned(),
            "task-1".to_owned(),
            "source.mkv".to_owned(),
            "h264".to_owned(),
            "hevc".to_owned(),
            "42:00".to_owned(),
        ])
    }

    #[test]
    fn preferences_round_trip_per_workflow_settings() {
        let directory = TestDirectory::new("preferences");
        let store = StateStore::at(directory.0.clone());
        let mut preferences = AppPreferences {
            selected_mode: ContentMode::PhotoSlideshow,
            prevent_system_sleep: false,
            ..AppPreferences::default()
        };
        preferences
            .settings_by_mode
            .insert(ContentMode::PhotoSlideshow, detailed_settings(&directory.0));

        store
            .save_preferences(&preferences)
            .expect("save preferences");
        let restored = store
            .load_preferences()
            .expect("load preferences")
            .expect("stored preferences");

        let mut expected = preferences;
        expected.selected_mode = ContentMode::Tv;
        assert_eq!(restored, expected);
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.0.join(super::APP_SETTINGS_FILE)).unwrap(),
        )
        .unwrap();
        assert!(value.get("selected_mode").is_none());
    }

    #[test]
    fn stored_decimal_quality_is_rounded_to_a_supported_integer() {
        let stored = StoredSettings {
            mode: "TV".to_owned(),
            encoder: "x265".to_owned(),
            quality_crf: "31.5".to_owned(),
            ..StoredSettings::default()
        };

        assert_eq!(stored.into_settings().unwrap().quality, Some(32.0));
    }

    #[test]
    fn queue_round_trip_normalizes_interrupted_work_and_preserves_order() {
        let directory = TestDirectory::new("queue");
        let store = StateStore::at(directory.0.clone());
        let mut queue = Queue::default();
        let settings = detailed_settings(&directory.0);
        let mut running = QueueTask::new(
            "task-7",
            "Running",
            vec!["input-one.mkv".into()],
            settings.clone(),
        );
        running.status = QueueStatus::Running;
        running.source_root = Some("watched-folder".into());
        running.queued_time = "2026-08-25 12:00:00".to_owned();
        queue.add(running).unwrap();
        let mut completed = QueueTask::new(
            "task-9",
            "Completed",
            vec!["input-two.mkv".into()],
            QueueSettings::default(),
        );
        completed.status = QueueStatus::Completed;
        completed.queued_time = "2026-08-25 12:01:00".to_owned();
        completed.complete_time = "2026-08-25 12:02:00".to_owned();
        completed.skipped_paths = vec!["already-x265.mp4".into()];
        queue.add(completed).unwrap();

        store.save_queue(&queue, true).expect("save queue");
        let restored = store
            .load_queue()
            .expect("load queue")
            .expect("stored queue");

        assert_eq!(restored.resume_task_id.as_deref(), Some("task-7"));
        assert_eq!(restored.next_id, 10);
        assert_eq!(restored.queue.tasks()[0].status, QueueStatus::Pending);
        assert_eq!(
            restored.queue.tasks()[0].source_root.as_deref(),
            Some(std::path::Path::new("watched-folder"))
        );
        assert_eq!(restored.queue.tasks()[1].status, QueueStatus::Completed);
        assert_eq!(restored.queue.tasks()[0].queued_time, "2026-08-25 12:00:00");
        assert_eq!(restored.queue.tasks()[1].queued_time, "2026-08-25 12:01:00");
        assert_eq!(
            restored.queue.tasks()[1].complete_time,
            "2026-08-25 12:02:00"
        );
        assert_eq!(restored.queue.tasks()[0].settings, settings);
        assert_eq!(
            restored.queue.tasks()[1].skipped_paths,
            [std::path::PathBuf::from("already-x265.mp4")]
        );
    }

    #[test]
    fn loader_recovers_last_valid_file_after_interrupted_replacement() {
        let directory = TestDirectory::new("backup");
        let store = StateStore::at(directory.0.clone());
        let mut queue = Queue::default();
        queue
            .add(QueueTask::new(
                "task-1",
                "Recover me",
                vec!["input.mkv".into()],
                QueueSettings::default(),
            ))
            .unwrap();
        store.save_queue(&queue, false).expect("save queue");
        let primary = directory.0.join(QUEUE_STATE_FILE);
        std::fs::copy(&primary, backup_path(&primary)).expect("backup fixture");
        std::fs::write(&primary, b"{truncated").expect("corrupt primary");

        let restored = store
            .load_queue()
            .expect("recover backup")
            .expect("stored queue");

        assert_eq!(restored.queue.tasks()[0].name, "Recover me");
    }

    #[test]
    fn successful_write_leaves_no_temporary_or_backup_file() {
        let directory = TestDirectory::new("atomic");
        let store = StateStore::at(directory.0.clone());
        store
            .save_queue(&Queue::default(), false)
            .expect("first save");
        store
            .save_queue(&Queue::default(), false)
            .expect("replacement save");
        let primary = directory.0.join(QUEUE_STATE_FILE);
        assert!(primary.is_file());
        assert!(!temporary_path(&primary).exists());
        assert!(!backup_path(&primary).exists());
    }

    #[test]
    fn malformed_json_is_reported_without_panicking() {
        let directory = TestDirectory::new("invalid");
        let store = StateStore::at(directory.0.clone());
        std::fs::write(directory.0.join(QUEUE_STATE_FILE), b"not json").unwrap();
        assert!(store.load_queue().is_err());
    }

    #[test]
    fn reads_the_python_version_two_queue_schema() {
        let directory = TestDirectory::new("python-schema");
        let store = StateStore::at(directory.0.clone());
        let soundtrack = directory.0.join("soundtrack.m4a");
        std::fs::write(&soundtrack, b"audio").unwrap();
        let missing_soundtrack = directory.0.join("missing.m4a");
        let state = serde_json::json!({
            "version": 2,
            "was_running": true,
            "tasks": [{
                "name": "Python camera task",
                "target_paths": ["camera/DJI_0001.MP4"],
                "settings": {
                    "mode": "Camera videos",
                    "encoder": "x265",
                    "fps_raw": "None",
                    "target_fps": null,
                    "share_lowest_fps": false,
                    "quality_crf": "28",
                    "quality_preset": "medium",
                    "stabilize_strength": "Balanced",
                    "trim_start": "00:00",
                    "trim_end": "00:10",
                    "photo_interval_seconds": 4.0,
                    "slideshow_resolution": "1080p",
                    "slideshow_fps": 30,
                    "slideshow_collage_enabled": false,
                    "slideshow_audio_paths": [soundtrack, missing_soundtrack],
                    "slideshow_image_paths": [],
                    "slideshow_review_image_paths": []
                },
                "queued_time": "2026-08-25 12:00:00",
                "task_data_id": "python-task-id",
                "status": "running",
                "complete_time": ""
            }]
        });
        std::fs::write(
            directory.0.join(QUEUE_STATE_FILE),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();

        let restored = store
            .load_queue()
            .expect("load Python queue")
            .expect("Python queue state");
        let task = &restored.queue.tasks()[0];

        assert_eq!(restored.resume_task_id.as_deref(), Some("python-task-id"));
        assert_eq!(task.status, QueueStatus::Pending);
        assert_eq!(task.settings.mode, ContentMode::CameraVideos);
        assert_eq!(task.settings.encoder, Encoder::X265);
        assert_eq!(task.settings.fps, FpsPolicy::Source);
        assert!(task.settings.apply_lut);
        assert_eq!(task.settings.slideshow_audio_paths, [soundtrack]);
        assert_eq!(task.queued_time, "2026-08-25 12:00:00");
        assert!(task.complete_time.is_empty());
    }

    #[test]
    fn python_loader_disables_lut_for_non_camera_or_hardware_tasks() {
        for (mode, encoder) in [("TV", "x265"), ("Camera videos", "hevc_nvenc")] {
            let stored: StoredSettings = serde_json::from_value(serde_json::json!({
                "mode": mode,
                "encoder": encoder,
                "fps_raw": "None",
                "share_lowest_fps": false,
                "quality_crf": "28",
                "quality_preset": "medium",
                "stabilize_strength": "Balanced",
                "trim_start": "00:00",
                "trim_end": "00:10",
                "apply_lut": true
            }))
            .unwrap();

            assert!(!stored.into_settings().unwrap().apply_lut);
        }
    }

    #[test]
    fn saved_software_encoder_names_are_accepted_by_python() {
        let directory = TestDirectory::new("python-encoder-names");
        let store = StateStore::at(directory.0.clone());
        let mut queue = Queue::default();
        queue
            .add(QueueTask::new(
                "x265-task",
                "x265",
                vec!["episode.mkv".into()],
                videoferry_presets::default_settings(ContentMode::Tv, Encoder::X265),
            ))
            .unwrap();
        queue
            .add(QueueTask::new(
                "x264-task",
                "x264",
                vec!["movie.mp4".into()],
                videoferry_presets::default_settings(ContentMode::Tv, Encoder::X264),
            ))
            .unwrap();

        store.save_queue(&queue, false).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.0.join(QUEUE_STATE_FILE)).unwrap())
                .unwrap();
        let encoders = value["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["settings"]["encoder"].as_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(encoders, ["x265", "x264"]);
    }

    #[test]
    fn history_normalizes_all_python_legacy_row_shapes() {
        let directory = TestDirectory::new("history-legacy");
        let store = StateStore::at(directory.0.clone());
        let rows = serde_json::json!([
            [
                "eleven", "TV", "LUT", "start", "end", "1", "2", "3", "4", "5", "6"
            ],
            ["ten", "TV", "start", "end", "1", "2", "3", "4", "5", "6"],
            ["nine", "TV", "start", "end", "1", "2", "3", "5", "6"],
            ["eight", "start", "end", "1", "2", "3", "5", "6"],
            ["invalid"]
        ]);
        std::fs::write(
            directory.0.join(COMPLETED_HISTORY_FILE),
            serde_json::to_vec_pretty(&rows).unwrap(),
        )
        .unwrap();

        let restored = store.load_history().expect("load legacy history");

        assert_eq!(restored.len(), 4);
        assert_eq!(restored[0].columns()[2], "LUT");
        assert_eq!(restored[1].columns()[2], "-");
        assert_eq!(restored[2].columns()[8], "-");
        assert_eq!(restored[3].columns()[1], "-");
        assert_eq!(restored[3].columns()[8], "-");
        assert_eq!(&restored[0].columns()[11..14], ["-", "-", "-"]);
        assert_eq!(&restored[0].columns()[14..], ["-", "-", "-", "-", "-"]);
    }

    #[test]
    fn history_append_merges_newest_first_and_deduplicates_python_key() {
        let directory = TestDirectory::new("history-merge");
        let store = StateStore::at(directory.0.clone());
        store
            .append_history(history_row("one.mkv", "start-one", "10.00 MB"))
            .expect("append first row");
        store
            .append_history(history_row("two.mkv", "start-two", "11.00 MB"))
            .expect("append second row");
        store
            .append_history(history_row("one.mkv", "start-one", "12.00 MB"))
            .expect("replace duplicate row");

        let restored = store.load_history().expect("load merged history");

        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].columns()[0], "one.mkv");
        assert_eq!(restored[0].columns()[7], "12.00 MB");
        assert_eq!(restored[1].columns()[0], "two.mkv");
    }

    #[test]
    fn history_clear_persists_an_empty_python_compatible_list() {
        let directory = TestDirectory::new("history-clear");
        let store = StateStore::at(directory.0.clone());
        store
            .append_history(history_row("one.mkv", "start", "10.00 MB"))
            .expect("append history");

        store.clear_history().expect("clear history");

        assert!(
            store
                .load_history()
                .expect("load cleared history")
                .is_empty()
        );
        let stored: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.0.join(COMPLETED_HISTORY_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(stored, serde_json::json!([]));
    }
}
