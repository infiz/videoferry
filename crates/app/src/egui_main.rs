#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#![forbid(unsafe_code)]

fn main() -> eframe::Result<()> {
    videoferry_app::legacy::run()
}
