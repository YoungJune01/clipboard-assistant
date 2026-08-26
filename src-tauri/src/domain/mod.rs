pub mod clipboard;
pub mod paste;
pub mod record;
pub mod settings;

pub use clipboard::{CapturedClipboard, ClipboardRepresentation, ContentIdentity, SourceIdentity};
pub use paste::{PasteFallbackReason, PasteOutcome, TargetToken};
pub use record::{ClipboardRecord, GroupId, RecordId, RecordNote, RecordNoteError, RefreshError};
pub use settings::{
    AccentColor, AppSettings, CaptureSound, ClipboardSettings, Language, PasteSettings,
    RetentionPeriod, Shortcut, ShortcutKey, ShortcutModifiers, UserSettings,
};
