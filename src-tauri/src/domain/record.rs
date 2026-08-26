use std::{error::Error, fmt};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use super::{
    CapturedClipboard, ClipboardRepresentation, ContentIdentity, ContentKind, SourceIdentity,
};

const MAX_NOTE_CHARACTERS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordId(Uuid);

impl RecordId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for RecordId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupId(Uuid);

impl GroupId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn parse(value: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(value).map(Self)
    }

    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for GroupId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub captured_at: DateTime<Utc>,
    pub id: RecordId,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
    pub cursor: Option<HistoryCursor>,
    pub limit: usize,
    pub content_kind: Option<ContentKind>,
    pub group_id: Option<GroupId>,
    pub ungrouped_only: bool,
    pub favorites_only: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RecordNote(String);

impl RecordNote {
    pub fn new(value: impl Into<String>) -> Result<Self, RecordNoteError> {
        let value = value.into();
        if value.contains(['\r', '\n']) {
            return Err(RecordNoteError::MustBeSingleLine);
        }

        let actual_characters = value.chars().count();
        if actual_characters > MAX_NOTE_CHARACTERS {
            return Err(RecordNoteError::TooLong {
                max_characters: MAX_NOTE_CHARACTERS,
                actual_characters,
            });
        }

        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RecordNote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordNote")
            .field("characters", &self.0.chars().count())
            .finish()
    }
}

impl<'de> Deserialize<'de> for RecordNote {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordNoteError {
    MustBeSingleLine,
    TooLong {
        max_characters: usize,
        actual_characters: usize,
    },
}

impl fmt::Display for RecordNoteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MustBeSingleLine => formatter.write_str("record note must be a single line"),
            Self::TooLong {
                max_characters,
                actual_characters,
            } => write!(
                formatter,
                "record note exceeds {max_characters} characters ({actual_characters})"
            ),
        }
    }
}

impl Error for RecordNoteError {}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardRecord {
    pub id: RecordId,
    pub content_identity: ContentIdentity,
    pub captured_at: DateTime<Utc>,
    pub source: SourceIdentity,
    pub representations: Vec<ClipboardRepresentation>,
    pub note: Option<RecordNote>,
    pub group_id: Option<GroupId>,
    pub pinned: bool,
    pub favorite: bool,
    pub sensitive: bool,
}

impl fmt::Debug for ClipboardRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClipboardRecord")
            .field("id", &self.id)
            .field("content_identity", &self.content_identity)
            .field("captured_at", &self.captured_at)
            .field("source", &self.source)
            .field("representations", &self.representations)
            .field("note", &self.note)
            .field("group_id", &self.group_id)
            .field("pinned", &self.pinned)
            .field("favorite", &self.favorite)
            .field("sensitive", &self.sensitive)
            .finish()
    }
}

impl ClipboardRecord {
    pub fn from_capture(capture: CapturedClipboard) -> Self {
        Self {
            id: RecordId::new(),
            content_identity: capture.content_identity,
            captured_at: capture.captured_at,
            source: capture.source,
            representations: capture.representations,
            note: None,
            group_id: None,
            pinned: false,
            favorite: false,
            sensitive: false,
        }
    }

    pub fn refresh_from(&mut self, capture: CapturedClipboard) -> Result<(), RefreshError> {
        if self.content_identity != capture.content_identity {
            return Err(RefreshError::ContentIdentityMismatch {
                existing: self.content_identity.clone(),
                incoming: capture.content_identity,
            });
        }

        self.captured_at = capture.captured_at;
        self.source = capture.source;
        for representation in capture.representations {
            if !self
                .representations
                .iter()
                .any(|existing| existing.has_same_kind(&representation))
            {
                self.representations.push(representation);
            }
        }

        Ok(())
    }

    pub fn content_kind(&self) -> ContentKind {
        ContentKind::classify(&self.representations)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshError {
    ContentIdentityMismatch {
        existing: ContentIdentity,
        incoming: ContentIdentity,
    },
}

impl fmt::Display for RefreshError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContentIdentityMismatch { .. } => {
                formatter.write_str("clipboard content identity does not match")
            }
        }
    }
}

impl Error for RefreshError {}

#[cfg(test)]
mod tests {
    use super::{ClipboardRecord, GroupId, RecordNote, RecordNoteError, RefreshError};
    use crate::domain::{
        CapturedClipboard, ClipboardRepresentation, ContentIdentity, ContentKind, SourceIdentity,
    };
    use chrono::{TimeZone, Utc};

    fn captured(identity: &str, second: u32) -> CapturedClipboard {
        CapturedClipboard {
            content_identity: ContentIdentity::new(identity),
            captured_at: Utc.with_ymd_and_hms(2026, 8, 22, 1, 2, second).unwrap(),
            source: SourceIdentity {
                application_name: Some("Editor".to_owned()),
                executable_path: None,
            },
            representations: vec![ClipboardRepresentation::UnicodeText {
                text: "hello".to_owned(),
            }],
        }
    }

    #[test]
    fn record_note_counts_unicode_characters_not_utf8_bytes() {
        let two_hundred_multibyte_characters = "界".repeat(200);
        let two_hundred_one_multibyte_characters = "界".repeat(201);

        assert!(RecordNote::new(two_hundred_multibyte_characters).is_ok());
        assert_eq!(
            RecordNote::new(two_hundred_one_multibyte_characters),
            Err(RecordNoteError::TooLong {
                max_characters: 200,
                actual_characters: 201,
            })
        );
    }

    #[test]
    fn record_note_rejects_line_breaks() {
        assert_eq!(
            RecordNote::new("first\nsecond"),
            Err(RecordNoteError::MustBeSingleLine)
        );
        assert_eq!(
            RecordNote::new("first\rsecond"),
            Err(RecordNoteError::MustBeSingleLine)
        );
    }

    #[test]
    fn record_note_deserialization_enforces_domain_validation() {
        let too_long = serde_json::to_string(&"界".repeat(201)).unwrap();

        assert!(serde_json::from_str::<RecordNote>(&too_long).is_err());
        assert!(serde_json::from_str::<RecordNote>(r#""first\nsecond""#).is_err());
    }

    #[test]
    fn debug_output_redacts_record_note_and_clipboard_payload() {
        const SECRET_NOTE: &str = "DEBUG_SECRET_NOTE";
        const SECRET_TEXT: &str = "DEBUG_SECRET_RECORD_TEXT";
        let note = RecordNote::new(SECRET_NOTE).unwrap();
        let note_debug = format!("{note:?}");

        assert!(!note_debug.contains(SECRET_NOTE));
        assert!(note_debug.contains("RecordNote"));
        assert!(note_debug.contains("characters: 17"));

        let mut record = ClipboardRecord::from_capture(CapturedClipboard {
            content_identity: ContentIdentity::new("same"),
            captured_at: Utc.with_ymd_and_hms(2026, 8, 22, 1, 2, 3).unwrap(),
            source: SourceIdentity::default(),
            representations: vec![
                ClipboardRepresentation::UnicodeText {
                    text: SECRET_TEXT.to_owned(),
                },
                ClipboardRepresentation::Png {
                    bytes: vec![202, 254, 186, 190],
                },
            ],
        });
        record.note = Some(note);

        let record_debug = format!("{record:?}");

        assert!(!record_debug.contains(SECRET_NOTE));
        assert!(!record_debug.contains(SECRET_TEXT));
        assert!(!record_debug.contains("[202, 254, 186, 190]"));
        assert!(record_debug.contains("ClipboardRecord"));
        assert!(record_debug.contains("UnicodeText"));
        assert!(record_debug.contains("characters: 24"));
        assert!(record_debug.contains("Png"));
        assert!(record_debug.contains("bytes: 4"));
    }

    #[test]
    fn duplicate_refresh_preserves_metadata_and_adds_representations() {
        let mut record = ClipboardRecord::from_capture(captured("same", 3));
        let note = RecordNote::new("keep this").unwrap();
        let group_id = GroupId::new();
        record.note = Some(note.clone());
        record.group_id = Some(group_id);
        record.pinned = true;
        record.favorite = true;
        record.sensitive = true;

        let mut duplicate = captured("same", 9);
        duplicate.source.application_name = Some("New Source".to_owned());
        duplicate.representations = vec![ClipboardRepresentation::Png {
            bytes: vec![137, 80, 78, 71],
        }];

        record.refresh_from(duplicate).unwrap();

        assert_eq!(record.note, Some(note));
        assert_eq!(record.group_id, Some(group_id));
        assert!(record.pinned);
        assert!(record.favorite);
        assert!(record.sensitive);
        assert_eq!(record.captured_at.second(), 9);
        assert_eq!(
            record.source.application_name.as_deref(),
            Some("New Source")
        );
        assert_eq!(record.representations.len(), 2);
        assert!(matches!(
            record.representations[0],
            ClipboardRepresentation::UnicodeText { .. }
        ));
        assert!(matches!(
            record.representations[1],
            ClipboardRepresentation::Png { .. }
        ));
    }

    #[test]
    fn record_reports_its_primary_content_kind() {
        let mut record = ClipboardRecord::from_capture(captured("same", 3));
        assert_eq!(record.content_kind(), ContentKind::Text);

        record.representations.push(ClipboardRepresentation::Html {
            bytes: b"<b>rich</b>".to_vec(),
        });
        assert_eq!(record.content_kind(), ContentKind::RichText);

        record
            .representations
            .push(ClipboardRepresentation::Png { bytes: vec![1] });
        assert_eq!(record.content_kind(), ContentKind::Image);

        record
            .representations
            .push(ClipboardRepresentation::FileList {
                paths: vec![r"C:\Temp\one.txt".into()],
            });
        assert_eq!(record.content_kind(), ContentKind::Files);
    }

    #[test]
    fn refresh_rejects_different_content_identity_without_mutating_record() {
        let mut record = ClipboardRecord::from_capture(captured("original", 3));
        let original = record.clone();

        let error = record.refresh_from(captured("different", 9)).unwrap_err();

        assert_eq!(
            error,
            RefreshError::ContentIdentityMismatch {
                existing: ContentIdentity::new("original"),
                incoming: ContentIdentity::new("different"),
            }
        );
        assert_eq!(record, original);
    }

    use chrono::Timelike;
}
