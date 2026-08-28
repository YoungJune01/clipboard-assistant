pub mod activation;
pub mod clipboard;
pub mod credentials;
pub mod hotkey;
mod message_loop;
pub mod monitor;
pub mod ocr;
pub mod paste;
pub mod sound;
pub mod startup;

use windows::Win32::{
    Foundation::{GetLastError, HWND, WIN32_ERROR},
    UI::WindowsAndMessaging::{
        GUI_INMOVESIZE, GUITHREADINFO, GWL_EXSTYLE, GetGUIThreadInfo, GetWindowLongW,
        GetWindowThreadProcessId, SetWindowLongW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    },
};

use std::mem::size_of;

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

pub fn is_window_in_move_or_size(hwnd: HWND) -> bool {
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, None) };
    if thread_id == 0 {
        return false;
    }
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetGUIThreadInfo(thread_id, &mut info) }.is_ok()
        && info.flags.0 & GUI_INMOVESIZE.0 != 0
}
