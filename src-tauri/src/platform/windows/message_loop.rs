use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, LRESULT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM},
        System::{
            DataExchange::{AddClipboardFormatListener, RemoveClipboardFormatListener},
            LibraryLoader::GetModuleHandleW,
        },
        UI::WindowsAndMessaging::{
            CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
            GWLP_USERDATA, GetWindowLongPtrW, HWND_MESSAGE, MSG, MsgWaitForMultipleObjects,
            PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassW, SetWindowLongPtrW,
            TranslateMessage, UnregisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
            WM_CLIPBOARDUPDATE, WM_NCCREATE, WM_QUIT, WNDCLASSW,
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
    shutdown_event: HANDLE,
) -> Result<(), ClipboardListenerError> {
    let result =
        unsafe { run_message_loop_inner(events, product_write, &ready, &shutdown, shutdown_event) };
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
    shutdown_event: HANDLE,
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
    let mut class_registered = true;
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
            let cleanup = cleanup_in_order(
                || Ok::<(), super::clipboard::CleanupFailure>(()),
                || Ok::<(), super::clipboard::CleanupFailure>(()),
                || {
                    unsafe { UnregisterClassW(class_name, Some(instance)) }
                        .map_err(super::clipboard::CleanupFailure::UnregisterClass)
                },
            );
            return Err(combine_operation_and_cleanup(
                ClipboardListenerError::Windows(error),
                cleanup,
            ));
        }
    };
    if let Err(error) = unsafe { AddClipboardFormatListener(hwnd) } {
        let cleanup = unsafe {
            cleanup_window_resources(hwnd, false, class_name, instance, &mut class_registered)
        };
        return Err(combine_operation_and_cleanup(
            ClipboardListenerError::Windows(error),
            cleanup,
        ));
    }
    let _ = ready.send(Ok(hwnd.0 as isize));

    let mut message = MSG::default();
    let loop_result = 'message_loop: loop {
        let wait = unsafe {
            MsgWaitForMultipleObjects(Some(&[shutdown_event]), false, u32::MAX, QS_ALLINPUT)
        };
        if wait == WAIT_FAILED {
            break Err(ClipboardListenerError::Windows(
                windows::core::Error::from_win32(),
            ));
        }
        if wait == WAIT_OBJECT_0 {
            break Ok(());
        }
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            if message.message == WM_QUIT {
                break 'message_loop Ok(());
            }
            if message.message == WM_CLIPBOARD_LISTENER_SHUTDOWN {
                match shutdown.try_recv() {
                    Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        break 'message_loop Ok(());
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => continue,
                }
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    };
    let cleanup = unsafe {
        cleanup_window_resources(hwnd, true, class_name, instance, &mut class_registered)
    };
    match loop_result {
        Ok(()) if cleanup.is_empty() => Ok(()),
        Ok(()) => Err(ClipboardListenerError::Cleanup(cleanup)),
        Err(error) => Err(combine_operation_and_cleanup(error, cleanup)),
    }
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

unsafe fn cleanup_window_resources(
    hwnd: HWND,
    listener_registered: bool,
    class_name: windows::core::PCWSTR,
    instance: windows::Win32::Foundation::HINSTANCE,
    class_registered: &mut bool,
) -> Vec<super::clipboard::CleanupFailure> {
    cleanup_in_order(
        || {
            if listener_registered {
                unsafe { RemoveClipboardFormatListener(hwnd) }
                    .map_err(super::clipboard::CleanupFailure::RemoveListener)
            } else {
                Ok(())
            }
        },
        || {
            let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ListenerState;
            if !pointer.is_null() {
                unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
                unsafe { drop(Box::from_raw(pointer)) };
            }
            unsafe { DestroyWindow(hwnd) }.map_err(super::clipboard::CleanupFailure::DestroyWindow)
        },
        || {
            if *class_registered {
                let result = unsafe { UnregisterClassW(class_name, Some(instance)) }
                    .map_err(super::clipboard::CleanupFailure::UnregisterClass);
                if result.is_ok() {
                    *class_registered = false;
                }
                result
            } else {
                Ok(())
            }
        },
    )
}

fn combine_operation_and_cleanup(
    operation: ClipboardListenerError,
    cleanup: Vec<super::clipboard::CleanupFailure>,
) -> ClipboardListenerError {
    if cleanup.is_empty() {
        operation
    } else {
        ClipboardListenerError::OperationAndCleanup {
            operation: Box::new(operation),
            cleanup,
        }
    }
}

pub(super) fn cleanup_in_order<E>(
    mut remove: impl FnMut() -> Result<(), E>,
    mut destroy: impl FnMut() -> Result<(), E>,
    mut unregister: impl FnMut() -> Result<(), E>,
) -> Vec<E> {
    let mut failures = Vec::new();
    if let Err(error) = remove() {
        failures.push(error);
    }
    if let Err(error) = destroy() {
        failures.push(error);
    }
    if let Err(error) = unregister() {
        failures.push(error);
    }
    failures
}

pub(super) struct WakeAndJoinResult<WakeError, ThreadError> {
    pub failures: Vec<WakeError>,
    pub thread: Result<(), ThreadError>,
}

pub(super) fn wake_and_join<WakeError, ThreadError>(
    mut control: impl FnMut() -> Result<(), WakeError>,
    mut post: impl FnMut() -> Result<(), WakeError>,
    mut signal: impl FnMut() -> Result<(), WakeError>,
    join: impl FnOnce() -> Result<(), ThreadError>,
) -> WakeAndJoinResult<WakeError, ThreadError> {
    let mut failures = Vec::new();
    if let Err(error) = control() {
        failures.push(error);
    }
    if let Err(error) = post() {
        failures.push(error);
    }
    if let Err(error) = signal() {
        failures.push(error);
    }
    WakeAndJoinResult {
        failures,
        thread: join(),
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
        ClipboardListenerError::ReadyTimeout => ClipboardListenerError::ReadyTimeout,
        ClipboardListenerError::Cleanup(failures) => {
            ClipboardListenerError::Cleanup(failures.clone())
        }
        ClipboardListenerError::OperationAndCleanup { operation, cleanup } => {
            ClipboardListenerError::OperationAndCleanup {
                operation: Box::new(clone_listener_error(operation)),
                cleanup: cleanup.clone(),
            }
        }
        ClipboardListenerError::Shutdown { failures, thread } => ClipboardListenerError::Shutdown {
            failures: failures.iter().map(clone_shutdown_failure).collect(),
            thread: thread
                .as_ref()
                .map(|error| Box::new(clone_listener_error(error))),
        },
    }
}

fn clone_shutdown_failure(
    failure: &super::clipboard::ShutdownFailure,
) -> super::clipboard::ShutdownFailure {
    use super::clipboard::ShutdownFailure;

    match failure {
        ShutdownFailure::ControlDisconnected => ShutdownFailure::ControlDisconnected,
        ShutdownFailure::PostMessage(error) => ShutdownFailure::PostMessage(error.clone()),
        ShutdownFailure::Signal(error) => ShutdownFailure::Signal(error.clone()),
        ShutdownFailure::CloseEvent(error) => ShutdownFailure::CloseEvent(error.clone()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::{cleanup_in_order, wake_and_join};

    #[test]
    fn post_and_control_failures_still_signal_and_join() {
        let calls = RefCell::new(Vec::new());
        let result = wake_and_join(
            || {
                calls.borrow_mut().push("control");
                Err("control")
            },
            || {
                calls.borrow_mut().push("post");
                Err("post")
            },
            || {
                calls.borrow_mut().push("signal");
                Ok(())
            },
            || {
                calls.borrow_mut().push("join");
                Ok::<(), ()>(())
            },
        );

        assert_eq!(result.failures, ["control", "post"]);
        assert!(result.thread.is_ok());
        assert_eq!(*calls.borrow(), ["control", "post", "signal", "join"]);
    }

    #[test]
    fn cleanup_attempts_every_stage_in_order_and_preserves_failures() {
        let calls = RefCell::new(Vec::new());
        let failures = cleanup_in_order(
            || {
                calls.borrow_mut().push("remove");
                Err("remove")
            },
            || {
                calls.borrow_mut().push("destroy");
                Err("destroy")
            },
            || {
                calls.borrow_mut().push("unregister");
                Err("unregister")
            },
        );

        assert_eq!(*calls.borrow(), ["remove", "destroy", "unregister"]);
        assert_eq!(failures, ["remove", "destroy", "unregister"]);
    }
}
