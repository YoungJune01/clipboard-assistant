use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crate::{
    platform::windows::monitor::{
        DipSize, MonitorIdentity, MonitorSnapshot, PhysicalPoint, PhysicalRect, place_panel,
    },
    services::panel::{
        ObserverAction, PanelError, PanelMonitor, PanelService, PanelWindow,
        observer_action_for_visibility,
    },
};

#[test]
fn placement_is_stable_across_dpi_and_all_quadrants() {
    struct Case {
        name: &'static str,
        pointer: PhysicalPoint,
        work: PhysicalRect,
        size: DipSize,
        dpi: u32,
        expected: PhysicalRect,
    }

    let cases = [
        Case {
            name: "96 dpi lower right",
            pointer: point(100, 100),
            work: rect(0, 0, 1920, 1040),
            size: dip(400, 300),
            dpi: 96,
            expected: rect(101, 101, 501, 401),
        },
        Case {
            name: "120 dpi flips left",
            pointer: point(1800, 100),
            work: rect(0, 0, 1920, 1040),
            size: dip(400, 300),
            dpi: 120,
            expected: rect(1299, 101, 1799, 476),
        },
        Case {
            name: "144 dpi flips up",
            pointer: point(100, 1000),
            work: rect(0, 0, 1920, 1040),
            size: dip(400, 300),
            dpi: 144,
            expected: rect(101, 549, 701, 999),
        },
        Case {
            name: "192 dpi flips both",
            pointer: point(1800, 1000),
            work: rect(0, 0, 1920, 1040),
            size: dip(400, 300),
            dpi: 192,
            expected: rect(999, 399, 1799, 999),
        },
    ];

    for case in cases {
        assert_eq!(
            place_panel(case.pointer, case.work, case.size, case.dpi),
            case.expected,
            "{}",
            case.name
        );
    }
}

#[test]
fn placement_handles_negative_monitors_edges_corners_and_taskbars() {
    let size = dip(300, 200);
    let cases = [
        (
            "left monitor",
            point(-1900, 20),
            rect(-1920, 0, 0, 1080),
            rect(-1899, 21, -1599, 221),
        ),
        (
            "above monitor",
            point(20, -1180),
            rect(0, -1200, 1920, 0),
            rect(21, -1179, 321, -979),
        ),
        (
            "right taskbar",
            point(1500, 500),
            rect(0, 0, 1536, 1080),
            rect(1199, 501, 1499, 701),
        ),
        (
            "left taskbar",
            point(80, 500),
            rect(72, 0, 1920, 1080),
            rect(81, 501, 381, 701),
        ),
        (
            "top taskbar",
            point(500, 40),
            rect(0, 32, 1920, 1080),
            rect(501, 41, 801, 241),
        ),
        (
            "bottom taskbar corner",
            point(1919, 1039),
            rect(0, 0, 1920, 1040),
            rect(1618, 838, 1918, 1038),
        ),
        (
            "top right corner",
            point(1919, 0),
            rect(0, 0, 1920, 1080),
            rect(1618, 1, 1918, 201),
        ),
        (
            "bottom left corner",
            point(0, 1079),
            rect(0, 0, 1920, 1080),
            rect(1, 878, 301, 1078),
        ),
        (
            "top left corner",
            point(0, 0),
            rect(0, 0, 1920, 1080),
            rect(1, 1, 301, 201),
        ),
        (
            "left edge clamp",
            point(0, 500),
            rect(0, 0, 1920, 1080),
            rect(1, 501, 301, 701),
        ),
        (
            "top edge clamp",
            point(500, 0),
            rect(0, 0, 1920, 1080),
            rect(501, 1, 801, 201),
        ),
        (
            "right edge flip",
            point(1919, 500),
            rect(0, 0, 1920, 1080),
            rect(1618, 501, 1918, 701),
        ),
        (
            "bottom edge flip",
            point(500, 1079),
            rect(0, 0, 1920, 1080),
            rect(501, 878, 801, 1078),
        ),
    ];

    for (name, pointer, work, expected) in cases {
        assert_eq!(place_panel(pointer, work, size, 96), expected, "{name}");
    }
}

#[test]
fn oversized_panel_is_reduced_to_work_area_and_invalid_dpi_falls_back() {
    assert_eq!(
        place_panel(
            point(-50, -50),
            rect(-100, -100, 300, 200),
            dip(900, 700),
            0
        ),
        rect(-100, -100, 300, 200)
    );
    assert_eq!(
        place_panel(point(10, 10), rect(0, 0, 100, 80), dip(20, 20), 120),
        rect(11, 11, 36, 36)
    );
}

#[test]
fn service_requeries_monitor_on_show_reposition_and_hot_unplug_fallback() {
    let monitor = FakeMonitor::new(vec![
        snapshot(point(1800, 1000), rect(0, 0, 1920, 1040), 96),
        snapshot(point(-100, 100), rect(-1280, 0, 0, 984), 144),
    ]);
    let window = FakeWindow::default();
    let service = PanelService::new(monitor.clone(), window.clone(), dip(400, 300));

    service.show().unwrap();
    assert_eq!(
        window.calls(),
        vec![
            Call::Bounds(rect(1399, 699, 1799, 999)),
            Call::Show,
            Call::Focus,
        ]
    );

    service.reposition_if_visible().unwrap();
    assert_eq!(monitor.query_count(), 2);
    assert_eq!(window.last_bounds(), Some(rect(-701, 101, -101, 551)));
    assert!(service.is_visible());
}

#[test]
fn visible_panel_stays_on_owner_monitor_when_pointer_moves_elsewhere() {
    let monitor = OwnerAwareMonitor::new(
        snapshot_on("A", point(1800, 1000), rect(0, 0, 1920, 1040), 96),
        vec![
            snapshot_on("A", point(1800, 1000), rect(0, 0, 1920, 1040), 96),
            snapshot_on("A", point(1800, 1000), rect(0, 0, 1920, 1040), 96),
        ],
    );
    let window = FakeWindow::default();
    let service = PanelService::new(monitor.clone(), window.clone(), dip(400, 300));

    service.show().unwrap();
    monitor.set_current(snapshot_on(
        "B",
        point(2500, 100),
        rect(1920, 0, 3840, 1040),
        144,
    ));
    assert!(!service.owner_environment_changed().unwrap());
    service.reposition_if_visible().unwrap();

    assert_eq!(monitor.owner_queries(), vec!["A", "A"]);
    assert_eq!(window.last_bounds(), Some(rect(1399, 699, 1799, 999)));
}

#[test]
fn manual_position_pauses_environment_reposition_until_the_next_show() {
    let monitor = FakeMonitor::new(vec![
        snapshot_on("A", point(1800, 900), rect(0, 0, 1920, 1040), 96),
        snapshot_on("B", point(2500, 500), rect(1920, 0, 3840, 1040), 144),
        snapshot_on("B", point(2500, 500), rect(1920, 0, 3840, 1040), 144),
    ]);
    let window = FakeWindow::default();
    let service = PanelService::new(monitor.clone(), window.clone(), dip(400, 300));

    service.show().unwrap();
    let initial_bounds = window.last_bounds();
    service.begin_dragging();
    service.finish_dragging().unwrap();

    assert!(!service.owner_environment_changed().unwrap());
    service.reposition_if_visible().unwrap();
    assert_eq!(window.last_bounds(), initial_bounds);
    assert_eq!(monitor.query_count(), 1);

    service.hide().unwrap();
    service.show().unwrap();
    assert_eq!(monitor.query_count(), 2);
    assert_ne!(window.last_bounds(), initial_bounds);
}

#[test]
fn native_drag_temporarily_suppresses_focus_loss_then_restores_normal_hiding() {
    let monitor = FakeMonitor::new(vec![snapshot(point(100, 100), rect(0, 0, 1920, 1040), 96)]);
    let window = FakeWindow::default();
    let service = PanelService::new(monitor, window.clone(), dip(400, 300));

    service.show().unwrap();
    service.begin_dragging();
    service.on_focus_changed(false).unwrap();
    assert!(service.is_visible());

    service.finish_dragging().unwrap();
    assert!(service.is_visible());
    service.on_focus_changed(false).unwrap();
    assert!(!service.is_visible());
}

#[test]
fn native_resize_temporarily_suppresses_focus_loss_then_restores_normal_hiding() {
    let monitor = FakeMonitor::new(vec![snapshot(point(100, 100), rect(0, 0, 1920, 1040), 96)]);
    let window = FakeWindow::default();
    let service = PanelService::new(monitor, window.clone(), dip(400, 300));

    service.show().unwrap();
    service.begin_resizing();
    service.on_focus_changed(false).unwrap();
    assert!(service.is_visible());

    service.finish_resizing().unwrap();
    assert!(service.is_visible());
    service.on_focus_changed(false).unwrap();
    assert!(!service.is_visible());
}

#[test]
fn owner_monitor_changes_reposition_with_original_anchor_and_removal_falls_back() {
    let monitor = OwnerAwareMonitor::new(
        snapshot_on("A", point(1800, 1000), rect(0, 0, 1920, 1040), 96),
        vec![
            snapshot_on("A", point(1800, 1000), rect(0, 40, 1920, 1000), 144),
            snapshot_on("A", point(1800, 1000), rect(0, 40, 1920, 1000), 144),
            snapshot_on("B", point(1919, 900), rect(1920, 0, 3840, 1040), 120),
            snapshot_on("B", point(1919, 900), rect(1920, 0, 3840, 1040), 120),
        ],
    );
    let window = FakeWindow::default();
    let service = PanelService::new(monitor.clone(), window.clone(), dip(400, 300));

    service.show().unwrap();
    assert!(service.owner_environment_changed().unwrap());
    service.reposition_if_visible().unwrap();
    assert_eq!(window.last_bounds(), Some(rect(1199, 549, 1799, 999)));

    assert!(service.owner_environment_changed().unwrap());
    service.reposition_if_visible().unwrap();
    assert_eq!(service.owner_identity().as_deref(), Some("B"));
    assert_eq!(window.last_bounds(), Some(rect(1920, 524, 2420, 899)));
    assert_eq!(monitor.owner_queries(), vec!["A", "A", "A", "A"]);
}

#[test]
fn service_toggle_hide_focus_loss_and_focus_domain_are_deterministic() {
    let monitor = FakeMonitor::new(vec![snapshot(point(100, 100), rect(0, 0, 1920, 1040), 96)]);
    let window = FakeWindow::default();
    let service = PanelService::new(monitor, window.clone(), dip(400, 300));

    service.toggle().unwrap();
    service.on_focus_changed(false).unwrap();
    assert!(!service.is_visible());

    service.show().unwrap();
    let domain = service.enter_focus_domain();
    service.on_focus_changed(false).unwrap();
    assert!(service.is_visible());
    drop(domain);
    service.on_focus_changed(false).unwrap();
    assert!(!service.is_visible());

    service.toggle().unwrap();
    service.toggle().unwrap();
    assert!(!service.is_visible());
    assert_eq!(
        window
            .calls()
            .iter()
            .filter(|call| **call == Call::Hide)
            .count(),
        3
    );
}

#[test]
fn failed_monitor_query_does_not_show_or_corrupt_state() {
    let monitor = FakeMonitor::failing();
    let window = FakeWindow::default();
    let service = PanelService::new(monitor, window.clone(), dip(400, 300));

    assert!(matches!(service.show(), Err(PanelError::Monitor(_))));
    assert!(!service.is_visible());
    assert!(window.calls().is_empty());
}

#[test]
fn focus_and_rollback_hide_failures_preserve_visible_state_and_both_errors() {
    let monitor = FakeMonitor::new(vec![snapshot(point(100, 100), rect(0, 0, 1920, 1040), 96)]);
    let window = ScriptedWindow::new(
        vec![Ok(())],
        vec![Err(FakeError("focus failed"))],
        vec![Err(FakeError("rollback hide failed")), Ok(())],
        vec![Ok(false)],
    );
    let service = PanelService::new(monitor, window.clone(), dip(400, 300));

    let error = service.show().expect_err("focus must fail");
    match error {
        PanelError::FocusAndRollback {
            focus,
            rollback_hide,
        } => {
            assert_eq!(focus.to_string(), "focus failed");
            assert_eq!(rollback_hide.to_string(), "rollback hide failed");
        }
        other => panic!("expected combined focus/rollback error, got {other:?}"),
    }
    assert!(service.is_visible());
    assert_eq!(
        observer_action_for_visibility(service.is_visible()),
        ObserverAction::StartOrKeep
    );

    service.toggle().expect("later hide succeeds");
    assert!(!service.is_visible());
    assert_eq!(
        observer_action_for_visibility(service.is_visible()),
        ObserverAction::Stop
    );
    assert_eq!(
        window.calls(),
        vec![
            Call::Bounds(rect(101, 101, 501, 401)),
            Call::Show,
            Call::Focus,
            Call::Hide,
            Call::Hide,
        ]
    );
}

#[test]
fn successful_focus_rollback_restores_hidden_state() {
    let monitor = FakeMonitor::new(vec![snapshot(point(100, 100), rect(0, 0, 1920, 1040), 96)]);
    let window = ScriptedWindow::new(
        vec![Ok(())],
        vec![Err(FakeError("focus failed"))],
        vec![Ok(())],
        vec![Ok(false)],
    );
    let service = PanelService::new(monitor, window, dip(400, 300));

    assert!(matches!(service.show(), Err(PanelError::Window(_))));
    assert!(!service.is_visible());
    assert_eq!(
        observer_action_for_visibility(service.is_visible()),
        ObserverAction::Stop
    );
}

#[test]
fn hide_does_not_report_success_until_the_window_is_actually_invisible() {
    let monitor = FakeMonitor::new(vec![snapshot(point(100, 100), rect(0, 0, 1920, 1040), 96)]);
    let window = ScriptedWindow::new(vec![Ok(())], vec![Ok(())], vec![Ok(())], vec![Ok(true)]);
    let service = PanelService::new(monitor, window, dip(400, 300));

    service.show().unwrap();
    assert!(matches!(service.hide(), Err(PanelError::Window(_))));
    assert!(service.is_visible());
}

#[test]
fn current_cursor_monitor_is_valid_without_showing_a_window() {
    let snapshot = crate::platform::windows::monitor::current_monitor_snapshot()
        .expect("query current cursor monitor");
    assert!(snapshot.work_area.width() > 0);
    assert!(snapshot.work_area.height() > 0);
    assert!(snapshot.dpi >= 96);
    let placed = place_panel(
        snapshot.pointer,
        snapshot.work_area,
        dip(400, 300),
        snapshot.dpi,
    );
    assert!(snapshot.work_area.contains_rect(placed));
}

#[test]
fn enumerated_fallback_monitor_is_valid_without_showing_a_window() {
    let snapshot = crate::platform::windows::monitor::fallback_monitor_snapshot_for_test()
        .expect("enumerate fallback monitor");
    assert!(snapshot.work_area.width() > 0);
    assert!(snapshot.work_area.height() > 0);
    assert!(snapshot.dpi >= 96);
    assert!(snapshot.pointer.x >= snapshot.work_area.left);
    assert!(snapshot.pointer.x < snapshot.work_area.right);
    assert!(snapshot.pointer.y >= snapshot.work_area.top);
    assert!(snapshot.pointer.y < snapshot.work_area.bottom);
    assert!(snapshot.work_area.contains_rect(place_panel(
        snapshot.pointer,
        snapshot.work_area,
        dip(400, 300),
        snapshot.dpi,
    )));
}

#[derive(Clone)]
struct FakeMonitor {
    snapshots: Arc<Mutex<Vec<MonitorSnapshot>>>,
    query_count: Arc<Mutex<usize>>,
    fail: bool,
}

impl FakeMonitor {
    fn new(mut snapshots: Vec<MonitorSnapshot>) -> Self {
        snapshots.reverse();
        Self {
            snapshots: Arc::new(Mutex::new(snapshots)),
            query_count: Arc::new(Mutex::new(0)),
            fail: false,
        }
    }

    fn failing() -> Self {
        Self {
            snapshots: Arc::new(Mutex::new(Vec::new())),
            query_count: Arc::new(Mutex::new(0)),
            fail: true,
        }
    }

    fn query_count(&self) -> usize {
        *self.query_count.lock().unwrap()
    }
}

impl PanelMonitor for FakeMonitor {
    type Error = FakeError;

    fn snapshot(&self) -> Result<MonitorSnapshot, Self::Error> {
        *self.query_count.lock().unwrap() += 1;
        if self.fail {
            return Err(FakeError("monitor unavailable"));
        }
        let mut snapshots = self.snapshots.lock().unwrap();
        let snapshot = snapshots.pop().expect("configured monitor snapshot");
        if snapshots.is_empty() {
            snapshots.push(snapshot.clone());
        }
        Ok(snapshot)
    }

    fn snapshot_for_owner(
        &self,
        _identity: &MonitorIdentity,
        _anchor: PhysicalPoint,
    ) -> Result<MonitorSnapshot, Self::Error> {
        self.snapshot()
    }
}

#[derive(Clone)]
struct OwnerAwareMonitor {
    current: Arc<Mutex<MonitorSnapshot>>,
    owner_snapshots: Arc<Mutex<VecDeque<MonitorSnapshot>>>,
    owner_queries: Arc<Mutex<Vec<String>>>,
}

impl OwnerAwareMonitor {
    fn new(current: MonitorSnapshot, owner_snapshots: Vec<MonitorSnapshot>) -> Self {
        Self {
            current: Arc::new(Mutex::new(current)),
            owner_snapshots: Arc::new(Mutex::new(owner_snapshots.into())),
            owner_queries: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn set_current(&self, snapshot: MonitorSnapshot) {
        *self.current.lock().unwrap() = snapshot;
    }

    fn owner_queries(&self) -> Vec<String> {
        self.owner_queries.lock().unwrap().clone()
    }
}

impl PanelMonitor for OwnerAwareMonitor {
    type Error = FakeError;

    fn snapshot(&self) -> Result<MonitorSnapshot, Self::Error> {
        Ok(self.current.lock().unwrap().clone())
    }

    fn snapshot_for_owner(
        &self,
        identity: &MonitorIdentity,
        _anchor: PhysicalPoint,
    ) -> Result<MonitorSnapshot, Self::Error> {
        self.owner_queries
            .lock()
            .unwrap()
            .push(identity.as_str().to_owned());
        self.owner_snapshots
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(FakeError("missing owner snapshot"))
    }
}

#[derive(Clone, Default)]
struct FakeWindow(Arc<Mutex<Vec<Call>>>);

impl FakeWindow {
    fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }

    fn last_bounds(&self) -> Option<PhysicalRect> {
        self.calls().iter().rev().find_map(|call| match call {
            Call::Bounds(bounds) => Some(*bounds),
            _ => None,
        })
    }
}

impl PanelWindow for FakeWindow {
    type Error = FakeError;

    fn set_bounds(&self, bounds: PhysicalRect) -> Result<(), Self::Error> {
        self.0.lock().unwrap().push(Call::Bounds(bounds));
        Ok(())
    }

    fn show(&self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().push(Call::Show);
        Ok(())
    }

    fn focus(&self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().push(Call::Focus);
        Ok(())
    }

    fn hide(&self) -> Result<(), Self::Error> {
        self.0.lock().unwrap().push(Call::Hide);
        Ok(())
    }

    fn is_visible(&self) -> Result<bool, Self::Error> {
        let calls = self.0.lock().unwrap();
        Ok(calls.iter().rposition(|call| *call == Call::Show)
            > calls.iter().rposition(|call| *call == Call::Hide))
    }
}

#[derive(Clone)]
struct ScriptedWindow {
    calls: Arc<Mutex<Vec<Call>>>,
    show_results: Arc<Mutex<VecDeque<Result<(), FakeError>>>>,
    focus_results: Arc<Mutex<VecDeque<Result<(), FakeError>>>>,
    hide_results: Arc<Mutex<VecDeque<Result<(), FakeError>>>>,
    visibility_results: Arc<Mutex<VecDeque<Result<bool, FakeError>>>>,
}

impl ScriptedWindow {
    fn new(
        show_results: Vec<Result<(), FakeError>>,
        focus_results: Vec<Result<(), FakeError>>,
        hide_results: Vec<Result<(), FakeError>>,
        visibility_results: Vec<Result<bool, FakeError>>,
    ) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            show_results: Arc::new(Mutex::new(show_results.into())),
            focus_results: Arc::new(Mutex::new(focus_results.into())),
            hide_results: Arc::new(Mutex::new(hide_results.into())),
            visibility_results: Arc::new(Mutex::new(visibility_results.into())),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn next(
        results: &Mutex<VecDeque<Result<(), FakeError>>>,
        operation: &'static str,
    ) -> Result<(), FakeError> {
        results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("missing scripted {operation} result"))
    }
}

impl PanelWindow for ScriptedWindow {
    type Error = FakeError;

    fn set_bounds(&self, bounds: PhysicalRect) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::Bounds(bounds));
        Ok(())
    }

    fn show(&self) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::Show);
        Self::next(&self.show_results, "show")
    }

    fn focus(&self) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::Focus);
        Self::next(&self.focus_results, "focus")
    }

    fn hide(&self) -> Result<(), Self::Error> {
        self.calls.lock().unwrap().push(Call::Hide);
        Self::next(&self.hide_results, "hide")
    }

    fn is_visible(&self) -> Result<bool, Self::Error> {
        self.visibility_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(false))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Bounds(PhysicalRect),
    Show,
    Focus,
    Hide,
}

#[derive(Debug)]
struct FakeError(&'static str);

impl std::fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for FakeError {}

const fn point(x: i32, y: i32) -> PhysicalPoint {
    PhysicalPoint { x, y }
}

const fn rect(left: i32, top: i32, right: i32, bottom: i32) -> PhysicalRect {
    PhysicalRect {
        left,
        top,
        right,
        bottom,
    }
}

const fn dip(width: u32, height: u32) -> DipSize {
    DipSize { width, height }
}

fn snapshot(pointer: PhysicalPoint, work_area: PhysicalRect, dpi: u32) -> MonitorSnapshot {
    MonitorSnapshot {
        identity: MonitorIdentity::from_static("test-monitor"),
        pointer,
        work_area,
        dpi,
    }
}

fn snapshot_on(
    identity: &'static str,
    pointer: PhysicalPoint,
    work_area: PhysicalRect,
    dpi: u32,
) -> MonitorSnapshot {
    MonitorSnapshot {
        identity: MonitorIdentity::from_static(identity),
        pointer,
        work_area,
        dpi,
    }
}
