pub mod clipboard;
pub mod paste;
pub mod record;
pub mod settings;

pub use clipboard::{
    CapturedClipboard, ClipboardRepresentation, ClipboardRepresentationDetails,
    ClipboardRepresentationKind, ContentIdentity, ContentKind, SourceIdentity,
};
pub use paste::{PasteFallbackReason, PasteOutcome, TargetToken};
pub use record::{
    ClipboardRecord, GroupId, HistoryCursor, HistoryQuery, RecordId, RecordNote, RecordNoteError,
    RefreshError,
};
pub use settings::{
    AccentColor, AppSettings, CaptureSound, ClipboardSettings, Language, PasteSettings,
    RetentionPeriod, Shortcut, ShortcutKey, ShortcutModifiers, StorageLimit, UserSettings,
};
