use std::{
    ptr,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::Duration,
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, GlobalFree, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
        System::{
            DataExchange::{
                CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
                OpenClipboard, SetClipboardData,
            },
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Threading::{CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject},
        },
    },
    core::w,
};

use crate::{
    domain::ClipboardRepresentation,
    platform::windows::clipboard::{ClipboardEvent, ClipboardListener},
};

const CF_UNICODETEXT_FORMAT: u32 = 13;
const LISTENER_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);

struct NamedClipboardLock(HANDLE);

impl NamedClipboardLock {
    fn acquire() -> Self {
        let handle = unsafe {
            CreateMutexW(
                None,
                false,
                w!("Local\\ClipboardAssistant.IntegrationTests.Clipboard"),
            )
            .expect("create named clipboard test mutex")
        };
        assert!(matches!(
            unsafe { WaitForSingleObject(handle, INFINITE) },
            WAIT_OBJECT_0 | WAIT_ABANDONED
        ));
        Self(handle)
    }
}

impl Drop for NamedClipboardLock {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

struct ClipboardTextBackup(Option<String>);

impl ClipboardTextBackup {
    fn capture() -> Self {
        Self(read_clipboard_text())
    }
}

impl Drop for ClipboardTextBackup {
    fn drop(&mut self) {
        if let Some(text) = &self.0 {
            let _ = write_clipboard_text(text);
        }
    }
}

#[test]
fn listener_captures_text_suppresses_owned_sequence_and_resumes() {
    let _lock = NamedClipboardLock::acquire();
    let _backup = ClipboardTextBackup::capture();
    let (sender, receiver) = mpsc::channel();
    let listener = start_listener(sender);

    let external_text = unique_text("external");
    let external_sequence = write_clipboard_text(&external_text).expect("write external text");
    let external_event = receive_sequence(&receiver, external_sequence);
    assert_event_text(&external_event, &external_text);
    assert_eq!(
        external_event.captured.content_identity.as_str(),
        format!("clipboard-sequence:{external_sequence}")
    );

    let owned_text = unique_text("owned");
    let ownership = listener
        .begin_product_write()
        .expect("begin product-owned clipboard write");
    let owned_sequence = write_clipboard_text(&owned_text).expect("write product-owned text");
    ownership.finish(owned_sequence);
    assert_no_sequence(&receiver, owned_sequence);

    let resumed_text = unique_text("resumed");
    let resumed_sequence = write_clipboard_text(&resumed_text).expect("write resumed text");
    let resumed_event = receive_sequence(&receiver, resumed_sequence);
    assert_event_text(&resumed_event, &resumed_text);

    shutdown_listener(listener);
}

#[test]
fn listener_can_start_and_shutdown_repeatedly() {
    let _lock = NamedClipboardLock::acquire();
    for _ in 0..3 {
        let (sender, _receiver) = mpsc::channel();
        let listener = start_listener(sender);
        let ownership = listener
            .begin_product_write()
            .expect("begin first product-owned clipboard write");
        assert!(listener.begin_product_write().is_err());
        drop(ownership);
        shutdown_listener(listener);
    }
}

fn start_listener(sender: mpsc::Sender<ClipboardEvent>) -> ClipboardListener {
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = result_sender.send(ClipboardListener::start(sender));
    });
    result_receiver
        .recv_timeout(LISTENER_OPERATION_TIMEOUT)
        .expect("clipboard listener ready signal before timeout")
        .expect("start clipboard listener")
}

fn shutdown_listener(listener: ClipboardListener) {
    let (result_sender, result_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = result_sender.send(listener.shutdown());
    });
    match result_receiver.recv_timeout(LISTENER_OPERATION_TIMEOUT) {
        Ok(result) => result.expect("shut down clipboard listener"),
        Err(RecvTimeoutError::Timeout) => panic!("clipboard listener shutdown before timeout"),
        Err(RecvTimeoutError::Disconnected) => panic!("clipboard listener shutdown result channel"),
    }
}

fn receive_sequence(receiver: &Receiver<ClipboardEvent>, sequence: u32) -> ClipboardEvent {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut observed = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let event = receiver.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "receive clipboard sequence {sequence} before timeout; observed {observed:?}: {error}"
            )
        });
        if event.sequence_number == sequence {
            return event;
        }
        observed.push(event.sequence_number);
    }
}

fn assert_no_sequence(receiver: &Receiver<ClipboardEvent>, sequence: u32) {
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    while let Ok(event) =
        receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
    {
        assert_ne!(
            event.sequence_number, sequence,
            "owned sequence was emitted"
        );
        if std::time::Instant::now() >= deadline {
            break;
        }
    }
}

fn assert_event_text(event: &ClipboardEvent, expected: &str) {
    assert!(event.captured.representations.iter().any(|representation| {
        matches!(
            representation,
            ClipboardRepresentation::UnicodeText { text } if text == expected
        )
    }));
}

fn unique_text(label: &str) -> String {
    format!("clipboard-assistant-{label}-{}", uuid::Uuid::new_v4())
}

fn write_clipboard_text(text: &str) -> windows::core::Result<u32> {
    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    unsafe {
        OpenClipboard(None)?;
        let result = (|| {
            EmptyClipboard()?;
            let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * size_of::<u16>())?;
            let target = GlobalLock(memory).cast::<u16>();
            if target.is_null() {
                let _ = GlobalFree(Some(memory));
                return Err(windows::core::Error::from_win32());
            }
            ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
            let _ = GlobalUnlock(memory);
            if let Err(error) = SetClipboardData(CF_UNICODETEXT_FORMAT, Some(HANDLE(memory.0))) {
                let _ = GlobalFree(Some(memory));
                return Err(error);
            }
            Ok(())
        })();
        match result {
            Ok(()) => CloseClipboard().map(|_| GetClipboardSequenceNumber()),
            Err(error) => {
                let _ = CloseClipboard();
                Err(error)
            }
        }
    }
}

fn read_clipboard_text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let result = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT_FORMAT).ok()?;
            let pointer = GlobalLock(windows::Win32::Foundation::HGLOBAL(handle.0)).cast::<u16>();
            if pointer.is_null() {
                return None;
            }
            let mut length = 0;
            while *pointer.add(length) != 0 {
                length += 1;
            }
            let text = String::from_utf16(std::slice::from_raw_parts(pointer, length)).ok();
            let _ = GlobalUnlock(windows::Win32::Foundation::HGLOBAL(handle.0));
            text
        })();
        let _ = CloseClipboard();
        result
    }
}
