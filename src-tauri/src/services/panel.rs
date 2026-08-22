use std::{
    error::Error,
    fmt,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        mpsc::{self, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crate::platform::windows::monitor::{
    DipSize, MonitorIdentity, MonitorSnapshot, PhysicalPoint, PhysicalRect, place_panel,
};

pub trait PanelMonitor: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn snapshot(&self) -> Result<MonitorSnapshot, Self::Error>;
    fn snapshot_for_owner(
        &self,
        identity: &MonitorIdentity,
        anchor: PhysicalPoint,
    ) -> Result<MonitorSnapshot, Self::Error>;
}

pub trait PanelWindow: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn set_bounds(&self, bounds: PhysicalRect) -> Result<(), Self::Error>;
    fn show(&self) -> Result<(), Self::Error>;
    fn focus(&self) -> Result<(), Self::Error>;
    fn hide(&self) -> Result<(), Self::Error>;
    fn is_visible(&self) -> Result<bool, Self::Error>;
}

#[derive(Debug)]
pub enum PanelError {
    Monitor(Box<dyn Error + Send + Sync>),
    Window(Box<dyn Error + Send + Sync>),
    FocusAndRollback {
        focus: Box<dyn Error + Send + Sync>,
        rollback_hide: Box<dyn Error + Send + Sync>,
    },
}

impl fmt::Display for PanelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Monitor(_) => formatter.write_str("quick-panel monitor query failed"),
            Self::Window(_) => formatter.write_str("quick-panel window operation failed"),
            Self::FocusAndRollback { .. } => {
                formatter.write_str("quick-panel focus and rollback hide both failed")
            }
        }
    }
}

impl Error for PanelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Monitor(error) | Self::Window(error) => Some(error.as_ref()),
            Self::FocusAndRollback { focus, .. } => Some(focus.as_ref()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObserverAction {
    StartOrKeep,
    Stop,
}

pub(crate) fn observer_action_for_visibility(visible: bool) -> ObserverAction {
    if visible {
        ObserverAction::StartOrKeep
    } else {
        ObserverAction::Stop
    }
}

struct PanelState {
    visible: bool,
    focus_domain_depth: usize,
    owner: Option<MonitorIdentity>,
    anchor: Option<PhysicalPoint>,
    environment: Option<DisplayEnvironment>,
}

pub struct PanelService<M, W> {
    monitor: M,
    window: W,
    panel_dip_size: DipSize,
    state: Arc<Mutex<PanelState>>,
}

impl<M, W> PanelService<M, W>
where
    M: PanelMonitor,
    W: PanelWindow,
{
    pub fn new(monitor: M, window: W, panel_dip_size: DipSize) -> Self {
        Self {
            monitor,
            window,
            panel_dip_size,
            state: Arc::new(Mutex::new(PanelState {
                visible: false,
                focus_domain_depth: 0,
                owner: None,
                anchor: None,
                environment: None,
            })),
        }
    }

    pub fn show(&self) -> Result<(), PanelError> {
        let snapshot = self.query_current_snapshot()?;
        self.apply_snapshot(&snapshot)?;
        self.window
            .show()
            .map_err(|error| PanelError::Window(Box::new(error)))?;
        {
            let mut state = lock_unpoisoned(&self.state);
            state.visible = true;
            state.owner = Some(snapshot.identity.clone());
            state.anchor = Some(snapshot.pointer);
            state.environment = Some(DisplayEnvironment::from(&snapshot));
        }
        if let Err(error) = self.window.focus() {
            return match self.window.hide() {
                Ok(()) => {
                    clear_visible_state(&mut lock_unpoisoned(&self.state));
                    Err(PanelError::Window(Box::new(error)))
                }
                Err(rollback_hide) => Err(PanelError::FocusAndRollback {
                    focus: Box::new(error),
                    rollback_hide: Box::new(rollback_hide),
                }),
            };
        }
        Ok(())
    }

    pub fn hide(&self) -> Result<(), PanelError> {
        self.window
            .hide()
            .map_err(|error| PanelError::Window(Box::new(error)))?;
        if self
            .window
            .is_visible()
            .map_err(|error| PanelError::Window(Box::new(error)))?
        {
            return Err(PanelError::Window(Box::new(PanelStateError(
                "quick-panel window remains visible",
            ))));
        }
        clear_visible_state(&mut lock_unpoisoned(&self.state));
        Ok(())
    }

    pub fn toggle(&self) -> Result<(), PanelError> {
        if self.is_visible() {
            self.hide()
        } else {
            self.show()
        }
    }

    pub fn reposition_if_visible(&self) -> Result<(), PanelError> {
        if self.is_visible() {
            let snapshot = self.query_owner_snapshot()?;
            self.apply_snapshot(&snapshot)?;
            let mut state = lock_unpoisoned(&self.state);
            state.owner = Some(snapshot.identity.clone());
            state.environment = Some(DisplayEnvironment::from(&snapshot));
            Ok(())
        } else {
            Ok(())
        }
    }

    pub fn owner_environment_changed(&self) -> Result<bool, PanelError> {
        if !self.is_visible() {
            return Ok(false);
        }
        let snapshot = self.query_owner_snapshot()?;
        let current = DisplayEnvironment::from(&snapshot);
        Ok(lock_unpoisoned(&self.state).environment.as_ref() != Some(&current))
    }

    pub fn owner_identity(&self) -> Option<String> {
        lock_unpoisoned(&self.state)
            .owner
            .as_ref()
            .map(|identity| identity.as_str().to_owned())
    }

    pub fn on_focus_changed(&self, focused: bool) -> Result<(), PanelError> {
        let should_hide = {
            let state = lock_unpoisoned(&self.state);
            !focused && state.visible && state.focus_domain_depth == 0
        };
        if should_hide { self.hide() } else { Ok(()) }
    }

    pub fn enter_focus_domain(&self) -> FocusDomainGuard {
        let mut state = lock_unpoisoned(&self.state);
        state.focus_domain_depth = state.focus_domain_depth.saturating_add(1);
        FocusDomainGuard {
            state: Arc::clone(&self.state),
        }
    }

    pub fn is_visible(&self) -> bool {
        lock_unpoisoned(&self.state).visible
    }

    pub fn verified_visibility(&self) -> Result<bool, PanelError> {
        self.window
            .is_visible()
            .map_err(|error| PanelError::Window(Box::new(error)))
    }

    fn query_current_snapshot(&self) -> Result<MonitorSnapshot, PanelError> {
        self.monitor
            .snapshot()
            .map_err(|error| PanelError::Monitor(Box::new(error)))
    }

    fn query_owner_snapshot(&self) -> Result<MonitorSnapshot, PanelError> {
        let (owner, anchor) = {
            let state = lock_unpoisoned(&self.state);
            (state.owner.clone(), state.anchor)
        };
        let owner = owner.ok_or_else(|| {
            PanelError::Monitor(Box::new(PanelStateError("quick-panel owner is missing")))
        })?;
        let anchor = anchor.ok_or_else(|| {
            PanelError::Monitor(Box::new(PanelStateError("quick-panel anchor is missing")))
        })?;
        self.monitor
            .snapshot_for_owner(&owner, anchor)
            .map_err(|error| PanelError::Monitor(Box::new(error)))
    }

    fn apply_snapshot(&self, snapshot: &MonitorSnapshot) -> Result<(), PanelError> {
        let bounds = place_panel(
            snapshot.pointer,
            snapshot.work_area,
            self.panel_dip_size,
            snapshot.dpi,
        );
        self.window
            .set_bounds(bounds)
            .map_err(|error| PanelError::Window(Box::new(error)))
    }
}

#[derive(Debug)]
struct PanelStateError(&'static str);

impl fmt::Display for PanelStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for PanelStateError {}

fn clear_visible_state(state: &mut PanelState) {
    state.visible = false;
    state.owner = None;
    state.anchor = None;
    state.environment = None;
}

pub struct FocusDomainGuard {
    state: Arc<Mutex<PanelState>>,
}

impl Drop for FocusDomainGuard {
    fn drop(&mut self) {
        let mut state = lock_unpoisoned(&self.state);
        state.focus_domain_depth = state.focus_domain_depth.saturating_sub(1);
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(windows)]
pub const QUICK_PANEL_DIP_SIZE: DipSize = DipSize {
    width: 420,
    height: 520,
};

#[cfg(windows)]
#[derive(Clone, Copy)]
pub struct WindowsMonitor;

#[cfg(windows)]
impl PanelMonitor for WindowsMonitor {
    type Error = crate::platform::windows::monitor::MonitorError;

    fn snapshot(&self) -> Result<MonitorSnapshot, Self::Error> {
        crate::platform::windows::monitor::current_monitor_snapshot()
    }

    fn snapshot_for_owner(
        &self,
        identity: &MonitorIdentity,
        anchor: PhysicalPoint,
    ) -> Result<MonitorSnapshot, Self::Error> {
        crate::platform::windows::monitor::snapshot_for_identity(identity, anchor)
    }
}

#[cfg(windows)]
#[derive(Clone)]
pub struct TauriPanelWindow(tauri::WebviewWindow);

#[cfg(windows)]
impl TauriPanelWindow {
    pub fn new(window: tauri::WebviewWindow) -> Self {
        Self(window)
    }
}

#[cfg(windows)]
impl PanelWindow for TauriPanelWindow {
    type Error = TauriPanelWindowError;

    fn set_bounds(&self, bounds: PhysicalRect) -> Result<(), Self::Error> {
        self.0.set_size(tauri::PhysicalSize::new(
            bounds.width() as u32,
            bounds.height() as u32,
        ))?;
        self.0
            .set_position(tauri::PhysicalPosition::new(bounds.left, bounds.top))
            .map_err(Into::into)
    }

    fn show(&self) -> Result<(), Self::Error> {
        self.0.show().map_err(Into::into)
    }

    fn focus(&self) -> Result<(), Self::Error> {
        let hwnd = self.0.hwnd()?;
        crate::platform::windows::activation::activate(hwnd).map_err(Into::into)
    }

    fn hide(&self) -> Result<(), Self::Error> {
        self.0.hide().map_err(Into::into)
    }

    fn is_visible(&self) -> Result<bool, Self::Error> {
        let hwnd = self.0.hwnd()?;
        Ok(unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(hwnd).as_bool() })
    }
}

#[cfg(windows)]
#[derive(Debug)]
pub enum TauriPanelWindowError {
    Tauri(tauri::Error),
    Activation(crate::platform::windows::activation::ActivationError),
}

#[cfg(windows)]
impl fmt::Display for TauriPanelWindowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tauri(_) => formatter.write_str("quick-panel window operation failed"),
            Self::Activation(_) => formatter.write_str("quick-panel activation was rejected"),
        }
    }
}

#[cfg(windows)]
impl Error for TauriPanelWindowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Tauri(error) => Some(error),
            Self::Activation(error) => Some(error),
        }
    }
}

#[cfg(windows)]
impl From<tauri::Error> for TauriPanelWindowError {
    fn from(error: tauri::Error) -> Self {
        Self::Tauri(error)
    }
}

#[cfg(windows)]
impl From<crate::platform::windows::activation::ActivationError> for TauriPanelWindowError {
    fn from(error: crate::platform::windows::activation::ActivationError) -> Self {
        Self::Activation(error)
    }
}

#[cfg(windows)]
type WindowsPanelService = PanelService<WindowsMonitor, TauriPanelWindow>;

#[cfg(windows)]
pub struct PanelController {
    service: WindowsPanelService,
    observer: Mutex<Option<PanelObserver>>,
}

#[cfg(windows)]
impl PanelController {
    pub fn new(window: tauri::WebviewWindow) -> Self {
        Self {
            service: PanelService::new(
                WindowsMonitor,
                TauriPanelWindow::new(window),
                QUICK_PANEL_DIP_SIZE,
            ),
            observer: Mutex::new(None),
        }
    }

    pub fn show(self: &Arc<Self>) -> Result<(), PanelError> {
        let result = self.service.show();
        self.update_observer();
        result
    }

    pub fn hide(&self) -> Result<(), PanelError> {
        let result = self.service.hide();
        if !self.service.is_visible() {
            self.stop_observer();
        }
        result
    }

    pub fn toggle(self: &Arc<Self>) -> Result<(), PanelError> {
        if self.service.is_visible() {
            self.hide()
        } else {
            self.show()
        }
    }

    pub fn reposition_if_visible(&self) -> Result<(), PanelError> {
        self.service.reposition_if_visible()
    }

    pub fn on_focus_changed(&self, focused: bool) -> Result<(), PanelError> {
        let result = self.service.on_focus_changed(focused);
        if !self.service.is_visible() {
            self.stop_observer();
        }
        result
    }

    pub fn enter_focus_domain(&self) -> FocusDomainGuard {
        self.service.enter_focus_domain()
    }

    pub fn is_visible(&self) -> bool {
        self.service.is_visible()
    }

    pub fn verified_visibility(&self) -> Result<bool, PanelError> {
        self.service.verified_visibility()
    }

    fn schedule_reposition(self: &Arc<Self>) {
        let controller = Arc::downgrade(self);
        let window = self.service.window.0.clone();
        let _ = window.run_on_main_thread(move || {
            if let Some(controller) = controller.upgrade() {
                let _ = controller.reposition_if_visible();
            }
        });
    }

    fn start_observer(self: &Arc<Self>) {
        let mut observer = lock_unpoisoned(&self.observer);
        if observer.is_some() {
            return;
        }
        let (stop, stopped) = mpsc::channel();
        let controller = Arc::downgrade(self);
        let thread = thread::Builder::new()
            .name("quick-panel-display-observer".to_owned())
            .spawn(move || {
                loop {
                    match stopped.recv_timeout(Duration::from_millis(500)) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            if controller_environment_changed(&controller)
                                && !reposition_controller(&controller)
                            {
                                break;
                            }
                        }
                    }
                }
            });
        match thread {
            Ok(thread) => *observer = Some(PanelObserver { stop, thread }),
            Err(error) => eprintln!("quick-panel display observer could not start: {error}"),
        }
    }

    fn stop_observer(&self) {
        let observer = lock_unpoisoned(&self.observer).take();
        if let Some(observer) = observer {
            observer.stop();
        }
    }

    fn update_observer(self: &Arc<Self>) {
        match observer_action_for_visibility(self.service.is_visible()) {
            ObserverAction::StartOrKeep => self.start_observer(),
            ObserverAction::Stop => self.stop_observer(),
        }
    }
}

#[cfg(windows)]
fn reposition_controller(controller: &Weak<PanelController>) -> bool {
    let Some(controller) = controller.upgrade() else {
        return false;
    };
    controller.schedule_reposition();
    true
}

#[cfg(windows)]
fn controller_environment_changed(controller: &Weak<PanelController>) -> bool {
    controller.upgrade().is_some_and(|controller| {
        controller
            .service
            .owner_environment_changed()
            .unwrap_or(false)
    })
}

#[cfg(windows)]
impl Drop for PanelController {
    fn drop(&mut self) {
        let observer = self
            .observer
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(observer) = observer {
            observer.stop();
        }
    }
}

#[cfg(windows)]
struct PanelObserver {
    stop: Sender<()>,
    thread: JoinHandle<()>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DisplayEnvironment {
    identity: MonitorIdentity,
    work_area: PhysicalRect,
    dpi: u32,
}

impl From<&MonitorSnapshot> for DisplayEnvironment {
    fn from(snapshot: &MonitorSnapshot) -> Self {
        Self {
            identity: snapshot.identity.clone(),
            work_area: snapshot.work_area,
            dpi: snapshot.dpi,
        }
    }
}

#[cfg(windows)]
impl PanelObserver {
    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.thread.join();
    }
}
