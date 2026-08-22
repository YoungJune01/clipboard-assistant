pub mod domain;
pub mod platform;
pub mod services;

#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use services::panel::PanelController;
#[cfg(windows)]
use services::paste::{QuickPanelPasteCoordinator, SafePasteService};
#[cfg(windows)]
use services::session_records::{SessionRecordCommands, SessionRecordStore, SessionRecordView};

#[cfg(windows)]
use tauri::{Emitter, Manager};

#[cfg(windows)]
type WindowsSafePasteService = SafePasteService<
    platform::windows::paste::Win32PasteTarget,
    platform::windows::clipboard::ClipboardPublisher,
    Arc<PanelController>,
    platform::windows::paste::Win32PasteInput,
>;

#[cfg(windows)]
type WindowsQuickPanelCoordinator =
    QuickPanelPasteCoordinator<Arc<PanelController>, WindowsSafePasteService>;

#[cfg(windows)]
struct ClipboardRuntime {
    listener: std::sync::Mutex<Option<platform::windows::clipboard::ClipboardListener>>,
    event_drain_stop: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    event_drain: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[cfg(windows)]
impl Drop for ClipboardRuntime {
    fn drop(&mut self) {
        let listener = self
            .listener
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(listener) = listener {
            let _ = listener.shutdown();
        }
        let stop = self
            .event_drain_stop
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(stop) = stop {
            let _ = stop.send(());
        }
        let drain = self
            .event_drain
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(drain) = drain {
            let _ = drain.join();
        }
    }
}

#[cfg(windows)]
fn spawn_clipboard_event_drain(
    events: platform::windows::clipboard::LatestClipboardEventReceiver,
    on_change: impl Fn() + Send + 'static,
) -> std::io::Result<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)> {
    let (stop, stopped) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("clipboard-event-drain".to_owned())
        .spawn(move || {
            loop {
                if stopped.try_recv().is_ok() {
                    break;
                }
                match events.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(_revision) => on_change(),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;
    Ok((stop, thread))
}

#[cfg(all(test, windows))]
mod runtime_tests {
    use super::spawn_clipboard_event_drain;
    use crate::{
        domain::{CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity},
        platform::windows::clipboard::{ClipboardEvent, latest_clipboard_event_channel},
        services::session_records::SessionRecordStore,
    };
    use chrono::Utc;
    use std::sync::Arc;

    #[test]
    fn clipboard_event_drain_stops_even_when_event_senders_remain_alive() {
        let records = Arc::new(SessionRecordStore::default());
        let (events, receiver) = latest_clipboard_event_channel(records);
        let retained_sender = events.clone();
        let (stop, thread) = spawn_clipboard_event_drain(receiver, || {}).unwrap();
        let (joined, observed) = std::sync::mpsc::sync_channel(1);

        stop.send(()).unwrap();
        std::thread::spawn(move || {
            let _retained_sender = retained_sender;
            let _ = joined.send(thread.join());
        });

        assert!(
            observed
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
    }

    #[test]
    fn clipboard_event_drain_exposes_real_captures_to_the_session_store() {
        let records = Arc::new(SessionRecordStore::default());
        let (events, receiver) = latest_clipboard_event_channel(Arc::clone(&records));
        let (stop, thread) = spawn_clipboard_event_drain(receiver, || {}).unwrap();
        events
            .send(ClipboardEvent {
                sequence_number: 41,
                captured: CapturedClipboard {
                    content_identity: ContentIdentity::new("clipboard-sequence:41"),
                    captured_at: Utc::now(),
                    source: SourceIdentity::default(),
                    representations: vec![ClipboardRepresentation::UnicodeText {
                        text: "real capture".to_owned(),
                    }],
                },
            })
            .unwrap();

        for _ in 0..100 {
            if !records.list().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        stop.send(()).unwrap();
        thread.join().unwrap();

        assert_eq!(records.list()[0].text.as_deref(), Some("real capture"));
    }
}

#[cfg(test)]
mod tests;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

#[cfg(windows)]
#[tauri::command]
fn show_quick_panel(
    coordinator: tauri::State<'_, Arc<WindowsQuickPanelCoordinator>>,
) -> Result<(), String> {
    coordinator.show()
}

#[cfg(windows)]
#[tauri::command]
fn paste_selected(
    record_id: domain::RecordId,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    coordinator: tauri::State<'_, Arc<WindowsQuickPanelCoordinator>>,
) -> Result<String, String> {
    let representations = SessionRecordCommands::new(records.inner())
        .representations(record_id)
        .map_err(|error| error.to_string())?;
    coordinator
        .paste(&representations)
        .map(domain::PasteOutcome::user_message)
        .map(str::to_owned)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn list_session_records(
    records: tauri::State<'_, Arc<SessionRecordStore>>,
) -> Vec<SessionRecordView> {
    SessionRecordCommands::new(records.inner()).list()
}

#[cfg(windows)]
#[tauri::command]
fn update_record_note(
    record_id: domain::RecordId,
    note: String,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
) -> Result<SessionRecordView, String> {
    SessionRecordCommands::new(records.inner())
        .update_note(record_id, note)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn hide_quick_panel(
    coordinator: tauri::State<'_, Arc<WindowsQuickPanelCoordinator>>,
) -> Result<(), String> {
    coordinator.hide()
}

#[cfg(windows)]
#[tauri::command]
fn toggle_quick_panel(
    coordinator: tauri::State<'_, Arc<WindowsQuickPanelCoordinator>>,
) -> Result<(), String> {
    coordinator.toggle()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(windows)]
    if let Err(error) = platform::windows::enable_per_monitor_v2() {
        eprintln!(
            "Per-Monitor V2 DPI awareness could not be set before window creation; continuing with the process DPI mode: {error}"
        );
    }

    let builder = tauri::Builder::default();
    #[cfg(windows)]
    let builder = builder
        .setup(|app| {
            let panel = app
                .get_webview_window("quick-panel")
                .ok_or_else(|| "quick-panel window is missing".to_owned())?;
            platform::windows::configure_quick_panel_style(panel.hwnd()?)?;
            let panel_controller = Arc::new(PanelController::new(panel));
            let session_records = Arc::new(SessionRecordStore::default());
            let (clipboard_events, clipboard_receiver) =
                platform::windows::clipboard::latest_clipboard_event_channel(Arc::clone(
                    &session_records,
                ));
            let listener =
                platform::windows::clipboard::ClipboardListener::start(clipboard_events)?;
            let app_handle = app.handle().clone();
            let (event_drain_stop, event_drain) =
                spawn_clipboard_event_drain(clipboard_receiver, move || {
                    let _ = app_handle.emit("clipboard-records-changed", ());
                })?;
            let paste = SafePasteService::new(
                platform::windows::paste::Win32PasteTarget::new(),
                listener.publisher(),
                Arc::clone(&panel_controller),
                platform::windows::paste::Win32PasteInput,
            );
            app.manage(Arc::clone(&panel_controller));
            app.manage(session_records);
            app.manage(Arc::new(QuickPanelPasteCoordinator::new(
                panel_controller,
                paste,
            )));
            app.manage(ClipboardRuntime {
                listener: std::sync::Mutex::new(Some(listener)),
                event_drain_stop: std::sync::Mutex::new(Some(event_drain_stop)),
                event_drain: std::sync::Mutex::new(Some(event_drain)),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "quick-panel" {
                return;
            }
            let controller = window.state::<Arc<PanelController>>();
            let coordinator = window.state::<Arc<WindowsQuickPanelCoordinator>>();
            match event {
                tauri::WindowEvent::Focused(focused) => {
                    let result = controller.on_focus_changed(*focused);
                    if !focused && (result.is_err() || !controller.is_visible()) {
                        coordinator.clear_target();
                    }
                }
                tauri::WindowEvent::ScaleFactorChanged { .. } => {
                    let _ = controller.reposition_if_visible();
                }
                tauri::WindowEvent::Destroyed => {
                    let _ = controller.hide();
                    coordinator.clear_target();
                }
                _ => {}
            }
        })
        .invoke_handler(tauri::generate_handler![
            greet,
            show_quick_panel,
            hide_quick_panel,
            toggle_quick_panel,
            paste_selected,
            list_session_records,
            update_record_note
        ]);
    #[cfg(not(windows))]
    let builder = builder.invoke_handler(tauri::generate_handler![greet]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
