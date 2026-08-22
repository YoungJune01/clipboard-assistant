use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use serde::Serialize;

use crate::domain::{
    CapturedClipboard, ClipboardRecord, ClipboardRepresentation, RecordId, RecordNote,
    RecordNoteError,
};

pub(crate) const MAX_SESSION_RECORDS: usize = 500;
pub(crate) const DEFAULT_TOTAL_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_CAPTURE_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const REPRESENTATION_OVERHEAD_BYTES: usize = 32;
const DEFAULT_RECORD_BYTES: usize = MAX_CAPTURE_RECORD_BYTES;
const DEFAULT_PREVIEW_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecordView {
    pub id: RecordId,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub source_application: Option<String>,
    pub text: Option<String>,
    pub has_image: bool,
    pub note: Option<RecordNote>,
}

pub struct SessionRecordStore {
    state: Mutex<SessionRecordState>,
    limits: SessionRecordLimits,
}

#[derive(Clone, Copy)]
struct SessionRecordLimits {
    total_bytes: usize,
    record_bytes: usize,
    preview_bytes: usize,
}

struct SessionRecordState {
    records: VecDeque<Arc<ClipboardRecord>>,
    total_bytes: usize,
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
            total_bytes: DEFAULT_TOTAL_BYTES,
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
                total_bytes: 0,
            }),
            limits,
        }
    }

    pub fn capture(&self, capture: CapturedClipboard) -> bool {
        !matches!(self.capture_one(capture), CaptureStatus::RejectedTooLarge)
    }

    pub(crate) fn capture_one(&self, mut capture: CapturedClipboard) -> CaptureStatus {
        retain_preferred_image(&mut capture.representations);
        let Some(capture_bytes) = checked_representation_bytes(&capture.representations) else {
            return CaptureStatus::RejectedTooLarge;
        };
        if capture_bytes > self.limits.record_bytes {
            return CaptureStatus::RejectedTooLarge;
        }
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
            evict_to_limits(&mut state, self.limits);
            return CaptureStatus::Refreshed {
                previous: previous_bytes,
                bytes: refreshed_bytes,
            };
        }
        let record = ClipboardRecord::from_capture(capture);
        state.total_bytes = state.total_bytes.saturating_add(capture_bytes);
        state.records.push_front(Arc::new(record));
        evict_to_limits(&mut state, self.limits);
        CaptureStatus::Inserted {
            bytes: capture_bytes,
        }
    }

    pub(crate) fn evict_oldest(&self) -> bool {
        let mut state = lock_unpoisoned(&self.state);
        let Some(removed) = state.records.pop_back() else {
            return false;
        };
        state.total_bytes = state.total_bytes.saturating_sub(record_bytes(&removed));
        true
    }

    pub fn list(&self) -> Vec<SessionRecordView> {
        let records: Vec<_> = lock_unpoisoned(&self.state)
            .records
            .iter()
            .cloned()
            .collect();
        records
            .iter()
            .map(|record| SessionRecordView::from_record(record, self.limits.preview_bytes))
            .collect()
    }

    pub fn representations(&self, id: RecordId) -> Option<Vec<ClipboardRepresentation>> {
        let record = lock_unpoisoned(&self.state)
            .records
            .iter()
            .find(|record| record.id == id)
            .cloned();
        record.map(|record| record.representations.clone())
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
        let mut state = lock_unpoisoned(&self.state);
        let index = state
            .records
            .iter()
            .position(|record| record.id == id)
            .ok_or(SessionRecordError::NotFound)?;
        let previous_bytes = record_bytes(&state.records[index]);
        let updated_bytes = representation_bytes(&state.records[index].representations)
            + note.as_ref().map_or(0, |note| note.as_str().len());
        if updated_bytes > self.limits.record_bytes {
            return Err(SessionRecordError::RecordTooLarge);
        }
        let record = Arc::make_mut(
            state
                .records
                .get_mut(index)
                .ok_or(SessionRecordError::NotFound)?,
        );
        record.note = note;
        state.total_bytes = state.total_bytes - previous_bytes + updated_bytes;
        evict_to_limits(&mut state, self.limits);
        state
            .records
            .iter()
            .find(|record| record.id == id)
            .map(|record| SessionRecordView::from_record(record, self.limits.preview_bytes))
            .ok_or(SessionRecordError::NotFound)
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
                ClipboardRepresentation::Png { .. } | ClipboardRepresentation::DibV5 { .. } => None,
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
        }
    }
}

pub(crate) fn checked_representation_bytes(
    representations: &[ClipboardRepresentation],
) -> Option<usize> {
    representations
        .iter()
        .try_fold(0_usize, |total, representation| {
            let payload = match representation {
                ClipboardRepresentation::UnicodeText { text } => text.len(),
                ClipboardRepresentation::Png { bytes }
                | ClipboardRepresentation::DibV5 { bytes } => bytes.len(),
            };
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
            ClipboardRepresentation::UnicodeText { .. } => None,
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

fn evict_to_limits(state: &mut SessionRecordState, limits: SessionRecordLimits) {
    while state.records.len() > MAX_SESSION_RECORDS || state.total_bytes > limits.total_bytes {
        let Some(removed) = state.records.pop_back() else {
            break;
        };
        state.total_bytes = state.total_bytes.saturating_sub(record_bytes(&removed));
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
}

impl std::fmt::Display for SessionRecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("clipboard record is no longer available"),
            Self::InvalidNote(_) => formatter.write_str("clipboard record note is invalid"),
            Self::RecordTooLarge => formatter.write_str("clipboard record exceeds memory limits"),
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

    pub fn representations(
        &self,
        id: RecordId,
    ) -> Result<Vec<ClipboardRepresentation>, SessionRecordError> {
        self.store
            .representations(id)
            .ok_or(SessionRecordError::NotFound)
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::domain::{ContentIdentity, SourceIdentity};

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

    fn capture(identity: &str, text: &str) -> CapturedClipboard {
        CapturedClipboard {
            content_identity: ContentIdentity::new(identity),
            captured_at: Utc::now(),
            source: SourceIdentity::default(),
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: text.to_owned(),
            }],
        }
    }
}
