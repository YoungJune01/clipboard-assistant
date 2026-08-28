use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration as StdDuration,
};

#[cfg(test)]
use std::sync::Barrier;

#[cfg(not(windows))]
use std::{collections::HashSet, sync::Condvar, time::Instant};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;

#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
    },
    core::PCWSTR,
};

#[cfg(windows)]
use std::{marker::PhantomData, rc::Rc};

use crate::domain::{
    AccentColor, CaptureSound, ClipboardRecord, ClipboardRepresentation,
    ClipboardRepresentationDetails, ClipboardRepresentationKind, ContentIdentity, ContentKind,
    GroupId, HistoryCursor, HistoryQuery, Language, RecordId, RecordNote, RetentionPeriod,
    ShortcutModifiers, SourceIdentity, StorageLimit, UserSettings,
};
use crate::services::backup;
use crate::services::session_records::{
    MAX_CAPTURE_RECORD_BYTES, MAX_DETAIL_FILE_LIST_BYTES, MAX_DETAIL_FILE_LIST_PATHS,
    MAX_DETAIL_TEXT_BYTES, REPRESENTATION_OVERHEAD_BYTES,
};
use crate::services::storage_location::{self, StorageLocation};

const SCHEMA_VERSION: i64 = 6;
pub(crate) const DATABASE_FILE: &str = "clipboard-history.sqlite3";
const MIGRATION_TEMP_SUFFIX: &str = ".migrate-v2";
const MIGRATION_BACKUP_SUFFIX: &str = ".migrate-v1-backup";
const DEFAULT_HISTORY_PAGE_LIMIT: usize = 50;
const MAX_HISTORY_PAGE_LIMIT: usize = 100;
const MAX_FILE_LIST_PATHS: usize = 4096;
const MAX_FILE_LIST_LOGICAL_BYTES: usize = MAX_CAPTURE_RECORD_BYTES - REPRESENTATION_OVERHEAD_BYTES;
const MAX_FILE_LIST_ENCODED_BYTES: usize = 24 * 1024 * 1024;
const DATABASE_INCREMENTAL_VACUUM_PAGES: usize = 256;
const WORK_QUEUE_CAPACITY: usize = 64;
const WORKER_TIMEOUT: StdDuration = StdDuration::from_secs(2);
const BACKUP_WORKER_TIMEOUT: StdDuration = StdDuration::from_secs(120);
const CONTROL_POLL_INTERVAL: StdDuration = StdDuration::from_millis(25);
const MIGRATION_LOCK_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const MIGRATION_MUTEX_PREFIX: &str = "Local\\ClipboardAssistant.StorageMigration.";

#[derive(Clone, Copy)]
pub(crate) struct RestoreBudget {
    pub(crate) max_records: usize,
    pub(crate) max_total_bytes: usize,
    pub(crate) max_record_bytes: usize,
}

#[derive(Debug)]
pub(crate) struct RestoredData {
    pub(crate) settings: UserSettings,
    pub(crate) excluded_applications: Vec<String>,
    pub(crate) groups: Vec<(GroupId, String)>,
    pub(crate) page: HistoryPage,
}

#[derive(Clone, Copy)]
pub(crate) struct DiskQuota {
    pub(crate) max_records: usize,
    pub(crate) max_payload_bytes: usize,
    pub(crate) incremental_vacuum_pages: usize,
}

impl Default for DiskQuota {
    fn default() -> Self {
        Self {
            max_records: usize::MAX,
            max_payload_bytes: usize::MAX,
            incremental_vacuum_pages: DATABASE_INCREMENTAL_VACUUM_PAGES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryRecordSummary {
    pub id: RecordId,
    pub captured_at: DateTime<Utc>,
    pub source_application: Option<String>,
    pub text: Option<String>,
    pub has_image: bool,
    pub ocr_text: Option<String>,
    pub qr_text: Option<String>,
    pub file_paths: Vec<String>,
    pub file_count: usize,
    pub content_kind: ContentKind,
    pub note: Option<RecordNote>,
    pub group_id: Option<GroupId>,
    pub pinned: bool,
    pub favorite: bool,
    pub sensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryPage {
    pub records: Vec<HistoryRecordSummary>,
    pub next_cursor: Option<HistoryCursor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedRecordDetails {
    pub id: RecordId,
    pub content_identity: ContentIdentity,
    pub captured_at: DateTime<Utc>,
    pub source_application: Option<String>,
    pub representations: Vec<ClipboardRepresentationDetails>,
    pub note: Option<RecordNote>,
    pub group_id: Option<GroupId>,
    pub content_kind: ContentKind,
    pub pinned: bool,
    pub favorite: bool,
    pub sensitive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageAvailability {
    Available,
    Unavailable,
}

#[derive(Default)]
pub(crate) struct PersistenceMutationCoordinator {
    gate: Mutex<()>,
}

impl PersistenceMutationCoordinator {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        lock_unpoisoned(&self.gate)
    }
}

#[derive(Debug)]
pub enum PersistenceError {
    CreateDirectory(std::io::Error),
    FileOperation(std::io::Error),
    Database(rusqlite::Error),
    Backup(backup::BackupError),
    InvalidData,
    UnsupportedRepresentationKind(String),
    UnsupportedSchema(i64),
    MigrationLockUnavailable,
    MigrationLockTimeout,
    RestoreRollbackFailed,
    WorkerUnavailable,
    WorkerStart(std::io::Error),
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateDirectory(_) => {
                formatter.write_str("application data directory is unavailable")
            }
            Self::FileOperation(_) => {
                formatter.write_str("local clipboard storage migration is unavailable")
            }
            Self::Database(_) => formatter.write_str("local clipboard storage is unavailable"),
            Self::Backup(error) => error.fmt(formatter),
            Self::InvalidData => {
                formatter.write_str("local clipboard storage contains invalid data")
            }
            Self::UnsupportedRepresentationKind(kind) => {
                write!(
                    formatter,
                    "clipboard backup contains unsupported format {kind}"
                )
            }
            Self::UnsupportedSchema(_) => {
                formatter.write_str("local clipboard storage schema is unsupported")
            }
            Self::MigrationLockUnavailable | Self::MigrationLockTimeout => {
                formatter.write_str("local clipboard storage migration is unavailable")
            }
            Self::RestoreRollbackFailed | Self::WorkerUnavailable | Self::WorkerStart(_) => {
                formatter.write_str("local clipboard storage is unavailable")
            }
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateDirectory(error) => Some(error),
            Self::FileOperation(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::Backup(error) => Some(error),
            Self::WorkerStart(error) => Some(error),
            Self::InvalidData
            | Self::UnsupportedRepresentationKind(_)
            | Self::UnsupportedSchema(_)
            | Self::MigrationLockUnavailable
            | Self::MigrationLockTimeout
            | Self::RestoreRollbackFailed
            | Self::WorkerUnavailable => None,
        }
    }
}

impl From<backup::BackupError> for PersistenceError {
    fn from(error: backup::BackupError) -> Self {
        Self::Backup(error)
    }
}

impl From<rusqlite::Error> for PersistenceError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub trait RecordPersistence: Send + Sync {
    fn save_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError>;
    fn update_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        self.save_record(record)
    }
    fn update_note(&self, id: RecordId, note: Option<&RecordNote>) -> Result<(), PersistenceError>;
    fn delete_record(&self, id: RecordId) -> Result<(), PersistenceError>;
    fn clear_records(&self) -> Result<usize, PersistenceError>;
    fn load_page(&self, _query: HistoryQuery) -> Result<HistoryPage, PersistenceError> {
        Err(PersistenceError::WorkerUnavailable)
    }
    fn search_history(
        &self,
        _query: crate::services::search::SearchQuery,
    ) -> Result<crate::services::search::SearchPage, PersistenceError> {
        Err(PersistenceError::WorkerUnavailable)
    }
    fn record_details(&self, _id: RecordId) -> Result<PersistedRecordDetails, PersistenceError> {
        Err(PersistenceError::WorkerUnavailable)
    }
    fn full_record(&self, _id: RecordId) -> Result<ClipboardRecord, PersistenceError> {
        Err(PersistenceError::WorkerUnavailable)
    }
}

enum PersistenceCommand {
    SaveRecord(
        ClipboardRecord,
        mpsc::SyncSender<Result<(), PersistenceError>>,
    ),
    UpdateNote(
        RecordId,
        Option<RecordNote>,
        mpsc::SyncSender<Result<(), PersistenceError>>,
    ),
    DeleteRecord(RecordId, mpsc::SyncSender<Result<(), PersistenceError>>),
    ClearRecords(mpsc::SyncSender<Result<usize, PersistenceError>>),
    SaveSettings(UserSettings, mpsc::SyncSender<Result<(), PersistenceError>>),
    UpdateStoragePolicy(
        StorageLimit,
        bool,
        mpsc::SyncSender<Result<usize, PersistenceError>>,
    ),
    SaveExcludedApplications(Vec<String>, mpsc::SyncSender<Result<(), PersistenceError>>),
    SaveRecognition(
        RecordId,
        Option<String>,
        Option<String>,
        String,
        mpsc::SyncSender<Result<(), PersistenceError>>,
    ),
    Prune(
        RetentionPeriod,
        DateTime<Utc>,
        mpsc::SyncSender<Result<usize, PersistenceError>>,
    ),
    Backup(PathBuf, mpsc::SyncSender<Result<(), PersistenceError>>),
    Restore(
        PathBuf,
        RestoreBudget,
        mpsc::SyncSender<Result<RestoredData, PersistenceError>>,
    ),
    MoveStorage(
        PathBuf,
        PathBuf,
        mpsc::SyncSender<Result<StorageLocation, PersistenceError>>,
    ),
}

enum PersistenceControl {
    FlushAndShutdown {
        accepted: usize,
        flushed: Sender<()>,
    },
}

pub struct PersistenceWorker {
    sender: Mutex<Option<SyncSender<PersistenceCommand>>>,
    control: Sender<PersistenceControl>,
    thread: Mutex<Option<JoinHandle<()>>>,
    storage_available: Arc<AtomicBool>,
    accepted: AtomicUsize,
    response_timeout: StdDuration,
    reader: Option<Arc<SqliteRepository>>,
}

trait PersistenceBackend: Send + Sync {
    fn persist_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError>;
    fn persist_note(&self, id: RecordId, note: Option<&RecordNote>)
    -> Result<(), PersistenceError>;
    fn delete_record(&self, id: RecordId) -> Result<(), PersistenceError>;
    fn clear_records(&self) -> Result<usize, PersistenceError>;
    fn persist_settings(&self, settings: UserSettings) -> Result<(), PersistenceError>;
    fn update_storage_policy(
        &self,
        storage_limit: StorageLimit,
        evict_favorites_when_full: bool,
    ) -> Result<usize, PersistenceError>;
    fn persist_excluded_applications(
        &self,
        applications: &[String],
    ) -> Result<(), PersistenceError>;
    fn persist_recognition(
        &self,
        id: RecordId,
        ocr_text: Option<&str>,
        qr_text: Option<&str>,
        status: &str,
    ) -> Result<(), PersistenceError>;
    fn prune_records(
        &self,
        retention: RetentionPeriod,
        now: DateTime<Utc>,
    ) -> Result<usize, PersistenceError>;
    fn backup_database(&self, destination: &Path) -> Result<(), PersistenceError>;
    fn restore_database(
        &self,
        source: &Path,
        budget: RestoreBudget,
    ) -> Result<RestoredData, PersistenceError>;
    fn move_storage(
        &self,
        _destination: &Path,
        _app_data: &Path,
    ) -> Result<StorageLocation, PersistenceError> {
        Err(PersistenceError::WorkerUnavailable)
    }
}

static THREAD_REAPER: LazyLock<Sender<JoinHandle<()>>> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::channel::<JoinHandle<()>>();
    thread::Builder::new()
        .name("clipboard-thread-reaper".to_owned())
        .spawn(move || {
            while let Ok(thread) = receiver.recv() {
                let _ = thread.join();
            }
        })
        .expect("clipboard thread reaper must start");
    sender
});

impl PersistenceWorker {
    pub fn start(
        repository: Arc<SqliteRepository>,
        storage_available: Arc<AtomicBool>,
    ) -> Result<Arc<Self>, PersistenceError> {
        Self::start_backend_with_reader(
            Arc::clone(&repository) as Arc<dyn PersistenceBackend>,
            storage_available,
            WORKER_TIMEOUT,
            Some(repository),
        )
    }

    #[cfg(test)]
    fn start_backend(
        repository: Arc<dyn PersistenceBackend>,
        storage_available: Arc<AtomicBool>,
        response_timeout: StdDuration,
    ) -> Result<Arc<Self>, PersistenceError> {
        Self::start_backend_with_reader(repository, storage_available, response_timeout, None)
    }

    fn start_backend_with_reader(
        repository: Arc<dyn PersistenceBackend>,
        storage_available: Arc<AtomicBool>,
        response_timeout: StdDuration,
        reader: Option<Arc<SqliteRepository>>,
    ) -> Result<Arc<Self>, PersistenceError> {
        let (sender, receiver) = mpsc::sync_channel(WORK_QUEUE_CAPACITY);
        let (control, controls) = mpsc::channel();
        let availability = Arc::clone(&storage_available);
        let thread = thread::Builder::new()
            .name("clipboard-persistence".to_owned())
            .spawn(move || {
                run_persistence_worker(repository, receiver, controls, availability);
            })
            .map_err(PersistenceError::WorkerStart)?;
        Ok(Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            control,
            thread: Mutex::new(Some(thread)),
            storage_available,
            accepted: AtomicUsize::new(0),
            response_timeout,
            reader,
        }))
    }

    pub fn save_settings(&self, settings: UserSettings) -> Result<(), PersistenceError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.enqueue(PersistenceCommand::SaveSettings(settings, reply))?;
        let result = response
            .recv_timeout(self.response_timeout)
            .map_err(|_| self.degrade())?;
        result.map_err(|error| self.degrade_with(error))
    }

    pub fn update_storage_policy(
        &self,
        storage_limit: StorageLimit,
        evict_favorites_when_full: bool,
    ) -> Result<usize, PersistenceError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.enqueue(PersistenceCommand::UpdateStoragePolicy(
            storage_limit,
            evict_favorites_when_full,
            reply,
        ))?;
        let result = response
            .recv_timeout(self.response_timeout)
            .map_err(|_| self.degrade())?;
        result.map_err(|error| self.degrade_with(error))
    }

    pub fn save_excluded_applications(
        &self,
        applications: &[String],
    ) -> Result<(), PersistenceError> {
        self.request(|reply| {
            PersistenceCommand::SaveExcludedApplications(applications.to_vec(), reply)
        })
    }

    pub(crate) fn save_recognition(
        &self,
        id: RecordId,
        ocr_text: Option<String>,
        qr_text: Option<String>,
        status: impl Into<String>,
    ) -> Result<(), PersistenceError> {
        self.request(|reply| {
            PersistenceCommand::SaveRecognition(id, ocr_text, qr_text, status.into(), reply)
        })
    }

    pub fn prune(
        &self,
        retention: RetentionPeriod,
        now: DateTime<Utc>,
    ) -> Result<usize, PersistenceError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.enqueue(PersistenceCommand::Prune(retention, now, reply))?;
        let result = response
            .recv_timeout(self.response_timeout)
            .map_err(|_| self.degrade())?;
        result.map_err(|error| self.degrade_with(error))
    }

    pub(crate) fn backup(&self, destination: PathBuf) -> Result<(), PersistenceError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.enqueue(PersistenceCommand::Backup(destination, reply))?;
        response
            .recv_timeout(BACKUP_WORKER_TIMEOUT)
            .map_err(|_| PersistenceError::WorkerUnavailable)?
    }

    pub(crate) fn restore(
        &self,
        source: PathBuf,
        budget: RestoreBudget,
    ) -> Result<RestoredData, PersistenceError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.enqueue(PersistenceCommand::Restore(source, budget, reply))?;
        response
            .recv_timeout(BACKUP_WORKER_TIMEOUT)
            .map_err(|_| PersistenceError::WorkerUnavailable)?
    }

    pub(crate) fn move_storage(
        &self,
        destination: PathBuf,
        app_data: PathBuf,
    ) -> Result<StorageLocation, PersistenceError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.enqueue(PersistenceCommand::MoveStorage(
            destination,
            app_data,
            reply,
        ))?;
        response
            .recv_timeout(BACKUP_WORKER_TIMEOUT)
            .map_err(|_| PersistenceError::WorkerUnavailable)?
    }

    pub(crate) fn storage_location(&self, app_data: &Path) -> Option<StorageLocation> {
        self.reader
            .as_ref()
            .map(|repository| storage_location::describe(&repository.path(), app_data))
    }

    fn enqueue(&self, command: PersistenceCommand) -> Result<(), PersistenceError> {
        if !self.storage_available.load(Ordering::Acquire) {
            return Err(PersistenceError::WorkerUnavailable);
        }
        let sender = lock_unpoisoned(&self.sender);
        let result = sender
            .as_ref()
            .ok_or(PersistenceError::WorkerUnavailable)?
            .try_send(command)
            .map_err(|_| PersistenceError::WorkerUnavailable);
        if result.is_ok() {
            self.accepted.fetch_add(1, Ordering::AcqRel);
        } else {
            let _ = self.degrade();
        }
        result
    }

    fn request(
        &self,
        command: impl FnOnce(mpsc::SyncSender<Result<(), PersistenceError>>) -> PersistenceCommand,
    ) -> Result<(), PersistenceError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.enqueue(command(reply))?;
        let result = response
            .recv_timeout(self.response_timeout)
            .map_err(|_| self.degrade())?;
        result.map_err(|error| self.degrade_with(error))
    }

    fn stop(&self) {
        let sender = lock_unpoisoned(&self.sender).take();
        let mut acknowledged = false;
        if sender.is_some() {
            let (flushed, flush_ack) = mpsc::channel();
            if self
                .control
                .send(PersistenceControl::FlushAndShutdown {
                    accepted: self.accepted.load(Ordering::Acquire),
                    flushed,
                })
                .is_ok()
            {
                acknowledged = flush_ack.recv_timeout(self.response_timeout).is_ok();
            }
        }
        if let Some(thread) = lock_unpoisoned(&self.thread).take() {
            if acknowledged {
                let _ = thread.join();
            } else {
                self.storage_available.store(false, Ordering::Release);
                let _ = THREAD_REAPER.send(thread);
            }
        }
    }

    fn degrade(&self) -> PersistenceError {
        self.storage_available.store(false, Ordering::Release);
        PersistenceError::WorkerUnavailable
    }

    fn degrade_with(&self, error: PersistenceError) -> PersistenceError {
        self.storage_available.store(false, Ordering::Release);
        error
    }
}

impl RecordPersistence for PersistenceWorker {
    fn save_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        self.request(|reply| PersistenceCommand::SaveRecord(record.clone(), reply))
    }

    fn update_note(&self, id: RecordId, note: Option<&RecordNote>) -> Result<(), PersistenceError> {
        self.request(|reply| PersistenceCommand::UpdateNote(id, note.cloned(), reply))
    }

    fn update_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        self.request(|reply| PersistenceCommand::SaveRecord(record.clone(), reply))
    }

    fn delete_record(&self, id: RecordId) -> Result<(), PersistenceError> {
        self.request(|reply| PersistenceCommand::DeleteRecord(id, reply))
    }

    fn clear_records(&self) -> Result<usize, PersistenceError> {
        let (reply, response) = mpsc::sync_channel(1);
        self.enqueue(PersistenceCommand::ClearRecords(reply))?;
        let result = response
            .recv_timeout(self.response_timeout)
            .map_err(|_| self.degrade())?;
        result.map_err(|error| self.degrade_with(error))
    }

    fn load_page(&self, query: HistoryQuery) -> Result<HistoryPage, PersistenceError> {
        if !self.storage_available.load(Ordering::Acquire) {
            return Err(PersistenceError::WorkerUnavailable);
        }
        self.reader
            .as_ref()
            .ok_or(PersistenceError::WorkerUnavailable)?
            .load_page(query)
    }

    fn search_history(
        &self,
        query: crate::services::search::SearchQuery,
    ) -> Result<crate::services::search::SearchPage, PersistenceError> {
        if !self.storage_available.load(Ordering::Acquire) {
            return Err(PersistenceError::WorkerUnavailable);
        }
        self.reader
            .as_ref()
            .ok_or(PersistenceError::WorkerUnavailable)?
            .search_history(query)
    }

    fn record_details(&self, id: RecordId) -> Result<PersistedRecordDetails, PersistenceError> {
        if !self.storage_available.load(Ordering::Acquire) {
            return Err(PersistenceError::WorkerUnavailable);
        }
        self.reader
            .as_ref()
            .ok_or(PersistenceError::WorkerUnavailable)?
            .record_details(id)
    }

    fn full_record(&self, id: RecordId) -> Result<ClipboardRecord, PersistenceError> {
        if !self.storage_available.load(Ordering::Acquire) {
            return Err(PersistenceError::WorkerUnavailable);
        }
        self.reader
            .as_ref()
            .ok_or(PersistenceError::WorkerUnavailable)?
            .full_record(id)
    }
}

impl Drop for PersistenceWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_persistence_worker(
    repository: Arc<dyn PersistenceBackend>,
    receiver: Receiver<PersistenceCommand>,
    controls: Receiver<PersistenceControl>,
    storage_available: Arc<AtomicBool>,
) {
    let mut processed = 0_usize;
    let mut shutdown = None;
    let mut repository_healthy = true;
    loop {
        if shutdown.is_none()
            && let Ok(control) = controls.try_recv()
        {
            let PersistenceControl::FlushAndShutdown { accepted, flushed } = control;
            shutdown = Some((accepted, flushed));
        }
        if shutdown
            .as_ref()
            .is_some_and(|(accepted, _)| processed >= *accepted)
        {
            if let Some((_, flushed)) = shutdown.take() {
                let _ = flushed.send(());
            }
            break;
        }
        let command = match receiver.recv_timeout(CONTROL_POLL_INTERVAL) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if let Some((_, flushed)) = shutdown.take() {
                    let _ = flushed.send(());
                }
                break;
            }
        };
        if !repository_healthy {
            reply_unavailable(command);
            processed = processed.saturating_add(1);
            continue;
        }
        match command {
            PersistenceCommand::SaveRecord(record, reply) => {
                let result = repository.persist_record(&record);
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::UpdateNote(id, note, reply) => {
                let result = repository.persist_note(id, note.as_ref());
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::DeleteRecord(id, reply) => {
                let result = repository.delete_record(id);
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::ClearRecords(reply) => {
                let result = repository.clear_records();
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::SaveSettings(settings, reply) => {
                let result = repository.persist_settings(settings);
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::UpdateStoragePolicy(
                storage_limit,
                evict_favorites_when_full,
                reply,
            ) => {
                let result =
                    repository.update_storage_policy(storage_limit, evict_favorites_when_full);
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::SaveExcludedApplications(applications, reply) => {
                let result = repository.persist_excluded_applications(&applications);
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::SaveRecognition(id, ocr_text, qr_text, status, reply) => {
                let result = repository.persist_recognition(
                    id,
                    ocr_text.as_deref(),
                    qr_text.as_deref(),
                    &status,
                );
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::Prune(retention, now, reply) => {
                let result = repository.prune_records(retention, now);
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
            }
            PersistenceCommand::Backup(destination, reply) => {
                let _ = reply.send(repository.backup_database(&destination));
            }
            PersistenceCommand::Restore(source, budget, reply) => {
                let result = repository.restore_database(&source, budget);
                let terminal = matches!(result, Err(PersistenceError::RestoreRollbackFailed));
                if terminal {
                    storage_available.store(false, Ordering::Release);
                    repository_healthy = false;
                }
                let _ = reply.send(result);
            }
            PersistenceCommand::MoveStorage(destination, app_data, reply) => {
                let _ = reply.send(repository.move_storage(&destination, &app_data));
            }
        }
        processed = processed.saturating_add(1);
    }
}

fn reply_unavailable(command: PersistenceCommand) {
    match command {
        PersistenceCommand::SaveRecord(_, reply)
        | PersistenceCommand::UpdateNote(_, _, reply)
        | PersistenceCommand::DeleteRecord(_, reply)
        | PersistenceCommand::SaveSettings(_, reply)
        | PersistenceCommand::SaveExcludedApplications(_, reply)
        | PersistenceCommand::SaveRecognition(_, _, _, _, reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
        PersistenceCommand::UpdateStoragePolicy(_, _, reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
        PersistenceCommand::ClearRecords(reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
        PersistenceCommand::Prune(_, _, reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
        PersistenceCommand::Backup(_, reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
        PersistenceCommand::Restore(_, _, reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
        PersistenceCommand::MoveStorage(_, _, reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
    }
}

pub struct SqliteRepository {
    state: Mutex<RepositoryState>,
    quota: DiskQuota,
    migration_locks: Arc<dyn MigrationLockProvider>,
    #[cfg(test)]
    fail_next_restore_snapshot: AtomicBool,
    #[cfg(test)]
    fail_next_restore_rollback: AtomicBool,
    #[cfg(test)]
    pause_after_restore_source_snapshot: Mutex<Option<Arc<(Barrier, Barrier)>>>,
}

struct RepositoryState {
    path: PathBuf,
    connection: Connection,
}

#[cfg(test)]
impl std::ops::Deref for RepositoryState {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

#[cfg(test)]
impl std::ops::DerefMut for RepositoryState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.connection
    }
}

impl SqliteRepository {
    pub fn open_app_data(directory: &Path) -> Result<Arc<Self>, PersistenceError> {
        fs::create_dir_all(directory).map_err(PersistenceError::CreateDirectory)?;
        Self::open(directory.join(DATABASE_FILE))
    }

    pub fn open(path: PathBuf) -> Result<Arc<Self>, PersistenceError> {
        Self::open_with_quota(path, DiskQuota::default())
    }

    pub(crate) fn open_with_quota(
        path: PathBuf,
        quota: DiskQuota,
    ) -> Result<Arc<Self>, PersistenceError> {
        Self::open_with_quota_and_file_ops(path, quota, &StdMigrationFileOps)
    }

    fn open_with_quota_and_file_ops(
        path: PathBuf,
        quota: DiskQuota,
        file_ops: &dyn MigrationFileOps,
    ) -> Result<Arc<Self>, PersistenceError> {
        Self::open_with_dependencies(
            path,
            quota,
            file_ops,
            Arc::new(StdMigrationLockProvider),
            &NoopMigrationHooks,
        )
    }

    fn open_with_dependencies(
        path: PathBuf,
        quota: DiskQuota,
        file_ops: &dyn MigrationFileOps,
        migration_locks: Arc<dyn MigrationLockProvider>,
        hooks: &dyn MigrationHooks,
    ) -> Result<Arc<Self>, PersistenceError> {
        let migration_guard = migration_locks.acquire(&path, MIGRATION_LOCK_TIMEOUT)?;
        recover_interrupted_migration(&path, file_ops)?;
        migrate_legacy_database_with_hooks(&path, quota, Utc::now(), file_ops, hooks)?;
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut connection)?;
        drop(migration_guard);
        Ok(Arc::new(Self {
            state: Mutex::new(RepositoryState { path, connection }),
            quota,
            migration_locks,
            #[cfg(test)]
            fail_next_restore_snapshot: AtomicBool::new(false),
            #[cfg(test)]
            fail_next_restore_rollback: AtomicBool::new(false),
            #[cfg(test)]
            pause_after_restore_source_snapshot: Mutex::new(None),
        }))
    }

    pub fn path(&self) -> PathBuf {
        lock_unpoisoned(&self.state).path.clone()
    }

    pub fn schema_version(&self) -> Result<i64, PersistenceError> {
        lock_unpoisoned(&self.state)
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    fn backup_to(&self, destination: &Path) -> Result<(), PersistenceError> {
        let state = lock_unpoisoned(&self.state);
        if same_file_path(&state.path, destination) {
            return Err(PersistenceError::InvalidData);
        }
        let snapshot = backup_work_path(destination, ".snapshot");
        remove_path_and_sidecars(&snapshot)?;
        let result = (|| {
            state
                .connection
                .backup(rusqlite::MAIN_DB, &snapshot, None)?;
            validate_backup_file(&snapshot)?;
            backup::create_archive(&snapshot, None, destination, env!("CARGO_PKG_VERSION"))?;
            Ok(())
        })();
        let _ = remove_path_and_sidecars(&snapshot);
        result
    }

    pub(crate) fn move_to_directory(
        &self,
        destination: &Path,
        app_data: &Path,
    ) -> Result<StorageLocation, PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let current_directory = state
            .path
            .parent()
            .ok_or(PersistenceError::InvalidData)?
            .to_path_buf();
        storage_location::validate_destination(destination, &current_directory)?;
        fs::create_dir_all(destination).map_err(PersistenceError::CreateDirectory)?;
        let staging = destination.join(format!(".migrating-{}", uuid::Uuid::new_v4()));
        let staged_database = staging.join(DATABASE_FILE);
        let destination_database = destination.join(DATABASE_FILE);
        let result = (|| {
            fs::create_dir(&staging).map_err(PersistenceError::CreateDirectory)?;
            state
                .connection
                .backup(rusqlite::MAIN_DB, &staged_database, None)?;
            validate_backup_file(&staged_database)?;
            fs::rename(&staged_database, &destination_database)
                .map_err(PersistenceError::FileOperation)?;
            let replacement = Connection::open(&destination_database)?;
            replacement.busy_timeout(std::time::Duration::from_secs(2))?;
            replacement.pragma_update(None, "foreign_keys", "ON")?;
            let integrity: String =
                replacement.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
            if integrity != "ok"
                || database_version_from_connection(&replacement)? != SCHEMA_VERSION
            {
                return Err(PersistenceError::InvalidData);
            }
            storage_location::write_bootstrap(app_data, destination)?;
            let old_path = std::mem::replace(&mut state.path, destination_database.clone());
            let old_connection = std::mem::replace(&mut state.connection, replacement);
            drop(old_connection);
            let _ = remove_path_and_sidecars(&old_path);
            Ok(storage_location::describe(&state.path, app_data))
        })();
        let _ = fs::remove_dir_all(&staging);
        if result.is_err() && state.path != destination_database {
            let _ = remove_path_and_sidecars(&destination_database);
        }
        result
    }

    pub(crate) fn restore_from(
        &self,
        source: &Path,
        budget: RestoreBudget,
    ) -> Result<RestoredData, PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        if same_file_path(&state.path, source) {
            return Err(PersistenceError::InvalidData);
        }
        let staging_parent = state.path.parent().ok_or(PersistenceError::InvalidData)?;
        let prepared = backup::prepare_restore(source, staging_parent)?;
        let restore_source = prepared.database_path();
        let source_snapshot = backup_work_path(&state.path, ".restore-source");
        remove_path_and_sidecars(&source_snapshot)?;
        let snapshot_result = snapshot_backup_file(restore_source, &source_snapshot);
        if let Err(error) = snapshot_result {
            let _ = remove_path_and_sidecars(&source_snapshot);
            return Err(error);
        }
        #[cfg(test)]
        if let Some(pause) = lock_unpoisoned(&self.pause_after_restore_source_snapshot).take() {
            pause.0.wait();
            pause.1.wait();
        }
        let preflight = SqliteRepository::open_with_quota(source_snapshot.clone(), self.quota)
            .and_then(|source_repository| {
                source_repository
                    .validate_restore_representation_kinds(budget.max_record_bytes)
                    .and_then(|()| source_repository.load_recent_bounded(budget).map(drop))
            });
        if let Err(error) = preflight {
            let _ = remove_path_and_sidecars(&source_snapshot);
            return Err(error);
        }

        let rollback = backup_work_path(&state.path, ".restore-rollback");
        remove_path_and_sidecars(&rollback)?;
        let connection = &mut state.connection;
        connection.backup(rusqlite::MAIN_DB, &rollback, None)?;
        let restore_result = (|| {
            connection.restore(
                rusqlite::MAIN_DB,
                &source_snapshot,
                None::<fn(rusqlite::backup::Progress)>,
            )?;
            connection.pragma_update(None, "foreign_keys", "ON")?;
            let integrity: String =
                connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
            if integrity != "ok" || database_version_from_connection(connection)? != SCHEMA_VERSION
            {
                return Err(PersistenceError::InvalidData);
            }
            validate_restore_connection(connection, budget.max_record_bytes)?;
            let transaction = connection.transaction()?;
            enforce_disk_quota(&transaction, self.quota)?;
            transaction.commit()?;
            #[cfg(test)]
            if self
                .fail_next_restore_snapshot
                .swap(false, Ordering::AcqRel)
            {
                return Err(PersistenceError::WorkerUnavailable);
            }
            let settings = load_settings_from_connection(connection)?;
            let excluded_applications = load_excluded_applications_from_connection(connection)?;
            let groups = load_groups_from_connection(connection)?;
            let page = load_page_from_connection(
                connection,
                HistoryQuery {
                    limit: 100,
                    ..HistoryQuery::default()
                },
            )?;
            Ok(RestoredData {
                settings,
                excluded_applications,
                groups,
                page,
            })
        })();
        let restored = match restore_result {
            Ok(restored) => restored,
            Err(error) => {
                #[cfg(test)]
                let rollback_result = if self
                    .fail_next_restore_rollback
                    .swap(false, Ordering::AcqRel)
                {
                    Err(rusqlite::Error::InvalidQuery)
                } else {
                    connection.restore(
                        rusqlite::MAIN_DB,
                        &rollback,
                        None::<fn(rusqlite::backup::Progress)>,
                    )
                };
                #[cfg(not(test))]
                let rollback_result = connection.restore(
                    rusqlite::MAIN_DB,
                    &rollback,
                    None::<fn(rusqlite::backup::Progress)>,
                );
                let _ = remove_path_and_sidecars(&rollback);
                let _ = remove_path_and_sidecars(&source_snapshot);
                return rollback_result
                    .map_or(Err(PersistenceError::RestoreRollbackFailed), |_| Err(error));
            }
        };
        let _ = remove_path_and_sidecars(&rollback);
        let _ = remove_path_and_sidecars(&source_snapshot);
        Ok(restored)
    }

    #[cfg(test)]
    fn fail_next_restore_snapshot(&self) {
        self.fail_next_restore_snapshot
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn fail_next_restore_rollback(&self) {
        self.fail_next_restore_rollback
            .store(true, Ordering::Release);
    }

    #[cfg(test)]
    fn pause_after_restore_source_snapshot(&self, pause: Arc<(Barrier, Barrier)>) {
        *lock_unpoisoned(&self.pause_after_restore_source_snapshot) = Some(pause);
    }

    fn validate_restore_representation_kinds(
        &self,
        max_record_bytes: usize,
    ) -> Result<(), PersistenceError> {
        let state = lock_unpoisoned(&self.state);
        validate_restore_connection(&state.connection, max_record_bytes)
    }

    pub fn load_settings(&self) -> Result<UserSettings, PersistenceError> {
        load_settings_from_connection(&lock_unpoisoned(&self.state).connection)
    }

    pub fn load_groups(&self) -> Result<Vec<(GroupId, String)>, PersistenceError> {
        load_groups_from_connection(&lock_unpoisoned(&self.state).connection)
    }

    pub fn save_group(&self, id: GroupId, name: &str) -> Result<(), PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        transaction.execute(
            "INSERT INTO clipboard_groups(id, name, position) \
             VALUES (?1, ?2, COALESCE((SELECT MAX(position) + 1 FROM clipboard_groups), 0)) \
             ON CONFLICT(id) DO UPDATE SET name = excluded.name",
            params![id.as_uuid().to_string(), name],
        )?;
        refresh_group_search_records(&transaction, id)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn save_group_order(&self, ids: &[GroupId]) -> Result<(), PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        for (position, id) in ids.iter().enumerate() {
            let changed = transaction.execute(
                "UPDATE clipboard_groups SET position = ?1 WHERE id = ?2",
                params![position as i64, id.as_uuid().to_string()],
            )?;
            if changed != 1 {
                return Err(PersistenceError::InvalidData);
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_group(&self, id: GroupId) -> Result<usize, PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        let record_ids = record_ids_in_group(&transaction, id)?;
        let moved = transaction.execute(
            "UPDATE clipboard_records SET group_id = NULL WHERE group_id = ?1",
            [id.as_uuid().to_string()],
        )?;
        let deleted = transaction.execute(
            "DELETE FROM clipboard_groups WHERE id = ?1",
            [id.as_uuid().to_string()],
        )?;
        if deleted != 1 {
            return Err(PersistenceError::InvalidData);
        }
        for record_id in record_ids {
            crate::services::search::refresh_search_record(&transaction, &record_id)?;
        }
        transaction.commit()?;
        Ok(moved)
    }

    pub fn save_settings(&self, settings: UserSettings) -> Result<(), PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        save_setting(&transaction, "language", language_value(settings.language))?;
        save_setting(
            &transaction,
            "retention",
            retention_value(settings.retention),
        )?;
        save_setting(
            &transaction,
            "storage_limit",
            storage_limit_value(settings.storage_limit),
        )?;
        save_setting(
            &transaction,
            "evict_favorites_when_full",
            bool_value(settings.evict_favorites_when_full),
        )?;
        save_setting(
            &transaction,
            "start_at_sign_in",
            bool_value(settings.start_at_sign_in),
        )?;
        save_setting(
            &transaction,
            "start_minimized",
            bool_value(settings.start_minimized),
        )?;
        save_setting(
            &transaction,
            "show_tray_icon",
            bool_value(settings.show_tray_icon),
        )?;
        save_setting(
            &transaction,
            "accent_color",
            accent_color_value(settings.accent_color),
        )?;
        save_setting(
            &transaction,
            "sound_enabled",
            bool_value(settings.sound_enabled),
        )?;
        save_setting(
            &transaction,
            "capture_sound",
            capture_sound_value(settings.capture_sound),
        )?;
        save_setting(
            &transaction,
            "quick_paste_enabled",
            bool_value(settings.quick_paste_enabled),
        )?;
        save_setting(
            &transaction,
            "offline_ocr_enabled",
            bool_value(settings.offline_ocr_enabled),
        )?;
        save_setting(
            &transaction,
            "qr_recognition_enabled",
            bool_value(settings.qr_recognition_enabled),
        )?;
        let activation_shortcut = serde_json::to_string(&settings.activation_shortcut)
            .map_err(|_| PersistenceError::InvalidData)?;
        let group_shortcut_modifiers = serde_json::to_string(&settings.group_shortcut_modifiers)
            .map_err(|_| PersistenceError::InvalidData)?;
        let quick_paste_modifiers = serde_json::to_string(&settings.quick_paste_modifiers)
            .map_err(|_| PersistenceError::InvalidData)?;
        save_setting(&transaction, "activation_shortcut", &activation_shortcut)?;
        save_setting(
            &transaction,
            "group_shortcut_modifiers",
            &group_shortcut_modifiers,
        )?;
        save_setting(
            &transaction,
            "quick_paste_modifiers",
            &quick_paste_modifiers,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn save_recognition(
        &self,
        id: RecordId,
        ocr_text: Option<&str>,
        qr_text: Option<&str>,
        status: &str,
    ) -> Result<(), PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        if !record_exists(&transaction, id)? {
            return Err(PersistenceError::InvalidData);
        }
        transaction.execute(
            "INSERT INTO clipboard_recognition(record_id, ocr_text, qr_text, status, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(record_id) DO UPDATE SET
                ocr_text = excluded.ocr_text,
                qr_text = excluded.qr_text,
                status = excluded.status,
                updated_at = excluded.updated_at",
            params![
                id.as_uuid().to_string(),
                ocr_text,
                qr_text,
                status,
                Utc::now().to_rfc3339()
            ],
        )?;
        crate::services::search::refresh_search_record(&transaction, &id.as_uuid().to_string())?;
        transaction.commit()?;
        Ok(())
    }

    pub fn update_storage_policy(
        &self,
        storage_limit: StorageLimit,
        evict_favorites_when_full: bool,
    ) -> Result<usize, PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        save_setting(
            &transaction,
            "storage_limit",
            storage_limit_value(storage_limit),
        )?;
        save_setting(
            &transaction,
            "evict_favorites_when_full",
            bool_value(evict_favorites_when_full),
        )?;
        let removed = enforce_disk_quota(&transaction, self.quota)?;
        transaction.commit()?;
        incremental_vacuum(&state.connection, self.quota)?;
        Ok(removed)
    }

    pub fn load_excluded_applications(&self) -> Result<Vec<String>, PersistenceError> {
        load_excluded_applications_from_connection(&lock_unpoisoned(&self.state).connection)
    }

    pub fn save_excluded_applications(
        &self,
        applications: &[String],
    ) -> Result<(), PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        let value =
            serde_json::to_string(applications).map_err(|_| PersistenceError::InvalidData)?;
        save_setting(&transaction, "excluded_applications", &value)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn prune(
        &self,
        retention: RetentionPeriod,
        now: DateTime<Utc>,
    ) -> Result<usize, PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        let before = record_count(&transaction)?;
        if let Some(days) = retention.days() {
            let cutoff = now - Duration::days(days);
            transaction.execute(
                "DELETE FROM clipboard_records WHERE captured_at < ?1",
                [cutoff.to_rfc3339()],
            )?;
        }
        enforce_disk_quota(&transaction, self.quota)?;
        let after = record_count(&transaction)?;
        transaction.commit()?;
        incremental_vacuum(&state.connection, self.quota)?;
        Ok(before.saturating_sub(after))
    }

    pub fn load_recent(&self, limit: usize) -> Result<Vec<ClipboardRecord>, PersistenceError> {
        self.load_recent_bounded(RestoreBudget {
            max_records: limit,
            max_total_bytes: usize::MAX,
            max_record_bytes: usize::MAX,
        })
    }

    pub fn load_page(&self, query: HistoryQuery) -> Result<HistoryPage, PersistenceError> {
        let state = lock_unpoisoned(&self.state);
        load_page_from_connection(&state.connection, query)
    }

    pub fn search_history(
        &self,
        query: crate::services::search::SearchQuery,
    ) -> Result<crate::services::search::SearchPage, PersistenceError> {
        let state = lock_unpoisoned(&self.state);
        crate::services::search::search_connection(&state.connection, query)
    }

    pub fn full_record(&self, id: RecordId) -> Result<ClipboardRecord, PersistenceError> {
        let state = lock_unpoisoned(&self.state);
        let row = load_db_record(&state.connection, id)?;
        validate_record_metadata(&state.connection, &row)?;
        let representations = load_representations(&state.connection, &row.id)?;
        let note = load_note(&state.connection, &row.id)?;
        row.into_record(representations, note)
    }

    pub fn record_details(&self, id: RecordId) -> Result<PersistedRecordDetails, PersistenceError> {
        let state = lock_unpoisoned(&self.state);
        let row = load_db_record(&state.connection, id)?;
        validate_record_metadata(&state.connection, &row)?;
        let representations = load_representation_details(&state.connection, &row.id)?;
        let content_kind = content_kind_from_details(&representations);
        let note = load_note(&state.connection, &row.id)?
            .map(RecordNote::new)
            .transpose()
            .map_err(|_| PersistenceError::InvalidData)?;
        Ok(PersistedRecordDetails {
            id: RecordId::parse(&row.id).map_err(|_| PersistenceError::InvalidData)?,
            content_identity: ContentIdentity::new(row.content_identity),
            captured_at: DateTime::parse_from_rfc3339(&row.captured_at)
                .map_err(|_| PersistenceError::InvalidData)?
                .with_timezone(&Utc),
            source_application: row.source_application,
            representations,
            note,
            group_id: row
                .group_id
                .map(|value| GroupId::parse(&value))
                .transpose()
                .map_err(|_| PersistenceError::InvalidData)?,
            content_kind,
            pinned: row.pinned,
            favorite: row.favorite,
            sensitive: row.sensitive,
        })
    }

    pub(crate) fn load_recent_bounded(
        &self,
        budget: RestoreBudget,
    ) -> Result<Vec<ClipboardRecord>, PersistenceError> {
        if budget.max_records == 0 || budget.max_total_bytes == 0 {
            return Ok(Vec::new());
        }
        let state = lock_unpoisoned(&self.state);
        let mut statement = state.connection.prepare(
            "SELECT id, content_identity, captured_at, source_application, source_path, \
                    typeof(note), length(CAST(note AS BLOB)), group_id, pinned, favorite, sensitive \
             FROM clipboard_records ORDER BY captured_at DESC, id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(DbRecord {
                id: row.get(0)?,
                content_identity: row.get(1)?,
                captured_at: row.get(2)?,
                source_application: row.get(3)?,
                source_path: row.get(4)?,
                note_storage_type: row.get(5)?,
                note_length: row.get(6)?,
                group_id: row.get(7)?,
                pinned: row.get(8)?,
                favorite: row.get(9)?,
                sensitive: row.get(10)?,
            })
        })?;
        let mut records = Vec::new();
        let mut total_bytes = 0_usize;
        for row in rows {
            let row = row?;
            let Some(record_bytes) = representation_metadata_bytes(&state.connection, &row)? else {
                continue;
            };
            if record_bytes > budget.max_record_bytes {
                continue;
            }
            let Some(next_total) = total_bytes.checked_add(record_bytes) else {
                break;
            };
            if next_total > budget.max_total_bytes {
                break;
            }
            let Ok(representations) = load_representations(&state.connection, &row.id) else {
                continue;
            };
            let Ok(note) = load_note(&state.connection, &row.id) else {
                continue;
            };
            let Ok(record) = row.into_record(representations, note) else {
                continue;
            };
            records.push(record);
            total_bytes = next_total;
            if records.len() >= budget.max_records {
                break;
            }
        }
        Ok(records)
    }

    fn save_record_inner(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        write_record(&transaction, record)?;
        let removed = enforce_disk_quota(&transaction, self.quota)?;
        if !record_exists(&transaction, record.id)? {
            return Err(PersistenceError::InvalidData);
        }
        transaction.commit()?;
        if removed > 0 {
            incremental_vacuum(&state.connection, self.quota)?;
        }
        Ok(())
    }
}

fn legacy_quick_paste_modifiers(value: &str) -> Option<ShortcutModifiers> {
    match value {
        "ctrl_alt" => Some(ShortcutModifiers::CTRL_ALT),
        "ctrl_shift" => Some(ShortcutModifiers::CTRL_SHIFT),
        "alt_shift" => Some(ShortcutModifiers {
            alt: true,
            shift: true,
            ..ShortcutModifiers::default()
        }),
        _ => None,
    }
}

#[cfg(test)]
fn save_record_transaction(
    connection: &mut Connection,
    record: &ClipboardRecord,
) -> Result<(), PersistenceError> {
    let transaction = connection.transaction()?;
    write_record(&transaction, record)?;
    transaction.commit()?;
    Ok(())
}

fn write_record(
    transaction: &Transaction<'_>,
    record: &ClipboardRecord,
) -> Result<(), PersistenceError> {
    validate_record_file_lists(record)?;
    transaction.execute(
        "INSERT INTO clipboard_records (
                id, content_identity, captured_at, source_application, source_path, note,
                group_id, pinned, favorite, sensitive, content_kind
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                content_identity = excluded.content_identity,
                captured_at = excluded.captured_at,
                source_application = excluded.source_application,
                source_path = excluded.source_path,
                note = excluded.note,
                group_id = excluded.group_id,
                pinned = excluded.pinned,
                favorite = excluded.favorite,
                sensitive = excluded.sensitive,
                content_kind = excluded.content_kind",
        params![
            record.id.as_uuid().to_string(),
            record.content_identity.as_str(),
            record.captured_at.to_rfc3339(),
            record.source.application_name,
            record.source.executable_path,
            record.note.as_ref().map(RecordNote::as_str),
            record.group_id.map(|id| id.as_uuid().to_string()),
            record.pinned,
            record.favorite,
            record.sensitive,
            content_kind_value(record.content_kind()),
        ],
    )?;
    transaction.execute(
        "DELETE FROM clipboard_representations WHERE record_id = ?1",
        [record.id.as_uuid().to_string()],
    )?;
    for (position, representation) in record.representations.iter().enumerate() {
        let file_list_json;
        let (kind, text_value, blob_value): (&str, Option<&str>, Option<&[u8]>) =
            match representation {
                ClipboardRepresentation::UnicodeText { text } => {
                    ("unicode_text", Some(text.as_str()), None)
                }
                ClipboardRepresentation::Png { bytes } => ("png", None, Some(bytes.as_slice())),
                ClipboardRepresentation::DibV5 { bytes } => {
                    ("dib_v5", None, Some(bytes.as_slice()))
                }
                ClipboardRepresentation::Rtf { bytes } => ("rtf", None, Some(bytes.as_slice())),
                ClipboardRepresentation::Html { bytes } => ("html", None, Some(bytes.as_slice())),
                ClipboardRepresentation::FileList { paths } => {
                    file_list_json =
                        serde_json::to_string(paths).map_err(|_| PersistenceError::InvalidData)?;
                    ("file_list", Some(file_list_json.as_str()), None)
                }
            };
        transaction.execute(
            "INSERT INTO clipboard_representations \
             (record_id, position, kind, text_value, blob_value) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.id.as_uuid().to_string(),
                position as i64,
                kind,
                text_value,
                blob_value,
            ],
        )?;
    }
    crate::services::search::refresh_search_record(transaction, &record.id.as_uuid().to_string())?;
    Ok(())
}

impl RecordPersistence for SqliteRepository {
    fn save_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        self.save_record_inner(record)
    }

    fn update_note(&self, id: RecordId, note: Option<&RecordNote>) -> Result<(), PersistenceError> {
        let mut state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let transaction = state.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE clipboard_records SET note = ?1 WHERE id = ?2",
            params![note.map(RecordNote::as_str), id.as_uuid().to_string()],
        )?;
        if changed == 0 {
            return Err(PersistenceError::InvalidData);
        }
        crate::services::search::refresh_search_record(&transaction, &id.as_uuid().to_string())?;
        let removed = enforce_disk_quota(&transaction, self.quota)?;
        if !record_exists(&transaction, id)? {
            return Err(PersistenceError::InvalidData);
        }
        transaction.commit()?;
        if removed > 0 {
            incremental_vacuum(&state.connection, self.quota)?;
        }
        Ok(())
    }

    fn update_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        self.save_record_inner(record)
    }

    fn delete_record(&self, id: RecordId) -> Result<(), PersistenceError> {
        let state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        state.connection.execute(
            "DELETE FROM clipboard_records WHERE id = ?1",
            [id.as_uuid().to_string()],
        )?;
        incremental_vacuum(&state.connection, self.quota)?;
        Ok(())
    }

    fn clear_records(&self) -> Result<usize, PersistenceError> {
        let state = lock_unpoisoned(&self.state);
        let _migration_guard = self
            .migration_locks
            .acquire(&state.path, MIGRATION_LOCK_TIMEOUT)?;
        let removed = state
            .connection
            .execute("DELETE FROM clipboard_records", [])?;
        incremental_vacuum(&state.connection, self.quota)?;
        Ok(removed)
    }

    fn load_page(&self, query: HistoryQuery) -> Result<HistoryPage, PersistenceError> {
        SqliteRepository::load_page(self, query)
    }

    fn search_history(
        &self,
        query: crate::services::search::SearchQuery,
    ) -> Result<crate::services::search::SearchPage, PersistenceError> {
        SqliteRepository::search_history(self, query)
    }

    fn record_details(&self, id: RecordId) -> Result<PersistedRecordDetails, PersistenceError> {
        SqliteRepository::record_details(self, id)
    }

    fn full_record(&self, id: RecordId) -> Result<ClipboardRecord, PersistenceError> {
        SqliteRepository::full_record(self, id)
    }
}

impl PersistenceBackend for SqliteRepository {
    fn persist_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        self.save_record_inner(record)
    }

    fn persist_note(
        &self,
        id: RecordId,
        note: Option<&RecordNote>,
    ) -> Result<(), PersistenceError> {
        RecordPersistence::update_note(self, id, note)
    }

    fn delete_record(&self, id: RecordId) -> Result<(), PersistenceError> {
        RecordPersistence::delete_record(self, id)
    }

    fn clear_records(&self) -> Result<usize, PersistenceError> {
        RecordPersistence::clear_records(self)
    }

    fn persist_settings(&self, settings: UserSettings) -> Result<(), PersistenceError> {
        SqliteRepository::save_settings(self, settings)
    }

    fn update_storage_policy(
        &self,
        storage_limit: StorageLimit,
        evict_favorites_when_full: bool,
    ) -> Result<usize, PersistenceError> {
        SqliteRepository::update_storage_policy(self, storage_limit, evict_favorites_when_full)
    }

    fn persist_excluded_applications(
        &self,
        applications: &[String],
    ) -> Result<(), PersistenceError> {
        SqliteRepository::save_excluded_applications(self, applications)
    }

    fn persist_recognition(
        &self,
        id: RecordId,
        ocr_text: Option<&str>,
        qr_text: Option<&str>,
        status: &str,
    ) -> Result<(), PersistenceError> {
        self.save_recognition(id, ocr_text, qr_text, status)
    }

    fn prune_records(
        &self,
        retention: RetentionPeriod,
        now: DateTime<Utc>,
    ) -> Result<usize, PersistenceError> {
        SqliteRepository::prune(self, retention, now)
    }

    fn backup_database(&self, destination: &Path) -> Result<(), PersistenceError> {
        self.backup_to(destination)
    }

    fn restore_database(
        &self,
        source: &Path,
        budget: RestoreBudget,
    ) -> Result<RestoredData, PersistenceError> {
        self.restore_from(source, budget)
    }

    fn move_storage(
        &self,
        destination: &Path,
        app_data: &Path,
    ) -> Result<StorageLocation, PersistenceError> {
        self.move_to_directory(destination, app_data)
    }
}

fn same_file_path(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        fs::canonicalize(path).unwrap_or_else(|_| {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            path.file_name()
                .map_or(parent.clone(), |name| parent.join(name))
        })
    };
    normalize(left) == normalize(right)
}

fn backup_work_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_path_and_sidecars(path: &Path) -> Result<(), PersistenceError> {
    for candidate in [
        path.to_path_buf(),
        sqlite_sidecar_path(path, "-wal"),
        sqlite_sidecar_path(path, "-shm"),
        sqlite_sidecar_path(path, "-journal"),
    ] {
        if candidate.exists() {
            fs::remove_file(candidate).map_err(PersistenceError::FileOperation)?;
        }
    }
    Ok(())
}

fn validate_backup_file(path: &Path) -> Result<(), PersistenceError> {
    if !path.is_file() {
        return Err(PersistenceError::InvalidData);
    }
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(PersistenceError::InvalidData);
    }
    let version = database_version_from_connection(&connection)?;
    if version != SCHEMA_VERSION {
        return Err(if version > SCHEMA_VERSION {
            PersistenceError::UnsupportedSchema(version)
        } else {
            PersistenceError::InvalidData
        });
    }
    Ok(())
}

fn snapshot_backup_file(source: &Path, destination: &Path) -> Result<(), PersistenceError> {
    if !source.is_file() {
        return Err(PersistenceError::InvalidData);
    }
    let connection =
        Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.backup(rusqlite::MAIN_DB, destination, None)?;
    validate_backup_file(destination)
}

fn validate_restore_connection(
    connection: &Connection,
    max_record_bytes: usize,
) -> Result<(), PersistenceError> {
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" || database_version_from_connection(connection)? != SCHEMA_VERSION {
        return Err(PersistenceError::InvalidData);
    }
    let unsupported = connection
        .query_row(
            "SELECT kind FROM clipboard_representations
             WHERE kind NOT IN ('unicode_text', 'rtf', 'html', 'png', 'dib_v5', 'file_list')
             ORDER BY kind LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(kind) = unsupported {
        return Err(PersistenceError::UnsupportedRepresentationKind(kind));
    }
    let mut ids = connection.prepare(
        "SELECT id, typeof(note), length(CAST(note AS BLOB))
         FROM clipboard_records ORDER BY captured_at DESC, id DESC",
    )?;
    let rows = ids.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    })?;
    for id in rows {
        let (id, note_storage_type, note_length) = id?;
        let note_bytes = note_metadata_bytes(&note_storage_type, note_length)
            .ok_or(PersistenceError::InvalidData)?;
        let representation_bytes = representation_metadata_bytes_for_id(connection, &id)?
            .ok_or(PersistenceError::InvalidData)?;
        let record_bytes = note_bytes
            .checked_add(representation_bytes)
            .ok_or(PersistenceError::InvalidData)?;
        if record_bytes > max_record_bytes {
            return Err(PersistenceError::InvalidData);
        }
    }
    Ok(())
}

fn load_settings_from_connection(
    connection: &Connection,
) -> Result<UserSettings, PersistenceError> {
    let language = setting(connection, "language")?
        .and_then(|value| parse_language(&value))
        .unwrap_or_default();
    let retention = setting(connection, "retention")?
        .and_then(|value| parse_retention(&value))
        .unwrap_or_default();
    let storage_limit = setting(connection, "storage_limit")?
        .and_then(|value| parse_storage_limit(&value))
        .unwrap_or_default();
    let evict_favorites_when_full = setting(connection, "evict_favorites_when_full")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false);
    let start_at_sign_in = setting(connection, "start_at_sign_in")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false);
    let start_minimized = setting(connection, "start_minimized")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false);
    let show_tray_icon = setting(connection, "show_tray_icon")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(true);
    let accent_color = setting(connection, "accent_color")?
        .and_then(|value| parse_accent_color(&value))
        .unwrap_or_default();
    let sound_enabled = setting(connection, "sound_enabled")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(true);
    let capture_sound = setting(connection, "capture_sound")?
        .and_then(|value| parse_capture_sound(&value))
        .unwrap_or_default();
    let quick_paste_enabled = setting(connection, "quick_paste_enabled")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false);
    let offline_ocr_enabled = setting(connection, "offline_ocr_enabled")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false);
    let qr_recognition_enabled = setting(connection, "qr_recognition_enabled")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false);
    let activation_shortcut = setting(connection, "activation_shortcut")?
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or_default();
    let group_shortcut_modifiers = setting(connection, "group_shortcut_modifiers")?
        .and_then(|value| serde_json::from_str(&value).ok())
        .unwrap_or(ShortcutModifiers::CTRL_ALT);
    let quick_paste_modifiers = setting(connection, "quick_paste_modifiers")?
        .and_then(|value| serde_json::from_str(&value).ok())
        .or_else(|| {
            setting(connection, "quick_paste_modifier")
                .ok()
                .flatten()
                .and_then(|value| legacy_quick_paste_modifiers(&value))
        })
        .unwrap_or(ShortcutModifiers::CTRL_ALT);
    Ok(UserSettings {
        language,
        retention,
        storage_limit,
        evict_favorites_when_full,
        start_at_sign_in,
        start_minimized,
        show_tray_icon,
        accent_color,
        sound_enabled,
        capture_sound,
        activation_shortcut,
        group_shortcut_modifiers,
        quick_paste_enabled,
        quick_paste_modifiers,
        offline_ocr_enabled,
        qr_recognition_enabled,
    })
}

fn load_groups_from_connection(
    connection: &Connection,
) -> Result<Vec<(GroupId, String)>, PersistenceError> {
    let mut statement = connection
        .prepare("SELECT id, name FROM clipboard_groups ORDER BY position, name COLLATE NOCASE")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut groups = Vec::new();
    for row in rows {
        let (id, name) = row?;
        groups.push((
            GroupId::parse(&id).map_err(|_| PersistenceError::InvalidData)?,
            name,
        ));
    }
    Ok(groups)
}

fn load_excluded_applications_from_connection(
    connection: &Connection,
) -> Result<Vec<String>, PersistenceError> {
    setting(connection, "excluded_applications")?
        .map(|value| serde_json::from_str(&value).map_err(|_| PersistenceError::InvalidData))
        .transpose()
        .map(Option::unwrap_or_default)
}

trait MigrationFileOps {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()>;
    fn remove_file(&self, path: &Path) -> std::io::Result<()>;
}

trait MigrationLockGuard {}

trait MigrationLockProvider: Send + Sync {
    fn acquire(
        &self,
        path: &Path,
        timeout: StdDuration,
    ) -> Result<Box<dyn MigrationLockGuard>, PersistenceError>;
}

struct StdMigrationLockProvider;

fn migration_lock_identity(path: &Path) -> u64 {
    let absolute = fs::canonicalize(path)
        .or_else(|_| {
            let file_name = path.file_name().ok_or(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "database path has no file name",
            ))?;
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            fs::canonicalize(parent).map(|parent| parent.join(file_name))
        })
        .unwrap_or_else(|_| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(path)
            }
        });
    let normalized = absolute.to_string_lossy().replace('/', "\\");
    let normalized = if cfg!(windows) {
        normalized.to_lowercase()
    } else {
        normalized
    };
    normalized
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
}

#[cfg(windows)]
struct WindowsMigrationLockGuard {
    handle: windows::Win32::Foundation::HANDLE,
    thread_affinity: PhantomData<Rc<()>>,
}

#[cfg(windows)]
impl MigrationLockGuard for WindowsMigrationLockGuard {}

#[cfg(windows)]
impl Drop for WindowsMigrationLockGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
impl MigrationLockProvider for StdMigrationLockProvider {
    fn acquire(
        &self,
        path: &Path,
        timeout: StdDuration,
    ) -> Result<Box<dyn MigrationLockGuard>, PersistenceError> {
        let name = format!(
            "{MIGRATION_MUTEX_PREFIX}{:016x}",
            migration_lock_identity(path)
        );
        let wide_name = name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide_name.as_ptr())) }
            .map_err(|_| PersistenceError::MigrationLockUnavailable)?;
        let timeout_millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
        match unsafe { WaitForSingleObject(handle, timeout_millis) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Box::new(WindowsMigrationLockGuard {
                handle,
                thread_affinity: PhantomData,
            })),
            WAIT_TIMEOUT => {
                let _ = unsafe { CloseHandle(handle) };
                Err(PersistenceError::MigrationLockTimeout)
            }
            WAIT_FAILED => {
                let _ = unsafe { CloseHandle(handle) };
                Err(PersistenceError::MigrationLockUnavailable)
            }
            _ => {
                let _ = unsafe { CloseHandle(handle) };
                Err(PersistenceError::MigrationLockUnavailable)
            }
        }
    }
}

#[cfg(not(windows))]
static PROCESS_MIGRATION_LOCKS: LazyLock<(Mutex<HashSet<u64>>, Condvar)> =
    LazyLock::new(|| (Mutex::new(HashSet::new()), Condvar::new()));

#[cfg(not(windows))]
struct ProcessMigrationLockGuard(u64);

#[cfg(not(windows))]
impl MigrationLockGuard for ProcessMigrationLockGuard {}

#[cfg(not(windows))]
impl Drop for ProcessMigrationLockGuard {
    fn drop(&mut self) {
        let (locks, available) = &*PROCESS_MIGRATION_LOCKS;
        lock_unpoisoned(locks).remove(&self.0);
        available.notify_all();
    }
}

#[cfg(not(windows))]
impl MigrationLockProvider for StdMigrationLockProvider {
    fn acquire(
        &self,
        path: &Path,
        timeout: StdDuration,
    ) -> Result<Box<dyn MigrationLockGuard>, PersistenceError> {
        let identity = migration_lock_identity(path);
        let deadline = Instant::now() + timeout;
        let (locks, available) = &*PROCESS_MIGRATION_LOCKS;
        let mut held = lock_unpoisoned(locks);
        while held.contains(&identity) {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(PersistenceError::MigrationLockTimeout);
            };
            let (next, wait) = available
                .wait_timeout(held, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            held = next;
            if wait.timed_out() && held.contains(&identity) {
                return Err(PersistenceError::MigrationLockTimeout);
            }
        }
        held.insert(identity);
        Ok(Box::new(ProcessMigrationLockGuard(identity)))
    }
}

trait MigrationHooks {
    fn after_selection(&self) {}
}

struct NoopMigrationHooks;

impl MigrationHooks for NoopMigrationHooks {}

struct StdMigrationFileOps;

impl MigrationFileOps for StdMigrationFileOps {
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        fs::rename(from, to)
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        fs::remove_file(path)
    }
}

#[cfg(test)]
fn migrate_legacy_database(
    path: &Path,
    quota: DiskQuota,
    now: DateTime<Utc>,
    file_ops: &dyn MigrationFileOps,
) -> Result<(), PersistenceError> {
    migrate_legacy_database_with_hooks(path, quota, now, file_ops, &NoopMigrationHooks)
}

fn migrate_legacy_database_with_hooks(
    path: &Path,
    quota: DiskQuota,
    now: DateTime<Utc>,
    file_ops: &dyn MigrationFileOps,
    hooks: &dyn MigrationHooks,
) -> Result<(), PersistenceError> {
    if !path.exists() || database_version(path)? != 1 {
        return Ok(());
    }

    checkpoint_database(path)?;
    let temp_path = migration_path(path, MIGRATION_TEMP_SUFFIX);
    remove_if_exists(&temp_path, file_ops)?;
    remove_sqlite_sidecars(&temp_path, file_ops)?;

    build_migrated_database(path, &temp_path, quota, now, hooks)?;
    install_migrated_database(path, &temp_path, file_ops)
}

fn recover_interrupted_migration(
    path: &Path,
    file_ops: &dyn MigrationFileOps,
) -> Result<(), PersistenceError> {
    let temp_path = migration_path(path, MIGRATION_TEMP_SUFFIX);
    let backup_path = migration_path(path, MIGRATION_BACKUP_SUFFIX);
    if backup_path.exists() {
        let backup_valid = database_is_valid(&backup_path);
        if path.exists() && database_is_valid(path) {
            remove_if_exists(&backup_path, file_ops)?;
        } else if backup_valid {
            remove_if_exists(path, file_ops)?;
            remove_sqlite_sidecars(path, file_ops)?;
            file_ops
                .rename(&backup_path, path)
                .map_err(PersistenceError::FileOperation)?;
        } else {
            return Err(PersistenceError::InvalidData);
        }
    }
    if temp_path.exists() {
        if path.exists() {
            remove_if_exists(&temp_path, file_ops)?;
        } else if database_is_valid(&temp_path) {
            file_ops
                .rename(&temp_path, path)
                .map_err(PersistenceError::FileOperation)?;
        } else {
            return Err(PersistenceError::InvalidData);
        }
    }
    remove_sqlite_sidecars(&temp_path, file_ops)?;
    remove_sqlite_sidecars(&backup_path, file_ops)?;
    Ok(())
}

fn select_legacy_records(
    connection: &Connection,
    quota: DiskQuota,
    now: DateTime<Utc>,
) -> Result<(Vec<String>, RetentionPeriod), PersistenceError> {
    let retention = setting(connection, "retention")?
        .and_then(|value| parse_retention(&value))
        .unwrap_or_default();
    let cutoff = retention
        .days()
        .map(|days| (now - Duration::days(days)).to_rfc3339());
    let mut statement = connection.prepare(
        "SELECT r.id,
                COALESCE(length(CAST(r.note AS BLOB)), 0) + COALESCE(SUM(
                    COALESCE(length(CAST(p.text_value AS BLOB)), 0) +
                    COALESCE(length(p.blob_value), 0) + ?1
                ), 0) AS payload_bytes
         FROM clipboard_records r
         LEFT JOIN clipboard_representations p ON p.record_id = r.id
         WHERE (?2 IS NULL OR r.captured_at >= ?2)
         GROUP BY r.id
         ORDER BY r.captured_at DESC, r.id DESC",
    )?;
    let rows = statement.query_map(
        params![REPRESENTATION_OVERHEAD_BYTES as i64, cutoff],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let (max_records, max_payload_bytes) = effective_quota_limits(connection, quota)?;
    let mut retained = Vec::new();
    let mut retained_bytes = 0_u64;
    for row in rows {
        let (id, payload_bytes) = row?;
        let Ok(payload_bytes) = u64::try_from(payload_bytes) else {
            continue;
        };
        if max_records.is_some_and(|limit| retained.len() >= limit) {
            break;
        }
        let Some(next_bytes) = retained_bytes.checked_add(payload_bytes) else {
            break;
        };
        if max_payload_bytes.is_some_and(|limit| next_bytes > limit) {
            continue;
        }
        retained.push(id);
        retained_bytes = next_bytes;
    }
    Ok((retained, retention))
}

fn build_migrated_database(
    source_path: &Path,
    temp_path: &Path,
    quota: DiskQuota,
    now: DateTime<Utc>,
    hooks: &dyn MigrationHooks,
) -> Result<(), PersistenceError> {
    {
        let mut temp_connection = Connection::open(temp_path)?;
        temp_connection.busy_timeout(StdDuration::from_secs(2))?;
        temp_connection.pragma_update(None, "foreign_keys", "ON")?;
        create_schema(&mut temp_connection)?;
        temp_connection
            .close()
            .map_err(|(_, error)| PersistenceError::Database(error))?;
    }

    let mut source = Connection::open(source_path)?;
    source.busy_timeout(StdDuration::from_secs(2))?;
    let temp_path_value = temp_path.to_str().ok_or(PersistenceError::InvalidData)?;
    source.execute("ATTACH DATABASE ?1 AS migrated", [temp_path_value])?;
    let copy_result = (|| {
        let transaction = source.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (retained_ids, retention) = select_legacy_records(&transaction, quota, now)?;
        hooks.after_selection();
        transaction.execute_batch(
            "CREATE TEMP TABLE retained_migration_ids (
                 id TEXT PRIMARY KEY NOT NULL
             );",
        )?;
        {
            let mut insert =
                transaction.prepare("INSERT INTO retained_migration_ids(id) VALUES (?1)")?;
            for id in &retained_ids {
                insert.execute([id])?;
            }
        }
        transaction.execute_batch(
            "INSERT INTO migrated.clipboard_records (
                id, content_identity, captured_at, source_application, source_path, note,
                group_id, pinned, favorite, sensitive, content_kind
             )
             SELECT r.id, r.content_identity, r.captured_at, r.source_application, r.source_path,
                    r.note, r.group_id, r.pinned, r.favorite, r.sensitive,
                    CASE
                      WHEN EXISTS (SELECT 1 FROM main.clipboard_representations p
                                   WHERE p.record_id = r.id AND p.kind = 'file_list') THEN 'files'
                      WHEN EXISTS (SELECT 1 FROM main.clipboard_representations p
                                   WHERE p.record_id = r.id AND p.kind IN ('png', 'dib_v5')) THEN 'image'
                      WHEN EXISTS (SELECT 1 FROM main.clipboard_representations p
                                   WHERE p.record_id = r.id AND p.kind IN ('rtf', 'html')) THEN 'rich_text'
                      ELSE 'text'
                    END
             FROM main.clipboard_records r
             JOIN retained_migration_ids keep ON keep.id = r.id;
             INSERT INTO migrated.clipboard_representations
             SELECT p.* FROM main.clipboard_representations p
             JOIN retained_migration_ids keep ON keep.id = p.record_id;
             INSERT INTO migrated.app_settings SELECT * FROM main.app_settings;",
        )?;
        transaction.execute(
            "INSERT INTO migrated.app_settings(key, value) VALUES ('retention', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [retention_value(retention)],
        )?;
        transaction.commit()?;
        Ok::<(), PersistenceError>(())
    })();
    source.execute_batch("DETACH DATABASE migrated;")?;
    copy_result?;
    let busy: i64 = source.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(PersistenceError::WorkerUnavailable);
    }
    source
        .close()
        .map_err(|(_, error)| PersistenceError::Database(error))?;

    let connection = Connection::open(temp_path)?;
    crate::services::search::rebuild_search_index(&connection)?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" || database_version_from_connection(&connection)? != SCHEMA_VERSION {
        return Err(PersistenceError::InvalidData);
    }
    connection
        .close()
        .map_err(|(_, error)| PersistenceError::Database(error))?;
    Ok(())
}

fn install_migrated_database(
    path: &Path,
    temp_path: &Path,
    file_ops: &dyn MigrationFileOps,
) -> Result<(), PersistenceError> {
    let backup_path = migration_path(path, MIGRATION_BACKUP_SUFFIX);
    remove_if_exists(&backup_path, file_ops)?;
    remove_sqlite_sidecars(&backup_path, file_ops)?;
    file_ops
        .rename(path, &backup_path)
        .map_err(PersistenceError::FileOperation)?;
    if let Err(error) = remove_sqlite_sidecars(path, file_ops) {
        let _ = file_ops.rename(&backup_path, path);
        return Err(error);
    }
    if let Err(error) = file_ops.rename(temp_path, path) {
        let rollback = file_ops.rename(&backup_path, path);
        return Err(PersistenceError::FileOperation(
            rollback.err().unwrap_or(error),
        ));
    }
    remove_if_exists(&backup_path, file_ops)?;
    remove_sqlite_sidecars(path, file_ops)?;
    Ok(())
}

fn checkpoint_database(path: &Path) -> Result<(), PersistenceError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(StdDuration::from_secs(2))?;
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(PersistenceError::InvalidData);
    }
    let busy: i64 =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    if busy != 0 {
        return Err(PersistenceError::WorkerUnavailable);
    }
    connection
        .close()
        .map_err(|(_, error)| PersistenceError::Database(error))?;
    Ok(())
}

fn database_version(path: &Path) -> Result<i64, PersistenceError> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    database_version_from_connection(&connection)
}

fn database_version_from_connection(connection: &Connection) -> Result<i64, PersistenceError> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Into::into)
}

fn database_is_valid(path: &Path) -> bool {
    let Ok(connection) =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return false;
    };
    let integrity = connection.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0));
    let version = database_version_from_connection(&connection);
    integrity.is_ok_and(|value| value == "ok")
        && version.is_ok_and(|value| (1..=SCHEMA_VERSION).contains(&value))
}

fn migration_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_sqlite_sidecars(
    path: &Path,
    file_ops: &dyn MigrationFileOps,
) -> Result<(), PersistenceError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        remove_if_exists(&sqlite_sidecar_path(path, suffix), file_ops)?;
    }
    Ok(())
}

fn remove_if_exists(path: &Path, file_ops: &dyn MigrationFileOps) -> Result<(), PersistenceError> {
    if path.exists() {
        file_ops
            .remove_file(path)
            .map_err(PersistenceError::FileOperation)?;
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), PersistenceError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(PersistenceError::UnsupportedSchema(version));
    }
    if version == 0 {
        create_schema(connection)?;
    } else if version == 1 {
        return Err(PersistenceError::UnsupportedSchema(version));
    } else if version == 2 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE clipboard_groups (
                    id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    position INTEGER NOT NULL
             );
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 3 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE clipboard_records
                ADD COLUMN content_kind TEXT NOT NULL DEFAULT 'text';
             UPDATE clipboard_records
             SET content_kind = CASE
                WHEN EXISTS (
                    SELECT 1 FROM clipboard_representations p
                    WHERE p.record_id = clipboard_records.id AND p.kind = 'file_list'
                ) THEN 'files'
                WHEN EXISTS (
                    SELECT 1 FROM clipboard_representations p
                    WHERE p.record_id = clipboard_records.id AND p.kind IN ('png', 'dib_v5')
                ) THEN 'image'
                WHEN EXISTS (
                    SELECT 1 FROM clipboard_representations p
                    WHERE p.record_id = clipboard_records.id AND p.kind IN ('rtf', 'html')
                ) THEN 'rich_text'
                ELSE 'text'
             END;
             CREATE INDEX clipboard_records_content_kind_page
                ON clipboard_records(content_kind, captured_at DESC, id DESC);
             CREATE INDEX clipboard_records_group_page
                ON clipboard_records(group_id, captured_at DESC, id DESC);
             CREATE INDEX clipboard_records_favorite_page
                ON clipboard_records(favorite, captured_at DESC, id DESC);
             CREATE INDEX clipboard_records_pinned_page
                ON clipboard_records(pinned, captured_at DESC, id DESC);
             PRAGMA user_version = 4;
             COMMIT;",
        )?;
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 4 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        crate::services::search::create_search_schema(&transaction)?;
        transaction.pragma_update(None, "user_version", 5)?;
        transaction.commit()?;
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 5 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS clipboard_recognition (
                 record_id TEXT PRIMARY KEY NOT NULL
                    REFERENCES clipboard_records(id) ON DELETE CASCADE,
                 ocr_text TEXT,
                 qr_text TEXT,
                 status TEXT NOT NULL,
                 updated_at TEXT NOT NULL
             );",
        )?;
        crate::services::search::rebuild_search_index(&transaction)?;
        transaction.pragma_update(None, "user_version", 6)?;
        transaction.commit()?;
    }
    Ok(())
}

fn create_schema(connection: &mut Connection) -> Result<(), PersistenceError> {
    connection.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE clipboard_records (
                id TEXT PRIMARY KEY NOT NULL,
                content_identity TEXT NOT NULL,
                captured_at TEXT NOT NULL,
                source_application TEXT,
                source_path TEXT,
                note TEXT,
                group_id TEXT,
                pinned INTEGER NOT NULL DEFAULT 0,
                favorite INTEGER NOT NULL DEFAULT 0,
                sensitive INTEGER NOT NULL DEFAULT 0
                ,content_kind TEXT NOT NULL DEFAULT 'text'
         );
         CREATE INDEX clipboard_records_captured_at
                ON clipboard_records(captured_at DESC);
         CREATE INDEX clipboard_records_content_kind_page
                ON clipboard_records(content_kind, captured_at DESC, id DESC);
         CREATE INDEX clipboard_records_group_page
                ON clipboard_records(group_id, captured_at DESC, id DESC);
         CREATE INDEX clipboard_records_favorite_page
                ON clipboard_records(favorite, captured_at DESC, id DESC);
         CREATE INDEX clipboard_records_pinned_page
                ON clipboard_records(pinned, captured_at DESC, id DESC);
         CREATE TABLE clipboard_representations (
                record_id TEXT NOT NULL REFERENCES clipboard_records(id) ON DELETE CASCADE,
                position INTEGER NOT NULL,
                kind TEXT NOT NULL,
                text_value TEXT,
                blob_value BLOB,
                PRIMARY KEY(record_id, position)
         );
         CREATE TABLE app_settings (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
         );
         CREATE TABLE clipboard_groups (
                id TEXT PRIMARY KEY NOT NULL,
                name TEXT NOT NULL,
                position INTEGER NOT NULL
         );
         CREATE TABLE clipboard_recognition (
                record_id TEXT PRIMARY KEY NOT NULL
                    REFERENCES clipboard_records(id) ON DELETE CASCADE,
                ocr_text TEXT,
                qr_text TEXT,
                status TEXT NOT NULL,
                updated_at TEXT NOT NULL
         );
         PRAGMA user_version = 6;
         COMMIT;",
    )?;
    crate::services::search::create_search_schema(connection)?;
    Ok(())
}

fn setting(connection: &Connection, key: &str) -> Result<Option<String>, PersistenceError> {
    connection
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

fn load_page_from_connection(
    connection: &Connection,
    query: HistoryQuery,
) -> Result<HistoryPage, PersistenceError> {
    let requested = if query.limit == 0 {
        DEFAULT_HISTORY_PAGE_LIMIT
    } else {
        query.limit.min(MAX_HISTORY_PAGE_LIMIT)
    };
    let mut sql = String::from(
        "SELECT r.id, r.captured_at, r.source_application, r.note, r.group_id, \
                r.pinned, r.favorite, r.sensitive, r.content_kind, \
                (SELECT substr(p.text_value, 1, 4096) FROM clipboard_representations p \
                 WHERE p.record_id = r.id AND p.kind = 'unicode_text' \
                 ORDER BY p.position LIMIT 1), \
                EXISTS(SELECT 1 FROM clipboard_representations p \
                       WHERE p.record_id = r.id AND p.kind IN ('png', 'dib_v5')), \
                (SELECT x.ocr_text FROM clipboard_recognition x WHERE x.record_id = r.id), \
                (SELECT x.qr_text FROM clipboard_recognition x WHERE x.record_id = r.id), \
                (SELECT json_extract(p.text_value, '$[0]') FROM clipboard_representations p \
                 WHERE p.record_id = r.id AND p.kind = 'file_list' ORDER BY p.position LIMIT 1), \
                (SELECT json_extract(p.text_value, '$[1]') FROM clipboard_representations p \
                 WHERE p.record_id = r.id AND p.kind = 'file_list' ORDER BY p.position LIMIT 1), \
                (SELECT json_extract(p.text_value, '$[2]') FROM clipboard_representations p \
                 WHERE p.record_id = r.id AND p.kind = 'file_list' ORDER BY p.position LIMIT 1), \
                COALESCE((SELECT json_array_length(p.text_value) FROM clipboard_representations p \
                 WHERE p.record_id = r.id AND p.kind = 'file_list' ORDER BY p.position LIMIT 1), 0) \
         FROM clipboard_records r WHERE 1 = 1",
    );
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(cursor) = query.cursor {
        sql.push_str(" AND (r.captured_at < ? OR (r.captured_at = ? AND r.id < ?))");
        values.push(Box::new(cursor.captured_at.to_rfc3339()));
        values.push(Box::new(cursor.captured_at.to_rfc3339()));
        values.push(Box::new(cursor.id.as_uuid().to_string()));
    }
    if let Some(kind) = query.content_kind {
        sql.push_str(" AND r.content_kind = ?");
        values.push(Box::new(content_kind_value(kind).to_owned()));
    }
    if let Some(group_id) = query.group_id {
        sql.push_str(" AND r.group_id = ?");
        values.push(Box::new(group_id.as_uuid().to_string()));
    }
    if query.ungrouped_only {
        sql.push_str(" AND r.group_id IS NULL");
    }
    if query.favorites_only {
        sql.push_str(" AND r.favorite = 1");
    }
    sql.push_str(" ORDER BY r.captured_at DESC, r.id DESC");
    let params = values.iter().map(|value| value.as_ref());
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(params), |row| {
        Ok(DbSummary {
            id: row.get(0)?,
            captured_at: row.get(1)?,
            source_application: row.get(2)?,
            note: row.get(3)?,
            group_id: row.get(4)?,
            pinned: row.get(5)?,
            favorite: row.get(6)?,
            sensitive: row.get(7)?,
            content_kind: row.get(8)?,
            text: row.get(9)?,
            has_image: row.get(10)?,
            ocr_text: row.get(11)?,
            qr_text: row.get(12)?,
            file_paths: [
                row.get::<_, Option<String>>(13)?,
                row.get::<_, Option<String>>(14)?,
                row.get::<_, Option<String>>(15)?,
            ]
            .into_iter()
            .flatten()
            .collect(),
            file_count: row.get(16)?,
        })
    })?;
    let mut records = Vec::with_capacity(requested);
    for row in rows {
        let Ok(row) = row else {
            continue;
        };
        let Ok(summary) = row.into_summary() else {
            continue;
        };
        records.push(summary);
        if records.len() > requested {
            break;
        }
    }
    let has_more = records.len() > requested;
    records.truncate(requested);
    let next_cursor = has_more.then(|| {
        let last = records.last().expect("non-empty page with lookahead");
        HistoryCursor {
            captured_at: last.captured_at,
            id: last.id,
        }
    });
    Ok(HistoryPage {
        records,
        next_cursor,
    })
}

fn save_setting(transaction: &Transaction<'_>, key: &str, value: &str) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO app_settings(key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn record_count(transaction: &Transaction<'_>) -> Result<usize, PersistenceError> {
    let count: i64 =
        transaction.query_row("SELECT COUNT(*) FROM clipboard_records", [], |row| {
            row.get(0)
        })?;
    usize::try_from(count).map_err(|_| PersistenceError::InvalidData)
}

fn record_exists(transaction: &Transaction<'_>, id: RecordId) -> Result<bool, PersistenceError> {
    transaction
        .query_row(
            "SELECT 1 FROM clipboard_records WHERE id = ?1",
            [id.as_uuid().to_string()],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(Into::into)
}

fn record_ids_in_group(
    transaction: &Transaction<'_>,
    group_id: GroupId,
) -> Result<Vec<String>, PersistenceError> {
    let mut statement =
        transaction.prepare("SELECT id FROM clipboard_records WHERE group_id = ?1 ORDER BY id")?;
    let ids = statement
        .query_map([group_id.as_uuid().to_string()], |row| row.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

fn refresh_group_search_records(
    transaction: &Transaction<'_>,
    group_id: GroupId,
) -> Result<(), PersistenceError> {
    for record_id in record_ids_in_group(transaction, group_id)? {
        crate::services::search::refresh_search_record(transaction, &record_id)?;
    }
    Ok(())
}

// The quota covers replayable payload bytes, per-representation allocation overhead, and notes.
fn enforce_disk_quota(
    transaction: &Transaction<'_>,
    quota: DiskQuota,
) -> Result<usize, PersistenceError> {
    let (max_records, max_payload_bytes) = effective_quota_limits(transaction, quota)?;
    let evict_favorites = setting(transaction, "evict_favorites_when_full")?
        .and_then(|value| parse_bool(&value))
        .unwrap_or(false);
    let mut records = Vec::new();
    let mut statement = transaction.prepare(
        "SELECT r.id,
                COALESCE(length(CAST(r.note AS BLOB)), 0) + COALESCE(SUM(
                    COALESCE(length(CAST(p.text_value AS BLOB)), 0) +
                    COALESCE(length(p.blob_value), 0) + ?1
                ), 0) AS payload_bytes,
                r.favorite
         FROM clipboard_records r
         LEFT JOIN clipboard_representations p ON p.record_id = r.id
         WHERE r.pinned = 0
         GROUP BY r.id
         ORDER BY r.favorite ASC, r.captured_at ASC, r.id ASC",
    )?;
    let rows = statement.query_map([REPRESENTATION_OVERHEAD_BYTES as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, bool>(2)?,
        ))
    })?;
    for row in rows {
        records.push(row?);
    }
    drop(statement);

    let total_count: i64 =
        transaction.query_row("SELECT COUNT(*) FROM clipboard_records", [], |row| {
            row.get(0)
        })?;
    let total_bytes: i64 = transaction.query_row(
        "SELECT COALESCE(SUM(payload_bytes), 0) FROM (
             SELECT COALESCE(length(CAST(r.note AS BLOB)), 0) + COALESCE(SUM(
                 COALESCE(length(CAST(p.text_value AS BLOB)), 0) +
                 COALESCE(length(p.blob_value), 0) + ?1
             ), 0) AS payload_bytes
             FROM clipboard_records r
             LEFT JOIN clipboard_representations p ON p.record_id = r.id
             GROUP BY r.id
         )",
        [REPRESENTATION_OVERHEAD_BYTES as i64],
        |row| row.get(0),
    )?;
    let mut retained_count =
        usize::try_from(total_count).map_err(|_| PersistenceError::InvalidData)?;
    let mut retained_bytes =
        u64::try_from(total_bytes).map_err(|_| PersistenceError::InvalidData)?;
    let mut removed = 0_usize;
    for (id, payload_bytes, favorite) in records {
        let within_count = max_records.is_none_or(|limit| retained_count <= limit);
        let within_bytes = max_payload_bytes.is_none_or(|limit| retained_bytes <= limit);
        if within_count && within_bytes {
            break;
        }
        if favorite && !evict_favorites {
            continue;
        }
        let Ok(payload_bytes) = u64::try_from(payload_bytes) else {
            transaction.execute("DELETE FROM clipboard_records WHERE id = ?1", [id])?;
            removed = removed.saturating_add(1);
            retained_count = retained_count.saturating_sub(1);
            continue;
        };
        transaction.execute("DELETE FROM clipboard_records WHERE id = ?1", [id])?;
        retained_count = retained_count.saturating_sub(1);
        retained_bytes = retained_bytes.saturating_sub(payload_bytes);
        removed = removed.saturating_add(1);
    }
    Ok(removed)
}

fn effective_quota_limits(
    connection: &Connection,
    quota: DiskQuota,
) -> Result<(Option<usize>, Option<u64>), PersistenceError> {
    let settings_bytes = setting(connection, "storage_limit")?
        .and_then(|value| parse_storage_limit(&value))
        .unwrap_or_default()
        .bytes();
    let injected_bytes =
        (quota.max_payload_bytes != usize::MAX).then_some(quota.max_payload_bytes as u64);
    let max_payload_bytes = match (settings_bytes, injected_bytes) {
        (Some(settings), Some(injected)) => Some(settings.min(injected)),
        (Some(settings), None) => Some(settings),
        (None, Some(injected)) => Some(injected),
        (None, None) => None,
    };
    let max_records = (quota.max_records != usize::MAX).then_some(quota.max_records);
    Ok((max_records, max_payload_bytes))
}

fn incremental_vacuum(connection: &Connection, quota: DiskQuota) -> Result<(), PersistenceError> {
    connection.execute_batch(&format!(
        "PRAGMA incremental_vacuum({});",
        quota.incremental_vacuum_pages
    ))?;
    Ok(())
}

fn representation_metadata_bytes(
    connection: &Connection,
    record: &DbRecord,
) -> Result<Option<usize>, PersistenceError> {
    let Some(note_bytes) = note_metadata_bytes(&record.note_storage_type, record.note_length)
    else {
        return Ok(None);
    };
    let Some(representation_bytes) = representation_metadata_bytes_for_id(connection, &record.id)?
    else {
        return Ok(None);
    };
    note_bytes
        .checked_add(representation_bytes)
        .map(Some)
        .ok_or(PersistenceError::InvalidData)
}

fn note_metadata_bytes(storage_type: &str, length: Option<i64>) -> Option<usize> {
    match (storage_type, length) {
        ("null", None) => Some(0),
        ("text", Some(length)) => usize::try_from(length).ok(),
        _ => None,
    }
}

fn load_db_record(connection: &Connection, id: RecordId) -> Result<DbRecord, PersistenceError> {
    connection
        .query_row(
            "SELECT id, content_identity, captured_at, source_application, source_path, \
                    typeof(note), length(CAST(note AS BLOB)), group_id, pinned, favorite, sensitive \
             FROM clipboard_records WHERE id = ?1",
            [id.as_uuid().to_string()],
            |row| {
                Ok(DbRecord {
                    id: row.get(0)?,
                    content_identity: row.get(1)?,
                    captured_at: row.get(2)?,
                    source_application: row.get(3)?,
                    source_path: row.get(4)?,
                    note_storage_type: row.get(5)?,
                    note_length: row.get(6)?,
                    group_id: row.get(7)?,
                    pinned: row.get(8)?,
                    favorite: row.get(9)?,
                    sensitive: row.get(10)?,
                })
            },
        )
        .optional()?
        .ok_or(PersistenceError::InvalidData)
}

fn validate_record_metadata(
    connection: &Connection,
    row: &DbRecord,
) -> Result<(), PersistenceError> {
    let note_bytes = note_metadata_bytes(&row.note_storage_type, row.note_length)
        .ok_or(PersistenceError::InvalidData)?;
    let representation_bytes = representation_metadata_bytes_for_id(connection, &row.id)?
        .ok_or(PersistenceError::InvalidData)?;
    let total = representation_bytes
        .checked_add(note_bytes)
        .ok_or(PersistenceError::InvalidData)?;
    if total > MAX_CAPTURE_RECORD_BYTES {
        return Err(PersistenceError::InvalidData);
    }
    Ok(())
}

fn representation_metadata_bytes_for_id(
    connection: &Connection,
    record_id: &str,
) -> Result<Option<usize>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT position, kind, typeof(text_value), length(CAST(text_value AS BLOB)),
                typeof(blob_value), length(blob_value)
         FROM clipboard_representations
         WHERE record_id = ?1 ORDER BY position",
    )?;
    let rows = statement.query_map([record_id], |row| {
        Ok(RepresentationMetadata {
            position: row.get(0)?,
            kind: row.get(1)?,
            text_storage_type: row.get(2)?,
            text_length: row.get(3)?,
            blob_storage_type: row.get(4)?,
            blob_length: row.get(5)?,
        })
    })?;
    let mut total = 0_usize;
    let mut count = 0_usize;
    for row in rows {
        let metadata = row?;
        let payload = match metadata.kind.as_str() {
            "unicode_text"
                if metadata.text_storage_type == "text" && metadata.blob_storage_type == "null" =>
            {
                metadata.text_length
            }
            "rtf" | "html" | "png" | "dib_v5"
                if metadata.text_storage_type == "null" && metadata.blob_storage_type == "blob" =>
            {
                metadata.blob_length
            }
            "file_list"
                if metadata.text_storage_type == "text" && metadata.blob_storage_type == "null" =>
            {
                let Some(encoded_bytes) = metadata
                    .text_length
                    .and_then(|value| usize::try_from(value).ok())
                else {
                    return Ok(None);
                };
                if encoded_bytes > MAX_FILE_LIST_ENCODED_BYTES {
                    return Ok(None);
                }
                match file_list_metadata(connection, record_id, metadata.position) {
                    Ok(metadata) => Some(metadata.logical_bytes as i64),
                    Err(PersistenceError::InvalidData) => return Ok(None),
                    Err(error) => return Err(error),
                }
            }
            _ => return Ok(None),
        };
        let Some(payload) = payload.and_then(|value| usize::try_from(value).ok()) else {
            return Ok(None);
        };
        let Some(next) = total
            .checked_add(payload)
            .and_then(|bytes| bytes.checked_add(REPRESENTATION_OVERHEAD_BYTES))
        else {
            return Ok(None);
        };
        total = next;
        count = count.saturating_add(1);
    }
    Ok((count > 0).then_some(total))
}

fn load_note(connection: &Connection, record_id: &str) -> Result<Option<String>, PersistenceError> {
    connection
        .query_row(
            "SELECT note FROM clipboard_records WHERE id = ?1",
            [record_id],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn load_representations(
    connection: &Connection,
    record_id: &str,
) -> Result<Vec<ClipboardRepresentation>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT kind, text_value, blob_value FROM clipboard_representations \
         WHERE record_id = ?1 ORDER BY position",
    )?;
    let rows = statement.query_map([record_id], |row| {
        let kind: String = row.get(0)?;
        let text: Option<String> = row.get(1)?;
        let blob: Option<Vec<u8>> = row.get(2)?;
        Ok((kind, text, blob))
    })?;
    let mut representations = Vec::new();
    for row in rows {
        let (kind, text, blob) = row?;
        representations.push(match kind.as_str() {
            "unicode_text" => ClipboardRepresentation::UnicodeText {
                text: text.ok_or(PersistenceError::InvalidData)?,
            },
            "png" => ClipboardRepresentation::Png {
                bytes: blob.ok_or(PersistenceError::InvalidData)?,
            },
            "dib_v5" => ClipboardRepresentation::DibV5 {
                bytes: blob.ok_or(PersistenceError::InvalidData)?,
            },
            "rtf" => ClipboardRepresentation::Rtf {
                bytes: blob.ok_or(PersistenceError::InvalidData)?,
            },
            "html" => ClipboardRepresentation::Html {
                bytes: blob.ok_or(PersistenceError::InvalidData)?,
            },
            "file_list" => {
                let paths: Vec<String> =
                    serde_json::from_str(text.as_deref().ok_or(PersistenceError::InvalidData)?)
                        .map_err(|_| PersistenceError::InvalidData)?;
                validate_file_list(&paths)?;
                ClipboardRepresentation::FileList { paths }
            }
            _ => return Err(PersistenceError::InvalidData),
        });
    }
    if representations.is_empty() {
        return Err(PersistenceError::InvalidData);
    }
    Ok(representations)
}

fn load_representation_details(
    connection: &Connection,
    record_id: &str,
) -> Result<Vec<ClipboardRepresentationDetails>, PersistenceError> {
    let mut statement = connection.prepare(
        "SELECT position, kind,
                length(CASE WHEN text_value IS NOT NULL THEN CAST(text_value AS BLOB) ELSE blob_value END),
                CASE WHEN kind = 'unicode_text' THEN substr(CAST(text_value AS BLOB), 1, ?2) ELSE NULL END,
                CASE WHEN kind IN ('rtf', 'html') THEN substr(blob_value, 1, ?2) ELSE NULL END
         FROM clipboard_representations
         WHERE record_id = ?1 ORDER BY position",
    )?;
    let rows = statement.query_map(params![record_id, MAX_DETAIL_TEXT_BYTES as i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<Vec<u8>>>(3)?,
            row.get::<_, Option<Vec<u8>>>(4)?,
        ))
    })?;
    let mut details = Vec::new();
    for row in rows {
        let (position, kind, stored_byte_length, unicode_text, rich_bytes) = row?;
        let stored_byte_length =
            usize::try_from(stored_byte_length).map_err(|_| PersistenceError::InvalidData)?;
        if kind != "file_list" && stored_byte_length > MAX_CAPTURE_RECORD_BYTES {
            return Err(PersistenceError::InvalidData);
        }
        let (kind, byte_length, item_count, text, paths, truncated) = match kind.as_str() {
            "unicode_text" => (
                ClipboardRepresentationKind::UnicodeText,
                stored_byte_length,
                None,
                Some(bounded_utf8_lossy(
                    &unicode_text.ok_or(PersistenceError::InvalidData)?,
                    MAX_DETAIL_TEXT_BYTES,
                )),
                None,
                stored_byte_length > MAX_DETAIL_TEXT_BYTES,
            ),
            "rtf" => (
                ClipboardRepresentationKind::Rtf,
                stored_byte_length,
                None,
                Some(bounded_utf8_lossy(
                    &rich_bytes.ok_or(PersistenceError::InvalidData)?,
                    MAX_DETAIL_TEXT_BYTES,
                )),
                None,
                stored_byte_length > MAX_DETAIL_TEXT_BYTES,
            ),
            "html" => (
                ClipboardRepresentationKind::Html,
                stored_byte_length,
                None,
                Some(bounded_utf8_lossy(
                    &rich_bytes.ok_or(PersistenceError::InvalidData)?,
                    MAX_DETAIL_TEXT_BYTES,
                )),
                None,
                stored_byte_length > MAX_DETAIL_TEXT_BYTES,
            ),
            "png" => (
                ClipboardRepresentationKind::Png,
                stored_byte_length,
                None,
                None,
                None,
                false,
            ),
            "dib_v5" => (
                ClipboardRepresentationKind::DibV5,
                stored_byte_length,
                None,
                None,
                None,
                false,
            ),
            "file_list" => {
                let projection = load_file_list_details(connection, record_id, position)?;
                (
                    ClipboardRepresentationKind::FileList,
                    projection.byte_length,
                    Some(projection.item_count),
                    None,
                    Some(projection.paths),
                    projection.truncated,
                )
            }
            _ => return Err(PersistenceError::InvalidData),
        };
        details.push(ClipboardRepresentationDetails {
            kind,
            byte_length,
            item_count,
            text,
            paths,
            truncated,
        });
    }
    if details.is_empty() {
        return Err(PersistenceError::InvalidData);
    }
    Ok(details)
}

struct FileListDetailsProjection {
    byte_length: usize,
    item_count: usize,
    paths: Vec<String>,
    truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileListMetadata {
    item_count: usize,
    logical_bytes: usize,
}

fn file_list_metadata(
    connection: &Connection,
    record_id: &str,
    position: i64,
) -> Result<FileListMetadata, PersistenceError> {
    let (encoded_bytes, json_valid, json_type): (i64, i64, Option<String>) = connection.query_row(
        "SELECT length(CAST(text_value AS BLOB)), json_valid(text_value),
                CASE WHEN json_valid(text_value) THEN json_type(text_value) ELSE NULL END
         FROM clipboard_representations
         WHERE record_id = ?1 AND position = ?2 AND kind = 'file_list'
           AND typeof(text_value) = 'text' AND typeof(blob_value) = 'null'",
        params![record_id, position],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let encoded_bytes =
        usize::try_from(encoded_bytes).map_err(|_| PersistenceError::InvalidData)?;
    if json_valid != 1
        || json_type.as_deref() != Some("array")
        || encoded_bytes > MAX_FILE_LIST_ENCODED_BYTES
    {
        return Err(PersistenceError::InvalidData);
    }
    let (item_count, logical_bytes, invalid_items): (i64, i64, i64) = connection.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(length(CAST(value AS BLOB))), 0),
                COALESCE(SUM(CASE
                    WHEN type != 'text' OR length(CAST(value AS BLOB)) = 0
                         OR instr(value, char(0)) > 0 THEN 1 ELSE 0 END), 0)
         FROM json_each((SELECT text_value FROM clipboard_representations
                         WHERE record_id = ?1 AND position = ?2 AND kind = 'file_list'))",
        params![record_id, position],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let item_count = usize::try_from(item_count).map_err(|_| PersistenceError::InvalidData)?;
    let logical_bytes =
        usize::try_from(logical_bytes).map_err(|_| PersistenceError::InvalidData)?;
    if item_count == 0
        || item_count > MAX_FILE_LIST_PATHS
        || logical_bytes > MAX_FILE_LIST_LOGICAL_BYTES
        || invalid_items != 0
    {
        return Err(PersistenceError::InvalidData);
    }
    Ok(FileListMetadata {
        item_count,
        logical_bytes,
    })
}

fn load_file_list_details(
    connection: &Connection,
    record_id: &str,
    position: i64,
) -> Result<FileListDetailsProjection, PersistenceError> {
    let metadata = file_list_metadata(connection, record_id, position)?;

    let mut statement = connection.prepare(
        "SELECT substr(CAST(value AS BLOB), 1, ?3), length(CAST(value AS BLOB))
         FROM json_each((SELECT text_value FROM clipboard_representations
                         WHERE record_id = ?1 AND position = ?2 AND kind = 'file_list'))
         LIMIT ?4",
    )?;
    let mut rows = statement.query(params![
        record_id,
        position,
        MAX_DETAIL_FILE_LIST_BYTES as i64,
        MAX_DETAIL_FILE_LIST_PATHS as i64,
    ])?;
    let mut paths = Vec::new();
    let mut projected_bytes = 0_usize;
    let mut path_was_truncated = false;
    while let Some(row) = rows.next()? {
        let bytes: Vec<u8> = row.get(0)?;
        let original_length =
            usize::try_from(row.get::<_, i64>(1)?).map_err(|_| PersistenceError::InvalidData)?;
        let remaining = MAX_DETAIL_FILE_LIST_BYTES.saturating_sub(projected_bytes);
        if remaining == 0 {
            break;
        }
        let bounded = &bytes[..bytes.len().min(remaining)];
        let value = bounded_utf8_lossy(bounded, remaining);
        projected_bytes = projected_bytes.saturating_add(value.len());
        path_was_truncated |= original_length > bounded.len();
        paths.push(value);
        if path_was_truncated {
            break;
        }
    }
    Ok(FileListDetailsProjection {
        byte_length: metadata.logical_bytes,
        item_count: metadata.item_count,
        truncated: path_was_truncated || paths.len() < metadata.item_count,
        paths,
    })
}

fn bounded_utf8_lossy(bytes: &[u8], max_bytes: usize) -> String {
    let value = String::from_utf8_lossy(&bytes[..bytes.len().min(max_bytes)]);
    if value.len() <= max_bytes {
        return value.into_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn content_kind_from_details(details: &[ClipboardRepresentationDetails]) -> ContentKind {
    if details
        .iter()
        .any(|value| value.kind == ClipboardRepresentationKind::FileList)
    {
        ContentKind::Files
    } else if details.iter().any(|value| {
        matches!(
            value.kind,
            ClipboardRepresentationKind::Png | ClipboardRepresentationKind::DibV5
        )
    }) {
        ContentKind::Image
    } else if details.iter().any(|value| {
        matches!(
            value.kind,
            ClipboardRepresentationKind::Rtf | ClipboardRepresentationKind::Html
        )
    }) {
        ContentKind::RichText
    } else {
        ContentKind::Text
    }
}

fn validate_file_list(paths: &[String]) -> Result<(), PersistenceError> {
    if paths.is_empty() || paths.len() > MAX_FILE_LIST_PATHS {
        return Err(PersistenceError::InvalidData);
    }
    let total = paths.iter().try_fold(0_usize, |total, path| {
        if path.is_empty() || path.contains('\0') {
            return None;
        }
        total.checked_add(path.len())
    });
    if total.is_none_or(|total| total > MAX_FILE_LIST_LOGICAL_BYTES) {
        return Err(PersistenceError::InvalidData);
    }
    Ok(())
}

fn validate_record_file_lists(record: &ClipboardRecord) -> Result<(), PersistenceError> {
    for representation in &record.representations {
        let ClipboardRepresentation::FileList { paths } = representation else {
            continue;
        };
        validate_file_list(paths)?;
        let encoded = serde_json::to_vec(paths).map_err(|_| PersistenceError::InvalidData)?;
        if encoded.len() > MAX_FILE_LIST_ENCODED_BYTES {
            return Err(PersistenceError::InvalidData);
        }
    }
    Ok(())
}

struct DbRecord {
    id: String,
    content_identity: String,
    captured_at: String,
    source_application: Option<String>,
    source_path: Option<String>,
    note_storage_type: String,
    note_length: Option<i64>,
    group_id: Option<String>,
    pinned: bool,
    favorite: bool,
    sensitive: bool,
}

struct DbSummary {
    id: String,
    captured_at: String,
    source_application: Option<String>,
    note: Option<String>,
    group_id: Option<String>,
    pinned: bool,
    favorite: bool,
    sensitive: bool,
    content_kind: String,
    text: Option<String>,
    has_image: bool,
    ocr_text: Option<String>,
    qr_text: Option<String>,
    file_paths: Vec<String>,
    file_count: usize,
}

impl DbSummary {
    fn into_summary(self) -> Result<HistoryRecordSummary, PersistenceError> {
        Ok(HistoryRecordSummary {
            id: RecordId::parse(&self.id).map_err(|_| PersistenceError::InvalidData)?,
            captured_at: DateTime::parse_from_rfc3339(&self.captured_at)
                .map_err(|_| PersistenceError::InvalidData)?
                .with_timezone(&Utc),
            source_application: self.source_application,
            text: self.text,
            has_image: self.has_image,
            ocr_text: self.ocr_text,
            qr_text: self.qr_text,
            file_paths: self.file_paths,
            file_count: self.file_count,
            content_kind: parse_content_kind(&self.content_kind)
                .ok_or(PersistenceError::InvalidData)?,
            note: self
                .note
                .map(RecordNote::new)
                .transpose()
                .map_err(|_| PersistenceError::InvalidData)?,
            group_id: self
                .group_id
                .map(|value| GroupId::parse(&value))
                .transpose()
                .map_err(|_| PersistenceError::InvalidData)?,
            pinned: self.pinned,
            favorite: self.favorite,
            sensitive: self.sensitive,
        })
    }
}

impl DbRecord {
    fn into_record(
        self,
        representations: Vec<ClipboardRepresentation>,
        note: Option<String>,
    ) -> Result<ClipboardRecord, PersistenceError> {
        Ok(ClipboardRecord {
            id: RecordId::parse(&self.id).map_err(|_| PersistenceError::InvalidData)?,
            content_identity: ContentIdentity::new(self.content_identity),
            captured_at: DateTime::parse_from_rfc3339(&self.captured_at)
                .map_err(|_| PersistenceError::InvalidData)?
                .with_timezone(&Utc),
            source: SourceIdentity {
                application_name: self.source_application,
                executable_path: self.source_path,
            },
            representations,
            note: note
                .map(RecordNote::new)
                .transpose()
                .map_err(|_| PersistenceError::InvalidData)?,
            group_id: self
                .group_id
                .map(|value| GroupId::parse(&value))
                .transpose()
                .map_err(|_| PersistenceError::InvalidData)?,
            pinned: self.pinned,
            favorite: self.favorite,
            sensitive: self.sensitive,
        })
    }
}

struct RepresentationMetadata {
    position: i64,
    kind: String,
    text_storage_type: String,
    text_length: Option<i64>,
    blob_storage_type: String,
    blob_length: Option<i64>,
}

fn language_value(language: Language) -> &'static str {
    match language {
        Language::ZhCn => "zh_cn",
        Language::En => "en",
    }
}

fn parse_language(value: &str) -> Option<Language> {
    match value {
        "zh_cn" => Some(Language::ZhCn),
        "en" => Some(Language::En),
        _ => None,
    }
}

fn accent_color_value(color: AccentColor) -> &'static str {
    match color {
        AccentColor::Blue => "blue",
        AccentColor::Teal => "teal",
        AccentColor::Rose => "rose",
        AccentColor::Violet => "violet",
        AccentColor::Amber => "amber",
    }
}

fn parse_accent_color(value: &str) -> Option<AccentColor> {
    match value {
        "blue" => Some(AccentColor::Blue),
        "teal" => Some(AccentColor::Teal),
        "rose" => Some(AccentColor::Rose),
        "violet" => Some(AccentColor::Violet),
        "amber" => Some(AccentColor::Amber),
        _ => None,
    }
}

fn capture_sound_value(sound: CaptureSound) -> &'static str {
    match sound {
        CaptureSound::Default => "default",
        CaptureSound::Custom => "custom",
    }
}

fn parse_capture_sound(value: &str) -> Option<CaptureSound> {
    match value {
        "default" => Some(CaptureSound::Default),
        "custom" => Some(CaptureSound::Custom),
        _ => None,
    }
}

fn retention_value(retention: RetentionPeriod) -> &'static str {
    match retention {
        RetentionPeriod::OneDay => "one_day",
        RetentionPeriod::SevenDays => "seven_days",
        RetentionPeriod::ThirtyDays => "thirty_days",
        RetentionPeriod::NinetyDays => "ninety_days",
        RetentionPeriod::Forever => "forever",
    }
}

fn parse_retention(value: &str) -> Option<RetentionPeriod> {
    match value {
        "one_day" => Some(RetentionPeriod::OneDay),
        "seven_days" => Some(RetentionPeriod::SevenDays),
        "thirty_days" => Some(RetentionPeriod::ThirtyDays),
        "ninety_days" => Some(RetentionPeriod::NinetyDays),
        "forever" => Some(RetentionPeriod::Forever),
        _ => None,
    }
}

fn storage_limit_value(limit: StorageLimit) -> &'static str {
    match limit {
        StorageLimit::OneGb => "one_gb",
        StorageLimit::FiveGb => "five_gb",
        StorageLimit::TenGb => "ten_gb",
        StorageLimit::Unlimited => "unlimited",
    }
}

fn parse_storage_limit(value: &str) -> Option<StorageLimit> {
    match value {
        "one_gb" => Some(StorageLimit::OneGb),
        "five_gb" => Some(StorageLimit::FiveGb),
        "ten_gb" => Some(StorageLimit::TenGb),
        "unlimited" => Some(StorageLimit::Unlimited),
        _ => None,
    }
}

fn content_kind_value(kind: ContentKind) -> &'static str {
    match kind {
        ContentKind::Text => "text",
        ContentKind::RichText => "rich_text",
        ContentKind::Image => "image",
        ContentKind::Files => "files",
    }
}

fn parse_content_kind(value: &str) -> Option<ContentKind> {
    match value {
        "text" => Some(ContentKind::Text),
        "rich_text" => Some(ContentKind::RichText),
        "image" => Some(ContentKind::Image),
        "files" => Some(ContentKind::Files),
        _ => None,
    }
}

fn bool_value(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Shortcut;
    use chrono::TimeZone;
    #[cfg(windows)]
    use static_assertions::assert_not_impl_any;
    use std::{
        collections::HashSet,
        io,
        sync::{Barrier, atomic::AtomicUsize},
    };
    use tempfile::tempdir;

    #[cfg(windows)]
    assert_not_impl_any!(WindowsMigrationLockGuard: Send, Sync);

    struct TestBackend {
        writes: AtomicUsize,
        fail_first: AtomicBool,
        delay: StdDuration,
    }

    impl TestBackend {
        fn new(fail_first: bool, delay: StdDuration) -> Self {
            Self {
                writes: AtomicUsize::new(0),
                fail_first: AtomicBool::new(fail_first),
                delay,
            }
        }
    }

    impl PersistenceBackend for TestBackend {
        fn persist_record(&self, _record: &ClipboardRecord) -> Result<(), PersistenceError> {
            self.writes.fetch_add(1, Ordering::AcqRel);
            if !self.delay.is_zero() {
                thread::sleep(self.delay);
            }
            if self.fail_first.swap(false, Ordering::AcqRel) {
                Err(PersistenceError::WorkerUnavailable)
            } else {
                Ok(())
            }
        }

        fn persist_note(
            &self,
            _id: RecordId,
            _note: Option<&RecordNote>,
        ) -> Result<(), PersistenceError> {
            Ok(())
        }

        fn delete_record(&self, _id: RecordId) -> Result<(), PersistenceError> {
            Ok(())
        }

        fn clear_records(&self) -> Result<usize, PersistenceError> {
            Ok(0)
        }

        fn persist_settings(&self, _settings: UserSettings) -> Result<(), PersistenceError> {
            Ok(())
        }

        fn persist_recognition(
            &self,
            _id: RecordId,
            _ocr_text: Option<&str>,
            _qr_text: Option<&str>,
            _status: &str,
        ) -> Result<(), PersistenceError> {
            Ok(())
        }

        fn update_storage_policy(
            &self,
            _storage_limit: StorageLimit,
            _evict_favorites_when_full: bool,
        ) -> Result<usize, PersistenceError> {
            Ok(0)
        }

        fn persist_excluded_applications(
            &self,
            _applications: &[String],
        ) -> Result<(), PersistenceError> {
            Ok(())
        }

        fn prune_records(
            &self,
            _retention: RetentionPeriod,
            _now: DateTime<Utc>,
        ) -> Result<usize, PersistenceError> {
            Ok(0)
        }

        fn backup_database(&self, _destination: &Path) -> Result<(), PersistenceError> {
            Ok(())
        }

        fn restore_database(
            &self,
            _source: &Path,
            _budget: RestoreBudget,
        ) -> Result<RestoredData, PersistenceError> {
            Ok(RestoredData {
                settings: UserSettings::default(),
                excluded_applications: Vec::new(),
                groups: Vec::new(),
                page: HistoryPage {
                    records: Vec::new(),
                    next_cursor: None,
                },
            })
        }
    }

    struct FailSecondRename {
        renames: AtomicUsize,
    }

    struct BlockingMigrationHooks {
        selected: Arc<Barrier>,
        resume: Arc<Barrier>,
    }

    impl MigrationHooks for BlockingMigrationHooks {
        fn after_selection(&self) {
            self.selected.wait();
            self.resume.wait();
        }
    }

    impl MigrationFileOps for FailSecondRename {
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            let call = self.renames.fetch_add(1, Ordering::AcqRel) + 1;
            if call == 2 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected migration install failure",
                ));
            }
            fs::rename(from, to)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            fs::remove_file(path)
        }
    }

    fn record(identity: &str, captured_at: DateTime<Utc>) -> ClipboardRecord {
        let mut record = ClipboardRecord::from_capture(crate::domain::CapturedClipboard {
            content_identity: ContentIdentity::new(identity),
            captured_at,
            source: SourceIdentity {
                application_name: Some("Editor".to_owned()),
                executable_path: Some(r"C:\Tools\editor.exe".to_owned()),
            },
            representations: vec![
                ClipboardRepresentation::UnicodeText {
                    text: "saved text".to_owned(),
                },
                ClipboardRepresentation::Png {
                    bytes: vec![137, 80, 78, 71],
                },
            ],
        });
        record.note = Some(RecordNote::new("account note").unwrap());
        record
    }

    fn text_record(identity: &str, captured_at: DateTime<Utc>, text: &str) -> ClipboardRecord {
        ClipboardRecord::from_capture(crate::domain::CapturedClipboard {
            content_identity: ContentIdentity::new(identity),
            captured_at,
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: text.to_owned(),
            }],
        })
    }

    fn escaped_file_paths(path_count: usize, minimum_logical_bytes: usize) -> Vec<String> {
        let bytes_per_path = minimum_logical_bytes.div_ceil(path_count);
        (0..path_count)
            .map(|index| {
                let prefix = format!(r#"C:\quoted\"folder\{index}\"#);
                let escaped_segment =
                    r#"\""#.repeat(bytes_per_path.saturating_sub(prefix.len()) / 2);
                format!("{prefix}{escaped_segment}")
            })
            .collect()
    }

    fn file_list_record(identity: &str, paths: Vec<String>) -> ClipboardRecord {
        ClipboardRecord::from_capture(crate::domain::CapturedClipboard {
            content_identity: ContentIdentity::new(identity),
            captured_at: Utc::now(),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::FileList { paths }],
        })
    }

    #[test]
    fn backup_and_restore_preserve_records_groups_notes_and_settings() {
        let directory = tempdir().unwrap();
        let live_path = directory.path().join("live.sqlite3");
        let backup_path = directory.path().join("history.clipbackup");
        let repository = SqliteRepository::open(live_path).unwrap();
        let group = GroupId::new();
        repository.save_group(group, "Accounts").unwrap();
        let mut expected = record("backup-record", Utc::now());
        expected.group_id = Some(group);
        repository.save_record(&expected).unwrap();
        repository
            .save_settings(UserSettings {
                language: Language::En,
                retention: RetentionPeriod::Forever,
                accent_color: AccentColor::Rose,
                ..UserSettings::default()
            })
            .unwrap();

        repository.backup_to(&backup_path).unwrap();
        assert_eq!(
            RecordPersistence::clear_records(repository.as_ref()).unwrap(),
            1
        );
        repository.delete_group(group).unwrap();
        repository.save_settings(UserSettings::default()).unwrap();

        let restored = repository
            .restore_from(
                &backup_path,
                RestoreBudget {
                    max_records: 500,
                    max_total_bytes: crate::services::session_records::DEFAULT_STORE_BYTES,
                    max_record_bytes: crate::services::session_records::MAX_CAPTURE_RECORD_BYTES,
                },
            )
            .unwrap();

        assert_eq!(restored.groups, vec![(group, "Accounts".to_owned())]);
        assert_eq!(restored.settings.language, Language::En);
        assert_eq!(restored.settings.retention, RetentionPeriod::Forever);
        assert_eq!(restored.settings.accent_color, AccentColor::Rose);
        assert_eq!(repository.load_recent(10).unwrap(), vec![expected]);
    }

    #[test]
    fn verified_archive_with_invalid_sqlite_leaves_live_database_untouched() {
        let directory = tempdir().unwrap();
        let live_path = directory.path().join("live.sqlite3");
        let invalid_database = directory.path().join("invalid.sqlite3");
        let backup_path = directory.path().join("invalid.clipbackup");
        let repository = SqliteRepository::open(live_path).unwrap();
        let expected = text_record("live-record", Utc::now(), "keep current data");
        repository.save_record(&expected).unwrap();
        fs::write(&invalid_database, b"not a sqlite database").unwrap();
        crate::services::backup::create_archive(&invalid_database, None, &backup_path, "0.1.0")
            .unwrap();

        let result = repository.restore_from(
            &backup_path,
            RestoreBudget {
                max_records: 500,
                max_total_bytes: crate::services::session_records::DEFAULT_STORE_BYTES,
                max_record_bytes: crate::services::session_records::MAX_CAPTURE_RECORD_BYTES,
            },
        );

        assert!(result.is_err());
        assert_eq!(repository.load_recent(10).unwrap(), vec![expected]);
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".restore-archive-")
        }));
    }

    #[test]
    fn moving_storage_preserves_data_reopens_from_bootstrap_and_stays_writable() {
        let root = tempdir().unwrap();
        let app_data = root.path().join("app-data");
        let destination = root.path().join("custom-storage");
        fs::create_dir_all(&app_data).unwrap();
        let original_path = app_data.join(DATABASE_FILE);
        let repository = SqliteRepository::open(original_path.clone()).unwrap();
        let group = GroupId::new();
        repository.save_group(group, "Accounts").unwrap();
        let mut expected = text_record("move-record", Utc::now(), "saved before move");
        expected.group_id = Some(group);
        expected.note = Some(RecordNote::new("work account").unwrap());
        repository.save_record(&expected).unwrap();
        repository
            .save_recognition(
                expected.id,
                Some("recognized invoice"),
                Some("https://example.test/account"),
                "complete",
            )
            .unwrap();
        let settings = UserSettings {
            language: Language::En,
            retention: RetentionPeriod::Forever,
            accent_color: AccentColor::Rose,
            ..UserSettings::default()
        };
        repository.save_settings(settings).unwrap();
        repository
            .save_excluded_applications(&["KeePass.exe".to_owned()])
            .unwrap();

        let location = repository
            .move_to_directory(&destination, &app_data)
            .unwrap();
        let destination_path = destination.join(DATABASE_FILE);

        assert_eq!(location.database_path, destination_path);
        assert!(!location.is_default);
        assert_eq!(repository.path(), destination_path);
        assert!(!original_path.exists());
        assert_eq!(repository.load_recent(10).unwrap(), vec![expected.clone()]);
        assert_eq!(
            repository.load_groups().unwrap(),
            vec![(group, "Accounts".to_owned())]
        );
        assert_eq!(repository.load_settings().unwrap(), settings);
        assert_eq!(
            repository.load_excluded_applications().unwrap(),
            vec!["KeePass.exe".to_owned()]
        );
        let recognized = repository.load_page(HistoryQuery::default()).unwrap();
        let recognized = recognized
            .records
            .iter()
            .find(|record| record.id == expected.id)
            .unwrap();
        assert_eq!(recognized.ocr_text.as_deref(), Some("recognized invoice"));
        assert_eq!(
            recognized.qr_text.as_deref(),
            Some("https://example.test/account")
        );

        let after_move = text_record("after-move", Utc::now(), "saved without restart");
        repository.save_record(&after_move).unwrap();
        drop(repository);

        let resolved = storage_location::resolve_database_path(&app_data).unwrap();
        assert_eq!(resolved, destination_path);
        let reopened = SqliteRepository::open(resolved).unwrap();
        let records = reopened.load_recent(10).unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.contains(&expected));
        assert!(records.contains(&after_move));
    }

    #[test]
    fn rejected_storage_move_keeps_the_original_database_active_and_cleans_staging() {
        let root = tempdir().unwrap();
        let app_data = root.path().join("app-data");
        let destination = root.path().join("occupied");
        fs::create_dir_all(&app_data).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("keep.txt"), b"occupied").unwrap();
        let original_path = app_data.join(DATABASE_FILE);
        let repository = SqliteRepository::open(original_path.clone()).unwrap();
        let expected = text_record("original", Utc::now(), "still available");
        repository.save_record(&expected).unwrap();

        assert!(
            repository
                .move_to_directory(&destination, &app_data)
                .is_err()
        );

        assert_eq!(repository.path(), original_path);
        assert_eq!(repository.load_recent(10).unwrap(), vec![expected]);
        assert!(!app_data.join("storage-location.json").exists());
        assert_eq!(
            fs::read_dir(&destination)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("keep.txt")]
        );
    }

    #[test]
    fn queued_storage_move_orders_writes_and_keeps_worker_available() {
        let root = tempdir().unwrap();
        let app_data = root.path().join("app-data");
        let destination = root.path().join("custom-storage");
        fs::create_dir_all(&app_data).unwrap();
        let repository = SqliteRepository::open(app_data.join(DATABASE_FILE)).unwrap();
        let available = Arc::new(AtomicBool::new(true));
        let worker =
            PersistenceWorker::start(Arc::clone(&repository), Arc::clone(&available)).unwrap();
        let before = text_record("queued-before", Utc::now(), "before move");
        let after = text_record("queued-after", Utc::now(), "after move");

        worker.save_record(&before).unwrap();
        worker
            .move_storage(destination.clone(), app_data.clone())
            .unwrap();
        worker.save_record(&after).unwrap();

        assert!(available.load(Ordering::Acquire));
        assert_eq!(repository.path(), destination.join(DATABASE_FILE));
        let records = repository.load_recent(10).unwrap();
        assert!(records.contains(&before));
        assert!(records.contains(&after));
    }

    #[test]
    fn restore_snapshot_failure_rolls_back_and_keeps_the_worker_available() {
        let directory = tempdir().unwrap();
        let live = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let source_path = directory.path().join("source.clipbackup");
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let old = record("old", Utc::now());
        let restored = record("restored", Utc::now() + Duration::seconds(1));
        live.save_record(&old).unwrap();
        source.save_record(&restored).unwrap();
        drop(source);
        let available = Arc::new(AtomicBool::new(true));
        let worker = PersistenceWorker::start(Arc::clone(&live), Arc::clone(&available)).unwrap();
        live.fail_next_restore_snapshot();

        assert!(
            worker
                .restore(
                    source_path,
                    RestoreBudget {
                        max_records: 500,
                        max_total_bytes: 64 * 1024 * 1024,
                        max_record_bytes: 16 * 1024 * 1024,
                    },
                )
                .is_err()
        );

        assert!(available.load(Ordering::Acquire));
        assert_eq!(live.full_record(old.id).unwrap(), old);
        assert!(live.full_record(restored.id).is_err());
        let after = record("after", Utc::now() + Duration::seconds(2));
        worker.save_record(&after).unwrap();
        assert_eq!(live.full_record(after.id).unwrap(), after);
    }

    #[test]
    fn restore_rollback_failure_makes_the_worker_terminally_unavailable() {
        let directory = tempdir().unwrap();
        let live = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let existing = record("existing", Utc::now() - Duration::seconds(1));
        live.save_record(&existing).unwrap();
        let source_path = directory.path().join("source.clipbackup");
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        source.save_record(&record("restored", Utc::now())).unwrap();
        drop(source);
        let available = Arc::new(AtomicBool::new(true));
        let worker = PersistenceWorker::start(Arc::clone(&live), Arc::clone(&available)).unwrap();
        live.fail_next_restore_snapshot();
        live.fail_next_restore_rollback();

        let error = worker
            .restore(
                source_path,
                RestoreBudget {
                    max_records: 500,
                    max_total_bytes: 64 * 1024 * 1024,
                    max_record_bytes: 16 * 1024 * 1024,
                },
            )
            .unwrap_err();

        assert!(matches!(error, PersistenceError::RestoreRollbackFailed));
        assert!(!available.load(Ordering::Acquire));
        assert!(matches!(
            worker.save_record(&record("after", Utc::now())),
            Err(PersistenceError::WorkerUnavailable)
        ));
        assert!(matches!(
            worker.load_page(HistoryQuery::default()),
            Err(PersistenceError::WorkerUnavailable)
        ));
        assert!(matches!(
            worker.full_record(existing.id),
            Err(PersistenceError::WorkerUnavailable)
        ));
    }

    #[test]
    fn restore_uses_the_source_snapshot_when_the_original_path_is_replaced() {
        let directory = tempdir().unwrap();
        let live = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let source_path = directory.path().join("source.clipbackup");
        let replacement_path = directory.path().join("replacement.clipbackup");
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let expected = record("snapshotted", Utc::now());
        source.save_record(&expected).unwrap();
        source
            .save_settings(UserSettings {
                language: Language::En,
                ..UserSettings::default()
            })
            .unwrap();
        drop(source);
        let replacement = SqliteRepository::open(replacement_path.clone()).unwrap();
        replacement
            .save_record(&record("replacement", Utc::now() + Duration::seconds(1)))
            .unwrap();
        replacement
            .save_settings(UserSettings {
                language: Language::ZhCn,
                ..UserSettings::default()
            })
            .unwrap();
        drop(replacement);
        let pause = Arc::new((Barrier::new(2), Barrier::new(2)));
        live.pause_after_restore_source_snapshot(Arc::clone(&pause));
        let restoring = {
            let live = Arc::clone(&live);
            let source_path = source_path.clone();
            thread::spawn(move || {
                live.restore_from(
                    &source_path,
                    RestoreBudget {
                        max_records: 500,
                        max_total_bytes: 64 * 1024 * 1024,
                        max_record_bytes: 16 * 1024 * 1024,
                    },
                )
            })
        };
        pause.0.wait();
        fs::remove_file(&source_path).unwrap();
        fs::rename(&replacement_path, &source_path).unwrap();
        pause.1.wait();

        let restored = restoring.join().unwrap().unwrap();
        assert_eq!(restored.settings.language, Language::En);
        assert_eq!(live.full_record(expected.id).unwrap(), expected);
        assert!(
            restored
                .page
                .records
                .iter()
                .any(|summary| summary.id == expected.id)
        );
    }

    #[test]
    fn restore_accepts_valid_history_larger_than_the_bounded_working_set() {
        let directory = tempdir().unwrap();
        let live_path = directory.path().join("live.sqlite3");
        let backup_path = directory.path().join("large-history.clipbackup");
        let repository = SqliteRepository::open(live_path).unwrap();
        let source = SqliteRepository::open(backup_path.clone()).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 26, 14, 0, 0).unwrap();
        for index in 0..6 {
            source
                .save_record(&ClipboardRecord::from_capture(
                    crate::domain::CapturedClipboard {
                        content_identity: ContentIdentity::new(format!("large-backup-{index}")),
                        captured_at: now + Duration::seconds(index),
                        source: SourceIdentity::default(),
                        representations: vec![ClipboardRepresentation::Png {
                            bytes: vec![index as u8; 12 * 1024 * 1024],
                        }],
                    },
                ))
                .unwrap();
        }
        drop(source);

        let budget = RestoreBudget {
            max_records: 5,
            max_total_bytes: 64 * 1024 * 1024,
            max_record_bytes: 16 * 1024 * 1024,
        };
        repository.restore_from(&backup_path, budget).unwrap();

        assert_eq!(
            repository
                .load_page(HistoryQuery::default())
                .unwrap()
                .records
                .len(),
            6
        );
        let working_set = repository.load_recent_bounded(budget).unwrap();
        assert_eq!(working_set.len(), 5);
        assert_eq!(working_set[0].content_identity.as_str(), "large-backup-5");
        assert_eq!(working_set[4].content_identity.as_str(), "large-backup-1");
        assert!(
            working_set
                .iter()
                .all(|record| record.content_identity.as_str() != "large-backup-0")
        );
    }

    #[test]
    fn invalid_restore_does_not_modify_live_database() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let expected = text_record("live", Utc::now(), "keep me");
        repository.save_record(&expected).unwrap();
        let invalid = directory.path().join("invalid.clipbackup");
        fs::write(&invalid, b"not sqlite").unwrap();

        assert!(
            repository
                .restore_from(
                    &invalid,
                    RestoreBudget {
                        max_records: 500,
                        max_total_bytes: crate::services::session_records::DEFAULT_STORE_BYTES,
                        max_record_bytes:
                            crate::services::session_records::MAX_CAPTURE_RECORD_BYTES,
                    },
                )
                .is_err()
        );
        assert_eq!(repository.load_recent(10).unwrap(), vec![expected]);
    }

    #[test]
    fn restore_rejects_unsupported_representation_kinds_without_modifying_live_database() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let expected_record = text_record("live", Utc::now(), "keep me");
        let expected_settings = UserSettings {
            language: Language::En,
            retention: RetentionPeriod::Forever,
            accent_color: AccentColor::Rose,
            ..UserSettings::default()
        };
        repository.save_record(&expected_record).unwrap();
        repository.save_settings(expected_settings).unwrap();

        let kind = "future_format";
        let source_path = directory
            .path()
            .join(format!("unsupported-{kind}.clipbackup"));
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let source_record = text_record("source", Utc::now(), "replace me");
        source.save_record(&source_record).unwrap();
        lock_unpoisoned(&source.state)
            .execute(
                "UPDATE clipboard_representations
                 SET kind = ?1, text_value = NULL, blob_value = ?2
                 WHERE record_id = ?3",
                params![
                    kind,
                    b"UNSUPPORTED_SECRET_PAYLOAD",
                    source_record.id.as_uuid().to_string()
                ],
            )
            .unwrap();
        drop(source);

        let error = repository
            .restore_from(
                &source_path,
                RestoreBudget {
                    max_records: 500,
                    max_total_bytes: crate::services::session_records::DEFAULT_STORE_BYTES,
                    max_record_bytes: crate::services::session_records::MAX_CAPTURE_RECORD_BYTES,
                },
            )
            .unwrap_err();

        assert!(matches!(
            &error,
            PersistenceError::UnsupportedRepresentationKind(error_kind)
                if error_kind == kind
        ));
        assert!(!error.to_string().contains("UNSUPPORTED_SECRET_PAYLOAD"));
        assert_eq!(repository.load_recent(10).unwrap(), vec![expected_record]);
        assert_eq!(repository.load_settings().unwrap(), expected_settings);
    }

    #[test]
    fn backup_rejects_the_live_database_path() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("live.sqlite3");
        let repository = SqliteRepository::open(path.clone()).unwrap();

        assert!(matches!(
            repository.backup_to(&path),
            Err(PersistenceError::InvalidData)
        ));
    }

    #[test]
    fn delete_and_clear_remove_records_without_touching_settings() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let now = Utc::now();
        let first = text_record("first", now, "one");
        let second = text_record("second", now + Duration::seconds(1), "two");
        repository.save_record(&first).unwrap();
        repository.save_record(&second).unwrap();

        RecordPersistence::delete_record(repository.as_ref(), first.id).unwrap();
        let loaded = repository.load_recent(10).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, second.id);

        RecordPersistence::clear_records(repository.as_ref()).unwrap();
        assert!(repository.load_recent(10).unwrap().is_empty());
        assert_eq!(repository.load_settings().unwrap(), UserSettings::default());
    }

    #[test]
    fn group_order_persists_and_deleting_a_group_ungroups_its_records() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let repository = SqliteRepository::open(path.clone()).unwrap();
        let first = GroupId::new();
        let second = GroupId::new();
        repository.save_group(first, "First").unwrap();
        repository.save_group(second, "Second").unwrap();
        repository.save_group_order(&[second, first]).unwrap();

        let mut grouped = text_record("grouped", Utc::now(), "value");
        grouped.group_id = Some(first);
        repository.save_record(&grouped).unwrap();
        assert_eq!(repository.delete_group(first).unwrap(), 1);
        drop(repository);

        let reopened = SqliteRepository::open(path).unwrap();
        assert_eq!(
            reopened.load_groups().unwrap(),
            vec![(second, "Second".to_owned())]
        );
        let records = reopened.load_recent(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].group_id, None);
    }

    fn create_v1_database(
        path: &Path,
        records: &[(String, DateTime<Utc>, usize)],
        retention: RetentionPeriod,
    ) {
        create_v1_database_with_retention(path, records, Some(retention_value(retention)));
    }

    fn create_v1_database_with_retention(
        path: &Path,
        records: &[(String, DateTime<Utc>, usize)],
        retention: Option<&str>,
    ) {
        let mut connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode = DELETE;
                 CREATE TABLE clipboard_records (
                    id TEXT PRIMARY KEY NOT NULL,
                    content_identity TEXT NOT NULL,
                    captured_at TEXT NOT NULL,
                    source_application TEXT,
                    source_path TEXT,
                    note TEXT,
                    group_id TEXT,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    favorite INTEGER NOT NULL DEFAULT 0,
                    sensitive INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE INDEX clipboard_records_captured_at
                    ON clipboard_records(captured_at DESC);
                 CREATE TABLE clipboard_representations (
                    record_id TEXT NOT NULL REFERENCES clipboard_records(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    text_value TEXT,
                    blob_value BLOB,
                    PRIMARY KEY(record_id, position)
                 );
                 CREATE TABLE app_settings (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        if let Some(retention) = retention {
            transaction
                .execute(
                    "INSERT INTO app_settings(key, value) VALUES ('retention', ?1)",
                    [retention],
                )
                .unwrap();
        }
        for (identity, captured_at, payload_bytes) in records {
            let id = RecordId::new().as_uuid().to_string();
            transaction
                .execute(
                    "INSERT INTO clipboard_records (
                        id, content_identity, captured_at, pinned, favorite, sensitive
                     ) VALUES (?1, ?2, ?3, 0, 0, 0)",
                    params![id, identity, captured_at.to_rfc3339()],
                )
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO clipboard_representations
                     (record_id, position, kind, blob_value)
                     VALUES (?1, 0, 'png', zeroblob(?2))",
                    params![id, *payload_bytes as i64],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        connection.close().unwrap();
    }

    fn insert_v1_record(
        connection: &Connection,
        identity: &str,
        captured_at: DateTime<Utc>,
        payload_bytes: usize,
    ) {
        let id = RecordId::new().as_uuid().to_string();
        connection
            .execute(
                "INSERT INTO clipboard_records (
                    id, content_identity, captured_at, pinned, favorite, sensitive
                 ) VALUES (?1, ?2, ?3, 0, 0, 0)",
                params![id, identity, captured_at.to_rfc3339()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO clipboard_representations
                 (record_id, position, kind, blob_value)
                 VALUES (?1, 0, 'png', zeroblob(?2))",
                params![id, payload_bytes as i64],
            )
            .unwrap();
    }

    fn record_count_at(path: &Path) -> usize {
        let connection = Connection::open(path).unwrap();
        connection
            .query_row("SELECT COUNT(*) FROM clipboard_records", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|value| value as usize)
            .unwrap()
    }

    #[test]
    fn schema_is_versioned_and_restart_restores_text_binary_metadata_and_note() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let expected = record("same", Utc.with_ymd_and_hms(2026, 8, 23, 1, 2, 3).unwrap());
        {
            let repository = SqliteRepository::open(path.clone()).unwrap();
            repository.save_record(&expected).unwrap();
            assert_eq!(repository.schema_version().unwrap(), SCHEMA_VERSION);
        }

        let repository = SqliteRepository::open(path).unwrap();
        assert_eq!(repository.load_recent(500).unwrap(), vec![expected]);
    }

    #[test]
    fn backup_and_restore_preserve_all_six_representation_formats() {
        let directory = tempdir().unwrap();
        let live_path = directory.path().join("live.sqlite3");
        let backup_path = directory.path().join("all-formats.clipbackup");
        let repository = SqliteRepository::open(live_path).unwrap();
        let mut expected = text_record("all-formats-backup", Utc::now(), "plain");
        expected.representations = vec![
            ClipboardRepresentation::UnicodeText {
                text: "plain".into(),
            },
            ClipboardRepresentation::Rtf {
                bytes: br"{\rtf1 rich}".to_vec(),
            },
            ClipboardRepresentation::Html {
                bytes: b"<b>html</b>".to_vec(),
            },
            ClipboardRepresentation::Png {
                bytes: vec![1, 2, 3],
            },
            ClipboardRepresentation::DibV5 {
                bytes: vec![4, 5, 6],
            },
            ClipboardRepresentation::FileList {
                paths: vec![r"C:\Temp\one.txt".into(), r"D:\two.bin".into()],
            },
        ];
        repository.save_record(&expected).unwrap();
        repository.backup_to(&backup_path).unwrap();
        RecordPersistence::clear_records(repository.as_ref()).unwrap();

        repository
            .restore_from(
                &backup_path,
                RestoreBudget {
                    max_records: 500,
                    max_total_bytes: crate::services::session_records::DEFAULT_STORE_BYTES,
                    max_record_bytes: crate::services::session_records::MAX_CAPTURE_RECORD_BYTES,
                },
            )
            .unwrap();

        assert_eq!(repository.full_record(expected.id).unwrap(), expected);
    }

    #[test]
    fn schema_three_migrates_to_current_and_backfills_content_kind_without_losing_records() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE clipboard_records (
                    id TEXT PRIMARY KEY NOT NULL, content_identity TEXT NOT NULL,
                    captured_at TEXT NOT NULL, source_application TEXT, source_path TEXT,
                    note TEXT, group_id TEXT, pinned INTEGER NOT NULL DEFAULT 0,
                    favorite INTEGER NOT NULL DEFAULT 0, sensitive INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE clipboard_representations (
                    record_id TEXT NOT NULL REFERENCES clipboard_records(id) ON DELETE CASCADE,
                    position INTEGER NOT NULL, kind TEXT NOT NULL, text_value TEXT,
                    blob_value BLOB, PRIMARY KEY(record_id, position)
                 );
                 CREATE TABLE app_settings (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
                 CREATE TABLE clipboard_groups (
                    id TEXT PRIMARY KEY NOT NULL, name TEXT NOT NULL, position INTEGER NOT NULL
                 );
                 PRAGMA user_version = 3;",
                )
                .unwrap();
            let id = RecordId::new().as_uuid().to_string();
            connection
                .execute(
                    "INSERT INTO clipboard_records
                 (id, content_identity, captured_at, pinned, favorite, sensitive)
                 VALUES (?1, 'legacy', ?2, 0, 0, 0)",
                    params![id, Utc::now().to_rfc3339()],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO clipboard_representations
                 (record_id, position, kind, blob_value) VALUES (?1, 0, 'html', ?2)",
                    params![id, b"<b>legacy</b>"],
                )
                .unwrap();
        }

        let repository = SqliteRepository::open(path).unwrap();
        assert_eq!(repository.schema_version().unwrap(), SCHEMA_VERSION);
        let page = repository.load_page(HistoryQuery::default()).unwrap();
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].content_kind, ContentKind::RichText);
        assert!(matches!(
            repository
                .full_record(page.records[0].id)
                .unwrap()
                .representations[0],
            ClipboardRepresentation::Html { .. }
        ));
    }

    #[test]
    fn all_six_representations_round_trip_in_order_after_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let mut expected = text_record("all-formats", Utc::now(), "plain");
        expected.representations = vec![
            ClipboardRepresentation::UnicodeText {
                text: "plain".into(),
            },
            ClipboardRepresentation::Rtf {
                bytes: br"{\rtf1 rich}".to_vec(),
            },
            ClipboardRepresentation::Html {
                bytes: b"<b>html</b>".to_vec(),
            },
            ClipboardRepresentation::Png {
                bytes: vec![1, 2, 3],
            },
            ClipboardRepresentation::DibV5 {
                bytes: vec![4, 5, 6],
            },
            ClipboardRepresentation::FileList {
                paths: vec![r"C:\Temp\one.txt".into(), r"D:\two.bin".into()],
            },
        ];
        SqliteRepository::open(path.clone())
            .unwrap()
            .save_record(&expected)
            .unwrap();
        let reopened = SqliteRepository::open(path).unwrap();
        assert_eq!(reopened.full_record(expected.id).unwrap(), expected);
    }

    #[test]
    fn escaped_file_list_uses_decoded_capacity_after_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let paths = escaped_file_paths(MAX_FILE_LIST_PATHS, 9 * 1024 * 1024);
        let logical_bytes = paths.iter().map(String::len).sum::<usize>();
        let encoded_bytes = serde_json::to_vec(&paths).unwrap().len();
        assert!(logical_bytes <= MAX_FILE_LIST_LOGICAL_BYTES);
        assert!(encoded_bytes > MAX_CAPTURE_RECORD_BYTES);
        assert!(encoded_bytes <= MAX_FILE_LIST_ENCODED_BYTES);
        let expected = file_list_record("escaped-file-list", paths);
        let id = expected.id;

        SqliteRepository::open(path.clone())
            .unwrap()
            .save_record(&expected)
            .unwrap();

        let reopened = SqliteRepository::open(path).unwrap();
        assert_eq!(reopened.full_record(id).unwrap(), expected);
        let details = reopened.record_details(id).unwrap();
        assert_eq!(details.representations.len(), 1);
        let file_list = &details.representations[0];
        assert_eq!(file_list.byte_length, logical_bytes);
        assert_eq!(file_list.item_count, Some(MAX_FILE_LIST_PATHS));
        assert!(file_list.truncated);
        let projected_bytes = file_list
            .paths
            .as_ref()
            .unwrap()
            .iter()
            .map(String::len)
            .sum::<usize>();
        assert!(projected_bytes <= MAX_DETAIL_FILE_LIST_BYTES);
    }

    #[test]
    fn encoded_file_list_bound_is_rejected_before_write() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let repository = SqliteRepository::open(path.clone()).unwrap();
        let paths = escaped_file_paths(MAX_FILE_LIST_PATHS, 13 * 1024 * 1024);
        let logical_bytes = paths.iter().map(String::len).sum::<usize>();
        let encoded_bytes = serde_json::to_vec(&paths).unwrap().len();
        assert!(logical_bytes <= MAX_FILE_LIST_LOGICAL_BYTES);
        assert!(encoded_bytes > MAX_FILE_LIST_ENCODED_BYTES);

        assert!(matches!(
            repository.save_record(&file_list_record("encoded-too-large", paths)),
            Err(PersistenceError::InvalidData)
        ));
        assert_eq!(record_count_at(&path), 0);
    }

    #[test]
    fn file_list_quota_counts_encoded_database_bytes() {
        let directory = tempdir().unwrap();
        let paths = escaped_file_paths(MAX_FILE_LIST_PATHS, 128 * 1024);
        let encoded_bytes = serde_json::to_vec(&paths).unwrap().len();
        let exact_quota = encoded_bytes + REPRESENTATION_OVERHEAD_BYTES;
        let retained_path = directory.path().join("retained.sqlite3");
        let retained = SqliteRepository::open_with_quota(
            retained_path,
            DiskQuota {
                max_records: 1,
                max_payload_bytes: exact_quota,
                incremental_vacuum_pages: 1,
            },
        )
        .unwrap();
        retained
            .save_record(&file_list_record("exact-quota", paths.clone()))
            .unwrap();
        assert_eq!(retained.load_recent(10).unwrap().len(), 1);

        let rejected_path = directory.path().join("rejected.sqlite3");
        let rejected = SqliteRepository::open_with_quota(
            rejected_path.clone(),
            DiskQuota {
                max_records: 1,
                max_payload_bytes: exact_quota - 1,
                incremental_vacuum_pages: 1,
            },
        )
        .unwrap();
        assert!(matches!(
            rejected.save_record(&file_list_record("over-quota", paths)),
            Err(PersistenceError::InvalidData)
        ));
        assert_eq!(record_count_at(&rejected_path), 0);
    }

    #[test]
    fn malformed_file_list_is_skipped_by_bounded_restore_without_panicking() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let valid = text_record("valid", Utc::now() - Duration::seconds(1), "ok");
        repository.save_record(&valid).unwrap();
        let malformed = text_record("malformed", Utc::now(), "bad");
        repository.save_record(&malformed).unwrap();
        lock_unpoisoned(&repository.state)
            .execute(
                "UPDATE clipboard_representations
             SET kind = 'file_list', text_value = '[\"bad\\u0000path\"]', blob_value = NULL
             WHERE record_id = ?1",
                [malformed.id.as_uuid().to_string()],
            )
            .unwrap();

        assert_eq!(repository.load_recent(10).unwrap(), vec![valid]);
    }

    #[test]
    fn keyset_paging_is_stable_across_new_inserts_and_caps_limits() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        for index in 0..40 {
            repository
                .save_record(&text_record(
                    &format!("record-{index}"),
                    base + Duration::seconds(index),
                    "x",
                ))
                .unwrap();
        }
        let first = repository
            .load_page(HistoryQuery {
                limit: 20,
                ..HistoryQuery::default()
            })
            .unwrap();
        repository
            .save_record(&text_record("inserted", base + Duration::seconds(100), "x"))
            .unwrap();
        let second = repository
            .load_page(HistoryQuery {
                cursor: first.next_cursor.clone(),
                limit: 20,
                ..HistoryQuery::default()
            })
            .unwrap();
        let ids = first
            .records
            .iter()
            .chain(&second.records)
            .map(|record| record.id)
            .collect::<HashSet<_>>();
        assert_eq!(first.records.len(), 20);
        assert_eq!(second.records.len(), 20);
        assert_eq!(ids.len(), 40);
        assert_eq!(
            repository
                .load_page(HistoryQuery::default())
                .unwrap()
                .records
                .len(),
            41
        );
        assert_eq!(
            repository
                .load_page(HistoryQuery {
                    limit: usize::MAX,
                    ..HistoryQuery::default()
                })
                .unwrap()
                .records
                .len(),
            41
        );
    }

    #[test]
    fn keyset_paging_breaks_equal_timestamps_by_id() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let captured_at = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        let records = (0..3)
            .map(|index| text_record(&format!("tie-{index}"), captured_at, "x"))
            .collect::<Vec<_>>();
        for record in &records {
            repository.save_record(record).unwrap();
        }
        let mut expected_ids = records.iter().map(|record| record.id).collect::<Vec<_>>();
        expected_ids.sort_by_key(|id| std::cmp::Reverse(id.as_uuid().to_string()));

        let first = repository
            .load_page(HistoryQuery {
                limit: 1,
                ..HistoryQuery::default()
            })
            .unwrap();
        let second = repository
            .load_page(HistoryQuery {
                cursor: first.next_cursor.clone(),
                limit: 2,
                ..HistoryQuery::default()
            })
            .unwrap();
        let actual_ids = first
            .records
            .iter()
            .chain(&second.records)
            .map(|record| record.id)
            .collect::<Vec<_>>();

        assert_eq!(actual_ids, expected_ids);
    }

    #[test]
    fn history_page_filters_by_kind_group_ungrouped_and_favorite() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let group = GroupId::new();
        repository.save_group(group, "Filtered").unwrap();
        let now = Utc::now();
        let plain = text_record("plain", now, "plain");
        let mut rich = text_record("rich", now + Duration::seconds(1), "fallback");
        rich.representations.push(ClipboardRepresentation::Html {
            bytes: b"<b>rich</b>".to_vec(),
        });
        rich.group_id = Some(group);
        rich.favorite = true;
        let mut image = text_record("image", now + Duration::seconds(2), "fallback");
        image.representations.push(ClipboardRepresentation::Png {
            bytes: vec![1, 2, 3],
        });
        for record in [&plain, &rich, &image] {
            repository.save_record(record).unwrap();
        }

        let rich_page = repository
            .load_page(HistoryQuery {
                content_kind: Some(ContentKind::RichText),
                ..HistoryQuery::default()
            })
            .unwrap();
        let grouped_page = repository
            .load_page(HistoryQuery {
                group_id: Some(group),
                ..HistoryQuery::default()
            })
            .unwrap();
        let ungrouped_page = repository
            .load_page(HistoryQuery {
                ungrouped_only: true,
                ..HistoryQuery::default()
            })
            .unwrap();
        let favorite_page = repository
            .load_page(HistoryQuery {
                favorites_only: true,
                ..HistoryQuery::default()
            })
            .unwrap();

        assert_eq!(rich_page.records.len(), 1);
        assert_eq!(rich_page.records[0].id, rich.id);
        assert_eq!(grouped_page.records[0].id, rich.id);
        assert_eq!(ungrouped_page.records.len(), 2);
        assert!(
            ungrouped_page
                .records
                .iter()
                .all(|record| record.id != rich.id)
        );
        assert_eq!(favorite_page.records[0].id, rich.id);
    }

    #[test]
    fn history_summary_projects_file_names_and_total_count_after_reopen() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let paths = vec![
            r"C:\Users\admin\Downloads\report.pdf".to_owned(),
            r"C:\Users\admin\Downloads\budget.xlsx".to_owned(),
            r"C:\Users\admin\Downloads\notes.txt".to_owned(),
            r"C:\Users\admin\Downloads\archive.zip".to_owned(),
        ];
        let expected = file_list_record("file-summary", paths.clone());
        SqliteRepository::open(path.clone())
            .unwrap()
            .save_record(&expected)
            .unwrap();

        let page = SqliteRepository::open(path)
            .unwrap()
            .load_page(HistoryQuery::default())
            .unwrap();

        assert_eq!(page.records[0].file_paths, paths[..3]);
        assert_eq!(page.records[0].file_count, paths.len());
    }

    #[test]
    fn history_summary_bounds_text_without_loading_binary_payloads() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let mut expected = text_record("large-summary", Utc::now(), &"x".repeat(8_192));
        expected.representations.push(ClipboardRepresentation::Png {
            bytes: vec![7; 1024 * 1024],
        });
        repository.save_record(&expected).unwrap();

        let page = repository.load_page(HistoryQuery::default()).unwrap();

        assert_eq!(
            page.records[0].text.as_deref().unwrap().chars().count(),
            4_096
        );
        assert!(page.records[0].has_image);
        assert_eq!(repository.full_record(expected.id).unwrap(), expected);
    }

    #[test]
    fn production_quota_uses_storage_limit_without_legacy_byte_or_count_caps() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        repository
            .save_settings(UserSettings {
                storage_limit: StorageLimit::FiveGb,
                ..UserSettings::default()
            })
            .unwrap();
        let connection = lock_unpoisoned(&repository.state);

        assert_eq!(
            effective_quota_limits(&connection, repository.quota).unwrap(),
            (None, Some(5 * 1024 * 1024 * 1024))
        );
        drop(connection);
        repository
            .save_settings(UserSettings {
                storage_limit: StorageLimit::TenGb,
                ..UserSettings::default()
            })
            .unwrap();
        let connection = lock_unpoisoned(&repository.state);
        assert_eq!(
            effective_quota_limits(&connection, repository.quota).unwrap(),
            (None, Some(10 * 1024 * 1024 * 1024))
        );
        drop(connection);
        repository
            .save_settings(UserSettings {
                storage_limit: StorageLimit::Unlimited,
                ..UserSettings::default()
            })
            .unwrap();
        let connection = lock_unpoisoned(&repository.state);
        assert_eq!(
            effective_quota_limits(&connection, repository.quota).unwrap(),
            (None, None)
        );
    }

    #[test]
    fn storage_policy_update_persists_and_applies_the_new_quota_atomically() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let quota = DiskQuota {
            max_records: 2,
            max_payload_bytes: usize::MAX,
            incremental_vacuum_pages: 1,
        };
        let repository = SqliteRepository::open_with_quota(path.clone(), quota).unwrap();
        let now = Utc::now();
        let mut favorite = text_record("favorite", now, "favorite");
        favorite.favorite = true;
        repository.save_record(&favorite).unwrap();
        repository
            .save_record(&text_record(
                "ordinary",
                now + Duration::seconds(1),
                "ordinary",
            ))
            .unwrap();
        {
            let mut connection = lock_unpoisoned(&repository.state);
            let transaction = connection.transaction().unwrap();
            write_record(
                &transaction,
                &text_record("newest", now + Duration::seconds(2), "newest"),
            )
            .unwrap();
            transaction.commit().unwrap();
        }

        assert_eq!(
            repository
                .update_storage_policy(StorageLimit::FiveGb, false)
                .unwrap(),
            1
        );
        assert_eq!(repository.load_recent(10).unwrap().len(), 2);
        assert!(repository.full_record(favorite.id).is_ok());
        drop(repository);

        let reopened = SqliteRepository::open_with_quota(path, quota).unwrap();
        let settings = reopened.load_settings().unwrap();
        assert_eq!(settings.storage_limit, StorageLimit::FiveGb);
        assert!(!settings.evict_favorites_when_full);
    }

    #[test]
    fn storage_policy_can_evict_favorites_only_after_explicit_opt_in() {
        let directory = tempdir().unwrap();
        let quota = DiskQuota {
            max_records: 1,
            max_payload_bytes: usize::MAX,
            incremental_vacuum_pages: 1,
        };
        let repository =
            SqliteRepository::open_with_quota(directory.path().join("history.sqlite3"), quota)
                .unwrap();
        let now = Utc::now();
        let mut favorite = text_record("favorite", now, "favorite");
        favorite.favorite = true;
        {
            let mut connection = lock_unpoisoned(&repository.state);
            let transaction = connection.transaction().unwrap();
            write_record(&transaction, &favorite).unwrap();
            write_record(
                &transaction,
                &text_record("newest", now + Duration::seconds(1), "newest"),
            )
            .unwrap();
            transaction.commit().unwrap();
        }

        assert_eq!(
            repository
                .update_storage_policy(StorageLimit::OneGb, false)
                .unwrap(),
            1
        );
        assert!(repository.full_record(favorite.id).is_ok());
        {
            let mut latest = text_record("latest", now + Duration::seconds(2), "latest");
            latest.favorite = true;
            let mut connection = lock_unpoisoned(&repository.state);
            let transaction = connection.transaction().unwrap();
            write_record(&transaction, &latest).unwrap();
            transaction.commit().unwrap();
        }
        assert_eq!(
            repository
                .update_storage_policy(StorageLimit::OneGb, true)
                .unwrap(),
            1
        );
        assert!(
            repository
                .load_recent(10)
                .unwrap()
                .iter()
                .all(|record| record.id != favorite.id)
        );
    }

    #[test]
    fn restore_rejects_oversized_metadata_before_reading_blob_payload() {
        let directory = tempdir().unwrap();
        let live_path = directory.path().join("live.sqlite3");
        let source_path = directory.path().join("oversized.clipbackup");
        let repository = SqliteRepository::open(live_path).unwrap();
        let expected = text_record("live", Utc::now(), "keep");
        repository.save_record(&expected).unwrap();
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let oversized = text_record("oversized", Utc::now(), "placeholder");
        source.save_record(&oversized).unwrap();
        lock_unpoisoned(&source.state)
            .execute(
                "UPDATE clipboard_representations
                 SET kind = 'png', text_value = NULL, blob_value = zeroblob(?1)
                 WHERE record_id = ?2",
                params![2 * 1024 * 1024, oversized.id.as_uuid().to_string()],
            )
            .unwrap();
        drop(source);

        assert!(matches!(
            repository.restore_from(
                &source_path,
                RestoreBudget {
                    max_records: 10,
                    max_total_bytes: 1024 * 1024,
                    max_record_bytes: 1024 * 1024,
                },
            ),
            Err(PersistenceError::InvalidData)
        ));
        assert_eq!(repository.load_recent(10).unwrap(), vec![expected]);
    }

    #[test]
    fn history_page_scans_past_malformed_summaries_for_valid_lookahead() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        let oldest = text_record("oldest", base, "oldest");
        let middle = text_record("middle", base + Duration::seconds(1), "middle");
        let malformed = text_record("malformed", base + Duration::seconds(2), "bad");
        let newest = text_record("newest", base + Duration::seconds(3), "newest");
        for record in [&oldest, &middle, &malformed, &newest] {
            repository.save_record(record).unwrap();
        }
        lock_unpoisoned(&repository.state)
            .execute(
                "UPDATE clipboard_records SET content_kind = 'corrupt'
                 WHERE id = ?1",
                [malformed.id.as_uuid().to_string()],
            )
            .unwrap();

        let first = repository
            .load_page(HistoryQuery {
                limit: 2,
                ..HistoryQuery::default()
            })
            .unwrap();
        let second = repository
            .load_page(HistoryQuery {
                cursor: first.next_cursor.clone(),
                limit: 2,
                ..HistoryQuery::default()
            })
            .unwrap();

        assert_eq!(
            first
                .records
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![newest.id, middle.id]
        );
        assert!(first.next_cursor.is_some());
        assert_eq!(
            second
                .records
                .iter()
                .map(|record| record.id)
                .collect::<Vec<_>>(),
            vec![oldest.id]
        );
    }

    #[test]
    fn quota_never_evicts_pinned_and_requires_opt_in_for_favorites() {
        let directory = tempdir().unwrap();
        let quota = DiskQuota {
            max_records: 3,
            max_payload_bytes: 1024,
            incremental_vacuum_pages: 1,
        };
        let repository =
            SqliteRepository::open_with_quota(directory.path().join("history.sqlite3"), quota)
                .unwrap();
        let now = Utc::now();
        let mut pinned = text_record("pinned", now, "x");
        pinned.pinned = true;
        let mut favorite = text_record("favorite", now + Duration::seconds(1), "x");
        favorite.favorite = true;
        repository
            .save_record(&text_record("ordinary", now - Duration::seconds(1), "x"))
            .unwrap();
        repository.save_record(&pinned).unwrap();
        repository.save_record(&favorite).unwrap();
        {
            let mut connection = lock_unpoisoned(&repository.state);
            let transaction = connection.transaction().unwrap();
            write_record(
                &transaction,
                &text_record("newest", now + Duration::seconds(2), "x"),
            )
            .unwrap();
            let strict = DiskQuota {
                max_records: 2,
                ..quota
            };
            enforce_disk_quota(&transaction, strict).unwrap();
            transaction.commit().unwrap();
        }
        let records = repository.load_recent(10).unwrap();
        assert!(records.iter().any(|record| record.id == pinned.id));
        assert!(records.iter().any(|record| record.id == favorite.id));

        repository
            .save_settings(UserSettings {
                evict_favorites_when_full: true,
                retention: RetentionPeriod::Forever,
                ..UserSettings::default()
            })
            .unwrap();
        repository.prune(RetentionPeriod::Forever, now).unwrap();
        assert!(
            repository
                .load_recent(10)
                .unwrap()
                .iter()
                .any(|record| record.id == pinned.id)
        );
    }

    #[test]
    fn bounded_restore_stops_before_materializing_more_than_the_memory_budget() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        for index in 0..4 {
            let item = ClipboardRecord::from_capture(crate::domain::CapturedClipboard {
                content_identity: ContentIdentity::new(format!("large-{index}")),
                captured_at: now + Duration::seconds(index),
                source: SourceIdentity::default(),
                representations: vec![ClipboardRepresentation::Png {
                    bytes: vec![index as u8; 12 * 1024 * 1024],
                }],
            });
            repository.save_record(&item).unwrap();
        }

        let restored = repository
            .load_recent_bounded(RestoreBudget {
                max_records: 500,
                max_total_bytes: 48 * 1024 * 1024,
                max_record_bytes: 16 * 1024 * 1024,
            })
            .unwrap();

        assert_eq!(restored.len(), 3);
        assert_eq!(restored[0].content_identity.as_str(), "large-3");
        assert_eq!(restored[2].content_identity.as_str(), "large-1");
    }

    #[test]
    fn bounded_restore_skips_an_externally_inserted_oversized_blob() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let small = record("small", Utc::now() - Duration::seconds(1));
        repository.save_record(&small).unwrap();
        let oversized_id = RecordId::new();
        {
            let connection = lock_unpoisoned(&repository.state);
            connection
                .execute(
                    "INSERT INTO clipboard_records (
                        id, content_identity, captured_at, pinned, favorite, sensitive
                     ) VALUES (?1, ?2, ?3, 0, 0, 0)",
                    params![
                        oversized_id.as_uuid().to_string(),
                        "external-oversized",
                        Utc::now().to_rfc3339(),
                    ],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO clipboard_representations
                     (record_id, position, kind, blob_value)
                     VALUES (?1, 0, 'png', zeroblob(?2))",
                    params![
                        oversized_id.as_uuid().to_string(),
                        (16 * 1024 * 1024 + 1) as i64,
                    ],
                )
                .unwrap();
        }

        let restored = repository
            .load_recent_bounded(RestoreBudget {
                max_records: 500,
                max_total_bytes: 48 * 1024 * 1024,
                max_record_bytes: 16 * 1024 * 1024,
            })
            .unwrap();

        assert_eq!(restored, vec![small]);
    }

    #[test]
    fn record_details_rejects_externally_oversized_blob_from_metadata() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let record = text_record("oversized-details", Utc::now(), "caption");
        repository.save_record(&record).unwrap();
        {
            let connection = lock_unpoisoned(&repository.state);
            connection
                .execute(
                    "UPDATE clipboard_representations
                     SET kind = 'png', text_value = NULL, blob_value = zeroblob(?1)
                     WHERE record_id = ?2 AND position = 0",
                    params![
                        (crate::services::session_records::MAX_CAPTURE_RECORD_BYTES + 1) as i64,
                        record.id.as_uuid().to_string()
                    ],
                )
                .unwrap();
        }

        assert!(matches!(
            repository.record_details(record.id),
            Err(PersistenceError::InvalidData)
        ));
    }

    #[test]
    fn bounded_restore_count_limit_is_applied_during_iteration() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        for index in 0..4 {
            repository
                .save_record(&text_record(
                    &format!("record-{index}"),
                    now + Duration::seconds(index),
                    "x",
                ))
                .unwrap();
        }

        let restored = repository
            .load_recent_bounded(RestoreBudget {
                max_records: 2,
                max_total_bytes: 1024,
                max_record_bytes: 1024,
            })
            .unwrap();

        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].content_identity.as_str(), "record-3");
        assert_eq!(restored[1].content_identity.as_str(), "record-2");
    }

    #[test]
    fn bounded_restore_skips_malformed_representation_columns_without_reading_payload() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let valid = text_record("valid", Utc::now() - Duration::seconds(1), "ok");
        repository.save_record(&valid).unwrap();
        let malformed_id = RecordId::new();
        {
            let connection = lock_unpoisoned(&repository.state);
            connection
                .execute(
                    "INSERT INTO clipboard_records (
                        id, content_identity, captured_at, pinned, favorite, sensitive
                     ) VALUES (?1, ?2, ?3, 0, 0, 0)",
                    params![
                        malformed_id.as_uuid().to_string(),
                        "malformed",
                        Utc::now().to_rfc3339(),
                    ],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO clipboard_representations
                     (record_id, position, kind, text_value, blob_value)
                     VALUES (?1, 0, 'png', 'wrong-column', X'01')",
                    [malformed_id.as_uuid().to_string()],
                )
                .unwrap();
        }

        let restored = repository
            .load_recent_bounded(RestoreBudget {
                max_records: 500,
                max_total_bytes: 48 * 1024 * 1024,
                max_record_bytes: 16 * 1024 * 1024,
            })
            .unwrap();

        assert_eq!(restored, vec![valid]);
    }

    #[test]
    fn disk_quota_retains_the_exact_payload_boundary_and_evicts_oldest_after_crossing() {
        let directory = tempdir().unwrap();
        let quota = DiskQuota {
            max_records: 10,
            max_payload_bytes: 70,
            incremental_vacuum_pages: 1,
        };
        let repository =
            SqliteRepository::open_with_quota(directory.path().join("history.sqlite3"), quota)
                .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        repository
            .save_record(&text_record("old", now, "abc"))
            .unwrap();
        repository
            .save_record(&text_record("new", now + Duration::seconds(1), "def"))
            .unwrap();
        assert_eq!(repository.load_recent(10).unwrap().len(), 2);

        repository
            .save_record(&text_record("newest", now + Duration::seconds(2), "g"))
            .unwrap();

        let records = repository.load_recent(10).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].content_identity.as_str(), "newest");
        assert_eq!(records[1].content_identity.as_str(), "new");
    }

    #[test]
    fn forever_and_periodic_maintenance_still_enforce_the_disk_count_quota() {
        let directory = tempdir().unwrap();
        let quota = DiskQuota {
            max_records: 2,
            max_payload_bytes: 1024,
            incremental_vacuum_pages: 1,
        };
        let repository =
            SqliteRepository::open_with_quota(directory.path().join("history.sqlite3"), quota)
                .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        for index in 0..3 {
            let item = text_record(
                &format!("record-{index}"),
                now + Duration::seconds(index),
                "x",
            );
            let mut connection = lock_unpoisoned(&repository.state);
            save_record_transaction(&mut connection, &item).unwrap();
        }

        assert_eq!(repository.prune(RetentionPeriod::Forever, now).unwrap(), 1);
        let records = repository.load_recent(10).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].content_identity.as_str(), "record-2");
        assert_eq!(records[1].content_identity.as_str(), "record-1");
    }

    #[test]
    fn refreshed_record_payload_is_recalculated_before_quota_commit() {
        let directory = tempdir().unwrap();
        let quota = DiskQuota {
            max_records: 10,
            max_payload_bytes: 70,
            incremental_vacuum_pages: 1,
        };
        let repository =
            SqliteRepository::open_with_quota(directory.path().join("history.sqlite3"), quota)
                .unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        let old = text_record("old", now, "abc");
        let mut refreshed = text_record("refresh", now + Duration::seconds(1), "def");
        repository.save_record(&old).unwrap();
        repository.save_record(&refreshed).unwrap();
        refreshed.representations = vec![ClipboardRepresentation::UnicodeText {
            text: "expanded".to_owned(),
        }];

        repository.save_record(&refreshed).unwrap();

        let records = repository.load_recent(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id, refreshed.id);
    }

    #[test]
    fn sqlite_uses_incremental_auto_vacuum() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let mode: i64 = lock_unpoisoned(&repository.state)
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();

        assert_eq!(mode, 2);
    }

    #[test]
    fn schema_five_migrates_recognition_storage_without_losing_search() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let record = text_record("v5-record", Utc::now(), "existing searchable text");
        {
            let repository = SqliteRepository::open(path.clone()).unwrap();
            repository.save_record(&record).unwrap();
            let connection = lock_unpoisoned(&repository.state);
            connection
                .execute("DROP TABLE clipboard_recognition", [])
                .unwrap();
            connection.pragma_update(None, "user_version", 5).unwrap();
        }

        let repository = SqliteRepository::open(path).unwrap();
        let page = repository
            .search_history(crate::services::search::SearchQuery {
                query: "existing searchable".to_owned(),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(repository.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, record.id);
    }

    #[test]
    fn recognition_persists_refreshes_search_and_cascades_on_delete() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let record = text_record("recognized", Utc::now(), "unrelated preview");
        let id = record.id;
        {
            let repository = SqliteRepository::open(path.clone()).unwrap();
            repository.save_record(&record).unwrap();
            repository
                .save_recognition(
                    id,
                    Some("offline invoice number alpha"),
                    Some("https://local.example/qr-beta"),
                    "complete",
                )
                .unwrap();
        }

        let repository = SqliteRepository::open(path.clone()).unwrap();
        let history = repository.load_page(HistoryQuery::default()).unwrap();
        assert_eq!(
            history.records[0].ocr_text.as_deref(),
            Some("offline invoice number alpha")
        );
        assert_eq!(
            history.records[0].qr_text.as_deref(),
            Some("https://local.example/qr-beta")
        );
        for query in ["invoice number alpha", "qr-beta"] {
            let page = repository
                .search_history(crate::services::search::SearchQuery {
                    query: query.to_owned(),
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].id, id);
            assert_eq!(
                page.items[0].ocr_text.as_deref(),
                Some("offline invoice number alpha")
            );
            assert_eq!(
                page.items[0].qr_text.as_deref(),
                Some("https://local.example/qr-beta")
            );
        }
        RecordPersistence::delete_record(repository.as_ref(), id).unwrap();
        drop(repository);

        let connection = Connection::open(path).unwrap();
        let recognition_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM clipboard_recognition", [], |row| {
                row.get(0)
            })
            .unwrap();
        let search_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM clipboard_search", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(recognition_count, 0);
        assert_eq!(search_count, 0);
    }

    #[test]
    fn version_one_database_migrates_to_incremental_auto_vacuum() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE clipboard_records (
                        id TEXT PRIMARY KEY NOT NULL,
                        content_identity TEXT NOT NULL,
                        captured_at TEXT NOT NULL,
                        source_application TEXT,
                        source_path TEXT,
                        note TEXT,
                        group_id TEXT,
                        pinned INTEGER NOT NULL DEFAULT 0,
                        favorite INTEGER NOT NULL DEFAULT 0,
                        sensitive INTEGER NOT NULL DEFAULT 0
                     );
                     CREATE INDEX clipboard_records_captured_at
                        ON clipboard_records(captured_at DESC);
                     CREATE TABLE clipboard_representations (
                        record_id TEXT NOT NULL REFERENCES clipboard_records(id) ON DELETE CASCADE,
                        position INTEGER NOT NULL,
                        kind TEXT NOT NULL,
                        text_value TEXT,
                        blob_value BLOB,
                        PRIMARY KEY(record_id, position)
                     );
                     CREATE TABLE app_settings (
                        key TEXT PRIMARY KEY NOT NULL,
                        value TEXT NOT NULL
                     );
                     PRAGMA user_version = 1;",
                )
                .unwrap();
        }

        let repository = SqliteRepository::open(path).unwrap();
        let connection = lock_unpoisoned(&repository.state);
        let mode: i64 = connection
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();

        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(mode, 2);
    }

    #[test]
    fn version_one_migration_copies_only_records_within_retention_and_capacity() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        create_v1_database(
            &path,
            &[
                ("expired".to_owned(), now - Duration::days(8), 128 * 1024),
                ("old".to_owned(), now - Duration::hours(3), 128 * 1024),
                ("new".to_owned(), now - Duration::hours(2), 128 * 1024),
                ("newest".to_owned(), now - Duration::hours(1), 128 * 1024),
            ],
            RetentionPeriod::SevenDays,
        );
        let quota = DiskQuota {
            max_records: 2,
            max_payload_bytes: 2 * (128 * 1024 + REPRESENTATION_OVERHEAD_BYTES),
            incremental_vacuum_pages: 1,
        };

        migrate_legacy_database(&path, quota, now, &StdMigrationFileOps).unwrap();
        let repository = SqliteRepository::open_with_quota(path, quota).unwrap();
        let records = repository.load_recent(10).unwrap();

        assert_eq!(repository.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].content_identity.as_str(), "newest");
        assert_eq!(records[1].content_identity.as_str(), "new");
        assert_eq!(
            repository.load_settings().unwrap().retention,
            RetentionPeriod::SevenDays
        );
    }

    #[test]
    fn missing_or_invalid_legacy_retention_defaults_to_thirty_days_before_copy() {
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        for retention in [None, Some("invalid-retention")] {
            let directory = tempdir().unwrap();
            let path = directory.path().join("history.sqlite3");
            create_v1_database_with_retention(
                &path,
                &[
                    ("expired".to_owned(), now - Duration::days(31), 1024),
                    ("retained".to_owned(), now - Duration::days(29), 1024),
                ],
                retention,
            );

            migrate_legacy_database(&path, DiskQuota::default(), now, &StdMigrationFileOps)
                .unwrap();
            let repository = SqliteRepository::open(path).unwrap();
            let records = repository.load_recent(10).unwrap();

            assert_eq!(records.len(), 1);
            assert_eq!(records[0].content_identity.as_str(), "retained");
            assert_eq!(
                repository.load_settings().unwrap().retention,
                RetentionPeriod::ThirtyDays
            );
        }
    }

    #[test]
    fn wal_record_committed_before_migration_lock_is_preserved() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        create_v1_database(
            &path,
            &[("original".to_owned(), now, 1024)],
            RetentionPeriod::Forever,
        );
        let writer = Connection::open(&path).unwrap();
        writer.pragma_update(None, "journal_mode", "WAL").unwrap();
        insert_v1_record(&writer, "wal-record", now + Duration::seconds(1), 1024);
        drop(writer);

        let repository = SqliteRepository::open(path).unwrap();
        let identities = repository
            .load_recent(10)
            .unwrap()
            .into_iter()
            .map(|record| record.content_identity.as_str().to_owned())
            .collect::<Vec<_>>();

        assert_eq!(identities, vec!["wal-record", "original"]);
    }

    #[test]
    fn migration_lock_serializes_an_independent_writer_after_selection() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        create_v1_database(
            &path,
            &[("original".to_owned(), now, 1024)],
            RetentionPeriod::Forever,
        );
        let selected = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let hooks = BlockingMigrationHooks {
            selected: Arc::clone(&selected),
            resume: Arc::clone(&resume),
        };
        let migration_path = path.clone();
        let migration = thread::spawn(move || {
            SqliteRepository::open_with_dependencies(
                migration_path,
                DiskQuota::default(),
                &StdMigrationFileOps,
                Arc::new(StdMigrationLockProvider),
                &hooks,
            )
        });
        selected.wait();

        let writer_path = path.clone();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let writer = thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            let _guard = StdMigrationLockProvider
                .acquire(&writer_path, StdDuration::from_secs(5))
                .unwrap();
            let connection = Connection::open(writer_path).unwrap();
            connection.busy_timeout(StdDuration::from_secs(5)).unwrap();
            insert_v1_record(
                &connection,
                "after-migration",
                now + Duration::seconds(1),
                1024,
            );
            finished_tx.send(()).unwrap();
        });
        attempted_rx
            .recv_timeout(StdDuration::from_secs(1))
            .unwrap();
        assert!(
            finished_rx
                .recv_timeout(StdDuration::from_millis(100))
                .is_err()
        );

        resume.wait();
        migration.join().unwrap().unwrap();
        writer.join().unwrap();
        finished_rx.recv_timeout(StdDuration::from_secs(1)).unwrap();
        assert_eq!(database_version(&path).unwrap(), SCHEMA_VERSION);
        assert_eq!(record_count_at(&path), 2);
    }

    #[test]
    fn same_path_openers_serialize_while_different_paths_do_not_block() {
        let first_directory = tempdir().unwrap();
        let second_directory = tempdir().unwrap();
        let first_path = first_directory.path().join("history.sqlite3");
        let second_path = second_directory.path().join("history.sqlite3");
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        create_v1_database(
            &first_path,
            &[("first".to_owned(), now, 1024)],
            RetentionPeriod::Forever,
        );
        create_v1_database(
            &second_path,
            &[("second".to_owned(), now, 1024)],
            RetentionPeriod::Forever,
        );
        assert_ne!(
            migration_lock_identity(&first_path),
            migration_lock_identity(&second_path)
        );
        let selected = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let hooks = BlockingMigrationHooks {
            selected: Arc::clone(&selected),
            resume: Arc::clone(&resume),
        };
        let first_for_thread = first_path.clone();
        let first = thread::spawn(move || {
            SqliteRepository::open_with_dependencies(
                first_for_thread,
                DiskQuota::default(),
                &StdMigrationFileOps,
                Arc::new(StdMigrationLockProvider),
                &hooks,
            )
        });
        selected.wait();

        let same_path = first_path.clone();
        let (same_tx, same_rx) = mpsc::channel();
        let competing = thread::spawn(move || {
            let result = SqliteRepository::open(same_path);
            same_tx.send(result.is_ok()).unwrap();
        });
        assert!(same_rx.recv_timeout(StdDuration::from_millis(100)).is_err());

        let different = SqliteRepository::open(second_path).unwrap();
        assert_eq!(different.schema_version().unwrap(), SCHEMA_VERSION);

        resume.wait();
        assert!(first.join().unwrap().is_ok());
        competing.join().unwrap();
        assert!(same_rx.recv_timeout(StdDuration::from_secs(1)).unwrap());
    }

    #[test]
    fn migration_lock_times_out_then_can_be_reacquired_after_release() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let held = StdMigrationLockProvider
            .acquire(&path, StdDuration::from_secs(1))
            .unwrap();
        let competing_path = path.clone();
        let competing = thread::spawn(move || {
            matches!(
                StdMigrationLockProvider.acquire(&competing_path, StdDuration::from_millis(50)),
                Err(PersistenceError::MigrationLockTimeout)
            )
        });

        assert!(competing.join().unwrap());
        drop(held);
        assert!(
            StdMigrationLockProvider
                .acquire(&path, StdDuration::from_secs(1))
                .is_ok()
        );
    }

    #[cfg(windows)]
    #[test]
    fn abandoned_windows_migration_mutex_is_safely_acquired() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let abandoned_path = path.clone();
        thread::spawn(move || {
            let guard = StdMigrationLockProvider
                .acquire(&abandoned_path, StdDuration::from_secs(1))
                .unwrap();
            std::mem::forget(guard);
        })
        .join()
        .unwrap();

        assert!(
            StdMigrationLockProvider
                .acquire(&path, StdDuration::from_secs(1))
                .is_ok()
        );
    }

    #[test]
    fn version_one_migration_physically_reclaims_space_after_quota_selection() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        let records = (0..8)
            .map(|index| {
                (
                    format!("record-{index}"),
                    now + Duration::seconds(index),
                    192 * 1024,
                )
            })
            .collect::<Vec<_>>();
        create_v1_database(&path, &records, RetentionPeriod::Forever);
        let before = fs::metadata(&path).unwrap().len();
        let quota = DiskQuota {
            max_records: 1,
            max_payload_bytes: 192 * 1024 + REPRESENTATION_OVERHEAD_BYTES,
            incremental_vacuum_pages: 1,
        };

        migrate_legacy_database(&path, quota, now, &StdMigrationFileOps).unwrap();
        let after = fs::metadata(&path).unwrap().len();

        assert_eq!(database_version(&path).unwrap(), SCHEMA_VERSION);
        assert_eq!(record_count_at(&path), 1);
        assert!(after * 3 < before, "before={before}, after={after}");
    }

    #[test]
    fn failed_version_one_install_restores_the_original_and_retries_cleanly() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        create_v1_database(
            &path,
            &[
                ("old".to_owned(), now, 64 * 1024),
                ("new".to_owned(), now + Duration::seconds(1), 64 * 1024),
            ],
            RetentionPeriod::Forever,
        );
        let quota = DiskQuota {
            max_records: 1,
            max_payload_bytes: 64 * 1024 + REPRESENTATION_OVERHEAD_BYTES,
            incremental_vacuum_pages: 1,
        };
        let failing_ops = FailSecondRename {
            renames: AtomicUsize::new(0),
        };

        assert!(migrate_legacy_database(&path, quota, now, &failing_ops).is_err());
        assert_eq!(database_version(&path).unwrap(), 1);
        assert_eq!(record_count_at(&path), 2);
        assert!(database_is_valid(&path));
        assert!(!migration_path(&path, MIGRATION_BACKUP_SUFFIX).exists());

        let repository = SqliteRepository::open_with_quota(path.clone(), quota).unwrap();
        assert_eq!(repository.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(repository.load_recent(10).unwrap().len(), 1);
        assert!(!migration_path(&path, MIGRATION_TEMP_SUFFIX).exists());
        assert!(!migration_path(&path, MIGRATION_BACKUP_SUFFIX).exists());
    }

    #[test]
    fn startup_recovers_a_crash_between_migration_renames() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let backup_path = migration_path(&path, MIGRATION_BACKUP_SUFFIX);
        let temp_path = migration_path(&path, MIGRATION_TEMP_SUFFIX);
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        create_v1_database(
            &path,
            &[
                ("old".to_owned(), now, 64 * 1024),
                ("new".to_owned(), now + Duration::seconds(1), 64 * 1024),
            ],
            RetentionPeriod::Forever,
        );
        let quota = DiskQuota {
            max_records: 1,
            max_payload_bytes: 64 * 1024 + REPRESENTATION_OVERHEAD_BYTES,
            incremental_vacuum_pages: 1,
        };
        build_migrated_database(&path, &temp_path, quota, now, &NoopMigrationHooks).unwrap();
        fs::rename(&path, &backup_path).unwrap();

        let repository = SqliteRepository::open_with_quota(path.clone(), quota).unwrap();

        assert_eq!(repository.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(repository.load_recent(10).unwrap().len(), 1);
        assert!(database_is_valid(&path));
        assert!(!temp_path.exists());
        assert!(!backup_path.exists());
    }

    #[test]
    fn note_refresh_and_settings_survive_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let record = record("same", Utc::now());
        let id = record.id;
        {
            let repository = SqliteRepository::open(path.clone()).unwrap();
            repository
                .save_excluded_applications(&["KeePass.exe".to_owned(), "mstsc.exe".to_owned()])
                .unwrap();
            repository.save_record(&record).unwrap();
            repository
                .update_note(id, Some(&RecordNote::new("updated").unwrap()))
                .unwrap();
            repository
                .save_settings(UserSettings {
                    language: Language::En,
                    retention: RetentionPeriod::Forever,
                    storage_limit: StorageLimit::FiveGb,
                    evict_favorites_when_full: true,
                    start_at_sign_in: true,
                    start_minimized: true,
                    show_tray_icon: true,
                    accent_color: AccentColor::Rose,
                    sound_enabled: false,
                    capture_sound: CaptureSound::Custom,
                    activation_shortcut: Shortcut::default(),
                    group_shortcut_modifiers: ShortcutModifiers::CTRL_ALT,
                    quick_paste_enabled: true,
                    quick_paste_modifiers: ShortcutModifiers {
                        alt: true,
                        shift: true,
                        ..ShortcutModifiers::default()
                    },
                    offline_ocr_enabled: true,
                    qr_recognition_enabled: true,
                })
                .unwrap();
        }

        let repository = SqliteRepository::open(path).unwrap();
        assert_eq!(
            repository.load_excluded_applications().unwrap(),
            vec!["KeePass.exe".to_owned(), "mstsc.exe".to_owned()]
        );
        assert_eq!(
            repository.load_recent(1).unwrap()[0]
                .note
                .as_ref()
                .map(RecordNote::as_str),
            Some("updated")
        );
        assert_eq!(
            repository.load_settings().unwrap(),
            UserSettings {
                language: Language::En,
                retention: RetentionPeriod::Forever,
                storage_limit: StorageLimit::FiveGb,
                evict_favorites_when_full: true,
                start_at_sign_in: true,
                start_minimized: true,
                show_tray_icon: true,
                accent_color: AccentColor::Rose,
                sound_enabled: false,
                capture_sound: CaptureSound::Custom,
                activation_shortcut: Shortcut::default(),
                group_shortcut_modifiers: ShortcutModifiers::CTRL_ALT,
                quick_paste_enabled: true,
                quick_paste_modifiers: ShortcutModifiers {
                    alt: true,
                    shift: true,
                    ..ShortcutModifiers::default()
                },
                offline_ocr_enabled: true,
                qr_recognition_enabled: true,
            }
        );
    }

    #[test]
    fn retention_prunes_expired_records_and_forever_keeps_all() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        repository
            .save_record(&record("old", now - Duration::days(8)))
            .unwrap();
        repository
            .save_record(&record("new", now - Duration::days(2)))
            .unwrap();

        assert_eq!(repository.prune(RetentionPeriod::Forever, now).unwrap(), 0);
        assert_eq!(repository.load_recent(10).unwrap().len(), 2);
        assert_eq!(
            repository.prune(RetentionPeriod::SevenDays, now).unwrap(),
            1
        );
        assert_eq!(repository.load_recent(10).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_database_open_fails_without_exposing_payloads() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        fs::write(&path, b"not a sqlite database SECRET_PAYLOAD").unwrap();

        let error = match SqliteRepository::open(path) {
            Ok(_) => panic!("corrupt database must fail"),
            Err(error) => error,
        };

        assert_eq!(error.to_string(), "local clipboard storage is unavailable");
        assert!(!format!("{error}").contains("SECRET_PAYLOAD"));
    }

    #[test]
    fn worker_preserves_write_order_and_flushes_before_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let repository = SqliteRepository::open(path.clone()).unwrap();
        let available = Arc::new(AtomicBool::new(true));
        let worker = PersistenceWorker::start(repository, Arc::clone(&available)).unwrap();
        let record = record("ordered", Utc::now());
        let id = record.id;

        worker.save_record(&record).unwrap();
        worker
            .update_note(id, Some(&RecordNote::new("latest note").unwrap()))
            .unwrap();
        worker
            .save_settings(UserSettings {
                language: Language::En,
                retention: RetentionPeriod::Forever,
                ..UserSettings::default()
            })
            .unwrap();
        drop(worker);

        let reopened = SqliteRepository::open(path).unwrap();
        assert_eq!(
            reopened.load_recent(1).unwrap()[0]
                .note
                .as_ref()
                .map(RecordNote::as_str),
            Some("latest note")
        );
        assert_eq!(reopened.load_settings().unwrap().language, Language::En);
        assert!(available.load(Ordering::Acquire));
    }

    #[test]
    fn shutdown_flushes_an_accumulated_backlog_before_acknowledging() {
        let backend = Arc::new(TestBackend::new(false, StdDuration::from_millis(2)));
        let available = Arc::new(AtomicBool::new(true));
        let worker = PersistenceWorker::start_backend(
            Arc::clone(&backend) as Arc<dyn PersistenceBackend>,
            Arc::clone(&available),
            StdDuration::from_secs(2),
        )
        .unwrap();
        let sender = lock_unpoisoned(&worker.sender)
            .as_ref()
            .expect("worker sender")
            .clone();
        let mut responses = Vec::new();
        for _ in 0..96 {
            let (reply, response) = mpsc::sync_channel(1);
            sender
                .send(PersistenceCommand::SaveRecord(
                    record("backlog", Utc::now()),
                    reply,
                ))
                .unwrap();
            worker.accepted.fetch_add(1, Ordering::AcqRel);
            responses.push(response);
        }

        worker.stop();

        assert_eq!(backend.writes.load(Ordering::Acquire), 96);
        assert!(
            responses
                .into_iter()
                .all(|response| response.recv().unwrap().is_ok())
        );
        assert!(available.load(Ordering::Acquire));
    }

    #[test]
    fn first_worker_failure_enters_terminal_session_only_mode() {
        let backend = Arc::new(TestBackend::new(false, StdDuration::ZERO));
        backend.fail_first.store(true, Ordering::Release);
        let available = Arc::new(AtomicBool::new(true));
        let worker = PersistenceWorker::start_backend(
            Arc::clone(&backend) as Arc<dyn PersistenceBackend>,
            Arc::clone(&available),
            StdDuration::from_secs(1),
        )
        .unwrap();
        let item = record("failure", Utc::now());

        assert!(worker.save_record(&item).is_err());
        assert!(!available.load(Ordering::Acquire));
        assert!(worker.save_record(&item).is_err());
        assert_eq!(backend.writes.load(Ordering::Acquire), 1);
    }
}
