// This is a desktop GUI in both debug and release builds.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    clipboard_assistant_lib::run()
}
