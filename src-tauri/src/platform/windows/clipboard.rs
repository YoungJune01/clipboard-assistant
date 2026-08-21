use std::{
    error::Error,
    fmt,
    sync::{
        Arc, Mutex,
        mpsc::{Sender, sync_channel},
    },
    thread::{self, JoinHandle},
};

use chrono::Utc;
use windows::Win32::{
    Foundation::{HGLOBAL, HWND},
    System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
            RegisterClipboardFormatW,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
    },
};
use windows::core::w;

use crate::domain::{CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity};

use super::message_loop::{ListenerState, WM_CLIPBOARD_LISTENER_SHUTDOWN, run_message_loop};

const CF_UNICODETEXT_FORMAT: u32 = 13;
const CF_DIBV5_FORMAT: u32 = 17;
const MAX_PENDING_PRODUCT_WRITE_EVENTS: usize = 32;

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
    thread: Option<JoinHandle<Result<(), ClipboardListenerError>>>,
}

impl ClipboardListener {
    pub fn start(events: Sender<ClipboardEvent>) -> Result<Self, ClipboardListenerError> {
        let product_write = Arc::new(Mutex::new(ProductWriteState::Idle));
        let thread_product_write = Arc::clone(&product_write);
        let (ready_sender, ready_receiver) = sync_channel(1);
        let (shutdown_sender, shutdown_receiver) = std::sync::mpsc::channel();
        let thread_events = events.clone();
        let thread = thread::Builder::new()
            .name("clipboard-listener".to_owned())
            .spawn(move || {
                run_message_loop(
                    thread_events,
                    thread_product_write,
                    ready_sender,
                    shutdown_receiver,
                )
            })
            .map_err(ClipboardListenerError::ThreadSpawn)?;

        match ready_receiver.recv() {
            Ok(Ok(hwnd_value)) => Ok(Self {
                hwnd_value,
                events,
                product_write,
                shutdown: shutdown_sender,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => match thread.join() {
                Ok(Err(error)) => Err(error),
                Ok(Ok(())) => Err(ClipboardListenerError::UnexpectedThreadExit),
                Err(_) => Err(ClipboardListenerError::ThreadPanicked),
            },
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
        let shutdown_sent = self.shutdown.send(()).is_ok();
        unsafe {
            if let Err(error) = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(HWND(self.hwnd_value as *mut std::ffi::c_void)),
                WM_CLIPBOARD_LISTENER_SHUTDOWN,
                Default::default(),
                Default::default(),
            ) {
                self.thread = Some(thread);
                return Err(ClipboardListenerError::Windows(error));
            }
        }
        match thread.join() {
            Ok(result) if shutdown_sent => result,
            Ok(Err(error)) => Err(error),
            Ok(Ok(())) => Err(ClipboardListenerError::UnexpectedThreadExit),
            Err(_) => Err(ClipboardListenerError::ThreadPanicked),
        }
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
        }
    }
}

impl Error for ClipboardListenerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn(error) => Some(error),
            Self::Windows(error) => Some(error),
            Self::ProductWriteAlreadyInProgress
            | Self::UnexpectedThreadExit
            | Self::ThreadPanicked => None,
        }
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

fn read_supported_representations() -> windows::core::Result<(u32, Vec<ClipboardRepresentation>)> {
    let _clipboard = ClipboardGuard::open_with_retry()?;
    let sequence_number = unsafe { GetClipboardSequenceNumber() };
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let mut representations = Vec::with_capacity(3);
    if let Some(text) = read_unicode_text(CF_UNICODETEXT_FORMAT) {
        representations.push(ClipboardRepresentation::UnicodeText { text });
    }
    if png_format != 0
        && let Some(bytes) = read_global_bytes(png_format)
    {
        representations.push(ClipboardRepresentation::Png { bytes });
    }
    if let Some(bytes) = read_global_bytes(CF_DIBV5_FORMAT) {
        representations.push(ClipboardRepresentation::DibV5 { bytes });
    }
    Ok((sequence_number, representations))
}

struct ClipboardGuard;

impl ClipboardGuard {
    fn open_with_retry() -> windows::core::Result<Self> {
        const DELAYS_MS: [u64; 5] = [5, 10, 20, 40, 80];
        let mut last_error = match unsafe { OpenClipboard(None) } {
            Ok(()) => return Ok(Self),
            Err(error) => error,
        };
        for delay in DELAYS_MS {
            thread::sleep(std::time::Duration::from_millis(delay));
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn read_unicode_text(format: u32) -> Option<String> {
    let handle = unsafe { GetClipboardData(format).ok()? };
    let memory = HGLOBAL(handle.0);
    let lock = GlobalMemoryLock::new(memory)?;
    let byte_len = unsafe { GlobalSize(memory) };
    if byte_len < size_of::<u16>() {
        return None;
    }
    let units = unsafe { std::slice::from_raw_parts(lock.pointer.cast::<u16>(), byte_len / 2) };
    let nul = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16(&units[..nul]).ok()
}

fn read_global_bytes(format: u32) -> Option<Vec<u8>> {
    let handle = unsafe { GetClipboardData(format).ok()? };
    let memory = HGLOBAL(handle.0);
    let lock = GlobalMemoryLock::new(memory)?;
    let len = unsafe { GlobalSize(memory) };
    if len == 0 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(lock.pointer.cast::<u8>(), len) }.to_vec())
}

struct GlobalMemoryLock {
    memory: HGLOBAL,
    pointer: *mut std::ffi::c_void,
}

impl GlobalMemoryLock {
    fn new(memory: HGLOBAL) -> Option<Self> {
        let pointer = unsafe { GlobalLock(memory) };
        (!pointer.is_null()).then_some(Self { memory, pointer })
    }
}

impl Drop for GlobalMemoryLock {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalUnlock(self.memory);
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
    use std::sync::{Arc, Mutex, mpsc};

    use chrono::Utc;

    use super::{
        ClipboardEvent, MAX_PENDING_PRODUCT_WRITE_EVENTS, ProductWriteGuard, ProductWriteState,
        route_event,
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
