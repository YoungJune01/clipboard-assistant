pub mod clipboard;
mod message_loop;
pub mod monitor;

use windows::Win32::{
    Foundation::{GetLastError, HWND, WIN32_ERROR},
    UI::{
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        WindowsAndMessaging::{
            GWL_EXSTYLE, GetWindowLongW, SetWindowLongW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
        },
    },
};

pub fn enable_per_monitor_v2() -> windows::core::Result<()> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
}

pub fn configure_quick_panel_style(hwnd: HWND) -> windows::core::Result<()> {
    let current = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) };
    let desired = (current as u32 | WS_EX_TOOLWINDOW.0) & !WS_EX_APPWINDOW.0;
    if desired == current as u32 {
        return Ok(());
    }
    unsafe { windows::Win32::Foundation::SetLastError(WIN32_ERROR(0)) };
    let previous = unsafe { SetWindowLongW(hwnd, GWL_EXSTYLE, desired as i32) };
    if previous == 0 && unsafe { GetLastError() }.0 != 0 {
        Err(windows::core::Error::from_win32())
    } else {
        Ok(())
    }
}
