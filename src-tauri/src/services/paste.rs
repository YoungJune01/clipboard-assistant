use std::{
    error::Error,
    fmt,
    sync::{Mutex, MutexGuard},
};

use crate::domain::{ClipboardRepresentation, PasteFallbackReason, PasteOutcome};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetIdentity {
    pub window_instance_id: u64,
    pub process_id: u32,
    pub thread_id: u32,
    pub process_started_at: u64,
    pub session_id: u32,
    pub desktop_id: u64,
    pub integrity_level: u32,
    pub restricted: bool,
    pub app_container: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetSnapshot<Window> {
    pub window: Window,
    pub focused_control: Option<Window>,
    pub identity: TargetIdentity,
}

pub trait PasteTarget: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;
    type Window: Copy + Eq + Send + Sync + 'static;

    fn capture(&self) -> Result<TargetSnapshot<Self::Window>, Self::Error>;
    fn inspect(&self, window: Self::Window) -> Result<TargetIdentity, Self::Error>;
    fn input_allowed(&self, identity: &TargetIdentity) -> Result<bool, Self::Error>;
    fn input_desktop(&self) -> Result<u64, Self::Error>;
    fn restore(&self, target: &TargetSnapshot<Self::Window>) -> Result<(), Self::Error>;
    fn foreground(&self) -> Result<Self::Window, Self::Error>;
}

pub trait PasteClipboard: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn publish(&self, representations: &[ClipboardRepresentation]) -> Result<u32, Self::Error>;
}

pub trait PastePanel: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn hide(&self) -> Result<(), Self::Error>;
}

#[cfg(windows)]
pub trait QuickPanel: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn show(&self) -> Result<(), Self::Error>;
    fn hide(&self) -> Result<(), Self::Error>;
    fn is_visible(&self) -> bool;
}

pub trait PasteInput<Window>: Send + Sync + 'static {
    type Error: Error + Send + Sync + 'static;

    fn send_ctrl_v(&self, expected_foreground: Window) -> Result<(), Self::Error>;
}

#[derive(Debug)]
pub enum SafePasteError {
    Capture(Box<dyn Error + Send + Sync>),
    Clipboard(Box<dyn Error + Send + Sync>),
}

impl fmt::Display for SafePasteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(_) => formatter.write_str("could not capture the original paste target"),
            Self::Clipboard(_) => {
                formatter.write_str("could not publish the selected clipboard content")
            }
        }
    }
}

impl Error for SafePasteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Capture(error) | Self::Clipboard(error) => Some(error.as_ref()),
        }
    }
}

pub struct SafePasteService<T, C, P, I>
where
    T: PasteTarget,
    C: PasteClipboard,
    P: PastePanel,
    I: PasteInput<T::Window>,
{
    target: T,
    clipboard: C,
    panel: P,
    input: I,
    original_target: Mutex<Option<TargetSnapshot<T::Window>>>,
    operation: Mutex<()>,
}

impl<T, C, P, I> SafePasteService<T, C, P, I>
where
    T: PasteTarget,
    C: PasteClipboard,
    P: PastePanel,
    I: PasteInput<T::Window>,
{
    pub fn new(target: T, clipboard: C, panel: P, input: I) -> Self {
        Self {
            target,
            clipboard,
            panel,
            input,
            original_target: Mutex::new(None),
            operation: Mutex::new(()),
        }
    }

    pub fn prepare_target(&self) -> Result<(), SafePasteError> {
        let _operation = lock_unpoisoned(&self.operation);
        let captured = self.target.capture().map_err(|error| {
            *lock_unpoisoned(&self.original_target) = None;
            SafePasteError::Capture(Box::new(error))
        })?;
        *lock_unpoisoned(&self.original_target) = Some(captured);
        Ok(())
    }

    pub fn clear_target(&self) {
        let _operation = lock_unpoisoned(&self.operation);
        *lock_unpoisoned(&self.original_target) = None;
    }

    pub fn paste(
        &self,
        representations: &[ClipboardRepresentation],
    ) -> Result<PasteOutcome, SafePasteError> {
        let _operation = lock_unpoisoned(&self.operation);
        let target = lock_unpoisoned(&self.original_target).take();
        let target_is_safe = target
            .as_ref()
            .is_some_and(|target| self.target_is_safe(target));
        self.clipboard
            .publish(representations)
            .map_err(|error| SafePasteError::Clipboard(Box::new(error)))?;

        let Some(target) = target.filter(|_| target_is_safe) else {
            let _ = self.panel.hide();
            return Ok(copy_only());
        };
        if self.panel.hide().is_err() {
            return Ok(copy_only());
        }
        if self.target.restore(&target).is_err() || !self.target_is_safe(&target) {
            return Ok(copy_only());
        }
        if self.target.foreground().ok() != Some(target.window) {
            return Ok(copy_only());
        }
        if self.input.send_ctrl_v(target.window).is_err() {
            return Ok(copy_only());
        }
        Ok(PasteOutcome::CommandSent)
    }

    fn target_is_safe(&self, target: &TargetSnapshot<T::Window>) -> bool {
        let Ok(identity) = self.target.inspect(target.window) else {
            return false;
        };
        if identity != target.identity {
            return false;
        }
        if !matches!(self.target.input_allowed(&identity), Ok(true)) {
            return false;
        }
        self.target.input_desktop().ok() == Some(identity.desktop_id)
    }
}

#[cfg(windows)]
pub struct QuickPanelPasteCoordinator<Q, S> {
    panel: Q,
    paste: S,
}

#[cfg(windows)]
impl<Q, S> QuickPanelPasteCoordinator<Q, S> {
    pub fn new(panel: Q, paste: S) -> Self {
        Self { panel, paste }
    }
}

#[cfg(windows)]
impl<Q, T, C, P, I> QuickPanelPasteCoordinator<Q, SafePasteService<T, C, P, I>>
where
    Q: QuickPanel,
    T: PasteTarget,
    C: PasteClipboard,
    P: PastePanel,
    I: PasteInput<T::Window>,
{
    pub fn show(&self) -> Result<(), String> {
        if self.panel.is_visible() {
            return Ok(());
        }
        let _ = self.paste.prepare_target();
        if let Err(error) = self.panel.show() {
            self.paste.clear_target();
            Err(error.to_string())
        } else {
            Ok(())
        }
    }

    pub fn hide(&self) -> Result<(), String> {
        let result = self.panel.hide().map_err(|error| error.to_string());
        self.paste.clear_target();
        result
    }

    pub fn toggle(&self) -> Result<(), String> {
        if self.panel.is_visible() {
            self.hide()
        } else {
            self.show()
        }
    }

    pub fn clear_target(&self) {
        self.paste.clear_target();
    }

    pub fn paste(
        &self,
        representations: &[ClipboardRepresentation],
    ) -> Result<PasteOutcome, SafePasteError> {
        self.paste.paste(representations)
    }
}

#[cfg(windows)]
impl PastePanel for std::sync::Arc<crate::services::panel::PanelController> {
    type Error = crate::services::panel::PanelError;

    fn hide(&self) -> Result<(), Self::Error> {
        crate::services::panel::PanelController::hide(self)
    }
}

#[cfg(windows)]
impl QuickPanel for std::sync::Arc<crate::services::panel::PanelController> {
    type Error = crate::services::panel::PanelError;

    fn show(&self) -> Result<(), Self::Error> {
        crate::services::panel::PanelController::show(self)
    }

    fn hide(&self) -> Result<(), Self::Error> {
        crate::services::panel::PanelController::hide(self)
    }

    fn is_visible(&self) -> bool {
        crate::services::panel::PanelController::is_visible(self)
    }
}

fn copy_only() -> PasteOutcome {
    PasteOutcome::CopyOnly {
        reason: PasteFallbackReason::UnsafeTarget,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
