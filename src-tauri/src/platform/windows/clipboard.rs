use std::{
    collections::VecDeque,
    error::Error,
    fmt,
    mem::size_of,
    path::Path,
    ptr,
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{RecvTimeoutError, Sender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use chrono::Utc;
use windows::Win32::{
    Foundation::{
        CloseHandle, ERROR_SUCCESS, GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, SetLastError,
    },
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
        Threading::{
            CreateEventW, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            QueryFullProcessImageNameW, SetEvent,
        },
    },
    UI::{
        Shell::{DROPFILES, DragQueryFileW, HDROP},
        WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    },
};
use windows::core::{PCWSTR, PWSTR, w};

use crate::domain::{CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity};
use crate::services::session_records::{
    CaptureStatus, INGESTION_QUEUE_BYTES, INGESTION_QUEUE_EVENTS, MAX_CAPTURE_RECORD_BYTES,
    SessionRecordStore, checked_representation_bytes, representation_bytes,
};

use super::message_loop::{ListenerState, run_message_loop, wake_and_join};

const CF_UNICODETEXT_FORMAT: u32 = 13;
const CF_HDROP_FORMAT: u32 = 15;
const CF_DIBV5_FORMAT: u32 = 17;
const MAX_TEXT_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_IMAGE_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_CAPTURE_PAYLOAD_BYTES: usize = MAX_CAPTURE_RECORD_BYTES;
// Resource boundary only; file contents are not copied.
const MAX_CAPTURE_FILE_COUNT: usize = 4096;
const FILE_PATH_RESOURCE_OVERHEAD_BYTES: usize = size_of::<String>();
const PRODUCT_WRITE_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(2);
const PRODUCT_WRITE_PENDING_BYTES: usize = 16 * 1024 * 1024;
const MAX_PENDING_EVENTS: usize = 1024;
const CLIPBOARD_EVENT_OVERHEAD_BYTES: usize = 64;
const LISTENER_INITIALIZATION_TIMEOUT: Duration = Duration::from_secs(3);
const LISTENER_INITIALIZATION_CLEANUP_GRACE: Duration = Duration::from_secs(1);
static NEXT_PRODUCT_WRITE_TRANSACTION: AtomicU64 = AtomicU64::new(1);
static REGISTERED_FORMATS: OnceLock<Result<RegisteredFormats, u32>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RegisteredFormats {
    png: u32,
    rtf: u32,
    html: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardEvent {
    pub sequence_number: u32,
    pub captured: CapturedClipboard,
}

pub struct ClipboardListener {
    events: EventSender,
    product_write: Arc<Mutex<ProductWriteState>>,
    shutdown: Sender<()>,
    shutdown_event: Option<ShutdownEvent>,
    thread: Option<JoinHandle<Result<(), ClipboardListenerError>>>,
}

#[derive(Clone)]
pub struct ClipboardPublisher {
    events: EventSender,
    product_write: Arc<Mutex<ProductWriteState>>,
}

impl ClipboardListener {
    pub fn start(events: impl Into<EventSender>) -> Result<Self, ClipboardListenerError> {
        let events = events.into();
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
                registered_formats().map_err(ClipboardListenerError::Windows)?;
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
        self.begin_product_write_with_timeout(PRODUCT_WRITE_TRANSACTION_TIMEOUT)
    }

    fn begin_product_write_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ProductWriteGuard, ClipboardListenerError> {
        let baseline = unsafe { GetClipboardSequenceNumber() };
        begin_product_write_transaction(
            Arc::clone(&self.product_write),
            self.events.clone(),
            baseline,
            timeout,
        )
    }
}

fn begin_product_write_transaction(
    product_write: Arc<Mutex<ProductWriteState>>,
    events: impl Into<EventSender>,
    baseline: u32,
    timeout: Duration,
) -> Result<ProductWriteGuard, ClipboardListenerError> {
    let events = events.into();
    begin_product_write_transaction_with_spawner(
        product_write,
        events,
        baseline,
        timeout,
        spawn_product_write_timeout,
    )
}

fn begin_product_write_transaction_with_spawner(
    product_write: Arc<Mutex<ProductWriteState>>,
    events: impl Into<EventSender>,
    baseline: u32,
    timeout: Duration,
    spawn_timeout: impl FnOnce(
        Arc<Mutex<ProductWriteState>>,
        EventSender,
        u64,
        Duration,
        std::sync::mpsc::Receiver<()>,
    ) -> Result<JoinHandle<()>, ClipboardListenerError>,
) -> Result<ProductWriteGuard, ClipboardListenerError> {
    let events = events.into();
    let mut state = lock_unpoisoned(&product_write);
    let previously_expected = match *state {
        ProductWriteState::Expected { sequence } => Some(sequence),
        ProductWriteState::Idle => None,
        ProductWriteState::Armed { .. } => {
            return Err(ClipboardListenerError::ProductWriteAlreadyInProgress);
        }
    };
    let mut armed = ProductWriteState::armed(baseline);
    if let (
        Some(sequence),
        ProductWriteState::Armed {
            owned_sequences, ..
        },
    ) = (previously_expected, &mut armed)
    {
        owned_sequences.push(sequence);
    }
    let transaction_id = armed.transaction_id().expect("armed state has an id");
    *state = armed;
    let (cancel_timeout, timeout_cancelled) = sync_channel(1);
    let timeout_thread = match spawn_timeout(
        Arc::clone(&product_write),
        events.clone(),
        transaction_id,
        timeout,
        timeout_cancelled,
    ) {
        Ok(thread) => thread,
        Err(error) => {
            if state.transaction_id() == Some(transaction_id) {
                *state = ProductWriteState::Idle;
            }
            return Err(error);
        }
    };
    drop(state);
    Ok(ProductWriteGuard {
        state: product_write,
        events,
        transaction_id,
        cancel_timeout: Some(cancel_timeout),
        timeout_thread: Some(timeout_thread),
        finished: false,
    })
}

impl ClipboardPublisher {
    pub fn publish(
        &self,
        representations: &[ClipboardRepresentation],
    ) -> Result<u32, ClipboardWriteError> {
        validate_representations(representations)?;
        let ownership = self
            .begin_product_write()
            .map_err(ClipboardWriteError::Ownership)?;
        let attempt = write_representations(representations, ownership);
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
    InvalidFileList,
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
    RegisterFormats,
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
            Self::InvalidFileList => {
                formatter.write_str("the selected clipboard file list is invalid")
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
            Self::RegisterFormats => "format registration",
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
            | Self::InvalidFileList
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
    if checked_representation_bytes(representations)
        .is_none_or(|bytes| bytes > MAX_CAPTURE_RECORD_BYTES)
    {
        return Err(ClipboardWriteError::PayloadTooLarge);
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
            ClipboardRepresentation::Rtf { bytes }
            | ClipboardRepresentation::Html { bytes }
            | ClipboardRepresentation::Png { bytes }
            | ClipboardRepresentation::DibV5 { bytes } => (bytes.len(), MAX_IMAGE_PAYLOAD_BYTES),
            ClipboardRepresentation::FileList { paths } => {
                validate_file_paths(paths)?;
                (dropfiles_size(paths)?, MAX_CAPTURE_PAYLOAD_BYTES)
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
}

fn finish_product_write(ownership: ProductWriteGuard, owned_sequences: &[u32]) {
    if owned_sequences.is_empty() {
        ownership.cancel();
    } else {
        ownership.finish(owned_sequences);
    }
}

fn write_representations(
    representations: &[ClipboardRepresentation],
    mut ownership: ProductWriteGuard,
) -> ClipboardWriteAttempt {
    let formats = match registered_formats_for_write() {
        Ok(formats) => formats,
        Err(error) => {
            ownership.cancel();
            return ClipboardWriteAttempt { result: Err(error) };
        }
    };
    let prepared = match prepare_representations(representations, formats) {
        Ok(prepared) => prepared,
        Err(error) => {
            ownership.cancel();
            return ClipboardWriteAttempt { result: Err(error) };
        }
    };
    let (mut pending, clipboard) = match prepare_then_open(
        &prepared,
        &Win32GlobalAllocator,
        ClipboardWriteGuard::open_with_retry,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            ownership.cancel();
            return ClipboardWriteAttempt { result: Err(error) };
        }
    };
    let mut owned_sequences = Vec::new();
    let operation = transfer_pending_payloads(
        &mut pending,
        &mut Win32ClipboardTransfer {
            ownership: &mut ownership,
            owned_sequences: &mut owned_sequences,
        },
    )
    .map(|()| {
        *owned_sequences
            .last()
            .expect("a successful write changed the clipboard")
    });
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
    finish_product_write(ownership, &owned_sequences);
    ClipboardWriteAttempt { result }
}

struct PreparedClipboardPayload {
    format: u32,
    bytes: Vec<u8>,
}

trait ClipboardPendingMemory {
    type Handle: Copy;

    fn handle(&self) -> Self::Handle;
    fn transfer(&mut self);
}

trait GlobalMemoryApi: Clone {
    type Handle: Copy;

    fn unlock(&self, handle: Self::Handle) -> windows::core::Result<()>;
    fn free(&self, handle: Self::Handle);
}

struct OwnedGlobalMemory<A: GlobalMemoryApi> {
    handle: Option<A::Handle>,
    locked: bool,
    api: A,
}

impl<A: GlobalMemoryApi> OwnedGlobalMemory<A> {
    fn new(handle: A::Handle, api: A) -> Self {
        Self {
            handle: Some(handle),
            locked: false,
            api,
        }
    }

    fn mark_locked(&mut self) {
        self.locked = true;
    }

    fn unlock(&mut self) -> windows::core::Result<()> {
        if !self.locked {
            return Ok(());
        }
        let result = self
            .api
            .unlock(self.handle.expect("owned global memory must have a handle"));
        if result.is_ok() {
            self.locked = false;
        }
        result
    }

    #[cfg(test)]
    fn is_locked(&self) -> bool {
        self.locked
    }
}

#[cfg(test)]
#[derive(Clone)]
struct FakeableGlobalMemoryApi {
    outcomes: Arc<Mutex<VecDeque<bool>>>,
    operations: Arc<Mutex<Vec<&'static str>>>,
}

#[cfg(test)]
impl FakeableGlobalMemoryApi {
    fn new(outcomes: impl IntoIterator<Item = bool>) -> Self {
        Self {
            outcomes: Arc::new(Mutex::new(outcomes.into_iter().collect())),
            operations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn operations(&self) -> Vec<&'static str> {
        self.operations.lock().unwrap().clone()
    }
}

#[cfg(test)]
impl GlobalMemoryApi for FakeableGlobalMemoryApi {
    type Handle = usize;

    fn unlock(&self, _handle: Self::Handle) -> windows::core::Result<()> {
        self.operations.lock().unwrap().push("unlock");
        if self.outcomes.lock().unwrap().pop_front().unwrap_or(true) {
            Ok(())
        } else {
            Err(windows::core::Error::from_win32())
        }
    }

    fn free(&self, _handle: Self::Handle) {
        self.operations.lock().unwrap().push("free");
    }
}

#[cfg(test)]
impl OwnedGlobalMemory<FakeableGlobalMemoryApi> {
    fn new_for_test(handle: usize, locked: bool, api: FakeableGlobalMemoryApi) -> Self {
        Self {
            handle: Some(handle),
            locked,
            api,
        }
    }
}

impl<A: GlobalMemoryApi> ClipboardPendingMemory for OwnedGlobalMemory<A> {
    type Handle = A::Handle;

    fn handle(&self) -> Self::Handle {
        assert!(!self.locked, "locked global memory cannot be transferred");
        self.handle
            .expect("pending global memory must own its handle")
    }

    fn transfer(&mut self) {
        assert!(!self.locked, "locked global memory cannot be transferred");
        self.handle = None;
    }
}

impl<A: GlobalMemoryApi> Drop for OwnedGlobalMemory<A> {
    fn drop(&mut self) {
        if self.locked && self.unlock().is_err() {
            return;
        }
        if let Some(handle) = self.handle.take() {
            self.api.free(handle);
        }
    }
}

trait ClipboardGlobalAllocator {
    type Memory: ClipboardPendingMemory;

    fn allocate_and_copy(&self, bytes: &[u8]) -> Result<Self::Memory, ClipboardWriteError>;
}

struct PendingClipboardPayload<M> {
    format: u32,
    memory: M,
}

type PreparedClipboardWrite<M, O> = (Vec<PendingClipboardPayload<M>>, O);

fn prepare_pending_payloads<A: ClipboardGlobalAllocator>(
    prepared: &[PreparedClipboardPayload],
    allocator: &A,
) -> Result<Vec<PendingClipboardPayload<A::Memory>>, ClipboardWriteError> {
    prepared
        .iter()
        .map(|payload| {
            Ok(PendingClipboardPayload {
                format: payload.format,
                memory: allocator.allocate_and_copy(&payload.bytes)?,
            })
        })
        .collect()
}

fn prepare_then_open<A, O, F>(
    prepared: &[PreparedClipboardPayload],
    allocator: &A,
    open: F,
) -> Result<PreparedClipboardWrite<A::Memory, O>, ClipboardWriteError>
where
    A: ClipboardGlobalAllocator,
    F: FnOnce() -> Result<O, ClipboardWriteError>,
{
    let pending = prepare_pending_payloads(prepared, allocator)?;
    let clipboard = open()?;
    Ok((pending, clipboard))
}

#[cfg(test)]
fn prepare_representations_then_open<A, O, F>(
    representations: &[ClipboardRepresentation],
    formats: RegisteredFormats,
    allocator: &A,
    open: F,
) -> Result<PreparedClipboardWrite<A::Memory, O>, ClipboardWriteError>
where
    A: ClipboardGlobalAllocator,
    F: FnOnce() -> Result<O, ClipboardWriteError>,
{
    let prepared = prepare_representations(representations, formats)?;
    prepare_then_open(&prepared, allocator, open)
}

trait ClipboardTransferApi<H: Copy> {
    fn empty(&mut self) -> Result<(), ClipboardWriteError>;
    fn set(&mut self, format: u32, handle: H) -> Result<(), ClipboardWriteError>;
}

fn transfer_pending_payloads<M: ClipboardPendingMemory>(
    pending: &mut [PendingClipboardPayload<M>],
    api: &mut impl ClipboardTransferApi<M::Handle>,
) -> Result<(), ClipboardWriteError> {
    api.empty()?;
    for payload in pending {
        api.set(payload.format, payload.memory.handle())?;
        payload.memory.transfer();
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Win32GlobalMemoryApi;

impl GlobalMemoryApi for Win32GlobalMemoryApi {
    type Handle = HGLOBAL;

    fn unlock(&self, handle: Self::Handle) -> windows::core::Result<()> {
        global_unlock(handle)
    }

    fn free(&self, handle: Self::Handle) {
        let _ = unsafe { GlobalFree(Some(handle)) };
    }
}

struct Win32GlobalAllocator;

impl ClipboardGlobalAllocator for Win32GlobalAllocator {
    type Memory = OwnedGlobalMemory<Win32GlobalMemoryApi>;

    fn allocate_and_copy(&self, bytes: &[u8]) -> Result<Self::Memory, ClipboardWriteError> {
        let memory =
            unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }.map_err(|error| {
                ClipboardWriteError::Windows(ClipboardWriteOperation::Allocate, error)
            })?;
        let mut pending = OwnedGlobalMemory::new(memory, Win32GlobalMemoryApi);
        let pointer = unsafe { GlobalLock(memory) }.cast::<u8>();
        if pointer.is_null() {
            return Err(ClipboardWriteError::Windows(
                ClipboardWriteOperation::Lock,
                windows::core::Error::from_win32(),
            ));
        }
        pending.mark_locked();
        if !bytes.is_empty() {
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
        }
        pending.unlock().map_err(|error| {
            ClipboardWriteError::Windows(ClipboardWriteOperation::Unlock, error)
        })?;
        Ok(pending)
    }
}

struct Win32ClipboardTransfer<'a> {
    ownership: &'a mut ProductWriteGuard,
    owned_sequences: &'a mut Vec<u32>,
}

impl ClipboardTransferApi<HGLOBAL> for Win32ClipboardTransfer<'_> {
    fn empty(&mut self) -> Result<(), ClipboardWriteError> {
        self.ownership
            .perform_owned_change(self.owned_sequences, || {
                unsafe { EmptyClipboard() }.map_err(|error| {
                    ClipboardWriteError::Windows(ClipboardWriteOperation::Empty, error)
                })
            })
    }

    fn set(&mut self, format: u32, memory: HGLOBAL) -> Result<(), ClipboardWriteError> {
        self.ownership
            .perform_owned_change(self.owned_sequences, || {
                unsafe { SetClipboardData(format, Some(HANDLE(memory.0))) }.map_err(|error| {
                    ClipboardWriteError::Windows(ClipboardWriteOperation::Publish, error)
                })?;
                Ok(())
            })
    }
}

fn prepare_representations(
    representations: &[ClipboardRepresentation],
    formats: RegisteredFormats,
) -> Result<Vec<PreparedClipboardPayload>, ClipboardWriteError> {
    validate_representations(representations)?;
    let prepared: Vec<_> = representations
        .iter()
        .map(|representation| {
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
                ClipboardRepresentation::Rtf { bytes } => (formats.rtf, bytes.clone()),
                ClipboardRepresentation::Html { bytes } => (formats.html, bytes.clone()),
                ClipboardRepresentation::Png { bytes } => (formats.png, bytes.clone()),
                ClipboardRepresentation::DibV5 { bytes } => (CF_DIBV5_FORMAT, bytes.clone()),
                ClipboardRepresentation::FileList { paths } => {
                    (CF_HDROP_FORMAT, serialize_dropfiles(paths)?)
                }
            };
            Ok(PreparedClipboardPayload { format, bytes })
        })
        .collect::<Result<_, ClipboardWriteError>>()?;
    let prepared_bytes = prepared.iter().try_fold(0_usize, |total, payload| {
        total
            .checked_add(payload.bytes.len())
            .and_then(|bytes| {
                bytes.checked_add(crate::services::session_records::REPRESENTATION_OVERHEAD_BYTES)
            })
            .ok_or(ClipboardWriteError::PayloadTooLarge)
    })?;
    if prepared_bytes > MAX_CAPTURE_RECORD_BYTES {
        return Err(ClipboardWriteError::PayloadTooLarge);
    }
    Ok(prepared)
}

fn validate_file_paths(paths: &[String]) -> Result<(), ClipboardWriteError> {
    if paths.is_empty()
        || paths
            .iter()
            .any(|path| path.is_empty() || path.encode_utf16().any(|unit| unit == 0))
    {
        Err(ClipboardWriteError::InvalidFileList)
    } else {
        Ok(())
    }
}

fn dropfiles_size(paths: &[String]) -> Result<usize, ClipboardWriteError> {
    validate_file_paths(paths)?;
    let units = paths.iter().try_fold(1_usize, |total, path| {
        total.checked_add(path.encode_utf16().count().checked_add(1)?)
    });
    units
        .and_then(|units| units.checked_mul(size_of::<u16>()))
        .and_then(|bytes| bytes.checked_add(size_of::<DROPFILES>()))
        .ok_or(ClipboardWriteError::PayloadTooLarge)
}

fn registered_formats_for_write() -> Result<RegisteredFormats, ClipboardWriteError> {
    registered_formats().map_err(|error| {
        ClipboardWriteError::Windows(ClipboardWriteOperation::RegisterFormats, error)
    })
}

fn registered_formats() -> windows::core::Result<RegisteredFormats> {
    match REGISTERED_FORMATS.get_or_init(|| {
        let png = register_clipboard_format(w!("PNG"))?;
        let rtf = register_clipboard_format(w!("Rich Text Format"))?;
        let html = register_clipboard_format(w!("HTML Format"))?;
        Ok(RegisteredFormats { png, rtf, html })
    }) {
        Ok(formats) => Ok(*formats),
        Err(code) => Err(windows::core::Error::from_hresult(
            windows::core::HRESULT::from_win32(*code),
        )),
    }
}

fn register_clipboard_format(name: PCWSTR) -> Result<u32, u32> {
    unsafe { SetLastError(ERROR_SUCCESS) };
    let format = unsafe { RegisterClipboardFormatW(name) };
    if format != 0 {
        Ok(format)
    } else {
        let error = unsafe { GetLastError() }.0;
        Err(if error == ERROR_SUCCESS.0 { 1 } else { error })
    }
}

fn serialize_dropfiles(paths: &[String]) -> Result<Vec<u8>, ClipboardWriteError> {
    let total_bytes = dropfiles_size(paths)?;
    let header_size = size_of::<DROPFILES>();
    let p_files = u32::try_from(header_size).map_err(|_| ClipboardWriteError::PayloadTooLarge)?;
    let header = DROPFILES {
        pFiles: p_files,
        fWide: true.into(),
        ..Default::default()
    };
    let mut bytes = Vec::with_capacity(total_bytes);
    let header_bytes = unsafe {
        std::slice::from_raw_parts((&header as *const DROPFILES).cast::<u8>(), header_size)
    };
    bytes.extend_from_slice(header_bytes);
    for path in paths {
        for unit in path.encode_utf16().chain([0]) {
            bytes.extend_from_slice(&unit.to_ne_bytes());
        }
    }
    bytes.extend_from_slice(&0_u16.to_ne_bytes());
    debug_assert_eq!(bytes.len(), total_bytes);
    Ok(bytes)
}

fn record_owned_sequence(owned_sequences: &mut Vec<u32>) {
    let sequence = unsafe { GetClipboardSequenceNumber() };
    if owned_sequences.last().copied() != Some(sequence) {
        owned_sequences.push(sequence);
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
    capture_update_with_source(state, &Win32ForegroundSourceApi);
}

trait ForegroundSourceApi {
    fn foreground_window(&self) -> Option<HWND>;
    fn process_id(&self, window: HWND) -> Option<u32>;
    fn executable_path(&self, process_id: u32) -> Option<String>;
}

struct Win32ForegroundSourceApi;

impl ForegroundSourceApi for Win32ForegroundSourceApi {
    fn foreground_window(&self) -> Option<HWND> {
        let window = unsafe { GetForegroundWindow() };
        (!window.0.is_null()).then_some(window)
    }

    fn process_id(&self, window: HWND) -> Option<u32> {
        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(window, Some(&mut process_id)) };
        (thread_id != 0 && process_id != 0).then_some(process_id)
    }

    fn executable_path(&self, process_id: u32) -> Option<String> {
        let process =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()? };
        let process = SourceProcessHandle(process);
        let mut buffer = vec![0_u16; 32_768];
        let mut length = buffer.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                process.0,
                Default::default(),
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        }
        .ok()?;
        String::from_utf16(&buffer[..length as usize]).ok()
    }
}

struct SourceProcessHandle(HANDLE);

impl Drop for SourceProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

fn capture_source(api: &impl ForegroundSourceApi) -> SourceIdentity {
    let Some(path) = api
        .foreground_window()
        .and_then(|window| api.process_id(window))
        .and_then(|process_id| api.executable_path(process_id))
        .filter(|path| !path.is_empty())
    else {
        return SourceIdentity::default();
    };
    let application_name = Path::new(&path)
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    SourceIdentity {
        application_name,
        executable_path: Some(path),
    }
}

fn capture_update_with_source(state: &ListenerState, source_api: &impl ForegroundSourceApi) {
    let source = capture_source(source_api);
    let Ok((sequence_number, representations)) = read_supported_representations() else {
        return;
    };
    if representations.is_empty() {
        return;
    }
    let captured = CapturedClipboard {
        content_identity: ContentIdentity::new(format!("clipboard-sequence:{sequence_number}")),
        captured_at: Utc::now(),
        source,
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
    events: &impl ClipboardEventSink,
    event: ClipboardEvent,
) {
    let mut state = lock_unpoisoned(state);
    match &mut *state {
        ProductWriteState::Armed {
            pending,
            pending_bytes,
            pending_budget,
            pending_events,
            pending_event_limit,
            owned_sequences,
            ..
        } => {
            if owned_sequences.contains(&event.sequence_number) {
                return;
            }
            let event_bytes = clipboard_event_bytes(&event);
            if event_bytes
                .and_then(|event_bytes| checked_pending_bytes(*pending_bytes, event_bytes))
                .is_some_and(|bytes| bytes <= *pending_budget)
                && pending_events
                    .checked_add(1)
                    .is_some_and(|count| count <= *pending_event_limit)
            {
                *pending_bytes += event_bytes.expect("checked event bytes above");
                *pending_events += 1;
                pending.push_back(event);
            } else {
                let previous = std::mem::replace(&mut *state, ProductWriteState::Idle);
                let ProductWriteState::Armed {
                    pending,
                    owned_sequences,
                    ..
                } = previous
                else {
                    unreachable!("matched armed product write state")
                };
                let mut batch: Vec<_> = pending.into_iter().collect();
                batch.push(event);
                batch.retain(|event| !owned_sequences.contains(&event.sequence_number));
                let _ = events.send_batch(batch);
            }
        }
        ProductWriteState::Expected { sequence } if *sequence == event.sequence_number => {}
        ProductWriteState::Expected { .. } => {
            *state = ProductWriteState::Idle;
            let _ = events.send_event(event);
        }
        ProductWriteState::Idle => {
            let _ = events.send_event(event);
        }
    }
}

#[derive(Debug)]
pub(super) enum ProductWriteState {
    Idle,
    Armed {
        transaction_id: u64,
        baseline: u32,
        pending: VecDeque<ClipboardEvent>,
        pending_bytes: usize,
        pending_budget: usize,
        pending_events: usize,
        pending_event_limit: usize,
        owned_sequences: Vec<u32>,
    },
    Expected {
        sequence: u32,
    },
}

impl ProductWriteState {
    fn armed(baseline: u32) -> Self {
        Self::armed_with_budget(baseline, PRODUCT_WRITE_PENDING_BYTES)
    }

    fn armed_with_budget(baseline: u32, pending_budget: usize) -> Self {
        Self::armed_with_limits(baseline, pending_budget, MAX_PENDING_EVENTS)
    }

    fn armed_with_limits(baseline: u32, pending_budget: usize, pending_event_limit: usize) -> Self {
        Self::Armed {
            transaction_id: NEXT_PRODUCT_WRITE_TRANSACTION.fetch_add(1, Ordering::Relaxed),
            baseline,
            pending: VecDeque::new(),
            pending_bytes: 0,
            pending_budget,
            pending_events: 0,
            pending_event_limit,
            owned_sequences: Vec::new(),
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
    events: EventSender,
    transaction_id: u64,
    cancel_timeout: Option<std::sync::mpsc::SyncSender<()>>,
    timeout_thread: Option<JoinHandle<()>>,
    finished: bool,
}

impl ProductWriteGuard {
    #[cfg(test)]
    fn note_owned_sequences(&mut self, sequences: &[u32]) {
        let mut state = lock_unpoisoned(&self.state);
        if let ProductWriteState::Armed {
            transaction_id,
            owned_sequences,
            ..
        } = &mut *state
            && *transaction_id == self.transaction_id
        {
            for sequence in sequences {
                if !owned_sequences.contains(sequence) {
                    owned_sequences.push(*sequence);
                }
            }
        }
    }

    fn perform_owned_change(
        &mut self,
        sequences: &mut Vec<u32>,
        change: impl FnOnce() -> Result<(), ClipboardWriteError>,
    ) -> Result<(), ClipboardWriteError> {
        let mut state = lock_unpoisoned(&self.state);
        let ProductWriteState::Armed {
            transaction_id,
            owned_sequences,
            ..
        } = &mut *state
        else {
            return Err(ClipboardWriteError::Ownership(
                ClipboardListenerError::ProductWriteAlreadyInProgress,
            ));
        };
        if *transaction_id != self.transaction_id {
            return Err(ClipboardWriteError::Ownership(
                ClipboardListenerError::ProductWriteAlreadyInProgress,
            ));
        }
        change()?;
        record_owned_sequence(sequences);
        if let Some(sequence) = sequences.last().copied()
            && !owned_sequences.contains(&sequence)
        {
            owned_sequences.push(sequence);
        }
        Ok(())
    }

    pub fn finish(mut self, owned_sequences: &[u32]) {
        self.stop_timeout();
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
                let _ = self.events.send_batch(pending_to_send);
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
        self.stop_timeout();
        cancel_product_write(&self.state, &self.events, self.transaction_id);
    }

    fn stop_timeout(&mut self) {
        if let Some(cancel) = self.cancel_timeout.take() {
            let _ = cancel.send(());
        }
        if let Some(thread) = self.timeout_thread.take() {
            let _ = thread.join();
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

fn spawn_product_write_timeout(
    state: Arc<Mutex<ProductWriteState>>,
    events: EventSender,
    transaction_id: u64,
    timeout: Duration,
    cancelled: std::sync::mpsc::Receiver<()>,
) -> Result<JoinHandle<()>, ClipboardListenerError> {
    thread::Builder::new()
        .name("clipboard-product-write-timeout".to_owned())
        .spawn(move || match cancelled.recv_timeout(timeout) {
            Err(RecvTimeoutError::Timeout) => {
                cancel_product_write(&state, &events, transaction_id);
            }
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        })
        .map_err(ClipboardListenerError::ThreadSpawn)
}

fn cancel_product_write(
    state: &Mutex<ProductWriteState>,
    events: &impl ClipboardEventSink,
    transaction_id: u64,
) {
    let mut state = lock_unpoisoned(state);
    if state.transaction_id() != Some(transaction_id) {
        return;
    }
    if let ProductWriteState::Armed { pending, .. } =
        std::mem::replace(&mut *state, ProductWriteState::Idle)
    {
        let _ = events.send_batch(pending.into_iter().collect());
    }
}

fn read_supported_representations()
-> Result<(u32, Vec<ClipboardRepresentation>), ClipboardReadError> {
    let clipboard = ClipboardGuard::open_with_retry()?;
    let operation = (|| {
        let sequence_number = unsafe { GetClipboardSequenceNumber() };
        let formats = registered_formats().map_err(|error| {
            ClipboardReadError::windows(ClipboardReadOperation::RegisterFormats, error)
        })?;
        let representations = read_bounded_representations_with_files(
            &Win32ClipboardMemory,
            &Win32ClipboardFiles,
            formats,
            MAX_CAPTURE_PAYLOAD_BYTES,
        )?;
        Ok((sequence_number, representations))
    })();
    finish_clipboard_read(operation, clipboard)
}

fn finish_clipboard_read<T, C: ClipboardCloser>(
    operation: Result<T, ClipboardReadError>,
    clipboard: ClipboardGuard<C>,
) -> Result<T, ClipboardReadError> {
    combine_clipboard_operation_and_close(operation, clipboard.close())
        .map_err(ClipboardReadError::from_combined)
}

#[derive(Debug)]
enum ClipboardReadError {
    Windows(ClipboardReadOperation, windows::core::Error),
    InvalidFileList,
    OperationAndClose {
        operation: Box<ClipboardReadError>,
        close: windows::core::Error,
    },
}

impl fmt::Display for ClipboardReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows(operation, _) => write!(formatter, "clipboard {operation} failed"),
            Self::InvalidFileList => formatter.write_str("clipboard file list is invalid"),
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
            Self::InvalidFileList => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum ClipboardReadOperation {
    Open,
    RegisterFormats,
    GetData,
    QueryFiles,
    GlobalLock,
    GlobalSize,
    GlobalUnlock,
    Close,
}

impl fmt::Display for ClipboardReadOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Open => "open",
            Self::RegisterFormats => "format registration",
            Self::GetData => "data access",
            Self::QueryFiles => "file list access",
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

    fn is_cleanup_failure(&self) -> bool {
        matches!(
            self,
            Self::Windows(ClipboardReadOperation::GlobalUnlock, _) | Self::OperationAndClose { .. }
        )
    }
}

trait ClipboardCloser: Clone {
    fn close(&self) -> windows::core::Result<()>;
}

#[cfg(test)]
#[derive(Clone)]
struct FakeableClipboardCloser {
    outcomes: Arc<Mutex<VecDeque<bool>>>,
    calls: Arc<Mutex<usize>>,
}

#[cfg(test)]
impl FakeableClipboardCloser {
    fn new(outcomes: impl IntoIterator<Item = bool>) -> Self {
        Self {
            outcomes: Arc::new(Mutex::new(outcomes.into_iter().collect())),
            calls: Arc::new(Mutex::new(0)),
        }
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

#[cfg(test)]
impl ClipboardCloser for FakeableClipboardCloser {
    fn close(&self) -> windows::core::Result<()> {
        *self.calls.lock().unwrap() += 1;
        if self.outcomes.lock().unwrap().pop_front().unwrap_or(true) {
            Ok(())
        } else {
            Err(windows::core::Error::from_win32())
        }
    }
}

#[derive(Clone, Copy)]
struct Win32ClipboardCloser;

impl ClipboardCloser for Win32ClipboardCloser {
    fn close(&self) -> windows::core::Result<()> {
        unsafe { CloseClipboard() }
    }
}

struct ClipboardGuard<C: ClipboardCloser = Win32ClipboardCloser> {
    open: bool,
    closer: C,
}

impl ClipboardGuard {
    fn open_with_retry() -> Result<Self, ClipboardReadError> {
        const DELAYS_MS: [u64; 5] = [5, 10, 20, 40, 80];
        let mut last_error = match unsafe { OpenClipboard(None) } {
            Ok(()) => {
                return Ok(Self {
                    open: true,
                    closer: Win32ClipboardCloser,
                });
            }
            Err(error) => error,
        };
        for delay in DELAYS_MS {
            thread::sleep(std::time::Duration::from_millis(delay));
            match unsafe { OpenClipboard(None) } {
                Ok(()) => {
                    return Ok(Self {
                        open: true,
                        closer: Win32ClipboardCloser,
                    });
                }
                Err(error) => last_error = error,
            }
        }
        Err(ClipboardReadError::windows(
            ClipboardReadOperation::Open,
            last_error,
        ))
    }
}

impl<C: ClipboardCloser> ClipboardGuard<C> {
    fn close(mut self) -> windows::core::Result<()> {
        let result = self.closer.close();
        if result.is_ok() {
            self.open = false;
        }
        result
    }
}

#[cfg(test)]
impl ClipboardGuard<FakeableClipboardCloser> {
    fn new_for_test(closer: FakeableClipboardCloser) -> Self {
        Self { open: true, closer }
    }
}

impl<C: ClipboardCloser> Drop for ClipboardGuard<C> {
    fn drop(&mut self) {
        if self.open && self.closer.close().is_ok() {
            self.open = false;
        }
    }
}

trait ClipboardMemoryReader {
    type Locked;

    fn size(&self, format: u32) -> Result<Option<usize>, ClipboardReadError>;
    fn lock(&self, format: u32) -> Result<Self::Locked, ClipboardReadError>;
    fn locked_size(&self, locked: &Self::Locked) -> Result<usize, ClipboardReadError>;
    fn copy(&self, locked: &Self::Locked, size: usize) -> Result<Vec<u8>, ClipboardReadError>;
    fn unlock(&self, locked: &mut Self::Locked) -> Result<(), ClipboardReadError>;
}

trait ClipboardFileReader {
    fn count(&self) -> Result<Option<usize>, ClipboardReadError>;
    fn path_len(&self, index: usize) -> Result<usize, ClipboardReadError>;
    fn path(&self, index: usize, buffer: &mut [u16]) -> Result<usize, ClipboardReadError>;
}

struct Win32ClipboardMemory;

struct Win32ClipboardFiles;

impl ClipboardMemoryReader for Win32ClipboardMemory {
    type Locked = GlobalMemoryLock;

    fn size(&self, format: u32) -> Result<Option<usize>, ClipboardReadError> {
        if unsafe { IsClipboardFormatAvailable(format) }.is_err() {
            return Ok(None);
        }
        let handle = clipboard_data_handle(format)?;
        global_memory_size(HGLOBAL(handle.0)).map(Some)
    }

    fn lock(&self, format: u32) -> Result<Self::Locked, ClipboardReadError> {
        let handle = clipboard_data_handle(format)?;
        GlobalMemoryLock::new(HGLOBAL(handle.0))
    }

    fn locked_size(&self, locked: &Self::Locked) -> Result<usize, ClipboardReadError> {
        global_memory_size(locked.memory)
    }

    fn copy(&self, lock: &Self::Locked, size: usize) -> Result<Vec<u8>, ClipboardReadError> {
        Ok(unsafe { std::slice::from_raw_parts(lock.pointer.cast::<u8>(), size) }.to_vec())
    }

    fn unlock(&self, lock: &mut Self::Locked) -> Result<(), ClipboardReadError> {
        lock.unlock()
    }
}

impl ClipboardFileReader for Win32ClipboardFiles {
    fn count(&self) -> Result<Option<usize>, ClipboardReadError> {
        if unsafe { IsClipboardFormatAvailable(CF_HDROP_FORMAT) }.is_err() {
            return Ok(None);
        }
        let handle = clipboard_data_handle(CF_HDROP_FORMAT)?;
        let count = unsafe { DragQueryFileW(HDROP(handle.0), u32::MAX, None) };
        Ok(Some(count as usize))
    }

    fn path_len(&self, index: usize) -> Result<usize, ClipboardReadError> {
        let handle = clipboard_data_handle(CF_HDROP_FORMAT)?;
        let index = u32::try_from(index).map_err(|_| ClipboardReadError::InvalidFileList)?;
        let length = unsafe { DragQueryFileW(HDROP(handle.0), index, None) };
        if length == 0 {
            Err(ClipboardReadError::windows(
                ClipboardReadOperation::QueryFiles,
                windows::core::Error::from_win32(),
            ))
        } else {
            Ok(length as usize)
        }
    }

    fn path(&self, index: usize, buffer: &mut [u16]) -> Result<usize, ClipboardReadError> {
        let handle = clipboard_data_handle(CF_HDROP_FORMAT)?;
        let index = u32::try_from(index).map_err(|_| ClipboardReadError::InvalidFileList)?;
        let copied = unsafe { DragQueryFileW(HDROP(handle.0), index, Some(buffer)) } as usize;
        if copied == 0 || copied >= buffer.len() {
            Err(ClipboardReadError::windows(
                ClipboardReadOperation::QueryFiles,
                windows::core::Error::from_win32(),
            ))
        } else {
            Ok(copied)
        }
    }
}

fn clipboard_data_handle(format: u32) -> Result<HANDLE, ClipboardReadError> {
    classify_format_read(true, unsafe {
        GetClipboardData(format)
            .map_err(|error| ClipboardReadError::windows(ClipboardReadOperation::GetData, error))
    })?
    .ok_or_else(|| {
        ClipboardReadError::windows(
            ClipboardReadOperation::GetData,
            windows::core::Error::from_win32(),
        )
    })
}

fn global_memory_size(memory: HGLOBAL) -> Result<usize, ClipboardReadError> {
    let len = unsafe { GlobalSize(memory) };
    if len == 0 {
        return Err(ClipboardReadError::windows(
            ClipboardReadOperation::GlobalSize,
            windows::core::Error::from_win32(),
        ));
    }
    Ok(len)
}

#[cfg(test)]
fn read_bounded_representations(
    reader: &impl ClipboardMemoryReader,
    png_format: u32,
    dib_format: u32,
    budget: usize,
) -> Result<Vec<ClipboardRepresentation>, ClipboardReadError> {
    let text_size = reader.size(CF_UNICODETEXT_FORMAT)?;
    let png_size = reader.size(png_format)?;
    let dib_size = reader.size(dib_format)?;
    let mut representations = Vec::with_capacity(2);

    if let Some(size) = text_size.filter(|size| text_source_fits_budget(*size, budget))
        && let Some(bytes) = copy_unchanged(reader, CF_UNICODETEXT_FORMAT, size)?
        && let Ok(text) = decode_unicode_text(&bytes)
    {
        drop(bytes);
        let representation = ClipboardRepresentation::UnicodeText { text };
        if representation_bytes(std::slice::from_ref(&representation)) <= budget {
            representations.push(representation);
        }
    }

    let used = representation_bytes(&representations);
    let remaining = budget.saturating_sub(used);

    let mut image_candidates: Vec<_> =
        [(png_format, png_size, true), (dib_format, dib_size, false)]
            .into_iter()
            .filter_map(|(format, size, png)| size.map(|size| (format, size, png)))
            .filter(|(_, size, _)| {
                *size <= MAX_IMAGE_PAYLOAD_BYTES
                    && size
                        .checked_add(
                            crate::services::session_records::REPRESENTATION_OVERHEAD_BYTES,
                        )
                        .is_some_and(|bytes| bytes <= remaining)
            })
            .collect();
    image_candidates.sort_unstable_by_key(|(_, size, _)| *size);
    for (format, size, png) in image_candidates {
        if let Some(bytes) = copy_format_unchanged(reader, format, size)? {
            representations.push(if png {
                ClipboardRepresentation::Png { bytes }
            } else {
                ClipboardRepresentation::DibV5 { bytes }
            });
            break;
        }
    }
    debug_assert!(representation_bytes(&representations) <= budget);
    Ok(representations)
}

fn read_bounded_representations_with_files(
    reader: &impl ClipboardMemoryReader,
    files: &impl ClipboardFileReader,
    formats: RegisteredFormats,
    budget: usize,
) -> Result<Vec<ClipboardRepresentation>, ClipboardReadError> {
    let mut representations = Vec::with_capacity(5);

    if let Some(size) = probe_format_size(reader, CF_UNICODETEXT_FORMAT)
        .filter(|size| text_source_fits_budget(*size, budget))
        && let Some(bytes) = copy_format_unchanged(reader, CF_UNICODETEXT_FORMAT, size)?
        && let Ok(text) = decode_unicode_text(&bytes)
    {
        push_if_fits(
            &mut representations,
            ClipboardRepresentation::UnicodeText { text },
            budget,
        );
    }

    for (format, rich) in [(formats.rtf, true), (formats.html, false)] {
        let remaining = budget.saturating_sub(representation_bytes(&representations));
        if let Some(size) = probe_format_size(reader, format)
            .filter(|size| binary_source_fits_budget(*size, remaining))
            && let Some(bytes) = copy_format_unchanged(reader, format, size)?
        {
            let representation = if rich {
                ClipboardRepresentation::Rtf { bytes }
            } else {
                ClipboardRepresentation::Html { bytes }
            };
            push_if_fits(&mut representations, representation, budget);
        }
    }

    let _ = read_file_list(files, &mut representations, budget);

    let remaining = budget.saturating_sub(representation_bytes(&representations));
    let mut image_candidates: Vec<_> = [(formats.png, true), (CF_DIBV5_FORMAT, false)]
        .into_iter()
        .filter_map(|(format, png)| {
            probe_format_size(reader, format).map(|size| (format, size, png))
        })
        .filter(|(_, size, _)| {
            *size <= MAX_IMAGE_PAYLOAD_BYTES && binary_source_fits_budget(*size, remaining)
        })
        .collect();
    image_candidates.sort_unstable_by_key(|(_, size, _)| *size);
    for (format, size, png) in image_candidates {
        if let Some(bytes) = copy_format_unchanged(reader, format, size)? {
            representations.push(if png {
                ClipboardRepresentation::Png { bytes }
            } else {
                ClipboardRepresentation::DibV5 { bytes }
            });
            break;
        }
    }
    debug_assert!(representation_bytes(&representations) <= budget);
    Ok(representations)
}

fn probe_format_size(reader: &impl ClipboardMemoryReader, format: u32) -> Option<usize> {
    reader.size(format).ok().flatten()
}

fn copy_format_unchanged(
    reader: &impl ClipboardMemoryReader,
    format: u32,
    expected_size: usize,
) -> Result<Option<Vec<u8>>, ClipboardReadError> {
    match copy_unchanged(reader, format, expected_size) {
        Ok(bytes) => Ok(bytes),
        Err(error) if error.is_cleanup_failure() => Err(error),
        Err(_) => Ok(None),
    }
}

fn binary_source_fits_budget(source_bytes: usize, budget: usize) -> bool {
    source_bytes <= MAX_CAPTURE_PAYLOAD_BYTES
        && source_bytes
            .checked_add(crate::services::session_records::REPRESENTATION_OVERHEAD_BYTES)
            .is_some_and(|bytes| bytes <= budget)
}

fn push_if_fits(
    representations: &mut Vec<ClipboardRepresentation>,
    representation: ClipboardRepresentation,
    budget: usize,
) {
    let used = representation_bytes(representations);
    let additional = representation_bytes(std::slice::from_ref(&representation));
    if used
        .checked_add(additional)
        .is_some_and(|total| total <= budget)
    {
        representations.push(representation);
    }
}

fn read_file_list(
    reader: &impl ClipboardFileReader,
    representations: &mut Vec<ClipboardRepresentation>,
    budget: usize,
) -> Result<(), ClipboardReadError> {
    let Some(count) = reader.count()? else {
        return Ok(());
    };
    if count == 0 || count > MAX_CAPTURE_FILE_COUNT {
        return Ok(());
    }
    let remaining = budget.saturating_sub(representation_bytes(representations));
    let payload_budget =
        remaining.saturating_sub(crate::services::session_records::REPRESENTATION_OVERHEAD_BYTES);
    let mut paths = Vec::new();
    let mut utf8_bytes = 0_usize;
    let mut path_overhead_bytes = 0_usize;
    for index in 0..count {
        let length = reader.path_len(index)?;
        if length == 0 {
            return Err(ClipboardReadError::InvalidFileList);
        }
        path_overhead_bytes = path_overhead_bytes
            .checked_add(FILE_PATH_RESOURCE_OVERHEAD_BYTES)
            .ok_or(ClipboardReadError::InvalidFileList)?;
        let units = length
            .checked_add(1)
            .ok_or(ClipboardReadError::InvalidFileList)?;
        let allocation_bytes = units
            .checked_mul(size_of::<u16>())
            .ok_or(ClipboardReadError::InvalidFileList)?;
        if allocation_bytes > MAX_CAPTURE_PAYLOAD_BYTES
            || utf8_bytes
                .checked_add(path_overhead_bytes)
                .and_then(|bytes| bytes.checked_add(length))
                .is_none_or(|bytes| bytes > payload_budget)
        {
            return Ok(());
        }
        let mut buffer = vec![0_u16; units];
        let copied = reader.path(index, &mut buffer)?;
        if copied != length || buffer[..copied].contains(&0) {
            return Err(ClipboardReadError::InvalidFileList);
        }
        let path = String::from_utf16(&buffer[..copied])
            .map_err(|_| ClipboardReadError::InvalidFileList)?;
        if path.is_empty() {
            return Err(ClipboardReadError::InvalidFileList);
        }
        utf8_bytes = utf8_bytes
            .checked_add(path.len())
            .ok_or(ClipboardReadError::InvalidFileList)?;
        if utf8_bytes
            .checked_add(path_overhead_bytes)
            .is_none_or(|bytes| bytes > payload_budget)
        {
            return Ok(());
        }
        paths.push(path);
    }
    representations.push(ClipboardRepresentation::FileList { paths });
    Ok(())
}

fn copy_unchanged(
    reader: &impl ClipboardMemoryReader,
    format: u32,
    expected_size: usize,
) -> Result<Option<Vec<u8>>, ClipboardReadError> {
    let mut locked = reader.lock(format)?;
    let result = match reader.locked_size(&locked) {
        Ok(actual_size) if actual_size == expected_size => {
            reader.copy(&locked, actual_size).map(Some)
        }
        Ok(_) | Err(_) => Ok(None),
    };
    reader.unlock(&mut locked)?;
    result
}

fn text_source_fits_budget(source_bytes: usize, budget: usize) -> bool {
    if source_bytes < size_of::<u16>()
        || !source_bytes.is_multiple_of(size_of::<u16>())
        || source_bytes > MAX_TEXT_PAYLOAD_BYTES
    {
        return false;
    }
    let content_units = source_bytes / size_of::<u16>() - 1;
    content_units
        .checked_mul(3)
        .and_then(|bytes| {
            bytes.checked_add(crate::services::session_records::REPRESENTATION_OVERHEAD_BYTES)
        })
        .is_some_and(|bytes| bytes <= budget)
}

struct GlobalMemoryLock<A: GlobalMemoryApi = Win32GlobalMemoryApi> {
    memory: A::Handle,
    pointer: *mut std::ffi::c_void,
    api: A,
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
            Ok(Self {
                memory,
                pointer,
                api: Win32GlobalMemoryApi,
            })
        }
    }
}

impl<A: GlobalMemoryApi> GlobalMemoryLock<A> {
    fn unlock(&mut self) -> Result<(), ClipboardReadError> {
        let result = self.api.unlock(self.memory);
        if result.is_ok() {
            self.pointer = std::ptr::null_mut();
        }
        result.map_err(|error| {
            ClipboardReadError::windows(ClipboardReadOperation::GlobalUnlock, error)
        })
    }

    #[cfg(test)]
    fn new_for_test(memory: A::Handle, pointer: *mut std::ffi::c_void, api: A) -> Self {
        Self {
            memory,
            pointer,
            api,
        }
    }

    #[cfg(test)]
    fn is_locked(&self) -> bool {
        !self.pointer.is_null()
    }
}

impl<A: GlobalMemoryApi> Drop for GlobalMemoryLock<A> {
    fn drop(&mut self) {
        if !self.pointer.is_null() && self.api.unlock(self.memory).is_ok() {
            self.pointer = std::ptr::null_mut();
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

#[cfg(test)]
fn classify_registered_format(format: u32, last_error: u32) -> Result<u32, ClipboardReadError> {
    if format == 0 {
        let code = if last_error == 0 { 1 } else { last_error };
        Err(ClipboardReadError::windows(
            ClipboardReadOperation::RegisterFormats,
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

#[derive(Clone)]
pub struct EventSender(EventSenderKind);

#[derive(Clone)]
enum EventSenderKind {
    Queue(Sender<ClipboardEvent>),
    Latest(Arc<ClipboardIngestionShared>),
}

struct ClipboardIngestionShared {
    records: Arc<SessionRecordStore>,
    policy: Arc<CapturePolicy>,
    queue: Mutex<ClipboardIngestionQueue>,
    queue_changed: Condvar,
    revision: AtomicU64,
    notification: Mutex<Option<u64>>,
    notification_changed: Condvar,
    total_bytes_limit: usize,
    record_count_limit: usize,
}

#[derive(Default)]
pub struct CapturePolicy {
    paused: AtomicBool,
    excluded_applications: RwLock<Vec<String>>,
}

impl CapturePolicy {
    pub fn new(excluded_applications: Vec<String>) -> Self {
        Self {
            paused: AtomicBool::new(false),
            excluded_applications: RwLock::new(excluded_applications),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    pub fn excluded_applications(&self) -> Vec<String> {
        self.excluded_applications
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_excluded_applications(&self, applications: Vec<String>) {
        *self
            .excluded_applications
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = applications;
    }

    fn allows(&self, capture: &CapturedClipboard) -> bool {
        if self.is_paused() {
            return false;
        }
        let excluded = self
            .excluded_applications
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !excluded.iter().any(|application| {
            capture
                .source
                .application_name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(application))
                || capture
                    .source
                    .executable_path
                    .as_deref()
                    .and_then(|path| Path::new(path).file_name())
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case(application))
        })
    }
}

struct ClipboardIngestionQueue {
    captures: VecDeque<QueuedCapture>,
    reserved_bytes: usize,
    reserved_count: usize,
    processing: bool,
    stopped: bool,
    paused: bool,
    #[cfg(test)]
    pause_before_store_commit: bool,
    #[cfg(test)]
    processing_started: bool,
    #[cfg(test)]
    processed_generation: u64,
}

struct QueuedCapture {
    capture: CapturedClipboard,
    bytes: usize,
}

pub struct LatestClipboardEventReceiver {
    shared: Arc<ClipboardIngestionShared>,
    worker: Option<JoinHandle<()>>,
    worker_done: std::sync::mpsc::Receiver<()>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClipboardEventSendError;

impl EventSender {
    pub fn send(&self, event: ClipboardEvent) -> Result<(), ClipboardEventSendError> {
        self.send_batch(vec![event])
    }

    fn send_batch(&self, events: Vec<ClipboardEvent>) -> Result<(), ClipboardEventSendError> {
        if events.is_empty() {
            return Ok(());
        }
        match &self.0 {
            EventSenderKind::Queue(sender) => events
                .into_iter()
                .try_for_each(|event| sender.send(event).map_err(|_| ClipboardEventSendError)),
            EventSenderKind::Latest(shared) => enqueue_captures(shared, events),
        }
    }
}

trait ClipboardEventSink {
    fn send_event(&self, event: ClipboardEvent) -> Result<(), ClipboardEventSendError>;

    fn send_batch(&self, events: Vec<ClipboardEvent>) -> Result<(), ClipboardEventSendError>;
}

impl ClipboardEventSink for EventSender {
    fn send_event(&self, event: ClipboardEvent) -> Result<(), ClipboardEventSendError> {
        self.send(event)
    }

    fn send_batch(&self, events: Vec<ClipboardEvent>) -> Result<(), ClipboardEventSendError> {
        self.send_batch(events)
    }
}

impl ClipboardEventSink for Sender<ClipboardEvent> {
    fn send_event(&self, event: ClipboardEvent) -> Result<(), ClipboardEventSendError> {
        self.send(event).map_err(|_| ClipboardEventSendError)
    }

    fn send_batch(&self, events: Vec<ClipboardEvent>) -> Result<(), ClipboardEventSendError> {
        events
            .into_iter()
            .try_for_each(|event| self.send(event).map_err(|_| ClipboardEventSendError))
    }
}

impl From<Sender<ClipboardEvent>> for EventSender {
    fn from(sender: Sender<ClipboardEvent>) -> Self {
        Self(EventSenderKind::Queue(sender))
    }
}

pub fn latest_clipboard_event_channel(
    records: Arc<SessionRecordStore>,
) -> (EventSender, LatestClipboardEventReceiver) {
    clipboard_event_channel_with_limits(records, INGESTION_QUEUE_BYTES, INGESTION_QUEUE_EVENTS)
}

pub fn latest_clipboard_event_channel_with_policy(
    records: Arc<SessionRecordStore>,
    policy: Arc<CapturePolicy>,
) -> (EventSender, LatestClipboardEventReceiver) {
    clipboard_event_channel_with_policy_and_limits(
        records,
        policy,
        INGESTION_QUEUE_BYTES,
        INGESTION_QUEUE_EVENTS,
    )
}

fn clipboard_event_channel_with_limits(
    records: Arc<SessionRecordStore>,
    total_bytes_limit: usize,
    record_count_limit: usize,
) -> (EventSender, LatestClipboardEventReceiver) {
    clipboard_event_channel_with_policy_and_limits(
        records,
        Arc::new(CapturePolicy::default()),
        total_bytes_limit,
        record_count_limit,
    )
}

fn clipboard_event_channel_with_policy_and_limits(
    records: Arc<SessionRecordStore>,
    policy: Arc<CapturePolicy>,
    total_bytes_limit: usize,
    record_count_limit: usize,
) -> (EventSender, LatestClipboardEventReceiver) {
    let shared = Arc::new(ClipboardIngestionShared {
        records,
        policy,
        queue: Mutex::new(ClipboardIngestionQueue {
            captures: VecDeque::new(),
            reserved_bytes: 0,
            reserved_count: 0,
            processing: false,
            stopped: false,
            paused: false,
            #[cfg(test)]
            pause_before_store_commit: false,
            #[cfg(test)]
            processing_started: false,
            #[cfg(test)]
            processed_generation: 0,
        }),
        queue_changed: Condvar::new(),
        revision: AtomicU64::new(0),
        notification: Mutex::new(None),
        notification_changed: Condvar::new(),
        total_bytes_limit,
        record_count_limit,
    });
    let (done_sender, worker_done) = sync_channel(1);
    let worker_shared = Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("clipboard-ingestion".to_owned())
        .spawn(move || {
            run_clipboard_ingestion(worker_shared);
            let _ = done_sender.send(());
        })
        .expect("clipboard ingestion worker must start");
    (
        EventSender(EventSenderKind::Latest(Arc::clone(&shared))),
        LatestClipboardEventReceiver {
            shared,
            worker: Some(worker),
            worker_done,
        },
    )
}

fn enqueue_captures(
    shared: &ClipboardIngestionShared,
    events: Vec<ClipboardEvent>,
) -> Result<(), ClipboardEventSendError> {
    let mut queued = Vec::with_capacity(events.len());
    for event in events {
        let bytes = checked_representation_bytes(&event.captured.representations)
            .ok_or(ClipboardEventSendError)?;
        if bytes > MAX_CAPTURE_RECORD_BYTES {
            return Err(ClipboardEventSendError);
        }
        queued.push(QueuedCapture {
            capture: event.captured,
            bytes,
        });
    }
    let mut state = lock_unpoisoned(&shared.queue);
    if state.stopped {
        return Err(ClipboardEventSendError);
    }
    let queued_bytes = state
        .captures
        .iter()
        .try_fold(0_usize, |total, capture| total.checked_add(capture.bytes))
        .ok_or(ClipboardEventSendError)?;
    let processing_bytes = state.reserved_bytes.saturating_sub(queued_bytes);
    let processing_count = usize::from(state.processing);
    let available_bytes = shared.total_bytes_limit.saturating_sub(processing_bytes);
    let available_count = shared.record_count_limit.saturating_sub(processing_count);
    if available_count == 0 || queued.iter().any(|capture| capture.bytes > available_bytes) {
        return Err(ClipboardEventSendError);
    }
    for capture in queued {
        while state
            .reserved_bytes
            .checked_add(capture.bytes)
            .is_none_or(|bytes| bytes > shared.total_bytes_limit)
            || state.reserved_count + 1 > shared.record_count_limit
        {
            if let Some(removed) = state.captures.pop_front() {
                state.reserved_bytes = state.reserved_bytes.saturating_sub(removed.bytes);
                state.reserved_count = state.reserved_count.saturating_sub(1);
            } else {
                return Err(ClipboardEventSendError);
            }
        }
        state.reserved_bytes = state
            .reserved_bytes
            .checked_add(capture.bytes)
            .ok_or(ClipboardEventSendError)?;
        state.reserved_count = state
            .reserved_count
            .checked_add(1)
            .ok_or(ClipboardEventSendError)?;
        state.captures.push_back(capture);
    }
    drop(state);
    shared.queue_changed.notify_one();
    Ok(())
}

fn run_clipboard_ingestion(shared: Arc<ClipboardIngestionShared>) {
    loop {
        let queued = {
            let state = lock_unpoisoned(&shared.queue);
            let mut state = shared
                .queue_changed
                .wait_while(state, |state| {
                    !state.stopped && (state.paused || state.captures.is_empty())
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.stopped {
                return;
            }
            let queued = state.captures.pop_front();
            if queued.is_some() {
                state.processing = true;
                #[cfg(test)]
                {
                    state.processing_started = true;
                    shared.queue_changed.notify_all();
                }
            }
            queued
        };
        let Some(queued) = queued else {
            continue;
        };

        #[cfg(test)]
        {
            let state = lock_unpoisoned(&shared.queue);
            let state = shared
                .queue_changed
                .wait_while(state, |state| {
                    !state.stopped && state.pause_before_store_commit
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.stopped {
                return;
            }
        }

        let status = shared
            .policy
            .allows(&queued.capture)
            .then(|| shared.records.capture_one(queued.capture));
        {
            let mut state = lock_unpoisoned(&shared.queue);
            state.reserved_bytes = state.reserved_bytes.saturating_sub(queued.bytes);
            state.reserved_count = state.reserved_count.saturating_sub(1);
            state.processing = false;
        }
        if status.is_some_and(|status| !matches!(status, CaptureStatus::RejectedTooLarge)) {
            let revision = shared.revision.fetch_add(1, Ordering::AcqRel) + 1;
            *lock_unpoisoned(&shared.notification) = Some(revision);
            shared.notification_changed.notify_one();
        }
        #[cfg(test)]
        {
            let mut state = lock_unpoisoned(&shared.queue);
            state.processing_started = false;
            state.processed_generation = state.processed_generation.saturating_add(1);
            shared.queue_changed.notify_all();
        }
    }
}

impl LatestClipboardEventReceiver {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<u64, RecvTimeoutError> {
        let notification = lock_unpoisoned(&self.shared.notification);
        let (mut notification, wait) = self
            .shared
            .notification_changed
            .wait_timeout_while(notification, timeout, |revision| revision.is_none())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        notification.take().ok_or(if wait.timed_out() {
            RecvTimeoutError::Timeout
        } else {
            RecvTimeoutError::Disconnected
        })
    }

    pub fn try_recv(&self) -> Result<u64, std::sync::mpsc::TryRecvError> {
        lock_unpoisoned(&self.shared.notification)
            .take()
            .ok_or(std::sync::mpsc::TryRecvError::Empty)
    }
}

impl Drop for LatestClipboardEventReceiver {
    fn drop(&mut self) {
        {
            let mut state = lock_unpoisoned(&self.shared.queue);
            state.stopped = true;
            state.paused = false;
            #[cfg(test)]
            {
                state.pause_before_store_commit = false;
            }
        }
        self.shared.queue_changed.notify_all();
        if self
            .worker_done
            .recv_timeout(Duration::from_secs(1))
            .is_ok()
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
impl LatestClipboardEventReceiver {
    fn pause_ingestion(&self) {
        lock_unpoisoned(&self.shared.queue).paused = true;
    }

    fn resume_ingestion(&self) {
        lock_unpoisoned(&self.shared.queue).paused = false;
        self.shared.queue_changed.notify_all();
    }

    fn pause_before_store_commit(&self) {
        lock_unpoisoned(&self.shared.queue).pause_before_store_commit = true;
    }

    fn wait_until_processing(&self, timeout: Duration) {
        let state = lock_unpoisoned(&self.shared.queue);
        let (state, wait) = self
            .shared
            .queue_changed
            .wait_timeout_while(state, timeout, |state| {
                !state.stopped && !state.processing_started
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            state.processing_started,
            "clipboard ingestion did not start processing before deadline (timed out: {})",
            wait.timed_out()
        );
    }

    fn resume_store_commit(&self) {
        lock_unpoisoned(&self.shared.queue).pause_before_store_commit = false;
        self.shared.queue_changed.notify_all();
    }

    fn processed_generation(&self) -> u64 {
        lock_unpoisoned(&self.shared.queue).processed_generation
    }

    fn wait_for_processed_generation(&self, expected: u64, timeout: Duration) {
        let state = lock_unpoisoned(&self.shared.queue);
        let (state, wait) = self
            .shared
            .queue_changed
            .wait_timeout_while(state, timeout, |state| {
                !state.stopped && state.processed_generation < expected
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            state.processed_generation >= expected,
            "clipboard ingestion did not reach processed generation {expected} before deadline; current generation is {} (timed out: {})",
            state.processed_generation,
            wait.timed_out()
        );
    }

    fn wait_until_ingestion_idle(&self, timeout: Duration) {
        let state = lock_unpoisoned(&self.shared.queue);
        let (state, wait) = self
            .shared
            .queue_changed
            .wait_timeout_while(state, timeout, |state| {
                !state.stopped
                    && (state.processing
                        || state.processing_started
                        || state.reserved_count != 0
                        || !state.captures.is_empty())
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !state.processing
                && !state.processing_started
                && state.reserved_count == 0
                && state.captures.is_empty(),
            "clipboard ingestion did not become idle before deadline (timed out: {})",
            wait.timed_out()
        );
    }
}
pub(super) type ProductWriteOwnership = Arc<Mutex<ProductWriteState>>;
pub(super) type ReadySender = std::sync::mpsc::SyncSender<Result<(), ClipboardListenerError>>;
pub(super) type ShutdownReceiver = std::sync::mpsc::Receiver<()>;

fn clipboard_event_bytes(event: &ClipboardEvent) -> Option<usize> {
    checked_representation_bytes(&event.captured.representations)?
        .checked_add(CLIPBOARD_EVENT_OVERHEAD_BYTES)
        .and_then(|bytes| bytes.checked_add(size_of::<ClipboardEvent>()))
        .and_then(|bytes| bytes.checked_add(size_of::<usize>() * 2))
}

fn checked_pending_bytes(current: usize, event: usize) -> Option<usize> {
    current.checked_add(event)
}

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        collections::HashMap,
        mem::size_of,
        ptr,
        sync::{Arc, Barrier, Mutex, mpsc},
        time::Duration,
    };

    use chrono::Utc;

    use super::{
        CF_HDROP_FORMAT, CF_UNICODETEXT_FORMAT, CapturePolicy, ClipboardEvent, ClipboardFileReader,
        ClipboardGlobalAllocator, ClipboardListenerError, ClipboardMemoryReader,
        ClipboardPendingMemory, ClipboardReadError, ClipboardTransferApi, ClipboardWriteError,
        EventSender, LatestClipboardEventReceiver, MAX_CAPTURE_PAYLOAD_BYTES,
        MAX_IMAGE_PAYLOAD_BYTES, ProductWriteGuard, ProductWriteState, ReadyWait,
        RegisteredFormats, ShutdownFailure, begin_product_write_transaction,
        begin_product_write_transaction_with_spawner, capture_source, classify_format_read,
        classify_global_unlock, classify_registered_format, clipboard_event_channel_with_limits,
        combine_clipboard_operation_and_close, decode_unicode_text, finish_product_write,
        latest_clipboard_event_channel, latest_clipboard_event_channel_with_policy,
        orchestrate_listener_initialization, prepare_pending_payloads, prepare_representations,
        prepare_representations_then_open, prepare_then_open, read_bounded_representations,
        read_bounded_representations_with_files, representation_bytes, route_event,
        serialize_dropfiles, transfer_pending_payloads, validate_representations,
    };
    use crate::domain::{
        CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity,
    };
    use crate::services::session_records::SessionRecordStore;
    use windows::Win32::Foundation::HWND;

    struct FakeSourceApi {
        window: Option<HWND>,
        process_id: Option<u32>,
        path: Option<String>,
    }

    impl super::ForegroundSourceApi for FakeSourceApi {
        fn foreground_window(&self) -> Option<HWND> {
            self.window
        }

        fn process_id(&self, _window: HWND) -> Option<u32> {
            self.process_id
        }

        fn executable_path(&self, _process_id: u32) -> Option<String> {
            self.path.clone()
        }
    }

    #[test]
    fn foreground_source_keeps_path_and_derives_displayable_application_name() {
        let source = capture_source(&FakeSourceApi {
            window: Some(HWND(std::ptr::dangling_mut())),
            process_id: Some(42),
            path: Some(r"C:\Program Files\Editor\editor.exe".to_owned()),
        });

        assert_eq!(source.application_name.as_deref(), Some("editor"));
        assert_eq!(
            source.executable_path.as_deref(),
            Some(r"C:\Program Files\Editor\editor.exe")
        );
    }

    #[test]
    fn foreground_source_failure_is_an_empty_non_sensitive_fallback() {
        let source = capture_source(&FakeSourceApi {
            window: None,
            process_id: None,
            path: None,
        });

        assert_eq!(source, SourceIdentity::default());
        assert_eq!(
            format!("{source:?}"),
            "SourceIdentity { has_application_name: false, has_executable_path: false }"
        );
    }

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
    fn repeated_notifications_after_finish_are_suppressed_until_sequence_changes() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(50)));

        product_write_guard(Arc::clone(&state), events.clone()).finish(&[51]);

        route_event(&state, &events, event(51));
        route_event(&state, &events, event(51));
        assert!(receiver.try_recv().is_err());
        route_event(&state, &events, event(52));
        assert_eq!(receiver.recv().unwrap().sequence_number, 52);
    }

    #[test]
    fn next_product_write_accepts_and_suppresses_late_prior_notification() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Expected { sequence: 51 }));

        let guard = begin_product_write_transaction(
            Arc::clone(&state),
            events.clone(),
            51,
            Duration::from_secs(1),
        )
        .unwrap();
        route_event(&state, &events, event(51));
        route_event(&state, &events, event(52));
        guard.finish(&[52]);

        assert!(receiver.try_recv().is_err());
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
    fn pending_byte_budget_cancels_before_flushing_one_atomic_external_batch() {
        let records = Arc::new(SessionRecordStore::default());
        let (events, receiver) = latest_clipboard_event_channel(Arc::clone(&records));
        let one = super::clipboard_event_bytes(&text_event(81, "1234")).unwrap();
        let state = Arc::new(Mutex::new(ProductWriteState::armed_with_budget(
            80,
            one * 2,
        )));

        route_event(&state, &events, text_event(81, "1234"));
        route_event(&state, &events, text_event(82, "5678"));
        route_event(&state, &events, text_event(83, "90"));

        assert!(matches!(*state.lock().unwrap(), ProductWriteState::Idle));
        wait_for_record_count(&receiver, &records, 3);
        assert_eq!(stored_texts(&records), ["90", "5678", "1234"]);
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn production_mailbox_keeps_external_order_and_suppresses_owned_sequences_on_overflow() {
        let records = Arc::new(SessionRecordStore::default());
        let (events, receiver) = latest_clipboard_event_channel(Arc::clone(&records));
        let one = super::clipboard_event_bytes(&text_event(91, "one")).unwrap();
        let state = Arc::new(Mutex::new(ProductWriteState::armed_with_budget(
            90,
            one * 3,
        )));
        route_event(&state, &events, text_event(91, "one"));
        {
            let mut guard = product_write_guard(Arc::clone(&state), events.clone());
            guard.note_owned_sequences(&[92]);
            route_event(&state, &events, text_event(92, "owned"));
            route_event(&state, &events, text_event(93, "three"));
            route_event(&state, &events, text_event(94, "four"));
            assert!(matches!(*state.lock().unwrap(), ProductWriteState::Idle));
            guard.finish(&[92]);
        }

        wait_for_record_count(&receiver, &records, 3);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
        let stored = stored_texts(&records);
        assert_eq!(stored, ["four", "three", "one"]);
        assert!(!stored.iter().any(|text| text == "owned"));
    }

    #[test]
    fn paused_notification_drain_does_not_lose_external_history() {
        let records = Arc::new(SessionRecordStore::default());
        let (events, receiver) = latest_clipboard_event_channel(Arc::clone(&records));
        receiver.pause_ingestion();
        let started = std::time::Instant::now();
        events
            .send_batch(vec![text_event(101, "transaction")])
            .unwrap();
        events.send(text_event(102, "old latest")).unwrap();
        events.send(text_event(103, "new latest")).unwrap();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(records.list().is_empty());
        receiver.resume_ingestion();
        wait_for_record_count(&receiver, &records, 3);

        assert_eq!(
            stored_texts(&records),
            ["new latest", "old latest", "transaction"]
        );
        assert_eq!(receiver.try_recv().unwrap(), 3);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn ingestion_budget_evicts_the_globally_oldest_history() {
        let bytes = representation_bytes(&text_event(1, "12345678").captured.representations);
        let records = Arc::new(SessionRecordStore::with_test_limits(bytes * 3, bytes, 16));
        let (events, receiver) =
            clipboard_event_channel_with_limits(Arc::clone(&records), bytes * 3, 3);
        events.send(text_event(1, "oldest-a")).unwrap();
        events.send(text_event(2, "middle-b")).unwrap();
        wait_for_record_count(&receiver, &records, 2);
        receiver.pause_ingestion();
        events.send(text_event(3, "queued-c")).unwrap();
        events.send(text_event(4, "newest-d")).unwrap();
        receiver.resume_ingestion();
        wait_for_record_count(&receiver, &records, 3);

        assert_eq!(stored_texts(&records), ["newest-d", "queued-c", "middle-b"]);
    }

    #[test]
    fn paused_worker_keeps_queue_within_its_own_byte_and_count_budget() {
        let records = Arc::new(SessionRecordStore::default());
        let bytes = representation_bytes(&text_event(1, "12345678").captured.representations);
        let (events, receiver) =
            clipboard_event_channel_with_limits(Arc::clone(&records), bytes * 2, 2);
        receiver.pause_ingestion();

        events.send(text_event(1, "oldest-a")).unwrap();
        events.send(text_event(2, "middle-b")).unwrap();
        events.send(text_event(3, "newest-c")).unwrap();

        let queue = receiver.shared.queue.lock().unwrap();
        assert_eq!(queue.captures.len(), 2);
        assert!(queue.reserved_bytes <= bytes * 2);
        assert!(queue.reserved_count <= 2);
        drop(queue);
        receiver.resume_ingestion();
        wait_for_record_count(&receiver, &records, 2);
        assert_eq!(stored_texts(&records), ["newest-c", "middle-b"]);
    }

    #[test]
    fn processing_capture_keeps_its_queue_reservation_until_store_commit() {
        let records = Arc::new(SessionRecordStore::default());
        let bytes = representation_bytes(&text_event(1, "processing").captured.representations);
        let (events, receiver) =
            clipboard_event_channel_with_limits(Arc::clone(&records), bytes, 1);
        receiver.pause_before_store_commit();
        events.send(text_event(1, "processing")).unwrap();
        receiver.wait_until_processing(Duration::from_secs(1));

        let queue = receiver.shared.queue.lock().unwrap();
        assert!(queue.processing);
        assert_eq!(queue.reserved_count, 1);
        assert_eq!(queue.reserved_bytes, bytes);
        drop(queue);
        receiver.resume_store_commit();
        wait_for_record_count(&receiver, &records, 1);
    }

    #[test]
    fn note_eviction_remains_authoritative_through_production_ingestion() {
        let records = Arc::new(SessionRecordStore::with_test_limits(77, 48, 16));
        let (events, receiver) = clipboard_event_channel_with_limits(Arc::clone(&records), 128, 4);
        events.send(text_event(1, "12345")).unwrap();
        events.send(text_event(2, "abcde")).unwrap();
        wait_for_record_count(&receiver, &records, 2);
        let newest = records.list()[0].id;

        records.update_note(newest, "note".to_owned()).unwrap();
        assert_eq!(records.list().len(), 1);
        events.send(text_event(3, "xyz")).unwrap();
        wait_for_record_count(&receiver, &records, 2);

        let listed = records.list();
        assert_eq!(listed[1].id, newest);
        assert_eq!(receiver.recv_timeout(Duration::from_millis(50)).unwrap(), 3);
    }

    #[test]
    fn duplicate_refresh_at_full_count_does_not_pre_evict_store_history() {
        let records = Arc::new(SessionRecordStore::default());
        for index in 0..500 {
            records.capture(text_event(index, &format!("value-{index}")).captured);
        }
        let before = records.list();
        let oldest = before.last().unwrap().id;
        let (events, receiver) = latest_clipboard_event_channel(Arc::clone(&records));

        let mut duplicate = text_event(501, "value-499");
        duplicate.captured.content_identity = ContentIdentity::new("test:499");
        events.send(duplicate).unwrap();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), 1);

        let after = records.list();
        assert_eq!(after.len(), 500);
        assert_eq!(after.last().unwrap().id, oldest);
    }

    #[test]
    fn rejected_refresh_through_production_ingestion_keeps_store_unchanged() {
        let records = Arc::new(SessionRecordStore::with_test_limits(128, 80, 16));
        records.capture(text_event(1, "old").captured);
        records.capture(text_event(2, &"a".repeat(40)).captured);
        let before = records.list();
        let (events, receiver) = latest_clipboard_event_channel(Arc::clone(&records));
        let mut duplicate = text_event(3, "");
        duplicate.captured.content_identity = ContentIdentity::new("test:2");
        duplicate.captured.representations =
            vec![ClipboardRepresentation::Png { bytes: vec![1; 40] }];

        let processed = receiver.processed_generation();
        events.send(duplicate).unwrap();
        receiver.wait_for_processed_generation(processed + 1, Duration::from_secs(1));

        let queue = receiver.shared.queue.lock().unwrap();
        assert!(!queue.processing);
        assert_eq!(queue.reserved_count, 0);
        assert_eq!(queue.reserved_bytes, 0);
        drop(queue);
        assert_eq!(records.list(), before);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn failed_enqueue_does_not_evict_existing_queued_history() {
        let records = Arc::new(SessionRecordStore::default());
        let bytes = representation_bytes(&text_event(1, "kept").captured.representations);
        let (events, receiver) =
            clipboard_event_channel_with_limits(Arc::clone(&records), bytes, 1);
        receiver.pause_ingestion();
        events.send(text_event(1, "kept")).unwrap();

        assert!(events.send(text_event(2, &"x".repeat(bytes))).is_err());
        let queue = receiver.shared.queue.lock().unwrap();
        assert_eq!(queue.captures.len(), 1);
        assert_eq!(
            queue.captures[0].capture.content_identity,
            ContentIdentity::new("test:1")
        );
        drop(queue);

        receiver.resume_ingestion();
        wait_for_record_count(&receiver, &records, 1);
        assert_eq!(stored_texts(&records), ["kept"]);
    }

    #[test]
    fn store_and_processing_queue_reservations_stay_within_partitioned_total_budget() {
        let records = Arc::new(SessionRecordStore::with_test_limits(80, 48, 16));
        records.capture(text_event(1, "12345678").captured);
        records.capture(text_event(2, "abcdefgh").captured);
        let queue_bytes = representation_bytes(&text_event(3, "queue").captured.representations);
        let (events, receiver) =
            clipboard_event_channel_with_limits(Arc::clone(&records), queue_bytes, 1);
        receiver.pause_before_store_commit();
        events.send(text_event(3, "queue")).unwrap();
        receiver.wait_until_processing(Duration::from_secs(1));

        let (store_bytes, _) = records.budget_snapshot();
        let queue = receiver.shared.queue.lock().unwrap();
        assert!(store_bytes <= 80);
        assert!(queue.reserved_bytes <= queue_bytes);
        assert!(store_bytes + queue.reserved_bytes <= 80 + queue_bytes);
        drop(queue);

        receiver.resume_store_commit();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), 1);
    }

    #[test]
    fn ingestion_worker_shutdown_joins_and_releases_shared_state() {
        let records = Arc::new(SessionRecordStore::default());
        let (events, receiver) = latest_clipboard_event_channel(records);
        let shared = Arc::downgrade(&receiver.shared);
        events.send(text_event(1, "queued")).unwrap();

        drop(events);
        let started = std::time::Instant::now();
        drop(receiver);

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(shared.upgrade().is_none());
    }

    #[test]
    fn empty_pending_events_hit_the_count_limit_and_replay_in_order() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed_with_limits(
            200,
            usize::MAX,
            2,
        )));

        route_event(&state, &events, text_event(201, ""));
        route_event(&state, &events, text_event(202, ""));
        route_event(&state, &events, text_event(203, ""));

        assert!(matches!(*state.lock().unwrap(), ProductWriteState::Idle));
        assert_eq!(
            receiver
                .try_iter()
                .map(|event| event.sequence_number)
                .collect::<Vec<_>>(),
            [201, 202, 203]
        );
    }

    #[test]
    fn pending_byte_accounting_rejects_checked_overflow() {
        assert_eq!(super::checked_pending_bytes(usize::MAX, 1), None);
        assert_eq!(super::checked_pending_bytes(7, 11), Some(18));
    }

    #[test]
    fn latest_event_mailbox_is_bounded_and_preserves_the_newest_capture() {
        let records = Arc::new(SessionRecordStore::default());
        let (events, receiver) = latest_clipboard_event_channel(Arc::clone(&records));

        events.send(text_event(91, &"a".repeat(1024))).unwrap();
        events.send(text_event(92, &"b".repeat(1024))).unwrap();
        events.send(text_event(93, &"c".repeat(1024))).unwrap();

        wait_for_record_count(&receiver, &records, 3);
        assert_eq!(receiver.recv_timeout(Duration::from_millis(50)).unwrap(), 3);
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            stored_texts(&records),
            ["c".repeat(1024), "b".repeat(1024), "a".repeat(1024)]
        );
    }

    #[test]
    fn cancelling_and_routing_share_one_global_ordering_boundary() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::armed(120)));
        route_event(&state, &events, event(121));
        let guard = product_write_guard(Arc::clone(&state), events.clone());
        let locked = state.lock().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let route_thread = {
            let state = Arc::clone(&state);
            let events = events.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                route_event(&state, &events, event(122));
            })
        };
        barrier.wait();
        drop(locked);
        guard.cancel();
        route_thread.join().unwrap();

        assert_eq!(
            receiver
                .try_iter()
                .map(|event| event.sequence_number)
                .collect::<Vec<_>>(),
            [121, 122]
        );
    }

    #[test]
    fn external_commit_and_arming_share_one_coordinator_boundary() {
        struct BlockingSink {
            entered: mpsc::SyncSender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            delivered: Mutex<Vec<u32>>,
        }

        impl super::ClipboardEventSink for BlockingSink {
            fn send_event(
                &self,
                event: ClipboardEvent,
            ) -> Result<(), super::ClipboardEventSendError> {
                self.send_batch(vec![event])
            }

            fn send_batch(
                &self,
                events: Vec<ClipboardEvent>,
            ) -> Result<(), super::ClipboardEventSendError> {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                self.delivered
                    .lock()
                    .unwrap()
                    .extend(events.into_iter().map(|event| event.sequence_number));
                Ok(())
            }
        }

        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::channel();
        let sink = Arc::new(BlockingSink {
            entered: entered_tx,
            release: Mutex::new(release_rx),
            delivered: Mutex::new(Vec::new()),
        });
        let state = Arc::new(Mutex::new(ProductWriteState::Idle));
        let route_thread = {
            let state = Arc::clone(&state);
            let sink = Arc::clone(&sink);
            std::thread::spawn(move || {
                route_event(&state, &*sink, text_event(101, "external"));
            })
        };
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let (events, _receiver) = mpsc::channel();
        let (begin_done_tx, begin_done_rx) = mpsc::sync_channel(1);
        let begin_thread = {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                let result =
                    begin_product_write_transaction(state, events, 101, Duration::from_secs(1));
                begin_done_tx.send(()).unwrap();
                result
            })
        };
        assert!(
            begin_done_rx
                .recv_timeout(Duration::from_millis(20))
                .is_err()
        );
        release_tx.send(()).unwrap();
        route_thread.join().unwrap();

        let guard = begin_thread.join().unwrap().unwrap();
        let (events, _receiver) = mpsc::channel();
        let second = begin_product_write_transaction(
            Arc::clone(&state),
            events,
            101,
            Duration::from_secs(1),
        );

        assert!(matches!(
            second,
            Err(ClipboardListenerError::ProductWriteAlreadyInProgress)
        ));
        assert_eq!(*sink.delivered.lock().unwrap(), [101]);
        guard.cancel();
    }

    #[test]
    fn deadline_replays_pending_without_any_later_clipboard_operation() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Idle));
        let guard = begin_product_write_transaction(
            Arc::clone(&state),
            events.clone(),
            120,
            Duration::from_millis(20),
        )
        .unwrap();
        route_event(&state, &events, event(121));

        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .sequence_number,
            121
        );
        assert!(matches!(*state.lock().unwrap(), ProductWriteState::Idle));
        drop(guard);
    }

    #[test]
    fn timeout_thread_spawn_failure_restores_idle_state() {
        let (events, _receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Idle));

        let result = begin_product_write_transaction_with_spawner(
            Arc::clone(&state),
            events,
            120,
            Duration::from_secs(1),
            |_, _, _, _, _| {
                Err(ClipboardListenerError::ThreadSpawn(std::io::Error::other(
                    "forced timeout thread spawn failure",
                )))
            },
        );

        assert!(matches!(
            result,
            Err(ClipboardListenerError::ThreadSpawn(_))
        ));
        assert!(matches!(*state.lock().unwrap(), ProductWriteState::Idle));
    }

    #[test]
    fn old_deadline_cannot_cancel_a_newer_product_write_transaction() {
        let (events, receiver) = mpsc::channel();
        let state = Arc::new(Mutex::new(ProductWriteState::Idle));
        let old_guard = begin_product_write_transaction(
            Arc::clone(&state),
            events.clone(),
            120,
            Duration::from_millis(20),
        )
        .unwrap();
        let old_transaction_id = old_guard.transaction_id;
        route_event(&state, &events, event(121));
        let replayed = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(replayed.sequence_number, 121);
        assert!(matches!(*state.lock().unwrap(), ProductWriteState::Idle));

        let new_guard = begin_product_write_transaction(
            Arc::clone(&state),
            events.clone(),
            130,
            Duration::from_secs(1),
        )
        .unwrap();
        route_event(&state, &events, event(131));
        super::cancel_product_write(&state, &events, old_transaction_id);

        match &*state.lock().unwrap() {
            ProductWriteState::Armed {
                transaction_id,
                pending,
                ..
            } => {
                assert_eq!(*transaction_id, new_guard.transaction_id);
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

        drop(old_guard);
        new_guard.cancel();
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .sequence_number,
            131
        );
        assert!(receiver.try_recv().is_err());
    }

    fn product_write_guard(
        state: Arc<Mutex<ProductWriteState>>,
        events: impl Into<EventSender>,
    ) -> ProductWriteGuard {
        let transaction_id = state
            .lock()
            .unwrap()
            .transaction_id()
            .expect("product write must be armed");
        ProductWriteGuard {
            state,
            events: events.into(),
            transaction_id,
            cancel_timeout: None,
            timeout_thread: None,
            finished: false,
        }
    }

    #[derive(Default)]
    struct FakeClipboardMemory {
        payloads: HashMap<u32, Vec<u8>>,
        size_errors: Vec<u32>,
        lock_errors: Vec<u32>,
        locked_sizes: HashMap<u32, Result<usize, ()>>,
        copy_errors: Vec<u32>,
        unlock_errors: Vec<u32>,
        copies: RefCell<Vec<u32>>,
        operations: RefCell<Vec<(&'static str, u32)>>,
    }

    struct FakeLockedClipboardMemory {
        format: u32,
    }

    impl ClipboardMemoryReader for FakeClipboardMemory {
        type Locked = FakeLockedClipboardMemory;

        fn size(&self, format: u32) -> Result<Option<usize>, ClipboardReadError> {
            if self.size_errors.contains(&format) {
                return Err(ClipboardReadError::windows(
                    super::ClipboardReadOperation::GlobalSize,
                    windows::core::Error::from_win32(),
                ));
            }
            Ok(self.payloads.get(&format).map(Vec::len))
        }

        fn lock(&self, format: u32) -> Result<Self::Locked, ClipboardReadError> {
            self.operations.borrow_mut().push(("lock", format));
            if self.lock_errors.contains(&format) {
                return Err(ClipboardReadError::windows(
                    super::ClipboardReadOperation::GlobalLock,
                    windows::core::Error::from_win32(),
                ));
            }
            Ok(FakeLockedClipboardMemory { format })
        }

        fn locked_size(&self, locked: &Self::Locked) -> Result<usize, ClipboardReadError> {
            self.operations
                .borrow_mut()
                .push(("locked_size", locked.format));
            self.locked_sizes
                .get(&locked.format)
                .cloned()
                .unwrap_or_else(|| Ok(self.payloads[&locked.format].len()))
                .map_err(|()| {
                    ClipboardReadError::windows(
                        super::ClipboardReadOperation::GlobalSize,
                        windows::core::Error::from_win32(),
                    )
                })
        }

        fn copy(&self, locked: &Self::Locked, size: usize) -> Result<Vec<u8>, ClipboardReadError> {
            self.operations.borrow_mut().push(("copy", locked.format));
            if self.copy_errors.contains(&locked.format) {
                return Err(ClipboardReadError::windows(
                    super::ClipboardReadOperation::GetData,
                    windows::core::Error::from_win32(),
                ));
            }
            self.copies.borrow_mut().push(locked.format);
            let bytes = self.payloads.get(&locked.format).cloned().unwrap();
            assert_eq!(bytes.len(), size);
            Ok(bytes)
        }

        fn unlock(&self, locked: &mut Self::Locked) -> Result<(), ClipboardReadError> {
            self.operations.borrow_mut().push(("unlock", locked.format));
            if self.unlock_errors.contains(&locked.format) {
                return Err(ClipboardReadError::windows(
                    super::ClipboardReadOperation::GlobalUnlock,
                    windows::core::Error::from_win32(),
                ));
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeClipboardFiles {
        paths: Vec<Vec<u16>>,
        count_override: Option<usize>,
        fail_length_at: Option<usize>,
        fail_data_at: Option<usize>,
        queried: RefCell<Vec<usize>>,
    }

    impl ClipboardFileReader for FakeClipboardFiles {
        fn count(&self) -> Result<Option<usize>, ClipboardReadError> {
            Ok(self
                .count_override
                .or_else(|| (!self.paths.is_empty()).then_some(self.paths.len())))
        }

        fn path_len(&self, index: usize) -> Result<usize, ClipboardReadError> {
            self.queried.borrow_mut().push(index);
            if self.fail_length_at == Some(index) {
                return Err(ClipboardReadError::InvalidFileList);
            }
            Ok(self.paths[index].len())
        }

        fn path(&self, index: usize, buffer: &mut [u16]) -> Result<usize, ClipboardReadError> {
            if self.fail_data_at == Some(index) {
                return Err(ClipboardReadError::InvalidFileList);
            }
            let path = &self.paths[index];
            buffer[..path.len()].copy_from_slice(path);
            Ok(path.len())
        }
    }

    fn stored_texts(records: &SessionRecordStore) -> Vec<String> {
        records
            .list()
            .into_iter()
            .filter_map(|record| record.text)
            .collect()
    }

    fn wait_for_record_count(
        receiver: &LatestClipboardEventReceiver,
        records: &SessionRecordStore,
        expected: usize,
    ) {
        receiver.wait_until_ingestion_idle(Duration::from_secs(1));
        assert_eq!(records.list().len(), expected);
    }

    #[test]
    fn capture_budget_skips_oversized_formats_before_copy() {
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([(100, vec![0; MAX_CAPTURE_PAYLOAD_BYTES + 1])]),
            ..Default::default()
        };

        let captured =
            read_bounded_representations(&reader, 100, 101, MAX_CAPTURE_PAYLOAD_BYTES).unwrap();

        assert!(captured.is_empty());
        assert!(reader.copies.borrow().is_empty());
    }

    #[test]
    fn capture_keeps_text_rtf_html_and_two_files_within_one_budget() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([
                (CF_UNICODETEXT_FORMAT, [b'h', 0, b'i', 0, 0, 0].to_vec()),
                (formats.rtf, br"{\rtf1 hi}".to_vec()),
                (formats.html, b"Version:1.0\r\n<html>hi</html>".to_vec()),
            ]),
            ..Default::default()
        };
        let files = FakeClipboardFiles {
            paths: vec![
                r"C:\one.txt".encode_utf16().collect(),
                r"D:\two.png".encode_utf16().collect(),
            ],
            ..Default::default()
        };

        let captured =
            read_bounded_representations_with_files(&reader, &files, formats, 1024 * 1024).unwrap();

        assert!(matches!(
            captured.as_slice(),
            [
                ClipboardRepresentation::UnicodeText { text },
                ClipboardRepresentation::Rtf { bytes: rtf },
                ClipboardRepresentation::Html { bytes: html },
                ClipboardRepresentation::FileList { paths },
            ] if text == "hi"
                && rtf == br"{\rtf1 hi}"
                && html == b"Version:1.0\r\n<html>hi</html>"
                && paths == &[r"C:\one.txt", r"D:\two.png"]
        ));
    }

    #[test]
    fn rich_format_failure_keeps_readable_unicode_text() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        for reader in [
            FakeClipboardMemory {
                payloads: HashMap::from([
                    (CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec()),
                    (formats.rtf, br"{\rtf1 bad}".to_vec()),
                ]),
                size_errors: vec![formats.rtf],
                ..Default::default()
            },
            FakeClipboardMemory {
                payloads: HashMap::from([
                    (CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec()),
                    (formats.rtf, br"{\rtf1 bad}".to_vec()),
                ]),
                copy_errors: vec![formats.rtf],
                ..Default::default()
            },
        ] {
            let captured = read_bounded_representations_with_files(
                &reader,
                &FakeClipboardFiles::default(),
                formats,
                1024,
            )
            .unwrap();
            assert!(matches!(
                captured.as_slice(),
                [ClipboardRepresentation::UnicodeText { text }] if text == "ok"
            ));
        }
    }

    #[test]
    fn changing_file_list_keeps_text_and_html() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([
                (CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec()),
                (formats.html, b"<b>ok</b>".to_vec()),
            ]),
            ..Default::default()
        };
        for files in [
            FakeClipboardFiles {
                paths: vec![r"C:\one.txt".encode_utf16().collect()],
                fail_length_at: Some(0),
                ..Default::default()
            },
            FakeClipboardFiles {
                paths: vec![r"C:\one.txt".encode_utf16().collect()],
                fail_data_at: Some(0),
                ..Default::default()
            },
        ] {
            let captured =
                read_bounded_representations_with_files(&reader, &files, formats, 1024).unwrap();
            assert!(matches!(
                captured.as_slice(),
                [
                    ClipboardRepresentation::UnicodeText { text },
                    ClipboardRepresentation::Html { bytes },
                ] if text == "ok" && bytes == b"<b>ok</b>"
            ));
        }
    }

    #[test]
    fn image_probe_failure_keeps_readable_unicode_text() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([(CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec())]),
            size_errors: vec![formats.png],
            ..Default::default()
        };

        let captured = read_bounded_representations_with_files(
            &reader,
            &FakeClipboardFiles::default(),
            formats,
            1024,
        )
        .unwrap();

        assert!(matches!(
            captured.as_slice(),
            [ClipboardRepresentation::UnicodeText { text }] if text == "ok"
        ));
    }

    #[test]
    fn failed_smaller_image_falls_back_to_larger_image_and_keeps_text() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        for reader in [
            FakeClipboardMemory {
                payloads: HashMap::from([
                    (CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec()),
                    (formats.png, vec![1; 4]),
                    (super::CF_DIBV5_FORMAT, vec![2; 6]),
                ]),
                locked_sizes: HashMap::from([(formats.png, Ok(5))]),
                ..Default::default()
            },
            FakeClipboardMemory {
                payloads: HashMap::from([
                    (CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec()),
                    (formats.png, vec![1; 4]),
                    (super::CF_DIBV5_FORMAT, vec![2; 6]),
                ]),
                copy_errors: vec![formats.png],
                ..Default::default()
            },
        ] {
            let captured = read_bounded_representations_with_files(
                &reader,
                &FakeClipboardFiles::default(),
                formats,
                1024,
            )
            .unwrap();

            assert!(matches!(
                captured.as_slice(),
                [
                    ClipboardRepresentation::UnicodeText { text },
                    ClipboardRepresentation::DibV5 { bytes },
                ] if text == "ok" && bytes == &[2; 6]
            ));
        }
    }

    #[test]
    fn read_unlock_failure_is_not_downgraded_to_a_missing_format() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([(CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec())]),
            unlock_errors: vec![CF_UNICODETEXT_FORMAT],
            ..Default::default()
        };

        let result = read_bounded_representations_with_files(
            &reader,
            &FakeClipboardFiles::default(),
            formats,
            1024,
        );

        assert!(matches!(
            result,
            Err(ClipboardReadError::Windows(
                super::ClipboardReadOperation::GlobalUnlock,
                _
            ))
        ));
    }

    #[test]
    fn locked_guard_retries_unlock_on_drop_and_frees_only_after_success() {
        let api = super::FakeableGlobalMemoryApi::new([false, true]);
        {
            let mut memory = super::OwnedGlobalMemory::new_for_test(7, true, api.clone());
            assert!(memory.unlock().is_err());
            assert!(memory.is_locked());
        }
        assert_eq!(api.operations(), ["unlock", "unlock", "free"]);
    }

    #[test]
    fn read_lock_retains_locked_state_and_retries_unlock_on_drop() {
        let api = super::FakeableGlobalMemoryApi::new([false, true]);
        let mut lock = super::GlobalMemoryLock::new_for_test(
            7,
            std::ptr::dangling_mut::<std::ffi::c_void>(),
            api.clone(),
        );
        assert!(lock.is_locked());
        assert!(lock.unlock().is_err());
        assert!(lock.is_locked());
        drop(lock);

        assert_eq!(api.operations(), ["unlock", "unlock"]);
    }

    #[test]
    fn read_unlock_and_close_failures_are_aggregated() {
        let operation = ClipboardReadError::windows(
            super::ClipboardReadOperation::GlobalUnlock,
            windows::core::Error::from_win32(),
        );
        let combined = combine_clipboard_operation_and_close::<(), _, _>(
            Err(operation),
            Err(windows::core::Error::from_win32()),
        )
        .map_err(ClipboardReadError::from_combined)
        .unwrap_err();

        assert!(matches!(
            combined,
            ClipboardReadError::OperationAndClose { operation, .. }
                if matches!(
                    *operation,
                    ClipboardReadError::Windows(
                        super::ClipboardReadOperation::GlobalUnlock,
                        _
                    )
                )
        ));
    }

    #[derive(Clone, Copy)]
    enum EarlyReadExit {
        SizeError,
        SizeMismatch,
    }

    struct CleanupTrackingReader {
        exit: EarlyReadExit,
        api: super::FakeableGlobalMemoryApi,
    }

    impl ClipboardMemoryReader for CleanupTrackingReader {
        type Locked = super::GlobalMemoryLock<super::FakeableGlobalMemoryApi>;

        fn size(&self, _format: u32) -> Result<Option<usize>, ClipboardReadError> {
            Ok(Some(4))
        }

        fn lock(&self, _format: u32) -> Result<Self::Locked, ClipboardReadError> {
            Ok(super::GlobalMemoryLock::new_for_test(
                7,
                std::ptr::dangling_mut::<std::ffi::c_void>(),
                self.api.clone(),
            ))
        }

        fn locked_size(&self, _locked: &Self::Locked) -> Result<usize, ClipboardReadError> {
            match self.exit {
                EarlyReadExit::SizeError => Err(ClipboardReadError::windows(
                    super::ClipboardReadOperation::GlobalSize,
                    windows::core::Error::from_win32(),
                )),
                EarlyReadExit::SizeMismatch => Ok(5),
            }
        }

        fn copy(
            &self,
            _locked: &Self::Locked,
            _size: usize,
        ) -> Result<Vec<u8>, ClipboardReadError> {
            panic!("early read exits must not copy clipboard data")
        }

        fn unlock(&self, locked: &mut Self::Locked) -> Result<(), ClipboardReadError> {
            locked.unlock()
        }
    }

    #[test]
    fn early_read_exit_propagates_unlock_failure_retries_drop_and_aggregates_close() {
        for exit in [EarlyReadExit::SizeError, EarlyReadExit::SizeMismatch] {
            let api = super::FakeableGlobalMemoryApi::new([false, true]);
            let reader = CleanupTrackingReader {
                exit,
                api: api.clone(),
            };
            let closer = super::FakeableClipboardCloser::new([false, true]);
            let clipboard = super::ClipboardGuard::new_for_test(closer.clone());
            let operation = super::copy_unchanged(&reader, 100, 4).map(|_| ());
            let result = super::finish_clipboard_read(operation, clipboard);

            assert!(matches!(
                result,
                Err(ClipboardReadError::OperationAndClose { operation, .. })
                    if matches!(
                        *operation,
                        ClipboardReadError::Windows(
                            super::ClipboardReadOperation::GlobalUnlock,
                            _
                        )
                    )
            ));
            assert_eq!(api.operations(), ["unlock", "unlock"]);
            assert_eq!(closer.calls(), 2);
        }
    }

    #[test]
    fn read_clipboard_guard_retries_close_on_drop() {
        let closer = super::FakeableClipboardCloser::new([false, true]);
        {
            let guard = super::ClipboardGuard::new_for_test(closer.clone());
            assert!(guard.close().is_err());
        }
        assert_eq!(closer.calls(), 2);
    }

    #[test]
    fn publication_rejects_aggregate_logical_budget_before_preparation() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let half = MAX_CAPTURE_PAYLOAD_BYTES / 2;
        let representations = [
            ClipboardRepresentation::Rtf {
                bytes: vec![1; half],
            },
            ClipboardRepresentation::Html {
                bytes: vec![2; half],
            },
        ];

        let state = FakeAllocationState::default();
        let allocator = FakeAllocator {
            fail_at: None,
            failure: super::ClipboardWriteOperation::Allocate,
            state: state.clone(),
        };
        let result =
            prepare_representations_then_open(&representations, formats, &allocator, || {
                state.operations.lock().unwrap().push("open");
                Ok(())
            });

        assert!(matches!(result, Err(ClipboardWriteError::PayloadTooLarge)));
        assert!(state.operations.lock().unwrap().is_empty());
    }

    #[test]
    fn publication_rejects_expanded_windows_payload_before_allocation_or_open() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let representations = [ClipboardRepresentation::UnicodeText {
            text: "a".repeat(MAX_CAPTURE_PAYLOAD_BYTES / 2 + 1),
        }];
        let state = FakeAllocationState::default();
        let allocator = FakeAllocator {
            fail_at: None,
            failure: super::ClipboardWriteOperation::Allocate,
            state: state.clone(),
        };
        let result =
            prepare_representations_then_open(&representations, formats, &allocator, || {
                state.operations.lock().unwrap().push("open");
                Ok(())
            });

        assert!(matches!(result, Err(ClipboardWriteError::PayloadTooLarge)));
        assert!(state.operations.lock().unwrap().is_empty());
    }

    #[test]
    fn excessive_file_count_skips_file_list_without_querying_items() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([(CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec())]),
            ..Default::default()
        };
        let files = FakeClipboardFiles {
            count_override: Some(super::MAX_CAPTURE_FILE_COUNT + 1),
            ..Default::default()
        };

        let captured =
            read_bounded_representations_with_files(&reader, &files, formats, 1024).unwrap();

        assert!(matches!(
            captured.as_slice(),
            [ClipboardRepresentation::UnicodeText { text }] if text == "ok"
        ));
        assert!(files.queried.borrow().is_empty());
    }

    #[test]
    fn zero_length_or_changing_file_path_keeps_other_formats() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([
                (CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec()),
                (formats.html, b"<b>ok</b>".to_vec()),
            ]),
            ..Default::default()
        };
        for files in [
            FakeClipboardFiles {
                paths: vec![Vec::new()],
                ..Default::default()
            },
            FakeClipboardFiles {
                paths: vec![r"C:\one.txt".encode_utf16().collect()],
                fail_data_at: Some(0),
                ..Default::default()
            },
        ] {
            let captured =
                read_bounded_representations_with_files(&reader, &files, formats, 1024).unwrap();
            assert!(matches!(
                captured.as_slice(),
                [
                    ClipboardRepresentation::UnicodeText { text },
                    ClipboardRepresentation::Html { bytes },
                ] if text == "ok" && bytes == b"<b>ok</b>"
            ));
        }
    }

    #[derive(Clone, Default)]
    struct FakeAllocationState {
        operations: Arc<Mutex<Vec<&'static str>>>,
        freed: Arc<Mutex<Vec<usize>>>,
    }

    struct FakePendingMemory {
        id: usize,
        transferred: bool,
        state: FakeAllocationState,
    }

    impl Drop for FakePendingMemory {
        fn drop(&mut self) {
            if !self.transferred {
                self.state.freed.lock().unwrap().push(self.id);
            }
        }
    }

    impl ClipboardPendingMemory for FakePendingMemory {
        type Handle = usize;

        fn handle(&self) -> Self::Handle {
            self.id
        }

        fn transfer(&mut self) {
            self.transferred = true;
        }
    }

    struct FakeAllocator {
        fail_at: Option<usize>,
        failure: super::ClipboardWriteOperation,
        state: FakeAllocationState,
    }

    impl ClipboardGlobalAllocator for FakeAllocator {
        type Memory = FakePendingMemory;

        fn allocate_and_copy(&self, bytes: &[u8]) -> Result<Self::Memory, ClipboardWriteError> {
            let mut operations = self.state.operations.lock().unwrap();
            let id = operations
                .iter()
                .filter(|operation| **operation == "allocate")
                .count();
            operations.push("allocate");
            if self.fail_at == Some(id) {
                return Err(ClipboardWriteError::Windows(
                    self.failure,
                    windows::core::Error::from_win32(),
                ));
            }
            assert!(!bytes.is_empty());
            Ok(FakePendingMemory {
                id,
                transferred: false,
                state: self.state.clone(),
            })
        }
    }

    struct FakeTransferApi {
        fail_at: Option<usize>,
        operations: Arc<Mutex<Vec<&'static str>>>,
        sets: usize,
    }

    impl ClipboardTransferApi<usize> for FakeTransferApi {
        fn empty(&mut self) -> Result<(), ClipboardWriteError> {
            self.operations.lock().unwrap().push("empty");
            Ok(())
        }

        fn set(&mut self, _format: u32, _handle: usize) -> Result<(), ClipboardWriteError> {
            self.operations.lock().unwrap().push("set");
            let index = self.sets;
            self.sets += 1;
            if self.fail_at == Some(index) {
                Err(ClipboardWriteError::Windows(
                    super::ClipboardWriteOperation::Publish,
                    windows::core::Error::from_win32(),
                ))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn preparation_failure_happens_before_clipboard_is_opened_or_emptied() {
        let state = FakeAllocationState::default();
        let prepared = vec![
            super::PreparedClipboardPayload {
                format: 1,
                bytes: vec![1],
            },
            super::PreparedClipboardPayload {
                format: 2,
                bytes: vec![2],
            },
        ];
        for failure in [
            super::ClipboardWriteOperation::Allocate,
            super::ClipboardWriteOperation::Lock,
        ] {
            state.operations.lock().unwrap().clear();
            state.freed.lock().unwrap().clear();
            let allocator = FakeAllocator {
                fail_at: Some(1),
                failure,
                state: state.clone(),
            };

            let result = prepare_then_open(&prepared, &allocator, || {
                state.operations.lock().unwrap().push("open");
                Ok(())
            });

            assert!(result.is_err());
            assert_eq!(
                &*state.operations.lock().unwrap(),
                &["allocate", "allocate"]
            );
            assert_eq!(&*state.freed.lock().unwrap(), &[0]);
            assert!(!state.operations.lock().unwrap().contains(&"open"));
            assert!(!state.operations.lock().unwrap().contains(&"empty"));
            assert!(!state.operations.lock().unwrap().contains(&"set"));
        }
    }

    #[test]
    fn transfer_failure_frees_only_handles_not_owned_by_the_clipboard() {
        let state = FakeAllocationState::default();
        let prepared = vec![
            super::PreparedClipboardPayload {
                format: 1,
                bytes: vec![1],
            },
            super::PreparedClipboardPayload {
                format: 2,
                bytes: vec![2],
            },
            super::PreparedClipboardPayload {
                format: 3,
                bytes: vec![3],
            },
        ];
        let allocator = FakeAllocator {
            fail_at: None,
            failure: super::ClipboardWriteOperation::Allocate,
            state: state.clone(),
        };
        let mut pending = prepare_pending_payloads(&prepared, &allocator).unwrap();
        let mut api = FakeTransferApi {
            fail_at: Some(1),
            operations: state.operations.clone(),
            sets: 0,
        };

        assert!(transfer_pending_payloads(&mut pending, &mut api).is_err());
        drop(pending);

        let mut freed = state.freed.lock().unwrap().clone();
        freed.sort_unstable();
        assert_eq!(freed, [1, 2]);
        assert!(!freed.contains(&0));
        assert_eq!(
            &*state.operations.lock().unwrap(),
            &["allocate", "allocate", "allocate", "empty", "set", "set"]
        );
    }

    #[test]
    fn file_list_budget_uses_final_utf8_bytes_and_representation_overhead() {
        let files = FakeClipboardFiles {
            paths: vec!["C:\\界.txt".encode_utf16().collect()],
            ..Default::default()
        };
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let logical =
            "C:\\界.txt".len() + crate::services::session_records::REPRESENTATION_OVERHEAD_BYTES;
        let required = logical + super::FILE_PATH_RESOURCE_OVERHEAD_BYTES;

        let rejected = read_bounded_representations_with_files(
            &FakeClipboardMemory::default(),
            &files,
            formats,
            required - 1,
        )
        .unwrap();
        assert!(rejected.is_empty());

        let captured = read_bounded_representations_with_files(
            &FakeClipboardMemory::default(),
            &files,
            formats,
            required,
        )
        .unwrap();
        assert_eq!(representation_bytes(&captured), logical);
        assert!(matches!(
            captured.as_slice(),
            [ClipboardRepresentation::FileList { paths }] if paths == &["C:\\界.txt"]
        ));
    }

    #[test]
    fn dropfiles_serialization_has_wide_header_and_double_nul_path_list() {
        let bytes =
            serialize_dropfiles(&[r"C:\one.txt".to_owned(), r"D:\two.png".to_owned()]).unwrap();
        let header_size = size_of::<windows::Win32::UI::Shell::DROPFILES>();
        let header = unsafe {
            ptr::read_unaligned(
                bytes
                    .as_ptr()
                    .cast::<windows::Win32::UI::Shell::DROPFILES>(),
            )
        };
        assert_eq!(header.pFiles as usize, header_size);
        assert!(header.fWide.as_bool());

        let units = bytes[header_size..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        let expected = r"C:\one.txt"
            .encode_utf16()
            .chain([0])
            .chain(r"D:\two.png".encode_utf16())
            .chain([0, 0])
            .collect::<Vec<_>>();
        assert_eq!(units, expected);
    }

    #[test]
    fn publication_prepares_all_supported_formats_before_touching_clipboard() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        let prepared = prepare_representations(
            &[
                ClipboardRepresentation::UnicodeText {
                    text: "text".to_owned(),
                },
                ClipboardRepresentation::Rtf {
                    bytes: br"{\rtf1 text}".to_vec(),
                },
                ClipboardRepresentation::Html {
                    bytes: b"<b>text</b>".to_vec(),
                },
                ClipboardRepresentation::FileList {
                    paths: vec![r"C:\one.txt".to_owned()],
                },
            ],
            formats,
        )
        .unwrap();

        assert_eq!(
            prepared
                .iter()
                .map(|payload| payload.format)
                .collect::<Vec<_>>(),
            [
                CF_UNICODETEXT_FORMAT,
                formats.rtf,
                formats.html,
                CF_HDROP_FORMAT
            ]
        );
    }

    #[test]
    fn invalid_file_lists_fail_during_preparation() {
        let formats = RegisteredFormats {
            png: 100,
            rtf: 101,
            html: 102,
        };
        for paths in [vec![String::new()], vec!["C:\\bad\0name.txt".to_owned()]] {
            assert!(matches!(
                prepare_representations(&[ClipboardRepresentation::FileList { paths }], formats),
                Err(ClipboardWriteError::InvalidFileList)
            ));
        }
    }

    #[test]
    fn capture_materializes_only_the_smaller_image_format() {
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([(100, vec![1; 8]), (101, vec![2; 4])]),
            ..Default::default()
        };

        let captured = read_bounded_representations(&reader, 100, 101, 64).unwrap();

        assert!(
            matches!(captured.as_slice(), [ClipboardRepresentation::DibV5 { bytes }] if bytes.len() == 4)
        );
        assert_eq!(&*reader.copies.borrow(), &[101]);
    }

    #[test]
    fn capture_prioritizes_text_and_only_copies_an_image_that_fits_remaining_budget() {
        let text = [b'a', 0, 0, 0].to_vec();
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([
                (CF_UNICODETEXT_FORMAT, text),
                (100, vec![1; 7]),
                (101, vec![2; 8]),
            ]),
            ..Default::default()
        };

        let captured = read_bounded_representations(&reader, 100, 101, 36).unwrap();

        assert!(
            matches!(captured.as_slice(), [ClipboardRepresentation::UnicodeText { text }] if text == "a")
        );
        assert_eq!(&*reader.copies.borrow(), &[CF_UNICODETEXT_FORMAT]);
    }

    #[test]
    fn capture_skips_format_when_size_changes_after_lock() {
        for locked_size in [Ok(3), Ok(5), Err(())] {
            let reader = FakeClipboardMemory {
                payloads: HashMap::from([
                    (CF_UNICODETEXT_FORMAT, [b'o', 0, b'k', 0, 0, 0].to_vec()),
                    (100, vec![1; 4]),
                ]),
                locked_sizes: HashMap::from([(100, locked_size)]),
                ..Default::default()
            };

            let captured = read_bounded_representations(&reader, 100, 101, 128).unwrap();

            assert!(matches!(
                captured.as_slice(),
                [ClipboardRepresentation::UnicodeText { text }] if text == "ok"
            ));
            assert_eq!(&*reader.copies.borrow(), &[CF_UNICODETEXT_FORMAT]);
            assert_eq!(
                &*reader.operations.borrow(),
                &[
                    ("lock", CF_UNICODETEXT_FORMAT),
                    ("locked_size", CF_UNICODETEXT_FORMAT),
                    ("copy", CF_UNICODETEXT_FORMAT),
                    ("unlock", CF_UNICODETEXT_FORMAT),
                    ("lock", 100),
                    ("locked_size", 100),
                    ("unlock", 100),
                ]
            );
        }
    }

    #[test]
    fn final_utf8_bytes_and_overhead_control_the_aggregate_budget() {
        let text = "界";
        let mut encoded = text
            .encode_utf16()
            .flat_map(u16::to_ne_bytes)
            .collect::<Vec<_>>();
        encoded.extend_from_slice(&0_u16.to_ne_bytes());
        let reader = FakeClipboardMemory {
            payloads: HashMap::from([(CF_UNICODETEXT_FORMAT, encoded), (100, vec![1; 1])]),
            ..Default::default()
        };

        let captured = read_bounded_representations(&reader, 100, 101, 36).unwrap();

        assert!(matches!(
            captured.as_slice(),
            [ClipboardRepresentation::UnicodeText { text }] if text == "界"
        ));
        assert_eq!(&*reader.copies.borrow(), &[CF_UNICODETEXT_FORMAT]);
        assert_eq!(representation_bytes(&captured), 35);
        assert!(SessionRecordStore::default().capture(CapturedClipboard {
            content_identity: ContentIdentity::new("boundary"),
            captured_at: Utc::now(),
            source: SourceIdentity::default(),
            representations: captured,
        }));
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

    #[test]
    fn capture_policy_blocks_paused_and_excluded_sources_before_history() {
        let records = Arc::new(SessionRecordStore::default());
        let policy = Arc::new(CapturePolicy::new(vec!["KeePass.exe".to_owned()]));
        let (events, receiver) =
            latest_clipboard_event_channel_with_policy(Arc::clone(&records), Arc::clone(&policy));
        let mut excluded = text_event(1, "secret");
        excluded.captured.source = SourceIdentity {
            application_name: Some("KeePass".to_owned()),
            executable_path: Some(r"C:\Tools\KeePass.exe".to_owned()),
        };
        events.send(excluded).unwrap();
        receiver.wait_for_processed_generation(1, Duration::from_secs(1));
        assert!(records.list().is_empty());

        policy.set_paused(true);
        events.send(text_event(2, "paused")).unwrap();
        receiver.wait_for_processed_generation(2, Duration::from_secs(1));
        assert!(records.list().is_empty());

        policy.set_paused(false);
        events.send(text_event(3, "allowed")).unwrap();
        receiver.wait_for_processed_generation(3, Duration::from_secs(1));
        assert_eq!(records.list()[0].text.as_deref(), Some("allowed"));
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

    fn text_event(sequence_number: u32, text: &str) -> ClipboardEvent {
        ClipboardEvent {
            sequence_number,
            captured: CapturedClipboard {
                content_identity: ContentIdentity::new(format!("test:{sequence_number}")),
                captured_at: Utc::now(),
                source: SourceIdentity::default(),
                representations: vec![ClipboardRepresentation::UnicodeText {
                    text: text.to_owned(),
                }],
            },
        }
    }
}
