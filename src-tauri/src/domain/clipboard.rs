use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentIdentity(String);

impl ContentIdentity {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ContentIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ContentIdentity(<redacted>)")
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub application_name: Option<String>,
    pub executable_path: Option<String>,
}

impl fmt::Debug for SourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceIdentity")
            .field("has_application_name", &self.application_name.is_some())
            .field("has_executable_path", &self.executable_path.is_some())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardRepresentation {
    UnicodeText { text: String },
    Png { bytes: Vec<u8> },
    DibV5 { bytes: Vec<u8> },
}

impl fmt::Debug for ClipboardRepresentation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnicodeText { text } => formatter
                .debug_struct("UnicodeText")
                .field("characters", &text.chars().count())
                .finish(),
            Self::Png { bytes } => formatter
                .debug_struct("Png")
                .field("bytes", &bytes.len())
                .finish(),
            Self::DibV5 { bytes } => formatter
                .debug_struct("DibV5")
                .field("bytes", &bytes.len())
                .finish(),
        }
    }
}

impl ClipboardRepresentation {
    pub(crate) fn has_same_kind(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::UnicodeText { .. }, Self::UnicodeText { .. })
                | (Self::Png { .. }, Self::Png { .. })
                | (Self::DibV5 { .. }, Self::DibV5 { .. })
        )
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedClipboard {
    pub content_identity: ContentIdentity,
    pub captured_at: DateTime<Utc>,
    pub source: SourceIdentity,
    pub representations: Vec<ClipboardRepresentation>,
}

impl fmt::Debug for CapturedClipboard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CapturedClipboard")
            .field("content_identity", &self.content_identity)
            .field("captured_at", &self.captured_at)
            .field("source", &self.source)
            .field("representations", &self.representations)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity};
    use chrono::{TimeZone, Utc};

    #[test]
    fn clipboard_representation_uses_stable_snake_case_json_tags() {
        let representation = ClipboardRepresentation::Png {
            bytes: vec![0, 127, 255],
        };

        let json = serde_json::to_string(&representation).unwrap();

        assert_eq!(json, r#"{"kind":"png","bytes":[0,127,255]}"#);
        assert_eq!(
            serde_json::from_str::<ClipboardRepresentation>(&json).unwrap(),
            representation
        );
    }

    #[test]
    fn captured_clipboard_round_trips_stably() {
        let captured = CapturedClipboard {
            content_identity: ContentIdentity::new("sha256:abc"),
            captured_at: Utc.with_ymd_and_hms(2026, 8, 22, 1, 2, 3).unwrap(),
            source: SourceIdentity {
                application_name: Some("Editor".to_owned()),
                executable_path: Some(r"C:\Tools\editor.exe".to_owned()),
            },
            representations: vec![
                ClipboardRepresentation::UnicodeText {
                    text: "你好".to_owned(),
                },
                ClipboardRepresentation::DibV5 {
                    bytes: vec![1, 2, 3],
                },
            ],
        };

        let json = serde_json::to_string(&captured).unwrap();
        let decoded: CapturedClipboard = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded, captured);
        assert!(json.contains(r#""kind":"unicode_text""#));
        assert!(json.contains(r#""kind":"dib_v5""#));
    }

    #[test]
    fn debug_output_redacts_clipboard_payloads() {
        const SECRET_TEXT: &str = "DEBUG_SECRET_TEXT_界";
        const SECRET_IDENTITY: &str = "DEBUG_SECRET_IDENTITY";
        const SECRET_APPLICATION: &str = "DEBUG_SECRET_APPLICATION";
        const SECRET_PATH: &str = r"C:\DEBUG_SECRET_PATH\app.exe";
        let binary = vec![222, 173, 190, 239];
        let representations = [
            ClipboardRepresentation::UnicodeText {
                text: SECRET_TEXT.to_owned(),
            },
            ClipboardRepresentation::Png {
                bytes: binary.clone(),
            },
            ClipboardRepresentation::DibV5 {
                bytes: binary.clone(),
            },
        ];

        let representation_debug = format!("{representations:?}");

        assert!(!representation_debug.contains(SECRET_TEXT));
        assert!(!representation_debug.contains("[222, 173, 190, 239]"));
        assert!(representation_debug.contains("UnicodeText"));
        assert!(representation_debug.contains("characters: 19"));
        assert!(representation_debug.contains("Png"));
        assert!(representation_debug.contains("DibV5"));
        assert!(representation_debug.contains("bytes: 4"));

        let capture = CapturedClipboard {
            content_identity: ContentIdentity::new(SECRET_IDENTITY),
            captured_at: Utc.with_ymd_and_hms(2026, 8, 22, 1, 2, 3).unwrap(),
            source: SourceIdentity {
                application_name: Some(SECRET_APPLICATION.to_owned()),
                executable_path: Some(SECRET_PATH.to_owned()),
            },
            representations: representations.into(),
        };
        let capture_debug = format!("{capture:?}");

        assert!(!capture_debug.contains(SECRET_TEXT));
        assert!(!capture_debug.contains(SECRET_IDENTITY));
        assert!(!capture_debug.contains(SECRET_APPLICATION));
        assert!(!capture_debug.contains(SECRET_PATH));
        assert!(!capture_debug.contains("[222, 173, 190, 239]"));
        assert!(capture_debug.contains("CapturedClipboard"));
        assert!(capture_debug.contains("ContentIdentity(<redacted>)"));
        assert!(capture_debug.contains("has_application_name: true"));
        assert!(capture_debug.contains("has_executable_path: true"));
        assert!(capture_debug.contains("representations"));
    }
}
