use std::{
    cell::RefCell,
    collections::HashMap,
    collections::hash_map::DefaultHasher,
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    mem::{size_of, zeroed},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::{
        CloseHandle, ERROR_SUCCESS, FILETIME, GetLastError, HANDLE, HWND, SetLastError, WIN32_ERROR,
    },
    Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsTokenRestricted,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TokenIntegrityLevel, TokenIsAppContainer,
    },
    System::{
        RemoteDesktop::ProcessIdToSessionId,
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_READOBJECTS, GetThreadDesktop, GetUserObjectInformationW,
            OpenInputDesktop, UOI_NAME,
        },
        Threading::{
            AttachThreadInput, GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
            GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::{
        Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
            SendInput, SetFocus, VIRTUAL_KEY, VK_CONTROL, VK_V,
        },
        WindowsAndMessaging::{
            CHILDID_SELF, DispatchMessageW, EVENT_OBJECT_DESTROY, GA_ROOT, GCW_ATOM, GUITHREADINFO,
            GWLP_HINSTANCE, GWLP_USERDATA, GWLP_WNDPROC, GetAncestor, GetClassLongPtrW,
            GetForegroundWindow, GetGUIThreadInfo, GetWindowLongPtrW, GetWindowThreadProcessId,
            IsWindow, MSG, OBJID_WINDOW, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW,
            WINEVENT_OUTOFCONTEXT, WM_APP,
        },
    },
};

use crate::domain::TargetToken;
use crate::services::paste::{PasteInput, PasteTarget, TargetIdentity, TargetSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsWindow(isize);

impl WindowsWindow {
    fn from_hwnd(window: HWND) -> Self {
        Self(window.0 as isize)
    }

    fn hwnd(self) -> HWND {
        HWND(self.0 as *mut std::ffi::c_void)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsPasteError {
    NoForegroundWindow,
    InvalidWindow,
    WindowIdentity,
    ProcessOpen,
    ProcessTimes,
    ProcessToken,
    TokenInformation,
    SessionInformation,
    DesktopInformation,
    ForegroundRejected,
    FocusRestore,
    PhysicalPasteKeyDown,
    ForegroundChanged,
    InputDispatch,
    InputCleanup,
    LifecycleProof,
}

impl fmt::Display for WindowsPasteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NoForegroundWindow => "no eligible foreground paste target is available",
            Self::InvalidWindow => "the paste target window is no longer valid",
            Self::WindowIdentity => "the paste target window identity could not be verified",
            Self::ProcessOpen => "the paste target process could not be inspected",
            Self::ProcessTimes => "the paste target process lifetime could not be verified",
            Self::ProcessToken => "the paste target security token could not be opened",
            Self::TokenInformation => "the paste target security token could not be verified",
            Self::SessionInformation => "the paste target session could not be verified",
            Self::DesktopInformation => "the input desktop could not be verified",
            Self::ForegroundRejected => "Windows rejected foreground restoration",
            Self::FocusRestore => "the focused control could not be restored",
            Self::PhysicalPasteKeyDown => "a physical paste key is already held down",
            Self::ForegroundChanged => "the foreground window changed during paste dispatch",
            Self::InputDispatch => "the paste shortcut could not be dispatched completely",
            Self::InputCleanup => "paste shortcut cleanup could not release injected keys",
            Self::LifecycleProof => "the paste target lifetime could not be proven",
        })
    }
}

impl Error for WindowsPasteError {}

pub struct Win32PasteTarget {
    observer: Option<Win32LifecycleObserver>,
}

impl Win32PasteTarget {
    pub fn new() -> Self {
        Self {
            observer: Win32LifecycleObserver::start().ok(),
        }
    }
}

impl Default for Win32PasteTarget {
    fn default() -> Self {
        Self::new()
    }
}

impl PasteTarget for Win32PasteTarget {
    type Error = WindowsPasteError;
    type Window = WindowsWindow;

    fn capture(&self) -> Result<TargetSnapshot<Self::Window>, Self::Error> {
        let observer = self
            .observer
            .as_ref()
            .ok_or(WindowsPasteError::LifecycleProof)?;
        let (lifecycle_token, captured) = capture_with_observer(observer, capture_target_state)?;
        let window = captured.top;
        let captured = captured.value;
        let identity = captured.identity;
        let thread_id = identity.thread_id;
        let focused = captured.focused;
        let focused_control_instance_id = captured.focused_control_instance_id;
        let snapshot = TargetSnapshot {
            window: WindowsWindow::from_hwnd(window),
            focused_control: (focused != window).then(|| WindowsWindow::from_hwnd(focused)),
            identity,
            lifecycle_token,
            focused_control_instance_id,
        };
        retain_validated_capture(
            snapshot,
            lifecycle_token,
            |snapshot| {
                Ok(lifecycle_valid(snapshot)?
                    && inspect_window(window)? == identity
                    && foreground_root() == Some(window)
                    && focused_control(thread_id)? == Some(focused))
            },
            release_lifecycle,
        )
    }

    fn inspect(&self, window: Self::Window) -> Result<TargetIdentity, Self::Error> {
        inspect_window(window.hwnd())
    }

    fn input_allowed(&self, identity: &TargetIdentity) -> Result<bool, Self::Error> {
        let current = current_process_security()?;
        let current_session = process_session(unsafe { GetCurrentProcessId() })?;
        Ok(identity.process_id != unsafe { GetCurrentProcessId() }
            && identity.session_id == current_session
            && identity.integrity_level <= current.integrity_level
            && !identity.restricted
            && !identity.app_container)
    }

    fn input_desktop(&self) -> Result<u64, Self::Error> {
        input_desktop_identity()
    }

    fn restore(&self, target: &TargetSnapshot<Self::Window>) -> Result<(), Self::Error> {
        if !lifecycle_valid(target)? || inspect_window(target.window.hwnd())? != target.identity {
            return Err(WindowsPasteError::LifecycleProof);
        }
        crate::platform::windows::activation::activate(target.window.hwnd())
            .map_err(|_| WindowsPasteError::ForegroundRejected)?;
        if !lifecycle_valid(target)? || inspect_window(target.window.hwnd())? != target.identity {
            return Err(WindowsPasteError::LifecycleProof);
        }
        if let Some(control) = target.focused_control {
            restore_focus_control(
                target,
                control,
                target.focused_control_instance_id,
                target.identity.process_id,
                target.identity.thread_id,
            )?;
        }
        if unsafe { GetForegroundWindow() } == target.window.hwnd() {
            Ok(())
        } else {
            Err(WindowsPasteError::ForegroundRejected)
        }
    }

    fn foreground(&self) -> Result<Self::Window, Self::Error> {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.0.is_null() {
            Err(WindowsPasteError::NoForegroundWindow)
        } else {
            let root = unsafe { GetAncestor(foreground, GA_ROOT) };
            Ok(WindowsWindow::from_hwnd(if root.0.is_null() {
                foreground
            } else {
                root
            }))
        }
    }

    fn lifecycle_valid(&self, target: &TargetSnapshot<Self::Window>) -> Result<bool, Self::Error> {
        lifecycle_valid(target)
    }

    fn release_lifecycle(&self, target: &TargetSnapshot<Self::Window>) {
        release_lifecycle(target.lifecycle_token);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CapturedTargetState {
    identity: TargetIdentity,
    focused: HWND,
    focused_control_instance_id: Option<u64>,
}

fn capture_target_state() -> Result<ObservedCapture<HWND, CapturedTargetState>, WindowsPasteError> {
    let window = foreground_root().ok_or(WindowsPasteError::NoForegroundWindow)?;
    let identity = inspect_window(window)?;
    let focused = focused_control(identity.thread_id)?.ok_or(WindowsPasteError::LifecycleProof)?;
    let focused_control_instance_id = if focused == window {
        None
    } else {
        if window_process_thread(focused)? != (identity.process_id, identity.thread_id) {
            return Err(WindowsPasteError::LifecycleProof);
        }
        Some(window_instance_id(focused)?)
    };
    if foreground_root() != Some(window)
        || inspect_window(window)? != identity
        || focused_control(identity.thread_id)? != Some(focused)
    {
        return Err(WindowsPasteError::LifecycleProof);
    }
    Ok(ObservedCapture {
        top: window,
        focus: Some(focused),
        value: CapturedTargetState {
            identity,
            focused,
            focused_control_instance_id,
        },
    })
}

const LIFECYCLE_STOP_MESSAGE: u32 = WM_APP + 0x31;
const LIFECYCLE_SYNC_MESSAGE: u32 = WM_APP + 0x32;
const LIFECYCLE_SYNC_TIMEOUT: Duration = Duration::from_secs(1);

static NEXT_LIFECYCLE_TOKEN: AtomicUsize = AtomicUsize::new(1);
static NEXT_LIFECYCLE_BARRIER_NONCE: AtomicU64 = AtomicU64::new(1);
static NEXT_LIFECYCLE_OBSERVER_EPOCH: AtomicU64 = AtomicU64::new(1);
static LIFECYCLE_REGISTRY: OnceLock<Mutex<HashMap<usize, LifecycleEntry>>> = OnceLock::new();

thread_local! {
    static THREAD_LIFECYCLE_STATE: RefCell<Option<Arc<LifecycleState>>> = const { RefCell::new(None) };
}

struct LifecycleState {
    observer_epoch: u64,
    destroy_generation: AtomicU64,
    alive: AtomicBool,
    stop: AtomicBool,
    tokens: Mutex<HashMap<usize, TrackedLifecycle>>,
}

struct TrackedLifecycle {
    top: isize,
    focus: isize,
    process_id: u32,
    thread_id: u32,
    observer_epoch: u64,
    capture_generation: u64,
    valid: bool,
}

impl LifecycleState {
    fn new() -> Self {
        Self {
            observer_epoch: NEXT_LIFECYCLE_OBSERVER_EPOCH.fetch_add(1, Ordering::Relaxed),
            destroy_generation: AtomicU64::new(0),
            alive: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    fn activate(
        &self,
        token: usize,
        top: HWND,
        focus: Option<HWND>,
        process_id: u32,
        thread_id: u32,
        capture_generation: u64,
    ) -> Result<(), WindowsPasteError> {
        if !self.alive.load(Ordering::Acquire)
            || self.destroy_generation.load(Ordering::Acquire) != capture_generation
        {
            return Err(WindowsPasteError::LifecycleProof);
        }
        lock_unpoisoned(&self.tokens).insert(
            token,
            TrackedLifecycle {
                top: top.0 as isize,
                focus: focus.map_or(0, |window| window.0 as isize),
                process_id,
                thread_id,
                observer_epoch: self.observer_epoch,
                capture_generation,
                valid: true,
            },
        );
        if !self.alive.load(Ordering::Acquire)
            || self.destroy_generation.load(Ordering::Acquire) != capture_generation
        {
            lock_unpoisoned(&self.tokens).remove(&token);
            return Err(WindowsPasteError::LifecycleProof);
        }
        Ok(())
    }

    fn valid_for(
        &self,
        token: usize,
        top: HWND,
        focus: HWND,
        process_id: u32,
        thread_id: u32,
        capture_generation: u64,
    ) -> bool {
        self.alive.load(Ordering::Acquire)
            && lock_unpoisoned(&self.tokens)
                .get(&token)
                .is_some_and(|entry| {
                    entry.valid
                        && entry.observer_epoch == self.observer_epoch
                        && entry.capture_generation == capture_generation
                        && entry.top == top.0 as isize
                        && entry.focus == focus.0 as isize
                        && entry.process_id == process_id
                        && entry.thread_id == thread_id
                })
    }
}

struct LifecycleEntry {
    state: Arc<LifecycleState>,
    capture_generation: u64,
    observer_thread_id: u32,
    barrier: Arc<LifecycleBarrierClient>,
}

struct Win32LifecycleObserver {
    state: Arc<LifecycleState>,
    observer_thread_id: u32,
    barrier: Arc<LifecycleBarrierClient>,
    thread: Option<JoinHandle<()>>,
}

impl Win32LifecycleObserver {
    fn start() -> Result<Self, WindowsPasteError> {
        let state = Arc::new(LifecycleState::new());
        let thread_state = Arc::clone(&state);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (barrier_request_tx, barrier_request_rx) = mpsc::channel();
        let (barrier_ack_tx, barrier_ack_rx) = mpsc::channel();
        let barrier = Arc::new(LifecycleBarrierClient {
            requests: barrier_request_tx,
            acknowledgements: Mutex::new(barrier_ack_rx),
        });
        let thread = thread::Builder::new()
            .name("paste-lifecycle-observer".into())
            .spawn(move || {
                lifecycle_observer_thread(
                    thread_state,
                    ready_tx,
                    barrier_request_rx,
                    barrier_ack_tx,
                )
            })
            .map_err(|_| WindowsPasteError::LifecycleProof)?;
        match ready_rx.recv() {
            Ok(Ok(observer_thread_id)) => Ok(Self {
                state,
                observer_thread_id,
                barrier,
                thread: Some(thread),
            }),
            _ => {
                let _ = thread.join();
                Err(WindowsPasteError::LifecycleProof)
            }
        }
    }

    fn capture_generation(&self) -> Result<u64, WindowsPasteError> {
        self.barrier.request(self.observer_thread_id)?;
        if !self.state.alive.load(Ordering::Acquire) {
            return Err(WindowsPasteError::LifecycleProof);
        }
        Ok(self.state.destroy_generation.load(Ordering::Acquire))
    }

    fn activate(
        &self,
        top: HWND,
        focus: Option<HWND>,
        capture_generation: u64,
    ) -> Result<TargetToken, WindowsPasteError> {
        let (process_id, thread_id) = window_process_thread(top)?;
        if let Some(focus) = focus
            && window_process_thread(focus)? != (process_id, thread_id)
        {
            return Err(WindowsPasteError::LifecycleProof);
        }
        let confirmed_generation = self.capture_generation()?;
        if confirmed_generation != capture_generation {
            return Err(WindowsPasteError::LifecycleProof);
        }
        let token = NEXT_LIFECYCLE_TOKEN.fetch_add(1, Ordering::Relaxed).max(1);
        self.state
            .activate(token, top, focus, process_id, thread_id, capture_generation)?;
        lifecycle_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                token,
                LifecycleEntry {
                    state: Arc::clone(&self.state),
                    capture_generation,
                    observer_thread_id: self.observer_thread_id,
                    barrier: Arc::clone(&self.barrier),
                },
            );
        Ok(TargetToken::from_platform_value(token))
    }

    fn stop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = unsafe {
                PostThreadMessageW(
                    self.observer_thread_id,
                    LIFECYCLE_STOP_MESSAGE,
                    Default::default(),
                    Default::default(),
                )
            };
            let _ = thread.join();
        }
    }
}

impl Drop for Win32LifecycleObserver {
    fn drop(&mut self) {
        self.stop();
        lock_unpoisoned(lifecycle_registry())
            .retain(|_, entry| !Arc::ptr_eq(&entry.state, &self.state));
        lock_unpoisoned(&self.state.tokens).clear();
    }
}

struct OwnedWinEventHook(HWINEVENTHOOK);

impl Drop for OwnedWinEventHook {
    fn drop(&mut self) {
        let _ = unsafe { UnhookWinEvent(self.0) };
    }
}

fn lifecycle_observer_thread(
    state: Arc<LifecycleState>,
    ready: mpsc::SyncSender<Result<u32, WindowsPasteError>>,
    barrier_requests: Receiver<u64>,
    barrier_acks: Sender<u64>,
) {
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    THREAD_LIFECYCLE_STATE.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&state)));
    let hook = unsafe {
        SetWinEventHook(
            EVENT_OBJECT_DESTROY,
            EVENT_OBJECT_DESTROY,
            None,
            Some(lifecycle_event_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        )
    };
    if hook.0.is_null() {
        THREAD_LIFECYCLE_STATE.with(|slot| slot.borrow_mut().take());
        let _ = ready.send(Err(WindowsPasteError::LifecycleProof));
        return;
    }
    let _hook = OwnedWinEventHook(hook);
    state.alive.store(true, Ordering::Release);
    if ready.send(Ok(unsafe { GetCurrentThreadId() })).is_err() {
        state.stop.store(true, Ordering::Release);
    } else {
        while !state.stop.load(Ordering::Acquire) {
            let mut processed = false;
            while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                processed = true;
                if message.message == LIFECYCLE_STOP_MESSAGE {
                    state.stop.store(true, Ordering::Release);
                    break;
                }
                if message.message == LIFECYCLE_SYNC_MESSAGE {
                    if !acknowledge_pending_barriers(&barrier_requests, &barrier_acks) {
                        state.stop.store(true, Ordering::Release);
                        break;
                    }
                } else {
                    unsafe { DispatchMessageW(&message) };
                }
            }
            if !processed {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    state.alive.store(false, Ordering::Release);
    for token in lock_unpoisoned(&state.tokens).values_mut() {
        token.valid = false;
    }
    THREAD_LIFECYCLE_STATE.with(|slot| slot.borrow_mut().take());
}

unsafe extern "system" fn lifecycle_event_callback(
    _hook: HWINEVENTHOOK,
    event: u32,
    window: HWND,
    object_id: i32,
    child_id: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    if event != EVENT_OBJECT_DESTROY
        || object_id != OBJID_WINDOW.0
        || child_id != CHILDID_SELF as i32
    {
        return;
    }
    THREAD_LIFECYCLE_STATE.with(|slot| {
        let state = slot.borrow();
        let Some(state) = state.as_ref() else {
            return;
        };
        state.destroy_generation.fetch_add(1, Ordering::AcqRel);
        for token in lock_unpoisoned(&state.tokens).values_mut() {
            if token.top == window.0 as isize || token.focus == window.0 as isize {
                token.valid = false;
            }
        }
    });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObservedCapture<W, T> {
    top: W,
    focus: Option<W>,
    value: T,
}

trait LifecycleObserver<W: Copy> {
    fn begin_capture(&self) -> Result<u64, WindowsPasteError>;
    fn verify_capture_boundary(&self, generation: u64) -> Result<(), WindowsPasteError>;
    fn activate(
        &self,
        top: W,
        focus: Option<W>,
        generation: u64,
    ) -> Result<TargetToken, WindowsPasteError>;
}

impl LifecycleObserver<HWND> for Win32LifecycleObserver {
    fn begin_capture(&self) -> Result<u64, WindowsPasteError> {
        self.capture_generation()
    }

    fn verify_capture_boundary(&self, generation: u64) -> Result<(), WindowsPasteError> {
        if self.capture_generation()? == generation {
            Ok(())
        } else {
            Err(WindowsPasteError::LifecycleProof)
        }
    }

    fn activate(
        &self,
        top: HWND,
        focus: Option<HWND>,
        generation: u64,
    ) -> Result<TargetToken, WindowsPasteError> {
        self.activate(top, focus, generation)
    }
}

fn capture_with_observer<W, T>(
    observer: &impl LifecycleObserver<W>,
    mut capture: impl FnMut() -> Result<ObservedCapture<W, T>, WindowsPasteError>,
) -> Result<(TargetToken, ObservedCapture<W, T>), WindowsPasteError>
where
    W: Copy + Eq,
    T: PartialEq,
{
    let generation = observer.begin_capture()?;
    let initial = capture()?;
    observer.verify_capture_boundary(generation)?;
    let captured = capture()?;
    if captured.top != initial.top
        || captured.focus != initial.focus
        || captured.value != initial.value
    {
        return Err(WindowsPasteError::LifecycleProof);
    }
    let token = observer.activate(captured.top, captured.focus, generation)?;
    Ok((token, captured))
}

struct LifecycleBarrierClient {
    requests: Sender<u64>,
    acknowledgements: Mutex<Receiver<u64>>,
}

impl LifecycleBarrierClient {
    fn request(&self, observer_thread_id: u32) -> Result<(), WindowsPasteError> {
        self.request_with_wake(|| unsafe {
            PostThreadMessageW(
                observer_thread_id,
                LIFECYCLE_SYNC_MESSAGE,
                Default::default(),
                Default::default(),
            )
            .map_err(|_| WindowsPasteError::LifecycleProof)
        })
    }

    fn request_with_wake(
        &self,
        wake: impl FnOnce() -> Result<(), WindowsPasteError>,
    ) -> Result<(), WindowsPasteError> {
        let acknowledgements = self
            .acknowledgements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let nonce = NEXT_LIFECYCLE_BARRIER_NONCE.fetch_add(1, Ordering::Relaxed);
        self.requests
            .send(nonce)
            .map_err(|_| WindowsPasteError::LifecycleProof)?;
        wake()?;
        let deadline = Instant::now() + LIFECYCLE_SYNC_TIMEOUT;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(WindowsPasteError::LifecycleProof);
            };
            match acknowledgements.recv_timeout(remaining) {
                Ok(received) if received == nonce => return Ok(()),
                Ok(_) => continue,
                Err(_) => return Err(WindowsPasteError::LifecycleProof),
            }
        }
    }
}

fn acknowledge_pending_barriers(requests: &Receiver<u64>, acknowledgements: &Sender<u64>) -> bool {
    while let Ok(nonce) = requests.try_recv() {
        if acknowledgements.send(nonce).is_err() {
            return false;
        }
    }
    true
}

fn retain_validated_capture<T>(
    captured: T,
    token: TargetToken,
    validate: impl FnOnce(&T) -> Result<bool, WindowsPasteError>,
    release: impl FnOnce(TargetToken),
) -> Result<T, WindowsPasteError> {
    match validate(&captured) {
        Ok(true) => Ok(captured),
        Ok(false) => {
            release(token);
            Err(WindowsPasteError::LifecycleProof)
        }
        Err(error) => {
            release(token);
            Err(error)
        }
    }
}

fn lifecycle_registry() -> &'static Mutex<HashMap<usize, LifecycleEntry>> {
    LIFECYCLE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lifecycle_valid(target: &TargetSnapshot<WindowsWindow>) -> Result<bool, WindowsPasteError> {
    let focus = target.focused_control.unwrap_or(target.window);
    let registry = lifecycle_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(entry) = registry.get(&target.lifecycle_token.platform_value()) else {
        return Ok(false);
    };
    if entry.barrier.request(entry.observer_thread_id).is_err() {
        if let Some(token) =
            lock_unpoisoned(&entry.state.tokens).get_mut(&target.lifecycle_token.platform_value())
        {
            token.valid = false;
        }
        return Ok(false);
    }
    if !entry.state.valid_for(
        target.lifecycle_token.platform_value(),
        target.window.hwnd(),
        focus.hwnd(),
        target.identity.process_id,
        target.identity.thread_id,
        entry.capture_generation,
    ) {
        return Ok(false);
    }
    match (target.focused_control, target.focused_control_instance_id) {
        (None, None) => Ok(true),
        (Some(control), Some(instance)) => Ok(window_instance_id(control.hwnd())? == instance),
        _ => Ok(false),
    }
}

fn release_lifecycle(token: TargetToken) {
    let entry = lifecycle_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&token.platform_value());
    if let Some(entry) = entry {
        lock_unpoisoned(&entry.state.tokens).remove(&token.platform_value());
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn foreground_root() -> Option<HWND> {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return None;
    }
    let root = unsafe { GetAncestor(foreground, GA_ROOT) };
    Some(if root.0.is_null() { foreground } else { root })
}

fn inspect_window(window: HWND) -> Result<TargetIdentity, WindowsPasteError> {
    if !unsafe { IsWindow(Some(window)) }.as_bool() {
        return Err(WindowsPasteError::InvalidWindow);
    }
    let (process_id, thread_id) = window_process_thread(window)?;
    let process = OwnedHandle::open_process(process_id)?;
    let process_started_at = process.creation_time()?;
    let security = process.security()?;
    Ok(TargetIdentity {
        window_instance_id: window_instance_id(window)?,
        process_id,
        thread_id,
        process_started_at,
        session_id: process_session(process_id)?,
        desktop_id: thread_desktop_identity(thread_id)?,
        integrity_level: security.integrity_level,
        restricted: security.restricted,
        app_container: security.app_container,
    })
}

fn window_process_thread(window: HWND) -> Result<(u32, u32), WindowsPasteError> {
    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(window, Some(&mut process_id)) };
    if thread_id == 0 || process_id == 0 {
        Err(WindowsPasteError::WindowIdentity)
    } else {
        Ok((process_id, thread_id))
    }
}

fn window_instance_id(window: HWND) -> Result<u64, WindowsPasteError> {
    if !unsafe { IsWindow(Some(window)) }.as_bool() {
        return Err(WindowsPasteError::InvalidWindow);
    }
    let mut hasher = DefaultHasher::new();
    (window.0 as usize).hash(&mut hasher);
    unsafe { SetLastError(ERROR_SUCCESS) };
    let class_atom = unsafe { GetClassLongPtrW(window, GCW_ATOM) };
    if class_atom == 0 && unsafe { GetLastError() } != ERROR_SUCCESS {
        return Err(WindowsPasteError::WindowIdentity);
    }
    class_atom.hash(&mut hasher);
    for field in [GWLP_WNDPROC, GWLP_HINSTANCE, GWLP_USERDATA] {
        unsafe { SetLastError(ERROR_SUCCESS) };
        let value = unsafe { GetWindowLongPtrW(window, field) };
        if value == 0 && unsafe { GetLastError() } != ERROR_SUCCESS {
            return Err(WindowsPasteError::WindowIdentity);
        }
        value.hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn focused_control(thread_id: u32) -> Result<Option<HWND>, WindowsPasteError> {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..unsafe { zeroed() }
    };
    unsafe { GetGUIThreadInfo(thread_id, &mut info) }
        .map_err(|_| WindowsPasteError::LifecycleProof)?;
    Ok((!info.hwndFocus.0.is_null()).then_some(info.hwndFocus))
}

fn restore_focus_control(
    target: &TargetSnapshot<WindowsWindow>,
    control: WindowsWindow,
    expected_instance_id: Option<u64>,
    process_id: u32,
    thread_id: u32,
) -> Result<(), WindowsPasteError> {
    let control = control.hwnd();
    let Some(expected_instance_id) = expected_instance_id else {
        return Err(WindowsPasteError::LifecycleProof);
    };
    if !unsafe { IsWindow(Some(control)) }.as_bool()
        || window_process_thread(control)? != (process_id, thread_id)
        || !lifecycle_valid(target)?
        || window_instance_id(control)? != expected_instance_id
    {
        return Err(WindowsPasteError::FocusRestore);
    }
    let current_thread = unsafe { GetCurrentThreadId() };
    let attached = current_thread != thread_id
        && unsafe { AttachThreadInput(current_thread, thread_id, true) }.as_bool();
    if current_thread != thread_id && !attached {
        return Err(WindowsPasteError::FocusRestore);
    }
    if !lifecycle_valid(target)? || window_instance_id(control)? != expected_instance_id {
        if attached {
            let _ = unsafe { AttachThreadInput(current_thread, thread_id, false) };
        }
        return Err(WindowsPasteError::LifecycleProof);
    }
    let _ = unsafe { SetFocus(Some(control)) };
    let detached =
        !attached || unsafe { AttachThreadInput(current_thread, thread_id, false) }.as_bool();
    if !detached {
        return Err(WindowsPasteError::FocusRestore);
    }
    if lifecycle_valid(target)?
        && window_instance_id(control)? == expected_instance_id
        && focused_control(thread_id)? == Some(control)
    {
        Ok(())
    } else {
        Err(WindowsPasteError::FocusRestore)
    }
}

#[derive(Clone, Copy)]
struct ProcessSecurity {
    integrity_level: u32,
    restricted: bool,
    app_container: bool,
}

fn current_process_security() -> Result<ProcessSecurity, WindowsPasteError> {
    token_security(unsafe { GetCurrentProcess() })
}

fn token_security(process: HANDLE) -> Result<ProcessSecurity, WindowsPasteError> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
        .map_err(|_| WindowsPasteError::ProcessToken)?;
    let token = OwnedHandle(token);
    Ok(ProcessSecurity {
        integrity_level: token_integrity(token.0)?,
        restricted: token_restricted(token.0)?,
        app_container: token_u32(token.0, TokenIsAppContainer)? != 0,
    })
}

fn token_integrity(token: HANDLE) -> Result<u32, WindowsPasteError> {
    let buffer = token_information(token, TokenIntegrityLevel)?;
    let label = unsafe { &*(buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()) };
    let sid = label.Label.Sid;
    if sid.0.is_null() {
        return Err(WindowsPasteError::TokenInformation);
    }
    let count = unsafe { *GetSidSubAuthorityCount(sid) } as u32;
    if count == 0 {
        return Err(WindowsPasteError::TokenInformation);
    }
    Ok(unsafe { *GetSidSubAuthority(sid, count - 1) })
}

fn token_u32(
    token: HANDLE,
    class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<u32, WindowsPasteError> {
    let mut value = 0u32;
    let mut returned = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some((&mut value as *mut u32).cast()),
            size_of::<u32>() as u32,
            &mut returned,
        )
    }
    .map_err(|_| WindowsPasteError::TokenInformation)?;
    if returned < size_of::<u32>() as u32 {
        Err(WindowsPasteError::TokenInformation)
    } else {
        Ok(value)
    }
}

fn token_restricted(token: HANDLE) -> Result<bool, WindowsPasteError> {
    unsafe { SetLastError(ERROR_SUCCESS) };
    let restricted = unsafe { IsTokenRestricted(token) };
    classify_token_restrictions(restricted.is_ok(), unsafe { GetLastError() })
}

fn classify_token_restrictions(
    restricted: bool,
    last_error: WIN32_ERROR,
) -> Result<bool, WindowsPasteError> {
    if restricted {
        Ok(true)
    } else if last_error == ERROR_SUCCESS {
        Ok(false)
    } else {
        Err(WindowsPasteError::TokenInformation)
    }
}

fn token_information(
    token: HANDLE,
    class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<Vec<u8>, WindowsPasteError> {
    let mut needed = 0;
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &mut needed) };
    if needed == 0 {
        return Err(WindowsPasteError::TokenInformation);
    }
    let mut buffer = vec![0u8; needed as usize];
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|_| WindowsPasteError::TokenInformation)?;
    Ok(buffer)
}

fn process_session(process_id: u32) -> Result<u32, WindowsPasteError> {
    let mut session_id = 0;
    unsafe { ProcessIdToSessionId(process_id, &mut session_id) }
        .map_err(|_| WindowsPasteError::SessionInformation)?;
    Ok(session_id)
}

fn thread_desktop_identity(thread_id: u32) -> Result<u64, WindowsPasteError> {
    let desktop = unsafe { GetThreadDesktop(thread_id) }
        .map_err(|_| WindowsPasteError::DesktopInformation)?;
    desktop_identity(HANDLE(desktop.0))
}

fn input_desktop_identity() -> Result<u64, WindowsPasteError> {
    let desktop = unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) }
        .map_err(|_| WindowsPasteError::DesktopInformation)?;
    let identity = desktop_identity(HANDLE(desktop.0));
    let closed = unsafe { CloseDesktop(desktop) };
    match (identity, closed) {
        (Ok(identity), Ok(())) => Ok(identity),
        (Err(error), _) => Err(error),
        (Ok(_), Err(_)) => Err(WindowsPasteError::DesktopInformation),
    }
}

fn desktop_identity(desktop: HANDLE) -> Result<u64, WindowsPasteError> {
    let mut needed = 0;
    let _ = unsafe { GetUserObjectInformationW(desktop, UOI_NAME, None, 0, Some(&mut needed)) };
    if needed < size_of::<u16>() as u32 {
        return Err(WindowsPasteError::DesktopInformation);
    }
    let mut name = vec![0u16; needed as usize / size_of::<u16>()];
    unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            needed,
            Some(&mut needed),
        )
    }
    .map_err(|_| WindowsPasteError::DesktopInformation)?;
    let length = name
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(name.len());
    let mut hasher = DefaultHasher::new();
    name[..length].hash(&mut hasher);
    Ok(hasher.finish())
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn open_process(process_id: u32) -> Result<Self, WindowsPasteError> {
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
            .map(Self)
            .map_err(|_| WindowsPasteError::ProcessOpen)
    }

    fn creation_time(&self) -> Result<u64, WindowsPasteError> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }
            .map_err(|_| WindowsPasteError::ProcessTimes)?;
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    fn security(&self) -> Result<ProcessSecurity, WindowsPasteError> {
        token_security(self.0)
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Win32PasteInput;

impl PasteInput<WindowsWindow> for Win32PasteInput {
    type Error = WindowsPasteError;

    fn send_ctrl_v(&self, target: &TargetSnapshot<WindowsWindow>) -> Result<(), Self::Error> {
        send_ctrl_v(&Win32InputApi, target)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasteKey {
    Control,
    V,
}

trait InputApi {
    type Window: Copy + Eq;

    fn foreground(&self) -> Self::Window;
    fn target_valid(&self, target: &TargetSnapshot<Self::Window>) -> bool;
    fn focused_window(&self, thread_id: u32) -> Option<Self::Window>;
    fn key_down(&self, key: PasteKey) -> bool;
    fn send(&self, key: PasteKey, key_up: bool) -> bool;
}

fn send_ctrl_v<A: InputApi>(
    api: &A,
    target: &TargetSnapshot<A::Window>,
) -> Result<(), WindowsPasteError> {
    let expected = target.window;
    if !target_matches(api, target, expected) {
        return Err(WindowsPasteError::ForegroundChanged);
    }
    if api.key_down(PasteKey::Control) || api.key_down(PasteKey::V) {
        return Err(WindowsPasteError::PhysicalPasteKeyDown);
    }
    let mut pressed = Vec::with_capacity(2);
    for key in [PasteKey::Control, PasteKey::V] {
        if !target_matches(api, target, expected) {
            return release_pressed(api, &mut pressed, WindowsPasteError::ForegroundChanged);
        }
        if !api.send(key, false) {
            return release_pressed(api, &mut pressed, WindowsPasteError::InputDispatch);
        }
        pressed.push(key);
        if !target_matches(api, target, expected) {
            return release_pressed(api, &mut pressed, WindowsPasteError::ForegroundChanged);
        }
    }
    for key in [PasteKey::V, PasteKey::Control] {
        if !target_matches(api, target, expected) {
            return release_pressed(api, &mut pressed, WindowsPasteError::ForegroundChanged);
        }
        if !api.send(key, true) {
            return release_pressed(api, &mut pressed, WindowsPasteError::InputDispatch);
        }
        pressed.pop();
        if !target_matches(api, target, expected) {
            return release_pressed(api, &mut pressed, WindowsPasteError::ForegroundChanged);
        }
    }
    Ok(())
}

fn target_matches<A: InputApi>(
    api: &A,
    target: &TargetSnapshot<A::Window>,
    expected: A::Window,
) -> bool {
    api.foreground() == expected
        && api.target_valid(target)
        && api.focused_window(target.identity.thread_id)
            == Some(target.focused_control.unwrap_or(target.window))
}

fn release_pressed<A: InputApi>(
    api: &A,
    pressed: &mut Vec<PasteKey>,
    operation: WindowsPasteError,
) -> Result<(), WindowsPasteError> {
    let mut cleanup_failed = false;
    while let Some(key) = pressed.pop() {
        cleanup_failed |= !api.send(key, true);
    }
    if cleanup_failed {
        Err(WindowsPasteError::InputCleanup)
    } else {
        Err(operation)
    }
}

struct Win32InputApi;

impl InputApi for Win32InputApi {
    type Window = WindowsWindow;

    fn foreground(&self) -> Self::Window {
        let foreground = unsafe { GetForegroundWindow() };
        let root = unsafe { GetAncestor(foreground, GA_ROOT) };
        WindowsWindow::from_hwnd(if root.0.is_null() { foreground } else { root })
    }

    fn key_down(&self, key: PasteKey) -> bool {
        (unsafe { GetAsyncKeyState(virtual_key(key).0.into()) }) < 0
    }

    fn target_valid(&self, target: &TargetSnapshot<Self::Window>) -> bool {
        inspect_window(target.window.hwnd()).ok() == Some(target.identity)
            && lifecycle_valid(target).ok() == Some(true)
    }

    fn focused_window(&self, thread_id: u32) -> Option<Self::Window> {
        focused_control(thread_id)
            .ok()
            .flatten()
            .map(WindowsWindow::from_hwnd)
    }

    fn send(&self, key: PasteKey, key_up: bool) -> bool {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: virtual_key(key),
                    dwFlags: if key_up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    ..Default::default()
                },
            },
        };
        (unsafe { SendInput(&[input], size_of::<INPUT>() as i32) }) == 1
    }
}

fn virtual_key(key: PasteKey) -> VIRTUAL_KEY {
    match key {
        PasteKey::Control => VK_CONTROL,
        PasteKey::V => VK_V,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use super::*;

    #[test]
    fn capture_starts_with_an_active_observer_before_any_authoritative_read() {
        let observer = FakeLifecycleObserver::new();
        let mut captures = 0;

        let result = capture_with_observer(&observer, || {
            observer.order.lock().unwrap().push("capture");
            captures += 1;
            Ok(observed_capture(7, 8, ()))
        })
        .unwrap();

        assert!(observer.is_valid(&result.0));
        assert_eq!(captures, 2);
        assert_eq!(
            *observer.order.lock().unwrap(),
            [
                "begin", "capture", "verify", "capture", "activate", "verify"
            ]
        );
    }

    #[test]
    fn lifecycle_observer_rejects_destroyed_or_reused_windows_and_cleans_up_tokens() {
        let observer = FakeLifecycleObserver::new();
        let (token, _) =
            capture_with_observer(&observer, || Ok(observed_capture(7, 8, ()))).unwrap();
        assert!(observer.is_valid(&token));
        assert_eq!(observer.tracked_count(), 1);

        observer.destroy(8);
        assert!(!observer.is_valid(&token));

        observer.release(token);
        assert!(!observer.is_valid(&token));
        assert_eq!(observer.tracked_count(), 0);
    }

    #[test]
    fn unrelated_destroy_does_not_invalidate_an_active_target_token() {
        let observer = FakeLifecycleObserver::new();
        let (token, _) =
            capture_with_observer(&observer, || Ok(observed_capture(7, 8, ()))).unwrap();

        observer.destroy(99);

        assert!(observer.is_valid(&token));
    }

    #[test]
    fn lifecycle_hook_failure_is_conservative() {
        let observer = FakeLifecycleObserver::new();
        *observer.install_ok.lock().unwrap() = false;
        let captures = AtomicUsize::new(0);

        let result = capture_with_observer(&observer, || {
            captures.fetch_add(1, Ordering::Relaxed);
            Ok(observed_capture(7, 8, ()))
        });

        assert!(matches!(result, Err(WindowsPasteError::LifecycleProof)));
        assert_eq!(captures.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn two_phase_capture_rejects_complete_fingerprint_change() {
        let observer = FakeLifecycleObserver::new();
        let mut first = true;

        let result = capture_with_observer(&observer, || {
            let value = if first { 41 } else { 42 };
            first = false;
            Ok(observed_capture(7, 8, value))
        });

        assert!(matches!(result, Err(WindowsPasteError::LifecycleProof)));
        assert!(observer.states.lock().unwrap().is_empty());
    }

    #[test]
    fn identical_destroy_and_reuse_before_first_authoritative_read_is_rejected() {
        let observer = FakeLifecycleObserver::new();
        let mut first = true;

        let result = capture_with_observer(&observer, || {
            if first {
                first = false;
                observer.destroy(7);
            }
            Ok(observed_capture(7, 8, 41u64))
        });

        assert!(matches!(result, Err(WindowsPasteError::LifecycleProof)));
        assert!(observer.states.lock().unwrap().is_empty());
    }

    #[test]
    fn two_phase_capture_rejects_focused_child_change() {
        let observer = FakeLifecycleObserver::new();
        let mut first = true;

        let result = capture_with_observer(&observer, || {
            let focus = if first { 8 } else { 9 };
            first = false;
            Ok(observed_capture(7, focus, 41u64))
        });

        assert!(matches!(result, Err(WindowsPasteError::LifecycleProof)));
        assert!(observer.states.lock().unwrap().is_empty());
    }

    #[test]
    fn observer_exit_invalidates_existing_tokens_and_future_capture() {
        let observer = FakeLifecycleObserver::new();
        let (token, _) =
            capture_with_observer(&observer, || Ok(observed_capture(7, 8, ()))).unwrap();

        observer.exit();

        assert!(!observer.is_valid(&token));
        assert!(matches!(
            capture_with_observer(&observer, || Ok(observed_capture(7, 8, ()))),
            Err(WindowsPasteError::LifecycleProof)
        ));
    }

    #[test]
    fn final_capture_validation_error_releases_lifecycle_token() {
        let released = Mutex::new(Vec::new());
        let token = TargetToken::from_platform_value(41);

        assert_eq!(
            retain_validated_capture(
                7,
                token,
                |_| Err(WindowsPasteError::WindowIdentity),
                |token| released.lock().unwrap().push(token.platform_value()),
            ),
            Err(WindowsPasteError::WindowIdentity)
        );
        assert_eq!(*released.lock().unwrap(), [41]);
    }

    #[test]
    fn spoofed_wake_without_internal_request_cannot_create_an_acknowledgement() {
        let (requests, request_receiver) = mpsc::channel();
        let (acknowledgements, acknowledgement_receiver) = mpsc::channel();
        drop(requests);

        assert!(acknowledge_pending_barriers(
            &request_receiver,
            &acknowledgements
        ));
        assert!(acknowledgement_receiver.try_recv().is_err());
    }

    #[test]
    fn stale_barrier_ack_is_discarded_until_the_matching_nonce_arrives() {
        let (requests, request_receiver) = mpsc::channel();
        let (acknowledgements, acknowledgement_receiver) = mpsc::channel();
        let barrier = LifecycleBarrierClient {
            requests,
            acknowledgements: Mutex::new(acknowledgement_receiver),
        };
        acknowledgements.send(u64::MAX).unwrap();

        barrier
            .request_with_wake(|| {
                assert!(acknowledge_pending_barriers(
                    &request_receiver,
                    &acknowledgements
                ));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn concurrent_barriers_each_require_their_own_internal_nonce_ack() {
        let (requests, request_receiver) = mpsc::channel();
        let (acknowledgements, acknowledgement_receiver) = mpsc::channel();
        let barrier = Arc::new(LifecycleBarrierClient {
            requests,
            acknowledgements: Mutex::new(acknowledgement_receiver),
        });
        let observer = std::thread::spawn(move || {
            for _ in 0..2 {
                let nonce = request_receiver.recv().unwrap();
                acknowledgements.send(nonce).unwrap();
            }
        });
        let mut callers = Vec::new();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            callers.push(std::thread::spawn(move || {
                barrier.request_with_wake(|| Ok(()))
            }));
        }

        for caller in callers {
            caller.join().unwrap().unwrap();
        }
        observer.join().unwrap();
    }

    fn observed_capture<T>(top: i32, focus: i32, value: T) -> ObservedCapture<i32, T> {
        ObservedCapture {
            top,
            focus: Some(focus),
            value,
        }
    }

    #[test]
    fn input_state_machine_sends_balanced_ctrl_v() {
        let api = FakeInputApi::new([true, true, true, true]);

        send_ctrl_v(&api, &fake_target()).unwrap();

        assert_eq!(
            *api.events.lock().unwrap(),
            [
                (PasteKey::Control, false),
                (PasteKey::V, false),
                (PasteKey::V, true),
                (PasteKey::Control, true),
            ]
        );
    }

    #[test]
    fn partial_send_releases_only_keys_pressed_by_this_operation() {
        let api = FakeInputApi::new([true, false, true]);

        assert_eq!(
            send_ctrl_v(&api, &fake_target()),
            Err(WindowsPasteError::InputDispatch)
        );
        assert_eq!(
            *api.events.lock().unwrap(),
            [
                (PasteKey::Control, false),
                (PasteKey::V, false),
                (PasteKey::Control, true),
            ]
        );
    }

    #[test]
    fn cleanup_failure_is_reported_after_partial_send() {
        let api = FakeInputApi::new([true, false, false]);

        assert_eq!(
            send_ctrl_v(&api, &fake_target()),
            Err(WindowsPasteError::InputCleanup)
        );
    }

    #[test]
    fn physical_ctrl_or_v_prevents_any_injection() {
        for held in [[true, false], [false, true]] {
            let api = FakeInputApi::new([]);
            *api.held.lock().unwrap() = held;

            assert_eq!(
                send_ctrl_v(&api, &fake_target()),
                Err(WindowsPasteError::PhysicalPasteKeyDown)
            );
            assert!(api.events.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn foreground_change_stops_new_key_down_and_releases_owned_key() {
        let api = FakeInputApi::new([true, true]);
        *api.foregrounds.lock().unwrap() = VecDeque::from([7, 7, 8]);

        assert_eq!(
            send_ctrl_v(&api, &fake_target()),
            Err(WindowsPasteError::ForegroundChanged)
        );
        assert_eq!(
            *api.events.lock().unwrap(),
            [(PasteKey::Control, false), (PasteKey::Control, true)]
        );
    }

    #[test]
    fn top_level_reuse_before_first_key_down_sends_no_input() {
        let api = FakeInputApi::new([]);
        *api.validity.lock().unwrap() = VecDeque::from([true, false]);

        assert_eq!(
            send_ctrl_v(&api, &fake_target()),
            Err(WindowsPasteError::ForegroundChanged)
        );
        assert!(api.events.lock().unwrap().is_empty());
    }

    #[test]
    fn focused_child_reuse_before_first_key_down_sends_no_input() {
        let api = FakeInputApi::new([]);
        *api.validity.lock().unwrap() = VecDeque::from([true, false]);

        assert_eq!(
            send_ctrl_v(&api, &fake_target()),
            Err(WindowsPasteError::ForegroundChanged)
        );
        assert!(api.events.lock().unwrap().is_empty());
    }

    #[test]
    fn lifecycle_change_before_later_key_down_releases_owned_control() {
        let api = FakeInputApi::new([true, true]);
        *api.validity.lock().unwrap() = VecDeque::from([true, true, true, false]);

        assert_eq!(
            send_ctrl_v(&api, &fake_target()),
            Err(WindowsPasteError::ForegroundChanged)
        );
        assert_eq!(
            *api.events.lock().unwrap(),
            [(PasteKey::Control, false), (PasteKey::Control, true)]
        );
    }

    #[test]
    fn focus_change_before_first_key_down_sends_no_input() {
        let api = FakeInputApi::new([]);
        *api.focuses.lock().unwrap() = VecDeque::from([Some(8), Some(9)]);

        assert_eq!(
            send_ctrl_v(&api, &fake_target()),
            Err(WindowsPasteError::ForegroundChanged)
        );
        assert!(api.events.lock().unwrap().is_empty());
    }

    #[test]
    fn focus_change_before_later_key_down_releases_owned_control() {
        let api = FakeInputApi::new([true, true]);
        *api.focuses.lock().unwrap() = VecDeque::from([Some(8), Some(8), Some(8), Some(9)]);

        assert_eq!(
            send_ctrl_v(&api, &fake_target()),
            Err(WindowsPasteError::ForegroundChanged)
        );
        assert_eq!(
            *api.events.lock().unwrap(),
            [(PasteKey::Control, false), (PasteKey::Control, true)]
        );
    }

    #[test]
    fn top_level_focus_is_required_when_capture_has_no_independent_child() {
        let api = FakeInputApi::new([]);
        let target = fake_top_level_focus_target();
        *api.focuses.lock().unwrap() = VecDeque::from([Some(7), None]);

        assert_eq!(
            send_ctrl_v(&api, &target),
            Err(WindowsPasteError::ForegroundChanged)
        );
        assert!(api.events.lock().unwrap().is_empty());
    }

    #[test]
    fn token_restriction_query_distinguishes_all_three_results() {
        assert_eq!(classify_token_restrictions(false, ERROR_SUCCESS), Ok(false));
        assert_eq!(classify_token_restrictions(true, ERROR_SUCCESS), Ok(true));
        assert_eq!(
            classify_token_restrictions(false, WIN32_ERROR(5)),
            Err(WindowsPasteError::TokenInformation)
        );
    }

    #[test]
    fn production_input_source_contains_no_alt_activation_path() {
        let source = include_str!("paste.rs");
        let forbidden = ["VK_", "MENU"].concat();

        assert!(!source.contains(&forbidden));
    }

    #[test]
    fn production_window_identity_excludes_mutable_window_geometry() {
        let source = include_str!("paste.rs");
        let forbidden = ["GetWindow", "Info"].concat();

        assert!(!source.contains(&forbidden));
    }

    #[test]
    fn non_destructive_windows_identity_harness_queries_current_desktop_and_token() {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .expect("open current process token");
        let token = OwnedHandle(token);
        assert!(token_integrity(token.0).expect("query current integrity") > 0);
        let _ = token_restricted(token.0).expect("query current token restrictions");
        let _ = token_u32(token.0, TokenIsAppContainer).expect("query current app container");
        assert!(input_desktop_identity().is_ok());
    }

    #[test]
    #[ignore = "sends Ctrl+V to the foreground desktop; set CLIPBOARD_ASSISTANT_RUN_PASTE_TESTS=1 and run explicitly"]
    fn destructive_send_input_harness_is_environment_gated() {
        assert_eq!(
            std::env::var("CLIPBOARD_ASSISTANT_RUN_PASTE_TESTS").as_deref(),
            Ok("1")
        );
        let target = Win32PasteTarget::new().capture().unwrap();
        Win32PasteInput.send_ctrl_v(&target).unwrap();
    }

    struct FakeInputApi {
        foregrounds: Mutex<VecDeque<i32>>,
        held: Mutex<[bool; 2]>,
        results: Mutex<VecDeque<bool>>,
        validity: Mutex<VecDeque<bool>>,
        focuses: Mutex<VecDeque<Option<i32>>>,
        events: Mutex<Vec<(PasteKey, bool)>>,
    }

    impl FakeInputApi {
        fn new(results: impl IntoIterator<Item = bool>) -> Self {
            Self {
                foregrounds: Mutex::new(VecDeque::from([7])),
                held: Mutex::new([false, false]),
                results: Mutex::new(results.into_iter().collect()),
                validity: Mutex::new(VecDeque::from([true])),
                focuses: Mutex::new(VecDeque::from([Some(8)])),
                events: Mutex::new(Vec::new()),
            }
        }
    }

    impl InputApi for FakeInputApi {
        type Window = i32;

        fn foreground(&self) -> Self::Window {
            let mut values = self.foregrounds.lock().unwrap();
            if values.len() > 1 {
                values.pop_front().unwrap()
            } else {
                *values.front().unwrap()
            }
        }

        fn key_down(&self, key: PasteKey) -> bool {
            self.held.lock().unwrap()[match key {
                PasteKey::Control => 0,
                PasteKey::V => 1,
            }]
        }

        fn target_valid(&self, _target: &TargetSnapshot<Self::Window>) -> bool {
            let mut values = self.validity.lock().unwrap();
            if values.len() > 1 {
                values.pop_front().unwrap()
            } else {
                *values.front().unwrap()
            }
        }

        fn focused_window(&self, _thread_id: u32) -> Option<Self::Window> {
            let mut values = self.focuses.lock().unwrap();
            if values.len() > 1 {
                values.pop_front().unwrap()
            } else {
                *values.front().unwrap()
            }
        }

        fn send(&self, key: PasteKey, key_up: bool) -> bool {
            self.events.lock().unwrap().push((key, key_up));
            self.results.lock().unwrap().pop_front().unwrap_or(true)
        }
    }

    fn fake_target() -> TargetSnapshot<i32> {
        TargetSnapshot {
            window: 7,
            focused_control: Some(8),
            identity: TargetIdentity {
                window_instance_id: 1,
                process_id: 2,
                thread_id: 3,
                process_started_at: 4,
                session_id: 5,
                desktop_id: 6,
                integrity_level: 7,
                restricted: false,
                app_container: false,
            },
            lifecycle_token: TargetToken::from_platform_value(10),
            focused_control_instance_id: Some(11),
        }
    }

    fn fake_top_level_focus_target() -> TargetSnapshot<i32> {
        TargetSnapshot {
            focused_control: None,
            focused_control_instance_id: None,
            ..fake_target()
        }
    }

    struct FakeLifecycleState {
        top: i32,
        focus: Option<i32>,
        valid: bool,
    }

    struct FakeLifecycleObserver {
        install_ok: Mutex<bool>,
        alive: AtomicBool,
        generation: AtomicU64,
        order: Mutex<Vec<&'static str>>,
        next_token: AtomicUsize,
        states: Mutex<HashMap<usize, FakeLifecycleState>>,
    }

    impl FakeLifecycleObserver {
        fn new() -> Self {
            Self {
                install_ok: Mutex::new(true),
                alive: AtomicBool::new(true),
                generation: AtomicU64::new(0),
                order: Mutex::new(Vec::new()),
                next_token: AtomicUsize::new(1),
                states: Mutex::new(HashMap::new()),
            }
        }

        fn destroy(&self, window: i32) {
            self.generation.fetch_add(1, Ordering::AcqRel);
            for state in self.states.lock().unwrap().values_mut() {
                if state.top == window || state.focus == Some(window) {
                    state.valid = false;
                }
            }
        }

        fn is_valid(&self, token: &TargetToken) -> bool {
            self.states
                .lock()
                .unwrap()
                .get(&token.platform_value())
                .is_some_and(|state| state.valid && self.alive.load(Ordering::Acquire))
        }

        fn release(&self, token: TargetToken) {
            self.states.lock().unwrap().remove(&token.platform_value());
        }

        fn tracked_count(&self) -> usize {
            self.states.lock().unwrap().len()
        }

        fn exit(&self) {
            self.alive.store(false, Ordering::Release);
            for state in self.states.lock().unwrap().values_mut() {
                state.valid = false;
            }
        }
    }

    impl LifecycleObserver<i32> for FakeLifecycleObserver {
        fn begin_capture(&self) -> Result<u64, WindowsPasteError> {
            self.order.lock().unwrap().push("begin");
            if !*self.install_ok.lock().unwrap() || !self.alive.load(Ordering::Acquire) {
                return Err(WindowsPasteError::LifecycleProof);
            }
            Ok(self.generation.load(Ordering::Acquire))
        }

        fn verify_capture_boundary(&self, generation: u64) -> Result<(), WindowsPasteError> {
            self.order.lock().unwrap().push("verify");
            if !self.alive.load(Ordering::Acquire)
                || self.generation.load(Ordering::Acquire) != generation
            {
                return Err(WindowsPasteError::LifecycleProof);
            }
            Ok(())
        }

        fn activate(
            &self,
            top: i32,
            focus: Option<i32>,
            generation: u64,
        ) -> Result<TargetToken, WindowsPasteError> {
            self.order.lock().unwrap().push("activate");
            self.verify_capture_boundary(generation)?;
            let id = self.next_token.fetch_add(1, Ordering::Relaxed);
            self.states.lock().unwrap().insert(
                id,
                FakeLifecycleState {
                    top,
                    focus,
                    valid: true,
                },
            );
            Ok(TargetToken::from_platform_value(id))
        }
    }
}
