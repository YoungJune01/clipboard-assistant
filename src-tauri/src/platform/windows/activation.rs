use std::{error::Error, fmt};

use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{GetForegroundWindow, SetForegroundWindow},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationError {
    Rejected,
}

impl fmt::Display for ActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Windows did not activate the quick panel")
    }
}

impl Error for ActivationError {}

trait ActivationApi {
    type Window: Copy + Eq;

    fn request_foreground(&self, window: Self::Window) -> bool;
    fn foreground_window(&self) -> Self::Window;
}

pub fn activate(window: HWND) -> Result<(), ActivationError> {
    activate_and_verify(&Win32ActivationApi, window)
}

fn activate_and_verify<A: ActivationApi>(
    api: &A,
    window: A::Window,
) -> Result<(), ActivationError> {
    let _requested = api.request_foreground(window);
    if api.foreground_window() == window {
        Ok(())
    } else {
        Err(ActivationError::Rejected)
    }
}

struct Win32ActivationApi;

impl ActivationApi for Win32ActivationApi {
    type Window = HWND;

    fn request_foreground(&self, window: Self::Window) -> bool {
        unsafe { SetForegroundWindow(window) }.as_bool()
    }

    fn foreground_window(&self) -> Self::Window {
        unsafe { GetForegroundWindow() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_foreground_request_returns_error_without_input_injection_api() {
        let api = FakeActivationApi {
            requested: std::sync::Mutex::new(Vec::new()),
            foreground: 41,
        };

        let error = activate_and_verify(&api, 99).expect_err("activation must be rejected");

        assert_eq!(error, ActivationError::Rejected);
        assert_eq!(*api.requested.lock().unwrap(), vec![99]);
    }

    #[test]
    fn production_adapter_has_no_send_input_path() {
        let source = include_str!("activation.rs");
        let forbidden = ["Send", "Input"].concat();

        assert!(!source.contains(&forbidden));
    }

    struct FakeActivationApi {
        requested: std::sync::Mutex<Vec<isize>>,
        foreground: isize,
    }

    impl ActivationApi for FakeActivationApi {
        type Window = isize;

        fn request_foreground(&self, window: Self::Window) -> bool {
            self.requested.lock().unwrap().push(window);
            false
        }

        fn foreground_window(&self) -> Self::Window {
            self.foreground
        }
    }
}
