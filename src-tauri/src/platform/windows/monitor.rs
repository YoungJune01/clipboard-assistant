use std::{
    error::Error,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
};

use windows::{
    Win32::{
        Foundation::{GetLastError, LPARAM, POINT},
        Graphics::Gdi::{
            EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST,
            MONITORINFO, MONITORINFOEXW, MonitorFromPoint,
        },
        UI::{
            HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            WindowsAndMessaging::{GetCursorPos, MONITORINFOF_PRIMARY},
        },
    },
    core::BOOL,
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorIdentity(String);

impl MonitorIdentity {
    pub fn from_static(identity: &'static str) -> Self {
        Self(identity.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_device_name(device_name: &[u16]) -> Self {
        let length = device_name
            .iter()
            .position(|character| *character == 0)
            .unwrap_or(device_name.len());
        Self(String::from_utf16_lossy(&device_name[..length]))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorSnapshot {
    pub identity: MonitorIdentity,
    pub pointer: PhysicalPoint,
    pub work_area: PhysicalRect,
    pub dpi: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitorDetails {
    identity: MonitorIdentity,
    work_area: PhysicalRect,
    primary: bool,
}

trait MonitorApi {
    type Monitor: Copy;

    fn cursor_position(&self) -> Option<PhysicalPoint>;
    fn monitor_from_point(&self, pointer: PhysicalPoint) -> Option<Self::Monitor>;
    fn monitor_details(&self, monitor: Self::Monitor) -> Option<MonitorDetails>;
    fn monitor_dpi(&self, monitor: Self::Monitor) -> Option<u32>;
    fn enumerate_monitors(&self) -> Vec<Self::Monitor>;
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
    query_monitor_snapshot(&Win32MonitorApi)
}

pub fn snapshot_for_point(pointer: PhysicalPoint) -> Result<MonitorSnapshot, MonitorError> {
    query_snapshot_for_pointer(&Win32MonitorApi, pointer)
}

pub fn snapshot_for_identity(
    identity: &MonitorIdentity,
    anchor: PhysicalPoint,
) -> Result<MonitorSnapshot, MonitorError> {
    query_snapshot_for_identity(&Win32MonitorApi, identity, anchor)
}

#[cfg(test)]
pub(crate) fn fallback_monitor_snapshot_for_test() -> Result<MonitorSnapshot, MonitorError> {
    fallback_snapshot(&Win32MonitorApi, None)
}

fn query_monitor_snapshot<A: MonitorApi>(api: &A) -> Result<MonitorSnapshot, MonitorError> {
    match api.cursor_position() {
        Some(pointer) => query_snapshot_for_pointer(api, pointer),
        None => fallback_snapshot(api, None),
    }
}

fn query_snapshot_for_pointer<A: MonitorApi>(
    api: &A,
    pointer: PhysicalPoint,
) -> Result<MonitorSnapshot, MonitorError> {
    if let Some(monitor) = api.monitor_from_point(pointer)
        && let Some(details) = api.monitor_details(monitor)
        && valid_work_area(details.work_area)
    {
        return Ok(snapshot_from_monitor(api, monitor, details, pointer));
    }
    fallback_snapshot(api, Some(pointer))
}

fn query_snapshot_for_identity<A: MonitorApi>(
    api: &A,
    identity: &MonitorIdentity,
    anchor: PhysicalPoint,
) -> Result<MonitorSnapshot, MonitorError> {
    for monitor in api.enumerate_monitors() {
        let Some(details) = api.monitor_details(monitor) else {
            continue;
        };
        if details.identity == *identity && valid_work_area(details.work_area) {
            let pointer = fallback_pointer(Some(anchor), details.work_area);
            return Ok(snapshot_from_monitor(api, monitor, details, pointer));
        }
    }
    fallback_snapshot(api, Some(anchor))
}

fn fallback_snapshot<A: MonitorApi>(
    api: &A,
    pointer: Option<PhysicalPoint>,
) -> Result<MonitorSnapshot, MonitorError> {
    let mut fallback = None;
    for monitor in api.enumerate_monitors() {
        let Some(details) = api.monitor_details(monitor) else {
            continue;
        };
        if !valid_work_area(details.work_area) {
            continue;
        }
        if details.primary {
            let fallback_pointer = fallback_pointer(pointer, details.work_area);
            return Ok(snapshot_from_monitor(
                api,
                monitor,
                details,
                fallback_pointer,
            ));
        }
        fallback.get_or_insert((monitor, details));
    }
    let (monitor, details) = fallback.ok_or(MonitorError::MonitorUnavailable)?;
    let fallback_pointer = fallback_pointer(pointer, details.work_area);
    Ok(snapshot_from_monitor(
        api,
        monitor,
        details,
        fallback_pointer,
    ))
}

fn snapshot_from_monitor<A: MonitorApi>(
    api: &A,
    monitor: A::Monitor,
    details: MonitorDetails,
    pointer: PhysicalPoint,
) -> MonitorSnapshot {
    MonitorSnapshot {
        identity: details.identity,
        pointer,
        work_area: details.work_area,
        dpi: api.monitor_dpi(monitor).unwrap_or(DEFAULT_DPI),
    }
}

fn valid_work_area(work_area: PhysicalRect) -> bool {
    work_area.width() > 0 && work_area.height() > 0
}

fn fallback_pointer(pointer: Option<PhysicalPoint>, work_area: PhysicalRect) -> PhysicalPoint {
    let max_x = work_area.right.saturating_sub(1);
    let max_y = work_area.bottom.saturating_sub(1);
    match pointer {
        Some(pointer) => PhysicalPoint {
            x: pointer.x.clamp(work_area.left, max_x),
            y: pointer.y.clamp(work_area.top, max_y),
        },
        None => PhysicalPoint {
            x: (i64::from(work_area.left) + i64::from(work_area.width()) / 2) as i32,
            y: (i64::from(work_area.top) + i64::from(work_area.height()) / 2) as i32,
        },
    }
}

struct Win32MonitorApi;

impl MonitorApi for Win32MonitorApi {
    type Monitor = HMONITOR;

    fn cursor_position(&self) -> Option<PhysicalPoint> {
        let mut pointer = POINT::default();
        unsafe { GetCursorPos(&mut pointer) }.ok()?;
        Some(PhysicalPoint {
            x: pointer.x,
            y: pointer.y,
        })
    }

    fn monitor_from_point(&self, pointer: PhysicalPoint) -> Option<Self::Monitor> {
        let monitor = unsafe {
            MonitorFromPoint(
                POINT {
                    x: pointer.x,
                    y: pointer.y,
                },
                MONITOR_DEFAULTTONEAREST,
            )
        };
        (!monitor.is_invalid()).then_some(monitor)
    }

    fn monitor_details(&self, monitor: Self::Monitor) -> Option<MonitorDetails> {
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            return None;
        }
        Some(MonitorDetails {
            identity: MonitorIdentity::from_device_name(&info.szDevice),
            work_area: PhysicalRect {
                left: info.monitorInfo.rcWork.left,
                top: info.monitorInfo.rcWork.top,
                right: info.monitorInfo.rcWork.right,
                bottom: info.monitorInfo.rcWork.bottom,
            },
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        })
    }

    fn monitor_dpi(&self, monitor: Self::Monitor) -> Option<u32> {
        let mut dpi_x = DEFAULT_DPI;
        let mut dpi_y = DEFAULT_DPI;
        if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_ok()
            && dpi_x > 0
        {
            Some(dpi_x)
        } else {
            let _ = unsafe { GetLastError() };
            None
        }
    }

    fn enumerate_monitors(&self) -> Vec<Self::Monitor> {
        let mut monitors = Vec::new();
        let data = LPARAM((&mut monitors as *mut Vec<HMONITOR>) as isize);
        let succeeded = unsafe { EnumDisplayMonitors(None, None, Some(collect_monitor), data) };
        if succeeded.as_bool() {
            monitors
        } else {
            Vec::new()
        }
    }
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _device_context: HDC,
    _monitor_rect: *mut windows::Win32::Foundation::RECT,
    data: LPARAM,
) -> BOOL {
    let collected = catch_unwind(AssertUnwindSafe(|| {
        let monitors = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
        monitors.push(monitor);
    }));
    BOOL::from(collected.is_ok())
}

fn dip_to_physical(dip: u32, dpi: u32) -> i32 {
    let physical =
        (u64::from(dip) * u64::from(dpi) + u64::from(DEFAULT_DPI / 2)) / u64::from(DEFAULT_DPI);
    physical.min(i32::MAX as u64) as i32
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use super::*;

    #[test]
    fn stale_point_monitor_falls_back_to_fresh_primary_monitor() {
        let api = FakeMonitorApi::new(
            Some(PhysicalPoint { x: 2500, y: 400 }),
            Some(7),
            vec![(
                11,
                MonitorDetails {
                    identity: MonitorIdentity::from_static("primary"),
                    work_area: PhysicalRect {
                        left: 0,
                        top: 0,
                        right: 1920,
                        bottom: 1040,
                    },
                    primary: true,
                },
            )],
            vec![(7, None), (11, Some(DEFAULT_DPI))],
        );

        let snapshot = query_monitor_snapshot(&api).expect("fallback monitor snapshot");

        assert_eq!(snapshot.pointer, PhysicalPoint { x: 1919, y: 400 });
        assert_eq!(
            snapshot.work_area,
            PhysicalRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040
            }
        );
        assert_eq!(snapshot.dpi, DEFAULT_DPI);
        assert_eq!(api.info_queries(), vec![7, 11]);
        assert_eq!(api.enumeration_count(), 1);
    }

    #[test]
    fn cursor_failure_uses_center_of_an_enumerated_valid_monitor() {
        let api = FakeMonitorApi::new(
            None,
            None,
            vec![(
                3,
                MonitorDetails {
                    identity: MonitorIdentity::from_static("fallback"),
                    work_area: PhysicalRect {
                        left: -1600,
                        top: -200,
                        right: 0,
                        bottom: 700,
                    },
                    primary: false,
                },
            )],
            vec![(3, Some(144))],
        );

        let snapshot = query_monitor_snapshot(&api).expect("enumerated fallback snapshot");

        assert_eq!(snapshot.pointer, PhysicalPoint { x: -800, y: 250 });
        assert_eq!(snapshot.dpi, 144);
        assert!(snapshot.pointer.x >= snapshot.work_area.left);
        assert!(snapshot.pointer.x < snapshot.work_area.right);
        assert!(snapshot.pointer.y >= snapshot.work_area.top);
        assert!(snapshot.pointer.y < snapshot.work_area.bottom);
        assert_eq!(api.enumeration_count(), 1);
    }

    #[test]
    fn identity_query_stays_on_owner_until_owner_is_removed() {
        let owner = MonitorIdentity::from_static("A");
        let api = FakeMonitorApi::new(
            Some(PhysicalPoint { x: 2500, y: 100 }),
            Some(2),
            vec![
                (
                    1,
                    MonitorDetails {
                        identity: owner.clone(),
                        work_area: PhysicalRect {
                            left: 0,
                            top: 40,
                            right: 1920,
                            bottom: 1000,
                        },
                        primary: true,
                    },
                ),
                (
                    2,
                    MonitorDetails {
                        identity: MonitorIdentity::from_static("B"),
                        work_area: PhysicalRect {
                            left: 1920,
                            top: 0,
                            right: 3840,
                            bottom: 1040,
                        },
                        primary: false,
                    },
                ),
            ],
            vec![(1, Some(144))],
        );

        let snapshot =
            query_snapshot_for_identity(&api, &owner, PhysicalPoint { x: 1800, y: 1000 })
                .expect("owner monitor snapshot");

        assert_eq!(snapshot.identity, owner);
        assert_eq!(snapshot.pointer, PhysicalPoint { x: 1800, y: 999 });
        assert_eq!(snapshot.dpi, 144);
        assert_eq!(api.info_queries(), vec![1]);
    }

    struct FakeMonitorApi {
        cursor: Option<PhysicalPoint>,
        point_monitor: Option<usize>,
        monitors: Vec<(usize, MonitorDetails)>,
        detail_results: Mutex<VecDeque<(usize, Option<MonitorDetails>)>>,
        dpi_by_monitor: Vec<(usize, u32)>,
        info_queries: Mutex<Vec<usize>>,
        enumeration_count: Mutex<usize>,
    }

    impl FakeMonitorApi {
        fn new(
            cursor: Option<PhysicalPoint>,
            point_monitor: Option<usize>,
            monitors: Vec<(usize, MonitorDetails)>,
            results: Vec<(usize, Option<u32>)>,
        ) -> Self {
            let detail_results = results
                .iter()
                .map(|(monitor, dpi)| {
                    let details = dpi.and_then(|_| {
                        monitors.iter().find_map(|(candidate, details)| {
                            (*candidate == *monitor).then_some(details.clone())
                        })
                    });
                    (*monitor, details)
                })
                .collect();
            let dpi_by_monitor = results
                .into_iter()
                .filter_map(|(monitor, dpi)| dpi.map(|dpi| (monitor, dpi)))
                .collect();
            Self {
                cursor,
                point_monitor,
                monitors,
                detail_results: Mutex::new(detail_results),
                dpi_by_monitor,
                info_queries: Mutex::new(Vec::new()),
                enumeration_count: Mutex::new(0),
            }
        }

        fn info_queries(&self) -> Vec<usize> {
            self.info_queries.lock().unwrap().clone()
        }

        fn enumeration_count(&self) -> usize {
            *self.enumeration_count.lock().unwrap()
        }
    }

    impl MonitorApi for FakeMonitorApi {
        type Monitor = usize;

        fn cursor_position(&self) -> Option<PhysicalPoint> {
            self.cursor
        }

        fn monitor_from_point(&self, _pointer: PhysicalPoint) -> Option<Self::Monitor> {
            self.point_monitor
        }

        fn monitor_details(&self, monitor: Self::Monitor) -> Option<MonitorDetails> {
            self.info_queries.lock().unwrap().push(monitor);
            let (expected, details) = self
                .detail_results
                .lock()
                .unwrap()
                .pop_front()
                .expect("configured monitor info result");
            assert_eq!(monitor, expected);
            details
        }

        fn monitor_dpi(&self, monitor: Self::Monitor) -> Option<u32> {
            self.dpi_by_monitor
                .iter()
                .find_map(|(candidate, dpi)| (*candidate == monitor).then_some(*dpi))
        }

        fn enumerate_monitors(&self) -> Vec<Self::Monitor> {
            *self.enumeration_count.lock().unwrap() += 1;
            self.monitors.iter().map(|(monitor, _)| *monitor).collect()
        }
    }
}
