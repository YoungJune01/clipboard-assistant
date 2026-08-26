use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::Serialize;

use crate::domain::{
    CapturedClipboard, ClipboardRecord, ClipboardRepresentation, GroupId, HistoryCursor,
    HistoryQuery, RecordId, RecordNote, RecordNoteError,
};
use crate::services::persistence::{HistoryPage, HistoryRecordSummary, RecordPersistence};

pub(crate) const MAX_SESSION_RECORDS: usize = 500;
pub(crate) const STARTUP_HISTORY_RECORDS: usize = 100;
pub(crate) const TOTAL_SESSION_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const INGESTION_QUEUE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const INGESTION_QUEUE_EVENTS: usize = 64;
pub(crate) const DEFAULT_STORE_BYTES: usize = TOTAL_SESSION_PAYLOAD_BYTES - INGESTION_QUEUE_BYTES;
pub(crate) const MAX_CAPTURE_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const REPRESENTATION_OVERHEAD_BYTES: usize = 32;
const DEFAULT_RECORD_BYTES: usize = MAX_CAPTURE_RECORD_BYTES;
const DEFAULT_PREVIEW_BYTES: usize = 4 * 1024;
const IMAGE_PREVIEW_MAX_DIMENSION: u32 = 8192;
const IMAGE_PREVIEW_MAX_PIXELS: u64 = 24_000_000;
const IMAGE_PREVIEW_MAX_BYTES: usize = 2 * 1024 * 1024;
const BITMAP_FILE_HEADER_BYTES: usize = 14;
const BITMAP_V5_HEADER_BYTES: usize = 124;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecordView {
    pub id: RecordId,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub source_application: Option<String>,
    pub text: Option<String>,
    pub has_image: bool,
    pub note: Option<RecordNote>,
    pub group_id: Option<GroupId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePreviewView {
    pub data_url: String,
    pub width: u32,
    pub height: u32,
}

pub struct SessionRecordStore {
    state: Mutex<SessionRecordState>,
    last_deleted: Mutex<Option<ClipboardRecord>>,
    limits: SessionRecordLimits,
    persistence: NotePersistence,
    storage_available: Arc<AtomicBool>,
    mutation_refresh: Mutex<()>,
}

enum NotePersistence {
    NotConfigured,
    Durable(Arc<dyn RecordPersistence>),
    SessionOnly,
}

#[derive(Clone, Copy)]
struct SessionRecordLimits {
    total_bytes: usize,
    record_bytes: usize,
    preview_bytes: usize,
}

struct SessionRecordState {
    records: VecDeque<Arc<ClipboardRecord>>,
    page: VecDeque<SessionRecordView>,
    total_bytes: usize,
    paged: bool,
    base_query: HistoryQuery,
    next_cursor: Option<HistoryCursor>,
    loaded_target: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureStatus {
    Inserted { bytes: usize },
    Refreshed { previous: usize, bytes: usize },
    RejectedTooLarge,
}

impl Default for SessionRecordStore {
    fn default() -> Self {
        Self::with_limits(SessionRecordLimits {
            total_bytes: DEFAULT_STORE_BYTES,
            record_bytes: DEFAULT_RECORD_BYTES,
            preview_bytes: DEFAULT_PREVIEW_BYTES,
        })
    }
}

impl SessionRecordStore {
    fn with_limits(limits: SessionRecordLimits) -> Self {
        Self {
            state: Mutex::new(SessionRecordState {
                records: VecDeque::new(),
                page: VecDeque::new(),
                total_bytes: 0,
                paged: false,
                base_query: HistoryQuery::default(),
                next_cursor: None,
                loaded_target: 0,
            }),
            last_deleted: Mutex::new(None),
            limits,
            persistence: NotePersistence::NotConfigured,
            storage_available: Arc::new(AtomicBool::new(false)),
            mutation_refresh: Mutex::new(()),
        }
    }

    pub fn with_persistence(
        records: Vec<ClipboardRecord>,
        persistence: Arc<dyn RecordPersistence>,
        storage_available: Arc<AtomicBool>,
    ) -> Self {
        let store = Self {
            state: Mutex::new(SessionRecordState {
                records: VecDeque::new(),
                page: VecDeque::new(),
                total_bytes: 0,
                paged: false,
                base_query: HistoryQuery::default(),
                next_cursor: None,
                loaded_target: 0,
            }),
            last_deleted: Mutex::new(None),
            limits: SessionRecordLimits {
                total_bytes: DEFAULT_STORE_BYTES,
                record_bytes: DEFAULT_RECORD_BYTES,
                preview_bytes: DEFAULT_PREVIEW_BYTES,
            },
            persistence: NotePersistence::Durable(persistence),
            storage_available,
            mutation_refresh: Mutex::new(()),
        };
        store.replace_loaded(records);
        store
    }

    pub fn with_persistence_page(
        page: HistoryPage,
        persistence: Arc<dyn RecordPersistence>,
        storage_available: Arc<AtomicBool>,
    ) -> Self {
        Self::with_persistence_query_page(
            HistoryQuery {
                limit: STARTUP_HISTORY_RECORDS,
                ..HistoryQuery::default()
            },
            page,
            persistence,
            storage_available,
        )
    }

    pub fn with_persistence_query_page(
        query: HistoryQuery,
        page: HistoryPage,
        persistence: Arc<dyn RecordPersistence>,
        storage_available: Arc<AtomicBool>,
    ) -> Self {
        let store = Self {
            state: Mutex::new(SessionRecordState {
                records: VecDeque::new(),
                page: VecDeque::new(),
                total_bytes: 0,
                paged: true,
                base_query: base_history_query(&query),
                next_cursor: None,
                loaded_target: 0,
            }),
            last_deleted: Mutex::new(None),
            limits: SessionRecordLimits {
                total_bytes: DEFAULT_STORE_BYTES,
                record_bytes: DEFAULT_RECORD_BYTES,
                preview_bytes: DEFAULT_PREVIEW_BYTES,
            },
            persistence: NotePersistence::Durable(persistence),
            storage_available,
            mutation_refresh: Mutex::new(()),
        };
        store.replace_page_for_query(query, page);
        store
    }

    pub fn with_loaded(records: Vec<ClipboardRecord>) -> Self {
        let store = Self::default();
        store.replace_loaded(records);
        store
    }

    pub fn with_session_only(
        records: Vec<ClipboardRecord>,
        storage_available: Arc<AtomicBool>,
    ) -> Self {
        let store = Self {
            state: Mutex::new(SessionRecordState {
                records: VecDeque::new(),
                page: VecDeque::new(),
                total_bytes: 0,
                paged: false,
                base_query: HistoryQuery::default(),
                next_cursor: None,
                loaded_target: 0,
            }),
            last_deleted: Mutex::new(None),
            limits: SessionRecordLimits {
                total_bytes: DEFAULT_STORE_BYTES,
                record_bytes: DEFAULT_RECORD_BYTES,
                preview_bytes: DEFAULT_PREVIEW_BYTES,
            },
            persistence: NotePersistence::SessionOnly,
            storage_available,
            mutation_refresh: Mutex::new(()),
        };
        store.replace_loaded(records);
        store
    }

    #[cfg(test)]
    pub(crate) fn with_test_limits(
        total_bytes: usize,
        record_bytes: usize,
        preview_bytes: usize,
    ) -> Self {
        Self::with_limits(SessionRecordLimits {
            total_bytes,
            record_bytes,
            preview_bytes,
        })
    }

    pub fn capture(&self, capture: CapturedClipboard) -> bool {
        !matches!(self.capture_one(capture), CaptureStatus::RejectedTooLarge)
    }

    pub(crate) fn capture_one(&self, mut capture: CapturedClipboard) -> CaptureStatus {
        retain_preferred_image(&mut capture.representations);
        let content_identity = capture.content_identity.clone();
        let Some(capture_bytes) = checked_representation_bytes(&capture.representations) else {
            return CaptureStatus::RejectedTooLarge;
        };
        if capture_bytes > self.limits.record_bytes {
            return CaptureStatus::RejectedTooLarge;
        }
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        self.load_page_front_for_capture();
        let mut state = lock_unpoisoned(&self.state);
        if let Some(current) = state.records.front()
            && current.content_identity == capture.content_identity
        {
            let previous_bytes = record_bytes(current);
            let mut refreshed = current.as_ref().clone();
            let _ = refreshed.refresh_from(capture);
            retain_preferred_image(&mut refreshed.representations);
            let Some(refreshed_bytes) = checked_record_bytes(&refreshed) else {
                return CaptureStatus::RejectedTooLarge;
            };
            if refreshed_bytes > self.limits.record_bytes {
                return CaptureStatus::RejectedTooLarge;
            }
            state.total_bytes = state
                .total_bytes
                .saturating_sub(previous_bytes)
                .saturating_add(refreshed_bytes);
            state.records[0] = Arc::new(refreshed);
            sort_records(&mut state.records);
            evict_to_limits(&mut state, self.limits);
            let persisted = state
                .records
                .iter()
                .find(|record| record.content_identity == content_identity)
                .cloned();
            let status = CaptureStatus::Refreshed {
                previous: previous_bytes,
                bytes: refreshed_bytes,
            };
            drop(state);
            if let Some(record) = persisted {
                self.persist_record(&record);
            }
            return status;
        }
        let record = ClipboardRecord::from_capture(capture);
        state.total_bytes = state.total_bytes.saturating_add(capture_bytes);
        state.records.push_front(Arc::new(record));
        if state.paged
            && let Some(record) = state.records.front()
        {
            let view = SessionRecordView::from_record(record, self.limits.preview_bytes);
            state.page.retain(|existing| existing.id != view.id);
            state.page.push_front(view);
        }
        evict_to_limits(&mut state, self.limits);
        let persisted = state.records.front().cloned();
        let status = CaptureStatus::Inserted {
            bytes: capture_bytes,
        };
        drop(state);
        if let Some(record) = persisted {
            self.persist_record(&record);
        }
        status
    }

    pub fn list(&self) -> Vec<SessionRecordView> {
        let state = lock_unpoisoned(&self.state);
        if !state.page.is_empty() {
            return state.page.iter().cloned().collect();
        }
        let records: Vec<_> = state.records.iter().cloned().collect();
        drop(state);
        records
            .iter()
            .map(|record| SessionRecordView::from_record(record, self.limits.preview_bytes))
            .collect()
    }

    pub fn representations(&self, id: RecordId) -> Option<Vec<ClipboardRepresentation>> {
        self.record_details(id)
            .ok()
            .map(|record| record.representations)
    }

    pub fn record_details(&self, id: RecordId) -> Result<ClipboardRecord, SessionRecordError> {
        let record = lock_unpoisoned(&self.state)
            .records
            .iter()
            .find(|record| record.id == id)
            .cloned();
        if let Some(record) = record {
            return Ok(record.as_ref().clone());
        }
        match &self.persistence {
            NotePersistence::Durable(persistence) => persistence
                .record_details(id)
                .map_err(|_| SessionRecordError::NotFound),
            NotePersistence::NotConfigured | NotePersistence::SessionOnly => {
                Err(SessionRecordError::NotFound)
            }
        }
    }

    pub fn replace_page(&self, page: HistoryPage) {
        let query = {
            let state = lock_unpoisoned(&self.state);
            HistoryQuery {
                limit: page.records.len(),
                ..state.base_query.clone()
            }
        };
        self.replace_page_for_query(query, page);
    }

    pub fn replace_page_for_query(&self, query: HistoryQuery, page: HistoryPage) {
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        let mut state = lock_unpoisoned(&self.state);
        state.paged = true;
        state.base_query = base_history_query(&query);
        state.next_cursor = page.next_cursor.clone();
        state.loaded_target = page.records.len().min(MAX_SESSION_RECORDS);
        state.page = page
            .records
            .into_iter()
            .map(SessionRecordView::from_summary)
            .take(MAX_SESSION_RECORDS)
            .collect();
    }

    pub fn append_page(&self, page: HistoryPage) {
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        let mut state = lock_unpoisoned(&self.state);
        state.paged = true;
        state.next_cursor = page.next_cursor.clone();
        for summary in page.records {
            if state.page.len() >= MAX_SESSION_RECORDS {
                break;
            }
            if state.page.iter().any(|existing| existing.id == summary.id) {
                continue;
            }
            state
                .page
                .push_back(SessionRecordView::from_summary(summary));
        }
        state.loaded_target = state.page.len();
    }

    pub fn next_page_query(&self) -> Option<HistoryQuery> {
        let state = lock_unpoisoned(&self.state);
        state.next_cursor.clone().map(|cursor| HistoryQuery {
            cursor: Some(cursor),
            limit: STARTUP_HISTORY_RECORDS,
            ..state.base_query.clone()
        })
    }

    pub fn image_preview(&self, id: RecordId) -> Result<ImagePreviewView, SessionRecordError> {
        let representations = self
            .representations(id)
            .ok_or(SessionRecordError::NotFound)?;
        representations
            .iter()
            .find_map(|representation| match representation {
                ClipboardRepresentation::Png { bytes } => png_preview(bytes),
                ClipboardRepresentation::DibV5 { bytes } => dib_preview(bytes),
                ClipboardRepresentation::UnicodeText { .. }
                | ClipboardRepresentation::Rtf { .. }
                | ClipboardRepresentation::Html { .. }
                | ClipboardRepresentation::FileList { .. } => None,
            })
            .ok_or(SessionRecordError::ImagePreviewUnavailable)
    }

    pub fn update_note(
        &self,
        id: RecordId,
        value: String,
    ) -> Result<SessionRecordView, SessionRecordError> {
        let note = if value.is_empty() {
            None
        } else {
            Some(RecordNote::new(value).map_err(SessionRecordError::InvalidNote)?)
        };
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        self.load_record_for_mutation(id)?;
        let mut state = lock_unpoisoned(&self.state);
        let index = state
            .records
            .iter()
            .position(|record| record.id == id)
            .ok_or(SessionRecordError::NotFound)?;
        let previous_bytes = checked_record_bytes(&state.records[index])
            .ok_or(SessionRecordError::RecordTooLarge)?;
        let updated_bytes = checked_representation_bytes(&state.records[index].representations)
            .and_then(|bytes| {
                bytes.checked_add(note.as_ref().map_or(0, |note| note.as_str().len()))
            })
            .ok_or(SessionRecordError::RecordTooLarge)?;
        if updated_bytes > self.limits.record_bytes {
            return Err(SessionRecordError::RecordTooLarge);
        }
        let updated_total_bytes = state
            .total_bytes
            .checked_sub(previous_bytes)
            .and_then(|bytes| bytes.checked_add(updated_bytes))
            .ok_or(SessionRecordError::RecordTooLarge)?;
        let record = Arc::make_mut(
            state
                .records
                .get_mut(index)
                .ok_or(SessionRecordError::NotFound)?,
        );
        record.note = note;
        let page_note = record.note.clone();
        if let Some(view) = state.page.iter_mut().find(|view| view.id == id) {
            view.note = page_note;
        }
        state.total_bytes = updated_total_bytes;
        evict_to_limits(&mut state, self.limits);
        let view = state
            .records
            .iter()
            .find(|record| record.id == id)
            .map(|record| SessionRecordView::from_record(record, self.limits.preview_bytes))
            .ok_or(SessionRecordError::NotFound)?;
        let persisted_note = state
            .records
            .iter()
            .find(|record| record.id == id)
            .and_then(|record| record.note.clone());
        drop(state);
        match &self.persistence {
            NotePersistence::NotConfigured => {}
            NotePersistence::SessionOnly => {
                return Err(SessionRecordError::PersistenceUnavailable);
            }
            NotePersistence::Durable(persistence) => {
                if !self.storage_available.load(Ordering::Acquire) {
                    return Err(SessionRecordError::PersistenceUnavailable);
                }
                if persistence
                    .update_note(id, persisted_note.as_ref())
                    .is_err()
                {
                    self.storage_available.store(false, Ordering::Release);
                    return Err(SessionRecordError::PersistenceUnavailable);
                }
                self.refresh_loaded_window()?;
            }
        }
        Ok(view)
    }

    pub fn update_group(
        &self,
        id: RecordId,
        group_id: Option<GroupId>,
    ) -> Result<SessionRecordView, SessionRecordError> {
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        self.load_record_for_mutation(id)?;
        let mut state = lock_unpoisoned(&self.state);
        let record = state
            .records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(SessionRecordError::NotFound)?;
        Arc::make_mut(record).group_id = group_id;
        let updated = Arc::clone(record);
        if let Some(view) = state.page.iter_mut().find(|view| view.id == id) {
            view.group_id = group_id;
        }
        let view = SessionRecordView::from_record(&updated, self.limits.preview_bytes);
        drop(state);
        match &self.persistence {
            NotePersistence::NotConfigured => {}
            NotePersistence::SessionOnly => return Err(SessionRecordError::PersistenceUnavailable),
            NotePersistence::Durable(persistence) => {
                if !self.storage_available.load(Ordering::Acquire)
                    || persistence.save_record(&updated).is_err()
                {
                    self.storage_available.store(false, Ordering::Release);
                    return Err(SessionRecordError::PersistenceUnavailable);
                }
                self.refresh_loaded_window()?;
            }
        }
        Ok(view)
    }

    pub fn clear_group(&self, group_id: GroupId) -> usize {
        let mut state = lock_unpoisoned(&self.state);
        let mut changed = 0;
        for record in &mut state.records {
            if record.group_id == Some(group_id) {
                Arc::make_mut(record).group_id = None;
                changed += 1;
            }
        }
        for view in &mut state.page {
            if view.group_id == Some(group_id) {
                view.group_id = None;
            }
        }
        changed
    }

    pub fn delete(&self, id: RecordId) -> Result<ClipboardRecord, SessionRecordError> {
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        self.require_durable_storage()?;
        self.load_record_for_mutation(id)?;
        let mut state = lock_unpoisoned(&self.state);
        let index = state
            .records
            .iter()
            .position(|record| record.id == id)
            .ok_or(SessionRecordError::NotFound)?;
        let removed = state
            .records
            .remove(index)
            .ok_or(SessionRecordError::NotFound)?;
        state.total_bytes = state.total_bytes.saturating_sub(record_bytes(&removed));
        drop(state);
        if let NotePersistence::Durable(persistence) = &self.persistence
            && persistence.delete_record(id).is_err()
        {
            self.restore_in_memory(removed.as_ref().clone());
            self.storage_available.store(false, Ordering::Release);
            return Err(SessionRecordError::PersistenceUnavailable);
        }
        lock_unpoisoned(&self.state)
            .page
            .retain(|view| view.id != id);
        self.refresh_loaded_window()?;
        let removed = removed.as_ref().clone();
        *lock_unpoisoned(&self.last_deleted) = Some(removed.clone());
        Ok(removed)
    }

    pub fn restore_last_deleted(
        &self,
        id: RecordId,
    ) -> Result<SessionRecordView, SessionRecordError> {
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        self.require_durable_storage()?;
        let mut deleted = lock_unpoisoned(&self.last_deleted);
        if deleted.as_ref().is_none_or(|record| record.id != id) {
            return Err(SessionRecordError::NotFound);
        }
        let record = deleted.take().ok_or(SessionRecordError::NotFound)?;
        drop(deleted);
        let view = SessionRecordView::from_record(&record, self.limits.preview_bytes);
        if let NotePersistence::Durable(persistence) = &self.persistence
            && persistence.save_record(&record).is_err()
        {
            *lock_unpoisoned(&self.last_deleted) = Some(record);
            self.storage_available.store(false, Ordering::Release);
            return Err(SessionRecordError::PersistenceUnavailable);
        }
        self.restore_in_memory(record);
        self.refresh_loaded_window()?;
        Ok(view)
    }

    pub fn clear(&self) -> Result<usize, SessionRecordError> {
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        self.require_durable_storage()?;
        let removed = match &self.persistence {
            NotePersistence::Durable(persistence) => persistence.clear_records().map_err(|_| {
                self.storage_available.store(false, Ordering::Release);
                SessionRecordError::PersistenceUnavailable
            })?,
            NotePersistence::NotConfigured | NotePersistence::SessionOnly => {
                return Err(SessionRecordError::PersistenceUnavailable);
            }
        };
        let mut state = lock_unpoisoned(&self.state);
        state.records.clear();
        state.page.clear();
        state.total_bytes = 0;
        state.next_cursor = None;
        state.loaded_target = 0;
        *lock_unpoisoned(&self.last_deleted) = None;
        Ok(removed)
    }

    pub fn prune_before(&self, cutoff: chrono::DateTime<chrono::Utc>) -> usize {
        let mut state = lock_unpoisoned(&self.state);
        let before = state.records.len();
        state.records.retain(|record| record.captured_at >= cutoff);
        state.page.retain(|record| record.captured_at >= cutoff);
        state.total_bytes = state
            .records
            .iter()
            .map(|record| record_bytes(record))
            .sum();
        before.saturating_sub(state.records.len())
    }

    pub fn storage_available_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.storage_available)
    }

    pub(crate) fn replace_all(&self, records: Vec<ClipboardRecord>) {
        let mut state = lock_unpoisoned(&self.state);
        state.records.clear();
        state.page.clear();
        state.total_bytes = 0;
        state.paged = false;
        state.base_query = HistoryQuery::default();
        state.next_cursor = None;
        state.loaded_target = 0;
        *lock_unpoisoned(&self.last_deleted) = None;
        for mut record in records {
            retain_preferred_image(&mut record.representations);
            let Some(bytes) = checked_record_bytes(&record) else {
                continue;
            };
            if bytes > self.limits.record_bytes {
                continue;
            }
            state.total_bytes = state.total_bytes.saturating_add(bytes);
            state.records.push_back(Arc::new(record));
            evict_to_limits(&mut state, self.limits);
        }
    }

    fn replace_loaded(&self, records: Vec<ClipboardRecord>) {
        self.replace_all(records);
    }

    #[cfg(test)]
    pub(crate) fn reload_first_page(&self) -> Result<(), SessionRecordError> {
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        let query = HistoryQuery {
            limit: STARTUP_HISTORY_RECORDS,
            ..HistoryQuery::default()
        };
        let page = self.load_page(query.clone())?;
        self.apply_page_state(query, page);
        Ok(())
    }

    pub(crate) fn apply_restored_page(&self, query: HistoryQuery, page: HistoryPage) {
        let _serial = lock_unpoisoned(&self.mutation_refresh);
        self.apply_page_state(query, page);
    }

    fn apply_page_state(&self, query: HistoryQuery, page: HistoryPage) {
        let mut state = lock_unpoisoned(&self.state);
        state.records.clear();
        state.total_bytes = 0;
        state.paged = true;
        state.base_query = base_history_query(&query);
        state.next_cursor = page.next_cursor.clone();
        state.loaded_target = page.records.len().min(MAX_SESSION_RECORDS);
        state.page = page
            .records
            .into_iter()
            .map(SessionRecordView::from_summary)
            .collect();
        *lock_unpoisoned(&self.last_deleted) = None;
    }

    fn persist_record(&self, record: &ClipboardRecord) {
        if let NotePersistence::Durable(persistence) = &self.persistence
            && self.storage_available.load(Ordering::Acquire)
        {
            if persistence.save_record(record).is_err() {
                self.storage_available.store(false, Ordering::Release);
            } else {
                let _ = self.refresh_loaded_window();
            }
        }
    }

    fn refresh_loaded_window(&self) -> Result<(), SessionRecordError> {
        let (base_query, loaded_target, paged) = {
            let state = lock_unpoisoned(&self.state);
            (state.base_query.clone(), state.loaded_target, state.paged)
        };
        if !paged {
            return Ok(());
        }
        let page = self.load_window(base_query, loaded_target)?;
        let page_ids: std::collections::HashSet<_> =
            page.records.iter().map(|summary| summary.id).collect();
        let mut state = lock_unpoisoned(&self.state);
        state.next_cursor = page.next_cursor.clone();
        state.page = page
            .records
            .into_iter()
            .map(SessionRecordView::from_summary)
            .collect();
        state.records.retain(|record| page_ids.contains(&record.id));
        state.total_bytes = state
            .records
            .iter()
            .map(|record| record_bytes(record))
            .sum();
        Ok(())
    }

    fn load_window(
        &self,
        base_query: HistoryQuery,
        loaded_target: usize,
    ) -> Result<HistoryPage, SessionRecordError> {
        let target = loaded_target.clamp(1, MAX_SESSION_RECORDS);
        let mut records = Vec::with_capacity(target);
        let mut cursor = None;
        let mut next_cursor = None;
        while records.len() < target {
            let query = HistoryQuery {
                cursor: cursor.clone(),
                limit: (target - records.len()).min(100),
                ..base_query.clone()
            };
            let page = self.load_page(query)?;
            records.extend(page.records);
            next_cursor = page.next_cursor;
            let Some(next) = next_cursor.clone() else {
                break;
            };
            cursor = Some(next);
        }
        Ok(HistoryPage {
            records,
            next_cursor,
        })
    }

    fn load_page(&self, query: HistoryQuery) -> Result<HistoryPage, SessionRecordError> {
        match &self.persistence {
            NotePersistence::Durable(persistence) => persistence
                .load_page(query)
                .map_err(|_| SessionRecordError::PersistenceUnavailable),
            NotePersistence::NotConfigured | NotePersistence::SessionOnly => {
                Err(SessionRecordError::PersistenceUnavailable)
            }
        }
    }

    fn load_page_front_for_capture(&self) {
        let id = {
            let state = lock_unpoisoned(&self.state);
            if !state.paged {
                return;
            }
            state
                .page
                .front()
                .filter(|view| !state.records.iter().any(|record| record.id == view.id))
                .map(|view| view.id)
        };
        if let Some(id) = id {
            let _ = self.load_record_for_mutation(id);
        }
    }

    fn require_durable_storage(&self) -> Result<(), SessionRecordError> {
        if !matches!(self.persistence, NotePersistence::Durable(_))
            || !self.storage_available.load(Ordering::Acquire)
        {
            return Err(SessionRecordError::PersistenceUnavailable);
        }
        Ok(())
    }

    fn load_record_for_mutation(&self, id: RecordId) -> Result<(), SessionRecordError> {
        if lock_unpoisoned(&self.state)
            .records
            .iter()
            .any(|record| record.id == id)
        {
            return Ok(());
        }
        let record = match &self.persistence {
            NotePersistence::Durable(persistence) => persistence
                .record_details(id)
                .map_err(|_| SessionRecordError::NotFound)?,
            NotePersistence::NotConfigured | NotePersistence::SessionOnly => {
                return Err(SessionRecordError::NotFound);
            }
        };
        let bytes = checked_record_bytes(&record).ok_or(SessionRecordError::RecordTooLarge)?;
        if bytes > self.limits.record_bytes {
            return Err(SessionRecordError::RecordTooLarge);
        }
        let mut state = lock_unpoisoned(&self.state);
        let position = state
            .records
            .iter()
            .position(|existing| {
                record_order(
                    record.captured_at,
                    record.id,
                    existing.captured_at,
                    existing.id,
                )
            })
            .unwrap_or(state.records.len());
        state.total_bytes = state.total_bytes.saturating_add(bytes);
        state.records.insert(position, Arc::new(record));
        while state.records.len() > MAX_SESSION_RECORDS
            || state.total_bytes > self.limits.total_bytes
        {
            let remove_index = state
                .records
                .iter()
                .rposition(|record| record.id != id)
                .ok_or(SessionRecordError::RecordTooLarge)?;
            let removed = state
                .records
                .remove(remove_index)
                .ok_or(SessionRecordError::RecordTooLarge)?;
            state.total_bytes = state.total_bytes.saturating_sub(record_bytes(&removed));
        }
        Ok(())
    }

    fn restore_in_memory(&self, record: ClipboardRecord) {
        let mut state = lock_unpoisoned(&self.state);
        if state
            .records
            .iter()
            .any(|existing| existing.id == record.id)
        {
            return;
        }
        let bytes = record_bytes(&record);
        let position = state
            .records
            .iter()
            .position(|existing| {
                record_order(
                    record.captured_at,
                    record.id,
                    existing.captured_at,
                    existing.id,
                )
            })
            .unwrap_or(state.records.len());
        state.total_bytes = state.total_bytes.saturating_add(bytes);
        let view = SessionRecordView::from_record(&record, self.limits.preview_bytes);
        state.records.insert(position, Arc::new(record));
        if state.paged {
            state.page.retain(|existing| existing.id != view.id);
            let page_position = state
                .page
                .iter()
                .position(|existing| {
                    record_order(view.captured_at, view.id, existing.captured_at, existing.id)
                })
                .unwrap_or(state.page.len());
            state.page.insert(page_position, view);
        }
        evict_to_limits(&mut state, self.limits);
    }

    #[cfg(test)]
    pub(crate) fn budget_snapshot(&self) -> (usize, usize) {
        let state = lock_unpoisoned(&self.state);
        (state.total_bytes, state.records.len())
    }
}

impl SessionRecordView {
    fn from_record(record: &ClipboardRecord, preview_bytes: usize) -> Self {
        let text = record
            .representations
            .iter()
            .find_map(|representation| match representation {
                ClipboardRepresentation::UnicodeText { text } => {
                    Some(utf8_preview(text, preview_bytes))
                }
                ClipboardRepresentation::Rtf { .. }
                | ClipboardRepresentation::Html { .. }
                | ClipboardRepresentation::Png { .. }
                | ClipboardRepresentation::DibV5 { .. }
                | ClipboardRepresentation::FileList { .. } => None,
            });
        let has_image = record.representations.iter().any(|representation| {
            matches!(
                representation,
                ClipboardRepresentation::Png { .. } | ClipboardRepresentation::DibV5 { .. }
            )
        });
        Self {
            id: record.id,
            captured_at: record.captured_at,
            source_application: record.source.application_name.clone(),
            text,
            has_image,
            note: record.note.clone(),
            group_id: record.group_id,
        }
    }

    fn from_summary(summary: HistoryRecordSummary) -> Self {
        Self {
            id: summary.id,
            captured_at: summary.captured_at,
            source_application: summary.source_application,
            text: summary.text,
            has_image: summary.has_image,
            note: summary.note,
            group_id: summary.group_id,
        }
    }
}

pub(crate) fn checked_representation_bytes(
    representations: &[ClipboardRepresentation],
) -> Option<usize> {
    representations
        .iter()
        .try_fold(0_usize, |total, representation| {
            let payload = representation.checked_payload_bytes()?;
            total.checked_add(payload.checked_add(REPRESENTATION_OVERHEAD_BYTES)?)
        })
}

pub(crate) fn representation_bytes(representations: &[ClipboardRepresentation]) -> usize {
    checked_representation_bytes(representations).unwrap_or(usize::MAX)
}

fn record_bytes(record: &ClipboardRecord) -> usize {
    checked_record_bytes(record).unwrap_or(usize::MAX)
}

fn checked_record_bytes(record: &ClipboardRecord) -> Option<usize> {
    checked_representation_bytes(&record.representations)?
        .checked_add(record.note.as_ref().map_or(0, |note| note.as_str().len()))
}

fn retain_preferred_image(representations: &mut Vec<ClipboardRepresentation>) {
    let preferred = representations
        .iter()
        .enumerate()
        .filter_map(|(index, representation)| match representation {
            ClipboardRepresentation::Png { bytes } | ClipboardRepresentation::DibV5 { bytes } => {
                Some((index, bytes.len()))
            }
            ClipboardRepresentation::UnicodeText { .. }
            | ClipboardRepresentation::Rtf { .. }
            | ClipboardRepresentation::Html { .. }
            | ClipboardRepresentation::FileList { .. } => None,
        })
        .min_by_key(|(_, bytes)| *bytes)
        .map(|(index, _)| index);
    let mut original_index = 0;
    representations.retain(|representation| {
        let keep = !matches!(
            representation,
            ClipboardRepresentation::Png { .. } | ClipboardRepresentation::DibV5 { .. }
        ) || preferred == Some(original_index);
        original_index += 1;
        keep
    });
}

fn sort_records(records: &mut VecDeque<Arc<ClipboardRecord>>) {
    records.make_contiguous().sort_by(|left, right| {
        right
            .captured_at
            .cmp(&left.captured_at)
            .then_with(|| right.id.as_uuid().cmp(left.id.as_uuid()))
    });
}

fn base_history_query(query: &HistoryQuery) -> HistoryQuery {
    HistoryQuery {
        cursor: None,
        limit: 0,
        content_kind: query.content_kind,
        group_id: query.group_id,
        ungrouped_only: query.ungrouped_only,
        favorites_only: query.favorites_only,
    }
}

fn record_order(
    left_at: chrono::DateTime<chrono::Utc>,
    left_id: RecordId,
    right_at: chrono::DateTime<chrono::Utc>,
    right_id: RecordId,
) -> bool {
    left_at > right_at || (left_at == right_at && left_id.as_uuid() > right_id.as_uuid())
}

fn evict_to_limits(state: &mut SessionRecordState, limits: SessionRecordLimits) {
    while state.records.len() > MAX_SESSION_RECORDS || state.total_bytes > limits.total_bytes {
        let Some(removed) = state.records.pop_back() else {
            break;
        };
        state.total_bytes = state.total_bytes.saturating_sub(record_bytes(&removed));
        state.page.retain(|view| view.id != removed.id);
    }
    while state.page.len() > MAX_SESSION_RECORDS {
        state.page.pop_back();
    }
}

fn utf8_preview(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[derive(Debug)]
pub enum SessionRecordError {
    NotFound,
    InvalidNote(RecordNoteError),
    RecordTooLarge,
    PersistenceUnavailable,
    ImagePreviewUnavailable,
}

impl std::fmt::Display for SessionRecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("clipboard record is no longer available"),
            Self::InvalidNote(_) => formatter.write_str("clipboard record note is invalid"),
            Self::RecordTooLarge => formatter.write_str("clipboard record exceeds memory limits"),
            Self::PersistenceUnavailable => {
                formatter.write_str("clipboard record metadata was not saved to local storage")
            }
            Self::ImagePreviewUnavailable => {
                formatter.write_str("clipboard image preview is unavailable")
            }
        }
    }
}

impl std::error::Error for SessionRecordError {}

pub struct SessionRecordCommands<'a> {
    store: &'a SessionRecordStore,
}

impl<'a> SessionRecordCommands<'a> {
    pub fn new(store: &'a SessionRecordStore) -> Self {
        Self { store }
    }

    pub fn list(&self) -> Vec<SessionRecordView> {
        self.store.list()
    }

    pub fn update_note(
        &self,
        id: RecordId,
        value: String,
    ) -> Result<SessionRecordView, SessionRecordError> {
        self.store.update_note(id, value)
    }

    pub fn update_group(
        &self,
        id: RecordId,
        group_id: Option<GroupId>,
    ) -> Result<SessionRecordView, SessionRecordError> {
        self.store.update_group(id, group_id)
    }

    pub fn delete(&self, id: RecordId) -> Result<ClipboardRecord, SessionRecordError> {
        self.store.delete(id)
    }

    pub fn restore_last_deleted(
        &self,
        id: RecordId,
    ) -> Result<SessionRecordView, SessionRecordError> {
        self.store.restore_last_deleted(id)
    }

    pub fn clear(&self) -> Result<usize, SessionRecordError> {
        self.store.clear()
    }

    pub fn representations(
        &self,
        id: RecordId,
    ) -> Result<Vec<ClipboardRepresentation>, SessionRecordError> {
        self.store
            .representations(id)
            .ok_or(SessionRecordError::NotFound)
    }

    pub fn image_preview(&self, id: RecordId) -> Result<ImagePreviewView, SessionRecordError> {
        self.store.image_preview(id)
    }
}

fn png_preview(bytes: &[u8]) -> Option<ImagePreviewView> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() > IMAGE_PREVIEW_MAX_BYTES
        || bytes.get(..8)? != PNG_SIGNATURE
        || bytes.get(12..16)? != b"IHDR"
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    valid_image_dimensions(width, height)?;
    Some(ImagePreviewView {
        data_url: format!("data:image/png;base64,{}", BASE64_STANDARD.encode(bytes)),
        width,
        height,
    })
}

fn dib_preview(dib: &[u8]) -> Option<ImagePreviewView> {
    if dib.len() > IMAGE_PREVIEW_MAX_BYTES {
        return None;
    }
    let width = read_i32(dib, 4)?.unsigned_abs();
    let height = read_i32(dib, 8)?.unsigned_abs();
    valid_image_dimensions(width, height)?;
    let bmp = dib_to_bmp(dib)?;
    Some(ImagePreviewView {
        data_url: format!("data:image/bmp;base64,{}", BASE64_STANDARD.encode(bmp)),
        width,
        height,
    })
}

fn valid_image_dimensions(width: u32, height: u32) -> Option<()> {
    if width == 0
        || height == 0
        || width > IMAGE_PREVIEW_MAX_DIMENSION
        || height > IMAGE_PREVIEW_MAX_DIMENSION
        || u64::from(width) * u64::from(height) > IMAGE_PREVIEW_MAX_PIXELS
    {
        return None;
    }
    Some(())
}

fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    let header_size = usize::try_from(read_u32(dib, 0)?).ok()?;
    if header_size < BITMAP_V5_HEADER_BYTES || header_size > dib.len() {
        return None;
    }
    let bits_per_pixel = usize::from(read_u16(dib, 14)?);
    let colors_used = usize::try_from(read_u32(dib, 32)?).ok()?;
    let palette_entries = if colors_used > 0 {
        colors_used
    } else if bits_per_pixel <= 8 {
        1_usize.checked_shl(u32::try_from(bits_per_pixel).ok()?)?
    } else {
        0
    };
    let palette_bytes = palette_entries.checked_mul(4)?;
    let pixel_offset = BITMAP_FILE_HEADER_BYTES
        .checked_add(header_size)?
        .checked_add(palette_bytes)?;
    let file_size = BITMAP_FILE_HEADER_BYTES.checked_add(dib.len())?;
    if pixel_offset > file_size || file_size > u32::MAX as usize {
        return None;
    }
    let mut bmp = Vec::with_capacity(file_size);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    Some(bmp)
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use std::sync::{Barrier, atomic::AtomicBool};

    use super::*;
    use crate::domain::{ContentIdentity, SourceIdentity};
    use crate::services::persistence::{PersistenceError, RecordPersistence};
    use crate::{domain::HistoryQuery, services::persistence::SqliteRepository};
    use tempfile::tempdir;

    struct BlockingPagePersistence {
        repository: Arc<SqliteRepository>,
        block_next_page: AtomicBool,
        page_loaded: Barrier,
        resume_page: Barrier,
    }

    impl RecordPersistence for BlockingPagePersistence {
        fn save_record(&self, record: &ClipboardRecord) -> Result<(), PersistenceError> {
            self.repository.save_record(record)
        }

        fn update_note(
            &self,
            id: RecordId,
            note: Option<&RecordNote>,
        ) -> Result<(), PersistenceError> {
            self.repository.update_note(id, note)
        }

        fn delete_record(&self, id: RecordId) -> Result<(), PersistenceError> {
            self.repository.delete_record(id)
        }

        fn clear_records(&self) -> Result<usize, PersistenceError> {
            self.repository.clear_records()
        }

        fn load_page(&self, query: HistoryQuery) -> Result<HistoryPage, PersistenceError> {
            let page = self.repository.load_page(query)?;
            if self.block_next_page.swap(false, Ordering::AcqRel) {
                self.page_loaded.wait();
                self.resume_page.wait();
            }
            Ok(page)
        }

        fn record_details(&self, id: RecordId) -> Result<ClipboardRecord, PersistenceError> {
            self.repository.record_details(id)
        }
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    struct FailingPersistence;

    impl RecordPersistence for FailingPersistence {
        fn save_record(&self, _record: &ClipboardRecord) -> Result<(), PersistenceError> {
            Err(PersistenceError::WorkerUnavailable)
        }

        fn update_note(
            &self,
            _id: RecordId,
            _note: Option<&RecordNote>,
        ) -> Result<(), PersistenceError> {
            Err(PersistenceError::WorkerUnavailable)
        }

        fn delete_record(&self, _id: RecordId) -> Result<(), PersistenceError> {
            Err(PersistenceError::WorkerUnavailable)
        }

        fn clear_records(&self) -> Result<usize, PersistenceError> {
            Err(PersistenceError::WorkerUnavailable)
        }
    }

    #[test]
    fn store_exposes_only_real_session_captures_and_updates_notes_in_memory() {
        let store = SessionRecordStore::default();
        assert!(store.list().is_empty());
        store.capture(capture("one", "real text"));
        let record = store.list().pop().unwrap();

        assert_eq!(record.text.as_deref(), Some("real text"));
        assert_eq!(
            store.representations(record.id),
            Some(vec![ClipboardRepresentation::UnicodeText {
                text: "real text".to_owned()
            }])
        );
        assert_eq!(
            store
                .update_note(record.id, "work account".to_owned())
                .unwrap()
                .note
                .as_ref()
                .map(RecordNote::as_str),
            Some("work account")
        );
    }

    #[test]
    fn persistence_failure_keeps_capture_and_note_available_in_memory() {
        let available = Arc::new(AtomicBool::new(true));
        let store = SessionRecordStore::with_persistence(
            Vec::new(),
            Arc::new(FailingPersistence),
            Arc::clone(&available),
        );

        assert!(store.capture(capture("one", "still available")));
        let id = store.list()[0].id;
        assert_eq!(store.list()[0].text.as_deref(), Some("still available"));
        assert!(!available.load(Ordering::Acquire));

        available.store(true, Ordering::Release);
        assert!(matches!(
            store.update_note(id, "local draft".to_owned()),
            Err(SessionRecordError::PersistenceUnavailable)
        ));
        assert_eq!(
            store.list()[0].note.as_ref().map(RecordNote::as_str),
            Some("local draft")
        );
        assert!(!available.load(Ordering::Acquire));
    }

    #[test]
    fn session_only_note_edit_keeps_the_draft_but_reports_it_is_not_durable() {
        let available = Arc::new(AtomicBool::new(false));
        let record = ClipboardRecord::from_capture(capture("one", "session text"));
        let id = record.id;
        let store = SessionRecordStore::with_session_only(vec![record], available);

        assert!(matches!(
            store.update_note(id, "retry later".to_owned()),
            Err(SessionRecordError::PersistenceUnavailable)
        ));
        assert_eq!(
            store.list()[0].note.as_ref().map(RecordNote::as_str),
            Some("retry later")
        );
    }

    #[test]
    fn loaded_history_obeys_record_count_and_memory_budgets() {
        let records = (0..(MAX_SESSION_RECORDS + 20))
            .map(|index| ClipboardRecord::from_capture(capture(&format!("record-{index}"), "x")))
            .collect();
        let store = SessionRecordStore::with_loaded(records);

        let (bytes, count) = store.budget_snapshot();
        assert_eq!(count, MAX_SESSION_RECORDS);
        assert!(bytes <= DEFAULT_STORE_BYTES);

        let oversized = ClipboardRecord::from_capture(capture(
            "oversized",
            &"x".repeat(MAX_CAPTURE_RECORD_BYTES + 1),
        ));
        let store = SessionRecordStore::with_loaded(vec![oversized]);
        assert!(store.list().is_empty());
    }

    #[test]
    fn paged_working_set_deduplicates_and_loads_binary_details_on_demand() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let mut binary = ClipboardRecord::from_capture(capture("binary", "caption"));
        binary.representations.push(ClipboardRepresentation::Png {
            bytes: vec![9; 128],
        });
        repository.save_record(&binary).unwrap();
        let text = ClipboardRecord::from_capture(capture("text", "plain"));
        repository.save_record(&text).unwrap();
        let page = repository
            .load_page(HistoryQuery {
                limit: 1,
                ..HistoryQuery::default()
            })
            .unwrap();
        let next = repository
            .load_page(HistoryQuery {
                cursor: page.next_cursor.clone(),
                limit: 1,
                ..HistoryQuery::default()
            })
            .unwrap();
        let available = Arc::new(AtomicBool::new(true));
        let store = SessionRecordStore::with_persistence_page(
            page.clone(),
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            available,
        );

        store.append_page(page);
        store.append_page(next);

        assert_eq!(store.list().len(), 2);
        assert!(serde_json::to_string(&store.list()).unwrap().len() < 2048);
        assert!(store.record_details(binary.id).unwrap().representations.iter().any(
            |representation| matches!(representation, ClipboardRepresentation::Png { bytes } if bytes.len() == 128)
        ));
    }

    #[test]
    fn paged_history_supports_note_group_and_delete_mutations_on_demand() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let note_record = ClipboardRecord::from_capture(capture("paged-note", "note"));
        let group_record = ClipboardRecord::from_capture(capture("paged-group", "group"));
        let delete_record = ClipboardRecord::from_capture(capture("paged-delete", "delete"));
        for record in [&note_record, &group_record, &delete_record] {
            repository.save_record(record).unwrap();
        }
        let page = repository.load_page(HistoryQuery::default()).unwrap();
        let available = Arc::new(AtomicBool::new(true));
        let store = SessionRecordStore::with_persistence_page(
            page,
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            available,
        );
        let group = GroupId::new();

        assert_eq!(
            store
                .update_note(note_record.id, "loaded on demand".to_owned())
                .unwrap()
                .note
                .as_ref()
                .map(RecordNote::as_str),
            Some("loaded on demand")
        );
        assert_eq!(
            store
                .update_group(group_record.id, Some(group))
                .unwrap()
                .group_id,
            Some(group)
        );
        assert_eq!(
            repository.record_details(group_record.id).unwrap().group_id,
            Some(group)
        );
        assert_eq!(store.delete(delete_record.id).unwrap().id, delete_record.id);
        assert!(store.list().iter().all(|view| view.id != delete_record.id));
        assert!(repository.record_details(delete_record.id).is_err());
    }

    #[test]
    fn paged_mutation_preserves_appended_range_and_next_cursor() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 27, 8, 0, 0).unwrap();
        for index in 0..35 {
            repository
                .save_record(&ClipboardRecord::from_capture(capture_at(
                    &format!("range-{index}"),
                    "range",
                    base + Duration::seconds(index),
                )))
                .unwrap();
        }
        let query = HistoryQuery {
            limit: 10,
            ..HistoryQuery::default()
        };
        let first = repository.load_page(query.clone()).unwrap();
        let second = repository
            .load_page(HistoryQuery {
                cursor: first.next_cursor.clone(),
                ..query.clone()
            })
            .unwrap();
        let store = SessionRecordStore::with_persistence_query_page(
            query,
            first,
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
        );
        store.append_page(second);
        let mutate = store.list()[15].id;

        store.update_note(mutate, "kept range".to_owned()).unwrap();

        assert_eq!(store.list().len(), 20);
        assert_eq!(
            store
                .list()
                .iter()
                .find(|view| view.id == mutate)
                .and_then(|view| view.note.as_ref())
                .map(RecordNote::as_str),
            Some("kept range")
        );
        let next = store.next_page_query().expect("more history remains");
        assert!(next.cursor.is_some());
        assert_eq!(next.content_kind, None);
        assert!(!repository.load_page(next).unwrap().records.is_empty());
    }

    #[test]
    fn filtered_paged_mutation_preserves_the_base_query() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let included = GroupId::new();
        let excluded = GroupId::new();
        let mut included_records = Vec::new();
        for index in 0..12 {
            let mut record = ClipboardRecord::from_capture(capture(
                &format!("included-{index}"),
                &format!("included-{index}"),
            ));
            record.group_id = Some(included);
            repository.save_record(&record).unwrap();
            included_records.push(record);
        }
        for index in 0..8 {
            let mut record = ClipboardRecord::from_capture(capture(
                &format!("excluded-{index}"),
                &format!("excluded-{index}"),
            ));
            record.group_id = Some(excluded);
            repository.save_record(&record).unwrap();
        }
        let query = HistoryQuery {
            limit: 6,
            group_id: Some(included),
            ..HistoryQuery::default()
        };
        let page = repository.load_page(query.clone()).unwrap();
        let store = SessionRecordStore::with_persistence_query_page(
            query,
            page,
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
        );
        let mutate = store.list()[2].id;

        store.update_note(mutate, "filtered".to_owned()).unwrap();

        assert_eq!(store.list().len(), 6);
        assert!(
            store
                .list()
                .iter()
                .all(|view| view.group_id == Some(included))
        );
        let next = store
            .next_page_query()
            .expect("filtered history has another page");
        assert_eq!(next.group_id, Some(included));
        assert!(
            repository
                .load_page(next)
                .unwrap()
                .records
                .iter()
                .all(|summary| summary.group_id == Some(included))
        );
        assert!(included_records.iter().any(|record| record.id == mutate));
    }

    #[test]
    fn serialized_refresh_cannot_publish_an_old_snapshot_after_a_new_mutation() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let first = ClipboardRecord::from_capture(capture("first", "first"));
        let second = ClipboardRecord::from_capture(capture("second", "second"));
        repository.save_record(&first).unwrap();
        repository.save_record(&second).unwrap();
        let persistence = Arc::new(BlockingPagePersistence {
            repository: Arc::clone(&repository),
            block_next_page: AtomicBool::new(true),
            page_loaded: Barrier::new(2),
            resume_page: Barrier::new(2),
        });
        let store = Arc::new(SessionRecordStore::with_persistence_page(
            repository.load_page(HistoryQuery::default()).unwrap(),
            Arc::clone(&persistence) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
        ));
        let old_refresh = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || store.update_note(first.id, "old".to_owned()))
        };
        persistence.page_loaded.wait();
        let newer_mutation = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || store.update_note(first.id, "new".to_owned()))
        };
        persistence.resume_page.wait();
        old_refresh.join().unwrap().unwrap();
        newer_mutation.join().unwrap().unwrap();

        assert_eq!(
            store
                .list()
                .iter()
                .find(|view| view.id == first.id)
                .and_then(|view| view.note.as_ref())
                .map(RecordNote::as_str),
            Some("new")
        );
        assert_eq!(
            repository
                .record_details(first.id)
                .unwrap()
                .note
                .as_ref()
                .map(RecordNote::as_str),
            Some("new")
        );
    }

    #[test]
    fn paged_capture_refreshes_the_existing_front_record_and_reorders_the_summary() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 10, 0, 0).unwrap();
        let front = ClipboardRecord::from_capture(capture_at("same", "original", base));
        let other = ClipboardRecord::from_capture(capture_at(
            "other",
            "other",
            base - Duration::seconds(1),
        ));
        repository.save_record(&other).unwrap();
        repository.save_record(&front).unwrap();
        let store = SessionRecordStore::with_persistence_page(
            repository.load_page(HistoryQuery::default()).unwrap(),
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
        );
        let refreshed_at = base + Duration::seconds(5);

        assert!(matches!(
            store.capture_one(capture_at("same", "updated", refreshed_at)),
            CaptureStatus::Refreshed { .. }
        ));

        let listed = store.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, front.id);
        assert_eq!(listed[0].captured_at, refreshed_at);
        assert_eq!(
            repository
                .load_page(HistoryQuery::default())
                .unwrap()
                .records
                .len(),
            2
        );
        assert_eq!(
            repository.record_details(front.id).unwrap().captured_at,
            refreshed_at
        );
    }

    #[test]
    fn paged_delete_undo_restores_payload_and_summary_in_stable_order() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 11, 0, 0).unwrap();
        let older = ClipboardRecord::from_capture(capture_at("older", "older", base));
        let deleted = ClipboardRecord::from_capture(capture_at(
            "deleted",
            "deleted",
            base + Duration::seconds(1),
        ));
        let newer = ClipboardRecord::from_capture(capture_at(
            "newer",
            "newer",
            base + Duration::seconds(2),
        ));
        for record in [&older, &deleted, &newer] {
            repository.save_record(record).unwrap();
        }
        let store = SessionRecordStore::with_persistence_page(
            repository.load_page(HistoryQuery::default()).unwrap(),
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
        );

        store.delete(deleted.id).unwrap();
        store.restore_last_deleted(deleted.id).unwrap();

        assert_eq!(
            store.list().iter().map(|view| view.id).collect::<Vec<_>>(),
            vec![newer.id, deleted.id, older.id]
        );
        assert_eq!(store.record_details(deleted.id).unwrap(), deleted);
    }

    #[test]
    fn paged_capture_drops_summaries_evicted_by_disk_quota() {
        let directory = tempdir().unwrap();
        let quota = crate::services::persistence::DiskQuota {
            max_records: 2,
            max_payload_bytes: usize::MAX,
            incremental_vacuum_pages: 1,
        };
        let repository =
            SqliteRepository::open_with_quota(directory.path().join("history.sqlite3"), quota)
                .unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 12, 0, 0).unwrap();
        let oldest = ClipboardRecord::from_capture(capture_at("oldest", "oldest", base));
        let middle = ClipboardRecord::from_capture(capture_at(
            "middle",
            "middle",
            base + Duration::seconds(1),
        ));
        repository.save_record(&oldest).unwrap();
        repository.save_record(&middle).unwrap();
        let store = SessionRecordStore::with_persistence_page(
            repository.load_page(HistoryQuery::default()).unwrap(),
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
        );

        assert!(store.capture(capture_at("newest", "newest", base + Duration::seconds(2),)));

        let listed = store.list();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|view| view.id != oldest.id));
        assert!(repository.record_details(oldest.id).is_err());
    }

    #[test]
    fn reloading_after_large_restore_keeps_history_paged() {
        let directory = tempdir().unwrap();
        let live = SqliteRepository::open(directory.path().join("live.sqlite3")).unwrap();
        let source_path = directory.path().join("large.clipbackup");
        let source = SqliteRepository::open(source_path.clone()).unwrap();
        let base = Utc.with_ymd_and_hms(2026, 8, 26, 13, 0, 0).unwrap();
        for index in 0..520 {
            source
                .save_record(&ClipboardRecord::from_capture(capture_at(
                    &format!("restored-{index}"),
                    "x",
                    base + Duration::seconds(index),
                )))
                .unwrap();
        }
        drop(source);
        let store = SessionRecordStore::with_persistence_page(
            HistoryPage {
                records: Vec::new(),
                next_cursor: None,
            },
            Arc::clone(&live) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
        );
        live.restore_from(
            &source_path,
            crate::services::persistence::RestoreBudget {
                max_records: MAX_SESSION_RECORDS,
                max_total_bytes: DEFAULT_STORE_BYTES,
                max_record_bytes: MAX_CAPTURE_RECORD_BYTES,
            },
        )
        .unwrap();

        store.reload_first_page().unwrap();
        let first = live
            .load_page(HistoryQuery {
                limit: STARTUP_HISTORY_RECORDS,
                ..HistoryQuery::default()
            })
            .unwrap();
        let next = live
            .load_page(HistoryQuery {
                cursor: first.next_cursor.clone(),
                limit: STARTUP_HISTORY_RECORDS,
                ..HistoryQuery::default()
            })
            .unwrap();
        store.append_page(next);

        assert_eq!(store.list().len(), 200);
        assert_eq!(store.list()[0].text.as_deref(), Some("x"));
        assert_eq!(live.load_recent(600).unwrap().len(), 520);
    }

    #[test]
    fn paged_clear_returns_the_database_delete_count() {
        let directory = tempdir().unwrap();
        let repository = SqliteRepository::open(directory.path().join("history.sqlite3")).unwrap();
        for index in 0..3 {
            repository
                .save_record(&ClipboardRecord::from_capture(capture(
                    &format!("clear-{index}"),
                    "x",
                )))
                .unwrap();
        }
        let store = SessionRecordStore::with_persistence_page(
            repository.load_page(HistoryQuery::default()).unwrap(),
            Arc::clone(&repository) as Arc<dyn RecordPersistence>,
            Arc::new(AtomicBool::new(true)),
        );

        assert_eq!(store.clear().unwrap(), 3);
        assert!(store.list().is_empty());
        assert!(
            repository
                .load_page(HistoryQuery::default())
                .unwrap()
                .records
                .is_empty()
        );
    }

    #[test]
    fn note_boundary_rejects_newlines_and_more_than_200_unicode_characters() {
        let store = SessionRecordStore::default();
        store.capture(capture("one", "text"));
        let id = store.list()[0].id;

        assert!(
            store
                .update_note(id, "line one\nline two".to_owned())
                .is_err()
        );
        assert!(store.update_note(id, "界".repeat(201)).is_err());
        assert!(store.update_note(id, "界".repeat(200)).is_ok());
    }

    #[test]
    fn clearing_a_group_moves_only_matching_records_to_ungrouped() {
        let group = GroupId::new();
        let other_group = GroupId::new();
        let mut first = ClipboardRecord::from_capture(capture("first", "one"));
        first.group_id = Some(group);
        let mut second = ClipboardRecord::from_capture(capture("second", "two"));
        second.group_id = Some(other_group);
        let store = SessionRecordStore::with_loaded(vec![first, second]);

        assert_eq!(store.clear_group(group), 1);
        let listed = store.list();
        assert_eq!(listed[0].group_id, None);
        assert_eq!(listed[1].group_id, Some(other_group));
    }

    #[test]
    fn command_boundary_resolves_payload_only_from_a_real_session_record_id() {
        let store = SessionRecordStore::default();
        let commands = SessionRecordCommands::new(&store);
        assert!(commands.representations(RecordId::new()).is_err());

        store.capture(capture("one", "trusted text"));
        let record = commands.list().pop().unwrap();

        assert_eq!(
            commands.representations(record.id).unwrap(),
            vec![ClipboardRepresentation::UnicodeText {
                text: "trusted text".to_owned(),
            }]
        );
    }

    #[test]
    fn image_preview_is_bounded_png_and_requires_a_real_image_record() {
        let store = SessionRecordStore::default();
        let mut captured = capture("preview-image", "caption");
        captured.representations.push(ClipboardRepresentation::Png {
            bytes: png_bytes(1200, 600),
        });
        store.capture(captured);
        let record = store.list().remove(0);

        let preview = store.image_preview(record.id).unwrap();

        assert_eq!((preview.width, preview.height), (1200, 600));
        assert!(preview.data_url.starts_with("data:image/png;base64,"));
        assert!(store.image_preview(RecordId::new()).is_err());
    }

    #[test]
    fn malformed_or_oversized_image_dimensions_do_not_escape_as_a_preview() {
        let store = SessionRecordStore::default();
        let mut malformed = capture("malformed-image", "caption");
        malformed.representations = vec![ClipboardRepresentation::Png {
            bytes: vec![1, 2, 3, 4],
        }];
        store.capture(malformed);
        let id = store.list().remove(0).id;

        assert!(matches!(
            store.image_preview(id),
            Err(SessionRecordError::ImagePreviewUnavailable)
        ));
        assert!(dib_to_bmp(&[0; BITMAP_V5_HEADER_BYTES]).is_none());
    }

    #[test]
    fn store_enforces_single_and_total_byte_budgets_by_evicting_oldest_records() {
        let store = SessionRecordStore::with_limits(SessionRecordLimits {
            total_bytes: 82,
            record_bytes: 42,
            preview_bytes: 8,
        });

        store.capture(capture("one", "123456789"));
        store.capture(capture("two", "abcdefghi"));
        assert_eq!(store.list().len(), 2);

        store.capture(capture("three", "ABCDEFGHI"));
        let listed = store.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].text.as_deref(), Some("ABCDEFGH"));
        assert_eq!(listed[1].text.as_deref(), Some("abcdefgh"));

        store.capture(capture("too-large", &"x".repeat(43)));
        assert_eq!(store.list(), listed);
    }

    #[test]
    fn image_capture_keeps_only_the_smaller_replayable_format() {
        let store = SessionRecordStore::default();
        let mut image = capture("image", "caption");
        image.representations.extend([
            ClipboardRepresentation::Png { bytes: vec![1; 12] },
            ClipboardRepresentation::DibV5 { bytes: vec![2; 7] },
        ]);

        store.capture(image);
        let id = store.list()[0].id;
        let representations = store.representations(id).unwrap();

        assert!(representations.iter().any(|representation| matches!(
            representation,
            ClipboardRepresentation::UnicodeText { .. }
        )));
        assert!(representations.iter().any(|representation| matches!(
            representation,
            ClipboardRepresentation::DibV5 { bytes } if bytes.len() == 7
        )));
        assert!(
            !representations.iter().any(|representation| matches!(
                representation,
                ClipboardRepresentation::Png { .. }
            ))
        );
    }

    #[test]
    fn duplicate_merge_over_record_budget_keeps_existing_record_unchanged() {
        let store = SessionRecordStore::with_limits(SessionRecordLimits {
            total_bytes: 256,
            record_bytes: 80,
            preview_bytes: 16,
        });
        assert_eq!(
            store.capture_one(capture("same", &"a".repeat(40))),
            CaptureStatus::Inserted { bytes: 72 }
        );
        let before = store.list();
        let duplicate = CapturedClipboard {
            content_identity: ContentIdentity::new("same"),
            captured_at: Utc::now(),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::Png { bytes: vec![7; 40] }],
        };

        assert_eq!(
            store.capture_one(duplicate),
            CaptureStatus::RejectedTooLarge
        );
        assert_eq!(store.list(), before);
        assert_eq!(
            store.representations(before[0].id),
            Some(vec![ClipboardRepresentation::UnicodeText {
                text: "a".repeat(40)
            }])
        );
    }

    #[test]
    fn refresh_at_record_count_limit_does_not_evict_the_oldest_record() {
        let store = SessionRecordStore::with_limits(SessionRecordLimits {
            total_bytes: 64 * 1024,
            record_bytes: 1024,
            preview_bytes: 32,
        });
        for index in 0..MAX_SESSION_RECORDS {
            store.capture(capture(
                &format!("record-{index}"),
                &format!("value-{index}"),
            ));
        }
        let before = store.list();
        let newest = before[0].clone();

        assert!(matches!(
            store.capture_one(capture("record-499", "updated")),
            CaptureStatus::Refreshed { .. }
        ));

        let after = store.list();
        assert_eq!(after.len(), MAX_SESSION_RECORDS);
        assert_eq!(
            after.last().map(|record| record.id),
            before.last().map(|record| record.id)
        );
        assert_eq!(after[0].id, newest.id);
    }

    #[test]
    fn rejected_refresh_at_capacity_does_not_evict_any_record() {
        let store = SessionRecordStore::with_limits(SessionRecordLimits {
            total_bytes: 128,
            record_bytes: 80,
            preview_bytes: 16,
        });
        store.capture(capture("old", "old"));
        store.capture(capture("same", &"a".repeat(40)));
        let before = store.list();
        let duplicate = CapturedClipboard {
            content_identity: ContentIdentity::new("same"),
            captured_at: Utc::now(),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::Png { bytes: vec![1; 40] }],
        };

        assert_eq!(
            store.capture_one(duplicate),
            CaptureStatus::RejectedTooLarge
        );
        assert_eq!(store.list(), before);
    }

    #[test]
    fn note_eviction_is_authoritative_for_the_next_capture() {
        let store = SessionRecordStore::with_limits(SessionRecordLimits {
            total_bytes: 77,
            record_bytes: 48,
            preview_bytes: 16,
        });
        store.capture(capture("old", "12345"));
        store.capture(capture("new", "abcde"));
        let newest = store.list()[0].id;
        store.update_note(newest, "note".to_owned()).unwrap();
        assert_eq!(store.list().len(), 1);

        store.capture(capture("third", "xyz"));

        let listed = store.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[1].id, newest);
    }

    #[test]
    fn checked_representation_budget_reports_arithmetic_overflow() {
        let payload = ClipboardRepresentation::UnicodeText {
            text: String::new(),
        };
        assert_eq!(checked_representation_bytes(&[payload]), Some(32));
        assert_eq!(usize::MAX.checked_add(REPRESENTATION_OVERHEAD_BYTES), None);
    }

    #[test]
    fn list_returns_only_a_bounded_text_preview_without_binary_payloads() {
        let store = SessionRecordStore::with_limits(SessionRecordLimits {
            total_bytes: 1024,
            record_bytes: 1024,
            preview_bytes: 5,
        });
        let mut captured = capture("preview", "界界界");
        captured.representations.push(ClipboardRepresentation::Png {
            bytes: vec![9; 128],
        });
        store.capture(captured);

        let view = store.list().pop().unwrap();
        assert_eq!(view.text.as_deref(), Some("界"));
        assert!(view.has_image);
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("9,9,9"));
        assert!(json.len() < 512);
    }

    #[test]
    fn note_bytes_count_toward_the_total_budget() {
        let store = SessionRecordStore::with_limits(SessionRecordLimits {
            total_bytes: 76,
            record_bytes: 42,
            preview_bytes: 8,
        });
        store.capture(capture("oldest", "12345"));
        store.capture(capture("newest", "abcde"));
        let newest = store.list()[0].id;

        store.update_note(newest, "note".to_owned()).unwrap();

        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, newest);
        assert_eq!(
            listed[0].note.as_ref().map(RecordNote::as_str),
            Some("note")
        );
    }

    #[test]
    fn note_cannot_push_a_record_over_the_single_record_budget() {
        let store = SessionRecordStore::with_limits(SessionRecordLimits {
            total_bytes: 64,
            record_bytes: 40,
            preview_bytes: 8,
        });
        store.capture(capture("record", "123456"));
        let id = store.list()[0].id;

        assert!(matches!(
            store.update_note(id, "abc".to_owned()),
            Err(SessionRecordError::RecordTooLarge)
        ));
        assert_eq!(store.list()[0].note, None);
    }

    #[test]
    fn note_update_rejects_total_budget_overflow_without_mutating_file_list_record() {
        let record = ClipboardRecord::from_capture(CapturedClipboard {
            content_identity: ContentIdentity::new("file-list"),
            captured_at: Utc::now(),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::FileList {
                paths: vec![r"C:\Temp\one.txt".to_owned()],
            }],
        });
        let id = record.id;
        let store = SessionRecordStore::with_test_limits(usize::MAX, usize::MAX, 32);
        {
            let mut state = lock_unpoisoned(&store.state);
            state.records.push_back(Arc::new(record.clone()));
            state.total_bytes = usize::MAX;
        }
        let before_budget = store.budget_snapshot();
        let before_records = store.list();
        let before_representations = store.representations(id);

        assert!(matches!(
            store.update_note(id, "note".to_owned()),
            Err(SessionRecordError::RecordTooLarge)
        ));
        assert_eq!(store.list(), before_records);
        assert_eq!(store.representations(id), before_representations);
        assert_eq!(store.budget_snapshot(), before_budget);
    }

    fn capture(identity: &str, text: &str) -> CapturedClipboard {
        capture_at(identity, text, Utc::now())
    }

    fn capture_at(
        identity: &str,
        text: &str,
        captured_at: chrono::DateTime<Utc>,
    ) -> CapturedClipboard {
        CapturedClipboard {
            content_identity: ContentIdentity::new(identity),
            captured_at,
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: text.to_owned(),
            }],
        }
    }
}
