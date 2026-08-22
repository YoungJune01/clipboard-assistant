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

use crate::platform::windows::monitor::{DipSize, MonitorSnapshot, PhysicalRect, place_panel};

pub trait PanelMonitor: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn snapshot(&self) -> Result<MonitorSnapshot, Self::Error>;
}

pub trait PanelWindow: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn set_bounds(&self, bounds: PhysicalRect) -> Result<(), Self::Error>;
    fn show(&self) -> Result<(), Self::Error>;
    fn focus(&self) -> Result<(), Self::Error>;
    fn hide(&self) -> Result<(), Self::Error>;
}

#[derive(Debug)]
pub enum PanelError {
    Monitor(Box<dyn Error + Send + Sync>),
    Window(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for PanelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Monitor(_) => formatter.write_str("quick-panel monitor query failed"),
            Self::Window(_) => formatter.write_str("quick-panel window operation failed"),
        }
    }
}

impl Error for PanelError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Monitor(error) | Self::Window(error) => Some(error.as_ref()),
        }
    }
}

struct PanelState {
    visible: bool,
    focus_domain_depth: usize,
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
            })),
        }
    }

    pub fn show(&self) -> Result<(), PanelError> {
        self.reposition()?;
        self.window
            .show()
            .map_err(|error| PanelError::Window(Box::new(error)))?;
        if let Err(error) = self.window.focus() {
            let _ = self.window.hide();
            return Err(PanelError::Window(Box::new(error)));
        }
        lock_unpoisoned(&self.state).visible = true;
        Ok(())
    }

    pub fn hide(&self) -> Result<(), PanelError> {
        self.window
            .hide()
            .map_err(|error| PanelError::Window(Box::new(error)))?;
        lock_unpoisoned(&self.state).visible = false;
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
            self.reposition()
        } else {
            Ok(())
        }
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

    fn reposition(&self) -> Result<(), PanelError> {
        let snapshot = self
            .monitor
            .snapshot()
            .map_err(|error| PanelError::Monitor(Box::new(error)))?;
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
    type Error = tauri::Error;

    fn set_bounds(&self, bounds: PhysicalRect) -> Result<(), Self::Error> {
        self.0.set_size(tauri::PhysicalSize::new(
            bounds.width() as u32,
            bounds.height() as u32,
        ))?;
        self.0
            .set_position(tauri::PhysicalPosition::new(bounds.left, bounds.top))
    }

    fn show(&self) -> Result<(), Self::Error> {
        self.0.show()
    }

    fn focus(&self) -> Result<(), Self::Error> {
        self.0.set_focus()
    }

    fn hide(&self) -> Result<(), Self::Error> {
        self.0.hide()
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
        self.service.show()?;
        self.start_observer();
        Ok(())
    }

    pub fn hide(&self) -> Result<(), PanelError> {
        let result = self.service.hide();
        self.stop_observer();
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
        self.service.on_focus_changed(focused)?;
        if !self.service.is_visible() {
            self.stop_observer();
        }
        Ok(())
    }

    pub fn enter_focus_domain(&self) -> FocusDomainGuard {
        self.service.enter_focus_domain()
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
                let mut display = WindowsMonitor.snapshot().ok().map(DisplayEnvironment::from);
                loop {
                    match stopped.recv_timeout(Duration::from_millis(500)) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            let current = WindowsMonitor.snapshot().ok();
                            let environment = current.map(DisplayEnvironment::from);
                            if environment != display {
                                display = environment;
                                if !reposition_controller(&controller) {
                                    break;
                                }
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

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DisplayEnvironment {
    work_area: PhysicalRect,
    dpi: u32,
}

#[cfg(windows)]
impl From<MonitorSnapshot> for DisplayEnvironment {
    fn from(snapshot: MonitorSnapshot) -> Self {
        Self {
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
