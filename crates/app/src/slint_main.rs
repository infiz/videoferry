#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
// Slint's generated item tree contains audited framework internals that opt
// themselves out locally; handwritten frontend code remains safe-only.
#![deny(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};
use slint::{
    CloseRequestResponse, ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer,
    SharedString, Timer, TimerMode, VecModel,
};
use videoferry_app::legacy::{SlintAppSnapshot, SlintController, SlintTaskSnapshot};

slint::include_modules!();

const CONTROLLER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CONVERSION_STATUS_INTERVAL: Duration = Duration::from_secs(1);
const PRODUCT_PAGE_URL: &str = "https://apps.infiz.com/videoferry";

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

    wire_callbacks(&ui, &controller);
    wire_native_file_drop(&ui, &controller);
    let initial_snapshot = controller.borrow().snapshot();
    refresh(&ui, &initial_snapshot);

    let timer = Timer::default();
    let weak_ui = ui.as_weak();
    let timer_controller = Rc::clone(&controller);
    let last_snapshot = Rc::new(RefCell::new(initial_snapshot));
    let timer_snapshot = Rc::clone(&last_snapshot);
    let next_status_refresh = Rc::new(RefCell::new(Instant::now()));
    let timer_status_refresh = Rc::clone(&next_status_refresh);
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
        if snapshot != *timer_snapshot.borrow() {
            refresh(&ui, &snapshot);
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

fn wire_callbacks(ui: &AppWindow, controller: &Rc<RefCell<SlintController>>) {
    ui.on_open_product_page(|| {
        let _result = open_product_page();
    });
    wire_queue_callbacks(ui, controller);
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

fn wire_queue_callbacks(ui: &AppWindow, controller: &Rc<RefCell<SlintController>>) {
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

    let speed_controller = Rc::clone(controller);
    ui.on_cycle_speed(move || speed_controller.borrow_mut().cycle_speed());

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
    }
}

fn refresh(ui: &AppWindow, snapshot: &SlintAppSnapshot) {
    let tasks = snapshot.tasks.iter().map(task_item).collect::<Vec<_>>();
    let history = snapshot
        .history
        .iter()
        .map(|item| HistoryItem {
            title: SharedString::from(&item.title),
            subtitle: SharedString::from(&item.subtitle),
            detail: SharedString::from(&item.detail),
            configuration: SharedString::from(&item.configuration),
        })
        .collect::<Vec<_>>();

    ui.set_tasks(model(tasks));
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
    ui.set_speed(SharedString::from(&snapshot.settings.speed));
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
    ui.set_selected_index(snapshot.selected_index);
    ui.set_task_draft_targets(model(strings(&snapshot.task_draft_targets)));
    ui.set_task_draft_summary(SharedString::from(&snapshot.task_draft_summary));
}

fn strings(values: &[String]) -> Vec<SharedString> {
    values.iter().map(SharedString::from).collect()
}

fn model<T: Clone + 'static>(values: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(values)))
}
