use std::{
    collections::HashSet,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use chrono::{TimeZone, Utc};
use image::{DynamicImage, ImageFormat, RgbaImage};
use tempfile::TempDir;

use crate::{
    domain::{
        CapturedClipboard, ClipboardRecord, ClipboardRepresentation, ContentIdentity, HistoryQuery,
        RetentionPeriod, SourceIdentity,
    },
    platform::windows::monitor::{
        DipSize, MonitorIdentity, MonitorSnapshot, PhysicalPoint, PhysicalRect,
    },
    services::{
        backup,
        panel::{PanelMonitor, PanelService, PanelWindow},
        persistence::{PersistenceWorker, RecordPersistence, SqliteRepository},
        recognition::{RecognitionOptions, RecognitionService},
        search::SearchQuery,
        session_records::{MAX_SESSION_RECORDS, STARTUP_HISTORY_RECORDS, SessionRecordStore},
        sync::{BackupSource, RemoteObject, SyncError, WebDavClient, synchronize},
    },
};

const RECORD_COUNT: usize = 10_000;
const PAGE_SIZE: usize = 100;
const SAMPLE_RUNS: usize = 30;

#[test]
fn ten_thousand_record_workload_stays_bounded_and_responsive() {
    let dataset = PerformanceDataset::new();

    let mut query = HistoryQuery {
        limit: PAGE_SIZE,
        ..HistoryQuery::default()
    };
    let mut seen = HashSet::with_capacity(RECORD_COUNT);
    let mut page_durations = Vec::with_capacity(RECORD_COUNT / PAGE_SIZE);
    for _ in 0..RECORD_COUNT / PAGE_SIZE {
        let started = Instant::now();
        let page = dataset.repository.load_page(query.clone()).unwrap();
        page_durations.push(started.elapsed());
        assert_eq!(page.records.len(), PAGE_SIZE);
        for record in &page.records {
            assert!(
                seen.insert(record.id),
                "history page returned a duplicate ID"
            );
        }
        query.cursor = page.next_cursor;
    }
    assert_eq!(seen.len(), RECORD_COUNT);
    assert!(query.cursor.is_none());

    let first_page = dataset
        .repository
        .load_page(HistoryQuery {
            limit: 50,
            ..HistoryQuery::default()
        })
        .unwrap();
    let store = SessionRecordStore::with_persistence_page(
        first_page,
        Arc::clone(&dataset.repository) as Arc<dyn RecordPersistence>,
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );
    let (records, summaries, bytes) = store.working_set_stats();
    assert!(records <= STARTUP_HISTORY_RECORDS);
    assert!(summaries <= STARTUP_HISTORY_RECORDS);
    assert!(bytes <= crate::services::session_records::DEFAULT_STORE_BYTES);
    assert!(store.list().len() <= MAX_SESSION_RECORDS);

    let page_stats = duration_stats(&page_durations);
    assert!(
        page_stats.p95 <= Duration::from_millis(100),
        "100-record page p95 {:?} exceeded 100 ms",
        page_stats.p95
    );

    let first_page_durations = (0..SAMPLE_RUNS)
        .map(|_| {
            let started = Instant::now();
            let page = dataset
                .repository
                .load_page(HistoryQuery {
                    limit: 50,
                    ..HistoryQuery::default()
                })
                .unwrap();
            assert_eq!(page.records.len(), 50);
            started.elapsed()
        })
        .collect::<Vec<_>>();
    let first_page_stats = duration_stats(&first_page_durations);
    assert!(
        first_page_stats.p95 <= Duration::from_millis(100),
        "50-record first page p95 {:?} exceeded 100 ms",
        first_page_stats.p95
    );

    let search_durations = (0..SAMPLE_RUNS)
        .map(|run| {
            let started = Instant::now();
            let page = dataset
                .repository
                .search_history(SearchQuery {
                    query: format!("account-{:04}", run * 17),
                    limit: 50,
                    ..SearchQuery::default()
                })
                .unwrap();
            assert!(!page.items.is_empty());
            started.elapsed()
        })
        .collect::<Vec<_>>();
    let search_stats = duration_stats(&search_durations);
    assert!(
        search_stats.p95 <= Duration::from_millis(150),
        "text search p95 {:?} exceeded 150 ms",
        search_stats.p95
    );

    let capture_store = SessionRecordStore::default();
    let capture_durations = (0..SAMPLE_RUNS)
        .map(|run| {
            let started = Instant::now();
            assert!(capture_store.capture(text_capture(RECORD_COUNT + run)));
            started.elapsed()
        })
        .collect::<Vec<_>>();
    let capture_stats = duration_stats(&capture_durations);
    assert!(
        capture_stats.p95 <= Duration::from_millis(50),
        "capture acknowledgement p95 {:?} exceeded 50 ms",
        capture_stats.p95
    );

    eprintln!(
        "performance: pages p50={:?} p95={:?}; first-page p50={:?} p95={:?}; search p50={:?} p95={:?}; capture p50={:?} p95={:?}",
        page_stats.p50,
        page_stats.p95,
        first_page_stats.p50,
        first_page_stats.p95,
        search_stats.p50,
        search_stats.p95,
        capture_stats.p50,
        capture_stats.p95
    );
}

#[test]
fn quick_panel_show_path_stays_within_budget() {
    let service = PanelService::new(
        PerformanceMonitor,
        PerformanceWindow::default(),
        DipSize {
            width: 420,
            height: 560,
        },
    );
    let durations = (0..SAMPLE_RUNS)
        .map(|_| {
            let started = Instant::now();
            service.show().unwrap();
            let elapsed = started.elapsed();
            service.hide().unwrap();
            elapsed
        })
        .collect::<Vec<_>>();
    let stats = duration_stats(&durations);
    assert!(
        stats.p95 <= Duration::from_millis(300),
        "quick-panel controller show p95 {:?} exceeded 300 ms",
        stats.p95
    );
    eprintln!(
        "performance: quick-panel controller show p50={:?} p95={:?}",
        stats.p50, stats.p95
    );
}

#[test]
fn recognition_queue_saturation_remains_bounded_and_drains() {
    let directory = TempDir::new().unwrap();
    let repository = SqliteRepository::open(directory.path().join("recognition.sqlite3")).unwrap();
    let available = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let persistence = PersistenceWorker::start(repository.clone(), available).unwrap();
    let service = RecognitionService::start(persistence, Arc::new(|| {})).unwrap();
    service.set_options(RecognitionOptions {
        ocr: false,
        qr: true,
    });
    let png = queue_pressure_png();
    let mut accepted = 0;
    let mut rejected = 0;
    let mut peak_pending = 0;
    for index in 0..64 {
        let record = image_record(index, png.clone());
        if service.enqueue(&record) {
            accepted += 1;
        } else {
            rejected += 1;
        }
        peak_pending = peak_pending.max(service.pending_jobs());
        assert!(peak_pending <= 9);
    }
    assert!(accepted > 0);
    assert!(
        rejected > 0,
        "the bounded recognition queue should saturate"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while service.pending_jobs() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(service.pending_jobs(), 0);
}

#[test]
fn repeated_sync_failures_leave_no_staging_backlog() {
    let directory = TempDir::new().unwrap();
    let source = valid_backup(&directory);
    let client = FailingClient::default();
    let backup = CopyBackup { source };
    let mut config = crate::domain::WebDavConfig {
        enabled: true,
        endpoint: "https://dav.example.test/".to_owned(),
        ..crate::domain::WebDavConfig::default()
    };
    for minute in 0..96 {
        let now = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).unwrap()
            + chrono::Duration::minutes(minute * 15);
        assert!(matches!(
            synchronize(&mut config, &client, &backup, directory.path(), now),
            Err(SyncError::Network)
        ));
        let leftovers = fs::read_dir(directory.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("sync-"))
            .count();
        assert_eq!(leftovers, 0);
    }
    assert_eq!(*client.gets.lock().unwrap(), 96);
}

#[test]
fn twenty_four_hour_equivalent_maintenance_keeps_history_unique() {
    let dataset = PerformanceDataset::new();
    let start = Utc.with_ymd_and_hms(2026, 8, 29, 0, 0, 0).unwrap();
    for hour in 0..24 {
        dataset
            .repository
            .prune(
                RetentionPeriod::Forever,
                start + chrono::Duration::hours(hour),
            )
            .unwrap();
        let page = dataset
            .repository
            .load_page(HistoryQuery {
                limit: PAGE_SIZE,
                ..HistoryQuery::default()
            })
            .unwrap();
        let unique = page
            .records
            .iter()
            .map(|record| record.id)
            .collect::<HashSet<_>>();
        assert_eq!(unique.len(), page.records.len());
    }
}

struct PerformanceDataset {
    _directory: TempDir,
    repository: Arc<SqliteRepository>,
}

impl PerformanceDataset {
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("performance.sqlite3");
        fs::copy(performance_database_template(), &path).unwrap();
        let repository = SqliteRepository::open(path).unwrap();
        Self {
            _directory: directory,
            repository,
        }
    }
}

fn performance_database_template() -> &'static Path {
    static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
    TEMPLATE
        .get_or_init(|| {
            let directory = std::env::temp_dir().join(format!(
                "clipboard-assistant-performance-{}",
                std::process::id()
            ));
            fs::create_dir_all(&directory).unwrap();
            let path = directory.join("template.sqlite3");
            let repository = SqliteRepository::open(path.clone()).unwrap();
            let base = Utc.with_ymd_and_hms(2026, 8, 29, 12, 0, 0).unwrap();
            for batch_start in (0..RECORD_COUNT).step_by(500) {
                let records = (batch_start..batch_start + 500)
                    .map(|index| mixed_record(index, base))
                    .collect::<Vec<_>>();
                repository.seed_records_for_performance(&records).unwrap();
            }
            drop(repository);
            path
        })
        .as_path()
}

fn mixed_record(index: usize, base: chrono::DateTime<Utc>) -> ClipboardRecord {
    let mut record = ClipboardRecord::from_capture(CapturedClipboard {
        content_identity: ContentIdentity::new(format!("performance-{index}")),
        captured_at: base - chrono::Duration::seconds(index as i64),
        source: SourceIdentity {
            application_name: Some(format!("Application-{}", index % 12)),
            executable_path: None,
        },
        representations: match index % 4 {
            0 => vec![ClipboardRepresentation::UnicodeText {
                text: format!("account-{index:04} clipboard performance text"),
            }],
            1 => vec![
                ClipboardRepresentation::UnicodeText {
                    text: format!("account-{index:04} rich text"),
                },
                ClipboardRepresentation::Rtf {
                    bytes: br"{\rtf1 performance}".to_vec(),
                },
                ClipboardRepresentation::Html {
                    bytes: b"<p>performance</p>".to_vec(),
                },
            ],
            2 => vec![
                ClipboardRepresentation::UnicodeText {
                    text: format!("account-{index:04} image"),
                },
                ClipboardRepresentation::Png { bytes: tiny_png() },
            ],
            _ => vec![ClipboardRepresentation::FileList {
                paths: vec![format!(r"C:\Performance\account-{index:04}.txt")],
            }],
        },
    });
    record.favorite = index.is_multiple_of(19);
    record.pinned = index.is_multiple_of(101);
    record
}

fn text_capture(index: usize) -> CapturedClipboard {
    CapturedClipboard {
        content_identity: ContentIdentity::new(format!("capture-{index}")),
        captured_at: Utc::now(),
        source: SourceIdentity::default(),
        representations: vec![ClipboardRepresentation::UnicodeText {
            text: format!("capture acknowledgement {index}"),
        }],
    }
}

fn image_record(index: usize, png: Vec<u8>) -> ClipboardRecord {
    ClipboardRecord::from_capture(CapturedClipboard {
        content_identity: ContentIdentity::new(format!("recognition-{index}")),
        captured_at: Utc::now(),
        source: SourceIdentity::default(),
        representations: vec![ClipboardRepresentation::Png { bytes: png }],
    })
}

fn tiny_png() -> Vec<u8> {
    encode_png(RgbaImage::from_pixel(2, 2, image::Rgba([20, 80, 140, 255])))
}

fn queue_pressure_png() -> Vec<u8> {
    encode_png(RgbaImage::from_fn(768, 768, |x, y| {
        image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8, 255])
    }))
}

fn encode_png(image: RgbaImage) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut output, ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

#[derive(Default)]
struct FailingClient {
    gets: Mutex<usize>,
}

impl WebDavClient for FailingClient {
    fn get(&self, _path: &str, _max_bytes: u64) -> Result<Option<RemoteObject>, SyncError> {
        *self.gets.lock().unwrap() += 1;
        Err(SyncError::Network)
    }

    fn put(&self, _path: &str, _bytes: &[u8]) -> Result<Option<String>, SyncError> {
        panic!("network failure must stop before upload")
    }

    fn move_object(&self, _source: &str, _destination: &str) -> Result<(), SyncError> {
        panic!("network failure must stop before move")
    }
}

struct CopyBackup {
    source: PathBuf,
}

impl BackupSource for CopyBackup {
    fn create_backup(&self, destination: &Path) -> Result<(), SyncError> {
        fs::copy(&self.source, destination)?;
        Ok(())
    }
}

fn valid_backup(directory: &TempDir) -> PathBuf {
    let database = directory.path().join("source.sqlite3");
    rusqlite::Connection::open(&database).unwrap();
    let archive = directory.path().join("source.clipbackup");
    backup::create_archive(&database, None, &archive, "performance").unwrap();
    archive
}

struct DurationStats {
    p50: Duration,
    p95: Duration,
}

fn duration_stats(values: &[Duration]) -> DurationStats {
    assert!(!values.is_empty());
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    DurationStats {
        p50: sorted[(sorted.len() - 1) * 50 / 100],
        p95: sorted[(sorted.len() - 1) * 95 / 100],
    }
}

#[derive(Clone, Copy)]
struct PerformanceMonitor;

impl PanelMonitor for PerformanceMonitor {
    type Error = PerformanceError;

    fn snapshot(&self) -> Result<MonitorSnapshot, Self::Error> {
        Ok(performance_snapshot())
    }

    fn snapshot_for_owner(
        &self,
        _identity: &MonitorIdentity,
        _anchor: PhysicalPoint,
    ) -> Result<MonitorSnapshot, Self::Error> {
        Ok(performance_snapshot())
    }
}

#[derive(Clone, Default)]
struct PerformanceWindow(Arc<Mutex<bool>>);

impl PanelWindow for PerformanceWindow {
    type Error = PerformanceError;

    fn set_bounds(&self, _bounds: PhysicalRect) -> Result<(), Self::Error> {
        Ok(())
    }

    fn show(&self) -> Result<(), Self::Error> {
        *self.0.lock().unwrap() = true;
        Ok(())
    }

    fn focus(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn hide(&self) -> Result<(), Self::Error> {
        *self.0.lock().unwrap() = false;
        Ok(())
    }

    fn is_visible(&self) -> Result<bool, Self::Error> {
        Ok(*self.0.lock().unwrap())
    }
}

fn performance_snapshot() -> MonitorSnapshot {
    MonitorSnapshot {
        identity: MonitorIdentity::from_static("performance-monitor"),
        pointer: PhysicalPoint { x: 800, y: 450 },
        work_area: PhysicalRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        },
        dpi: 144,
    }
}

#[derive(Debug)]
struct PerformanceError;

impl std::fmt::Display for PerformanceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("performance fixture error")
    }
}

impl std::error::Error for PerformanceError {}
