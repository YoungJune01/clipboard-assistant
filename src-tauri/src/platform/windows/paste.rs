use std::{
    collections::hash_map::DefaultHasher,
    error::Error,
    fmt,
    hash::{Hash, Hasher},
    mem::{size_of, zeroed},
};

use windows::Win32::{
    Foundation::{CloseHandle, ERROR_SUCCESS, FILETIME, GetLastError, HANDLE, HWND, SetLastError},
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
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
            SendInput, SetFocus, VIRTUAL_KEY, VK_CONTROL, VK_V,
        },
        WindowsAndMessaging::{
            GA_ROOT, GCW_ATOM, GUITHREADINFO, GWLP_HINSTANCE, GWLP_USERDATA, GWLP_WNDPROC,
            GetAncestor, GetClassLongPtrW, GetForegroundWindow, GetGUIThreadInfo,
            GetWindowLongPtrW, GetWindowThreadProcessId, IsWindow,
        },
    },
};

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
        })
    }
}

impl Error for WindowsPasteError {}

#[derive(Clone, Copy, Default)]
pub struct Win32PasteTarget;

impl PasteTarget for Win32PasteTarget {
    type Error = WindowsPasteError;
    type Window = WindowsWindow;

    fn capture(&self) -> Result<TargetSnapshot<Self::Window>, Self::Error> {
        let foreground = unsafe { GetForegroundWindow() };
        if foreground.0.is_null() {
            return Err(WindowsPasteError::NoForegroundWindow);
        }
        let root = unsafe { GetAncestor(foreground, GA_ROOT) };
        let window = if root.0.is_null() { foreground } else { root };
        let identity = inspect_window(window)?;
        let focused_control = focused_control(identity.thread_id)
            .filter(|focused| {
                window_process_thread(*focused).ok()
                    == Some((identity.process_id, identity.thread_id))
            })
            .map(WindowsWindow::from_hwnd);
        Ok(TargetSnapshot {
            window: WindowsWindow::from_hwnd(window),
            focused_control,
            identity,
        })
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
        crate::platform::windows::activation::activate(target.window.hwnd())
            .map_err(|_| WindowsPasteError::ForegroundRejected)?;
        if let Some(control) = target.focused_control {
            restore_focus_control(
                control,
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

fn focused_control(thread_id: u32) -> Option<HWND> {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..unsafe { zeroed() }
    };
    unsafe { GetGUIThreadInfo(thread_id, &mut info) }
        .ok()
        .and_then(|_| (!info.hwndFocus.0.is_null()).then_some(info.hwndFocus))
}

fn restore_focus_control(
    control: WindowsWindow,
    process_id: u32,
    thread_id: u32,
) -> Result<(), WindowsPasteError> {
    let control = control.hwnd();
    if !unsafe { IsWindow(Some(control)) }.as_bool()
        || window_process_thread(control)? != (process_id, thread_id)
    {
        return Err(WindowsPasteError::FocusRestore);
    }
    let current_thread = unsafe { GetCurrentThreadId() };
    let attached = current_thread != thread_id
        && unsafe { AttachThreadInput(current_thread, thread_id, true) }.as_bool();
    if current_thread != thread_id && !attached {
        return Err(WindowsPasteError::FocusRestore);
    }
    let _ = unsafe { SetFocus(Some(control)) };
    let detached =
        !attached || unsafe { AttachThreadInput(current_thread, thread_id, false) }.as_bool();
    if !detached {
        return Err(WindowsPasteError::FocusRestore);
    }
    if focused_control(thread_id) == Some(control) {
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
        restricted: token_restricted(token.0),
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

fn token_restricted(token: HANDLE) -> bool {
    unsafe { IsTokenRestricted(token) }.is_ok()
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

    fn send_ctrl_v(&self, expected_foreground: WindowsWindow) -> Result<(), Self::Error> {
        send_ctrl_v(&Win32InputApi, expected_foreground)
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
    fn key_down(&self, key: PasteKey) -> bool;
    fn send(&self, key: PasteKey, key_up: bool) -> bool;
}

fn send_ctrl_v<A: InputApi>(api: &A, expected: A::Window) -> Result<(), WindowsPasteError> {
    if api.foreground() != expected {
        return Err(WindowsPasteError::ForegroundChanged);
    }
    if api.key_down(PasteKey::Control) || api.key_down(PasteKey::V) {
        return Err(WindowsPasteError::PhysicalPasteKeyDown);
    }
    let mut pressed = Vec::with_capacity(2);
    for key in [PasteKey::Control, PasteKey::V] {
        if api.foreground() != expected {
            return release_pressed(api, &mut pressed, WindowsPasteError::ForegroundChanged);
        }
        if !api.send(key, false) {
            return release_pressed(api, &mut pressed, WindowsPasteError::InputDispatch);
        }
        pressed.push(key);
        if api.foreground() != expected {
            return release_pressed(api, &mut pressed, WindowsPasteError::ForegroundChanged);
        }
    }
    for key in [PasteKey::V, PasteKey::Control] {
        if api.foreground() != expected {
            return release_pressed(api, &mut pressed, WindowsPasteError::ForegroundChanged);
        }
        if !api.send(key, true) {
            return release_pressed(api, &mut pressed, WindowsPasteError::InputDispatch);
        }
        pressed.pop();
        if api.foreground() != expected {
            return release_pressed(api, &mut pressed, WindowsPasteError::ForegroundChanged);
        }
    }
    Ok(())
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
    fn input_state_machine_sends_balanced_ctrl_v() {
        let api = FakeInputApi::new([true, true, true, true]);

        send_ctrl_v(&api, 7).unwrap();

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

        assert_eq!(send_ctrl_v(&api, 7), Err(WindowsPasteError::InputDispatch));
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

        assert_eq!(send_ctrl_v(&api, 7), Err(WindowsPasteError::InputCleanup));
    }

    #[test]
    fn physical_ctrl_or_v_prevents_any_injection() {
        for held in [[true, false], [false, true]] {
            let api = FakeInputApi::new([]);
            *api.held.lock().unwrap() = held;

            assert_eq!(
                send_ctrl_v(&api, 7),
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
            send_ctrl_v(&api, 7),
            Err(WindowsPasteError::ForegroundChanged)
        );
        assert_eq!(
            *api.events.lock().unwrap(),
            [(PasteKey::Control, false), (PasteKey::Control, true)]
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
        let _ = token_restricted(token.0);
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
        let foreground = Win32InputApi.foreground();
        Win32PasteInput.send_ctrl_v(foreground).unwrap();
    }

    struct FakeInputApi {
        foregrounds: Mutex<VecDeque<i32>>,
        held: Mutex<[bool; 2]>,
        results: Mutex<VecDeque<bool>>,
        events: Mutex<Vec<(PasteKey, bool)>>,
    }

    impl FakeInputApi {
        fn new(results: impl IntoIterator<Item = bool>) -> Self {
            Self {
                foregrounds: Mutex::new(VecDeque::from([7])),
                held: Mutex::new([false, false]),
                results: Mutex::new(results.into_iter().collect()),
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

        fn send(&self, key: PasteKey, key_up: bool) -> bool {
            self.events.lock().unwrap().push((key, key_up));
            self.results.lock().unwrap().pop_front().unwrap_or(true)
        }
    }
}
