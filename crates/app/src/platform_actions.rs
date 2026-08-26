use std::io;
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
pub(crate) fn open_path_with_default_app(path: &Path) -> io::Result<()> {
    Command::new("explorer.exe")
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(drop)
}

#[cfg(target_os = "macos")]
pub(crate) fn open_path_with_default_app(path: &Path) -> io::Result<()> {
    Command::new("open").arg(path).spawn().map(drop)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn open_path_with_default_app(path: &Path) -> io::Result<()> {
    Command::new("xdg-open").arg(path).spawn().map(drop)
}

#[cfg(target_os = "windows")]
pub(crate) fn reveal_path_in_file_manager(path: &Path) -> io::Result<()> {
    Command::new("explorer.exe")
        .arg("/select,")
        .arg(path)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(drop)
}

#[cfg(target_os = "macos")]
pub(crate) fn reveal_path_in_file_manager(path: &Path) -> io::Result<()> {
    Command::new("open").arg("-R").arg(path).spawn().map(drop)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) fn reveal_path_in_file_manager(path: &Path) -> io::Result<()> {
    Command::new("xdg-open")
        .arg(path.parent().unwrap_or_else(|| Path::new(".")))
        .spawn()
        .map(drop)
}
