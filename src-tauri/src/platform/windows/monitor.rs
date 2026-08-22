use std::{error::Error, fmt};

use windows::Win32::{
    Foundation::{GetLastError, POINT},
    Graphics::Gdi::{
        GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
    },
    UI::{
        HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
        WindowsAndMessaging::GetCursorPos,
    },
};

pub const DEFAULT_DPI: u32 = 96;
pub const POINTER_GAP_PX: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl PhysicalRect {
    pub fn width(self) -> i32 {
        self.right.saturating_sub(self.left)
    }

    pub fn height(self) -> i32 {
        self.bottom.saturating_sub(self.top)
    }

    pub fn contains_rect(self, other: Self) -> bool {
        other.left >= self.left
            && other.top >= self.top
            && other.right <= self.right
            && other.bottom <= self.bottom
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DipSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorSnapshot {
    pub pointer: PhysicalPoint,
    pub work_area: PhysicalRect,
    pub dpi: u32,
}

#[derive(Debug)]
pub enum MonitorError {
    Cursor(windows::core::Error),
    MonitorUnavailable,
    MonitorInfo(windows::core::Error),
    InvalidWorkArea,
}

impl fmt::Display for MonitorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cursor(_) => formatter.write_str("could not read the pointer position"),
            Self::MonitorUnavailable => formatter.write_str("no monitor is available"),
            Self::MonitorInfo(_) => formatter.write_str("could not read the monitor work area"),
            Self::InvalidWorkArea => formatter.write_str("the monitor work area is invalid"),
        }
    }
}

impl Error for MonitorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cursor(error) | Self::MonitorInfo(error) => Some(error),
            Self::MonitorUnavailable | Self::InvalidWorkArea => None,
        }
    }
}

pub fn place_panel(
    pointer: PhysicalPoint,
    work_area: PhysicalRect,
    panel_dip_size: DipSize,
    dpi: u32,
) -> PhysicalRect {
    let dpi = dpi.max(DEFAULT_DPI);
    let work_width = work_area.width().max(0);
    let work_height = work_area.height().max(0);
    let width = dip_to_physical(panel_dip_size.width, dpi).min(work_width);
    let height = dip_to_physical(panel_dip_size.height, dpi).min(work_height);

    let right_origin = pointer.x.saturating_add(POINTER_GAP_PX);
    let left_origin = pointer
        .x
        .saturating_sub(POINTER_GAP_PX)
        .saturating_sub(width);
    let down_origin = pointer.y.saturating_add(POINTER_GAP_PX);
    let up_origin = pointer
        .y
        .saturating_sub(POINTER_GAP_PX)
        .saturating_sub(height);

    let preferred_x = if right_origin.saturating_add(width) <= work_area.right {
        right_origin
    } else {
        left_origin
    };
    let preferred_y = if down_origin.saturating_add(height) <= work_area.bottom {
        down_origin
    } else {
        up_origin
    };
    let max_x = work_area.right.saturating_sub(width);
    let max_y = work_area.bottom.saturating_sub(height);
    let left = preferred_x.clamp(work_area.left, max_x);
    let top = preferred_y.clamp(work_area.top, max_y);

    PhysicalRect {
        left,
        top,
        right: left.saturating_add(width),
        bottom: top.saturating_add(height),
    }
}

pub fn current_monitor_snapshot() -> Result<MonitorSnapshot, MonitorError> {
    let mut pointer = POINT::default();
    unsafe { GetCursorPos(&mut pointer) }.map_err(MonitorError::Cursor)?;
    snapshot_for_point(PhysicalPoint {
        x: pointer.x,
        y: pointer.y,
    })
}

pub fn snapshot_for_point(pointer: PhysicalPoint) -> Result<MonitorSnapshot, MonitorError> {
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: pointer.x,
                y: pointer.y,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    if monitor.is_invalid() {
        return Err(MonitorError::MonitorUnavailable);
    }
    monitor_snapshot(pointer, monitor)
}

fn monitor_snapshot(
    pointer: PhysicalPoint,
    monitor: HMONITOR,
) -> Result<MonitorSnapshot, MonitorError> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return Err(MonitorError::MonitorInfo(windows::core::Error::from_win32()));
    }
    let work_area = PhysicalRect {
        left: info.rcWork.left,
        top: info.rcWork.top,
        right: info.rcWork.right,
        bottom: info.rcWork.bottom,
    };
    if work_area.width() <= 0 || work_area.height() <= 0 {
        return Err(MonitorError::InvalidWorkArea);
    }

    let mut dpi_x = DEFAULT_DPI;
    let mut dpi_y = DEFAULT_DPI;
    let dpi = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
        .is_ok()
        && dpi_x > 0
    {
        dpi_x
    } else {
        let _ = unsafe { GetLastError() };
        DEFAULT_DPI
    };
    Ok(MonitorSnapshot {
        pointer,
        work_area,
        dpi,
    })
}

fn dip_to_physical(dip: u32, dpi: u32) -> i32 {
    let physical =
        (u64::from(dip) * u64::from(dpi) + u64::from(DEFAULT_DPI / 2)) / u64::from(DEFAULT_DPI);
    physical.min(i32::MAX as u64) as i32
}
