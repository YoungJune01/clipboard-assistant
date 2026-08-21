use std::{
    error::Error,
    fmt,
    sync::{
        Arc, Mutex,
        mpsc::{RecvTimeoutError, Sender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use chrono::Utc;
use windows::Win32::{
    Foundation::{CloseHandle, ERROR_SUCCESS, GetLastError, HANDLE, HGLOBAL, HWND, SetLastError},
    System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        Threading::{CreateEventW, SetEvent},
    },
};
use windows::core::{PCWSTR, w};

use crate::domain::{CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity};

use super::message_loop::{
    ListenerState, WM_CLIPBOARD_LISTENER_SHUTDOWN, run_message_loop, wake_and_join,
};

const CF_UNICODETEXT_FORMAT: u32 = 13;
const CF_DIBV5_FORMAT: u32 = 17;
const MAX_PENDING_PRODUCT_WRITE_EVENTS: usize = 32;
const LISTENER_READY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardEvent {
    pub sequence_number: u32,
    pub captured: CapturedClipboard,
}

pub struct ClipboardListener {
    hwnd_value: isize,
    events: Sender<ClipboardEvent>,
    product_write: Arc<Mutex<ProductWriteState>>,
    shutdown: Sender<()>,
    shutdown_event_value: Option<isize>,
    thread: Option<JoinHandle<Result<(), ClipboardListenerError>>>,
}

impl ClipboardListener {
    pub fn start(events: Sender<ClipboardEvent>) -> Result<Self, ClipboardListenerError> {
        let product_write = Arc::new(Mutex::new(ProductWriteState::Idle));
        let thread_product_write = Arc::clone(&product_write);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
        let shutdown_event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(ClipboardListenerError::Windows)?;
        let thread_shutdown_event = shutdown_event.0 as isize;
        let thread_events = events.clone();
        let thread = match thread::Builder::new()
            .name("clipboard-listener".to_owned())
            .spawn(move || {
                run_message_loop(
                    thread_events,
                    thread_product_write,
                    ready_sender,
                    shutdown_receiver,
                    HANDLE(thread_shutdown_event as *mut std::ffi::c_void),
                )
            }) {
            Ok(thread) => thread,
            Err(error) => {
                unsafe {
                    let _ = CloseHandle(shutdown_event);
                }
                return Err(ClipboardListenerError::ThreadSpawn(error));
            }
        };

        match classify_ready_wait(ready_receiver.recv_timeout(LISTENER_READY_TIMEOUT)) {
            ReadyWait::Ready(Ok(hwnd_value)) => Ok(Self {
                hwnd_value,
                events,
                product_write,
                shutdown: shutdown_sender,
                shutdown_event_value: Some(shutdown_event.0 as isize),
                thread: Some(thread),
            }),
            ReadyWait::Ready(Err(error)) => {
                let joined = join_listener_thread(thread);
                let close = close_shutdown_event(shutdown_event);
                Err(combine_startup_failure(error, joined, close))
            }
            ReadyWait::Timeout => {
                let signal = unsafe { SetEvent(shutdown_event) }.map_err(ShutdownFailure::Signal);
                let joined = join_listener_thread(thread);
                let close = close_shutdown_event(shutdown_event);
                Err(combine_startup_shutdown(
                    ClipboardListenerError::ReadyTimeout,
                    signal.err().into_iter().collect(),
                    joined,
                    close,
                ))
            }
            ReadyWait::Disconnected => {
                let signal = unsafe { SetEvent(shutdown_event) }.map_err(ShutdownFailure::Signal);
                let joined = join_listener_thread(thread);
                let close = close_shutdown_event(shutdown_event);
                Err(combine_startup_shutdown(
                    ClipboardListenerError::UnexpectedThreadExit,
                    signal.err().into_iter().collect(),
                    joined,
                    close,
                ))
            }
        }
    }

    pub fn begin_product_write(&self) -> Result<ProductWriteGuard, ClipboardListenerError> {
        let baseline = unsafe { GetClipboardSequenceNumber() };
        let mut state = lock_unpoisoned(&self.product_write);
        if !matches!(*state, ProductWriteState::Idle) {
            return Err(ClipboardListenerError::ProductWriteAlreadyInProgress);
        }
        *state = ProductWriteState::Armed {
            baseline,
            pending: Vec::new(),
        };
        drop(state);
        Ok(ProductWriteGuard {
            state: Arc::clone(&self.product_write),
            events: self.events.clone(),
            finished: false,
        })
    }

    pub fn shutdown(mut self) -> Result<(), ClipboardListenerError> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), ClipboardListenerError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        let hwnd_value = self.hwnd_value;
        let event_value = self.shutdown_event_value;
        let attempt = wake_and_join(
            || {
                self.shutdown
                    .send(())
                    .map_err(|_| ShutdownFailure::ControlDisconnected)
            },
            || unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(hwnd_value as *mut std::ffi::c_void)),
                    WM_CLIPBOARD_LISTENER_SHUTDOWN,
                    Default::default(),
                    Default::default(),
                )
                .map_err(ShutdownFailure::PostMessage)
            },
            || match event_value {
                Some(value) => unsafe { SetEvent(HANDLE(value as *mut std::ffi::c_void)) }
                    .map_err(ShutdownFailure::Signal),
                None => Ok(()),
            },
            || join_listener_thread(thread),
        );
        let close = self
            .shutdown_event_value
            .take()
            .map(|value| close_shutdown_event(HANDLE(value as *mut std::ffi::c_void)));
        combine_shutdown_result(attempt.failures, attempt.thread, close)
    }
}

impl Drop for ClipboardListener {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Debug)]
pub enum ClipboardListenerError {
    ThreadSpawn(std::io::Error),
    Windows(windows::core::Error),
    ProductWriteAlreadyInProgress,
    UnexpectedThreadExit,
    ThreadPanicked,
    ReadyTimeout,
    Cleanup(Vec<CleanupFailure>),
    OperationAndCleanup {
        operation: Box<ClipboardListenerError>,
        cleanup: Vec<CleanupFailure>,
    },
    Shutdown {
        failures: Vec<ShutdownFailure>,
        thread: Option<Box<ClipboardListenerError>>,
    },
}

#[derive(Clone, Debug)]
pub enum CleanupFailure {
    RemoveListener(windows::core::Error),
    DestroyWindow(windows::core::Error),
    UnregisterClass(windows::core::Error),
}

#[derive(Debug)]
pub enum ShutdownFailure {
    ControlDisconnected,
    PostMessage(windows::core::Error),
    Signal(windows::core::Error),
    CloseEvent(windows::core::Error),
}

impl fmt::Display for ClipboardListenerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn(_) => {
                formatter.write_str("could not start clipboard listener thread")
            }
            Self::Windows(_) => formatter.write_str("Windows clipboard listener operation failed"),
            Self::ProductWriteAlreadyInProgress => {
                formatter.write_str("a product-owned clipboard write is already in progress")
            }
            Self::UnexpectedThreadExit => {
                formatter.write_str("clipboard listener exited before ready")
            }
            Self::ThreadPanicked => formatter.write_str("clipboard listener thread panicked"),
            Self::ReadyTimeout => formatter.write_str("clipboard listener ready wait timed out"),
            Self::Cleanup(failures) => write!(
                formatter,
                "clipboard listener cleanup failed in {} stage(s)",
                failures.len()
            ),
            Self::OperationAndCleanup { operation, cleanup } => {
                write!(
                    formatter,
                    "{operation}; cleanup failed in {} stage(s)",
                    cleanup.len()
                )
            }
            Self::Shutdown { failures, thread } => write!(
                formatter,
                "clipboard listener shutdown failed ({} wake/handle failure(s), thread error: {})",
                failures.len(),
                thread.is_some()
            ),
        }
    }
}

impl Error for ClipboardListenerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn(error) => Some(error),
            Self::Windows(error) => Some(error),
            Self::OperationAndCleanup { operation, .. } => Some(operation),
            Self::Shutdown {
                thread: Some(error),
                ..
            } => Some(error),
            Self::ProductWriteAlreadyInProgress
            | Self::UnexpectedThreadExit
            | Self::ThreadPanicked
            | Self::ReadyTimeout
            | Self::Cleanup(_)
            | Self::Shutdown { thread: None, .. } => None,
        }
    }
}

impl fmt::Display for CleanupFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stage = match self {
            Self::RemoveListener(_) => "remove clipboard listener",
            Self::DestroyWindow(_) => "destroy listener window",
            Self::UnregisterClass(_) => "unregister listener class",
        };
        formatter.write_str(stage)
    }
}

impl Error for CleanupFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RemoveListener(error)
            | Self::DestroyWindow(error)
            | Self::UnregisterClass(error) => Some(error),
        }
    }
}

fn join_listener_thread(
    thread: JoinHandle<Result<(), ClipboardListenerError>>,
) -> Result<(), ClipboardListenerError> {
    match thread.join() {
        Ok(result) => result,
        Err(_) => Err(ClipboardListenerError::ThreadPanicked),
    }
}

fn close_shutdown_event(event: HANDLE) -> Result<(), ShutdownFailure> {
    unsafe { CloseHandle(event) }.map_err(ShutdownFailure::CloseEvent)
}

fn combine_startup_failure(
    startup: ClipboardListenerError,
    joined: Result<(), ClipboardListenerError>,
    close: Result<(), ShutdownFailure>,
) -> ClipboardListenerError {
    combine_startup_shutdown(startup, Vec::new(), joined, close)
}

fn combine_startup_shutdown(
    startup: ClipboardListenerError,
    mut failures: Vec<ShutdownFailure>,
    joined: Result<(), ClipboardListenerError>,
    close: Result<(), ShutdownFailure>,
) -> ClipboardListenerError {
    if let Err(error) = close {
        failures.push(error);
    }
    let thread = joined
        .err()
        .filter(|error| !same_error_kind(error, &startup));
    if failures.is_empty() && thread.is_none() {
        startup
    } else {
        ClipboardListenerError::Shutdown {
            failures,
            thread: Some(Box::new(thread.unwrap_or(startup))),
        }
    }
}

fn same_error_kind(left: &ClipboardListenerError, right: &ClipboardListenerError) -> bool {
    std::mem::discriminant(left) == std::mem::discriminant(right)
}

fn combine_shutdown_result(
    mut failures: Vec<ShutdownFailure>,
    joined: Result<(), ClipboardListenerError>,
    close: Option<Result<(), ShutdownFailure>>,
) -> Result<(), ClipboardListenerError> {
    if let Some(Err(error)) = close {
        failures.push(error);
    }
    match (failures.is_empty(), joined) {
        (true, result) => result,
        (false, Ok(())) => Err(ClipboardListenerError::Shutdown {
            failures,
            thread: None,
        }),
        (false, Err(error)) => Err(ClipboardListenerError::Shutdown {
            failures,
            thread: Some(Box::new(error)),
        }),
    }
}

pub(super) fn capture_update(state: &ListenerState) {
    let Ok((sequence_number, representations)) = read_supported_representations() else {
        return;
    };
    if representations.is_empty() {
        return;
    }
    let captured = CapturedClipboard {
        content_identity: ContentIdentity::new(format!("clipboard-sequence:{sequence_number}")),
        captured_at: Utc::now(),
        source: SourceIdentity::default(),
        representations,
    };
    route_event(
        &state.product_write,
        &state.events,
        ClipboardEvent {
            sequence_number,
            captured,
        },
    );
}

fn route_event(
    state: &Mutex<ProductWriteState>,
    events: &Sender<ClipboardEvent>,
    event: ClipboardEvent,
) {
    let event_to_send = {
        let mut state = lock_unpoisoned(state);
        match &mut *state {
            ProductWriteState::Armed { pending, .. } => {
                let oldest =
                    (pending.len() == MAX_PENDING_PRODUCT_WRITE_EVENTS).then(|| pending.remove(0));
                pending.push(event);
                oldest
            }
            ProductWriteState::Expected { sequence } if *sequence == event.sequence_number => {
                *state = ProductWriteState::Idle;
                None
            }
            ProductWriteState::Expected { .. } => {
                *state = ProductWriteState::Idle;
                Some(event)
            }
            ProductWriteState::Idle => Some(event),
        }
    };
    if let Some(event) = event_to_send {
        let _ = events.send(event);
    }
}

#[derive(Debug)]
pub(super) enum ProductWriteState {
    Idle,
    Armed {
        baseline: u32,
        pending: Vec<ClipboardEvent>,
    },
    Expected {
        sequence: u32,
    },
}

pub struct ProductWriteGuard {
    state: Arc<Mutex<ProductWriteState>>,
    events: Sender<ClipboardEvent>,
    finished: bool,
}

impl ProductWriteGuard {
    pub fn finish(mut self, sequence_number: u32) {
        let mut state = lock_unpoisoned(&self.state);
        let previous = std::mem::replace(&mut *state, ProductWriteState::Idle);
        *state = match previous {
            ProductWriteState::Armed { baseline, pending } => {
                let matched = pending
                    .iter()
                    .any(|event| event.sequence_number == sequence_number);
                let pending_to_send: Vec<_> = pending
                    .into_iter()
                    .filter(|event| event.sequence_number != sequence_number)
                    .collect();
                if matched || sequence_number == baseline {
                    *state = ProductWriteState::Idle;
                } else {
                    *state = ProductWriteState::Expected {
                        sequence: sequence_number,
                    };
                }
                drop(state);
                for event in pending_to_send {
                    let _ = self.events.send(event);
                }
                self.finished = true;
                return;
            }
            _ => ProductWriteState::Idle,
        };
        self.finished = true;
    }
}

impl Drop for ProductWriteGuard {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = lock_unpoisoned(&self.state);
            if let ProductWriteState::Armed { pending, .. } =
                std::mem::replace(&mut *state, ProductWriteState::Idle)
            {
                drop(state);
                for event in pending {
                    let _ = self.events.send(event);
                }
            }
        }
    }
}

fn read_supported_representations()
-> Result<(u32, Vec<ClipboardRepresentation>), ClipboardReadError> {
    let clipboard = ClipboardGuard::open_with_retry()?;
    let operation = (|| {
        let sequence_number = unsafe { GetClipboardSequenceNumber() };
        unsafe { SetLastError(ERROR_SUCCESS) };
        let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        let png_format = classify_registered_format(png_format, unsafe { GetLastError() }.0)?;
        let mut representations = Vec::with_capacity(3);
        if let Some(text) = read_unicode_text(CF_UNICODETEXT_FORMAT)? {
            representations.push(ClipboardRepresentation::UnicodeText { text });
        }
        if let Some(bytes) = read_global_bytes(png_format)? {
            representations.push(ClipboardRepresentation::Png { bytes });
        }
        if let Some(bytes) = read_global_bytes(CF_DIBV5_FORMAT)? {
            representations.push(ClipboardRepresentation::DibV5 { bytes });
        }
        Ok((sequence_number, representations))
    })();
    combine_clipboard_operation_and_close(operation, clipboard.close())
        .map_err(ClipboardReadError::from_combined)
}

#[derive(Debug)]
enum ClipboardReadError {
    Windows(ClipboardReadOperation, windows::core::Error),
    InvalidUnicodeText(UnicodeTextError),
    OperationAndClose {
        operation: Box<ClipboardReadError>,
        close: windows::core::Error,
    },
}

impl fmt::Display for ClipboardReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows(operation, _) => write!(formatter, "clipboard {operation} failed"),
            Self::InvalidUnicodeText(reason) => {
                write!(formatter, "clipboard Unicode text is {reason}")
            }
            Self::OperationAndClose { operation, .. } => {
                write!(formatter, "{operation}; closing clipboard also failed")
            }
        }
    }
}

impl Error for ClipboardReadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Windows(_, error) => Some(error),
            Self::OperationAndClose { operation, close } => {
                let _ = close;
                Some(operation)
            }
            Self::InvalidUnicodeText(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ClipboardReadOperation {
    Open,
    RegisterPng,
    GetData,
    GlobalLock,
    GlobalSize,
    GlobalUnlock,
    Close,
}

impl fmt::Display for ClipboardReadOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Open => "open",
            Self::RegisterPng => "PNG format registration",
            Self::GetData => "data access",
            Self::GlobalLock => "global memory lock",
            Self::GlobalSize => "global memory size",
            Self::GlobalUnlock => "global memory unlock",
            Self::Close => "close",
        })
    }
}

#[derive(Debug)]
enum UnicodeTextError {
    OddByteLength,
    MissingNul,
    InvalidUtf16,
}

impl fmt::Display for UnicodeTextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OddByteLength => "an odd byte length",
            Self::MissingNul => "missing its NUL terminator",
            Self::InvalidUtf16 => "invalid UTF-16",
        })
    }
}

impl ClipboardReadError {
    fn windows(operation: ClipboardReadOperation, error: windows::core::Error) -> Self {
        Self::Windows(operation, error)
    }

    fn from_combined((operation, close): (Option<Self>, Option<windows::core::Error>)) -> Self {
        match (operation, close) {
            (Some(operation), Some(close)) => Self::OperationAndClose {
                operation: Box::new(operation),
                close,
            },
            (Some(operation), None) => operation,
            (None, Some(close)) => Self::windows(ClipboardReadOperation::Close, close),
            (None, None) => unreachable!("combined clipboard result must contain an error"),
        }
    }
}

struct ClipboardGuard {
    open: bool,
}

impl ClipboardGuard {
    fn open_with_retry() -> Result<Self, ClipboardReadError> {
        const DELAYS_MS: [u64; 5] = [5, 10, 20, 40, 80];
        let mut last_error = match unsafe { OpenClipboard(None) } {
            Ok(()) => return Ok(Self { open: true }),
            Err(error) => error,
        };
        for delay in DELAYS_MS {
            thread::sleep(std::time::Duration::from_millis(delay));
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self { open: true }),
                Err(error) => last_error = error,
            }
        }
        Err(ClipboardReadError::windows(
            ClipboardReadOperation::Open,
            last_error,
        ))
    }

    fn close(mut self) -> windows::core::Result<()> {
        let result = unsafe { CloseClipboard() };
        self.open = false;
        result
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        if self.open {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }
}

fn read_unicode_text(format: u32) -> Result<Option<String>, ClipboardReadError> {
    read_global_bytes(format)?
        .map(|bytes| decode_unicode_text(&bytes).map_err(ClipboardReadError::InvalidUnicodeText))
        .transpose()
}

fn read_global_bytes(format: u32) -> Result<Option<Vec<u8>>, ClipboardReadError> {
    if unsafe { IsClipboardFormatAvailable(format) }.is_err() {
        return Ok(None);
    }
    let handle = classify_format_read(true, unsafe {
        GetClipboardData(format)
            .map_err(|error| ClipboardReadError::windows(ClipboardReadOperation::GetData, error))
    })?
    .expect("available clipboard format must produce a classified read");
    let memory = HGLOBAL(handle.0);
    let lock = GlobalMemoryLock::new(memory)?;
    let len = unsafe { GlobalSize(memory) };
    if len == 0 {
        return Err(ClipboardReadError::windows(
            ClipboardReadOperation::GlobalSize,
            windows::core::Error::from_win32(),
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts(lock.pointer.cast::<u8>(), len) }.to_vec();
    lock.unlock()?;
    Ok(Some(bytes))
}

struct GlobalMemoryLock {
    memory: HGLOBAL,
    pointer: *mut std::ffi::c_void,
}

impl GlobalMemoryLock {
    fn new(memory: HGLOBAL) -> Result<Self, ClipboardReadError> {
        let pointer = unsafe { GlobalLock(memory) };
        if pointer.is_null() {
            Err(ClipboardReadError::windows(
                ClipboardReadOperation::GlobalLock,
                windows::core::Error::from_win32(),
            ))
        } else {
            Ok(Self { memory, pointer })
        }
    }

    fn unlock(mut self) -> Result<(), ClipboardReadError> {
        let result = global_unlock(self.memory);
        self.pointer = std::ptr::null_mut();
        result.map_err(|error| {
            ClipboardReadError::windows(ClipboardReadOperation::GlobalUnlock, error)
        })
    }
}

impl Drop for GlobalMemoryLock {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            let _ = global_unlock(self.memory);
        }
    }
}

fn global_unlock(memory: HGLOBAL) -> windows::core::Result<()> {
    unsafe {
        SetLastError(ERROR_SUCCESS);
    }
    let unlocked = unsafe { GlobalUnlock(memory) }.is_ok();
    classify_global_unlock(unlocked, unsafe { GetLastError() }.0).map_err(|code| {
        windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code))
    })
}

fn classify_registered_format(format: u32, last_error: u32) -> Result<u32, ClipboardReadError> {
    if format == 0 {
        let code = if last_error == 0 { 1 } else { last_error };
        Err(ClipboardReadError::windows(
            ClipboardReadOperation::RegisterPng,
            windows::core::Error::from_hresult(windows::core::HRESULT::from_win32(code)),
        ))
    } else {
        Ok(format)
    }
}

fn classify_global_unlock(unlocked: bool, last_error: u32) -> Result<(), u32> {
    if unlocked || last_error == ERROR_SUCCESS.0 {
        Ok(())
    } else {
        Err(last_error)
    }
}

fn classify_format_read<T, E>(available: bool, read: Result<T, E>) -> Result<Option<T>, E> {
    if available { read.map(Some) } else { Ok(None) }
}

fn decode_unicode_text(bytes: &[u8]) -> Result<String, UnicodeTextError> {
    if !bytes.len().is_multiple_of(size_of::<u16>()) {
        return Err(UnicodeTextError::OddByteLength);
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
        .collect();
    let nul = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(UnicodeTextError::MissingNul)?;
    String::from_utf16(&units[..nul]).map_err(|_| UnicodeTextError::InvalidUtf16)
}

fn combine_clipboard_operation_and_close<T, OperationError, CloseError>(
    operation: Result<T, OperationError>,
    close: Result<(), CloseError>,
) -> Result<T, (Option<OperationError>, Option<CloseError>)> {
    match (operation, close) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(operation), Ok(())) => Err((Some(operation), None)),
        (Ok(_), Err(close)) => Err((None, Some(close))),
        (Err(operation), Err(close)) => Err((Some(operation), Some(close))),
    }
}

enum ReadyWait<T> {
    Ready(T),
    Timeout,
    Disconnected,
}

fn classify_ready_wait<T>(result: Result<T, RecvTimeoutError>) -> ReadyWait<T> {
    match result {
        Ok(value) => ReadyWait::Ready(value),
        Err(RecvTimeoutError::Timeout) => ReadyWait::Timeout,
        Err(RecvTimeoutError::Disconnected) => ReadyWait::Disconnected,
    }
}

#[cfg(test)]
fn wait_for_listener_ready<T, E>(
    _timeout: Duration,
    mut wait: impl FnMut() -> ReadyWait<T>,
    mut signal: impl FnMut(),
    mut wake: impl FnMut(),
    mut join: impl FnMut() -> Result<(), E>,
) -> Result<T, ()> {
    match wait() {
        ReadyWait::Ready(value) => Ok(value),
        ReadyWait::Timeout | ReadyWait::Disconnected => {
            signal();
            wake();
            let _ = join();
            Err(())
        }
    }
}

pub(super) fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(super) type EventSender = Sender<ClipboardEvent>;
pub(super) type ProductWriteOwnership = Arc<Mutex<ProductWriteState>>;
pub(super) type ReadySender = std::sync::mpsc::SyncSender<Result<isize, ClipboardListenerError>>;
pub(super) type ShutdownReceiver = std::sync::mpsc::Receiver<()>;

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };

    use chrono::Utc;

    use super::{
        ClipboardEvent, MAX_PENDING_PRODUCT_WRITE_EVENTS, ProductWriteGuard, ProductWriteState,
        ReadyWait, classify_format_read, classify_global_unlock, classify_registered_format,
        combine_clipboard_operation_and_close, decode_unicode_text, route_event,
        wait_for_listener_ready,
    };
    use crate::domain::{CapturedClipboard, ContentIdentity, SourceIdentity};

    #[test]
    fn finishing_owned_write_replays_interleaved_external_event() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Armed {
            baseline: 40,
            pending: Vec::new(),
        }));
        route_event(&state, &events, event(41));
        route_event(&state, &events, event(42));

        ProductWriteGuard {
            state,
            events,
            finished: false,
        }
        .finish(42);

        assert_eq!(receiver.recv().unwrap().sequence_number, 41);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn notification_after_finish_is_suppressed_once() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Armed {
            baseline: 50,
            pending: Vec::new(),
        }));

        ProductWriteGuard {
            state: Arc::clone(&state),
            events: events.clone(),
            finished: false,
        }
        .finish(51);

        route_event(&state, &events, event(51));
        assert!(receiver.try_recv().is_err());
        route_event(&state, &events, event(52));
        assert_eq!(receiver.recv().unwrap().sequence_number, 52);
    }

    #[test]
    fn dropping_unfinished_write_replays_pending_events() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Armed {
            baseline: 60,
            pending: Vec::new(),
        }));
        route_event(&state, &events, event(61));

        drop(ProductWriteGuard {
            state,
            events,
            finished: false,
        });

        assert_eq!(receiver.recv().unwrap().sequence_number, 61);
    }

    #[test]
    fn unchanged_baseline_does_not_suppress_next_external_event() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Armed {
            baseline: 65,
            pending: Vec::new(),
        }));

        ProductWriteGuard {
            state: Arc::clone(&state),
            events: events.clone(),
            finished: false,
        }
        .finish(65);

        route_event(&state, &events, event(66));
        assert_eq!(receiver.recv().unwrap().sequence_number, 66);
    }

    #[test]
    fn owned_write_pending_events_are_bounded_without_losing_order() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Armed {
            baseline: 70,
            pending: Vec::new(),
        }));
        let event_count = MAX_PENDING_PRODUCT_WRITE_EVENTS + 5;

        for offset in 1..=event_count {
            route_event(&state, &events, event(70 + offset as u32));
        }

        let pending_len = match &*state.lock().unwrap() {
            ProductWriteState::Armed { pending, .. } => pending.len(),
            state => panic!("expected armed state, got {state:?}"),
        };
        assert_eq!(pending_len, MAX_PENDING_PRODUCT_WRITE_EVENTS);

        ProductWriteGuard {
            state,
            events,
            finished: false,
        }
        .finish(70 + event_count as u32);

        let observed: Vec<_> = receiver
            .try_iter()
            .map(|event| event.sequence_number)
            .collect();
        assert_eq!(observed, (71..70 + event_count as u32).collect::<Vec<_>>());
    }

    #[test]
    fn ready_timeout_signals_wakes_and_joins_before_returning() {
        let calls = RefCell::new(Vec::new());
        let outcome = wait_for_listener_ready(
            Duration::ZERO,
            || ReadyWait::<()>::Timeout,
            || calls.borrow_mut().push("signal"),
            || calls.borrow_mut().push("wake"),
            || {
                calls.borrow_mut().push("join");
                Ok::<(), ()>(())
            },
        );

        assert!(outcome.is_err());
        assert_eq!(*calls.borrow(), ["signal", "wake", "join"]);
    }

    #[test]
    fn zero_registered_format_is_an_error() {
        assert!(classify_registered_format(0, 87).is_err());
        assert_eq!(classify_registered_format(49_152, 0).unwrap(), 49_152);
    }

    #[test]
    fn global_unlock_zero_uses_last_error_to_distinguish_success() {
        assert!(classify_global_unlock(false, 0).is_ok());
        assert!(classify_global_unlock(true, 5).is_ok());
        assert!(classify_global_unlock(false, 5).is_err());
    }

    #[test]
    fn unavailable_format_is_distinct_from_failed_data_read() {
        assert_eq!(classify_format_read::<u8, i32>(false, Ok(7)).unwrap(), None);
        assert!(classify_format_read::<u8, i32>(true, Err(5)).is_err());
        assert_eq!(
            classify_format_read::<i32, i32>(true, Ok(7)).unwrap(),
            Some(7)
        );
    }

    #[test]
    fn unicode_text_requires_even_bytes_and_a_nul_terminator() {
        assert!(decode_unicode_text(&[b'a', 0, 0]).is_err());
        assert!(decode_unicode_text(&[b'a', 0]).is_err());
        assert_eq!(decode_unicode_text(&[b'a', 0, 0, 0]).unwrap(), "a");
    }

    #[test]
    fn clipboard_operation_and_close_failures_are_both_preserved() {
        let error =
            combine_clipboard_operation_and_close::<(), &str, &str>(Err("read"), Err("close"))
                .unwrap_err();
        assert_eq!(error, (Some("read"), Some("close")));
    }

    fn event(sequence_number: u32) -> ClipboardEvent {
        ClipboardEvent {
            sequence_number,
            captured: CapturedClipboard {
                content_identity: ContentIdentity::new(format!("test:{sequence_number}")),
                captured_at: Utc::now(),
                source: SourceIdentity::default(),
                representations: Vec::new(),
            },
        }
    }
}
