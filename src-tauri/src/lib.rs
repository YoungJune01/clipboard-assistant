pub mod domain;
pub mod platform;
pub mod services;

#[cfg(windows)]
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[cfg(windows)]
use services::panel::PanelController;
#[cfg(windows)]
use services::paste::{QuickPanelPasteCoordinator, SafePasteService};
#[cfg(windows)]
use services::persistence::{PersistenceWorker, SqliteRepository};
#[cfg(windows)]
use services::session_records::{SessionRecordCommands, SessionRecordStore, SessionRecordView};

#[cfg(windows)]
use tauri::{Emitter, Manager};

#[cfg(windows)]
use domain::{Language, RetentionPeriod, UserSettings};

#[cfg(windows)]
use serde::Serialize;

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
struct HotkeyRuntime {
    hotkey: Mutex<Option<platform::windows::hotkey::GlobalHotkey>>,
}

#[cfg(windows)]
impl Drop for HotkeyRuntime {
    fn drop(&mut self) {
        if let Some(hotkey) = self
            .hotkey
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            hotkey.shutdown();
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HotkeyStatus {
    Available,
    Conflict,
    Unavailable,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    language: Language,
    retention: RetentionPeriod,
    storage_available: bool,
    hotkey_status: HotkeyStatus,
}

#[cfg(windows)]
struct ApplicationSettings {
    current: Mutex<UserSettings>,
    persistence: Option<Arc<PersistenceWorker>>,
    records: Arc<SessionRecordStore>,
    storage_available: Arc<AtomicBool>,
    hotkey_status: Mutex<HotkeyStatus>,
}

#[cfg(windows)]
impl ApplicationSettings {
    fn view(&self) -> SettingsView {
        let current = *lock_unpoisoned(&self.current);
        SettingsView {
            language: current.language,
            retention: current.retention,
            storage_available: self.storage_available.load(Ordering::Acquire),
            hotkey_status: *lock_unpoisoned(&self.hotkey_status),
        }
    }

    fn update_language(&self, language: Language) -> SettingsView {
        lock_unpoisoned(&self.current).language = language;
        self.persist_settings();
        self.view()
    }

    fn update_retention(&self, retention: RetentionPeriod) -> SettingsView {
        lock_unpoisoned(&self.current).retention = retention;
        self.persist_settings();
        self.prune_expired();
        self.view()
    }

    fn prune_expired(&self) {
        let retention = lock_unpoisoned(&self.current).retention;
        let now = chrono::Utc::now();
        if let Some(persistence) = &self.persistence
            && persistence.prune(retention, now).is_err()
        {
            self.storage_available.store(false, Ordering::Release);
        }
        if let Some(days) = retention.days() {
            self.records
                .prune_before(now - chrono::Duration::days(days));
        }
    }

    fn persist_settings(&self) {
        let settings = *lock_unpoisoned(&self.current);
        if let Some(persistence) = &self.persistence
            && persistence.save_settings(settings).is_err()
        {
            self.storage_available.store(false, Ordering::Release);
        }
    }
}

#[cfg(windows)]
fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    use super::{ApplicationSettings, HotkeyStatus, spawn_clipboard_event_drain};
    use crate::{
        domain::{
            CapturedClipboard, ClipboardRepresentation, ContentIdentity, RetentionPeriod,
            SourceIdentity, UserSettings,
        },
        platform::windows::clipboard::{ClipboardEvent, latest_clipboard_event_channel},
        services::persistence::{PersistenceWorker, RecordPersistence, SqliteRepository},
        services::session_records::SessionRecordStore,
    };
    use chrono::{Duration, Utc};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tempfile::tempdir;

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

    #[test]
    fn retention_update_immediately_prunes_database_and_memory() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let storage_available = Arc::new(AtomicBool::new(true));
        let persistence =
            PersistenceWorker::start(Arc::clone(&repository), Arc::clone(&storage_available))
                .unwrap();
        let records = Arc::new(SessionRecordStore::with_persistence(
            Vec::new(),
            Arc::clone(&persistence) as Arc<dyn RecordPersistence>,
            Arc::clone(&storage_available),
        ));
        let now = Utc::now();
        for (identity, captured_at) in [
            ("expired", now - Duration::days(8)),
            ("current", now - Duration::days(2)),
        ] {
            assert!(records.capture(CapturedClipboard {
                content_identity: ContentIdentity::new(identity),
                captured_at,
                source: SourceIdentity::default(),
                representations: vec![ClipboardRepresentation::UnicodeText {
                    text: identity.to_owned(),
                }],
            }));
        }
        let settings = ApplicationSettings {
            current: Mutex::new(UserSettings {
                retention: RetentionPeriod::Forever,
                ..UserSettings::default()
            }),
            persistence: Some(persistence),
            records: Arc::clone(&records),
            storage_available: Arc::clone(&storage_available),
            hotkey_status: Mutex::new(HotkeyStatus::Unavailable),
        };

        settings.update_retention(RetentionPeriod::SevenDays);

        assert_eq!(records.list().len(), 1);
        assert_eq!(repository.load_recent(10).unwrap().len(), 1);
        assert!(storage_available.load(Ordering::Acquire));
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
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> Result<SessionRecordView, String> {
    let result = SessionRecordCommands::new(records.inner())
        .update_note(record_id, note)
        .map_err(|error| error.to_string());
    let _ = app.emit("settings-changed", settings.view());
    result
}

#[cfg(windows)]
#[tauri::command]
fn get_settings(settings: tauri::State<'_, Arc<ApplicationSettings>>) -> SettingsView {
    settings.view()
}

#[cfg(windows)]
#[tauri::command]
fn update_language(
    language: Language,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> SettingsView {
    let view = settings.update_language(language);
    let _ = app.emit("settings-changed", view);
    view
}

#[cfg(windows)]
#[tauri::command]
fn update_retention(
    retention: RetentionPeriod,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> SettingsView {
    let view = settings.update_retention(retention);
    let _ = app.emit("settings-changed", view);
    let _ = app.emit("clipboard-records-changed", ());
    view
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
    let builder = tauri::Builder::default();
    #[cfg(windows)]
    let builder = builder
        .setup(|app| {
            let panel = app
                .get_webview_window("quick-panel")
                .ok_or_else(|| "quick-panel window is missing".to_owned())?;
            platform::windows::configure_quick_panel_style(panel.hwnd()?)?;
            let panel_controller = Arc::new(PanelController::new(panel));
            let storage_available = Arc::new(AtomicBool::new(false));
            let (repository, user_settings, loaded_records) = app
                .path()
                .app_data_dir()
                .ok()
                .and_then(|directory| SqliteRepository::open_app_data(&directory).ok())
                .and_then(|repository| {
                    let settings = repository.load_settings().ok()?;
                    repository
                        .prune(settings.retention, chrono::Utc::now())
                        .ok()?;
                    let records = repository
                        .load_recent(services::session_records::MAX_SESSION_RECORDS)
                        .ok()?;
                    Some((repository, settings, records))
                })
                .map_or(
                    (None, UserSettings::default(), Vec::new()),
                    |(repository, settings, records)| {
                        storage_available.store(true, Ordering::Release);
                        (Some(repository), settings, records)
                    },
                );
            let persistence = repository.and_then(|repository| {
                PersistenceWorker::start(repository, Arc::clone(&storage_available)).ok()
            });
            if persistence.is_none() {
                storage_available.store(false, Ordering::Release);
            }
            let session_records = match &persistence {
                Some(persistence) => Arc::new(SessionRecordStore::with_persistence(
                    loaded_records,
                    Arc::clone(persistence) as Arc<dyn services::persistence::RecordPersistence>,
                    Arc::clone(&storage_available),
                )),
                None => Arc::new(SessionRecordStore::with_loaded(loaded_records)),
            };
            let (clipboard_events, clipboard_receiver) =
                platform::windows::clipboard::latest_clipboard_event_channel(Arc::clone(
                    &session_records,
                ));
            let listener =
                platform::windows::clipboard::ClipboardListener::start(clipboard_events)?;
            let settings_state = Arc::new(ApplicationSettings {
                current: Mutex::new(user_settings),
                persistence,
                records: Arc::clone(&session_records),
                storage_available,
                hotkey_status: Mutex::new(HotkeyStatus::Unavailable),
            });
            let app_handle = app.handle().clone();
            let settings_for_drain = Arc::clone(&settings_state);
            let (event_drain_stop, event_drain) =
                spawn_clipboard_event_drain(clipboard_receiver, move || {
                    settings_for_drain.prune_expired();
                    let _ = app_handle.emit("clipboard-records-changed", ());
                    let _ = app_handle.emit("settings-changed", settings_for_drain.view());
                })?;
            let paste = SafePasteService::new(
                platform::windows::paste::Win32PasteTarget::new(),
                listener.publisher(),
                Arc::clone(&panel_controller),
                platform::windows::paste::Win32PasteInput,
            );
            let coordinator = Arc::new(QuickPanelPasteCoordinator::new(
                Arc::clone(&panel_controller),
                paste,
            ));
            let hotkey_app = app.handle().clone();
            let hotkey_coordinator = Arc::clone(&coordinator);
            let hotkey = platform::windows::hotkey::GlobalHotkey::start(move || {
                let coordinator = Arc::clone(&hotkey_coordinator);
                let _ = hotkey_app.run_on_main_thread(move || {
                    let _ = coordinator.toggle();
                });
            });
            *lock_unpoisoned(&settings_state.hotkey_status) = match &hotkey {
                Ok(_) => HotkeyStatus::Available,
                Err(platform::windows::hotkey::HotkeyError::Conflict) => HotkeyStatus::Conflict,
                Err(_) => HotkeyStatus::Unavailable,
            };
            app.manage(Arc::clone(&panel_controller));
            app.manage(session_records);
            app.manage(coordinator);
            app.manage(Arc::clone(&settings_state));
            app.manage(HotkeyRuntime {
                hotkey: Mutex::new(hotkey.ok()),
            });
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
            update_record_note,
            get_settings,
            update_language,
            update_retention
        ]);
    #[cfg(not(windows))]
    let builder = builder.invoke_handler(tauri::generate_handler![greet]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(all(test, windows))]
mod desktop_configuration_tests {
    #[test]
    fn dpi_awareness_is_manifest_owned_and_runtime_does_not_reset_it() {
        let manifest = include_str!("../windows-app-manifest.xml");
        let library = include_str!("lib.rs");
        let platform = include_str!("platform/windows/mod.rs");

        assert!(manifest.contains("PerMonitorV2,PerMonitor"));
        let runtime_api = ["SetProcessDpi", "AwarenessContext"].concat();
        assert!(!library.contains(&runtime_api));
        assert!(!platform.contains(&runtime_api));
    }

    #[test]
    fn windows_binary_is_always_a_gui_subsystem() {
        let main = include_str!("main.rs");

        assert!(main.contains("cfg_attr(windows, windows_subsystem = \"windows\")"));
        assert!(!main.contains("not(debug_assertions)"));
    }
}
