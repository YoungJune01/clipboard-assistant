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

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::domain::{
    ClipboardRecord, ClipboardRepresentation, ContentIdentity, GroupId, Language, RecordId,
    RecordNote, RetentionPeriod, SourceIdentity, UserSettings,
};

const SCHEMA_VERSION: i64 = 1;
const DATABASE_FILE: &str = "clipboard-history.sqlite3";
const WORK_QUEUE_CAPACITY: usize = 64;
const WORKER_TIMEOUT: StdDuration = StdDuration::from_secs(2);
const CONTROL_POLL_INTERVAL: StdDuration = StdDuration::from_millis(25);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageAvailability {
    Available,
    Unavailable,
}

#[derive(Debug)]
pub enum PersistenceError {
    CreateDirectory(std::io::Error),
    Database(rusqlite::Error),
    InvalidData,
    UnsupportedSchema(i64),
    WorkerUnavailable,
    WorkerStart(std::io::Error),
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateDirectory(_) => {
                formatter.write_str("application data directory is unavailable")
            }
            Self::Database(_) => formatter.write_str("local clipboard storage is unavailable"),
            Self::InvalidData => {
                formatter.write_str("local clipboard storage contains invalid data")
            }
            Self::UnsupportedSchema(_) => {
                formatter.write_str("local clipboard storage schema is unsupported")
            }
            Self::WorkerUnavailable | Self::WorkerStart(_) => {
                formatter.write_str("local clipboard storage is unavailable")
            }
        }
    }
}

impl Error for PersistenceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CreateDirectory(error) => Some(error),
            Self::Database(error) => Some(error),
            Self::WorkerStart(error) => Some(error),
            Self::InvalidData | Self::UnsupportedSchema(_) | Self::WorkerUnavailable => None,
        }
    }
}

impl From<rusqlite::Error> for PersistenceError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

pub trait RecordPersistence: Send + Sync {
    fn save_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError>;
    fn update_note(&self, id: RecordId, note: Option<&RecordNote>) -> Result<(), PersistenceError>;
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
    SaveSettings(UserSettings, mpsc::SyncSender<Result<(), PersistenceError>>),
    Prune(
        RetentionPeriod,
        DateTime<Utc>,
        mpsc::SyncSender<Result<usize, PersistenceError>>,
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
}

trait PersistenceBackend: Send + Sync {
    fn persist_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError>;
    fn persist_note(&self, id: RecordId, note: Option<&RecordNote>)
    -> Result<(), PersistenceError>;
    fn persist_settings(&self, settings: UserSettings) -> Result<(), PersistenceError>;
    fn prune_records(
        &self,
        retention: RetentionPeriod,
        now: DateTime<Utc>,
    ) -> Result<usize, PersistenceError>;
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
        Self::start_backend(repository, storage_available, WORKER_TIMEOUT)
    }

    fn start_backend(
        repository: Arc<dyn PersistenceBackend>,
        storage_available: Arc<AtomicBool>,
        response_timeout: StdDuration,
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
            PersistenceCommand::SaveSettings(settings, reply) => {
                let result = repository.persist_settings(settings);
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
        }
        processed = processed.saturating_add(1);
    }
}

fn reply_unavailable(command: PersistenceCommand) {
    match command {
        PersistenceCommand::SaveRecord(_, reply)
        | PersistenceCommand::UpdateNote(_, _, reply)
        | PersistenceCommand::SaveSettings(_, reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
        PersistenceCommand::Prune(_, _, reply) => {
            let _ = reply.send(Err(PersistenceError::WorkerUnavailable));
        }
    }
}

pub struct SqliteRepository {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl SqliteRepository {
    pub fn open_app_data(directory: &Path) -> Result<Arc<Self>, PersistenceError> {
        fs::create_dir_all(directory).map_err(PersistenceError::CreateDirectory)?;
        Self::open(directory.join(DATABASE_FILE))
    }

    pub fn open(path: PathBuf) -> Result<Arc<Self>, PersistenceError> {
        let connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_secs(2))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&connection)?;
        Ok(Arc::new(Self {
            path,
            connection: Mutex::new(connection),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i64, PersistenceError> {
        lock_unpoisoned(&self.connection)
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub fn load_settings(&self) -> Result<UserSettings, PersistenceError> {
        let connection = lock_unpoisoned(&self.connection);
        let language = setting(&connection, "language")?
            .and_then(|value| parse_language(&value))
            .unwrap_or_default();
        let retention = setting(&connection, "retention")?
            .and_then(|value| parse_retention(&value))
            .unwrap_or_default();
        Ok(UserSettings {
            language,
            retention,
        })
    }

    pub fn save_settings(&self, settings: UserSettings) -> Result<(), PersistenceError> {
        let mut connection = lock_unpoisoned(&self.connection);
        let transaction = connection.transaction()?;
        save_setting(&transaction, "language", language_value(settings.language))?;
        save_setting(
            &transaction,
            "retention",
            retention_value(settings.retention),
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn prune(
        &self,
        retention: RetentionPeriod,
        now: DateTime<Utc>,
    ) -> Result<usize, PersistenceError> {
        let Some(days) = retention.days() else {
            return Ok(0);
        };
        let cutoff = now - Duration::days(days);
        lock_unpoisoned(&self.connection)
            .execute(
                "DELETE FROM clipboard_records WHERE captured_at < ?1",
                [cutoff.to_rfc3339()],
            )
            .map_err(Into::into)
    }

    pub fn load_recent(&self, limit: usize) -> Result<Vec<ClipboardRecord>, PersistenceError> {
        let connection = lock_unpoisoned(&self.connection);
        let mut statement = connection.prepare(
            "SELECT id, content_identity, captured_at, source_application, source_path, note, \
                    group_id, pinned, favorite, sensitive \
             FROM clipboard_records ORDER BY captured_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(DbRecord {
                id: row.get(0)?,
                content_identity: row.get(1)?,
                captured_at: row.get(2)?,
                source_application: row.get(3)?,
                source_path: row.get(4)?,
                note: row.get(5)?,
                group_id: row.get(6)?,
                pinned: row.get(7)?,
                favorite: row.get(8)?,
                sensitive: row.get(9)?,
            })
        })?;
        let mut records = Vec::new();
        for row in rows {
            let row = row?;
            let representations = load_representations(&connection, &row.id)?;
            records.push(row.into_record(representations)?);
        }
        Ok(records)
    }

    fn save_record_inner(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        let mut connection = lock_unpoisoned(&self.connection);
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO clipboard_records (
                id, content_identity, captured_at, source_application, source_path, note,
                group_id, pinned, favorite, sensitive
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                content_identity = excluded.content_identity,
                captured_at = excluded.captured_at,
                source_application = excluded.source_application,
                source_path = excluded.source_path,
                note = excluded.note,
                group_id = excluded.group_id,
                pinned = excluded.pinned,
                favorite = excluded.favorite,
                sensitive = excluded.sensitive",
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
            ],
        )?;
        transaction.execute(
            "DELETE FROM clipboard_representations WHERE record_id = ?1",
            [record.id.as_uuid().to_string()],
        )?;
        for (position, representation) in record.representations.iter().enumerate() {
            let (kind, text_value, blob_value): (&str, Option<&str>, Option<&[u8]>) =
                match representation {
                    ClipboardRepresentation::UnicodeText { text } => {
                        ("unicode_text", Some(text.as_str()), None)
                    }
                    ClipboardRepresentation::Png { bytes } => ("png", None, Some(bytes.as_slice())),
                    ClipboardRepresentation::DibV5 { bytes } => {
                        ("dib_v5", None, Some(bytes.as_slice()))
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
        transaction.commit()?;
        Ok(())
    }
}

impl RecordPersistence for SqliteRepository {
    fn save_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
        self.save_record_inner(record)
    }

    fn update_note(&self, id: RecordId, note: Option<&RecordNote>) -> Result<(), PersistenceError> {
        let changed = lock_unpoisoned(&self.connection).execute(
            "UPDATE clipboard_records SET note = ?1 WHERE id = ?2",
            params![note.map(RecordNote::as_str), id.as_uuid().to_string()],
        )?;
        if changed == 0 {
            return Err(PersistenceError::InvalidData);
        }
        Ok(())
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

    fn persist_settings(&self, settings: UserSettings) -> Result<(), PersistenceError> {
        SqliteRepository::save_settings(self, settings)
    }

    fn prune_records(
        &self,
        retention: RetentionPeriod,
        now: DateTime<Utc>,
    ) -> Result<usize, PersistenceError> {
        SqliteRepository::prune(self, retention, now)
    }
}

fn migrate(connection: &Connection) -> Result<(), PersistenceError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(PersistenceError::UnsupportedSchema(version));
    }
    if version == 0 {
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
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
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

fn save_setting(transaction: &Transaction<'_>, key: &str, value: &str) -> rusqlite::Result<()> {
    transaction.execute(
        "INSERT INTO app_settings(key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
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
            _ => return Err(PersistenceError::InvalidData),
        });
    }
    if representations.is_empty() {
        return Err(PersistenceError::InvalidData);
    }
    Ok(representations)
}

struct DbRecord {
    id: String,
    content_identity: String,
    captured_at: String,
    source_application: Option<String>,
    source_path: Option<String>,
    note: Option<String>,
    group_id: Option<String>,
    pinned: bool,
    favorite: bool,
    sensitive: bool,
}

impl DbRecord {
    fn into_record(
        self,
        representations: Vec<ClipboardRepresentation>,
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

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::sync::atomic::AtomicUsize;
    use tempfile::tempdir;

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

        fn persist_settings(&self, _settings: UserSettings) -> Result<(), PersistenceError> {
            Ok(())
        }

        fn prune_records(
            &self,
            _retention: RetentionPeriod,
            _now: DateTime<Utc>,
        ) -> Result<usize, PersistenceError> {
            Ok(0)
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
    fn note_refresh_and_settings_survive_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let record = record("same", Utc::now());
        let id = record.id;
        {
            let repository = SqliteRepository::open(path.clone()).unwrap();
            repository.save_record(&record).unwrap();
            repository
                .update_note(id, Some(&RecordNote::new("updated").unwrap()))
                .unwrap();
            repository
                .save_settings(UserSettings {
                    language: Language::En,
                    retention: RetentionPeriod::Forever,
                })
                .unwrap();
        }

        let repository = SqliteRepository::open(path).unwrap();
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
