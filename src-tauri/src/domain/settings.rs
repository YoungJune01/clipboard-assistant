use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    ZhCn,
    En,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPeriod {
    OneDay,
    SevenDays,
    #[default]
    ThirtyDays,
    NinetyDays,
    Forever,
}

impl RetentionPeriod {
    pub fn days(self) -> Option<i64> {
        match self {
            Self::OneDay => Some(1),
            Self::SevenDays => Some(7),
            Self::ThirtyDays => Some(30),
            Self::NinetyDays => Some(90),
            Self::Forever => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSettings {
    pub language: Language,
    pub retention: RetentionPeriod,
}

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
    use super::{
        AppSettings, ClipboardSettings, Language, PasteSettings, RetentionPeriod, UserSettings,
    };

    #[test]
    fn user_settings_default_to_simplified_chinese_and_thirty_days() {
        assert_eq!(
            UserSettings::default(),
            UserSettings {
                language: Language::ZhCn,
                retention: RetentionPeriod::ThirtyDays,
            }
        );
        assert_eq!(RetentionPeriod::Forever.days(), None);
        assert_eq!(RetentionPeriod::SevenDays.days(), Some(7));
    }

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
