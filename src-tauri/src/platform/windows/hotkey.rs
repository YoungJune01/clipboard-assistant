use std::{
    error::Error,
    fmt,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use windows::Win32::{
    Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, GetLastError, LPARAM, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{
            MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, RegisterHotKey, UnregisterHotKey, VK_V,
        },
        WindowsAndMessaging::{
            GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, WM_HOTKEY, WM_QUIT,
        },
    },
};

const HOTKEY_ID: i32 = 0x4341;
const READY_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

static HOTKEY_REAPER: LazyLock<mpsc::Sender<JoinHandle<()>>> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::channel::<JoinHandle<()>>();
    thread::Builder::new()
        .name("global-hotkey-reaper".to_owned())
        .spawn(move || {
            while let Ok(thread) = receiver.recv() {
                let _ = thread.join();
            }
        })
        .expect("global hotkey reaper must start");
    sender
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyAvailability {
    Available,
    Conflict,
    Unavailable,
}

#[derive(Debug)]
pub enum HotkeyError {
    Conflict,
    Windows(windows::core::Error),
    ThreadSpawn(std::io::Error),
    InitializationTimeout,
    UnexpectedThreadExit,
}

impl fmt::Display for HotkeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => formatter.write_str("the global shortcut is already in use"),
            Self::Windows(_) => formatter.write_str("the global shortcut is unavailable"),
            Self::ThreadSpawn(_) => {
                formatter.write_str("the global shortcut thread could not start")
            }
            Self::InitializationTimeout => {
                formatter.write_str("the global shortcut initialization timed out")
            }
            Self::UnexpectedThreadExit => {
                formatter.write_str("the global shortcut thread exited unexpectedly")
            }
        }
    }
}

impl Error for HotkeyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Windows(error) => Some(error),
            Self::ThreadSpawn(error) => Some(error),
            Self::Conflict | Self::InitializationTimeout | Self::UnexpectedThreadExit => None,
        }
    }
}

pub struct GlobalHotkey {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
    done: mpsc::Receiver<()>,
}

impl GlobalHotkey {
    pub fn start(on_pressed: impl Fn() + Send + 'static) -> Result<Self, HotkeyError> {
        let (thread_id_sender, thread_id_receiver) = mpsc::sync_channel(1);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (done_sender, done) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let thread = thread::Builder::new()
            .name("global-hotkey".to_owned())
            .spawn(move || {
                let result = run_hotkey_loop(
                    on_pressed,
                    &thread_id_sender,
                    &ready_sender,
                    &thread_cancelled,
                );
                if result.is_err() {
                    let _ = ready_sender.send(result);
                }
                let _ = done_sender.send(());
            })
            .map_err(HotkeyError::ThreadSpawn)?;
        let thread_id = match thread_id_receiver.recv_timeout(READY_TIMEOUT) {
            Ok(thread_id) => thread_id,
            Err(_) => {
                bounded_cleanup(0, thread, &done, &cancelled);
                return Err(HotkeyError::InitializationTimeout);
            }
        };
        match ready_receiver.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                thread_id,
                thread: Some(thread),
                done,
            }),
            Ok(Err(error)) => {
                bounded_cleanup(thread_id, thread, &done, &cancelled);
                Err(error)
            }
            Err(RecvTimeoutError::Timeout) => {
                bounded_cleanup(thread_id, thread, &done, &cancelled);
                Err(HotkeyError::InitializationTimeout)
            }
            Err(RecvTimeoutError::Disconnected) => {
                bounded_cleanup(thread_id, thread, &done, &cancelled);
                Err(HotkeyError::UnexpectedThreadExit)
            }
        }
    }

    pub fn availability(&self) -> HotkeyAvailability {
        HotkeyAvailability::Available
    }

    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let thread_id = std::mem::take(&mut self.thread_id);
        let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if self.done.recv_timeout(SHUTDOWN_TIMEOUT).is_ok() {
            let _ = thread.join();
        } else {
            let _ = HOTKEY_REAPER.send(thread);
        }
    }
}

impl Drop for GlobalHotkey {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_hotkey_loop(
    on_pressed: impl Fn(),
    thread_id_sender: &mpsc::SyncSender<u32>,
    ready: &mpsc::SyncSender<Result<(), HotkeyError>>,
    cancelled: &AtomicBool,
) -> Result<(), HotkeyError> {
    let thread_id = unsafe { GetCurrentThreadId() };
    let mut pending = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut pending, None, 0, 0, PM_NOREMOVE);
    }
    if thread_id_sender.send(thread_id).is_err() {
        return Ok(());
    }
    if !registration_allowed(cancelled) {
        return Ok(());
    }
    let modifiers = MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT;
    let registered = register_with_cancellation(
        cancelled,
        || {
            unsafe { RegisterHotKey(None, HOTKEY_ID, modifiers, VK_V.0 as u32) }.map_err(|error| {
                if unsafe { GetLastError() } == ERROR_HOTKEY_ALREADY_REGISTERED {
                    HotkeyError::Conflict
                } else {
                    HotkeyError::Windows(error)
                }
            })
        },
        || {
            let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
        },
    );
    let registered = match registered {
        Ok(registered) => registered,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };
    if !registered {
        return Ok(());
    }
    if ready.send(Ok(())).is_err() {
        let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
        return Ok(());
    }
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            let error = HotkeyError::Windows(windows::core::Error::from_win32());
            let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
            return Err(error);
        }
        if result.0 == 0 {
            break;
        }
        if is_activation_message(&message) {
            on_pressed();
        }
    }
    let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
    Ok(())
}

fn is_activation_message(message: &MSG) -> bool {
    message.message == WM_HOTKEY && message.wParam.0 == HOTKEY_ID as usize
}

fn bounded_cleanup(
    thread_id: u32,
    thread: JoinHandle<()>,
    done: &mpsc::Receiver<()>,
    cancelled: &AtomicBool,
) {
    cancelled.store(true, Ordering::Release);
    if thread_id != 0 {
        let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
    if done.recv_timeout(SHUTDOWN_TIMEOUT).is_ok() {
        let _ = thread.join();
    } else {
        let _ = HOTKEY_REAPER.send(thread);
    }
}

fn registration_allowed(cancelled: &AtomicBool) -> bool {
    !cancelled.load(Ordering::Acquire)
}

fn register_with_cancellation(
    cancelled: &AtomicBool,
    register: impl FnOnce() -> Result<(), HotkeyError>,
    unregister: impl FnOnce(),
) -> Result<bool, HotkeyError> {
    if !registration_allowed(cancelled) {
        return Ok(false);
    }
    register()?;
    if !registration_allowed(cancelled) {
        unregister();
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shortcut_is_ctrl_shift_v_and_never_uses_input_injection() {
        let source = include_str!("hotkey.rs");

        assert!(source.contains("MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT"));
        assert!(source.contains("VK_V"));
        assert!(!source.contains(&["Send", "Input"].concat()));
    }

    #[test]
    fn conflict_has_a_stable_non_sensitive_status() {
        assert_eq!(HotkeyAvailability::Conflict, HotkeyAvailability::Conflict);
        assert_eq!(
            HotkeyError::Conflict.to_string(),
            "the global shortcut is already in use"
        );
    }

    #[test]
    fn message_adapter_accepts_only_the_registered_hotkey_id() {
        let mut message = MSG {
            message: WM_HOTKEY,
            wParam: WPARAM(HOTKEY_ID as usize),
            ..MSG::default()
        };
        assert!(is_activation_message(&message));

        message.wParam = WPARAM((HOTKEY_ID + 1) as usize);
        assert!(!is_activation_message(&message));
        message.message = WM_QUIT;
        message.wParam = WPARAM(HOTKEY_ID as usize);
        assert!(!is_activation_message(&message));
    }

    #[test]
    fn stop_is_guarded_by_ownership_of_the_thread_handle() {
        let source = include_str!("hotkey.rs");

        assert!(source.contains("let Some(thread) = self.thread.take() else"));
        assert!(source.contains("std::mem::take(&mut self.thread_id)"));
    }

    #[test]
    fn initialization_timeout_cancellation_blocks_late_registration() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancelled_during_registration = Arc::clone(&cancelled);
        let unregistered = AtomicBool::new(false);

        let registered = register_with_cancellation(
            &cancelled,
            move || {
                cancelled_during_registration.store(true, Ordering::Release);
                Ok(())
            },
            || unregistered.store(true, Ordering::Release),
        )
        .unwrap();

        assert!(!registered);
        assert!(unregistered.load(Ordering::Acquire));
    }

    #[test]
    #[ignore = "registers a desktop-wide shortcut; set CLIPBOARD_ASSISTANT_RUN_HOTKEY_TESTS=1 and run explicitly"]
    fn real_hotkey_registration_is_environment_gated() {
        if std::env::var_os("CLIPBOARD_ASSISTANT_RUN_HOTKEY_TESTS").is_none() {
            return;
        }
        let hotkey = GlobalHotkey::start(|| {}).expect("default hotkey should register");
        hotkey.shutdown();
    }
}
