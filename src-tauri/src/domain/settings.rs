use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    pub clipboard: ClipboardSettings,
    pub paste: PasteSettings,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardSettings {
    pub history_limit: usize,
    pub capture_sensitive_content: bool,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            history_limit: 500,
            capture_sensitive_content: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasteSettings {
    pub restore_clipboard_after_paste: bool,
}

impl Default for PasteSettings {
    fn default() -> Self {
        Self {
            restore_clipboard_after_paste: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AppSettings, ClipboardSettings, PasteSettings};

    #[test]
    fn settings_round_trip_with_stable_field_names() {
        let settings = AppSettings {
            clipboard: ClipboardSettings {
                history_limit: 500,
                capture_sensitive_content: false,
            },
            paste: PasteSettings {
                restore_clipboard_after_paste: true,
            },
        };

        let json = serde_json::to_string(&settings).unwrap();

        assert_eq!(
            json,
            r#"{"clipboard":{"history_limit":500,"capture_sensitive_content":false},"paste":{"restore_clipboard_after_paste":true}}"#
        );
        assert_eq!(
            serde_json::from_str::<AppSettings>(&json).unwrap(),
            settings
        );
    }
}
