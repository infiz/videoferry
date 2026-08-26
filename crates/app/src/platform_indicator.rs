#[derive(Default)]
pub struct PlatformIndicator {
    converting: Option<bool>,
}

impl PlatformIndicator {
    pub fn set_converting(&mut self, converting: bool) {
        if self.converting == Some(converting) {
            return;
        }
        self.converting = Some(converting);
        set_macos_dock_badge(converting);
    }
}

#[cfg(target_os = "macos")]
fn set_macos_dock_badge(converting: bool) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;
    use objc2_foundation::NSString;

    let Some(main_thread) = MainThreadMarker::new() else {
        return;
    };
    let dock_tile = NSApplication::sharedApplication(main_thread).dockTile();
    if converting {
        let label = NSString::from_str("▶");
        dock_tile.setBadgeLabel(Some(&label));
    } else {
        dock_tile.setBadgeLabel(None);
    }
}

#[cfg(not(target_os = "macos"))]
fn set_macos_dock_badge(_converting: bool) {}
