use std::{
    error::Error,
    fmt,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::{ERROR_HOTKEY_ALREADY_REGISTERED, GetLastError, LPARAM, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
            RegisterHotKey, UnregisterHotKey, VK_CONTROL, VK_LCONTROL, VK_LEFT, VK_LMENU,
            VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN,
            VK_SHIFT,
        },
        WindowsAndMessaging::{
            GetMessageW, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW, PostThreadMessageW, WM_HOTKEY,
            WM_QUIT,
        },
    },
};

use crate::domain::{Shortcut, ShortcutKey, ShortcutModifiers};

const HOTKEY_ID: i32 = 0x4341;
const PREVIOUS_GROUP_ID: i32 = 0x4350;
const NEXT_GROUP_ID: i32 = 0x4351;
const QUICK_PASTE_FIRST_ID: i32 = 0x4361;
const READY_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const KEY_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);
const KEY_RELEASE_POLL_INTERVAL: Duration = Duration::from_millis(8);
const GROUP_KEY_POLL_INTERVAL: Duration = Duration::from_millis(8);
const GROUP_KEY_REPEAT_DELAY: Duration = Duration::from_millis(320);
const GROUP_KEY_REPEAT_INTERVAL: Duration = Duration::from_millis(90);

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

pub struct GroupHotkeys {
    registration: Option<GlobalHotkey>,
    running: Arc<AtomicBool>,
}

impl GroupHotkeys {
    pub fn start(
        modifiers: ShortcutModifiers,
        on_switch: impl Fn(i8) + Send + 'static,
    ) -> Result<Self, HotkeyError> {
        let running = Arc::new(AtomicBool::new(true));
        let callback_running = Arc::clone(&running);
        let windows_modifiers = windows_modifiers(modifiers);
        let bindings = vec![
            HotkeyBinding {
                id: PREVIOUS_GROUP_ID,
                modifiers: windows_modifiers,
                key: VK_LEFT.0 as u32,
            },
            HotkeyBinding {
                id: NEXT_GROUP_ID,
                modifiers: windows_modifiers,
                key: VK_RIGHT.0 as u32,
            },
        ];
        let registration = GlobalHotkey::start_bindings(bindings, move |id| {
            track_group_switch_session(
                if id == PREVIOUS_GROUP_ID { -1 } else { 1 },
                modifiers,
                &callback_running,
                &on_switch,
            );
        })?;
        Ok(Self {
            registration: Some(registration),
            running,
        })
    }
}

impl Drop for GroupHotkeys {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        drop(self.registration.take());
    }
}

fn track_group_switch_session(
    initial_direction: i8,
    modifiers: ShortcutModifiers,
    running: &AtomicBool,
    on_switch: &impl Fn(i8),
) {
    on_switch(initial_direction);
    let now = Instant::now();
    let mut left = GroupKeyState::new(initial_direction < 0 && key_down(VK_LEFT.0.into()), now);
    let mut right = GroupKeyState::new(initial_direction > 0 && key_down(VK_RIGHT.0.into()), now);

    while running.load(Ordering::Acquire) && modifiers_down(modifiers) {
        thread::sleep(GROUP_KEY_POLL_INTERVAL);
        let now = Instant::now();
        update_group_key(&mut left, key_down(VK_LEFT.0.into()), now, -1, on_switch);
        update_group_key(&mut right, key_down(VK_RIGHT.0.into()), now, 1, on_switch);
    }
    drain_group_hotkey_messages();
}

fn drain_group_hotkey_messages() {
    let mut message = MSG::default();
    unsafe { while PeekMessageW(&mut message, None, WM_HOTKEY, WM_HOTKEY, PM_REMOVE).as_bool() {} }
}

struct GroupKeyState {
    down: bool,
    next_repeat: Instant,
}

impl GroupKeyState {
    fn new(down: bool, now: Instant) -> Self {
        Self {
            down,
            next_repeat: now + GROUP_KEY_REPEAT_DELAY,
        }
    }
}

fn update_group_key(
    state: &mut GroupKeyState,
    down: bool,
    now: Instant,
    direction: i8,
    on_switch: &impl Fn(i8),
) {
    if !down {
        state.down = false;
        return;
    }
    if !state.down {
        state.down = true;
        state.next_repeat = now + GROUP_KEY_REPEAT_DELAY;
        on_switch(direction);
    } else if now >= state.next_repeat {
        state.next_repeat = now + GROUP_KEY_REPEAT_INTERVAL;
        on_switch(direction);
    }
}

pub struct QuickPasteHotkeys {
    _registration: GlobalHotkey,
}

impl QuickPasteHotkeys {
    pub fn start(
        modifiers: ShortcutModifiers,
        on_paste: impl Fn(usize) + Send + 'static,
    ) -> Result<Self, HotkeyError> {
        let windows_modifiers = windows_modifiers(modifiers);
        let bindings = (0..9)
            .map(|index| HotkeyBinding {
                id: QUICK_PASTE_FIRST_ID + index,
                modifiers: windows_modifiers,
                key: 0x31 + index as u32,
            })
            .collect();
        GlobalHotkey::start_bindings(bindings, move |id| {
            let index = (id - QUICK_PASTE_FIRST_ID) as usize;
            let digit = 0x31 + index as i32;
            if wait_for_quick_paste_keys_released(modifiers, digit) {
                on_paste(index);
            }
        })
        .map(|registration| Self {
            _registration: registration,
        })
    }
}

fn wait_for_quick_paste_keys_released(modifiers: ShortcutModifiers, digit: i32) -> bool {
    wait_until_released(
        KEY_RELEASE_TIMEOUT,
        KEY_RELEASE_POLL_INTERVAL,
        || !any_configured_modifier_down(modifiers) && !key_down(digit),
        thread::sleep,
    )
}

fn wait_until_released(
    timeout: Duration,
    poll_interval: Duration,
    mut released: impl FnMut() -> bool,
    mut sleep: impl FnMut(Duration),
) -> bool {
    let started = std::time::Instant::now();
    loop {
        if released() {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        sleep(poll_interval);
    }
}

fn key_down(key: i32) -> bool {
    (unsafe { GetAsyncKeyState(key) }) < 0
}

fn control_down() -> bool {
    [VK_CONTROL, VK_LCONTROL, VK_RCONTROL]
        .into_iter()
        .any(|key| key_down(key.0.into()))
}

fn alt_down() -> bool {
    [VK_MENU, VK_LMENU, VK_RMENU]
        .into_iter()
        .any(|key| key_down(key.0.into()))
}

fn shift_down() -> bool {
    [VK_SHIFT, VK_LSHIFT, VK_RSHIFT]
        .into_iter()
        .any(|key| key_down(key.0.into()))
}

fn win_down() -> bool {
    [VK_LWIN, VK_RWIN]
        .into_iter()
        .any(|key| key_down(key.0.into()))
}

fn modifiers_down(modifiers: ShortcutModifiers) -> bool {
    (!modifiers.ctrl || control_down())
        && (!modifiers.alt || alt_down())
        && (!modifiers.shift || shift_down())
        && (!modifiers.win || win_down())
}

fn any_configured_modifier_down(modifiers: ShortcutModifiers) -> bool {
    (modifiers.ctrl && control_down())
        || (modifiers.alt && alt_down())
        || (modifiers.shift && shift_down())
        || (modifiers.win && win_down())
}

fn windows_modifiers(
    modifiers: ShortcutModifiers,
) -> windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS {
    let mut value = MOD_NOREPEAT;
    if modifiers.ctrl {
        value |= MOD_CONTROL;
    }
    if modifiers.alt {
        value |= MOD_ALT;
    }
    if modifiers.shift {
        value |= MOD_SHIFT;
    }
    if modifiers.win {
        value |= MOD_WIN;
    }
    value
}

fn virtual_key(key: ShortcutKey) -> u32 {
    match key {
        ShortcutKey::A => 0x41,
        ShortcutKey::B => 0x42,
        ShortcutKey::C => 0x43,
        ShortcutKey::D => 0x44,
        ShortcutKey::E => 0x45,
        ShortcutKey::F => 0x46,
        ShortcutKey::G => 0x47,
        ShortcutKey::H => 0x48,
        ShortcutKey::I => 0x49,
        ShortcutKey::J => 0x4A,
        ShortcutKey::K => 0x4B,
        ShortcutKey::L => 0x4C,
        ShortcutKey::M => 0x4D,
        ShortcutKey::N => 0x4E,
        ShortcutKey::O => 0x4F,
        ShortcutKey::P => 0x50,
        ShortcutKey::Q => 0x51,
        ShortcutKey::R => 0x52,
        ShortcutKey::S => 0x53,
        ShortcutKey::T => 0x54,
        ShortcutKey::U => 0x55,
        ShortcutKey::V => 0x56,
        ShortcutKey::W => 0x57,
        ShortcutKey::X => 0x58,
        ShortcutKey::Y => 0x59,
        ShortcutKey::Z => 0x5A,
        ShortcutKey::Digit0 => 0x30,
        ShortcutKey::Digit1 => 0x31,
        ShortcutKey::Digit2 => 0x32,
        ShortcutKey::Digit3 => 0x33,
        ShortcutKey::Digit4 => 0x34,
        ShortcutKey::Digit5 => 0x35,
        ShortcutKey::Digit6 => 0x36,
        ShortcutKey::Digit7 => 0x37,
        ShortcutKey::Digit8 => 0x38,
        ShortcutKey::Digit9 => 0x39,
        ShortcutKey::F1 => 0x70,
        ShortcutKey::F2 => 0x71,
        ShortcutKey::F3 => 0x72,
        ShortcutKey::F4 => 0x73,
        ShortcutKey::F5 => 0x74,
        ShortcutKey::F6 => 0x75,
        ShortcutKey::F7 => 0x76,
        ShortcutKey::F8 => 0x77,
        ShortcutKey::F9 => 0x78,
        ShortcutKey::F10 => 0x79,
        ShortcutKey::F11 => 0x7A,
        ShortcutKey::F12 => 0x7B,
        ShortcutKey::Left => 0x25,
        ShortcutKey::Up => 0x26,
        ShortcutKey::Right => 0x27,
        ShortcutKey::Down => 0x28,
        ShortcutKey::Space => 0x20,
    }
}

#[derive(Clone, Copy)]
struct HotkeyBinding {
    id: i32,
    modifiers: windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS,
    key: u32,
}

impl GlobalHotkey {
    pub fn start(
        shortcut: Shortcut,
        on_pressed: impl Fn() + Send + 'static,
    ) -> Result<Self, HotkeyError> {
        Self::start_bindings(
            vec![HotkeyBinding {
                id: HOTKEY_ID,
                modifiers: windows_modifiers(shortcut.modifiers),
                key: virtual_key(shortcut.key),
            }],
            move |_| on_pressed(),
        )
    }

    fn start_bindings(
        bindings: Vec<HotkeyBinding>,
        on_pressed: impl Fn(i32) + Send + 'static,
    ) -> Result<Self, HotkeyError> {
        let (thread_id_sender, thread_id_receiver) = mpsc::sync_channel(1);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (done_sender, done) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let thread = thread::Builder::new()
            .name("global-hotkey".to_owned())
            .spawn(move || {
                let result = run_hotkey_loop(
                    bindings,
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
    bindings: Vec<HotkeyBinding>,
    on_pressed: impl Fn(i32),
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
    let registered = register_with_cancellation(
        cancelled,
        || {
            let mut registered = Vec::new();
            for binding in &bindings {
                if let Err(error) =
                    unsafe { RegisterHotKey(None, binding.id, binding.modifiers, binding.key) }
                {
                    for id in registered {
                        let _ = unsafe { UnregisterHotKey(None, id) };
                    }
                    return Err(
                        if unsafe { GetLastError() } == ERROR_HOTKEY_ALREADY_REGISTERED {
                            HotkeyError::Conflict
                        } else {
                            HotkeyError::Windows(error)
                        },
                    );
                }
                registered.push(binding.id);
            }
            Ok(())
        },
        || {
            unregister_all(&bindings);
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
        unregister_all(&bindings);
        return Ok(());
    }
    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == -1 {
            let error = HotkeyError::Windows(windows::core::Error::from_win32());
            unregister_all(&bindings);
            return Err(error);
        }
        if result.0 == 0 {
            break;
        }
        if let Some(id) = activation_id(&message, &bindings) {
            on_pressed(id);
        }
    }
    unregister_all(&bindings);
    Ok(())
}

fn activation_id(message: &MSG, bindings: &[HotkeyBinding]) -> Option<i32> {
    (message.message == WM_HOTKEY)
        .then_some(message.wParam.0 as i32)
        .filter(|id| bindings.iter().any(|binding| binding.id == *id))
}

fn unregister_all(bindings: &[HotkeyBinding]) {
    for binding in bindings {
        let _ = unsafe { UnregisterHotKey(None, binding.id) };
    }
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
        let shortcut = Shortcut::default();
        assert_eq!(
            windows_modifiers(shortcut.modifiers),
            MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT
        );
        assert_eq!(virtual_key(shortcut.key), 0x56);
        assert!(!include_str!("hotkey.rs").contains(&["Send", "Input"].concat()));
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
        let bindings = [HotkeyBinding {
            id: HOTKEY_ID,
            modifiers: MOD_CONTROL,
            key: 0x56,
        }];
        assert_eq!(activation_id(&message, &bindings), Some(HOTKEY_ID));

        message.wParam = WPARAM((HOTKEY_ID + 1) as usize);
        assert_eq!(activation_id(&message, &bindings), None);
        message.message = WM_QUIT;
        message.wParam = WPARAM(HOTKEY_ID as usize);
        assert_eq!(activation_id(&message, &bindings), None);
    }

    #[test]
    fn customizable_modifiers_have_distinct_windows_bindings() {
        assert_ne!(
            windows_modifiers(ShortcutModifiers::CTRL_ALT),
            windows_modifiers(ShortcutModifiers::CTRL_SHIFT)
        );
        assert!(windows_modifiers(ShortcutModifiers::CTRL_ALT).contains(MOD_NOREPEAT));
    }

    #[test]
    fn group_shortcuts_allow_arrow_key_repeat_while_modifiers_stay_held() {
        let source = include_str!("hotkey.rs");
        let group_section = source
            .split("impl GroupHotkeys")
            .nth(1)
            .unwrap()
            .split("pub struct QuickPasteHotkeys")
            .next()
            .unwrap();

        assert!(group_section.contains("track_group_switch_session"));
        assert!(group_section.contains("update_group_key"));
        assert!(group_section.contains("drain_group_hotkey_messages"));
        assert!(!group_section.contains("if !control_down() || !alt_down()"));
    }

    #[test]
    fn group_key_repress_and_hold_each_switch_at_the_expected_time() {
        let started = Instant::now();
        let mut state = GroupKeyState::new(true, started);
        let switches = std::cell::RefCell::new(Vec::new());
        let record = |direction| switches.borrow_mut().push(direction);

        update_group_key(
            &mut state,
            true,
            started + GROUP_KEY_REPEAT_DELAY - Duration::from_millis(1),
            1,
            &record,
        );
        assert!(switches.borrow().is_empty());

        update_group_key(
            &mut state,
            true,
            started + GROUP_KEY_REPEAT_DELAY,
            1,
            &record,
        );
        update_group_key(&mut state, false, Instant::now(), 1, &record);
        update_group_key(&mut state, true, Instant::now(), 1, &record);

        assert_eq!(*switches.borrow(), [1, 1]);
    }

    #[test]
    fn quick_paste_waits_for_the_trigger_keys_to_be_released() {
        let mut polls = 0;
        let released = wait_until_released(
            Duration::from_secs(1),
            Duration::ZERO,
            || {
                polls += 1;
                polls == 3
            },
            |_| {},
        );

        assert!(released);
        assert_eq!(polls, 3);
    }

    #[test]
    fn quick_paste_abandons_a_trigger_that_never_releases() {
        let released = wait_until_released(Duration::ZERO, Duration::ZERO, || false, |_| {});

        assert!(!released);
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
        let hotkey = GlobalHotkey::start(Shortcut::default(), || {})
            .expect("default hotkey should register");
        hotkey.shutdown();
    }
}
