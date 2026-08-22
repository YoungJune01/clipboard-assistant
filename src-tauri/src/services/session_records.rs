use std::{collections::VecDeque, sync::Mutex};

use serde::Serialize;

use crate::domain::{
    CapturedClipboard, ClipboardRecord, ClipboardRepresentation, RecordId, RecordNote,
    RecordNoteError,
};

const MAX_SESSION_RECORDS: usize = 500;

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

#[derive(Default)]
pub struct SessionRecordStore {
    records: Mutex<VecDeque<ClipboardRecord>>,
}

impl SessionRecordStore {
    pub fn capture(&self, capture: CapturedClipboard) {
        let mut records = lock_unpoisoned(&self.records);
        if let Some(current) = records.front_mut()
            && current.content_identity == capture.content_identity
        {
            let _ = current.refresh_from(capture);
            return;
        }
        records.push_front(ClipboardRecord::from_capture(capture));
        records.truncate(MAX_SESSION_RECORDS);
    }

    pub fn list(&self) -> Vec<SessionRecordView> {
        lock_unpoisoned(&self.records)
            .iter()
            .map(SessionRecordView::from)
            .collect()
    }

    pub fn representations(&self, id: RecordId) -> Option<Vec<ClipboardRepresentation>> {
        lock_unpoisoned(&self.records)
            .iter()
            .find(|record| record.id == id)
            .map(|record| record.representations.clone())
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
        let mut records = lock_unpoisoned(&self.records);
        let record = records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or(SessionRecordError::NotFound)?;
        record.note = note;
        Ok(SessionRecordView::from(&*record))
    }
}

impl From<&ClipboardRecord> for SessionRecordView {
    fn from(record: &ClipboardRecord) -> Self {
        let text = record
            .representations
            .iter()
            .find_map(|representation| match representation {
                ClipboardRepresentation::UnicodeText { text } => Some(text.clone()),
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

#[derive(Debug)]
pub enum SessionRecordError {
    NotFound,
    InvalidNote(RecordNoteError),
}

impl std::fmt::Display for SessionRecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("clipboard record is no longer available"),
            Self::InvalidNote(_) => formatter.write_str("clipboard record note is invalid"),
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
