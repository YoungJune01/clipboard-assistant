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
use services::persistence::{
    PersistenceMutationCoordinator, PersistenceWorker, RestoreBudget, SqliteRepository,
};
#[cfg(windows)]
use services::session_records::{
    HistoryPageView, ImagePreviewView, RecordDetailsView, SessionRecordCommands,
    SessionRecordStore, SessionRecordView,
};

#[cfg(windows)]
use tauri::{Emitter, Manager};

#[cfg(windows)]
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[cfg(windows)]
use domain::{
    AccentColor, CaptureSound, Language, RetentionPeriod, Shortcut, ShortcutKey, ShortcutModifiers,
    UserSettings,
};

#[cfg(windows)]
use serde::Serialize;

#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClipboardGroupView {
    id: domain::GroupId,
    name: String,
}

#[cfg(windows)]
struct ClipboardGroups {
    groups: Mutex<Vec<ClipboardGroupView>>,
    repository: Option<Arc<SqliteRepository>>,
    mutation_coordinator: Arc<PersistenceMutationCoordinator>,
}

#[cfg(windows)]
impl ClipboardGroups {
    fn list(&self) -> Vec<ClipboardGroupView> {
        lock_unpoisoned(&self.groups).clone()
    }

    fn create(&self, name: String) -> Result<ClipboardGroupView, String> {
        let name = validate_group_name(name)?;
        let _coordinated = self.mutation_coordinator.lock();
        let mut groups = lock_unpoisoned(&self.groups);
        if groups
            .iter()
            .any(|group| group.name.eq_ignore_ascii_case(&name))
        {
            return Err("clipboard group name already exists".to_owned());
        }
        let group = ClipboardGroupView {
            id: domain::GroupId::new(),
            name,
        };
        let repository = self
            .repository
            .as_ref()
            .ok_or_else(|| "clipboard group was not saved to local storage".to_owned())?;
        repository
            .save_group(group.id, &group.name)
            .map_err(|error| error.to_string())?;
        groups.push(group.clone());
        Ok(group)
    }

    fn rename(&self, id: domain::GroupId, name: String) -> Result<ClipboardGroupView, String> {
        let name = validate_group_name(name)?;
        let _coordinated = self.mutation_coordinator.lock();
        let mut groups = lock_unpoisoned(&self.groups);
        if groups
            .iter()
            .any(|group| group.id != id && group.name.eq_ignore_ascii_case(&name))
        {
            return Err("clipboard group name already exists".to_owned());
        }
        let group = groups
            .iter_mut()
            .find(|group| group.id == id)
            .ok_or_else(|| "clipboard group is no longer available".to_owned())?;
        let repository = self
            .repository
            .as_ref()
            .ok_or_else(|| "clipboard group was not saved to local storage".to_owned())?;
        repository
            .save_group(id, &name)
            .map_err(|error| error.to_string())?;
        group.name = name;
        Ok(group.clone())
    }

    fn move_group(
        &self,
        id: domain::GroupId,
        direction: i8,
    ) -> Result<Vec<ClipboardGroupView>, String> {
        let _coordinated = self.mutation_coordinator.lock();
        let mut groups = lock_unpoisoned(&self.groups);
        let index = groups
            .iter()
            .position(|group| group.id == id)
            .ok_or_else(|| "clipboard group is no longer available".to_owned())?;
        let target = if direction < 0 {
            index.checked_sub(1)
        } else if direction > 0 && index + 1 < groups.len() {
            Some(index + 1)
        } else {
            None
        };
        let Some(target) = target else {
            return Ok(groups.clone());
        };
        groups.swap(index, target);
        let repository = self
            .repository
            .as_ref()
            .ok_or_else(|| "clipboard group order was not saved to local storage".to_owned())?;
        if let Err(error) =
            repository.save_group_order(&groups.iter().map(|group| group.id).collect::<Vec<_>>())
        {
            groups.swap(index, target);
            return Err(error.to_string());
        }
        Ok(groups.clone())
    }

    fn delete_coordinated(&self, id: domain::GroupId) -> Result<(), String> {
        let mut groups = lock_unpoisoned(&self.groups);
        let index = groups
            .iter()
            .position(|group| group.id == id)
            .ok_or_else(|| "clipboard group is no longer available".to_owned())?;
        let repository = self
            .repository
            .as_ref()
            .ok_or_else(|| "clipboard group was not deleted from local storage".to_owned())?;
        repository
            .delete_group(id)
            .map_err(|error| error.to_string())?;
        groups.remove(index);
        Ok(())
    }

    fn contains_coordinated(&self, id: domain::GroupId) -> bool {
        lock_unpoisoned(&self.groups)
            .iter()
            .any(|group| group.id == id)
    }

    fn replace_all_coordinated(&self, groups: Vec<(domain::GroupId, String)>) {
        *lock_unpoisoned(&self.groups) = groups
            .into_iter()
            .map(|(id, name)| ClipboardGroupView { id, name })
            .collect();
    }
}

#[cfg(windows)]
fn validate_group_name(name: String) -> Result<String, String> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.contains(['\r', '\n']) || name.chars().count() > 30 {
        return Err("clipboard group name is invalid".to_owned());
    }
    Ok(name)
}

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
    retention_stop: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    retention: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[cfg(windows)]
struct HotkeyRuntime {
    activation: Mutex<Option<platform::windows::hotkey::GlobalHotkey>>,
    groups: Mutex<Option<platform::windows::hotkey::GroupHotkeys>>,
    quick_paste: Mutex<Option<platform::windows::hotkey::QuickPasteHotkeys>>,
}

#[cfg(windows)]
impl Drop for HotkeyRuntime {
    fn drop(&mut self) {
        drop(
            self.activation
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        drop(
            self.groups
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
        drop(
            self.quick_paste
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ActiveGroup {
    #[default]
    All,
    Ungrouped,
    Group(domain::GroupId),
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ActiveGroupView {
    kind: &'static str,
    group_id: Option<domain::GroupId>,
}

#[cfg(windows)]
impl From<ActiveGroup> for ActiveGroupView {
    fn from(active: ActiveGroup) -> Self {
        match active {
            ActiveGroup::All => Self {
                kind: "all",
                group_id: None,
            },
            ActiveGroup::Ungrouped => Self {
                kind: "ungrouped",
                group_id: None,
            },
            ActiveGroup::Group(group_id) => Self {
                kind: "group",
                group_id: Some(group_id),
            },
        }
    }
}

#[cfg(windows)]
struct ActiveGroupState(Mutex<ActiveGroup>);

#[cfg(windows)]
impl ActiveGroupState {
    fn current(&self) -> ActiveGroup {
        *lock_unpoisoned(&self.0)
    }

    fn set(&self, active: ActiveGroup) {
        *lock_unpoisoned(&self.0) = active;
    }

    fn switch(&self, direction: i8, groups: &[ClipboardGroupView]) -> ActiveGroup {
        let mut choices = Vec::with_capacity(groups.len() + 2);
        choices.push(ActiveGroup::All);
        choices.push(ActiveGroup::Ungrouped);
        choices.extend(groups.iter().map(|group| ActiveGroup::Group(group.id)));
        let current = self.current();
        let index = choices
            .iter()
            .position(|choice| *choice == current)
            .unwrap_or(0);
        let next = if direction < 0 {
            index.checked_sub(1).unwrap_or(choices.len() - 1)
        } else {
            (index + 1) % choices.len()
        };
        self.set(choices[next]);
        choices[next]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    language: Language,
    retention: RetentionPeriod,
    storage_limit: crate::domain::StorageLimit,
    evict_favorites_when_full: bool,
    start_at_sign_in: bool,
    start_minimized: bool,
    show_tray_icon: bool,
    accent_color: AccentColor,
    sound_enabled: bool,
    capture_sound: CaptureSound,
    custom_sound_available: bool,
    activation_shortcut: Shortcut,
    group_shortcut_modifiers: ShortcutModifiers,
    quick_paste_enabled: bool,
    quick_paste_modifiers: ShortcutModifiers,
    storage_available: bool,
    hotkey_status: HotkeyStatus,
    capture_paused: bool,
    excluded_applications: Vec<String>,
}

#[cfg(windows)]
struct DesktopRuntime {
    tray: TrayIcon,
    exiting: AtomicBool,
}

#[cfg(windows)]
impl DesktopRuntime {
    fn set_language(&self, language: Language) {
        let tooltip = match language {
            Language::ZhCn => "剪贴板助手",
            Language::En => "Clipboard Assistant",
        };
        let _ = self.tray.set_tooltip(Some(tooltip));
    }
}

#[cfg(windows)]
fn mark_application_exiting(exiting: &AtomicBool) {
    exiting.store(true, Ordering::Release);
}

#[cfg(windows)]
struct ApplicationSettings {
    current: Mutex<UserSettings>,
    persistence: Option<Arc<PersistenceWorker>>,
    records: Arc<SessionRecordStore>,
    storage_available: Arc<AtomicBool>,
    hotkey_status: Mutex<HotkeyStatus>,
    custom_sound_path: std::path::PathBuf,
    capture_policy: Arc<platform::windows::clipboard::CapturePolicy>,
    mutation_coordinator: Arc<PersistenceMutationCoordinator>,
}

#[cfg(windows)]
impl ApplicationSettings {
    fn view(&self) -> SettingsView {
        let current = *lock_unpoisoned(&self.current);
        SettingsView {
            language: current.language,
            retention: current.retention,
            storage_limit: current.storage_limit,
            evict_favorites_when_full: current.evict_favorites_when_full,
            start_at_sign_in: current.start_at_sign_in,
            start_minimized: current.start_minimized,
            show_tray_icon: current.show_tray_icon,
            accent_color: current.accent_color,
            sound_enabled: current.sound_enabled,
            capture_sound: current.capture_sound,
            custom_sound_available: self.custom_sound_path.is_file(),
            activation_shortcut: current.activation_shortcut,
            group_shortcut_modifiers: current.group_shortcut_modifiers,
            quick_paste_enabled: current.quick_paste_enabled,
            quick_paste_modifiers: current.quick_paste_modifiers,
            storage_available: self.storage_available.load(Ordering::Acquire),
            hotkey_status: *lock_unpoisoned(&self.hotkey_status),
            capture_paused: self.capture_policy.is_paused(),
            excluded_applications: self.capture_policy.excluded_applications(),
        }
    }

    fn update_language(&self, language: Language) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        lock_unpoisoned(&self.current).language = language;
        self.persist_settings_coordinated();
        self.view()
    }

    fn update_retention(&self, retention: RetentionPeriod) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        lock_unpoisoned(&self.current).retention = retention;
        self.persist_settings_coordinated();
        self.prune_expired_coordinated();
        self.view()
    }

    fn update_storage_policy(
        &self,
        storage_limit: crate::domain::StorageLimit,
        evict_favorites_when_full: bool,
    ) -> Result<(SettingsView, bool), String> {
        let _coordinated = self.mutation_coordinator.lock();
        let current = *lock_unpoisoned(&self.current);
        let candidate = UserSettings {
            storage_limit,
            evict_favorites_when_full,
            ..current
        };
        let persistence = self
            .persistence
            .as_ref()
            .ok_or_else(|| "local clipboard storage is unavailable".to_owned())?;
        let removed = persistence
            .update_storage_policy(storage_limit, evict_favorites_when_full)
            .map_err(|error| error.to_string())?;
        *lock_unpoisoned(&self.current) = candidate;
        if removed > 0 {
            self.records.refresh_after_storage_maintenance_coordinated();
        }
        Ok((self.view(), removed > 0))
    }

    fn update_start_at_sign_in(&self, enabled: bool) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        lock_unpoisoned(&self.current).start_at_sign_in = enabled;
        self.persist_settings_coordinated();
        self.view()
    }

    fn update_start_minimized(&self, enabled: bool) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        lock_unpoisoned(&self.current).start_minimized = enabled;
        self.persist_settings_coordinated();
        self.view()
    }

    fn update_show_tray_icon(&self, enabled: bool) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        let mut current = lock_unpoisoned(&self.current);
        current.show_tray_icon = enabled;
        if !enabled {
            current.start_minimized = false;
        }
        drop(current);
        self.persist_settings_coordinated();
        self.view()
    }

    fn update_accent_color(&self, accent_color: AccentColor) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        lock_unpoisoned(&self.current).accent_color = accent_color;
        self.persist_settings_coordinated();
        self.view()
    }

    fn update_sound_enabled(&self, enabled: bool) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        lock_unpoisoned(&self.current).sound_enabled = enabled;
        self.persist_settings_coordinated();
        self.view()
    }

    fn update_capture_sound(&self, capture_sound: CaptureSound) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        lock_unpoisoned(&self.current).capture_sound = capture_sound;
        self.persist_settings_coordinated();
        self.view()
    }

    fn update_shortcuts(
        &self,
        activation: Shortcut,
        group_modifiers: ShortcutModifiers,
        quick_paste_enabled: bool,
        quick_paste_modifiers: ShortcutModifiers,
    ) -> SettingsView {
        let _coordinated = self.mutation_coordinator.lock();
        let mut current = lock_unpoisoned(&self.current);
        current.activation_shortcut = activation;
        current.group_shortcut_modifiers = group_modifiers;
        current.quick_paste_enabled = quick_paste_enabled;
        current.quick_paste_modifiers = quick_paste_modifiers;
        drop(current);
        self.persist_settings_coordinated();
        self.view()
    }

    fn replace_current(&self, settings: UserSettings) -> SettingsView {
        *lock_unpoisoned(&self.current) = settings;
        self.view()
    }

    fn current(&self) -> UserSettings {
        *lock_unpoisoned(&self.current)
    }

    fn play_capture_sound(&self) {
        let current = *lock_unpoisoned(&self.current);
        if !current.sound_enabled {
            return;
        }
        let result = match current.capture_sound {
            CaptureSound::Default => platform::windows::sound::play_default(),
            CaptureSound::Custom if self.custom_sound_path.is_file() => {
                platform::windows::sound::play_file(&self.custom_sound_path)
            }
            CaptureSound::Custom => platform::windows::sound::play_default(),
        };
        let _ = result;
    }

    fn prune_expired(&self) -> usize {
        let _coordinated = self.mutation_coordinator.lock();
        self.prune_expired_coordinated()
    }

    fn prune_expired_coordinated(&self) -> usize {
        let retention = lock_unpoisoned(&self.current).retention;
        let now = chrono::Utc::now();
        if let Some(persistence) = &self.persistence
            && persistence.prune(retention, now).is_err()
        {
            self.storage_available.store(false, Ordering::Release);
        }
        retention.days().map_or(0, |days| {
            self.records
                .prune_before_coordinated(now - chrono::Duration::days(days))
        })
    }

    fn persist_settings_coordinated(&self) {
        let settings = *lock_unpoisoned(&self.current);
        if let Some(persistence) = &self.persistence
            && persistence.save_settings(settings).is_err()
        {
            self.storage_available.store(false, Ordering::Release);
        }
    }

    fn update_capture_paused(&self, paused: bool) -> SettingsView {
        self.capture_policy.set_paused(paused);
        self.view()
    }

    fn update_excluded_applications(
        &self,
        applications: Vec<String>,
    ) -> Result<SettingsView, String> {
        let applications = normalize_excluded_applications(applications)?;
        let _coordinated = self.mutation_coordinator.lock();
        if let Some(persistence) = &self.persistence {
            persistence
                .save_excluded_applications(&applications)
                .map_err(|error| error.to_string())?;
        } else {
            return Err("local clipboard storage is unavailable".to_owned());
        }
        self.capture_policy.set_excluded_applications(applications);
        Ok(self.view())
    }
}

#[cfg(windows)]
fn normalize_excluded_applications(applications: Vec<String>) -> Result<Vec<String>, String> {
    if applications.len() > 50 {
        return Err("too_many_excluded_applications".to_owned());
    }
    let mut normalized = Vec::new();
    for application in applications {
        let trimmed = application.trim();
        let name = std::path::Path::new(trimmed)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(trimmed)
            .trim();
        if name.is_empty() || name.chars().count() > 128 || name.contains(['/', '\\']) {
            return Err("invalid_excluded_application".to_owned());
        }
        if !normalized
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(name))
        {
            normalized.push(name.to_owned());
        }
    }
    Ok(normalized)
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
        if let Some(stop) = self
            .retention_stop
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = stop.send(());
        }
        if let Some(retention) = self
            .retention
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = retention.join();
        }
    }
}

#[cfg(windows)]
fn spawn_periodic_retention(
    interval: std::time::Duration,
    on_tick: impl Fn() + Send + 'static,
) -> std::io::Result<(std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>)> {
    let (stop, stopped) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("clipboard-retention".to_owned())
        .spawn(move || {
            loop {
                match stopped.recv_timeout(interval) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => on_tick(),
                }
            }
        })?;
    Ok((stop, thread))
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
    use super::{
        ActiveGroup, ActiveGroupState, ApplicationSettings, ClipboardGroupView, ClipboardGroups,
        HotkeyStatus, create_text_record_state, delete_clipboard_group_state,
        spawn_clipboard_event_drain, spawn_periodic_retention, update_record_group_state,
    };
    use crate::{
        domain::{
            CapturedClipboard, ClipboardRepresentation, ContentIdentity, RetentionPeriod,
            SourceIdentity, StorageLimit, UserSettings,
        },
        platform::windows::clipboard::{ClipboardEvent, latest_clipboard_event_channel},
        services::persistence::{
            PersistenceMutationCoordinator, PersistenceWorker, RecordPersistence, RestoreBudget,
            SqliteRepository,
        },
        services::session_records::{
            DEFAULT_STORE_BYTES, MAX_CAPTURE_RECORD_BYTES, MAX_SESSION_RECORDS, SessionRecordStore,
        },
    };
    use chrono::{Duration, Utc};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tempfile::tempdir;

    #[test]
    fn active_group_switching_wraps_in_both_directions() {
        let group = ClipboardGroupView {
            id: crate::domain::GroupId::new(),
            name: "Accounts".to_owned(),
        };
        let active = ActiveGroupState(Mutex::new(ActiveGroup::All));

        assert_eq!(
            active.switch(-1, std::slice::from_ref(&group)),
            ActiveGroup::Group(group.id)
        );
        assert_eq!(
            active.switch(1, std::slice::from_ref(&group)),
            ActiveGroup::All
        );
        assert_eq!(
            active.switch(1, std::slice::from_ref(&group)),
            ActiveGroup::Ungrouped
        );
        assert_eq!(
            active.switch(1, std::slice::from_ref(&group)),
            ActiveGroup::Group(group.id)
        );
    }

    #[test]
    fn manual_record_rejects_a_missing_group_without_persisting() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let coordinator = Arc::new(PersistenceMutationCoordinator::default());
        let records = SessionRecordStore::with_persistence_page_and_coordinator(
            repository
                .load_page(crate::domain::HistoryQuery::default())
                .unwrap(),
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
            Arc::clone(&coordinator),
        );
        let groups = ClipboardGroups {
            groups: Mutex::new(Vec::new()),
            repository: Some(Arc::clone(&repository)),
            mutation_coordinator: coordinator,
        };

        assert!(
            create_text_record_state(
                "secret".to_owned(),
                Some("account".to_owned()),
                Some(crate::domain::GroupId::new()),
                &groups,
                &records,
            )
            .is_err()
        );
        assert!(
            repository
                .load_page(crate::domain::HistoryQuery::default())
                .unwrap()
                .records
                .is_empty()
        );
    }

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
            custom_sound_path: std::path::PathBuf::new(),
            capture_policy: Arc::new(crate::platform::windows::clipboard::CapturePolicy::default()),
            mutation_coordinator: Arc::new(PersistenceMutationCoordinator::default()),
        };

        settings.update_retention(RetentionPeriod::SevenDays);

        assert_eq!(records.list().len(), 1);
        assert_eq!(repository.load_recent(10).unwrap().len(), 1);
        assert!(storage_available.load(Ordering::Acquire));
    }

    #[test]
    fn storage_policy_failure_does_not_publish_runtime_settings() {
        let original = UserSettings::default();
        let settings = ApplicationSettings {
            current: Mutex::new(original),
            persistence: None,
            records: Arc::new(SessionRecordStore::default()),
            storage_available: Arc::new(AtomicBool::new(false)),
            hotkey_status: Mutex::new(HotkeyStatus::Unavailable),
            custom_sound_path: std::path::PathBuf::new(),
            capture_policy: Arc::new(crate::platform::windows::clipboard::CapturePolicy::default()),
            mutation_coordinator: Arc::new(PersistenceMutationCoordinator::default()),
        };

        assert!(
            settings
                .update_storage_policy(StorageLimit::FiveGb, true)
                .is_err()
        );
        assert_eq!(settings.current(), original);
    }

    #[test]
    fn restore_serializes_retention_and_prunes_using_the_restored_policy() {
        let directory = tempdir().unwrap();
        let live = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let source_path = directory.path().join("source.clipbackup");
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let restored_record = crate::domain::ClipboardRecord::from_capture(CapturedClipboard {
            content_identity: ContentIdentity::new("restored-old-record"),
            captured_at: Utc::now() - Duration::days(30),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: "keep forever".to_owned(),
            }],
        });
        source.save_record(&restored_record).unwrap();
        source
            .save_settings(UserSettings {
                retention: RetentionPeriod::Forever,
                ..UserSettings::default()
            })
            .unwrap();
        drop(source);

        let storage_available = Arc::new(AtomicBool::new(true));
        let persistence =
            PersistenceWorker::start(Arc::clone(&live), Arc::clone(&storage_available)).unwrap();
        let mutation_coordinator = Arc::new(PersistenceMutationCoordinator::default());
        let records = Arc::new(SessionRecordStore::with_persistence_page_and_coordinator(
            live.load_page(crate::domain::HistoryQuery::default())
                .unwrap(),
            Arc::clone(&persistence) as Arc<dyn RecordPersistence>,
            Arc::clone(&storage_available),
            Arc::clone(&mutation_coordinator),
        ));
        let settings = Arc::new(ApplicationSettings {
            current: Mutex::new(UserSettings {
                retention: RetentionPeriod::OneDay,
                ..UserSettings::default()
            }),
            persistence: Some(Arc::clone(&persistence)),
            records: Arc::clone(&records),
            storage_available,
            hotkey_status: Mutex::new(HotkeyStatus::Unavailable),
            custom_sound_path: std::path::PathBuf::new(),
            capture_policy: Arc::new(crate::platform::windows::clipboard::CapturePolicy::default()),
            mutation_coordinator,
        });
        let (restored_tx, restored_rx) = std::sync::mpsc::sync_channel(1);
        let (publish_tx, publish_rx) = std::sync::mpsc::sync_channel(1);
        let restoring = {
            let records = Arc::clone(&records);
            let settings = Arc::clone(&settings);
            let persistence = Arc::clone(&persistence);
            std::thread::spawn(move || {
                records.with_restore_guard(|guard| {
                    let restored = persistence
                        .restore(
                            source_path,
                            RestoreBudget {
                                max_records: MAX_SESSION_RECORDS,
                                max_total_bytes: DEFAULT_STORE_BYTES,
                                max_record_bytes: MAX_CAPTURE_RECORD_BYTES,
                            },
                        )
                        .unwrap();
                    restored_tx.send(()).unwrap();
                    publish_rx.recv().unwrap();
                    guard.apply_page(crate::domain::HistoryQuery::default(), restored.page);
                    settings.replace_current(restored.settings);
                });
            })
        };
        restored_rx.recv().unwrap();
        let (pruned_tx, pruned_rx) = std::sync::mpsc::sync_channel(1);
        let pruning = {
            let settings = Arc::clone(&settings);
            std::thread::spawn(move || pruned_tx.send(settings.prune_expired()).unwrap())
        };

        assert!(
            pruned_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        publish_tx.send(()).unwrap();
        restoring.join().unwrap();
        assert_eq!(pruned_rx.recv().unwrap(), 0);
        pruning.join().unwrap();

        assert_eq!(settings.current().retention, RetentionPeriod::Forever);
        assert_eq!(
            live.full_record(restored_record.id).unwrap(),
            restored_record
        );
        assert!(
            records
                .list()
                .iter()
                .any(|view| view.id == restored_record.id)
        );
    }

    #[test]
    fn restore_serializes_group_mutations_against_database_and_runtime_publication() {
        let directory = tempdir().unwrap();
        let live = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let source_path = directory.path().join("source.clipbackup");
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let restored_group = crate::domain::GroupId::new();
        source.save_group(restored_group, "Restored").unwrap();
        drop(source);

        let storage_available = Arc::new(AtomicBool::new(true));
        let persistence =
            PersistenceWorker::start(Arc::clone(&live), Arc::clone(&storage_available)).unwrap();
        let mutation_coordinator = Arc::new(PersistenceMutationCoordinator::default());
        let records = Arc::new(SessionRecordStore::with_persistence_page_and_coordinator(
            live.load_page(crate::domain::HistoryQuery::default())
                .unwrap(),
            Arc::clone(&persistence) as Arc<dyn RecordPersistence>,
            storage_available,
            Arc::clone(&mutation_coordinator),
        ));
        let groups = Arc::new(ClipboardGroups {
            groups: Mutex::new(Vec::new()),
            repository: Some(Arc::clone(&live)),
            mutation_coordinator,
        });
        let (restored_tx, restored_rx) = std::sync::mpsc::sync_channel(1);
        let (publish_tx, publish_rx) = std::sync::mpsc::sync_channel(1);
        let restoring = {
            let records = Arc::clone(&records);
            let groups = Arc::clone(&groups);
            let persistence = Arc::clone(&persistence);
            std::thread::spawn(move || {
                records.with_restore_guard(|guard| {
                    let restored = persistence
                        .restore(
                            source_path,
                            RestoreBudget {
                                max_records: MAX_SESSION_RECORDS,
                                max_total_bytes: DEFAULT_STORE_BYTES,
                                max_record_bytes: MAX_CAPTURE_RECORD_BYTES,
                            },
                        )
                        .unwrap();
                    restored_tx.send(()).unwrap();
                    publish_rx.recv().unwrap();
                    guard.apply_page(crate::domain::HistoryQuery::default(), restored.page);
                    groups.replace_all_coordinated(restored.groups);
                });
            })
        };
        restored_rx.recv().unwrap();
        let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
        let creating = {
            let groups = Arc::clone(&groups);
            std::thread::spawn(move || {
                created_tx
                    .send(groups.create("Concurrent".to_owned()))
                    .unwrap()
            })
        };

        assert!(
            created_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        publish_tx.send(()).unwrap();
        restoring.join().unwrap();
        let created = created_rx.recv().unwrap().unwrap();
        creating.join().unwrap();

        let runtime_groups = groups.list();
        let persisted_groups = live.load_groups().unwrap();
        assert!(
            runtime_groups
                .iter()
                .any(|group| group.id == restored_group)
        );
        assert!(runtime_groups.iter().any(|group| group.id == created.id));
        assert_eq!(
            runtime_groups
                .iter()
                .map(|group| (group.id, group.name.clone()))
                .collect::<Vec<_>>(),
            persisted_groups
        );
    }

    #[test]
    fn group_delete_waits_for_restore_and_clears_database_page_cache_and_active_group() {
        let directory = tempdir().unwrap();
        let live = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let source_path = directory.path().join("source.clipbackup");
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let group_id = crate::domain::GroupId::new();
        source.save_group(group_id, "Restored").unwrap();
        let mut restored_record = crate::domain::ClipboardRecord::from_capture(CapturedClipboard {
            content_identity: ContentIdentity::new("restored-group-record"),
            captured_at: Utc::now(),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: "grouped".to_owned(),
            }],
        });
        restored_record.group_id = Some(group_id);
        source.save_record(&restored_record).unwrap();
        drop(source);

        let storage_available = Arc::new(AtomicBool::new(true));
        let persistence =
            PersistenceWorker::start(Arc::clone(&live), Arc::clone(&storage_available)).unwrap();
        let coordinator = Arc::new(PersistenceMutationCoordinator::default());
        let records = Arc::new(SessionRecordStore::with_persistence_page_and_coordinator(
            live.load_page(crate::domain::HistoryQuery::default())
                .unwrap(),
            Arc::clone(&persistence) as Arc<dyn RecordPersistence>,
            storage_available,
            Arc::clone(&coordinator),
        ));
        let groups = Arc::new(ClipboardGroups {
            groups: Mutex::new(Vec::new()),
            repository: Some(Arc::clone(&live)),
            mutation_coordinator: coordinator,
        });
        let active = Arc::new(ActiveGroupState(Mutex::new(ActiveGroup::All)));
        let (restored_tx, restored_rx) = std::sync::mpsc::sync_channel(1);
        let (publish_tx, publish_rx) = std::sync::mpsc::sync_channel(1);
        let restoring = {
            let records = Arc::clone(&records);
            let groups = Arc::clone(&groups);
            let active = Arc::clone(&active);
            let persistence = Arc::clone(&persistence);
            std::thread::spawn(move || {
                records.with_restore_guard(|guard| {
                    let restored = persistence
                        .restore(
                            source_path,
                            RestoreBudget {
                                max_records: MAX_SESSION_RECORDS,
                                max_total_bytes: DEFAULT_STORE_BYTES,
                                max_record_bytes: MAX_CAPTURE_RECORD_BYTES,
                            },
                        )
                        .unwrap();
                    restored_tx.send(()).unwrap();
                    publish_rx.recv().unwrap();
                    guard.apply_page(crate::domain::HistoryQuery::default(), restored.page);
                    groups.replace_all_coordinated(restored.groups);
                    active.set(ActiveGroup::Group(group_id));
                });
            })
        };
        restored_rx.recv().unwrap();
        let (deleted_tx, deleted_rx) = std::sync::mpsc::sync_channel(1);
        let deleting = {
            let groups = Arc::clone(&groups);
            let records = Arc::clone(&records);
            let active = Arc::clone(&active);
            std::thread::spawn(move || {
                deleted_tx
                    .send(delete_clipboard_group_state(
                        group_id, &groups, &records, &active,
                    ))
                    .unwrap()
            })
        };

        assert!(
            deleted_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        publish_tx.send(()).unwrap();
        restoring.join().unwrap();
        assert!(deleted_rx.recv().unwrap().unwrap());
        deleting.join().unwrap();

        assert!(groups.list().is_empty());
        assert_eq!(active.current(), ActiveGroup::Ungrouped);
        assert!(live.load_groups().unwrap().is_empty());
        assert_eq!(live.full_record(restored_record.id).unwrap().group_id, None);
        assert_eq!(
            records
                .list()
                .iter()
                .find(|view| view.id == restored_record.id)
                .and_then(|view| view.group_id),
            None
        );
    }

    #[test]
    fn record_group_update_waits_for_restore_and_rejects_a_removed_group() {
        let directory = tempdir().unwrap();
        let live = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let stale_group = crate::domain::GroupId::new();
        live.save_group(stale_group, "Before restore").unwrap();
        let source_path = directory.path().join("source.clipbackup");
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let restored_record = crate::domain::ClipboardRecord::from_capture(CapturedClipboard {
            content_identity: ContentIdentity::new("restored-ungrouped-record"),
            captured_at: Utc::now(),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: "ungrouped".to_owned(),
            }],
        });
        source.save_record(&restored_record).unwrap();
        drop(source);

        let storage_available = Arc::new(AtomicBool::new(true));
        let persistence =
            PersistenceWorker::start(Arc::clone(&live), Arc::clone(&storage_available)).unwrap();
        let coordinator = Arc::new(PersistenceMutationCoordinator::default());
        let records = Arc::new(SessionRecordStore::with_persistence_page_and_coordinator(
            live.load_page(crate::domain::HistoryQuery::default())
                .unwrap(),
            Arc::clone(&persistence) as Arc<dyn RecordPersistence>,
            storage_available,
            Arc::clone(&coordinator),
        ));
        let groups = Arc::new(ClipboardGroups {
            groups: Mutex::new(vec![ClipboardGroupView {
                id: stale_group,
                name: "Before restore".to_owned(),
            }]),
            repository: Some(Arc::clone(&live)),
            mutation_coordinator: coordinator,
        });
        let (restored_tx, restored_rx) = std::sync::mpsc::sync_channel(1);
        let (publish_tx, publish_rx) = std::sync::mpsc::sync_channel(1);
        let restoring = {
            let records = Arc::clone(&records);
            let groups = Arc::clone(&groups);
            let persistence = Arc::clone(&persistence);
            std::thread::spawn(move || {
                records.with_restore_guard(|guard| {
                    let restored = persistence
                        .restore(
                            source_path,
                            RestoreBudget {
                                max_records: MAX_SESSION_RECORDS,
                                max_total_bytes: DEFAULT_STORE_BYTES,
                                max_record_bytes: MAX_CAPTURE_RECORD_BYTES,
                            },
                        )
                        .unwrap();
                    restored_tx.send(()).unwrap();
                    publish_rx.recv().unwrap();
                    guard.apply_page(crate::domain::HistoryQuery::default(), restored.page);
                    groups.replace_all_coordinated(restored.groups);
                });
            })
        };
        restored_rx.recv().unwrap();
        let (updated_tx, updated_rx) = std::sync::mpsc::sync_channel(1);
        let updating = {
            let groups = Arc::clone(&groups);
            let records = Arc::clone(&records);
            std::thread::spawn(move || {
                updated_tx
                    .send(update_record_group_state(
                        restored_record.id,
                        Some(stale_group),
                        &groups,
                        &records,
                    ))
                    .unwrap()
            })
        };

        assert!(
            updated_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );
        publish_tx.send(()).unwrap();
        restoring.join().unwrap();
        assert!(updated_rx.recv().unwrap().is_err());
        updating.join().unwrap();

        assert!(!groups.contains_coordinated(stale_group));
        assert_eq!(live.full_record(restored_record.id).unwrap().group_id, None);
        assert_eq!(
            records
                .list()
                .iter()
                .find(|view| view.id == restored_record.id)
                .and_then(|view| view.group_id),
            None
        );
    }

    #[test]
    fn periodic_retention_runs_while_idle_and_stops_promptly() {
        let (ticks, observed) = std::sync::mpsc::channel();
        let (stop, thread) =
            spawn_periodic_retention(std::time::Duration::from_millis(10), move || {
                let _ = ticks.send(());
            })
            .unwrap();

        observed
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        stop.send(()).unwrap();
        thread.join().unwrap();
    }

    #[test]
    fn hiding_the_tray_also_disables_start_minimized() {
        let settings = ApplicationSettings {
            current: Mutex::new(UserSettings {
                start_minimized: true,
                show_tray_icon: true,
                ..UserSettings::default()
            }),
            persistence: None,
            records: Arc::new(SessionRecordStore::default()),
            storage_available: Arc::new(AtomicBool::new(false)),
            hotkey_status: Mutex::new(HotkeyStatus::Unavailable),
            custom_sound_path: std::path::PathBuf::new(),
            capture_policy: Arc::new(crate::platform::windows::clipboard::CapturePolicy::default()),
            mutation_coordinator: Arc::new(PersistenceMutationCoordinator::default()),
        };

        let view = settings.update_show_tray_icon(false);

        assert!(!view.show_tray_icon);
        assert!(!view.start_minimized);
    }

    #[test]
    fn periodic_retention_prunes_expired_memory_without_clipboard_activity() {
        let records = Arc::new(SessionRecordStore::default());
        assert!(records.capture(CapturedClipboard {
            content_identity: ContentIdentity::new("idle-expired"),
            captured_at: Utc::now() - Duration::days(2),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: "expired".to_owned(),
            }],
        }));
        let settings = Arc::new(ApplicationSettings {
            current: Mutex::new(UserSettings {
                retention: RetentionPeriod::OneDay,
                ..UserSettings::default()
            }),
            persistence: None,
            records: Arc::clone(&records),
            storage_available: Arc::new(AtomicBool::new(false)),
            hotkey_status: Mutex::new(HotkeyStatus::Unavailable),
            custom_sound_path: std::path::PathBuf::new(),
            capture_policy: Arc::new(crate::platform::windows::clipboard::CapturePolicy::default()),
            mutation_coordinator: Arc::new(PersistenceMutationCoordinator::default()),
        });
        let (pruned, observed) = std::sync::mpsc::channel();
        let settings_for_tick = Arc::clone(&settings);
        let (stop, thread) =
            spawn_periodic_retention(std::time::Duration::from_millis(10), move || {
                let _ = pruned.send(settings_for_tick.prune_expired());
            })
            .unwrap();

        assert_eq!(
            observed
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            1
        );
        stop.send(()).unwrap();
        thread.join().unwrap();
        assert!(records.list().is_empty());
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
fn list_history_page(
    query: domain::HistoryQuery,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
) -> Result<HistoryPageView, String> {
    SessionRecordCommands::new(records.inner())
        .history_page(query)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn get_record_details(
    record_id: domain::RecordId,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
) -> Result<RecordDetailsView, String> {
    SessionRecordCommands::new(records.inner())
        .record_details(record_id)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn set_record_pinned(
    record_id: domain::RecordId,
    pinned: bool,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<RecordDetailsView, String> {
    let updated = SessionRecordCommands::new(records.inner())
        .set_pinned(record_id, pinned)
        .map_err(|error| error.to_string())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(updated)
}

#[cfg(windows)]
#[tauri::command]
fn set_record_favorite(
    record_id: domain::RecordId,
    favorite: bool,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<RecordDetailsView, String> {
    let updated = SessionRecordCommands::new(records.inner())
        .set_favorite(record_id, favorite)
        .map_err(|error| error.to_string())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(updated)
}

#[cfg(windows)]
#[tauri::command]
fn update_record_content(
    record_id: domain::RecordId,
    text: String,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<RecordDetailsView, String> {
    let updated = SessionRecordCommands::new(records.inner())
        .update_text(record_id, text)
        .map_err(|error| error.to_string())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(updated)
}

#[cfg(windows)]
#[tauri::command]
fn create_text_record(
    text: String,
    note: Option<String>,
    group_id: Option<domain::GroupId>,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<RecordDetailsView, String> {
    let created = create_text_record_state(text, note, group_id, groups.inner(), records.inner())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(created)
}

#[cfg(windows)]
fn create_text_record_state(
    text: String,
    note: Option<String>,
    group_id: Option<domain::GroupId>,
    groups: &ClipboardGroups,
    records: &SessionRecordStore,
) -> Result<RecordDetailsView, String> {
    let _coordinated = groups.mutation_coordinator.lock();
    if group_id.is_some_and(|id| !groups.contains_coordinated(id)) {
        return Err("clipboard group is no longer available".to_owned());
    }
    records
        .create_text_coordinated(text, note, group_id)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn get_record_image_preview(
    record_id: domain::RecordId,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
) -> Result<ImagePreviewView, String> {
    SessionRecordCommands::new(records.inner())
        .image_preview(record_id)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn update_record_note(
    record_id: domain::RecordId,
    note: String,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<SessionRecordView, String> {
    let updated = SessionRecordCommands::new(records.inner())
        .update_note(record_id, note)
        .map_err(|error| error.to_string())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(updated)
}

#[cfg(windows)]
#[tauri::command]
fn delete_session_record(
    record_id: domain::RecordId,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    SessionRecordCommands::new(records.inner())
        .delete(record_id)
        .map_err(|error| error.to_string())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(())
}

#[cfg(windows)]
#[tauri::command]
fn undo_delete_session_record(
    record_id: domain::RecordId,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<SessionRecordView, String> {
    let restored = SessionRecordCommands::new(records.inner())
        .restore_last_deleted(record_id)
        .map_err(|error| error.to_string())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(restored)
}

#[cfg(windows)]
#[tauri::command]
fn clear_clipboard_history(
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<usize, String> {
    let removed = SessionRecordCommands::new(records.inner())
        .clear()
        .map_err(|error| error.to_string())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(removed)
}

#[cfg(windows)]
#[tauri::command]
fn list_clipboard_groups(
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
) -> Vec<ClipboardGroupView> {
    groups.list()
}

#[cfg(windows)]
#[tauri::command]
fn get_active_group(active: tauri::State<'_, Arc<ActiveGroupState>>) -> ActiveGroupView {
    active.current().into()
}

#[cfg(windows)]
#[tauri::command]
fn set_active_group(
    kind: String,
    group_id: Option<domain::GroupId>,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    active: tauri::State<'_, Arc<ActiveGroupState>>,
    app: tauri::AppHandle,
) -> Result<ActiveGroupView, String> {
    let _coordinated = groups.mutation_coordinator.lock();
    let value = match kind.as_str() {
        "all" => ActiveGroup::All,
        "ungrouped" => ActiveGroup::Ungrouped,
        "group" => {
            let id = group_id.ok_or_else(|| "clipboard group is required".to_owned())?;
            if !groups.contains_coordinated(id) {
                return Err("clipboard group is no longer available".to_owned());
            }
            ActiveGroup::Group(id)
        }
        _ => return Err("clipboard group selection is invalid".to_owned()),
    };
    active.set(value);
    let view = ActiveGroupView::from(value);
    let _ = app.emit("active-group-changed", view);
    Ok(view)
}

#[cfg(windows)]
#[tauri::command]
fn create_clipboard_group(
    name: String,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    app: tauri::AppHandle,
) -> Result<ClipboardGroupView, String> {
    let group = groups.create(name)?;
    let _ = app.emit("clipboard-groups-changed", ());
    Ok(group)
}

#[cfg(windows)]
#[tauri::command]
fn rename_clipboard_group(
    group_id: domain::GroupId,
    name: String,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    app: tauri::AppHandle,
) -> Result<ClipboardGroupView, String> {
    let group = groups.rename(group_id, name)?;
    let _ = app.emit("clipboard-groups-changed", ());
    Ok(group)
}

#[cfg(windows)]
#[tauri::command]
fn move_clipboard_group(
    group_id: domain::GroupId,
    direction: i8,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    app: tauri::AppHandle,
) -> Result<Vec<ClipboardGroupView>, String> {
    if direction != -1 && direction != 1 {
        return Err("clipboard group direction is invalid".to_owned());
    }
    let ordered = groups.move_group(group_id, direction)?;
    let _ = app.emit("clipboard-groups-changed", ());
    Ok(ordered)
}

#[cfg(windows)]
#[tauri::command]
fn delete_clipboard_group(
    group_id: domain::GroupId,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    active: tauri::State<'_, Arc<ActiveGroupState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let active_changed =
        delete_clipboard_group_state(group_id, groups.inner(), records.inner(), active.inner())?;
    if active_changed {
        let _ = app.emit(
            "active-group-changed",
            ActiveGroupView::from(ActiveGroup::Ungrouped),
        );
    }
    let _ = app.emit("clipboard-groups-changed", ());
    let _ = app.emit("clipboard-records-changed", ());
    Ok(())
}

#[cfg(windows)]
fn delete_clipboard_group_state(
    group_id: domain::GroupId,
    groups: &ClipboardGroups,
    records: &SessionRecordStore,
    active: &ActiveGroupState,
) -> Result<bool, String> {
    let _coordinated = groups.mutation_coordinator.lock();
    groups.delete_coordinated(group_id)?;
    records.clear_group_coordinated(group_id);
    let active_changed = active.current() == ActiveGroup::Group(group_id);
    if active_changed {
        active.set(ActiveGroup::Ungrouped);
    }
    Ok(active_changed)
}

#[cfg(windows)]
#[tauri::command]
fn update_record_group(
    record_id: domain::RecordId,
    group_id: Option<domain::GroupId>,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    app: tauri::AppHandle,
) -> Result<SessionRecordView, String> {
    let updated = update_record_group_state(record_id, group_id, groups.inner(), records.inner())?;
    let _ = app.emit("clipboard-records-changed", ());
    Ok(updated)
}

#[cfg(windows)]
fn update_record_group_state(
    record_id: domain::RecordId,
    group_id: Option<domain::GroupId>,
    groups: &ClipboardGroups,
    records: &SessionRecordStore,
) -> Result<SessionRecordView, String> {
    let _coordinated = groups.mutation_coordinator.lock();
    if group_id.is_some_and(|id| !groups.contains_coordinated(id)) {
        return Err("clipboard group is no longer available".to_owned());
    }
    records
        .update_group_coordinated(record_id, group_id)
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[tauri::command]
fn get_settings(settings: tauri::State<'_, Arc<ApplicationSettings>>) -> SettingsView {
    settings.view()
}

#[cfg(windows)]
#[tauri::command]
fn update_capture_paused(
    paused: bool,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> SettingsView {
    let view = settings.update_capture_paused(paused);
    let _ = app.emit("settings-changed", view.clone());
    view
}

#[cfg(windows)]
#[tauri::command]
fn update_excluded_applications(
    applications: Vec<String>,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> Result<SettingsView, String> {
    let view = settings.update_excluded_applications(applications)?;
    let _ = app.emit("settings-changed", view.clone());
    Ok(view)
}

#[cfg(windows)]
#[tauri::command]
fn update_language(
    language: Language,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    desktop: tauri::State<'_, Arc<DesktopRuntime>>,
    app: tauri::AppHandle,
) -> SettingsView {
    let view = settings.update_language(language);
    desktop.set_language(language);
    let _ = app.emit("settings-changed", &view);
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
    let _ = app.emit("settings-changed", &view);
    let _ = app.emit("clipboard-records-changed", ());
    view
}

#[cfg(windows)]
#[tauri::command]
fn update_storage_policy(
    storage_limit: crate::domain::StorageLimit,
    evict_favorites_when_full: bool,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> Result<SettingsView, String> {
    let (view, records_changed) =
        settings.update_storage_policy(storage_limit, evict_favorites_when_full)?;
    let _ = app.emit("settings-changed", &view);
    if records_changed {
        let _ = app.emit("clipboard-records-changed", ());
    }
    Ok(view)
}

#[cfg(windows)]
#[tauri::command]
fn update_start_at_sign_in(
    enabled: bool,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> Result<SettingsView, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    platform::windows::startup::set_start_at_sign_in(enabled, &executable)?;
    let view = settings.update_start_at_sign_in(enabled);
    let _ = app.emit("settings-changed", &view);
    Ok(view)
}

#[cfg(windows)]
#[tauri::command]
fn update_start_minimized(
    enabled: bool,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> SettingsView {
    let view = settings.update_start_minimized(enabled);
    let _ = app.emit("settings-changed", &view);
    view
}

#[cfg(windows)]
#[tauri::command]
fn update_show_tray_icon(
    enabled: bool,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    desktop: tauri::State<'_, Arc<DesktopRuntime>>,
    app: tauri::AppHandle,
) -> Result<SettingsView, String> {
    desktop
        .tray
        .set_visible(enabled)
        .map_err(|error| error.to_string())?;
    let view = settings.update_show_tray_icon(enabled);
    let _ = app.emit("settings-changed", &view);
    Ok(view)
}

#[cfg(windows)]
#[tauri::command]
fn update_accent_color(
    accent_color: AccentColor,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> SettingsView {
    let view = settings.update_accent_color(accent_color);
    let _ = app.emit("settings-changed", &view);
    view
}

#[cfg(windows)]
#[tauri::command]
fn update_sound_enabled(
    enabled: bool,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> SettingsView {
    let view = settings.update_sound_enabled(enabled);
    let _ = app.emit("settings-changed", &view);
    view
}

#[cfg(windows)]
#[tauri::command]
fn update_capture_sound(
    capture_sound: CaptureSound,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> Result<SettingsView, String> {
    if capture_sound == CaptureSound::Custom && !settings.custom_sound_path.is_file() {
        return Err("custom capture sound is unavailable".to_owned());
    }
    let view = settings.update_capture_sound(capture_sound);
    let _ = app.emit("settings-changed", &view);
    Ok(view)
}

#[cfg(windows)]
#[tauri::command]
fn choose_custom_sound(
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    app: tauri::AppHandle,
) -> Result<Option<SettingsView>, String> {
    let Some(source) = rfd::FileDialog::new()
        .add_filter("WAV audio", &["wav"])
        .pick_file()
    else {
        return Ok(None);
    };
    if !source
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return Err("custom capture sound must be a WAV file".to_owned());
    }
    let metadata = std::fs::metadata(&source).map_err(|error| error.to_string())?;
    if metadata.len() > 10 * 1024 * 1024 {
        return Err("custom capture sound exceeds 10 MB".to_owned());
    }
    let header = std::fs::read(&source).map_err(|error| error.to_string())?;
    if header.len() < 12 || &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Err("custom capture sound is not a valid WAV file".to_owned());
    }
    if let Some(parent) = settings.custom_sound_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    if source != settings.custom_sound_path {
        std::fs::copy(&source, &settings.custom_sound_path).map_err(|error| error.to_string())?;
    }
    let view = settings.update_capture_sound(CaptureSound::Custom);
    settings.play_capture_sound();
    let _ = app.emit("settings-changed", &view);
    Ok(Some(view))
}

#[cfg(windows)]
#[tauri::command]
fn export_backup(settings: tauri::State<'_, Arc<ApplicationSettings>>) -> Result<bool, String> {
    let Some(destination) = rfd::FileDialog::new()
        .add_filter("Clipboard Assistant backup", &["clipbackup"])
        .set_file_name(format!(
            "clipboard-assistant-backup-{}.clipbackup",
            chrono::Local::now().format("%Y-%m-%d")
        ))
        .save_file()
    else {
        return Ok(false);
    };
    let persistence = settings
        .persistence
        .as_ref()
        .ok_or_else(|| "local clipboard storage is unavailable".to_owned())?;
    persistence
        .backup(destination)
        .map_err(|error| error.to_string())?;
    Ok(true)
}

#[cfg(windows)]
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn restore_backup(
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    runtime: tauri::State<'_, HotkeyRuntime>,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    active: tauri::State<'_, Arc<ActiveGroupState>>,
    coordinator: tauri::State<'_, Arc<WindowsQuickPanelCoordinator>>,
    desktop: tauri::State<'_, Arc<DesktopRuntime>>,
    app: tauri::AppHandle,
) -> Result<Option<SettingsView>, String> {
    let Some(source) = rfd::FileDialog::new()
        .add_filter("Clipboard Assistant backup", &["clipbackup"])
        .pick_file()
    else {
        return Ok(None);
    };
    let persistence = settings
        .persistence
        .as_ref()
        .ok_or_else(|| "local clipboard storage is unavailable".to_owned())?;
    let view = records.with_restore_guard(|restore_guard| {
        let restored = persistence
            .restore(
                source,
                RestoreBudget {
                    max_records: services::session_records::MAX_SESSION_RECORDS,
                    max_total_bytes: services::session_records::DEFAULT_STORE_BYTES,
                    max_record_bytes: services::session_records::MAX_CAPTURE_RECORD_BYTES,
                },
            )
            .map_err(|error| error.to_string())?;

        let previous = settings.current();
        let mut effective = restored.settings;
        if effective.capture_sound == CaptureSound::Custom && !settings.custom_sound_path.is_file()
        {
            effective.capture_sound = CaptureSound::Default;
        }
        if !effective.show_tray_icon {
            effective.start_minimized = false;
        }
        if validate_shortcuts(
            effective.activation_shortcut,
            effective.group_shortcut_modifiers,
            effective.quick_paste_modifiers,
        )
        .is_err()
            || reconfigure_hotkeys(
                effective.activation_shortcut,
                effective.group_shortcut_modifiers,
                effective.quick_paste_enabled,
                effective.quick_paste_modifiers,
                previous,
                &runtime,
                groups.inner(),
                records.inner(),
                active.inner(),
                coordinator.inner(),
                &app,
            )
            .is_err()
        {
            effective.activation_shortcut = previous.activation_shortcut;
            effective.group_shortcut_modifiers = previous.group_shortcut_modifiers;
            effective.quick_paste_enabled = previous.quick_paste_enabled;
            effective.quick_paste_modifiers = previous.quick_paste_modifiers;
        }

        restore_guard.apply_page(
            crate::domain::HistoryQuery {
                limit: services::session_records::STARTUP_HISTORY_RECORDS,
                ..crate::domain::HistoryQuery::default()
            },
            restored.page,
        );
        groups.replace_all_coordinated(restored.groups);
        active.set(ActiveGroup::All);
        let startup_updated = std::env::current_exe()
            .ok()
            .and_then(|executable| {
                platform::windows::startup::set_start_at_sign_in(
                    effective.start_at_sign_in,
                    &executable,
                )
                .ok()
            })
            .is_some();
        if !startup_updated {
            effective.start_at_sign_in = previous.start_at_sign_in;
        }
        if desktop.tray.set_visible(effective.show_tray_icon).is_err() {
            effective.show_tray_icon = previous.show_tray_icon;
            effective.start_minimized = previous.start_minimized;
        }
        desktop.set_language(effective.language);
        settings
            .capture_policy
            .set_excluded_applications(normalize_excluded_applications(
                restored.excluded_applications,
            )?);
        let view = settings.replace_current(effective);
        settings.persist_settings_coordinated();
        Ok::<_, String>(view)
    })?;
    let _ = app.emit("clipboard-records-changed", ());
    let _ = app.emit("clipboard-groups-changed", ());
    let _ = app.emit(
        "active-group-changed",
        ActiveGroupView::from(ActiveGroup::All),
    );
    let _ = app.emit("settings-changed", &view);
    Ok(Some(view))
}

#[cfg(windows)]
#[tauri::command]
fn preview_capture_sound(settings: tauri::State<'_, Arc<ApplicationSettings>>) {
    settings.play_capture_sound();
}

#[cfg(windows)]
fn validate_shortcuts(
    activation: Shortcut,
    group_modifiers: ShortcutModifiers,
    quick_paste_modifiers: ShortcutModifiers,
) -> Result<(), String> {
    if !activation.modifiers.is_safe_global_shortcut()
        || !group_modifiers.is_safe_global_shortcut()
        || !quick_paste_modifiers.is_safe_global_shortcut()
    {
        return Err("shortcut_requires_ctrl_alt_or_win".to_owned());
    }
    if (activation.key == ShortcutKey::Left || activation.key == ShortcutKey::Right)
        && activation.modifiers == group_modifiers
    {
        return Err("shortcut_conflict".to_owned());
    }
    if matches!(
        activation.key,
        ShortcutKey::Digit1
            | ShortcutKey::Digit2
            | ShortcutKey::Digit3
            | ShortcutKey::Digit4
            | ShortcutKey::Digit5
            | ShortcutKey::Digit6
            | ShortcutKey::Digit7
            | ShortcutKey::Digit8
            | ShortcutKey::Digit9
    ) && activation.modifiers == quick_paste_modifiers
    {
        return Err("shortcut_conflict".to_owned());
    }
    if activation.modifiers.alt && activation.key == ShortcutKey::F4
        || activation.modifiers.win && activation.key == ShortcutKey::L
    {
        return Err("shortcut_reserved".to_owned());
    }
    Ok(())
}

#[cfg(windows)]
fn start_activation_hotkey(
    shortcut: Shortcut,
    app: tauri::AppHandle,
    coordinator: Arc<WindowsQuickPanelCoordinator>,
) -> Result<platform::windows::hotkey::GlobalHotkey, String> {
    platform::windows::hotkey::GlobalHotkey::start(shortcut, move || {
        let coordinator = Arc::clone(&coordinator);
        let _ = app.run_on_main_thread(move || {
            let _ = coordinator.toggle();
        });
    })
    .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn start_group_hotkeys(
    modifiers: ShortcutModifiers,
    app: tauri::AppHandle,
    active: Arc<ActiveGroupState>,
    groups: Arc<ClipboardGroups>,
    coordinator: Arc<WindowsQuickPanelCoordinator>,
) -> Result<platform::windows::hotkey::GroupHotkeys, String> {
    platform::windows::hotkey::GroupHotkeys::start(modifiers, move |direction| {
        let app = app.clone();
        let active = Arc::clone(&active);
        let groups = Arc::clone(&groups);
        let coordinator = Arc::clone(&coordinator);
        let _ = app.clone().run_on_main_thread(move || {
            let view = ActiveGroupView::from(active.switch(direction, &groups.list()));
            let _ = app.emit("active-group-changed", view);
            let _ = coordinator.show();
        });
    })
    .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn start_quick_paste_hotkeys(
    modifiers: ShortcutModifiers,
    app: tauri::AppHandle,
    records: Arc<SessionRecordStore>,
    active: Arc<ActiveGroupState>,
    coordinator: Arc<WindowsQuickPanelCoordinator>,
) -> Result<platform::windows::hotkey::QuickPasteHotkeys, String> {
    platform::windows::hotkey::QuickPasteHotkeys::start(modifiers, move |index| {
        let records = Arc::clone(&records);
        let active = Arc::clone(&active);
        let coordinator = Arc::clone(&coordinator);
        let app = app.clone();
        let _ = app.run_on_main_thread(move || {
            let selected = records
                .list()
                .into_iter()
                .filter(|record| match active.current() {
                    ActiveGroup::All => true,
                    ActiveGroup::Ungrouped => record.group_id.is_none(),
                    ActiveGroup::Group(id) => record.group_id == Some(id),
                })
                .nth(index);
            let Some(selected) = selected else {
                return;
            };
            if let Some(representations) = records.representations(selected.id) {
                let _ = coordinator.direct_paste(&representations);
            }
        });
    })
    .map_err(|error| error.to_string())
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn reconfigure_hotkeys(
    activation: Shortcut,
    group_modifiers: ShortcutModifiers,
    quick_paste_enabled: bool,
    quick_paste_modifiers: ShortcutModifiers,
    previous: UserSettings,
    runtime: &HotkeyRuntime,
    groups: &Arc<ClipboardGroups>,
    records: &Arc<SessionRecordStore>,
    active: &Arc<ActiveGroupState>,
    coordinator: &Arc<WindowsQuickPanelCoordinator>,
    app: &tauri::AppHandle,
) -> Result<(), String> {
    drop(lock_unpoisoned(&runtime.activation).take());
    drop(lock_unpoisoned(&runtime.groups).take());
    drop(lock_unpoisoned(&runtime.quick_paste).take());

    let register = || -> Result<_, String> {
        let activation_registration =
            start_activation_hotkey(activation, app.clone(), Arc::clone(coordinator))?;
        let group_registration = start_group_hotkeys(
            group_modifiers,
            app.clone(),
            Arc::clone(active),
            Arc::clone(groups),
            Arc::clone(coordinator),
        )?;
        let quick_paste_registration = quick_paste_enabled
            .then(|| {
                start_quick_paste_hotkeys(
                    quick_paste_modifiers,
                    app.clone(),
                    Arc::clone(records),
                    Arc::clone(active),
                    Arc::clone(coordinator),
                )
            })
            .transpose()?;
        Ok((
            activation_registration,
            group_registration,
            quick_paste_registration,
        ))
    };
    let registrations = match register() {
        Ok(registrations) => registrations,
        Err(error) => {
            *lock_unpoisoned(&runtime.activation) = start_activation_hotkey(
                previous.activation_shortcut,
                app.clone(),
                Arc::clone(coordinator),
            )
            .ok();
            *lock_unpoisoned(&runtime.groups) = start_group_hotkeys(
                previous.group_shortcut_modifiers,
                app.clone(),
                Arc::clone(active),
                Arc::clone(groups),
                Arc::clone(coordinator),
            )
            .ok();
            *lock_unpoisoned(&runtime.quick_paste) = previous
                .quick_paste_enabled
                .then(|| {
                    start_quick_paste_hotkeys(
                        previous.quick_paste_modifiers,
                        app.clone(),
                        Arc::clone(records),
                        Arc::clone(active),
                        Arc::clone(coordinator),
                    )
                })
                .transpose()
                .ok()
                .flatten();
            return Err(error);
        }
    };
    *lock_unpoisoned(&runtime.activation) = Some(registrations.0);
    *lock_unpoisoned(&runtime.groups) = Some(registrations.1);
    *lock_unpoisoned(&runtime.quick_paste) = registrations.2;
    Ok(())
}

#[cfg(windows)]
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn update_shortcuts(
    activation: Shortcut,
    group_modifiers: ShortcutModifiers,
    quick_paste_enabled: bool,
    quick_paste_modifiers: ShortcutModifiers,
    settings: tauri::State<'_, Arc<ApplicationSettings>>,
    runtime: tauri::State<'_, HotkeyRuntime>,
    groups: tauri::State<'_, Arc<ClipboardGroups>>,
    records: tauri::State<'_, Arc<SessionRecordStore>>,
    active: tauri::State<'_, Arc<ActiveGroupState>>,
    coordinator: tauri::State<'_, Arc<WindowsQuickPanelCoordinator>>,
    app: tauri::AppHandle,
) -> Result<SettingsView, String> {
    validate_shortcuts(activation, group_modifiers, quick_paste_modifiers)?;
    reconfigure_hotkeys(
        activation,
        group_modifiers,
        quick_paste_enabled,
        quick_paste_modifiers,
        settings.current(),
        &runtime,
        groups.inner(),
        records.inner(),
        active.inner(),
        coordinator.inner(),
        &app,
    )?;
    *lock_unpoisoned(&settings.hotkey_status) = HotkeyStatus::Available;
    let view = settings.update_shortcuts(
        activation,
        group_modifiers,
        quick_paste_enabled,
        quick_paste_modifiers,
    );
    let _ = app.emit("settings-changed", &view);
    Ok(view)
}

#[cfg(windows)]
fn show_settings_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(windows)]
#[tauri::command]
fn exit_application(app: tauri::AppHandle, desktop: tauri::State<'_, Arc<DesktopRuntime>>) {
    mark_application_exiting(&desktop.exiting);
    app.exit(0);
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

#[cfg(windows)]
#[tauri::command]
fn begin_quick_panel_drag(controller: tauri::State<'_, Arc<PanelController>>) {
    controller.begin_dragging();
}

#[cfg(windows)]
#[tauri::command]
fn finish_quick_panel_drag(
    controller: tauri::State<'_, Arc<PanelController>>,
) -> Result<(), String> {
    controller
        .finish_dragging()
        .map_err(|error| error.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(windows)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        show_settings_window(app);
    }));
    #[cfg(windows)]
    let builder = builder
        .setup(|app| {
            let panel = app
                .get_webview_window("quick-panel")
                .ok_or_else(|| "quick-panel window is missing".to_owned())?;
            platform::windows::configure_quick_panel_style(panel.hwnd()?)?;
            let panel_controller = Arc::new(PanelController::new(panel));
            let storage_available = Arc::new(AtomicBool::new(false));
            let mutation_coordinator = Arc::new(PersistenceMutationCoordinator::default());
            let app_data_dir = app.path().app_data_dir().ok();
            let custom_sound_path = app_data_dir
                .as_ref()
                .map(|directory| directory.join("capture-sound.wav"))
                .unwrap_or_default();
            let (repository, user_settings, excluded_applications, loaded_records) = app
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
                        .load_page(crate::domain::HistoryQuery {
                            limit: services::session_records::STARTUP_HISTORY_RECORDS,
                            ..crate::domain::HistoryQuery::default()
                        })
                        .ok()?;
                    let exclusions = repository.load_excluded_applications().ok()?;
                    Some((repository, settings, exclusions, records))
                })
                .map_or(
                    (
                        None,
                        UserSettings::default(),
                        Vec::new(),
                        services::persistence::HistoryPage {
                            records: Vec::new(),
                            next_cursor: None,
                        },
                    ),
                    |(repository, settings, exclusions, records)| {
                        storage_available.store(true, Ordering::Release);
                        (Some(repository), settings, exclusions, records)
                    },
                );
            let loaded_groups = repository
                .as_ref()
                .and_then(|repository| repository.load_groups().ok())
                .unwrap_or_default()
                .into_iter()
                .map(|(id, name)| ClipboardGroupView { id, name })
                .collect();
            let groups = Arc::new(ClipboardGroups {
                groups: Mutex::new(loaded_groups),
                repository: repository.clone(),
                mutation_coordinator: Arc::clone(&mutation_coordinator),
            });
            let persistence = repository.and_then(|repository| {
                PersistenceWorker::start(repository, Arc::clone(&storage_available)).ok()
            });
            if persistence.is_none() {
                storage_available.store(false, Ordering::Release);
            }
            let session_records = match &persistence {
                Some(persistence) => {
                    Arc::new(SessionRecordStore::with_persistence_page_and_coordinator(
                        loaded_records,
                        Arc::clone(persistence)
                            as Arc<dyn services::persistence::RecordPersistence>,
                        Arc::clone(&storage_available),
                        Arc::clone(&mutation_coordinator),
                    ))
                }
                None => Arc::new(SessionRecordStore::with_session_only(
                    Vec::new(),
                    Arc::clone(&storage_available),
                )),
            };
            let capture_policy = Arc::new(platform::windows::clipboard::CapturePolicy::new(
                normalize_excluded_applications(excluded_applications).unwrap_or_default(),
            ));
            let (clipboard_events, clipboard_receiver) =
                platform::windows::clipboard::latest_clipboard_event_channel_with_policy(
                    Arc::clone(&session_records),
                    Arc::clone(&capture_policy),
                );
            let listener =
                platform::windows::clipboard::ClipboardListener::start(clipboard_events)?;
            let settings_state = Arc::new(ApplicationSettings {
                current: Mutex::new(user_settings),
                persistence,
                records: Arc::clone(&session_records),
                storage_available,
                hotkey_status: Mutex::new(HotkeyStatus::Unavailable),
                custom_sound_path,
                capture_policy,
                mutation_coordinator,
            });
            let settings_window = app
                .get_webview_window("settings")
                .ok_or_else(|| "settings window is missing".to_owned())?;
            let tray = TrayIconBuilder::with_id("main-tray")
                .show_menu_on_left_click(false)
                .tooltip("剪贴板助手")
                .icon(
                    app.default_window_icon()
                        .ok_or_else(|| "application icon is missing".to_owned())?
                        .clone(),
                )
                .on_tray_icon_event(|tray, event| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        show_settings_window(tray.app_handle());
                    }
                })
                .build(app)?;
            tray.set_visible(user_settings.show_tray_icon)?;
            let desktop = Arc::new(DesktopRuntime {
                tray,
                exiting: AtomicBool::new(false),
            });
            desktop.set_language(user_settings.language);
            if !user_settings.start_minimized {
                settings_window.show()?;
                settings_window.set_focus()?;
            }
            let app_handle = app.handle().clone();
            let settings_for_drain = Arc::clone(&settings_state);
            let (event_drain_stop, event_drain) =
                spawn_clipboard_event_drain(clipboard_receiver, move || {
                    settings_for_drain.prune_expired();
                    settings_for_drain.play_capture_sound();
                    let _ = app_handle.emit("clipboard-records-changed", ());
                    let _ = app_handle.emit("settings-changed", settings_for_drain.view());
                })?;
            let retention_app = app.handle().clone();
            let settings_for_retention = Arc::clone(&settings_state);
            let (retention_stop, retention) =
                spawn_periodic_retention(std::time::Duration::from_secs(60 * 60), move || {
                    let removed = settings_for_retention.prune_expired();
                    if removed > 0 {
                        let _ = retention_app.emit("clipboard-records-changed", ());
                    }
                    let _ = retention_app.emit("settings-changed", settings_for_retention.view());
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
            let active_group = Arc::new(ActiveGroupState(Mutex::new(ActiveGroup::All)));
            let activation_hotkey = start_activation_hotkey(
                user_settings.activation_shortcut,
                app.handle().clone(),
                Arc::clone(&coordinator),
            );
            *lock_unpoisoned(&settings_state.hotkey_status) = match &activation_hotkey {
                Ok(_) => HotkeyStatus::Available,
                Err(error) if error.contains("already in use") => HotkeyStatus::Conflict,
                Err(_) => HotkeyStatus::Unavailable,
            };
            let group_hotkeys = start_group_hotkeys(
                user_settings.group_shortcut_modifiers,
                app.handle().clone(),
                Arc::clone(&active_group),
                Arc::clone(&groups),
                Arc::clone(&coordinator),
            );
            let quick_paste_hotkeys = if user_settings.quick_paste_enabled {
                match start_quick_paste_hotkeys(
                    user_settings.quick_paste_modifiers,
                    app.handle().clone(),
                    Arc::clone(&session_records),
                    Arc::clone(&active_group),
                    Arc::clone(&coordinator),
                ) {
                    Ok(hotkeys) => Some(hotkeys),
                    Err(_) => {
                        settings_state.update_shortcuts(
                            user_settings.activation_shortcut,
                            user_settings.group_shortcut_modifiers,
                            false,
                            user_settings.quick_paste_modifiers,
                        );
                        None
                    }
                }
            } else {
                None
            };
            app.manage(Arc::clone(&panel_controller));
            app.manage(Arc::clone(&groups));
            app.manage(Arc::clone(&session_records));
            app.manage(Arc::clone(&coordinator));
            app.manage(active_group);
            app.manage(Arc::clone(&settings_state));
            app.manage(desktop);
            app.manage(HotkeyRuntime {
                activation: Mutex::new(activation_hotkey.ok()),
                groups: Mutex::new(group_hotkeys.ok()),
                quick_paste: Mutex::new(quick_paste_hotkeys),
            });
            app.manage(ClipboardRuntime {
                listener: std::sync::Mutex::new(Some(listener)),
                event_drain_stop: std::sync::Mutex::new(Some(event_drain_stop)),
                event_drain: std::sync::Mutex::new(Some(event_drain)),
                retention_stop: std::sync::Mutex::new(Some(retention_stop)),
                retention: std::sync::Mutex::new(Some(retention)),
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "settings" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    let settings = window.state::<Arc<ApplicationSettings>>().view();
                    let desktop = window.state::<Arc<DesktopRuntime>>();
                    if settings.show_tray_icon && !desktop.exiting.load(Ordering::Acquire) {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
                return;
            }
            if window.label() != "quick-panel" {
                return;
            }
            let controller = window.state::<Arc<PanelController>>();
            let coordinator = window.state::<Arc<WindowsQuickPanelCoordinator>>();
            match event {
                tauri::WindowEvent::Focused(focused) => {
                    if !focused
                        && window
                            .hwnd()
                            .is_ok_and(platform::windows::is_window_in_move_or_size)
                    {
                        controller.begin_resizing();
                    }
                    let result = controller.on_focus_changed(*focused);
                    if !focused && (result.is_err() || !controller.is_visible()) {
                        coordinator.clear_target();
                    }
                }
                tauri::WindowEvent::Resized(_) | tauri::WindowEvent::Moved(_) => {
                    controller.note_native_bounds_change();
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
            begin_quick_panel_drag,
            finish_quick_panel_drag,
            paste_selected,
            list_session_records,
            list_history_page,
            get_record_details,
            set_record_pinned,
            set_record_favorite,
            update_record_content,
            create_text_record,
            get_record_image_preview,
            update_record_note,
            delete_session_record,
            undo_delete_session_record,
            clear_clipboard_history,
            list_clipboard_groups,
            get_active_group,
            set_active_group,
            create_clipboard_group,
            rename_clipboard_group,
            move_clipboard_group,
            delete_clipboard_group,
            update_record_group,
            get_settings,
            update_language,
            update_retention,
            update_storage_policy,
            update_start_at_sign_in,
            update_start_minimized,
            update_show_tray_icon,
            update_accent_color,
            update_sound_enabled,
            update_capture_sound,
            update_capture_paused,
            update_excluded_applications,
            choose_custom_sound,
            preview_capture_sound,
            export_backup,
            restore_backup,
            update_shortcuts,
            exit_application
        ]);
    #[cfg(not(windows))]
    let builder = builder.invoke_handler(tauri::generate_handler![greet]);

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(all(test, windows))]
mod desktop_configuration_tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::mark_application_exiting;

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

    #[test]
    fn explicit_exit_marks_the_runtime_before_shutdown() {
        let exiting = AtomicBool::new(false);

        mark_application_exiting(&exiting);

        assert!(exiting.load(Ordering::Acquire));
    }
}
