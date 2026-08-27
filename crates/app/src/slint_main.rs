#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
// Slint's generated item tree contains audited framework internals that opt
// themselves out locally; handwritten frontend code remains safe-only.
#![deny(unsafe_code)]

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::language::StandardListViewItem;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use slint::{
    CloseRequestResponse, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer,
    SharedString, Timer, TimerMode, VecModel,
};
use videoferry_app::legacy::{
    SlintAppSnapshot, SlintController, SlintTaskFileSnapshot, SlintTaskSnapshot,
};

slint::include_modules!();

const CONTROLLER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CONVERSION_STATUS_INTERVAL: Duration = Duration::from_secs(1);
const PRODUCT_PAGE_URL: &str = "https://apps.infiz.com/videoferry";

#[derive(Clone, Copy, Default)]
struct TaskFileSort {
    column: usize,
    descending: bool,
}

fn main() -> Result<(), slint::PlatformError> {
    if videoferry_app::legacy::handle_runtime_verification() {
        return Ok(());
    }
    if let Err(error) = videoferry_app::legacy::cleanup_stale_temporary_files() {
        eprintln!("unable to clean stale VideoFerry temporary files: {error}");
    }

    let ui = AppWindow::new()?;
    ui.set_app_version(SharedString::from(env!("CARGO_PKG_VERSION")));
    let controller = Rc::new(RefCell::new(SlintController::new()));
    let task_file_sort = Rc::new(RefCell::new(TaskFileSort::default()));

    wire_callbacks(&ui, &controller, &task_file_sort);
    wire_native_file_drop(&ui, &controller);
    let initial_snapshot = controller.borrow().snapshot();
    refresh(&ui, &initial_snapshot, *task_file_sort.borrow());

    let timer = Timer::default();
    let weak_ui = ui.as_weak();
    let timer_controller = Rc::clone(&controller);
    let last_snapshot = Rc::new(RefCell::new(initial_snapshot));
    let timer_snapshot = Rc::clone(&last_snapshot);
    let next_status_refresh = Rc::new(RefCell::new(Instant::now()));
    let timer_status_refresh = Rc::clone(&next_status_refresh);
    let last_completion_notification = Rc::new(RefCell::new(String::new()));
    let timer_completion_notification = Rc::clone(&last_completion_notification);
    let timer_task_file_sort = Rc::clone(&task_file_sort);
    timer.start(TimerMode::Repeated, CONTROLLER_POLL_INTERVAL, move || {
        let Some(ui) = weak_ui.upgrade() else {
            return;
        };
        let mut snapshot = {
            let mut controller = timer_controller.borrow_mut();
            controller.tick();
            controller.snapshot()
        };
        let now = Instant::now();
        let refresh_conversion_status = {
            let displayed = timer_snapshot.borrow();
            !snapshot.is_running
                || !displayed.is_running
                || snapshot.active_detail != displayed.active_detail
                || now >= *timer_status_refresh.borrow()
        };
        if refresh_conversion_status {
            *timer_status_refresh.borrow_mut() = now + CONVERSION_STATUS_INTERVAL;
        } else {
            let displayed = timer_snapshot.borrow();
            snapshot.progress = displayed.progress;
            snapshot.live_status.clone_from(&displayed.live_status);
        }
        {
            let mut last_notification = timer_completion_notification.borrow_mut();
            if snapshot.completion_notice.is_empty() {
                last_notification.clear();
            } else if *last_notification != snapshot.completion_notice {
                show_completion_notification(&snapshot.completion_notice);
                last_notification.clone_from(&snapshot.completion_notice);
            }
        }
        if snapshot != *timer_snapshot.borrow() {
            refresh(&ui, &snapshot, *timer_task_file_sort.borrow());
            *timer_snapshot.borrow_mut() = snapshot;
        }
    });

    let close_controller = Rc::clone(&controller);
    ui.window().on_close_requested(move || {
        close_controller.borrow_mut().shutdown();
        CloseRequestResponse::HideWindow
    });

    ui.run()
}

fn show_completion_notification(message: &str) {
    let _result = notify_rust::Notification::new()
        .appname("VideoFerry")
        .summary("VideoFerry finished")
        .body(message)
        .show();
}

fn wire_native_file_drop(ui: &AppWindow, controller: &Rc<RefCell<SlintController>>) {
    let weak_ui = ui.as_weak();
    let drop_controller = Rc::clone(controller);
    ui.window().on_winit_window_event(move |_window, event| {
        let Some(ui) = weak_ui.upgrade() else {
            return EventResult::Propagate;
        };
        let accepts_task_media = ui.get_task_builder_open() && ui.get_task_builder_step() == 1;

        match event {
            WindowEvent::HoveredFile(_) => ui.set_task_drop_active(accepts_task_media),
            WindowEvent::HoveredFileCancelled => ui.set_task_drop_active(false),
            WindowEvent::DroppedFile(path) => {
                ui.set_task_drop_active(false);
                if accepts_task_media {
                    drop_controller
                        .borrow_mut()
                        .add_task_draft_paths(vec![path.clone()]);
                }
            }
            _ => {}
        }

        EventResult::Propagate
    });
}

fn wire_callbacks(
    ui: &AppWindow,
    controller: &Rc<RefCell<SlintController>>,
    task_file_sort: &Rc<RefCell<TaskFileSort>>,
) {
    ui.on_open_product_page(|| {
        let _result = open_product_page();
    });
    wire_queue_callbacks(ui, controller, task_file_sort);
    wire_task_builder_callbacks(ui, controller);
    wire_settings_callbacks(ui, controller);
}

fn open_product_page() -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("explorer.exe")
            .arg(PRODUCT_PAGE_URL)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(drop)
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(PRODUCT_PAGE_URL)
            .spawn()
            .map(drop)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(PRODUCT_PAGE_URL)
            .spawn()
            .map(drop)
    }
}

fn wire_task_builder_callbacks(ui: &AppWindow, controller: &Rc<RefCell<SlintController>>) {
    let begin_controller = Rc::clone(controller);
    ui.on_begin_task(move || begin_controller.borrow_mut().begin_task_draft());

    let prepare_controller = Rc::clone(controller);
    ui.on_prepare_task(move || prepare_controller.borrow_mut().prepare_task_draft());

    let cancel_controller = Rc::clone(controller);
    ui.on_cancel_task(move || cancel_controller.borrow_mut().cancel_task_draft());

    let add_files_controller = Rc::clone(controller);
    ui.on_add_task_files(move || {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Add files to this task")
            .pick_files()
        {
            add_files_controller
                .borrow_mut()
                .add_task_draft_paths(paths);
        }
    });

    let add_folder_controller = Rc::clone(controller);
    ui.on_add_task_folder(move || {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Add a folder to this task")
            .pick_folder()
        {
            add_folder_controller
                .borrow_mut()
                .add_task_draft_paths(vec![path]);
        }
    });

    let move_controller = Rc::clone(controller);
    ui.on_move_task_target(move |index, delta| {
        let Some(index) = valid_index(index) else {
            return -1;
        };
        let Ok(delta) = isize::try_from(delta) else {
            return i32::try_from(index).unwrap_or(-1);
        };
        move_controller
            .borrow_mut()
            .move_task_draft_target(index, delta)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_else(|| i32::try_from(index).unwrap_or(-1))
    });

    let remove_controller = Rc::clone(controller);
    ui.on_remove_task_target(move |index| {
        valid_index(index).is_some_and(|index| {
            remove_controller
                .borrow_mut()
                .remove_task_draft_target(index)
        })
    });

    let create_controller = Rc::clone(controller);
    ui.on_create_task(move |name| {
        create_controller
            .borrow_mut()
            .create_task_from_draft(name.as_str())
    });
}

fn wire_queue_callbacks(
    ui: &AppWindow,
    controller: &Rc<RefCell<SlintController>>,
    task_file_sort: &Rc<RefCell<TaskFileSort>>,
) {
    let start_controller = Rc::clone(controller);
    ui.on_start_queue(move || start_controller.borrow_mut().start_queue());

    let select_controller = Rc::clone(controller);
    ui.on_select_task(move |index| {
        if let Some(index) = valid_index(index) {
            select_controller.borrow_mut().select_task(index);
        }
    });

    let retry_controller = Rc::clone(controller);
    ui.on_retry_task(move |index| {
        if let Some(index) = valid_index(index) {
            let mut controller = retry_controller.borrow_mut();
            controller.select_task(index);
            controller.retry_selected();
            controller.start_selected();
        }
    });

    let weak_ui = ui.as_weak();
    let sort_controller = Rc::clone(controller);
    let sort_state = Rc::clone(task_file_sort);
    ui.on_sort_task_files(move |column, ascending| {
        let Some(column) = valid_index(column).filter(|column| *column <= 10) else {
            return;
        };
        let sort = TaskFileSort {
            column,
            descending: !ascending,
        };
        *sort_state.borrow_mut() = sort;
        let files = sort_controller.borrow().snapshot().selected_task_files;
        if let Some(ui) = weak_ui.upgrade() {
            refresh_task_file_rows(&ui, &files, sort);
        }
    });

    let move_controller = Rc::clone(controller);
    ui.on_move_selected(move |delta| {
        if let Ok(delta) = isize::try_from(delta) {
            move_controller.borrow_mut().move_selected(delta);
        }
    });

    let reorder_controller = Rc::clone(controller);
    ui.on_reorder_task(move |source, target| {
        if let (Some(source), Some(target)) = (valid_index(source), valid_index(target)) {
            reorder_controller.borrow_mut().move_task_to(source, target);
        }
    });

    let remove_controller = Rc::clone(controller);
    ui.on_remove_selected(move || remove_controller.borrow_mut().remove_selected());

    let clear_queue_controller = Rc::clone(controller);
    ui.on_clear_queue(move || clear_queue_controller.borrow_mut().clear_queue());

    let clear_history_controller = Rc::clone(controller);
    ui.on_clear_history(move || clear_history_controller.borrow_mut().clear_history());

    let open_history_controller = Rc::clone(controller);
    ui.on_open_history_item(move |path| {
        open_history_controller
            .borrow_mut()
            .open_history_item(path.as_str());
    });

    let reveal_history_controller = Rc::clone(controller);
    ui.on_reveal_history_item(move |path| {
        reveal_history_controller
            .borrow_mut()
            .reveal_history_item(path.as_str());
    });

    let copy_controller = Rc::clone(controller);
    ui.on_copy_text(move |value| {
        let result = arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(value.to_string()));
        copy_controller.borrow_mut().report_clipboard_result(
            "text",
            result.as_ref().err().map(ToString::to_string).as_deref(),
        );
    });

    let filter_controller = Rc::clone(controller);
    ui.on_set_history_filter(move |filter| {
        filter_controller
            .borrow_mut()
            .set_history_filter(filter.to_string());
    });

    let dismiss_controller = Rc::clone(controller);
    ui.on_dismiss_completion(move || {
        dismiss_controller.borrow_mut().dismiss_completion();
    });

    let pause_controller = Rc::clone(controller);
    ui.on_toggle_pause(move || pause_controller.borrow_mut().toggle_pause());

    let preview_controller = Rc::clone(controller);
    ui.on_set_live_preview(move |enabled| {
        preview_controller.borrow_mut().set_live_preview(enabled);
    });

    let pause_after_controller = Rc::clone(controller);
    ui.on_pause_after_current(move || pause_after_controller.borrow_mut().pause_after_current());

    let stop_controller = Rc::clone(controller);
    ui.on_stop_current(move || stop_controller.borrow_mut().stop_current());

    let stop_all_controller = Rc::clone(controller);
    ui.on_stop_all(move || stop_all_controller.borrow_mut().stop_all());
}

fn wire_settings_callbacks(ui: &AppWindow, controller: &Rc<RefCell<SlintController>>) {
    wire_audio_callbacks(ui, controller);

    let mode_controller = Rc::clone(controller);
    ui.on_set_mode(move |index| {
        if let Some(index) = valid_index(index) {
            mode_controller.borrow_mut().set_mode(index);
        }
    });

    let encoder_controller = Rc::clone(controller);
    ui.on_set_encoder(move |index| {
        if let Some(index) = valid_index(index) {
            encoder_controller.borrow_mut().set_encoder(index);
        }
    });

    let quality_controller = Rc::clone(controller);
    ui.on_set_quality(move |quality| quality_controller.borrow_mut().set_quality(quality));

    let fps_mode_controller = Rc::clone(controller);
    ui.on_set_fps_mode(move |index| {
        if let Some(index) = valid_index(index) {
            fps_mode_controller.borrow_mut().set_fps_mode(index);
        }
    });

    let explicit_fps_controller = Rc::clone(controller);
    ui.on_set_explicit_fps(move |fps| {
        explicit_fps_controller.borrow_mut().set_explicit_fps(fps);
    });

    let trim_start_controller = Rc::clone(controller);
    ui.on_set_trim_start(move |value| {
        trim_start_controller
            .borrow_mut()
            .set_trim_start(value.to_string());
    });

    let trim_end_controller = Rc::clone(controller);
    ui.on_set_trim_end(move |value| {
        trim_end_controller
            .borrow_mut()
            .set_trim_end(value.to_string());
    });

    let lut_controller = Rc::clone(controller);
    ui.on_set_apply_lut(move |enabled| lut_controller.borrow_mut().set_apply_lut(enabled));

    let stabilize_controller = Rc::clone(controller);
    ui.on_set_stabilize_strength(move |index| {
        if let Some(index) = valid_index(index) {
            stabilize_controller
                .borrow_mut()
                .set_stabilize_strength(index);
        }
    });

    let interval_controller = Rc::clone(controller);
    ui.on_set_slideshow_interval(move |seconds| {
        interval_controller
            .borrow_mut()
            .set_slideshow_interval(seconds);
    });

    let slideshow_fps_controller = Rc::clone(controller);
    ui.on_set_slideshow_fps(move |fps| {
        slideshow_fps_controller.borrow_mut().set_slideshow_fps(fps);
    });

    let resolution_controller = Rc::clone(controller);
    ui.on_set_slideshow_resolution(move |index| {
        if let Some(index) = valid_index(index) {
            resolution_controller
                .borrow_mut()
                .set_slideshow_resolution(index);
        }
    });

    let collage_controller = Rc::clone(controller);
    ui.on_set_slideshow_collage(move |enabled| {
        collage_controller
            .borrow_mut()
            .set_slideshow_collage(enabled);
    });

    let set_speed_controller = Rc::clone(controller);
    ui.on_set_speed(move |index| {
        if let Some(index) = valid_index(index) {
            set_speed_controller.borrow_mut().set_speed(index);
        }
    });

    let sleep_controller = Rc::clone(controller);
    ui.on_set_prevent_sleep(move |enabled| {
        sleep_controller.borrow_mut().set_prevent_sleep(enabled);
    });
}

fn wire_audio_callbacks(ui: &AppWindow, controller: &Rc<RefCell<SlintController>>) {
    let add_controller = Rc::clone(controller);
    ui.on_add_slideshow_audio(move || {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Choose slideshow audio")
            .add_filter(
                "Audio files",
                &["mp3", "m4a", "aac", "wav", "flac", "ogg", "opus", "wma"],
            )
            .pick_files()
        {
            add_controller.borrow_mut().add_slideshow_audio(paths);
        }
    });

    let select_controller = Rc::clone(controller);
    ui.on_select_slideshow_audio(move |index| {
        if let Some(index) = valid_index(index) {
            select_controller.borrow_mut().select_slideshow_audio(index);
        }
    });

    let move_controller = Rc::clone(controller);
    ui.on_move_slideshow_audio(move |delta| {
        if let Ok(delta) = isize::try_from(delta) {
            move_controller.borrow_mut().move_slideshow_audio(delta);
        }
    });

    let remove_controller = Rc::clone(controller);
    ui.on_remove_slideshow_audio(move || {
        remove_controller.borrow_mut().remove_slideshow_audio();
    });

    let clear_controller = Rc::clone(controller);
    ui.on_clear_slideshow_audio(move || {
        clear_controller.borrow_mut().clear_slideshow_audio();
    });
}

fn valid_index(index: i32) -> Option<usize> {
    usize::try_from(index).ok()
}

fn task_item(task: &SlintTaskSnapshot) -> TaskItem {
    TaskItem {
        title: SharedString::from(&task.title),
        subtitle: SharedString::from(&task.subtitle),
        status: SharedString::from(&task.status),
        completed_before: task.completed_before,
        completed_this_run: task.completed_this_run,
        has_progress: task.has_progress,
        progress_label: SharedString::from(&task.progress_label),
        progress_summary: SharedString::from(&task.progress_summary),
        completed_before_label: SharedString::from(&task.completed_before_label),
        completed_this_run_label: SharedString::from(&task.completed_this_run_label),
        remaining_label: SharedString::from(&task.remaining_label),
        selected: task.selected,
        active: task.active,
        can_run: task.can_run,
        can_retry: task.can_retry,
        can_reorder: task.can_reorder,
        has_error: !task.error_detail.is_empty(),
        error_detail: SharedString::from(&task.error_detail),
    }
}

fn task_file_path_row(file: &SlintTaskFileSnapshot) -> ModelRc<StandardListViewItem> {
    model(vec![StandardListViewItem::from(file.path.as_str())])
}

fn task_file_metric_row(file: &SlintTaskFileSnapshot) -> ModelRc<StandardListViewItem> {
    model(vec![
        StandardListViewItem::from(file.status.as_str()),
        StandardListViewItem::from(file.started_time.as_str()),
        StandardListViewItem::from(file.completed_time.as_str()),
        StandardListViewItem::from(file.conversion_time.as_str()),
        StandardListViewItem::from(file.original_size.as_str()),
        StandardListViewItem::from(file.new_size.as_str()),
        StandardListViewItem::from(file.original_fps.as_str()),
        StandardListViewItem::from(file.new_fps.as_str()),
        StandardListViewItem::from(file.codec.as_str()),
        StandardListViewItem::from(file.duration.as_str()),
    ])
}

fn task_file_table_rows(
    files: &[SlintTaskFileSnapshot],
) -> (
    Vec<ModelRc<StandardListViewItem>>,
    Vec<ModelRc<StandardListViewItem>>,
) {
    (
        files.iter().map(task_file_path_row).collect(),
        files.iter().map(task_file_metric_row).collect(),
    )
}

fn task_file_number(value: &str) -> Option<f64> {
    value
        .split_whitespace()
        .next()
        .and_then(|number| number.parse().ok())
}

fn task_file_duration_seconds(value: &str) -> Option<u64> {
    value.split(':').try_fold(0_u64, |total, part| {
        part.parse::<u64>()
            .ok()
            .and_then(|part| total.checked_mul(60)?.checked_add(part))
    })
}

fn compare_task_file_values(left: &str, right: &str) -> Ordering {
    match (left == "-", right == "-") {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => left.to_lowercase().cmp(&right.to_lowercase()),
    }
}

fn compare_task_file_numbers(left: &str, right: &str) -> Ordering {
    match (task_file_number(left), task_file_number(right)) {
        (Some(left), Some(right)) => left.total_cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_task_file_durations(left: &str, right: &str) -> Ordering {
    match (
        task_file_duration_seconds(left),
        task_file_duration_seconds(right),
    ) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_task_files(
    left: &SlintTaskFileSnapshot,
    right: &SlintTaskFileSnapshot,
    column: usize,
) -> Ordering {
    match column {
        1 => compare_task_file_values(&left.status, &right.status),
        2 => compare_task_file_values(&left.started_time, &right.started_time),
        3 => compare_task_file_values(&left.completed_time, &right.completed_time),
        4 => compare_task_file_numbers(&left.conversion_time, &right.conversion_time),
        5 => compare_task_file_numbers(&left.original_size, &right.original_size),
        6 => compare_task_file_numbers(&left.new_size, &right.new_size),
        7 => compare_task_file_numbers(&left.original_fps, &right.original_fps),
        8 => compare_task_file_numbers(&left.new_fps, &right.new_fps),
        9 => compare_task_file_values(&left.codec, &right.codec),
        10 => compare_task_file_durations(&left.duration, &right.duration),
        _ => Ordering::Equal,
    }
}

fn sorted_task_files(
    files: &[SlintTaskFileSnapshot],
    sort: TaskFileSort,
) -> Vec<SlintTaskFileSnapshot> {
    let mut files = files.to_vec();
    if sort.column == 0 {
        if sort.descending {
            files.reverse();
        }
    } else {
        files.sort_by(|left, right| {
            let order = compare_task_files(left, right, sort.column);
            if sort.descending {
                order.reverse()
            } else {
                order
            }
        });
    }
    files
}

fn refresh_task_file_rows(ui: &AppWindow, files: &[SlintTaskFileSnapshot], sort: TaskFileSort) {
    let files = sorted_task_files(files, sort);
    let (path_rows, metric_rows) = task_file_table_rows(&files);
    ui.set_selected_task_path_rows(model(path_rows));
    ui.set_selected_task_metric_rows(model(metric_rows));
}

fn refresh(ui: &AppWindow, snapshot: &SlintAppSnapshot, task_file_sort: TaskFileSort) {
    let tasks = snapshot.tasks.iter().map(task_item).collect::<Vec<_>>();
    let history = snapshot
        .history
        .iter()
        .map(|item| HistoryItem {
            title: SharedString::from(&item.title),
            path: SharedString::from(&item.path),
            subtitle: SharedString::from(&item.subtitle),
            detail: SharedString::from(&item.detail),
            configuration: SharedString::from(&item.configuration),
        })
        .collect::<Vec<_>>();

    ui.set_tasks(model(tasks));
    ui.set_selected_task_title(SharedString::from(&snapshot.selected_task_title));
    refresh_task_file_rows(ui, &snapshot.selected_task_files, task_file_sort);
    ui.set_history(model(history));
    ui.set_mode_labels(model(strings(&snapshot.settings.mode_labels)));
    ui.set_encoder_labels(model(strings(&snapshot.settings.encoder_labels)));
    ui.set_mode_index(i32::try_from(snapshot.settings.mode_index).unwrap_or_default());
    ui.set_encoder_index(i32::try_from(snapshot.settings.encoder_index).unwrap_or_default());
    ui.set_show_encoder(snapshot.settings.show_encoder);
    ui.set_fps_labels(model(strings(&snapshot.settings.fps_labels)));
    ui.set_fps_index(i32::try_from(snapshot.settings.fps_index).unwrap_or_default());
    ui.set_show_fps(snapshot.settings.show_fps);
    ui.set_explicit_fps(snapshot.settings.explicit_fps);
    ui.set_show_explicit_fps(snapshot.settings.show_explicit_fps);
    ui.set_quality(snapshot.settings.quality);
    ui.set_show_quality(snapshot.settings.show_quality);
    ui.set_quality_level(SharedString::from(&snapshot.settings.quality_level));
    ui.set_quality_guide_labels(model(strings(&snapshot.settings.quality_guide_labels)));
    ui.set_quality_maximum(snapshot.settings.quality_maximum);
    ui.set_speed(SharedString::from(&snapshot.settings.speed));
    ui.set_speed_labels(model(strings(&snapshot.settings.speed_labels)));
    ui.set_speed_index(i32::try_from(snapshot.settings.speed_index).unwrap_or_default());
    ui.set_show_speed(snapshot.settings.show_speed);
    ui.set_prevent_sleep(snapshot.settings.prevent_sleep);
    ui.set_trim_start(SharedString::from(&snapshot.settings.trim_start));
    ui.set_trim_end(SharedString::from(&snapshot.settings.trim_end));
    ui.set_apply_lut(snapshot.settings.apply_lut);
    ui.set_stabilize_index(i32::try_from(snapshot.settings.stabilize_index).unwrap_or_default());
    ui.set_slideshow_interval(snapshot.settings.slideshow_interval);
    ui.set_slideshow_fps(snapshot.settings.slideshow_fps);
    ui.set_slideshow_resolution_index(
        i32::try_from(snapshot.settings.slideshow_resolution_index).unwrap_or_default(),
    );
    ui.set_slideshow_collage(snapshot.settings.slideshow_collage);
    ui.set_slideshow_audio_labels(model(strings(&snapshot.settings.slideshow_audio_labels)));
    ui.set_slideshow_audio_selected(snapshot.settings.slideshow_audio_selected);
    ui.set_activity(SharedString::from(&snapshot.activity));
    ui.set_engine_status(SharedString::from(&snapshot.engine_status));
    ui.set_active_title(SharedString::from(&snapshot.active_title));
    ui.set_active_detail(SharedString::from(&snapshot.active_detail));
    ui.set_live_position(SharedString::from(&snapshot.live_status.position));
    ui.set_live_duration(SharedString::from(&snapshot.live_status.duration));
    ui.set_live_frames(SharedString::from(&snapshot.live_status.frames));
    ui.set_live_conversion_fps(SharedString::from(&snapshot.live_status.conversion_fps));
    ui.set_live_conversion_speed(SharedString::from(&snapshot.live_status.conversion_speed));
    ui.set_live_original_fps(SharedString::from(&snapshot.live_status.original_fps));
    ui.set_live_target_fps(SharedString::from(&snapshot.live_status.target_fps));
    ui.set_live_encoder(SharedString::from(&snapshot.live_status.encoder));
    ui.set_live_quality(SharedString::from(&snapshot.live_status.quality));
    ui.set_live_preset(SharedString::from(&snapshot.live_status.preset));
    ui.set_live_carried_audio(SharedString::from(&snapshot.live_status.carried_audio));
    ui.set_live_carried_subtitles(SharedString::from(&snapshot.live_status.carried_subtitles));
    ui.set_live_spent(SharedString::from(&snapshot.live_status.spent));
    ui.set_live_estimated_total(SharedString::from(&snapshot.live_status.estimated_total));
    ui.set_live_remaining(SharedString::from(&snapshot.live_status.remaining));
    ui.set_progress(snapshot.progress);
    ui.set_is_running(snapshot.is_running);
    ui.set_is_paused(snapshot.is_paused);
    ui.set_paused_between_files(snapshot.paused_between_files);
    ui.set_selected_can_move(snapshot.selected_can_move);
    ui.set_preview_enabled(snapshot.preview_enabled);
    ui.set_has_live_preview(snapshot.live_preview.is_some());
    ui.set_live_preview(
        snapshot
            .live_preview
            .as_ref()
            .map_or_else(Image::default, |preview| {
                Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                    &preview.rgba,
                    preview.width,
                    preview.height,
                ))
            }),
    );
    ui.set_pending_count(i32::try_from(snapshot.pending_count).unwrap_or(i32::MAX));
    ui.set_completed_count(i32::try_from(snapshot.completed_count).unwrap_or(i32::MAX));
    ui.set_attention_count(i32::try_from(snapshot.attention_count).unwrap_or(i32::MAX));
    ui.set_history_total_count(i32::try_from(snapshot.history_total_count).unwrap_or(i32::MAX));
    ui.set_pause_after_scheduled(snapshot.pause_after_current);
    ui.set_completion_notice(SharedString::from(&snapshot.completion_notice));
    ui.set_selected_index(snapshot.selected_index);
    ui.set_task_draft_targets(model(strings(&snapshot.task_draft_targets)));
    ui.set_task_draft_summary(SharedString::from(&snapshot.task_draft_summary));
    ui.set_task_draft_output_summary(SharedString::from(&snapshot.task_draft_output_summary));
}

fn strings(values: &[String]) -> Vec<SharedString> {
    values.iter().map(SharedString::from).collect()
}

fn model<T: Clone + 'static>(values: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(values)))
}

#[cfg(test)]
mod tests {
    use super::{SlintTaskFileSnapshot, TaskFileSort, sorted_task_files};

    fn task_file(path: &str, size: &str, duration: &str) -> SlintTaskFileSnapshot {
        SlintTaskFileSnapshot {
            title: path.to_owned(),
            path: path.to_owned(),
            status: "Completed".to_owned(),
            started_time: "-".to_owned(),
            completed_time: "-".to_owned(),
            conversion_time: "-".to_owned(),
            original_size: size.to_owned(),
            new_size: "-".to_owned(),
            original_fps: "-".to_owned(),
            new_fps: "-".to_owned(),
            codec: "x265".to_owned(),
            duration: duration.to_owned(),
        }
    }

    #[test]
    fn task_file_numeric_columns_sort_by_value() {
        let files = vec![
            task_file("file-2.mp4", "9.5 MB", "09:30"),
            task_file("file-10.mp4", "100 MB", "01:02:03"),
            task_file("file-20.mp4", "12 MB", "10:00"),
        ];

        let by_size = sorted_task_files(
            &files,
            TaskFileSort {
                column: 5,
                descending: false,
            },
        );
        assert_eq!(
            by_size
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["file-2.mp4", "file-20.mp4", "file-10.mp4"]
        );

        let by_duration = sorted_task_files(
            &files,
            TaskFileSort {
                column: 10,
                descending: true,
            },
        );
        assert_eq!(
            by_duration
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["file-10.mp4", "file-20.mp4", "file-2.mp4"]
        );
    }
}
