use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{
            DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener},
            LibraryLoader::GetModuleHandleW,
        },
        UI::WindowsAndMessaging::{
            CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
            GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, MSG, RegisterClassW,
            SetWindowLongPtrW, TranslateMessage, UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE,
            WM_APP, WM_CLIPBOARDUPDATE, WM_NCCREATE, WNDCLASSW,
        },
    },
    core::w,
};

use super::clipboard::{
    ClipboardListenerError, EventSender, ProductWriteOwnership, ReadySender, ShutdownReceiver,
    capture_update,
};

pub(super) const WM_CLIPBOARD_LISTENER_SHUTDOWN: u32 = WM_APP + 1;

pub(super) struct ListenerState {
    pub events: EventSender,
    pub product_write: ProductWriteOwnership,
}

pub(super) fn run_message_loop(
    events: EventSender,
    product_write: ProductWriteOwnership,
    ready: ReadySender,
    shutdown: ShutdownReceiver,
) -> Result<(), ClipboardListenerError> {
    let result = unsafe { run_message_loop_inner(events, product_write, &ready, &shutdown) };
    if let Err(error) = &result {
        let _ = ready.send(Err(clone_listener_error(error)));
    }
    result
}

unsafe fn run_message_loop_inner(
    events: EventSender,
    product_write: ProductWriteOwnership,
    ready: &ReadySender,
    shutdown: &ShutdownReceiver,
) -> Result<(), ClipboardListenerError> {
    let module = unsafe { GetModuleHandleW(None) }.map_err(ClipboardListenerError::Windows)?;
    let instance = windows::Win32::Foundation::HINSTANCE(module.0);
    let class_name = w!("ClipboardAssistant.ListenerWindow");
    let window_class = WNDCLASSW {
        hInstance: instance,
        lpszClassName: class_name,
        lpfnWndProc: Some(window_proc),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&window_class) } == 0 {
        return Err(ClipboardListenerError::Windows(
            windows::core::Error::from_win32(),
        ));
    }
    let _class = RegisteredWindowClass {
        class_name,
        instance,
    };
    let state_pointer = Box::into_raw(Box::new(ListenerState {
        events,
        product_write,
    }));
    let hwnd = match unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!(""),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            Some(state_pointer.cast()),
        )
    } {
        Ok(hwnd) => hwnd,
        Err(error) => {
            unsafe { drop(Box::from_raw(state_pointer)) };
            return Err(ClipboardListenerError::Windows(error));
        }
    };
    let mut window = ListenerWindow {
        hwnd,
        listener_registered: false,
    };
    unsafe { AddClipboardFormatListener(hwnd) }.map_err(ClipboardListenerError::Windows)?;
    window.listener_registered = true;
    let _ = ready.send(Ok(hwnd.0 as isize));

    let mut message = MSG::default();
    loop {
        let status = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if status.0 == -1 {
            return Err(ClipboardListenerError::Windows(
                windows::core::Error::from_win32(),
            ));
        }
        if status.0 == 0 {
            break;
        }
        if message.message == WM_CLIPBOARD_LISTENER_SHUTDOWN {
            match shutdown.try_recv() {
                Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                Err(std::sync::mpsc::TryRecvError::Empty) => continue,
            }
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    catch_unwind(AssertUnwindSafe(|| unsafe {
        window_proc_inner(hwnd, message, wparam, lparam)
    }))
    .unwrap_or_else(|_| unsafe { DefWindowProcW(hwnd, message, wparam, lparam) })
}

unsafe fn window_proc_inner(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let create = lparam.0 as *const CREATESTRUCTW;
        if !create.is_null() {
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*create).lpCreateParams as isize) };
        }
    }
    match message {
        WM_CLIPBOARDUPDATE => {
            if let Some(state) = unsafe { state_from_window(hwnd) } {
                capture_update(state);
            }
            LRESULT(0)
        }
        WM_CLIPBOARD_LISTENER_SHUTDOWN => LRESULT(0),
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe fn state_from_window(hwnd: HWND) -> Option<&'static ListenerState> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ListenerState;
    unsafe { pointer.as_ref() }
}

struct ListenerWindow {
    hwnd: HWND,
    listener_registered: bool,
}

impl Drop for ListenerWindow {
    fn drop(&mut self) {
        unsafe {
            if self.listener_registered {
                let _ = RemoveClipboardFormatListener(self.hwnd);
            }
            let pointer = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) as *mut ListenerState;
            if !pointer.is_null() {
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(pointer));
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

struct RegisteredWindowClass {
    class_name: windows::core::PCWSTR,
    instance: windows::Win32::Foundation::HINSTANCE,
}

impl Drop for RegisteredWindowClass {
    fn drop(&mut self) {
        unsafe {
            let _ = UnregisterClassW(self.class_name, Some(self.instance));
        }
    }
}

fn clone_listener_error(error: &ClipboardListenerError) -> ClipboardListenerError {
    match error {
        ClipboardListenerError::Windows(error) => ClipboardListenerError::Windows(error.clone()),
        ClipboardListenerError::ThreadSpawn(error) => ClipboardListenerError::ThreadSpawn(
            std::io::Error::new(error.kind(), error.to_string()),
        ),
        ClipboardListenerError::ProductWriteAlreadyInProgress => {
            ClipboardListenerError::ProductWriteAlreadyInProgress
        }
        ClipboardListenerError::UnexpectedThreadExit => {
            ClipboardListenerError::UnexpectedThreadExit
        }
        ClipboardListenerError::ThreadPanicked => ClipboardListenerError::ThreadPanicked,
    }
}
