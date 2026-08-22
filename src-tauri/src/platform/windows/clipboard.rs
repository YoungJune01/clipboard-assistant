use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    mem::size_of,
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{RecvTimeoutError, Sender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::Utc;
use windows::Win32::{
    Foundation::{
        CloseHandle, ERROR_SUCCESS, GetLastError, GlobalFree, HANDLE, HGLOBAL, SetLastError,
    },
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        Threading::{CreateEventW, SetEvent},
    },
};
use windows::core::{PCWSTR, w};

use crate::domain::{CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity};

use super::message_loop::{ListenerState, run_message_loop, wake_and_join};

const CF_UNICODETEXT_FORMAT: u32 = 13;
const CF_DIBV5_FORMAT: u32 = 17;
const MAX_TEXT_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_IMAGE_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;
const PRODUCT_WRITE_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(2);
const LISTENER_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(3);
const LISTENER_INITIALIZATION_CLEANUP_GRACE: Duration = Duration::from_secs(1);
static NEXT_PRODUCT_WRITE_TRANSACTION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardEvent {
    pub sequence_number: u32,
    pub captured: CapturedClipboard,
}

pub struct ClipboardListener {
    events: Sender<ClipboardEvent>,
    product_write: Arc<Mutex<ProductWriteState>>,
    shutdown: Sender<()>,
    shutdown_event: Option<ShutdownEvent>,
    thread: Option<JoinHandle<Result<(), ClipboardListenerError>>>,
}

#[derive(Clone)]
pub struct ClipboardPublisher {
    events: Sender<ClipboardEvent>,
    product_write: Arc<Mutex<ProductWriteState>>,
}

impl ClipboardListener {
    pub fn start(events: Sender<ClipboardEvent>) -> Result<Self, ClipboardListenerError> {
        let product_write = Arc::new(Mutex::new(ProductWriteState::Idle));
        let thread_product_write = Arc::clone(&product_write);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let (completion_sender, completion_receiver) = sync_channel(1);
        let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
        let shutdown_event = ShutdownEvent::create()?;
        let thread_shutdown_event = shutdown_event.clone();
        let thread_events = events.clone();
        let thread = match thread::Builder::new()
            .name("clipboard-listener".to_owned())
            .spawn(move || {
                let result = run_message_loop(
                    thread_events,
                    thread_product_write,
                    ready_sender,
                    shutdown_receiver,
                    thread_shutdown_event.handle(),
                );
                let _ = completion_sender.send(());
                result
            }) {
            Ok(thread) => thread,
            Err(error) => return Err(ClipboardListenerError::ThreadSpawn(error)),
        };

        let thread = std::cell::RefCell::new(Some(thread));
        orchestrate_listener_initialization(
            || classify_ready_wait(ready_receiver.recv_timeout(LISTENER_INITIALIZATION_TIMEOUT)),
            || shutdown_event.signal(),
            || {
                classify_ready_wait(
                    completion_receiver.recv_timeout(LISTENER_INITIALIZATION_CLEANUP_GRACE),
                )
            },
            || {
                join_listener_thread(
                    thread
                        .borrow_mut()
                        .take()
                        .expect("initialization thread must be available for join"),
                )
            },
        )?;
        Ok(Self {
            events,
            product_write,
            shutdown: shutdown_sender,
            shutdown_event: Some(shutdown_event),
            thread: thread.into_inner(),
        })
    }

    pub fn begin_product_write(&self) -> Result<ProductWriteGuard, ClipboardListenerError> {
        self.publisher().begin_product_write()
    }

    pub fn publisher(&self) -> ClipboardPublisher {
        ClipboardPublisher {
            events: self.events.clone(),
            product_write: Arc::clone(&self.product_write),
        }
    }

    pub fn publish(
        &self,
        representations: &[ClipboardRepresentation],
    ) -> Result<u32, ClipboardWriteError> {
        self.publisher().publish(representations)
    }

    pub fn shutdown(mut self) -> Result<(), ClipboardListenerError> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), ClipboardListenerError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        let event = self.shutdown_event.as_ref().cloned();
        let attempt = wake_and_join(
            || match &event {
                Some(event) => event.signal(),
                None => Ok(()),
            },
            || {
                self.shutdown
                    .send(())
                    .map_err(|_| ShutdownFailure::ControlDisconnected)
            },
            || join_listener_thread(thread),
        );
        drop(event);
        let close = self.shutdown_event.take().map(ShutdownEvent::close);
        combine_shutdown_result(attempt.failures, attempt.thread, close)
    }
}

impl ClipboardPublisher {
    pub fn begin_product_write(&self) -> Result<ProductWriteGuard, ClipboardListenerError> {
        let baseline = unsafe { GetClipboardSequenceNumber() };
        let mut state = lock_unpoisoned(&self.product_write);
        let expired_pending = match &mut *state {
            ProductWriteState::Armed {
                pending, deadline, ..
            } if Instant::now() >= *deadline => {
                let pending = std::mem::take(pending);
                *state = ProductWriteState::Idle;
                pending
            }
            _ => VecDeque::new(),
        };
        if !matches!(*state, ProductWriteState::Idle) {
            return Err(ClipboardListenerError::ProductWriteAlreadyInProgress);
        }
        let armed = ProductWriteState::armed(baseline);
        let transaction_id = armed.transaction_id().expect("armed state has an id");
        *state = armed;
        drop(state);
        for event in expired_pending {
            let _ = self.events.send(event);
        }
        Ok(ProductWriteGuard {
            state: Arc::clone(&self.product_write),
            events: self.events.clone(),
            transaction_id,
            finished: false,
        })
    }

    pub fn publish(
        &self,
        representations: &[ClipboardRepresentation],
    ) -> Result<u32, ClipboardWriteError> {
        validate_representations(representations)?;
        let ownership = self
            .begin_product_write()
            .map_err(ClipboardWriteError::Ownership)?;
        let attempt = write_representations(representations);
        finish_product_write(ownership, &attempt.owned_sequences);
        attempt.result
    }
}

impl crate::services::paste::PasteClipboard for ClipboardPublisher {
    type Error = ClipboardWriteError;

    fn publish(&self, representations: &[ClipboardRepresentation]) -> Result<u32, Self::Error> {
        ClipboardPublisher::publish(self, representations)
    }
}

impl Drop for ClipboardListener {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Debug)]
pub enum ClipboardWriteError {
    Ownership(ClipboardListenerError),
    Empty,
    UnsupportedRepresentation,
    DuplicateRepresentation,
    PayloadTooLarge,
    Windows(ClipboardWriteOperation, windows::core::Error),
    OperationAndClose {
        operation: Box<ClipboardWriteError>,
        close: windows::core::Error,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum ClipboardWriteOperation {
    Open,
    Empty,
    RegisterPng,
    Allocate,
    Lock,
    Unlock,
    Publish,
    Close,
}

impl fmt::Display for ClipboardWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ownership(_) => formatter.write_str("clipboard write ownership is unavailable"),
            Self::Empty => formatter.write_str("no clipboard representation was selected"),
            Self::UnsupportedRepresentation => {
                formatter.write_str("the selected clipboard representation is unsupported")
            }
            Self::DuplicateRepresentation => {
                formatter.write_str("a clipboard representation kind was selected more than once")
            }
            Self::PayloadTooLarge => {
                formatter.write_str("the selected clipboard representation exceeds its size limit")
            }
            Self::Windows(operation, _) => write!(formatter, "clipboard {operation} failed"),
            Self::OperationAndClose { operation, .. } => {
                write!(formatter, "{operation}; closing clipboard also failed")
            }
        }
    }
}

impl fmt::Display for ClipboardWriteOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Open => "open for writing",
            Self::Empty => "clear before writing",
            Self::RegisterPng => "PNG format registration",
            Self::Allocate => "payload allocation",
            Self::Lock => "payload lock",
            Self::Unlock => "payload unlock",
            Self::Publish => "data publication",
            Self::Close => "close after writing",
        })
    }
}

impl Error for ClipboardWriteError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ownership(error) => Some(error),
            Self::Windows(_, error) => Some(error),
            Self::OperationAndClose { operation, close } => {
                let _ = close;
                Some(operation)
            }
            Self::Empty
            | Self::UnsupportedRepresentation
            | Self::DuplicateRepresentation
            | Self::PayloadTooLarge => None,
        }
    }
}

fn validate_representations(
    representations: &[ClipboardRepresentation],
) -> Result<(), ClipboardWriteError> {
    if representations.is_empty() {
        return Err(ClipboardWriteError::Empty);
    }
    for (index, representation) in representations.iter().enumerate() {
        if representations[..index]
            .iter()
            .any(|existing| existing.has_same_kind(representation))
        {
            return Err(ClipboardWriteError::DuplicateRepresentation);
        }
        let (size, limit) = match representation {
            ClipboardRepresentation::UnicodeText { text } => (
                text.encode_utf16()
                    .count()
                    .saturating_add(1)
                    .saturating_mul(size_of::<u16>()),
                MAX_TEXT_PAYLOAD_BYTES,
            ),
            ClipboardRepresentation::Png { bytes } | ClipboardRepresentation::DibV5 { bytes } => {
                (bytes.len(), MAX_IMAGE_PAYLOAD_BYTES)
            }
        };
        if size > limit {
            return Err(ClipboardWriteError::PayloadTooLarge);
        }
    }
    Ok(())
}

struct ClipboardWriteAttempt {
    result: Result<u32, ClipboardWriteError>,
    owned_sequences: Vec<u32>,
}

fn finish_product_write(ownership: ProductWriteGuard, owned_sequences: &[u32]) {
    if owned_sequences.is_empty() {
        ownership.cancel();
    } else {
        ownership.finish(owned_sequences);
    }
}

fn write_representations(representations: &[ClipboardRepresentation]) -> ClipboardWriteAttempt {
    let clipboard = match ClipboardWriteGuard::open_with_retry() {
        Ok(clipboard) => clipboard,
        Err(error) => {
            return ClipboardWriteAttempt {
                result: Err(error),
                owned_sequences: Vec::new(),
            };
        }
    };
    let mut owned_sequences = Vec::new();
    let operation = (|| {
        unsafe { EmptyClipboard() }
            .map_err(|error| ClipboardWriteError::Windows(ClipboardWriteOperation::Empty, error))?;
        record_owned_sequence(&mut owned_sequences);
        let mut published = 0usize;
        for representation in representations {
            let (format, bytes) = match representation {
                ClipboardRepresentation::UnicodeText { text } => {
                    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            wide.as_ptr().cast::<u8>(),
                            wide.len() * size_of::<u16>(),
                        )
                    }
                    .to_vec();
                    (CF_UNICODETEXT_FORMAT, bytes)
                }
                ClipboardRepresentation::Png { bytes } => {
                    unsafe { SetLastError(ERROR_SUCCESS) };
                    let format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
                    if format == 0 {
                        return Err(ClipboardWriteError::Windows(
                            ClipboardWriteOperation::RegisterPng,
                            windows::core::Error::from_win32(),
                        ));
                    }
                    (format, bytes.clone())
                }
                ClipboardRepresentation::DibV5 { bytes } => (CF_DIBV5_FORMAT, bytes.clone()),
            };
            publish_global_bytes(format, &bytes)?;
            record_owned_sequence(&mut owned_sequences);
            published += 1;
        }
        if published == 0 {
            Err(ClipboardWriteError::UnsupportedRepresentation)
        } else {
            Ok(*owned_sequences
                .last()
                .expect("a successful write changed the clipboard"))
        }
    })();
    let close = clipboard.close();
    let result = match (operation, close) {
        (Ok(sequence), Ok(())) => Ok(sequence),
        (Err(operation), Ok(())) => Err(operation),
        (Ok(_), Err(close)) => Err(ClipboardWriteError::Windows(
            ClipboardWriteOperation::Close,
            close,
        )),
        (Err(operation), Err(close)) => Err(ClipboardWriteError::OperationAndClose {
            operation: Box::new(operation),
            close,
        }),
    };
    ClipboardWriteAttempt {
        result,
        owned_sequences,
    }
}

fn record_owned_sequence(owned_sequences: &mut Vec<u32>) {
    let sequence = unsafe { GetClipboardSequenceNumber() };
    if owned_sequences.last().copied() != Some(sequence) {
        owned_sequences.push(sequence);
    }
}

fn publish_global_bytes(format: u32, bytes: &[u8]) -> Result<(), ClipboardWriteError> {
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }
        .map_err(|error| ClipboardWriteError::Windows(ClipboardWriteOperation::Allocate, error))?;
    let pointer = unsafe { GlobalLock(memory) }.cast::<u8>();
    if pointer.is_null() {
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(ClipboardWriteError::Windows(
            ClipboardWriteOperation::Lock,
            windows::core::Error::from_win32(),
        ));
    }
    if !bytes.is_empty() {
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
    }
    if let Err(error) = global_unlock(memory) {
        let _ = unsafe { GlobalFree(Some(memory)) };
        return Err(ClipboardWriteError::Windows(
            ClipboardWriteOperation::Unlock,
            error,
        ));
    }
    if let Err(error) = unsafe { SetClipboardData(format, Some(HANDLE(memory.0))) } {
        let _ = unsafe { GlobalFree(Some(memory)) };
        Err(ClipboardWriteError::Windows(
            ClipboardWriteOperation::Publish,
            error,
        ))
    } else {
        Ok(())
    }
}

struct ClipboardWriteGuard(bool);

impl ClipboardWriteGuard {
    fn open_with_retry() -> Result<Self, ClipboardWriteError> {
        const DELAYS_MS: [u64; 5] = [5, 10, 20, 40, 80];
        let mut last_error = match unsafe { OpenClipboard(None) } {
            Ok(()) => return Ok(Self(true)),
            Err(error) => error,
        };
        for delay in DELAYS_MS {
            thread::sleep(Duration::from_millis(delay));
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self(true)),
                Err(error) => last_error = error,
            }
        }
        Err(ClipboardWriteError::Windows(
            ClipboardWriteOperation::Open,
            last_error,
        ))
    }

    fn close(mut self) -> windows::core::Result<()> {
        let result = unsafe { CloseClipboard() };
        self.0 = result.is_err();
        result
    }
}

impl Drop for ClipboardWriteGuard {
    fn drop(&mut self) {
        if self.0 {
            let _ = unsafe { CloseClipboard() };
        }
    }
}

#[derive(Debug)]
pub enum ClipboardListenerError {
    ThreadSpawn(std::io::Error),
    Windows(windows::core::Error),
    ProductWriteAlreadyInProgress,
    UnexpectedThreadExit,
    ThreadPanicked,
    InitializationTimeout,
    InitializationCleanupPending,
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
            Self::InitializationTimeout => {
                formatter.write_str("clipboard listener initialization timed out")
            }
            Self::InitializationCleanupPending => formatter.write_str(
                "clipboard listener initialization timed out; background cleanup pending",
            ),
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
            | Self::InitializationTimeout
            | Self::InitializationCleanupPending
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

#[derive(Clone)]
struct ShutdownEvent(Arc<ShutdownEventInner>);

struct ShutdownEventInner {
    value: isize,
}

impl ShutdownEvent {
    fn create() -> Result<Self, ClipboardListenerError> {
        let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(ClipboardListenerError::Windows)?;
        Ok(Self(Arc::new(ShutdownEventInner {
            value: event.0 as isize,
        })))
    }

    fn handle(&self) -> HANDLE {
        HANDLE(self.0.value as *mut std::ffi::c_void)
    }

    fn signal(&self) -> Result<(), ShutdownFailure> {
        unsafe { SetEvent(self.handle()) }.map_err(ShutdownFailure::Signal)
    }

    fn close(self) -> Result<(), ShutdownFailure> {
        match Arc::try_unwrap(self.0) {
            Ok(mut event) => event.close(),
            Err(event) => {
                drop(event);
                Ok(())
            }
        }
    }
}

impl Drop for ShutdownEventInner {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl ShutdownEventInner {
    fn close(&mut self) -> Result<(), ShutdownFailure> {
        if self.value == 0 {
            return Ok(());
        }
        let handle = HANDLE(self.value as *mut std::ffi::c_void);
        self.value = 0;
        unsafe { CloseHandle(handle).map_err(ShutdownFailure::CloseEvent) }
    }
}

fn combine_startup_shutdown(
    startup: ClipboardListenerError,
    failures: Vec<ShutdownFailure>,
    joined: Result<(), ClipboardListenerError>,
) -> ClipboardListenerError {
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
    let events_to_send = {
        let mut state = lock_unpoisoned(state);
        match &mut *state {
            ProductWriteState::Armed {
                pending, deadline, ..
            } if Instant::now() >= *deadline => {
                let mut pending = std::mem::take(pending);
                pending.push_back(event);
                *state = ProductWriteState::Idle;
                pending
            }
            ProductWriteState::Armed { pending, .. } => {
                pending.push_back(event);
                VecDeque::new()
            }
            ProductWriteState::Expected { sequence } if *sequence == event.sequence_number => {
                *state = ProductWriteState::Idle;
                VecDeque::new()
            }
            ProductWriteState::Expected { .. } => {
                *state = ProductWriteState::Idle;
                VecDeque::from([event])
            }
            ProductWriteState::Idle => VecDeque::from([event]),
        }
    };
    for event in events_to_send {
        let _ = events.send(event);
    }
}

#[derive(Debug)]
pub(super) enum ProductWriteState {
    Idle,
    Armed {
        transaction_id: u64,
        baseline: u32,
        pending: VecDeque<ClipboardEvent>,
        deadline: Instant,
    },
    Expected {
        sequence: u32,
    },
}

impl ProductWriteState {
    fn armed(baseline: u32) -> Self {
        Self::Armed {
            transaction_id: NEXT_PRODUCT_WRITE_TRANSACTION.fetch_add(1, Ordering::Relaxed),
            baseline,
            pending: VecDeque::new(),
            deadline: Instant::now() + PRODUCT_WRITE_TRANSACTION_TIMEOUT,
        }
    }

    fn transaction_id(&self) -> Option<u64> {
        match self {
            Self::Armed { transaction_id, .. } => Some(*transaction_id),
            _ => None,
        }
    }
}

pub struct ProductWriteGuard {
    state: Arc<Mutex<ProductWriteState>>,
    events: Sender<ClipboardEvent>,
    transaction_id: u64,
    finished: bool,
}

impl ProductWriteGuard {
    pub fn finish(mut self, owned_sequences: &[u32]) {
        let mut state = lock_unpoisoned(&self.state);
        if state.transaction_id() != Some(self.transaction_id) {
            self.finished = true;
            return;
        }
        let previous = std::mem::replace(&mut *state, ProductWriteState::Idle);
        *state = match previous {
            ProductWriteState::Armed {
                baseline, pending, ..
            } => {
                let owned_sequences: Vec<_> = owned_sequences
                    .iter()
                    .copied()
                    .filter(|sequence| *sequence != baseline)
                    .collect();
                let final_sequence = owned_sequences.last().copied();
                let matched = final_sequence.is_some_and(|sequence| {
                    pending
                        .iter()
                        .any(|event| event.sequence_number == sequence)
                });
                let pending_to_send: Vec<_> = pending
                    .into_iter()
                    .filter(|event| !owned_sequences.contains(&event.sequence_number))
                    .collect();
                match final_sequence {
                    Some(sequence) if !matched => {
                        *state = ProductWriteState::Expected { sequence };
                    }
                    _ => *state = ProductWriteState::Idle,
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

    pub fn cancel(mut self) {
        self.cancel_inner();
        self.finished = true;
    }

    fn cancel_inner(&mut self) {
        let pending = {
            let mut state = lock_unpoisoned(&self.state);
            if state.transaction_id() != Some(self.transaction_id) {
                return;
            }
            match std::mem::replace(&mut *state, ProductWriteState::Idle) {
                ProductWriteState::Armed { pending, .. } => pending,
                _ => VecDeque::new(),
            }
        };
        for event in pending {
            let _ = self.events.send(event);
        }
    }
}

impl Drop for ProductWriteGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.cancel_inner();
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
        if let Some(bytes) = read_global_bytes(
            png_format,
            MAX_IMAGE_PAYLOAD_BYTES,
            ClipboardPayloadKind::Image,
        )? {
            representations.push(ClipboardRepresentation::Png { bytes });
        }
        if let Some(bytes) = read_global_bytes(
            CF_DIBV5_FORMAT,
            MAX_IMAGE_PAYLOAD_BYTES,
            ClipboardPayloadKind::Image,
        )? {
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
    PayloadTooLarge {
        kind: ClipboardPayloadKind,
        size: usize,
        limit: usize,
    },
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
            Self::PayloadTooLarge { kind, size, limit } => write!(
                formatter,
                "clipboard {kind} payload size {size} exceeds the supported limit {limit}"
            ),
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
            Self::InvalidUnicodeText(_) | Self::PayloadTooLarge { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipboardPayloadKind {
    UnicodeText,
    Image,
}

impl fmt::Display for ClipboardPayloadKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnicodeText => "Unicode text",
            Self::Image => "image",
        })
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
    read_global_bytes(
        format,
        MAX_TEXT_PAYLOAD_BYTES,
        ClipboardPayloadKind::UnicodeText,
    )?
    .map(|bytes| decode_unicode_text(&bytes).map_err(ClipboardReadError::InvalidUnicodeText))
    .transpose()
}

fn read_global_bytes(
    format: u32,
    max_bytes: usize,
    kind: ClipboardPayloadKind,
) -> Result<Option<Vec<u8>>, ClipboardReadError> {
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
    classify_payload_size(len, max_bytes, kind)?;
    let bytes = unsafe { std::slice::from_raw_parts(lock.pointer.cast::<u8>(), len) }.to_vec();
    lock.unlock()?;
    Ok(Some(bytes))
}

fn classify_payload_size(
    size: usize,
    limit: usize,
    kind: ClipboardPayloadKind,
) -> Result<(), ClipboardReadError> {
    if size <= limit {
        Ok(())
    } else {
        Err(ClipboardReadError::PayloadTooLarge { kind, size, limit })
    }
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

fn orchestrate_listener_initialization<T>(
    wait: impl FnOnce() -> ReadyWait<Result<T, ClipboardListenerError>>,
    signal: impl FnOnce() -> Result<(), ShutdownFailure>,
    wait_completion: impl FnOnce() -> ReadyWait<()>,
    join: impl FnOnce() -> Result<(), ClipboardListenerError>,
) -> Result<T, ClipboardListenerError> {
    let startup = match wait() {
        ReadyWait::Ready(Ok(value)) => return Ok(value),
        ReadyWait::Ready(Err(error)) => {
            return Err(combine_startup_shutdown(error, Vec::new(), join()));
        }
        ReadyWait::Disconnected => {
            return Err(combine_startup_shutdown(
                ClipboardListenerError::UnexpectedThreadExit,
                Vec::new(),
                join(),
            ));
        }
        ReadyWait::Timeout => ClipboardListenerError::InitializationTimeout,
    };
    let mut failures = Vec::new();
    if let Err(error) = signal() {
        failures.push(error);
    }
    match wait_completion() {
        ReadyWait::Ready(()) | ReadyWait::Disconnected => {
            Err(combine_startup_shutdown(startup, failures, join()))
        }
        ReadyWait::Timeout => {
            // Windows initialization calls are not safely cancellable. The listener thread owns
            // its event clone, observes the already-signaled event once the call returns, and
            // performs thread-affine cleanup before releasing the final handle reference.
            let pending = ClipboardListenerError::InitializationCleanupPending;
            if failures.is_empty() {
                Err(pending)
            } else {
                Err(ClipboardListenerError::Shutdown {
                    failures,
                    thread: Some(Box::new(pending)),
                })
            }
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
pub(super) type ReadySender = std::sync::mpsc::SyncSender<Result<(), ClipboardListenerError>>;
pub(super) type ShutdownReceiver = std::sync::mpsc::Receiver<()>;

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::VecDeque,
        sync::{Arc, Mutex, mpsc},
        time::{Duration, Instant},
    };

    use chrono::Utc;

    use super::{
        ClipboardEvent, ClipboardListenerError, ClipboardPayloadKind, ClipboardReadError,
        ClipboardWriteError, MAX_IMAGE_PAYLOAD_BYTES, MAX_TEXT_PAYLOAD_BYTES, ProductWriteGuard,
        ProductWriteState, ReadyWait, ShutdownFailure, classify_format_read,
        classify_global_unlock, classify_payload_size, classify_registered_format,
        combine_clipboard_operation_and_close, decode_unicode_text, finish_product_write,
        orchestrate_listener_initialization, route_event, validate_representations,
    };
    use crate::domain::{
        CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity,
    };

    #[test]
    fn finishing_owned_write_replays_interleaved_external_event() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(40)));
        route_event(&state, &events, event(41));
        route_event(&state, &events, event(42));

        product_write_guard(state, events).finish(&[42]);

        assert_eq!(receiver.recv().unwrap().sequence_number, 41);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn notification_after_finish_is_suppressed_once() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(50)));

        product_write_guard(Arc::clone(&state), events.clone()).finish(&[51]);

        route_event(&state, &events, event(51));
        assert!(receiver.try_recv().is_err());
        route_event(&state, &events, event(52));
        assert_eq!(receiver.recv().unwrap().sequence_number, 52);
    }

    #[test]
    fn open_clipboard_failure_cancels_ownership_and_replays_pending_external_events() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(60)));
        route_event(&state, &events, event(61));

        drop(product_write_guard(state, events));

        assert_eq!(receiver.recv().unwrap().sequence_number, 61);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn empty_clipboard_failure_replays_every_pending_external_event() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(70)));
        route_event(&state, &events, event(71));
        route_event(&state, &events, event(72));

        finish_product_write(product_write_guard(state, events), &[]);

        assert_eq!(
            receiver
                .try_iter()
                .map(|event| event.sequence_number)
                .collect::<Vec<_>>(),
            [71, 72]
        );
    }

    #[test]
    fn unchanged_baseline_does_not_suppress_next_external_event() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(65)));

        product_write_guard(Arc::clone(&state), events.clone()).finish(&[65]);

        route_event(&state, &events, event(66));
        assert_eq!(receiver.recv().unwrap().sequence_number, 66);
    }

    #[test]
    fn close_clipboard_failure_after_owned_change_suppresses_only_the_owned_sequence() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(65)));
        route_event(&state, &events, event(66));

        finish_product_write(
            product_write_guard(Arc::clone(&state), events.clone()),
            &[66],
        );

        assert!(receiver.try_recv().is_err());
        route_event(&state, &events, event(67));
        assert_eq!(receiver.recv().unwrap().sequence_number, 67);
    }

    #[test]
    fn cancelling_owned_write_replays_more_than_32_external_events_without_loss() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(70)));
        let event_count = 37;

        for offset in 1..=event_count {
            route_event(&state, &events, event(70 + offset as u32));
        }

        product_write_guard(state, events).cancel();

        let observed: Vec<_> = receiver
            .try_iter()
            .map(|event| event.sequence_number)
            .collect();
        assert_eq!(observed, (71..=70 + event_count as u32).collect::<Vec<_>>());
    }

    #[test]
    fn successful_owned_write_suppresses_only_product_event_after_more_than_32_events() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(80)));
        let product_sequence = 81;
        route_event(&state, &events, event(product_sequence));
        for offset in 2..=37 {
            route_event(&state, &events, event(80 + offset as u32));
        }

        assert!(receiver.try_recv().is_err());
        product_write_guard(state, events).finish(&[product_sequence]);

        let observed: Vec<_> = receiver
            .try_iter()
            .map(|event| event.sequence_number)
            .collect();
        assert_eq!(observed, (82..=117).collect::<Vec<_>>());
    }

    #[test]
    fn expired_owned_write_replays_all_pending_events_and_current_event_in_order() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(120)));
        {
            let mut state = state.lock().unwrap();
            let ProductWriteState::Armed {
                pending, deadline, ..
            } = &mut *state
            else {
                panic!("expected armed product write");
            };
            *pending = VecDeque::from([event(121), event(122)]);
            *deadline = Instant::now() - Duration::from_millis(1);
        }

        route_event(&state, &events, event(123));

        assert_eq!(
            receiver
                .try_iter()
                .map(|event| event.sequence_number)
                .collect::<Vec<_>>(),
            [121, 122, 123]
        );
        assert!(matches!(*state.lock().unwrap(), ProductWriteState::Idle));
    }

    #[test]
    fn expired_guard_cannot_cancel_a_newer_product_write_transaction() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(120)));
        let expired_guard = product_write_guard(Arc::clone(&state), events.clone());
        {
            let mut state = state.lock().unwrap();
            let ProductWriteState::Armed { deadline, .. } = &mut *state else {
                panic!("expected armed product write");
            };
            *deadline = Instant::now() - Duration::from_millis(1);
        }
        route_event(&state, &events, event(121));

        *state.lock().unwrap() = ProductWriteState::armed(130);
        let new_transaction_id = state.lock().unwrap().transaction_id().unwrap();
        route_event(&state, &events, event(131));
        drop(expired_guard);

        match &*state.lock().unwrap() {
            ProductWriteState::Armed {
                transaction_id,
                pending,
                ..
            } => {
                assert_eq!(*transaction_id, new_transaction_id);
                assert_eq!(
                    pending
                        .iter()
                        .map(|event| event.sequence_number)
                        .collect::<Vec<_>>(),
                    [131]
                );
            }
            state => panic!("expected newer armed state, got {state:?}"),
        }

        product_write_guard(state, events).cancel();
        assert_eq!(
            receiver
                .try_iter()
                .map(|event| event.sequence_number)
                .collect::<Vec<_>>(),
            [121, 131]
        );
    }

    fn product_write_guard(
        state: Arc<Mutex<ProductWriteState>>,
        events: mpsc::Sender<ClipboardEvent>,
    ) -> ProductWriteGuard {
        let transaction_id = state
            .lock()
            .unwrap()
            .transaction_id()
            .expect("product write must be armed");
        ProductWriteGuard {
            state,
            events,
            transaction_id,
            finished: false,
        }
    }

    #[test]
    fn payload_limits_accept_boundary_and_reject_one_byte_over() {
        assert!(
            classify_payload_size(
                MAX_TEXT_PAYLOAD_BYTES,
                MAX_TEXT_PAYLOAD_BYTES,
                ClipboardPayloadKind::UnicodeText,
            )
            .is_ok()
        );
        assert!(matches!(
            classify_payload_size(
                MAX_TEXT_PAYLOAD_BYTES + 1,
                MAX_TEXT_PAYLOAD_BYTES,
                ClipboardPayloadKind::UnicodeText,
            ),
            Err(ClipboardReadError::PayloadTooLarge {
                kind: ClipboardPayloadKind::UnicodeText,
                ..
            })
        ));
        assert!(
            classify_payload_size(
                MAX_IMAGE_PAYLOAD_BYTES,
                MAX_IMAGE_PAYLOAD_BYTES,
                ClipboardPayloadKind::Image,
            )
            .is_ok()
        );
        assert!(matches!(
            classify_payload_size(
                MAX_IMAGE_PAYLOAD_BYTES + 1,
                MAX_IMAGE_PAYLOAD_BYTES,
                ClipboardPayloadKind::Image,
            ),
            Err(ClipboardReadError::PayloadTooLarge {
                kind: ClipboardPayloadKind::Image,
                ..
            })
        ));
    }

    #[test]
    fn paste_publication_rejects_duplicate_kinds_before_touching_clipboard() {
        let representations = [
            ClipboardRepresentation::UnicodeText {
                text: "first".to_owned(),
            },
            ClipboardRepresentation::UnicodeText {
                text: "second".to_owned(),
            },
        ];

        assert!(matches!(
            validate_representations(&representations),
            Err(ClipboardWriteError::DuplicateRepresentation)
        ));
    }

    #[test]
    fn paste_publication_enforces_capture_payload_limits() {
        let oversized = ClipboardRepresentation::Png {
            bytes: vec![0; MAX_IMAGE_PAYLOAD_BYTES + 1],
        };

        assert!(matches!(
            validate_representations(&[oversized]),
            Err(ClipboardWriteError::PayloadTooLarge)
        ));
    }

    #[test]
    fn blocked_initializer_returns_after_initialization_deadline() {
        let calls = RefCell::new(Vec::new());
        let outcome = orchestrate_listener_initialization::<isize>(
            || ReadyWait::Timeout,
            || {
                calls.borrow_mut().push("signal");
                Ok(())
            },
            || {
                calls.borrow_mut().push("completion_wait");
                ReadyWait::Timeout
            },
            || {
                calls.borrow_mut().push("join");
                Ok(())
            },
        );

        assert!(matches!(
            outcome,
            Err(ClipboardListenerError::InitializationCleanupPending)
        ));
        assert_eq!(*calls.borrow(), ["signal", "completion_wait"]);
    }

    #[test]
    fn blocked_initializer_wait_is_bounded_by_the_production_deadline() {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (completion_sender, completion_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::channel();
        let initializer = std::thread::spawn(move || {
            release_receiver.recv().unwrap();
            let _ = ready_sender.send(Ok::<(), ClipboardListenerError>(()));
            let _ = completion_sender.send(());
        });
        let started = std::time::Instant::now();
        let outcome = orchestrate_listener_initialization(
            || {
                super::classify_ready_wait(
                    ready_receiver.recv_timeout(std::time::Duration::from_millis(10)),
                )
            },
            || Ok(()),
            || {
                super::classify_ready_wait(
                    completion_receiver.recv_timeout(std::time::Duration::from_millis(10)),
                )
            },
            || panic!("a blocked initializer must not be joined after the cleanup grace period"),
        );

        assert!(matches!(
            outcome,
            Err(ClipboardListenerError::InitializationCleanupPending)
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        release_sender.send(()).unwrap();
        initializer.join().unwrap();
    }

    #[test]
    fn pending_cleanup_preserves_signal_failure_without_attempting_join() {
        let outcome = orchestrate_listener_initialization::<()>(
            || ReadyWait::Timeout,
            || Err(ShutdownFailure::Signal(windows::core::Error::from_win32())),
            || ReadyWait::Timeout,
            || panic!("a blocked initializer must not be joined after the cleanup grace period"),
        );

        let ClipboardListenerError::Shutdown { failures, thread } = outcome.unwrap_err() else {
            panic!("expected pending cleanup with the signal failure preserved");
        };
        assert!(matches!(failures.as_slice(), [ShutdownFailure::Signal(_)]));
        assert!(matches!(
            thread.as_deref(),
            Some(ClipboardListenerError::InitializationCleanupPending)
        ));
    }

    #[test]
    fn initialization_timeout_joins_when_thread_exits_during_grace_period() {
        let calls = RefCell::new(Vec::new());
        let outcome = orchestrate_listener_initialization::<isize>(
            || ReadyWait::Timeout,
            || {
                calls.borrow_mut().push("signal");
                Ok(())
            },
            || {
                calls.borrow_mut().push("completion_wait");
                ReadyWait::Ready(())
            },
            || {
                calls.borrow_mut().push("join");
                Ok(())
            },
        );

        assert!(matches!(
            outcome,
            Err(ClipboardListenerError::InitializationTimeout)
        ));
        assert_eq!(*calls.borrow(), ["signal", "completion_wait", "join"]);
    }

    #[test]
    fn initialization_success_skips_timeout_cleanup() {
        let calls = RefCell::new(Vec::new());
        let outcome = orchestrate_listener_initialization(
            || ReadyWait::Ready(Ok::<_, ClipboardListenerError>(41isize)),
            || {
                calls.borrow_mut().push("signal");
                Ok(())
            },
            || {
                calls.borrow_mut().push("completion_wait");
                ReadyWait::Ready(())
            },
            || {
                calls.borrow_mut().push("join");
                Ok(())
            },
        );

        assert_eq!(outcome.unwrap(), 41);
        assert!(calls.borrow().is_empty());
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
