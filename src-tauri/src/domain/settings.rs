use chrono::{DateTime, Utc};
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StorageLimit {
    #[default]
    #[serde(alias = "one_gb")]
    OneGb,
    #[serde(alias = "five_gb")]
    FiveGb,
    #[serde(alias = "ten_gb")]
    TenGb,
    Unlimited,
}

impl StorageLimit {
    pub const fn bytes(self) -> Option<u64> {
        const GIB: u64 = 1024 * 1024 * 1024;
        match self {
            Self::OneGb => Some(GIB),
            Self::FiveGb => Some(5 * GIB),
            Self::TenGb => Some(10 * GIB),
            Self::Unlimited => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccentColor {
    #[default]
    Blue,
    Teal,
    Rose,
    Violet,
    Amber,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSound {
    #[default]
    Default,
    Custom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShortcutModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
}

impl ShortcutModifiers {
    pub const CTRL_ALT: Self = Self {
        ctrl: true,
        alt: true,
        shift: false,
        win: false,
    };

    pub const CTRL_SHIFT: Self = Self {
        ctrl: true,
        alt: false,
        shift: true,
        win: false,
    };

    pub fn is_safe_global_shortcut(self) -> bool {
        self.ctrl || self.alt || self.win
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutKey {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    #[default]
    V,
    W,
    X,
    Y,
    Z,
    Digit0,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    Left,
    Right,
    Up,
    Down,
    Space,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shortcut {
    pub modifiers: ShortcutModifiers,
    pub key: ShortcutKey,
}

impl Default for Shortcut {
    fn default() -> Self {
        Self {
            modifiers: ShortcutModifiers::CTRL_SHIFT,
            key: ShortcutKey::V,
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSettings {
    pub language: Language,
    pub retention: RetentionPeriod,
    #[serde(default)]
    pub storage_limit: StorageLimit,
    #[serde(default)]
    pub evict_favorites_when_full: bool,
    pub start_at_sign_in: bool,
    pub start_minimized: bool,
    pub show_tray_icon: bool,
    pub accent_color: AccentColor,
    pub sound_enabled: bool,
    pub capture_sound: CaptureSound,
    pub activation_shortcut: Shortcut,
    pub group_shortcut_modifiers: ShortcutModifiers,
    pub quick_paste_enabled: bool,
    pub quick_paste_modifiers: ShortcutModifiers,
    #[serde(default)]
    pub offline_ocr_enabled: bool,
    #[serde(default)]
    pub qr_recognition_enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncInterval {
    #[default]
    Manual,
    FifteenMinutes,
    OneHour,
    SixHours,
    Daily,
}

impl SyncInterval {
    pub fn duration(self) -> Option<std::time::Duration> {
        match self {
            Self::Manual => None,
            Self::FifteenMinutes => Some(std::time::Duration::from_secs(15 * 60)),
            Self::OneHour => Some(std::time::Duration::from_secs(60 * 60)),
            Self::SixHours => Some(std::time::Duration::from_secs(6 * 60 * 60)),
            Self::Daily => Some(std::time::Duration::from_secs(24 * 60 * 60)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebDavConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub remote_folder: String,
    pub interval: SyncInterval,
    pub allow_insecure_http: bool,
    pub device_id: String,
    pub last_local_sha256: Option<String>,
    pub last_remote_sha256: Option<String>,
    pub last_etag: Option<String>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
}

impl Default for WebDavConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            remote_folder: String::new(),
            interval: SyncInterval::Manual,
            allow_insecure_http: false,
            device_id: uuid::Uuid::new_v4().to_string(),
            last_local_sha256: None,
            last_remote_sha256: None,
            last_etag: None,
            last_success_at: None,
            last_result: None,
        }
    }
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            language: Language::default(),
            retention: RetentionPeriod::default(),
            storage_limit: StorageLimit::default(),
            evict_favorites_when_full: false,
            start_at_sign_in: false,
            start_minimized: false,
            show_tray_icon: true,
            accent_color: AccentColor::default(),
            sound_enabled: true,
            capture_sound: CaptureSound::default(),
            activation_shortcut: Shortcut::default(),
            group_shortcut_modifiers: ShortcutModifiers::CTRL_ALT,
            quick_paste_enabled: false,
            quick_paste_modifiers: ShortcutModifiers::CTRL_ALT,
            offline_ocr_enabled: false,
            qr_recognition_enabled: false,
        }
    }
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
        AccentColor, AppSettings, CaptureSound, ClipboardSettings, Language, PasteSettings,
        RetentionPeriod, Shortcut, ShortcutKey, ShortcutModifiers, StorageLimit, UserSettings,
    };

    #[test]
    fn user_settings_default_to_simplified_chinese_and_thirty_days() {
        assert_eq!(
            UserSettings::default(),
            UserSettings {
                language: Language::ZhCn,
                retention: RetentionPeriod::ThirtyDays,
                storage_limit: StorageLimit::OneGb,
                evict_favorites_when_full: false,
                start_at_sign_in: false,
                start_minimized: false,
                show_tray_icon: true,
                accent_color: AccentColor::Blue,
                sound_enabled: true,
                capture_sound: CaptureSound::Default,
                activation_shortcut: Shortcut::default(),
                group_shortcut_modifiers: ShortcutModifiers::CTRL_ALT,
                quick_paste_enabled: false,
                quick_paste_modifiers: ShortcutModifiers::CTRL_ALT,
                offline_ocr_enabled: false,
                qr_recognition_enabled: false,
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

    #[test]
    fn older_user_settings_json_defaults_storage_policy_fields() {
        let json = r#"{
            "language":"en",
            "retention":"forever",
            "start_at_sign_in":false,
            "start_minimized":false,
            "show_tray_icon":true,
            "accent_color":"blue",
            "sound_enabled":true,
            "capture_sound":"default",
            "activation_shortcut":{"modifiers":{"ctrl":true,"alt":false,"shift":true,"win":false},"key":"v"},
            "group_shortcut_modifiers":{"ctrl":true,"alt":true,"shift":false,"win":false},
            "quick_paste_enabled":false,
            "quick_paste_modifiers":{"ctrl":true,"alt":true,"shift":false,"win":false}
        }"#;

        let settings: UserSettings = serde_json::from_str(json).unwrap();

        assert_eq!(settings.storage_limit, StorageLimit::OneGb);
        assert!(!settings.evict_favorites_when_full);
        assert!(!settings.offline_ocr_enabled);
        assert!(!settings.qr_recognition_enabled);
    }

    #[test]
    fn storage_limit_accepts_legacy_snake_case_and_emits_camel_case() {
        assert_eq!(
            serde_json::from_str::<StorageLimit>(r#""five_gb""#).unwrap(),
            StorageLimit::FiveGb
        );
        assert_eq!(
            serde_json::to_string(&StorageLimit::FiveGb).unwrap(),
            r#""fiveGb""#
        );
    }

    #[test]
    fn shortcut_defaults_and_validation_are_stable() {
        assert_eq!(Shortcut::default().key, ShortcutKey::V);
        assert_eq!(Shortcut::default().modifiers, ShortcutModifiers::CTRL_SHIFT);
        assert!(ShortcutModifiers::CTRL_ALT.is_safe_global_shortcut());
        assert!(
            !ShortcutModifiers {
                shift: true,
                ..ShortcutModifiers::default()
            }
            .is_safe_global_shortcut()
        );
    }
}
