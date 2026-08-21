use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceIdentity {
    pub application_name: Option<String>,
    pub executable_path: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClipboardRepresentation {
    UnicodeText { text: String },
    Png { bytes: Vec<u8> },
    DibV5 { bytes: Vec<u8> },
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedClipboard {
    pub content_identity: ContentIdentity,
    pub captured_at: DateTime<Utc>,
    pub source: SourceIdentity,
    pub representations: Vec<ClipboardRepresentation>,
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
}
